//! Canonical `FileSystem` trait implementation for AgentFS.
//!
//! This module is the only AgentFS mutation implementation. Path helpers, CLI
//! surfaces, FUSE, and NFS resolve into these inode-oriented operations so
//! namespace, metadata, lifecycle, and batcher semantics cannot diverge.

use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use turso::transaction::{Transaction, TransactionBehavior};
use turso::Value;

use crate::error::Error;
use crate::fs::{DirEntry, DirEntryPage, FileSystem, TimeChange};

use super::batcher::PendingTimeChange;
use super::*;

#[async_trait]
impl FileSystem for AgentFS {
    async fn lookup(&self, parent_ino: i64, name: &str) -> Result<Option<Stats>> {
        crate::telemetry::record_lookup();
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }

        // Connection-free fast paths via the in-memory caches. These are the
        // same caches (and invalidation semantics) that `lookup_child` already
        // trusts; consulting them BEFORE acquiring a pool connection avoids a
        // wasted acquire/release on every cache hit. This is the clone hot
        // path: `OverlayFS::resolve_delta_parent` does O(depth) negative
        // delta-parent probes per base-layer lookup, all of which are negative
        // cache hits that previously each took a connection.
        if name != ".." {
            if self.negative_dentry_cache.contains(parent_ino, name) {
                crate::telemetry::record_negative_lookup();
                return Ok(None);
            }
            if let Some(child_ino) = self.dentry_cache.get(parent_ino, name) {
                if let Some(mut stats) = self.attr_cache.get(child_ino) {
                    self.merge_pending_view(child_ino, Some(&mut stats));
                    return Ok(Some(stats));
                }
            }
        }

        let conn = self.pool.get_connection().await?;

        // Handle ".." by finding the parent of parent_ino
        if name == ".." {
            if parent_ino == ROOT_INO {
                // Root's parent is itself
                return self.getattr_with_conn(&conn, ROOT_INO).await;
            }
            let mut stmt = conn
                .prepare_cached("SELECT parent_ino FROM fs_dentry WHERE ino = ? LIMIT 1")
                .await?;
            let mut rows = stmt.query((parent_ino,)).await?;
            let parent = if let Some(row) = rows.next().await? {
                row.get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(ROOT_INO)
            } else {
                ROOT_INO
            };
            return self.getattr_with_conn(&conn, parent).await;
        }

        // Look up the child inode
        let child_ino = match self.lookup_child(&conn, parent_ino, name).await? {
            Some(ino) => ino,
            None => {
                crate::telemetry::record_negative_lookup();
                return Ok(None);
            }
        };
        let generation = self.pending_generation(child_ino);
        // Tier Four: do NOT call `drain_inode_writes` here. The single-
        // connection ephemeral pool (and even the file-backed pool under
        // contention) would deadlock — we already hold the only connection
        // permit, and `drain_inode_writes` -> `drain_pending_batched` tries
        // to acquire one. Read SQLite, then merge the batcher's pending
        // max-end into the size field the same way `getattr` does.

        // Get stats for the child inode
        let mut stmt = conn
            .prepare_cached("SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((child_ino,)).await?;

        if let Some(row) = rows.next().await? {
            let mut stats = store::stats_from_row(&row)?;
            self.merge_pending_view(child_ino, Some(&mut stats));
            // Cache the lookup result
            self.cache_dentry(parent_ino, name, child_ino);
            self.cache_attr_if_pending_generation(stats.clone(), generation);
            Ok(Some(stats))
        } else {
            Ok(None)
        }
    }

    async fn getattr(&self, ino: i64) -> Result<Option<Stats>> {
        crate::telemetry::record_getattr();
        // Connection-free fast path: an attr-cache hit needs no pool connection.
        // The cache is invalidated on every write (enqueue removes the entry),
        // so a hit means there is no uncommitted pending write to merge; the
        // merge below is therefore an idempotent no-op but is kept for safety.
        // Same cache `getattr_with_conn` already trusts, consulted before the
        // acquire.
        if let Some(mut stats) = self.attr_cache.get(ino) {
            self.merge_pending_view(ino, Some(&mut stats));
            return Ok(Some(stats));
        }
        // Tier Four: don't drain — read SQLite metadata and OR in the
        // batcher's peek_pending_max_end so the size view reflects pending
        // writes that haven't been committed yet. Refresh the attr cache
        // with the merged size so subsequent direct cache reads agree with
        // what we just returned.
        let conn = self.pool.get_connection().await?;
        self.getattr_with_conn(&conn, ino).await
    }

    /// DB-backed regular files qualify for `FOPEN_KEEP_CACHE`: every mutation
    /// path through a mount is kernel-originated (the kernel's pages stay
    /// coherent for its own writes) and the adapter's fingerprint guard
    /// revalidates mtime/ctime/size at each open, so out-of-band SDK writers
    /// are caught exactly like external edits to host-backed base files.
    /// The keepcache-delta kill switch restores the old policy where only
    /// host-backed base-layer files were eligible.
    async fn keep_cache_for_read_open(&self, ino: i64, flags: i32) -> Result<Option<Stats>> {
        if (flags & libc::O_ACCMODE) != libc::O_RDONLY || (flags & libc::O_TRUNC) != 0 {
            return Ok(None);
        }
        if !self.core_config.keepcache_delta {
            return Ok(None);
        }
        let Some(stats) = FileSystem::getattr(self, ino).await? else {
            return Ok(None);
        };
        Ok(stats.is_file().then_some(stats))
    }

