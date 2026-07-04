use super::*;
use crate::fs::{
    BoxedFile, DirEntry, FileSystem, FilesystemStats, FsError, KernelCachePolicy, Stats, TimeChange,
};
use async_trait::async_trait;
use std::collections::{HashMap, HashSet};
use tracing::trace;

#[async_trait]
impl FileSystem for OverlayFS {
    async fn lookup(&self, parent_ino: i64, name: &str) -> Result<Option<Stats>> {
        crate::telemetry::record_lookup();
        trace!(
            "OverlayFS::lookup: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check for whiteout
        if self.is_whiteout(&path) {
            crate::telemetry::record_lookup_whiteout();
            crate::telemetry::record_negative_lookup();
            return Ok(None);
        }

        // Try delta first
        let delta_parent_ino = self.resolve_delta_parent(&parent_info).await?;

        // Look up in delta (only if we resolved the correct parent)
        if let Some(delta_stats) = match delta_parent_ino {
            Some(ino) => {
                crate::telemetry::record_lookup_delta();
                self.delta.lookup(ino, name).await?
            }
            None => None,
        } {
            let delta_ino = delta_stats.ino;
            let ino = self.get_or_create_overlay_ino(Layer::Delta, delta_ino, &path);
            let mut stats = delta_stats;

            // Origin mapping: reuse an existing Base overlay inode for stable
            // numbering within a session.  After remount the base_ino stored in
            // the mapping may be stale (the new HostFS has a fresh inode cache),
            // so only use it when the reverse_map already contains a live entry.
            // Otherwise keep the Delta overlay inode — the downstream code
            // already walks base from root when the parent is tagged Delta.
            if let Some(base_ino) = self.get_origin_ino(stats.ino) {
                if let Some(existing_ino) = self.live_origin_overlay_ino(base_ino, &path) {
                    self.refresh_overlay_mapping(existing_ino, Layer::Delta, delta_ino, &path);
                    stats.ino = existing_ino;
                } else {
                    stats.ino = ino;
                }
            } else {
                stats.ino = ino;
            }

            return Ok(Some(stats));
        }

        // Try base
        let base_parent_ino = if parent_info.layer == Layer::Base {
            parent_info.underlying_ino
        } else {
            // Need to find corresponding base parent by path
            // For root, use base root (1)
            if parent_info.path == "/" {
                1
            } else {
                // Walk the base to find the parent
                let mut base_ino: i64 = 1;
                let components: Vec<_> = parent_info
                    .path
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .collect();
                crate::telemetry::record_path_resolution(components.len() as u64);
                for comp in components {
                    if let Some(s) = self.base.lookup(base_ino, comp).await? {
                        base_ino = s.ino;
                    } else {
                        crate::telemetry::record_negative_lookup();
                        return Ok(None);
                    }
                }
                base_ino
            }
        };

        crate::telemetry::record_lookup_base();
        if let Some(base_stats) = self.base.lookup(base_parent_ino, name).await? {
            let ino = self.get_or_create_overlay_ino(Layer::Base, base_stats.ino, &path);
            let mut stats = base_stats;
            stats.ino = ino;
            return Ok(Some(stats));
        }

        crate::telemetry::record_negative_lookup();
        Ok(None)
    }

    async fn getattr(&self, ino: i64) -> Result<Option<Stats>> {
        crate::telemetry::record_getattr();
        crate::telemetry::record_attr_cache_miss();
        trace!("OverlayFS::getattr: ino={}", ino);

        let info = match self.get_inode_info(ino) {
            Some(i) => i,
            None => return Ok(None),
        };
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            crate::telemetry::record_lookup_whiteout();
            return Ok(None);
        }

        match info.layer {
            Layer::Delta => Ok(FileSystem::getattr(&self.delta, info.underlying_ino)
                .await?
                .map(|mut s| {
                    s.ino = ino;
                    s
                })),
            Layer::Base => {
                let stats = self.resolve_base_path(&info.path).await?;
                Ok(stats.map(|mut s| {
                    self.refresh_overlay_mapping(ino, Layer::Base, s.ino, &info.path);
                    s.ino = ino;
                    s
                }))
            }
        }
    }

    async fn readlink(&self, ino: i64) -> Result<Option<String>> {
        trace!("OverlayFS::readlink: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Ok(None);
        }

        match info.layer {
            Layer::Delta => FileSystem::readlink(&self.delta, info.underlying_ino).await,
            Layer::Base => self.base.readlink(info.underlying_ino).await,
        }
    }

