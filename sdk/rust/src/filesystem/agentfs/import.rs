//! Bulk import session for AgentFS clone and import flows.
//!
//! Import keeps one pooled connection and a directory inode map across chunks,
//! committing entries in bounded transactions sized by the batcher config.
//! The transaction chunking defaults are clone-performance-sensitive.

use std::collections::HashMap;

use turso::transaction::{Transaction, TransactionBehavior};
use turso::Value;

use crate::error::Error;

use super::*;

/// One node for [`AgentFS::import_entries`]. `path` is relative to the import
/// root and '/'-separated; parent directories must precede their children.
#[derive(Debug, Clone)]
pub struct ImportEntry {
    pub path: String,
    /// Full `st_mode` bits (S_IFDIR / S_IFREG / S_IFLNK plus permissions).
    pub mode: u32,
    /// File content, or the symlink target bytes; empty for directories.
    pub data: Vec<u8>,
}

/// Result row for one imported node: echoes the exact `ino`/`mode`/`size`
/// the filesystem will serve so callers can fabricate externally-consistent
/// stat metadata (e.g. a git index) without re-reading content.
#[derive(Debug, Clone)]
pub struct ImportedEntry {
    pub path: String,
    pub ino: i64,
    pub mode: u32,
    pub size: u64,
}

/// Ownership and timestamps applied to every node of one bulk import.
#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub uid: u32,
    pub gid: u32,
    /// (secs, nanos) stamped as atime/mtime/ctime on every imported inode.
    pub timestamp: (i64, i64),
}

/// A streaming bulk import started by [`AgentFS::begin_import`]. Holds one
/// pooled connection plus the directory-path -> ino map across
/// [`ImportSession::import_chunk`] calls, so a producer can feed entries as
/// they become available (e.g. as `git cat-file --batch` emits blobs)
/// instead of buffering the whole tree in memory. The ordering contract
/// matches [`AgentFS::import_entries`]: every parent directory must appear
/// in some chunk before (or in the same chunk as) its children.
pub struct ImportSession {
    fs: AgentFS,
    conn: crate::connection_pool::PooledConnection,
    dest_parent: i64,
    opts: ImportOptions,
    dir_inos: HashMap<String, i64>,
    results: Vec<ImportedEntry>,
}

impl ImportSession {
    /// Import one batch of entries. Parent directories imported by earlier
    /// chunks (or earlier in this chunk) resolve normally; a parent that has
    /// never been imported yields `FsError::NotFound`.
    pub async fn import_chunk(&mut self, entries: &[ImportEntry]) -> Result<()> {
        self.fs
            .import_chunk_with_conn(
                &self.conn,
                self.dest_parent,
                &self.opts,
                &mut self.dir_inos,
                &mut self.results,
                entries,
            )
            .await
    }

    /// Finish the import and return one [`ImportedEntry`] per imported node,
    /// in the order the entries were fed.
    pub fn finish(self) -> Vec<ImportedEntry> {
        self.fs.invalidate_attr(self.dest_parent);
        self.results
    }
}

impl AgentFS {
    /// Bulk-import a tree of nodes under `dest_parent` using large
    /// multi-inode transactions instead of one transaction per node, sized by
    /// the write batcher's txn limits (`AGENTFS_BATCH_TXN_INODES` /
    /// `AGENTFS_BATCH_TXN_BYTES`). This is the fast path for populating the
    /// database without per-file FUSE round trips (`agentfs clone` / `fs
    /// import`): a 4.7k-file worktree pays a handful of commits instead of
    /// ~9.4k per-file create+write transaction boundaries.
    ///
    /// Entries must be ordered parents-before-children; every parent
    /// directory of a nested path must itself appear as an entry (or be the
    /// import root). All inodes are stamped with `opts.timestamp`, and the
    /// returned rows echo the exact `ino`/`mode`/`size` the filesystem will
    /// serve, so callers can fabricate externally-consistent stat metadata
    /// (e.g. a git index) without re-reading anything.
    pub async fn import_entries(
        &self,
        dest_parent: i64,
        entries: &[ImportEntry],
        opts: &ImportOptions,
    ) -> Result<Vec<ImportedEntry>> {
        let mut session = self.begin_import(dest_parent, opts.clone()).await?;
        session.import_chunk(entries).await?;
        Ok(session.finish())
    }

