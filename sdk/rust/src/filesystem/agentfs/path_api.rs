//! Path-based AgentFS convenience delegates.
//!
//! These are the six live path helpers retained after M2. Each resolves a
//! path to the canonical inode-oriented `FileSystem` trait operation so
//! metadata, batching, and mutation semantics have one implementation.

use std::path::Path;

use crate::filesystem::FileSystem;

use super::*;

impl AgentFS {
    /// Get file statistics, following symlinks.
    pub async fn stat(&self, path: &str) -> Result<Option<Stats>> {
        let path = self.normalize_path(path);
        let mut current_path = path;
        for _ in 0..40 {
            let ino = match self.resolve_path(&current_path).await? {
                Some(ino) => ino,
                None => return Ok(None),
            };
            let Some(stats) = FileSystem::getattr(self, ino).await? else {
                return Ok(None);
            };
            if !stats.is_symlink() {
                return Ok(Some(stats));
            }

            let target = FileSystem::readlink(self, ino)
                .await?
                .ok_or(FsError::NotFound)?;
            current_path = if target.starts_with('/') {
                target
            } else {
                let base_path = Path::new(&current_path);
                let parent = base_path.parent().unwrap_or(Path::new("/"));
                parent.join(&target).to_string_lossy().into_owned()
            };
            current_path = self.normalize_path(&current_path);
        }

        Err(FsError::SymlinkLoop.into())
    }

    /// Create a directory
    pub async fn mkdir(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let (parent_ino, name) = self.resolve_parent_and_name(path).await?;
        FileSystem::mkdir(self, parent_ino, &name, DEFAULT_DIR_MODE, uid, gid).await?;
        Ok(())
    }

    /// Create a new empty file with the specified mode and ownership.
    ///
    /// This is an optimized path for FUSE create() that combines inode creation,
    /// dentry creation, and file handle opening in a single operation.
    /// Returns both Stats and an open file handle.
    pub async fn create_file(
        &self,
        path: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Stats, BoxedFile)> {
        let (parent_ino, name) = self.resolve_parent_and_name(path).await?;
        FileSystem::create_file(self, parent_ino, &name, mode, uid, gid).await
    }

    /// Read data from a file
    pub async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let stats = match self.stat(path).await? {
            Some(stats) => stats,
            None => return Ok(None),
        };
        let file = FileSystem::open(self, stats.ino, libc::O_RDONLY).await?;
        let size = u64::try_from(stats.size).unwrap_or(0);
        Ok(Some(file.pread(0, size).await?))
    }

    /// Remove a file or empty directory
    pub async fn remove(&self, path: &str) -> Result<()> {
        let (parent_ino, name) = self.resolve_parent_and_name(path).await?;
        let stats = FileSystem::lookup(self, parent_ino, &name)
            .await?
            .ok_or(FsError::NotFound)?;
        if stats.is_directory() {
            FileSystem::rmdir(self, parent_ino, &name).await
        } else {
            FileSystem::unlink(self, parent_ino, &name).await
        }
    }

    /// Synchronize file data to persistent storage
    ///
    /// Temporarily enables FULL synchronous mode, runs a transaction to force
    /// a checkpoint, then restores NORMAL mode. This ensures durability while
    /// maintaining high performance for normal operations.
    ///
    pub async fn fsync(&self) -> Result<()> {
        FileSystem::drain_all(self).await?;
        let conn = self.pool.get_connection().await?;
        conn.prepare_cached(DURABLE_SYNCHRONOUS_SQL)
            .await?
            .execute(())
            .await?;
        checkpoint_wal(&conn).await?;
        conn.prepare_cached(BASELINE_SYNCHRONOUS_SQL)
            .await?
            .execute(())
            .await?;
        Ok(())
    }
}
