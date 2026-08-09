use super::*;
use crate::fs::{FileSystem, FsError, Stats};

impl OverlayFS {
    /// Copy inherited overlay state into this overlay's writable delta.
    ///
    /// `paths` and `whiteouts` describe changes owned by lower overlay
    /// layers, not host-base fallthrough. Visible entries are copied through
    /// the normal copy-up path so partial-origin files in a lower layer are
    /// resolved into complete file contents. Entries already owned or hidden
    /// by this delta retain precedence.
    pub async fn materialize_base_changes(
        &self,
        paths: &HashSet<String>,
        whiteouts: &HashSet<String>,
    ) -> Result<()> {
        let mut metadata = self.collect_delta_metadata().await?;
        let mut copied_inodes = HashMap::new();
        let mut ordered_paths = paths.iter().collect::<Vec<_>>();
        ordered_paths.sort_by(|left, right| {
            left.matches('/')
                .count()
                .cmp(&right.matches('/').count())
                .then_with(|| left.cmp(right))
        });

        for path in ordered_paths {
            if self.is_whiteout(path) || self.resolve_delta_path(path).await?.is_some() {
                continue;
            }
            let Some(stats) = self.resolve_visible_path(path).await? else {
                continue;
            };

            let delta_ino = if !stats.is_directory() {
                if let Some(delta_ino) = copied_inodes.get(&stats.ino).copied() {
                    self.materialize_hardlink(path, delta_ino, &stats).await?
                } else {
                    let delta_ino = self.materialize_visible_path(path, &stats).await?;
                    copied_inodes.insert(stats.ino, delta_ino);
                    delta_ino
                }
            } else {
                self.materialize_visible_path(path, &stats).await?
            };
            metadata.insert(delta_ino, stats);
        }

        let mut ordered_whiteouts = whiteouts.iter().collect::<Vec<_>>();
        ordered_whiteouts.sort_by(|left, right| {
            left.matches('/')
                .count()
                .cmp(&right.matches('/').count())
                .then_with(|| left.cmp(right))
        });
        for path in ordered_whiteouts {
            if self.is_whiteout(path) {
                continue;
            }
            if self.resolve_visible_path(path).await?.is_some() {
                continue;
            }
            self.create_whiteout(path).await?;
        }

        FileSystem::drain_all(&self.delta).await?;
        self.restore_materialized_metadata(metadata).await
    }

    async fn resolve_visible_path(&self, path: &str) -> Result<Option<Stats>> {
        let mut ino = ROOT_INO;
        if path == "/" {
            return FileSystem::getattr(self, ino).await;
        }
        let mut stats = None;
        for component in path.split('/').filter(|component| !component.is_empty()) {
            let next = match FileSystem::lookup(self, ino, component).await {
                Ok(Some(next)) => next,
                Ok(None) | Err(Error::Fs(FsError::NotFound | FsError::NotADirectory)) => {
                    return Ok(None)
                }
                Err(error) => return Err(error),
            };
            ino = next.ino;
            stats = Some(next);
        }
        Ok(stats)
    }

    async fn resolve_delta_path(&self, path: &str) -> Result<Option<Stats>> {
        let mut ino = ROOT_INO;
        if path == "/" {
            return FileSystem::getattr(&self.delta, ino).await;
        }
        let mut stats = None;
        for component in path.split('/').filter(|component| !component.is_empty()) {
            let next = match FileSystem::lookup(&self.delta, ino, component).await {
                Ok(Some(next)) => next,
                Ok(None) | Err(Error::Fs(FsError::NotFound | FsError::NotADirectory)) => {
                    return Ok(None)
                }
                Err(error) => return Err(error),
            };
            ino = next.ino;
            stats = Some(next);
        }
        Ok(stats)
    }