    /// Begin a streaming bulk import under `dest_parent`; see
    /// [`ImportSession`]. [`AgentFS::import_entries`] is the buffered
    /// one-shot form.
    pub async fn begin_import(
        &self,
        dest_parent: i64,
        opts: ImportOptions,
    ) -> Result<ImportSession> {
        Ok(ImportSession {
            fs: self.clone(),
            conn: self.pool.get_connection().await?,
            dest_parent,
            opts,
            dir_inos: HashMap::new(),
            results: Vec::new(),
        })
    }

    /// One chunk of a streaming import. `conn`, `dir_inos`, and `results`
    /// persist across calls so later chunks may reference directories
    /// imported by earlier ones; each call still splits its entries into
    /// bounded transactions.
    async fn import_chunk_with_conn(
        &self,
        conn: &crate::connection_pool::PooledConnection,
        dest_parent: i64,
        opts: &ImportOptions,
        dir_inos: &mut HashMap<String, i64>,
        results: &mut Vec<ImportedEntry>,
        entries: &[ImportEntry],
    ) -> Result<()> {
        let max_inodes = self.core_config.batcher.txn_max_inodes.max(1);
        let max_bytes = self.core_config.batcher.txn_max_bytes.max(1);
        let (ts_secs, ts_nsec) = opts.timestamp;

        let mut inode_stmt = conn
            .prepare_cached(
                "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, atime_nsec, mtime_nsec, ctime_nsec, data_inline, storage_kind)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING ino",
            )
            .await?;
        let mut dentry_stmt = conn
            .prepare_cached("INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)")
            .await?;
        let mut chunk_stmt = conn
            .prepare_cached("INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)")
            .await?;
        let mut symlink_stmt = conn
            .prepare_cached("INSERT INTO fs_symlink (ino, target) VALUES (?, ?)")
            .await?;
        let mut parent_stmt = conn
            .prepare_cached(
                "UPDATE fs_inode SET nlink = nlink + ?, ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?",
            )
            .await?;

        results.reserve(entries.len());

        let mut idx = 0usize;
        while idx < entries.len() {
            let mut batch_end = idx;
            let mut batch_bytes = 0usize;
            while batch_end < entries.len()
                && batch_end - idx < max_inodes
                && (batch_end == idx || batch_bytes + entries[batch_end].data.len() <= max_bytes)
            {
                batch_bytes += entries[batch_end].data.len();
                batch_end += 1;
            }

            // Cache fills staged until after a successful commit so a rolled
            // back batch never leaves phantom dentries/attrs behind.
            let mut staged: Vec<(i64, String, Stats)> = Vec::with_capacity(batch_end - idx);
            // parent ino -> nlink bump from new subdirectories ("..").
            let mut parent_bumps: HashMap<i64, i64> = HashMap::new();

            let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
            for entry in &entries[idx..batch_end] {
                let (parent_path, name) = match entry.path.rsplit_once('/') {
                    Some((parent, name)) => (parent, name),
                    None => ("", entry.path.as_str()),
                };
                if name.is_empty() || name == "." || name == ".." {
                    return Err(FsError::InvalidPath.into());
                }
                if name.len() > MAX_NAME_LEN {
                    return Err(FsError::NameTooLong.into());
                }
                let parent_ino = if parent_path.is_empty() {
                    dest_parent
                } else {
                    *dir_inos
                        .get(parent_path)
                        .ok_or_else(|| Error::Fs(FsError::NotFound))?
                };

                let kind = entry.mode & S_IFMT;
                let (nlink, size, data_inline, storage_kind) = match kind {
                    S_IFDIR => (2i64, 0u64, Value::Null, STORAGE_CHUNKED),
                    S_IFLNK => (1, entry.data.len() as u64, Value::Null, STORAGE_CHUNKED),
                    S_IFREG => {
                        if entry.data.len() <= self.inline_threshold {
                            (
                                1,
                                entry.data.len() as u64,
                                Value::Blob(entry.data.clone()),
                                STORAGE_INLINE,
                            )
                        } else {
                            (1, entry.data.len() as u64, Value::Null, STORAGE_CHUNKED)
                        }
                    }
                    _ => return Err(FsError::InvalidPath.into()),
                };

                let row = inode_stmt
                    .query_row((
                        entry.mode as i64,
                        nlink,
                        opts.uid,
                        opts.gid,
                        size as i64,
                        ts_secs,
                        ts_secs,
                        ts_secs,
                        ts_nsec,
                        ts_nsec,
                        ts_nsec,
                        data_inline,
                        storage_kind,
                    ))
                    .await?;
                let ino = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .ok_or_else(|| Error::Internal("failed to get inode".to_string()))?;

                match dentry_stmt.execute((name, parent_ino, ino)).await {
                    Ok(_) => {}
                    Err(turso::Error::Constraint(_)) => return Err(FsError::AlreadyExists.into()),
                    Err(error) => return Err(error.into()),
                }

                match kind {
                    S_IFDIR => {
                        dir_inos.insert(entry.path.clone(), ino);
                        *parent_bumps.entry(parent_ino).or_insert(0) += 1;
                    }
                    S_IFLNK => {
                        let target = std::str::from_utf8(&entry.data)
                            .map_err(|_| Error::Fs(FsError::InvalidPath))?;
                        symlink_stmt.execute((ino, target)).await?;
                        parent_bumps.entry(parent_ino).or_insert(0);
                    }
                    _ => {
                        if storage_kind == STORAGE_CHUNKED {
                            for (chunk_index, chunk) in
                                entry.data.chunks(self.chunk_size).enumerate()
                            {
                                chunk_stmt
                                    .execute((ino, chunk_index as i64, Value::Blob(chunk.to_vec())))
                                    .await?;
                            }
                        }
                        parent_bumps.entry(parent_ino).or_insert(0);
                    }
                }

                staged.push((
                    parent_ino,
                    name.to_string(),
                    Stats {
                        ino,
                        mode: entry.mode,
                        nlink: nlink as u32,
                        uid: opts.uid,
                        gid: opts.gid,
                        size: size as i64,
                        atime: ts_secs,
                        mtime: ts_secs,
                        ctime: ts_secs,
                        atime_nsec: ts_nsec as u32,
                        mtime_nsec: ts_nsec as u32,
                        ctime_nsec: ts_nsec as u32,
                        rdev: 0,
                    },
                ));
                results.push(ImportedEntry {
                    path: entry.path.clone(),
                    ino,
                    mode: entry.mode,
                    size,
                });
            }

            for (parent_ino, bump) in &parent_bumps {
                parent_stmt
                    .execute((*bump, ts_secs, ts_secs, ts_nsec, ts_nsec, *parent_ino))
                    .await?;
            }

            txn.commit().await?;
            #[cfg(test)]
            self.import_commit_sizes.lock().unwrap().push(staged.len());
            crate::profiling::record_agentfs_batcher_commit_txn(staged.len() as u64);

            for (parent_ino, name, stats) in staged {
                self.cache_dentry(parent_ino, &name, stats.ino);
                // Directories keep changing (nlink/time bumps as later batches
                // add children), so only leaf attrs are safe to prime.
                if stats.mode & S_IFMT != S_IFDIR {
                    self.cache_attr(stats);
                }
            }
            for parent_ino in parent_bumps.keys() {
                self.invalidate_attr(*parent_ino);
            }

            idx = batch_end;
        }

        Ok(())
    }
}