    async fn readdir(&self, ino: i64) -> Result<Option<Vec<String>>> {
        crate::telemetry::record_readdir();
        trace!("OverlayFS::readdir: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let child_whiteouts = self.get_child_whiteouts(&info.path);

        let mut entries = HashSet::new();

        // Get delta entries
        if info.layer == Layer::Delta {
            if let Some(delta_entries) = self.delta.readdir(info.underlying_ino).await? {
                for entry in delta_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry)
                    } else {
                        format!("{}/{}", info.path, entry)
                    };
                    if !self.is_whiteout(&entry_path) && !child_whiteouts.contains(&entry) {
                        entries.insert(entry);
                    }
                }
            }
        }

        // Get base entries (need to resolve base inode from path)
        let base_ino = if info.layer == Layer::Base {
            Some(info.underlying_ino)
        } else {
            // Walk base to find corresponding directory
            let components: Vec<&str> = info.path.split('/').filter(|s| !s.is_empty()).collect();
            let mut ino: i64 = 1;
            let mut found_all = true;
            crate::telemetry::record_path_resolution(components.len() as u64);
            for comp in &components {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    found_all = false;
                    break;
                }
            }
            if found_all {
                Some(ino)
            } else {
                None
            }
        };

        if let Some(base_ino) = base_ino {
            if let Some(base_entries) = self.base.readdir(base_ino).await? {
                for entry in base_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry)
                    } else {
                        format!("{}/{}", info.path, entry)
                    };
                    if !self.is_whiteout(&entry_path) && !child_whiteouts.contains(&entry) {
                        entries.insert(entry);
                    }
                }
            }
        }

        let mut result: Vec<_> = entries.into_iter().collect();
        result.sort();
        Ok(Some(result))
    }

    async fn readdir_plus(&self, ino: i64) -> Result<Option<Vec<DirEntry>>> {
        crate::telemetry::record_readdir_plus();
        trace!("OverlayFS::readdir_plus: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let child_whiteouts = self.get_child_whiteouts(&info.path);

        let mut entries_map: HashMap<String, DirEntry> = HashMap::new();

        // Get base entries first (so delta can override)
        let base_ino = if info.layer == Layer::Base {
            Some(info.underlying_ino)
        } else {
            let components: Vec<&str> = info.path.split('/').filter(|s| !s.is_empty()).collect();
            let mut ino: i64 = 1;
            let mut found_all = true;
            crate::telemetry::record_path_resolution(components.len() as u64);
            for comp in &components {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    found_all = false;
                    break;
                }
            }
            if found_all {
                Some(ino)
            } else {
                None
            }
        };

        if let Some(base_ino) = base_ino {
            if let Some(base_entries) = self.base.readdir_plus(base_ino).await? {
                for mut entry in base_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", info.path, entry.name)
                    };

                    if !self.is_whiteout(&entry_path) && !child_whiteouts.contains(&entry.name) {
                        let overlay_ino = self.get_or_create_overlay_ino(
                            Layer::Base,
                            entry.stats.ino,
                            &entry_path,
                        );
                        entry.stats.ino = overlay_ino;
                        entries_map.insert(entry.name.clone(), entry);
                    }
                }
            }
        }

        // Get delta entries (override base)
        if info.layer == Layer::Delta {
            if let Some(delta_entries) = self.delta.readdir_plus(info.underlying_ino).await? {
                for mut entry in delta_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", info.path, entry.name)
                    };
                    if self.is_whiteout(&entry_path) || child_whiteouts.contains(&entry.name) {
                        continue;
                    }

                    // Check for origin mapping
                    let delta_ino = entry.stats.ino;
                    if let Some(base_ino) = self.get_origin_ino(entry.stats.ino) {
                        let overlay_ino =
                            self.get_or_create_overlay_ino(Layer::Delta, delta_ino, &entry_path);
                        if let Some(existing_ino) =
                            self.live_origin_overlay_ino(base_ino, &entry_path)
                        {
                            self.refresh_overlay_mapping(
                                existing_ino,
                                Layer::Delta,
                                delta_ino,
                                &entry_path,
                            );
                            entry.stats.ino = existing_ino;
                        } else {
                            entry.stats.ino = overlay_ino;
                        }
                    } else {
                        let overlay_ino = self.get_or_create_overlay_ino(
                            Layer::Delta,
                            entry.stats.ino,
                            &entry_path,
                        );
                        entry.stats.ino = overlay_ino;
                    }

                    entries_map.insert(entry.name.clone(), entry);
                }
            }
        }

        let mut result: Vec<_> = entries_map.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(result))
    }

    async fn chmod(&self, ino: i64, mode: u32) -> Result<()> {
        trace!("OverlayFS::chmod: ino={}, mode={:o}", ino, mode);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => {
                let base_stats = self
                    .base
                    .getattr(info.underlying_ino)
                    .await?
                    .ok_or(FsError::NotFound)?;
                if self.partial_origin_policy.permits(&base_stats) {
                    self.partial_copy_up_and_update_mapping(ino, &info).await?
                } else {
                    self.copy_up_and_update_mapping(ino, &info).await?
                }
            }
        };

        self.delta.chmod(delta_ino, mode).await
    }

    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        trace!(
            "OverlayFS::chown: ino={}, uid={:?}, gid={:?}",
            ino,
            uid,
            gid
        );

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => {
                let base_stats = self
                    .base
                    .getattr(info.underlying_ino)
                    .await?
                    .ok_or(FsError::NotFound)?;
                if self.partial_origin_policy.permits(&base_stats) {
                    self.partial_copy_up_and_update_mapping(ino, &info).await?
                } else {
                    self.copy_up_and_update_mapping(ino, &info).await?
                }
            }
        };

        self.delta.chown(delta_ino, uid, gid).await
    }

    async fn utimens(&self, ino: i64, atime: TimeChange, mtime: TimeChange) -> Result<()> {
        trace!("OverlayFS::utimens: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => {
                let base_stats = self
                    .base
                    .getattr(info.underlying_ino)
                    .await?
                    .ok_or(FsError::NotFound)?;
                if self.partial_origin_policy.permits(&base_stats) {
                    self.partial_copy_up_and_update_mapping(ino, &info).await?
                } else {
                    self.copy_up_and_update_mapping(ino, &info).await?
                }
            }
        };

        self.delta.utimens(delta_ino, atime, mtime).await
    }

    async fn keep_cache_for_read_open(&self, ino: i64, flags: i32) -> Result<Option<Stats>> {
        if is_write_open(flags) {
            return Ok(None);
        }

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        match info.layer {
            Layer::Base => Ok(None),
            // Delta (DB-backed) files inherit the AgentFS keep-cache policy:
            // the adapter fingerprint guard revalidates per open.
            Layer::Delta => {
                FileSystem::keep_cache_for_read_open(&self.delta, info.underlying_ino, flags).await
            }
        }
    }

    fn delta_keep_cache_fast_path(&self) -> bool {
        self.delta.delta_keep_cache_fast_path()
    }

    fn kernel_cache_policy(&self, ino: i64) -> KernelCachePolicy {
        match self.get_inode_info(ino) {
            Some(info) if info.layer == Layer::Base && !self.is_whiteout(&info.path) => {
                KernelCachePolicy::ExternalDrift
            }
            _ => KernelCachePolicy::Stable,
        }
    }

    async fn open(&self, ino: i64, flags: i32) -> Result<BoxedFile> {
        trace!("OverlayFS::open: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        match info.layer {
            Layer::Delta => {
                return self
                    .partial_file_for_delta(ino, info.underlying_ino, flags)
                    .await;
            }
            Layer::Base if !is_write_open(flags) => {
                let current = self
                    .resolve_base_path(&info.path)
                    .await?
                    .ok_or(FsError::NotFound)?;
                self.refresh_overlay_mapping(ino, Layer::Base, current.ino, &info.path);
                let base_file = self.base.open(current.ino, flags).await?;
                return Ok(mount_visible_file(base_file, ino));
            }
            Layer::Base => {
                let base_stats = self
                    .resolve_base_path(&info.path)
                    .await?
                    .ok_or(FsError::NotFound)?;
                self.refresh_overlay_mapping(ino, Layer::Base, base_stats.ino, &info.path);
                if self.partial_origin_policy.permits(&base_stats) {
                    let delta_ino = self.partial_copy_up_and_update_mapping(ino, &info).await?;
                    return self.partial_file_for_delta(ino, delta_ino, flags).await;
                }
            }
        }

        let delta_ino = self.copy_up_and_update_mapping(ino, &info).await?;

        FileSystem::open(&self.delta, delta_ino, flags).await
    }

    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        trace!("OverlayFS::mkdir: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if already exists
        if self.lookup(parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Remove whiteout if exists
        self.remove_whiteout(&path).await?;

        // Ensure parent dirs exist in delta
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats =
            FileSystem::mkdir(&self.delta, delta_parent_ino, name, mode, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok(stats)
    }

    async fn create_file(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Stats, BoxedFile)> {
        trace!(
            "OverlayFS::create_file: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Remove whiteout if exists
        self.remove_whiteout(&path).await?;

        // Ensure parent dirs exist in delta
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let (mut stats, file) =
            FileSystem::create_file(&self.delta, delta_parent_ino, name, mode, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok((stats, file))
    }

    async fn mknod(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        trace!("OverlayFS::mknod: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        self.remove_whiteout(&path).await?;
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats =
            FileSystem::mknod(&self.delta, delta_parent_ino, name, mode, rdev, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok(stats)
    }

    async fn symlink(
        &self,
        parent_ino: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        trace!(
            "OverlayFS::symlink: parent_ino={}, name={}, target={}",
            parent_ino,
            name,
            target
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        self.remove_whiteout(&path).await?;
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats =
            FileSystem::symlink(&self.delta, delta_parent_ino, name, target, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok(stats)
    }

    async fn unlink(&self, parent_ino: i64, name: &str) -> Result<()> {
        trace!(
            "OverlayFS::unlink: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if it exists
        let stats = self
            .lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound)?;
        if stats.is_directory() {
            return Err(FsError::IsADirectory.into());
        }

        // Try to remove from delta. Walk the delta layer to find the parent,
        // since the overlay parent may map to Base even when a copy-up exists in delta.
        if let Some(dpi) = self.resolve_delta_parent(&parent_info).await? {
            let removed_delta_ino = FileSystem::lookup(&self.delta, dpi, name)
                .await?
                .map(|stats| stats.ino);
            match FileSystem::unlink(&self.delta, dpi, name).await {
                Ok(()) => {}
                Err(crate::error::Error::Fs(FsError::NotFound)) => {}
                Err(e) => return Err(e),
            }
            if let Some(delta_ino) = removed_delta_ino {
                self.cleanup_partial_origin_if_unlinked(delta_ino).await?;
            }
        }

        // If the file is still visible through the overlay after delta removal,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(parent_ino, name).await?.is_some() {
            self.create_whiteout(&path).await?;
        }

        Ok(())
    }

    async fn rmdir(&self, parent_ino: i64, name: &str) -> Result<()> {
        trace!("OverlayFS::rmdir: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if it exists and is a directory
        let stats = self
            .lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound)?;
        if !stats.is_directory() {
            return Err(FsError::NotADirectory.into());
        }

        // Check if directory is empty (in overlay view)
        let dir_entries = self.readdir(stats.ino).await?.unwrap_or_default();
        if !dir_entries.is_empty() {
            return Err(FsError::NotEmpty.into());
        }

        // Try to remove from delta. Walk the delta layer to find the parent,
        // since the overlay parent may map to Base even when a copy-up exists in delta.
        if let Some(dpi) = self.resolve_delta_parent(&parent_info).await? {
            match FileSystem::rmdir(&self.delta, dpi, name).await {
                Ok(()) => {}
                Err(crate::error::Error::Fs(FsError::NotFound)) => {}
                Err(e) => return Err(e),
            }
        }

        // If the directory is still visible through the overlay after delta removal,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(parent_ino, name).await?.is_some() {
            self.create_whiteout(&path).await?;
        }

        Ok(())
    }

    async fn link(&self, ino: i64, newparent_ino: i64, newname: &str) -> Result<Stats> {
        trace!(
            "OverlayFS::link: ino={}, newparent_ino={}, newname={}",
            ino,
            newparent_ino,
            newname
        );

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let parent_info = self
            .get_inode_info(newparent_ino)
            .ok_or(FsError::NotFound)?;
        let new_path = self.build_path(newparent_ino, newname)?;

        // Ensure file is in delta (copy up if needed)
        let delta_ino = if info.layer == Layer::Delta {
            info.underlying_ino
        } else {
            self.copy_up(&info.path, info.underlying_ino).await?
        };

        self.remove_whiteout(&new_path).await?;
        self.ensure_parent_dirs(&new_path, 0, 0).await?;

        // Resolve delta parent AFTER ensure_parent_dirs so the directories exist.
        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats = FileSystem::link(&self.delta, delta_ino, delta_parent_ino, newname).await?;
        stats.ino = ino; // Keep original overlay inode

        Ok(stats)
    }

    async fn rename(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> Result<()> {
        self.rename_with_replaced_ino(oldparent_ino, oldname, newparent_ino, newname)
            .await
            .map(|_| ())
    }

    async fn rename_with_replaced_ino(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> Result<Option<i64>> {
        trace!(
            "OverlayFS::rename: oldparent={}, oldname={}, newparent={}, newname={}",
            oldparent_ino,
            oldname,
            newparent_ino,
            newname
        );

        let old_parent_info = self
            .get_inode_info(oldparent_ino)
            .ok_or(FsError::NotFound)?;
        let new_parent_info = self
            .get_inode_info(newparent_ino)
            .ok_or(FsError::NotFound)?;
        let old_path = self.build_path(oldparent_ino, oldname)?;
        let new_path = self.build_path(newparent_ino, newname)?;

        // Get source stats
        let src_stats = self
            .lookup(oldparent_ino, oldname)
            .await?
            .ok_or(FsError::NotFound)?;
        let src_info = self
            .get_inode_info(src_stats.ino)
            .ok_or(FsError::NotFound)?;

        // A base-origin directory copy-up only creates the directory itself,
        // not its subtree. ensure_parent_dirs can promote such a directory to
        // Layer::Delta after a single child write, so the layer tag alone is not
        // a safe origin test. If the old path still resolves to a visible base
        // directory, return EXDEV so user-space callers such as `mv` perform
        // copy+delete.
        if src_stats.is_directory() && self.resolves_to_visible_base_directory(&old_path).await? {
            return Err(FsError::CrossDevice.into());
        }

        let replaced_ino = self
            .lookup(newparent_ino, newname)
            .await?
            .map(|stats| stats.ino);

        // Remove any destination whiteout before source copy-up. The whiteout
        // mutation runs in its own IMMEDIATE transaction and can fail under
        // fault injection; doing it first prevents a failed rename from leaving
        // source copy-up/origin state behind.
        self.remove_whiteout(&new_path).await?;

        // Ensure source is in delta after destination whiteout removal succeeds.
        let delta_src_ino = if src_info.layer == Layer::Base {
            self.copy_up(&old_path, src_info.underlying_ino).await?
        } else {
            src_info.underlying_ino
        };

        self.ensure_parent_dirs(&new_path, 0, 0).await?;

        // Resolve delta parents AFTER copy_up / ensure_parent_dirs,
        // since those create the parent directories in delta.
        let delta_src_parent_ino = self
            .resolve_delta_parent(&old_parent_info)
            .await?
            .ok_or(FsError::NotFound)?;
        let delta_dst_parent_ino = self
            .resolve_delta_parent(&new_parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        // Perform rename in delta
        FileSystem::rename(
            &self.delta,
            delta_src_parent_ino,
            oldname,
            delta_dst_parent_ino,
            newname,
        )
        .await?;
        self.refresh_overlay_mapping(src_stats.ino, Layer::Delta, delta_src_ino, &new_path);

        // If the old file is still visible through the overlay after the rename,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(oldparent_ino, oldname).await?.is_some() {
            self.create_whiteout(&old_path).await?;
        }

        Ok(replaced_ino)
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        FileSystem::statfs(&self.delta).await
    }

    async fn drain_inode_writes(&self, ino: i64) -> Result<()> {
        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        match info.layer {
            Layer::Delta => FileSystem::drain_inode_writes(&self.delta, info.underlying_ino).await,
            Layer::Base => self.base.drain_inode_writes(info.underlying_ino).await,
        }
    }

    async fn drain_all(&self) -> Result<()> {
        FileSystem::drain_all(&self.delta).await?;
        self.base.drain_all().await?;
        Ok(())
    }

    async fn finalize(&self) -> Result<()> {
        FileSystem::finalize(&self.delta).await?;
        self.base.finalize().await?;
        Ok(())
    }

    async fn retain_lookup(&self, ino: i64, nlookup: u64) -> Result<()> {
        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        match info.layer {
            Layer::Delta => {
                FileSystem::retain_lookup(&self.delta, info.underlying_ino, nlookup).await?
            }
            Layer::Base => {
                self.base
                    .retain_lookup(info.underlying_ino, nlookup)
                    .await?
            }
        }
        self.retain_overlay_lookup(ino, nlookup);
        Ok(())
    }

    async fn forget(&self, ino: i64, nlookup: u64) {
        // Look up the inode info to determine which layer it belongs to before
        // pruning the overlay maps.
        let info = match self.get_inode_info(ino) {
            Some(i) => i,
            None => return, // Unknown inode, nothing to forget
        };

        match info.layer {
            Layer::Delta => {
                // Delta (AgentFS) doesn't cache fds, but call it anyway for completeness.
                FileSystem::forget(&self.delta, info.underlying_ino, nlookup).await;
            }
            Layer::Base => {
                // Base layer (HostFS) caches O_PATH fds and needs forget.
                self.base.forget(info.underlying_ino, nlookup).await;
            }
        }

        self.forget_overlay_lookup(ino, nlookup);
    }
}