    fn delta_keep_cache_fast_path(&self) -> bool {
        self.core_config.keepcache_delta
    }

    async fn readlink(&self, ino: i64) -> Result<Option<String>> {
        let conn = self.pool.get_connection().await?;

        // Check if the inode exists and is a symlink
        if let Some(mode) = store::mode(&conn, ino).await? {
            if (mode & S_IFMT) != S_IFLNK {
                return Err(FsError::NotASymlink.into());
            }
        } else {
            return Ok(None);
        }

        // Read target from fs_symlink table
        let mut stmt = conn
            .prepare_cached("SELECT target FROM fs_symlink WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if let Some(row) = rows.next().await? {
            let target = row
                .get_value(0)
                .ok()
                .and_then(|v| match v {
                    Value::Text(s) => Some(s.to_string()),
                    _ => None,
                })
                .ok_or(FsError::InvalidPath)?;
            Ok(Some(target))
        } else {
            Ok(None)
        }
    }

    async fn readdir(&self, ino: i64) -> Result<Option<Vec<String>>> {
        crate::telemetry::record_readdir();
        let conn = self.pool.get_connection().await?;

        // Check if inode exists and is a directory
        if let Some(mode) = store::mode(&conn, ino).await? {
            if (mode & S_IFMT) != super::S_IFDIR {
                return Err(FsError::NotADirectory.into());
            }
        } else {
            return Ok(None);
        }

        let mut stmt = conn
            .prepare_cached("SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            let name = row
                .get_value(0)
                .ok()
                .and_then(|v| {
                    if let Value::Text(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            if !name.is_empty() {
                entries.push(name);
            }
        }

        Ok(Some(entries))
    }

    async fn readdir_plus(&self, ino: i64) -> Result<Option<Vec<DirEntry>>> {
        crate::telemetry::record_readdir_plus();
        self.drain_all().await?;
        let conn = self.pool.get_connection().await?;

        // Check if inode exists and is a directory
        if let Some(mode) = store::mode(&conn, ino).await? {
            if (mode & S_IFMT) != super::S_IFDIR {
                return Err(FsError::NotADirectory.into());
            }
        } else {
            return Ok(None);
        }

        let mut stmt = conn.prepare_cached("SELECT d.name, i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime, i.rdev, i.atime_nsec, i.mtime_nsec, i.ctime_nsec
            FROM fs_dentry d
            JOIN fs_inode i ON d.ino = i.ino
            WHERE d.parent_ino = ?
            ORDER BY d.name"
        ).await?;
        let mut rows = stmt.query((ino,)).await?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            let name = row
                .get_value(0)
                .ok()
                .and_then(|v| {
                    if let Value::Text(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            if name.is_empty() {
                continue;
            }

            let stats = store::stats_from_row_at(&row, 1)?;

            self.cache_attr(stats.clone());
            entries.push(DirEntry { name, stats });
        }

        Ok(Some(entries))
    }

    async fn readdir_plus_after(
        &self,
        ino: i64,
        start_after: i64,
        max_entries: usize,
    ) -> Result<Option<DirEntryPage>> {
        crate::telemetry::record_readdir_plus();
        self.drain_all().await?;
        let conn = self.pool.get_connection().await?;

        // Check if inode exists and is a directory
        if let Some(mode) = store::mode(&conn, ino).await? {
            if (mode & S_IFMT) != super::S_IFDIR {
                return Err(FsError::NotADirectory.into());
            }
        } else {
            return Ok(None);
        }

        let start_after_name = if start_after > 0 {
            let mut stmt = conn
                .prepare_cached("SELECT name FROM fs_dentry WHERE parent_ino = ? AND ino = ?")
                .await?;
            let mut rows = stmt.query((ino, start_after)).await?;
            match rows.next().await? {
                Some(row) => Some(
                    row.get_value(0)
                        .ok()
                        .and_then(|v| match v {
                            Value::Text(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default(),
                ),
                None => {
                    return Ok(Some(DirEntryPage {
                        entries: Vec::new(),
                        end: true,
                    }));
                }
            }
        } else {
            None
        };

        let fetch_limit = max_entries.saturating_add(1).min(i64::MAX as usize) as i64;
        let mut stmt = if start_after_name.is_some() {
            conn.prepare_cached(
                "SELECT d.name, i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime, i.rdev, i.atime_nsec, i.mtime_nsec, i.ctime_nsec
                FROM fs_dentry d
                JOIN fs_inode i ON d.ino = i.ino
                WHERE d.parent_ino = ? AND d.name > ?
                ORDER BY d.parent_ino, d.name
                LIMIT ?",
            )
            .await?
        } else {
            conn.prepare_cached(
                "SELECT d.name, i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime, i.rdev, i.atime_nsec, i.mtime_nsec, i.ctime_nsec
                FROM fs_dentry d
                JOIN fs_inode i ON d.ino = i.ino
                WHERE d.parent_ino = ?
                ORDER BY d.parent_ino, d.name
                LIMIT ?",
            )
            .await?
        };

        let mut rows = if let Some(start_after_name) = start_after_name {
            stmt.query((ino, start_after_name, fetch_limit)).await?
        } else {
            stmt.query((ino, fetch_limit)).await?
        };

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await? {
            let name = row
                .get_value(0)
                .ok()
                .and_then(|v| {
                    if let Value::Text(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            if name.is_empty() {
                continue;
            }

            let stats = store::stats_from_row_at(&row, 1)?;

            self.cache_attr(stats.clone());
            entries.push(DirEntry { name, stats });
        }

        let end = entries.len() <= max_entries;
        entries.truncate(max_entries);
        Ok(Some(DirEntryPage { entries, end }))
    }

    async fn chmod(&self, ino: i64, mode: u32) -> Result<()> {
        self.prepare_attr_change(ino).await?;
        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE so this serialises with concurrent batcher drain
        // transactions instead of racing them as an autocommit statement
        // (turso reports such write/write races as "database snapshot is
        // stale" instead of waiting on the write lock).
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<()> = async {
            // Get current mode to preserve file type bits
            let current_mode = store::mode(&conn, ino).await?.ok_or(FsError::NotFound)?;

            // Preserve file type bits (upper bits), replace permission bits (lower 12 bits)
            let new_mode = (current_mode & S_IFMT) | (mode & 0o7777);

            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE fs_inode SET mode = ?, ctime = ?, ctime_nsec = ? WHERE ino = ?",
                )
                .await?;
            stmt.execute((new_mode as i64, now_secs, now_nsec, ino))
                .await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.invalidate_attr(ino);
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        if uid.is_none() && gid.is_none() {
            return Ok(());
        }
        self.prepare_attr_change(ino).await?;

        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE: see `chmod` — avoid autocommit write/write races
        // with concurrent batcher drain transactions.
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<()> = async {
            // Verify inode exists
            let mut stmt = conn
                .prepare_cached("SELECT ino FROM fs_inode WHERE ino = ?")
                .await?;
            let mut rows = stmt.query((ino,)).await?;

            if rows.next().await?.is_none() {
                return Err(FsError::NotFound.into());
            }

            // Build the update query dynamically based on which values are provided
            let mut updates = Vec::new();
            let mut values: Vec<Value> = Vec::new();

            if let Some(uid) = uid {
                updates.push("uid = ?");
                values.push(Value::Integer(uid as i64));
            }
            if let Some(gid) = gid {
                updates.push("gid = ?");
                values.push(Value::Integer(gid as i64));
            }

            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            updates.push("ctime = ?");
            values.push(Value::Integer(now_secs));
            updates.push("ctime_nsec = ?");
            values.push(Value::Integer(now_nsec));

            values.push(Value::Integer(ino));
            let sql = format!("UPDATE fs_inode SET {} WHERE ino = ?", updates.join(", "));
            conn.execute(&sql, values).await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.invalidate_attr(ino);
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    async fn utimens(&self, ino: i64, atime: TimeChange, mtime: TimeChange) -> Result<()> {
        if matches!(atime, TimeChange::Omit) && matches!(mtime, TimeChange::Omit) {
            return Ok(());
        }

        // Group-commit fast path: with FUSE writeback caching the kernel sends
        // one SETATTR (mtime) per freshly written file, usually while that
        // file's data is pending in the write batcher (and sometimes after it
        // already drained). Instead of paying a dedicated SQLite transaction
        // per file for the time UPDATE, stash the resolved values in the
        // inode's pending entry (created on demand) — the batcher commits them
        // inside its next drain transaction (`apply_pending_times_with_conn`),
        // and `merge_pending_view` overlays them onto getattr/lookup results so
        // the change is visible immediately. Falls through to the direct
        // (transaction-wrapped) UPDATE when overlay reads are disabled or the
        // legacy drain is requested.
        if !self.core_config.drain_on_setattr && self.overlay_reads {
            if let Some(drain) = &self.write_drain {
                let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
                let now = (dur.as_secs() as i64, dur.subsec_nanos() as i64);
                let resolve = |tc: TimeChange| -> Option<(i64, i64)> {
                    match tc {
                        TimeChange::Set(secs, nsec) => Some((secs, nsec as i64)),
                        TimeChange::Now => Some(now),
                        TimeChange::Omit => None,
                    }
                };
                let change = PendingTimeChange {
                    atime: resolve(atime),
                    mtime: resolve(mtime),
                    // utimens always bumps ctime.
                    ctime: Some(now),
                };
                drain.stash_times(ino, change);
                self.invalidate_attr(ino);
                return Ok(());
            }
        }

        self.prepare_attr_change(ino).await?;
        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE: see `chmod` — avoid autocommit write/write races
        // with concurrent batcher drain transactions.
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<()> = async {
            // Verify inode exists
            let mut stmt = conn
                .prepare_cached("SELECT ino FROM fs_inode WHERE ino = ?")
                .await?;
            let mut rows = stmt.query((ino,)).await?;
            if rows.next().await?.is_none() {
                return Err(FsError::NotFound.into());
            }

            let mut updates = Vec::new();
            let mut values: Vec<Value> = Vec::new();

            let resolve = |tc: TimeChange| -> (i64, i64) {
                match tc {
                    TimeChange::Set(secs, nsec) => (secs, nsec as i64),
                    TimeChange::Now => {
                        let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
                        (dur.as_secs() as i64, dur.subsec_nanos() as i64)
                    }
                    TimeChange::Omit => unreachable!(),
                }
            };

            if !matches!(atime, TimeChange::Omit) {
                let (secs, nsec) = resolve(atime);
                updates.push("atime = ?");
                values.push(Value::Integer(secs));
                updates.push("atime_nsec = ?");
                values.push(Value::Integer(nsec));
            }

            if !matches!(mtime, TimeChange::Omit) {
                let (secs, nsec) = resolve(mtime);
                updates.push("mtime = ?");
                values.push(Value::Integer(secs));
                updates.push("mtime_nsec = ?");
                values.push(Value::Integer(nsec));
            }

            // Also update ctime
            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            updates.push("ctime = ?");
            values.push(Value::Integer(dur.as_secs() as i64));
            updates.push("ctime_nsec = ?");
            values.push(Value::Integer(dur.subsec_nanos() as i64));

            values.push(Value::Integer(ino));
            let sql = format!("UPDATE fs_inode SET {} WHERE ino = ?", updates.join(", "));
            conn.execute(&sql, values).await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.invalidate_attr(ino);
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    async fn open(&self, ino: i64, _flags: i32) -> Result<BoxedFile> {
        let conn = self.pool.get_connection().await?;

        // Verify inode exists
        let mut stmt = conn
            .prepare_cached("SELECT ino FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if rows.next().await?.is_none() {
            return Err(FsError::NotFound.into());
        }

        Ok(Arc::new(AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
            inline_threshold: self.inline_threshold,
            attr_cache: self.attr_cache.clone(),
            pending_view: self.pending_view.clone(),
            write_drain: self.write_drain.clone(),
            overlay_reads: self.overlay_reads,
            _open_guard: Some(self.lifecycle.guard(ino)),
        }))
    }

    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE: see `chmod` — multi-statement metadata mutations
        // must not run as autocommit statements that race the write batcher's
        // drain transactions (turso reports such write/write races as
        // "database snapshot is stale" instead of waiting on the write lock).
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<Stats> = async {
            // Check if already exists
            if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
                return Err(FsError::AlreadyExists.into());
            }

            // Create inode
            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            let mut stmt = conn
                .prepare_cached(
                    "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime, atime_nsec, mtime_nsec, ctime_nsec)
                    VALUES (?, ?, ?, 0, ?, ?, ?, ?, ?, ?) RETURNING ino",
                )
                .await?;
            let dir_mode = super::S_IFDIR | (mode & 0o7777);
            let row = stmt
                .query_row((
                    dir_mode as i64,
                    uid,
                    gid,
                    now_secs,
                    now_secs,
                    now_secs,
                    now_nsec,
                    now_nsec,
                    now_nsec,
                ))
                .await?;

            let ino = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| Error::Internal("failed to get inode".to_string()))?;

            // Create directory entry
            let mut stmt = conn
                .prepare_cached("INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)")
                .await?;
            stmt.execute((name, parent_ino, ino)).await?;

            // Set nlink to 2 for new directory (self "." + parent's dentry)
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET nlink = 2 WHERE ino = ?")
                .await?;
            stmt.execute((ino,)).await?;

            // Increment parent nlink (new directory's ".." link) and update timestamps
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE fs_inode SET nlink = nlink + 1, ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?",
                )
                .await?;
            stmt.execute((now_secs, now_secs, now_nsec, now_nsec, parent_ino))
                .await?;

            Ok(Stats {
                ino,
                mode: dir_mode,
                nlink: 2,
                uid,
                gid,
                size: 0,
                atime: now_secs,
                mtime: now_secs,
                ctime: now_secs,
                atime_nsec: now_nsec as u32,
                mtime_nsec: now_nsec as u32,
                ctime_nsec: now_nsec as u32,
                rdev: 0,
            })
        }
        .await;

        match result {
            Ok(stats) => {
                txn.commit().await?;
                // Populate dentry cache only after the transaction is durable.
                self.cache_dentry(parent_ino, name, stats.ino);
                self.invalidate_parent_attr(parent_ino);
                self.cache_attr(stats.clone());
                Ok(stats)
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    async fn create_file(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<(Stats, BoxedFile)> {
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;

        // No existence pre-check: fs_dentry's UNIQUE(parent_ino, name) makes
        // the dentry INSERT below the authoritative collision detector (its
        // Constraint error maps to AlreadyExists and the transaction drop
        // rolls back the inode row). Saves one SELECT on the synchronous
        // create path that every git-clone file pays.

        // Prepare statements before starting the transaction
        let mut inode_stmt = conn
            .prepare_cached(
                "INSERT INTO fs_inode (mode, nlink, uid, gid, size, atime, mtime, ctime, atime_nsec, mtime_nsec, ctime_nsec, data_inline, storage_kind)
                 VALUES (?, 1, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING ino",
            )
            .await?;
        let mut dentry_stmt = conn
            .prepare_cached("INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)")
            .await?;

        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;

        let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let now_secs = dur.as_secs() as i64;
        let now_nsec = dur.subsec_nanos() as i64;
        let file_mode = S_IFREG | (mode & 0o7777);

        let row = inode_stmt
            .query_row((
                file_mode as i64,
                uid,
                gid,
                now_secs,
                now_secs,
                now_secs,
                now_nsec,
                now_nsec,
                now_nsec,
                Value::Blob(Vec::new()),
                STORAGE_INLINE,
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

        // Parent mtime/ctime: stash into the batcher overlay (committed by the
        // next group drain, served immediately via merge_pending_view) instead
        // of paying an UPDATE on the synchronous create path. Falls back to
        // the in-transaction UPDATE when the overlay cannot serve reads.
        let stash_parent_times = self.overlay_reads && self.write_drain.is_some();
        if !stash_parent_times {
            conn.execute(
                "UPDATE fs_inode SET ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?",
                (now_secs, now_secs, now_nsec, now_nsec, parent_ino),
            )
            .await?;
        }

        txn.commit().await?;

        if stash_parent_times {
            if let Some(drain) = &self.write_drain {
                drain.stash_times(
                    parent_ino,
                    PendingTimeChange {
                        atime: None,
                        mtime: Some((now_secs, now_nsec)),
                        ctime: Some((now_secs, now_nsec)),
                    },
                );
            }
        }

        self.cache_dentry(parent_ino, name, ino);
        self.invalidate_parent_attr(parent_ino);

        let stats = Stats {
            ino,
            mode: file_mode,
            nlink: 1,
            uid,
            gid,
            size: 0,
            atime: now_secs,
            mtime: now_secs,
            ctime: now_secs,
            atime_nsec: now_nsec as u32,
            mtime_nsec: now_nsec as u32,
            ctime_nsec: now_nsec as u32,
            rdev: 0,
        };
        self.cache_attr(stats.clone());

        let file: BoxedFile = Arc::new(AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
            inline_threshold: self.inline_threshold,
            attr_cache: self.attr_cache.clone(),
            pending_view: self.pending_view.clone(),
            write_drain: self.write_drain.clone(),
            overlay_reads: self.overlay_reads,
            _open_guard: Some(self.lifecycle.guard(ino)),
        });

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
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE: see `mkdir` — never race the batcher's drain
        // transactions with autocommit metadata writes.
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<Stats> = async {
            // Check if already exists
            if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
                return Err(FsError::AlreadyExists.into());
            }

            // Create inode with mode and rdev
            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            let mut stmt = conn
                .prepare_cached(
                    "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
                    VALUES (?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?) RETURNING ino",
                )
                .await?;
            let row = stmt
                .query_row((
                    mode as i64,
                    uid,
                    gid,
                    now_secs,
                    now_secs,
                    now_secs,
                    rdev as i64,
                    now_nsec,
                    now_nsec,
                    now_nsec,
                ))
                .await?;

            let ino = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| Error::Internal("failed to get inode".to_string()))?;

            // Create directory entry
            let mut stmt = conn
                .prepare_cached("INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)")
                .await?;
            stmt.execute((name, parent_ino, ino)).await?;

            // Increment link count
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?")
                .await?;
            stmt.execute((ino,)).await?;

            // Update parent directory ctime and mtime
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?")
                .await?;
            stmt.execute((now_secs, now_secs, now_nsec, now_nsec, parent_ino))
                .await?;

            Ok(Stats {
                ino,
                mode,
                nlink: 1,
                uid,
                gid,
                size: 0,
                atime: now_secs,
                mtime: now_secs,
                ctime: now_secs,
                atime_nsec: now_nsec as u32,
                mtime_nsec: now_nsec as u32,
                ctime_nsec: now_nsec as u32,
                rdev,
            })
        }
        .await;

        match result {
            Ok(stats) => {
                txn.commit().await?;
                // Populate dentry cache only after the transaction is durable.
                self.cache_dentry(parent_ino, name, stats.ino);
                self.invalidate_parent_attr(parent_ino);
                self.cache_attr(stats.clone());
                Ok(stats)
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    async fn symlink(
        &self,
        parent_ino: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE: see `mkdir` — never race the batcher's drain
        // transactions with autocommit metadata writes.
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<Stats> = async {
            // Check if entry already exists
            if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
                return Err(FsError::AlreadyExists.into());
            }

            // Create inode for symlink
            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            let mode = S_IFLNK | 0o777; // Symlinks typically have 777 permissions
            let size = target.len() as i64;

            let mut stmt = conn
                .prepare_cached(
                    "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime, atime_nsec, mtime_nsec, ctime_nsec)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING ino",
                )
                .await?;
            let row = stmt
                .query_row((
                    mode, uid, gid, size, now_secs, now_secs, now_secs, now_nsec, now_nsec,
                    now_nsec,
                ))
                .await?;

            let ino = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| Error::Internal("failed to get inode".to_string()))?;

            // Store symlink target
            conn.execute(
                "INSERT INTO fs_symlink (ino, target) VALUES (?, ?)",
                (ino, target),
            )
            .await?;

            // Create directory entry
            conn.execute(
                "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
                (name, parent_ino, ino),
            )
            .await?;

            // Increment link count
            conn.execute(
                "UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?",
                (ino,),
            )
            .await?;

            // Update parent directory ctime and mtime
            conn.execute(
                "UPDATE fs_inode SET ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?",
                (now_secs, now_secs, now_nsec, now_nsec, parent_ino),
            )
            .await?;

            Ok(Stats {
                ino,
                mode,
                nlink: 1,
                uid,
                gid,
                size,
                atime: now_secs,
                mtime: now_secs,
                ctime: now_secs,
                atime_nsec: now_nsec as u32,
                mtime_nsec: now_nsec as u32,
                ctime_nsec: now_nsec as u32,
                rdev: 0,
            })
        }
        .await;

        match result {
            Ok(stats) => {
                txn.commit().await?;
                // Populate dentry cache only after the transaction is durable.
                self.cache_dentry(parent_ino, name, stats.ino);
                self.invalidate_parent_attr(parent_ino);
                self.cache_attr(stats.clone());
                Ok(stats)
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    async fn unlink(&self, parent_ino: i64, name: &str) -> Result<()> {
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        self.process_deferred_reaps().await?;
        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE: this is the path that intermittently failed with
        // "database snapshot is stale" -> EIO when its autocommit statements
        // raced the write batcher's drain transactions (git unlinking
        // `.git/config.lock` during a clone). The transaction also makes the
        // dentry/nlink/inode removal atomic.
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<i64> = async {
            // Look up the child inode
            let ino = self
                .lookup_child(&conn, parent_ino, name)
                .await?
                .ok_or(FsError::NotFound)?;

            // Check if it's a directory (use rmdir for directories)
            if let Some(mode) = store::mode(&conn, ino).await? {
                if (mode & S_IFMT) == super::S_IFDIR {
                    return Err(FsError::IsADirectory.into());
                }
            } else {
                return Err(FsError::NotFound.into());
            }

            // Delete the directory entry
            let mut stmt = conn
                .prepare_cached("DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?")
                .await?;
            stmt.execute((parent_ino, name)).await?;

            // Update parent directory mtime and ctime
            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?")
                .await?;
            stmt.execute((now_secs, now_secs, now_nsec, now_nsec, parent_ino))
                .await?;

            // Decrement link count and update ctime
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE fs_inode SET nlink = nlink - 1, ctime = ?, ctime_nsec = ? WHERE ino = ?",
                )
                .await?;
            stmt.execute((now_secs, now_nsec, ino)).await?;

            // Check if this was the last link to the inode. POSIX: while
            // open handles exist the nlink=0 rows stay alive; the last
            // handle drop queues the orphan for process_deferred_reaps.
            let link_count = self.get_link_count(&conn, ino).await?;
            let removed = link_count == 0 && !self.lifecycle.defer_reap_if_open(ino);
            if removed {
                self.discard_pending_before_reap(ino);
                self.reap_inode_with_conn(&conn, ino).await?;
            }

            Ok(ino)
        }
        .await;

        match result {
            Ok(ino) => {
                txn.commit().await?;
                self.invalidate_dentry(parent_ino, name);
                self.invalidate_parent_attr(parent_ino);
                self.invalidate_attr(ino);
                self.cache_negative_dentry(parent_ino, name);
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    async fn rmdir(&self, parent_ino: i64, name: &str) -> Result<()> {
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        self.process_deferred_reaps().await?;
        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE: see `unlink` — never race the batcher's drain
        // transactions with autocommit metadata writes.
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<i64> = async {
            // Look up the child inode
            let ino = self
                .lookup_child(&conn, parent_ino, name)
                .await?
                .ok_or(FsError::NotFound)?;

            if ino == ROOT_INO {
                return Err(FsError::RootOperation.into());
            }

            // Check if it's a directory
            if let Some(mode) = store::mode(&conn, ino).await? {
                if (mode & S_IFMT) != super::S_IFDIR {
                    return Err(FsError::NotADirectory.into());
                }
            } else {
                return Err(FsError::NotFound.into());
            }

            // Check if directory is empty
            let mut stmt = conn
                .prepare_cached("SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?")
                .await?;
            let mut rows = stmt.query((ino,)).await?;

            if let Some(row) = rows.next().await? {
                let count = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);
                if count > 0 {
                    return Err(FsError::NotEmpty.into());
                }
            }

            // Delete the directory entry
            let mut stmt = conn
                .prepare_cached("DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?")
                .await?;
            stmt.execute((parent_ino, name)).await?;

            // Decrement link count on removed directory
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?")
                .await?;
            stmt.execute((ino,)).await?;

            // Decrement parent nlink (removed directory's ".." link) and update timestamps
            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE fs_inode SET nlink = nlink - 1, ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?",
                )
                .await?;
            stmt.execute((now_secs, now_secs, now_nsec, now_nsec, parent_ino))
                .await?;

            // Delete inode if no more links
            let link_count = self.get_link_count(&conn, ino).await?;
            if link_count == 0 {
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_inode WHERE ino = ?")
                    .await?;
                stmt.execute((ino,)).await?;
            }

            Ok(ino)
        }
        .await;

        match result {
            Ok(ino) => {
                txn.commit().await?;
                self.invalidate_dentry(parent_ino, name);
                self.invalidate_parent_attr(parent_ino);
                self.invalidate_attr(ino);
                self.cache_negative_dentry(parent_ino, name);
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    async fn link(&self, ino: i64, newparent_ino: i64, newname: &str) -> Result<Stats> {
        if newname.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;
        // BEGIN IMMEDIATE: see `unlink` — never race the batcher's drain
        // transactions with autocommit metadata writes.
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<Stats> = async {
            // Check if source inode exists and is not a directory
            if let Some(mode) = store::mode(&conn, ino).await? {
                if (mode & S_IFMT) == super::S_IFDIR {
                    return Err(FsError::IsADirectory.into());
                }
            } else {
                return Err(FsError::NotFound.into());
            }

            // Check if destination already exists
            if self
                .lookup_child(&conn, newparent_ino, newname)
                .await?
                .is_some()
            {
                return Err(FsError::AlreadyExists.into());
            }

            // Create directory entry pointing to the same inode
            conn.execute(
                "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
                (newname, newparent_ino, ino),
            )
            .await?;

            // Increment link count and update ctime
            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            conn.execute(
                "UPDATE fs_inode SET nlink = nlink + 1, ctime = ?, ctime_nsec = ? WHERE ino = ?",
                (now_secs, now_nsec, ino),
            )
            .await?;

            // Update parent directory ctime and mtime
            conn.execute(
                "UPDATE fs_inode SET ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?",
                (now_secs, now_secs, now_nsec, now_nsec, newparent_ino),
            )
            .await?;

            // Return updated stats (drop the cached pre-link attr so the read
            // below reflects the nlink/ctime updates made in this transaction).
            self.invalidate_attr(ino);
            self.getattr_with_conn(&conn, ino)
                .await?
                .ok_or(FsError::NotFound.into())
        }
        .await;

        match result {
            Ok(stats) => {
                txn.commit().await?;
                // Populate dentry cache only after the transaction is durable.
                self.cache_dentry(newparent_ino, newname, ino);
                self.invalidate_parent_attr(newparent_ino);
                self.invalidate_attr(ino);
                Ok(stats)
            }
            Err(error) => {
                let _ = txn.rollback().await;
                self.invalidate_attr(ino);
                Err(error)
            }
        }
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
        if newname.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        self.process_deferred_reaps().await?;
        let conn = self.pool.get_connection().await?;

        // Get source inode
        let src_ino = self
            .lookup_child(&conn, oldparent_ino, oldname)
            .await?
            .ok_or(FsError::NotFound)?;

        if src_ino == ROOT_INO {
            return Err(FsError::RootOperation.into());
        }

        // Get source stats to check if it's a directory
        let src_stats = self
            .getattr_with_conn(&conn, src_ino)
            .await?
            .ok_or(FsError::NotFound)?;

        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;

        let result: Result<Option<i64>> = async {
            let mut replaced_dst_ino = None;

            if src_stats.is_directory() {
                let mut ancestor_ino = newparent_ino;
                let mut visited = HashSet::new();
                while ancestor_ino != ROOT_INO {
                    if ancestor_ino == src_ino {
                        return Err(FsError::InvalidRename.into());
                    }
                    if !visited.insert(ancestor_ino) {
                        return Err(FsError::InvalidPath.into());
                    }

                    let mut stmt = conn
                        .prepare_cached("SELECT parent_ino FROM fs_dentry WHERE ino = ?")
                        .await?;
                    let mut rows = stmt.query((ancestor_ino,)).await?;
                    let parent_ino = rows
                        .next()
                        .await?
                        .ok_or(FsError::NotFound)?
                        .get_value(0)
                        .ok()
                        .and_then(|value| value.as_integer().copied())
                        .ok_or(FsError::InvalidPath)?;
                    if rows.next().await?.is_some() {
                        return Err(FsError::InvalidPath.into());
                    }
                    ancestor_ino = parent_ino;
                }
            }

            // Check if destination exists
            if let Some(dst_ino) = self.lookup_child(&conn, newparent_ino, newname).await? {
                replaced_dst_ino = Some(dst_ino);
                let dst_stats = self.getattr_with_conn(&conn, dst_ino).await?.ok_or(FsError::NotFound)?;

                // Can't replace directory with non-directory
                if dst_stats.is_directory() && !src_stats.is_directory() {
                    return Err(FsError::IsADirectory.into());
                }

                // Can't replace non-directory with directory
                if !dst_stats.is_directory() && src_stats.is_directory() {
                    return Err(FsError::NotADirectory.into());
                }

                // If destination is directory, it must be empty
                if dst_stats.is_directory() {
                    let mut stmt = conn
                        .prepare_cached("SELECT COUNT(*) FROM fs_dentry WHERE parent_ino = ?")
                        .await?;
                    let mut rows = stmt.query((dst_ino,)).await?;

                    if let Some(row) = rows.next().await? {
                        let count = row
                            .get_value(0)
                            .ok()
                            .and_then(|v| v.as_integer().copied())
                            .unwrap_or(0);
                        if count > 0 {
                            return Err(FsError::NotEmpty.into());
                        }
                    }
                }

                // Remove destination entry
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?")
                    .await?;
                stmt.execute((newparent_ino, newname)).await?;

                // Decrement link count and update ctime on destination inode
                let dur_dec = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let now_dec = dur_dec.as_secs() as i64;
                let now_dec_nsec = dur_dec.subsec_nanos() as i64;
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink - 1, ctime = ?, ctime_nsec = ? WHERE ino = ?")
                    .await?;
                stmt.execute((now_dec, now_dec_nsec, dst_ino)).await?;

                // Clean up destination inode if no more links (deferred while
                // open handles exist — see lifecycle).
                let link_count = self.get_link_count(&conn, dst_ino).await?;
                if link_count == 0
                    && !self.lifecycle.defer_reap_if_open(dst_ino)
                {
                    self.discard_pending_before_reap(dst_ino);
                    self.reap_inode_with_conn(&conn, dst_ino).await?;
                }
            }

            // Update the dentry: change parent and/or name
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE fs_dentry SET parent_ino = ?, name = ? WHERE parent_ino = ? AND name = ?",
                )
                .await?;
            stmt.execute((newparent_ino, newname, oldparent_ino, oldname))
                .await?;

            // If renaming a directory across parents, adjust parent nlink counts
            // (the ".." link moves from old parent to new parent)
            if src_stats.is_directory() && oldparent_ino != newparent_ino {
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?")
                    .await?;
                stmt.execute((oldparent_ino,)).await?;

                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?")
                    .await?;
                stmt.execute((newparent_ino,)).await?;
            }

            // Update ctime of the inode
            let dur = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;

            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET ctime = ?, ctime_nsec = ? WHERE ino = ?")
                .await?;
            stmt.execute((now_secs, now_nsec, src_ino)).await?;

            // Update source parent directory timestamps
            let mut stmt = conn
                .prepare_cached("UPDATE fs_inode SET mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?")
                .await?;
            stmt.execute((now_secs, now_secs, now_nsec, now_nsec, oldparent_ino)).await?;

            // Update destination parent directory timestamps
            if newparent_ino != oldparent_ino {
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?")
                    .await?;
                stmt.execute((now_secs, now_secs, now_nsec, now_nsec, newparent_ino)).await?;
            }

            Ok(replaced_dst_ino)
        }
        .await;

        match result {
            Ok(replaced_dst_ino) => {
                txn.commit().await?;

                // Invalidate cache for source and destination
                self.invalidate_dentry(oldparent_ino, oldname);
                self.invalidate_dentry(newparent_ino, newname);
                self.invalidate_attr(src_ino);
                self.invalidate_parent_attr(oldparent_ino);
                self.invalidate_parent_attr(newparent_ino);
                if let Some(dst_ino) = replaced_dst_ino {
                    self.invalidate_attr(dst_ino);
                }

                // Add exact post-rename namespace state to the caches.
                if oldparent_ino != newparent_ino || oldname != newname {
                    self.cache_negative_dentry(oldparent_ino, oldname);
                }
                self.cache_dentry(newparent_ino, newname, src_ino);

                Ok(replaced_dst_ino)
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        AgentFS::statfs(self).await
    }

    async fn drain_inode_writes(&self, ino: i64) -> Result<()> {
        AgentFS::drain_inode_writes(self, ino).await
    }

    async fn drain_all(&self) -> Result<()> {
        AgentFS::drain_all(self).await
    }

    async fn finalize(&self) -> Result<()> {
        AgentFS::finalize(self).await
    }

    fn register_reap_hook(&self, hook: Arc<dyn ReapHook>) -> bool {
        AgentFS::register_reap_hook(self, hook)
    }

    // `forget` deliberately uses the default no-op trait impl: a FORGET only
    // drops the kernel's reference to the inode. Pending batched writes stay
    // readable through the Tier-4 overlay and are committed by the batcher
    // timer/bytes triggers, fsync, or finalize — committing them here issued
    // one serial SQLite transaction per written file during clone workloads
    // (the kernel FORGETs each file shortly after our post-write entry
    // invalidation).
}
