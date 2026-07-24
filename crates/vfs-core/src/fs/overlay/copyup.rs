use super::*;
use crate::fs::{BoxedFile, FsError};

impl OverlayFS {
    pub(super) async fn resolve_base_path(&self, path: &str) -> Result<Option<Stats>> {
        let mut ino = ROOT_INO;
        if path == "/" {
            return self.base.getattr(ino).await;
        }

        let mut stats = None;
        for component in path.split('/').filter(|s| !s.is_empty()) {
            let Some(next) = self.base.lookup(ino, component).await? else {
                return Ok(None);
            };
            ino = next.ino;
            stats = Some(next);
        }
        Ok(stats)
    }

    pub(super) async fn resolves_to_visible_base_directory(&self, path: &str) -> Result<bool> {
        if self.is_whiteout(path) {
            return Ok(false);
        }

        Ok(self
            .resolve_base_path(path)
            .await?
            .is_some_and(|stats| stats.is_directory()))
    }

    fn validate_partial_origin(&self, origin: &PartialOrigin, stats: &Stats) -> Result<()> {
        if stats.size != origin.base_fingerprint_size {
            return Err(Error::Internal(format!(
                "partial-origin base changed for {} (stored size={}, current size={})",
                origin.base_path, origin.base_fingerprint_size, stats.size
            )));
        }
        if stats.mtime != origin.base_mtime
            || stats.mtime_nsec != origin.base_mtime_nsec
            || stats.ctime != origin.base_ctime
            || stats.ctime_nsec != origin.base_ctime_nsec
        {
            return Err(Error::Internal(format!(
                "partial-origin base changed for {} (stored mtime={}.{}, current mtime={}.{}, stored ctime={}.{}, current ctime={}.{})",
                origin.base_path,
                origin.base_mtime,
                origin.base_mtime_nsec,
                stats.mtime,
                stats.mtime_nsec,
                origin.base_ctime,
                origin.base_ctime_nsec,
                stats.ctime,
                stats.ctime_nsec
            )));
        }
        Ok(())
    }

    pub(super) async fn cleanup_partial_origin_if_unlinked(&self, delta_ino: i64) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let mut rows = conn
            .query("SELECT 1 FROM fs_inode WHERE ino = ?", (delta_ino,))
            .await?;
        if rows.next().await?.is_some() {
            return Ok(());
        }