    async fn materialize_visible_path(&self, path: &str, stats: &Stats) -> Result<i64> {
        let info = self.get_inode_info(stats.ino).ok_or(FsError::NotFound)?;
        if info.layer != Layer::Base {
            return Err(Error::Internal(format!(
                "cannot materialize non-base path {path}"
            )));
        }
        if stats.is_file() || stats.is_directory() || stats.is_symlink() {
            return self.copy_up_and_update_mapping(stats.ino, &info).await;
        }

        let components = path
            .split('/')
            .filter(|component| !component.is_empty())
            .collect::<Vec<_>>();
        let name = components.last().ok_or(FsError::RootOperation)?;
        self.ensure_parent_dirs(path, stats.uid, stats.gid).await?;
        let mut parent_ino = ROOT_INO;
        for component in components.iter().take(components.len() - 1) {
            parent_ino = FileSystem::lookup(&self.delta, parent_ino, component)
                .await?
                .ok_or(FsError::NotFound)?
                .ino;
        }
        let materialized = FileSystem::mknod(
            &self.delta,
            parent_ino,
            name,
            stats.mode,
            stats.rdev,
            stats.uid,
            stats.gid,
        )
        .await?;
        self.add_origin_mapping(materialized.ino, info.underlying_ino)
            .await?;
        self.refresh_overlay_mapping(stats.ino, Layer::Delta, materialized.ino, path);
        Ok(materialized.ino)
    }

    async fn materialize_hardlink(
        &self,
        path: &str,
        source_delta_ino: i64,
        stats: &Stats,
    ) -> Result<i64> {
        let components = path
            .split('/')
            .filter(|component| !component.is_empty())
            .collect::<Vec<_>>();
        let name = components.last().ok_or(FsError::RootOperation)?;
        self.remove_whiteout(path).await?;
        self.ensure_parent_dirs(path, stats.uid, stats.gid).await?;

        let mut parent_ino = ROOT_INO;
        for component in components.iter().take(components.len() - 1) {
            parent_ino = FileSystem::lookup(&self.delta, parent_ino, component)
                .await?
                .ok_or(FsError::NotFound)?
                .ino;
        }
        FileSystem::link(&self.delta, source_delta_ino, parent_ino, name)
            .await
            .map(|linked| linked.ino)
    }

    async fn collect_delta_metadata(&self) -> Result<HashMap<i64, Stats>> {
        let mut metadata = HashMap::new();
        let root = FileSystem::getattr(&self.delta, ROOT_INO)
            .await?
            .ok_or(FsError::NotFound)?;
        metadata.insert(ROOT_INO, root);

        let mut directories = vec![ROOT_INO];
        while let Some(parent_ino) = directories.pop() {
            let entries = FileSystem::readdir_plus(&self.delta, parent_ino)
                .await?
                .ok_or(FsError::NotFound)?;
            for entry in entries {
                if entry.stats.is_directory() {
                    directories.push(entry.stats.ino);
                }
                metadata.entry(entry.stats.ino).or_insert(entry.stats);
            }
        }
        Ok(metadata)
    }

    async fn restore_materialized_metadata(&self, metadata: HashMap<i64, Stats>) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let mut txn =
            super::super::vfs::MutationTxn::begin(&conn, self.delta.journal_enabled()).await?;
        let mut restored = Vec::with_capacity(metadata.len());
        for (ino, stats) in metadata {
            txn.conn()
                .execute(
                    "UPDATE fs_inode
                 SET mode = ?, uid = ?, gid = ?, atime = ?, mtime = ?, ctime = ?,
                     atime_nsec = ?, mtime_nsec = ?, ctime_nsec = ?, rdev = ?
                 WHERE ino = ?",
                    (
                        stats.mode as i64,
                        stats.uid as i64,
                        stats.gid as i64,
                        stats.atime,
                        stats.mtime,
                        stats.ctime,
                        stats.atime_nsec as i64,
                        stats.mtime_nsec as i64,
                        stats.ctime_nsec as i64,
                        stats.rdev as i64,
                        ino,
                    ),
                )
                .await?;
            txn.record(super::super::vfs::JournalOp::new(
                "materialize_meta",
                serde_json::json!({ "ino": ino }),
            ));
            restored.push(ino);
        }
        txn.commit().await?;
        for ino in restored {
            self.delta.invalidate_attr(ino);
        }
        Ok(())
    }
}