        conn.execute("DELETE FROM fs_origin WHERE delta_ino = ?", (delta_ino,))
            .await?;
        conn.execute(
            "DELETE FROM fs_chunk_override WHERE delta_ino = ?",
            (delta_ino,),
        )
        .await?;
        conn.execute(
            "DELETE FROM fs_partial_origin WHERE delta_ino = ?",
            (delta_ino,),
        )
        .await?;
        Ok(())
    }

    /// Promote an overlay inode from base layer to delta layer.
    ///
    /// When a directory that was originally looked up from base gets a
    /// corresponding directory created in delta (via ensure_parent_dirs),
    /// we need to update the overlay inode to point to delta. This ensures
    /// that operations like readdir and unlink will check the delta layer.
    fn promote_to_delta(&self, path: &str, delta_ino: i64) {
        self.promote_mapping_to_delta(path, delta_ino);
    }

    /// Resolve the delta-layer inode for a parent directory.
    ///
    /// If the parent's overlay inode already maps to Delta, returns the underlying
    /// inode directly. Otherwise, walks the delta filesystem from root using the
    /// stored path. Returns Ok(None) if any path component is missing in delta.
    pub(super) async fn resolve_delta_parent(&self, info: &InodeInfo) -> Result<Option<i64>> {
        if info.layer == Layer::Delta {
            return Ok(Some(info.underlying_ino));
        }
        let mut ino: i64 = 1;
        for comp in info.path.split('/').filter(|s| !s.is_empty()) {
            match FileSystem::lookup(&self.delta, ino, comp).await? {
                Some(s) if s.is_directory() => ino = s.ino,
                Some(_) => return Ok(None),
                None => return Ok(None),
            }
        }
        Ok(Some(ino))
    }

    /// Ensure parent directories exist in delta layer
    pub(super) async fn ensure_parent_dirs(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        let mut current_path = String::new();
        let mut current_delta_ino: i64 = 1; // Delta root
        let mut current_base_ino: i64 = 1; // Base root

        for component in components.iter().take(components.len().saturating_sub(1)) {
            current_path = format!("{}/{}", current_path, component);

            // Remove any whiteout for this path
            self.remove_whiteout(&current_path).await?;

            // Check if directory exists in delta
            if let Some(stats) =
                FileSystem::lookup(&self.delta, current_delta_ino, component).await?
            {
                if stats.is_directory() {
                    current_delta_ino = stats.ino;
                    // Advance base in parallel so it stays in sync
                    if let Some(bs) = self.base.lookup(current_base_ino, component).await? {
                        current_base_ino = bs.ino;
                    }
                    continue;
                } else {
                    return Err(FsError::NotADirectory.into());
                }
            }

            // Not in delta, check base (using the base inode, not delta inode)
            let base_stats = self.base.lookup(current_base_ino, component).await?;
            let (dir_uid, dir_gid, origin_base_ino) = if let Some(s) = &base_stats {
                let base_ino = s.ino;
                current_base_ino = base_ino;
                (s.uid, s.gid, Some(base_ino))
            } else {
                (uid, gid, None)
            };

            // Create directory in delta
            let new_stats = FileSystem::mkdir(
                &self.delta,
                current_delta_ino,
                component,
                0o755,
                dir_uid,
                dir_gid,
            )
            .await?;
            current_delta_ino = new_stats.ino;

            // Create origin mapping if directory exists in base, so that
            // lookups return consistent overlay inodes
            if let Some(base_ino) = origin_base_ino {
                self.add_origin_mapping(new_stats.ino, base_ino).await?;
                // Promote the overlay inode to delta so readdir/unlink will check delta
                self.promote_to_delta(&current_path, new_stats.ino);
            }
        }

        Ok(())
    }

    /// Copy a file from base to delta for modification
    pub(super) async fn copy_up(&self, path: &str, base_ino: i64) -> Result<i64> {
        // Parse path to get parent and name
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let name = components.last().unwrap();

        // Check if already copied up - walk delta to find parent and check for file
        let mut parent_ino: i64 = 1;
        let mut found_parent = true;
        for comp in components.iter().take(components.len() - 1) {
            if let Some(stats) = FileSystem::lookup(&self.delta, parent_ino, comp).await? {
                parent_ino = stats.ino;
            } else {
                found_parent = false;
                break;
            }
        }

        // If parent exists in delta, check if file already exists there
        if found_parent {
            if let Some(stats) = FileSystem::lookup(&self.delta, parent_ino, name).await? {
                // Already copied up, return delta inode
                return Ok(stats.ino);
            }
        }

        // Get base stats
        let base_stats = self
            .base
            .getattr(base_ino)
            .await?
            .ok_or(FsError::NotFound)?;

        // Ensure parent directories exist
        self.ensure_parent_dirs(path, base_stats.uid, base_stats.gid)
            .await?;

        // Look up parent in delta by walking the path
        let mut parent_ino: i64 = 1; // Start at delta root
        for comp in components.iter().take(components.len() - 1) {
            let stats = FileSystem::lookup(&self.delta, parent_ino, comp)
                .await?
                .ok_or(FsError::NotFound)?;
            parent_ino = stats.ino;
        }

        // Copy based on file type
        let delta_ino = if base_stats.is_symlink() {
            let target = self
                .base
                .readlink(base_ino)
                .await?
                .ok_or(FsError::NotFound)?;
            let stats = FileSystem::symlink(
                &self.delta,
                parent_ino,
                name,
                &target,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            stats.ino
        } else if base_stats.is_directory() {
            let stats = FileSystem::mkdir(
                &self.delta,
                parent_ino,
                name,
                base_stats.mode & 0o7777,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            stats.ino
        } else {
            // Regular file - read content and create
            let base_file = self.base.open(base_ino, libc::O_RDONLY).await?;
            let content = base_file.pread(0, base_stats.size as u64).await?;

            let (stats, delta_file) = FileSystem::create_file(
                &self.delta,
                parent_ino,
                name,
                base_stats.mode,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            delta_file.pwrite(0, &content).await?;
            stats.ino
        };

        // Store origin mapping
        self.add_origin_mapping(delta_ino, base_ino).await?;

        Ok(delta_ino)
    }

    /// Copy-up a file and update the inode mapping so subsequent operations
    /// go to the delta layer. Returns the delta inode.
    pub(super) async fn copy_up_and_update_mapping(
        &self,
        overlay_ino: i64,
        info: &InodeInfo,
    ) -> Result<i64> {
        let delta_ino = self.copy_up(&info.path, info.underlying_ino).await?;
        self.refresh_overlay_mapping(overlay_ino, Layer::Delta, delta_ino, &info.path);
        Ok(delta_ino)
    }

    pub(super) async fn partial_copy_up_and_update_mapping(
        &self,
        overlay_ino: i64,
        info: &InodeInfo,
    ) -> Result<i64> {
        let components: Vec<&str> = info.path.split('/').filter(|s| !s.is_empty()).collect();
        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let name = components.last().unwrap();

        let base_stats = match self.resolve_base_path(&info.path).await? {
            Some(stats) => stats,
            None => self
                .base
                .getattr(info.underlying_ino)
                .await?
                .ok_or(FsError::NotFound)?,
        };
        if !base_stats.is_file() {
            return self.copy_up_and_update_mapping(overlay_ino, info).await;
        }

        self.ensure_parent_dirs(&info.path, base_stats.uid, base_stats.gid)
            .await?;

        let mut parent_ino = ROOT_INO;
        for comp in components.iter().take(components.len() - 1) {
            let stats = FileSystem::lookup(&self.delta, parent_ino, comp)
                .await?
                .ok_or(FsError::NotFound)?;
            parent_ino = stats.ino;
        }

        if let Some(stats) = FileSystem::lookup(&self.delta, parent_ino, name).await? {
            self.refresh_overlay_mapping(overlay_ino, Layer::Delta, stats.ino, &info.path);
            return Ok(stats.ino);
        }

        let (stats, _file) = FileSystem::create_file(
            &self.delta,
            parent_ino,
            name,
            base_stats.mode,
            base_stats.uid,
            base_stats.gid,
        )
        .await?;
        let delta_ino = stats.ino;

        let conn = self.delta.get_connection().await?;
        conn.execute(
            "UPDATE fs_inode
             SET mode = ?, uid = ?, gid = ?, size = ?, atime = ?, mtime = ?, ctime = ?,
                 atime_nsec = ?, mtime_nsec = ?, ctime_nsec = ?, data_inline = NULL, storage_kind = ?
             WHERE ino = ?",
            (
                base_stats.mode as i64,
                base_stats.uid as i64,
                base_stats.gid as i64,
                base_stats.size,
                base_stats.atime,
                base_stats.mtime,
                base_stats.ctime,
                base_stats.atime_nsec as i64,
                base_stats.mtime_nsec as i64,
                base_stats.ctime_nsec as i64,
                STORAGE_CHUNKED,
                delta_ino,
            ),
        )
        .await?;
        self.delta.invalidate_attr(delta_ino);

        self.add_origin_mapping(delta_ino, info.underlying_ino)
            .await?;
        self.add_partial_origin_mapping(delta_ino, info.underlying_ino, &info.path, &base_stats)
            .await?;
        self.refresh_overlay_mapping(overlay_ino, Layer::Delta, delta_ino, &info.path);

        Ok(delta_ino)
    }

    pub(super) async fn partial_file_for_delta(
        &self,
        overlay_ino: i64,
        delta_ino: i64,
        flags: i32,
    ) -> Result<BoxedFile> {
        if let Some(origin) = self.partial_origin_for_delta(delta_ino).await? {
            let base_stats = self
                .resolve_base_path(&origin.base_path)
                .await?
                .ok_or(FsError::NotFound)?;
            self.validate_partial_origin(&origin, &base_stats)?;
            let base_file = self.base.open(base_stats.ino, libc::O_RDONLY).await?;

            // Tier Two Axis C: HostFS passthrough for unmodified delta files.
            //
            // A partial-origin delta inode that has zero chunk overrides, zero
            // full chunks, no inline override, and a size matching the base is
            // byte-identical to the base file. In that case the
            // OverlayPartialFile wrapper would do a chunk-merge that always
            // hits the "no override; read from base" branch -- the SQLite
            // round trip is pure overhead. Returning the HostFS fd directly
            // sends pread() straight to the kernel VFS for every read on this
            // handle, which is most of the cost on `git status` / `git diff`
            // / agent stat-storms over a working tree that was copy-up'd but
            // not modified.
            //
            // Restricted to read-only opens: a write open MUST go through the
            // OverlayPartialFile wrapper so writes land as `fs_chunk_override`
            // rows in the delta DB and never touch the real base file
            // (no-real-write invariant from Tier One).
            if !is_write_open(flags) {
                crate::telemetry::record_base_fast_open_passthrough_attempted();
                if self
                    .delta_has_no_content_overrides(delta_ino, base_stats.size)
                    .await?
                {
                    crate::telemetry::record_base_fast_open_passthrough_succeeded();
                    return Ok(mount_visible_file(base_file, overlay_ino));
                }
                crate::telemetry::record_base_fast_open_passthrough_fallback();
            }

            let file: BoxedFile = Arc::new(OverlayPartialFile {
                delta: self.delta.clone(),
                base: self.base.clone(),
                base_file,
                origin,
                overlay_ino,
                delta_ino,
                chunk_size: self.delta.chunk_size(),
            });
            if (flags & libc::O_TRUNC) != 0 {
                file.truncate(0).await?;
            }
            Ok(file)
        } else {
            FileSystem::open(&self.delta, delta_ino, flags).await
        }
    }

    /// Returns true if the delta inode has no content modifications: no chunk
    /// overrides, no full chunks, no inline override, and size matches the
    /// base. Such a delta is purely a metadata copy and reads can bypass the
    /// `OverlayPartialFile` merge path entirely.
    ///
    /// This is the cheap "is this file unmodified?" check that Tier Two Axis
    /// C uses to decide whether `partial_file_for_delta` can short-circuit to
    /// a HostFS fd.
    async fn delta_has_no_content_overrides(&self, delta_ino: i64, base_size: i64) -> Result<bool> {
        let conn = self.delta.get_connection().await?;

        // Any per-chunk override?
        let mut rows = conn
            .query(
                "SELECT 1 FROM fs_chunk_override WHERE delta_ino = ? LIMIT 1",
                (delta_ino,),
            )
            .await?;
        if rows.next().await?.is_some() {
            return Ok(false);
        }

        // Any full chunk in fs_data? (Should be implied by no overrides for
        // partial-origin files, but check defensively in case of a
        // partial-origin → fully-overridden transition.)
        let mut rows = conn
            .query("SELECT 1 FROM fs_data WHERE ino = ? LIMIT 1", (delta_ino,))
            .await?;
        if rows.next().await?.is_some() {
            return Ok(false);
        }

        // Size match + no inline override?
        let mut rows = conn
            .query(
                "SELECT size, data_inline FROM fs_inode WHERE ino = ?",
                (delta_ino,),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(false);
        };
        let delta_size: i64 = row
            .get(0)
            .map_err(|e| Error::Internal(format!("fs_inode.size read failed: {e}")))?;
        if delta_size != base_size {
            return Ok(false);
        }
        let inline_value = row
            .get_value(1)
            .map_err(|e| Error::Internal(format!("fs_inode.data_inline read failed: {e}")))?;
        let inline_empty = match inline_value {
            Value::Null => true,
            Value::Blob(blob) => blob.is_empty(),
            _ => true,
        };
        if !inline_empty {
            return Ok(false);
        }

        Ok(true)
    }
}
