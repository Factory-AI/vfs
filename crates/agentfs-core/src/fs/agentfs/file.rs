//! Open-file handle implementation for AgentFS.
//!
//! `AgentFSFile` owns per-handle read, write, truncate, fsync, and fstat
//! behavior. It overlays pending batched writes onto committed SQLite state
//! without forcing drains on read paths, preserving the noopen/writeback hot
//! path.

use async_trait::async_trait;
use turso::transaction::{Transaction, TransactionBehavior};

use crate::fs::{File, FsError, Stats, WriteRange};

use super::batcher::EnqueueOutcome;
use super::store::WriteRangeRef;
use super::*;

/// An open file handle for AgentFS.
///
/// This struct holds the inode number resolved at open time, allowing
/// efficient read/write/fsync operations without path lookups.
pub struct AgentFSFile {
    pub(super) pool: ConnectionPool,
    pub(super) ino: i64,
    pub(super) chunk_size: usize,
    pub(super) inline_threshold: usize,
    pub(super) attr_cache: Arc<AttrCache>,
    pub(super) pending_view: Option<BatcherPendingView>,
    pub(super) write_drain: Option<BatcherDrain>,
    /// Same semantics as the field on `AgentFS`; cloned at open time so the
    /// hot read/write path doesn't have to chase an extra indirection.
    pub(super) overlay_reads: bool,
    /// Present for user-visible handles so unlink defers inode reaping while
    /// they live. This remains optional until lifecycle extraction flattens
    /// the handle construction API.
    pub(super) _open_guard: Option<OpenInodeGuard>,
}

#[async_trait]
impl File for AgentFSFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        // Tier Four: NO `drain_writes()` prelude. Read SQLite-resident bytes
        // (committed state) and overlay pending writes from the in-memory
        // batcher snapshot. Together they form a read-after-write consistent
        // view without forcing a SQLite commit on the read path.
        //
        // Ordering matters: peek the batcher state BEFORE acquiring a pool
        // connection, and release the connection BEFORE the splice loop. Long
        // pread workloads (parallel git-grep) saturate the 8-slot pool, and
        // holding a connection across `state.lock().await` starves the timer
        // drain task that also needs a connection to commit.
        if size == 0 {
            return Ok(Vec::new());
        }
        // Escape hatch: when overlay reads are disabled, behave like Tier 3
        // — drain the inode's pending writes before reading SQLite. Same
        // wire result, slower but battle-tested.
        if !self.overlay_reads {
            self.drain_writes().await?;
        }
        let pending_max_end = match &self.pending_view {
            Some(view) if self.overlay_reads && view.has_pending(self.ino) => {
                view.pending_max_end(self.ino)
            }
            _ => None,
        };

        let conn = self.pool.get_connection().await?;
        let metadata = store::file_storage(&conn, self.ino).await?;
        let effective_size = match pending_max_end {
            Some(end) => metadata.size.max(end),
            None => metadata.size,
        };

        if offset >= effective_size {
            return Ok(Vec::new());
        }
        let read_size = size.min(effective_size - offset);

        let base_window = if offset < metadata.size {
            (metadata.size - offset).min(read_size)
        } else {
            0
        };
        let mut result = if base_window > 0 {
            let mut buf = store::read_from_storage(
                &conn,
                self.ino,
                self.geometry(),
                &metadata,
                offset,
                base_window,
            )
            .await?;
            buf.resize(read_size as usize, 0);
            buf
        } else {
            vec![0u8; read_size as usize]
        };
        drop(conn);

        if let Some(view) = &self.pending_view {
            if pending_max_end.is_some() {
                let _ = view.overlay_read(self.ino, offset, &mut result);
            }
        }

        Ok(result)
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        // Tier Four: with the batcher wired AND overlay reads enabled,
        // route through enqueue so the overlay holds the write and readers
        // see it via `pread`'s peek_pending merge. Drain only on
        // fsync/destroy/timer. When `AGENTFS_OVERLAY_READS=0` the
        // overlay-reads escape hatch is engaged: skip the batcher and commit
        // directly so the legacy Tier 3 read path (which drains before
        // reading) sees the write.
        if let Some(drain) = &self.write_drain {
            if self.overlay_reads {
                let outcome = drain.enqueue(
                    self.ino,
                    vec![WriteRange {
                        offset,
                        data: data.to_vec(),
                    }],
                )?;
                return Self::finish_enqueue(drain, self.ino, outcome).await;
            }
        }
        // Fallback (no batcher): direct commit. drain_writes is a no-op
        // when there's no batcher, but keeping the call here makes the
        // contract explicit.
        self.drain_writes().await?;
        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let ranges = [WriteRangeRef { offset, data }];
        let result =
            store::write_ranges(&conn, self.ino, self.geometry(), &ranges, false, None).await;
        match result {
            Ok(()) => {
                txn.commit().await?;
                self.attr_cache.remove(self.ino);
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn pwrite_ranges(&self, ranges: Vec<WriteRange>) -> Result<()> {
        if ranges.iter().all(|range| range.data.is_empty()) {
            return Ok(());
        }
        // Tier Four: route through the batcher when overlay reads are
        // enabled; otherwise commit immediately (escape hatch — see pwrite).
        if let Some(drain) = &self.write_drain {
            if self.overlay_reads {
                let outcome = drain.enqueue(self.ino, ranges)?;
                return Self::finish_enqueue(drain, self.ino, outcome).await;
            }
        }
        self.drain_writes().await?;

        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let range_refs: Vec<_> = ranges
            .iter()
            .map(|range| WriteRangeRef {
                offset: range.offset,
                data: range.data.as_slice(),
            })
            .collect();
        let result =
            store::write_ranges(&conn, self.ino, self.geometry(), &range_refs, false, None).await;
        match result {
            Ok(()) => {
                txn.commit().await?;
                self.attr_cache.remove(self.ino);
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn pwrite_ranges_batched(&self, ranges: Vec<WriteRange>) -> Result<()> {
        if ranges.iter().all(|range| range.data.is_empty()) {
            return Ok(());
        }

        if let Some(drain) = &self.write_drain {
            let outcome = drain.enqueue(self.ino, ranges)?;
            Self::finish_enqueue(drain, self.ino, outcome).await
        } else {
            self.pwrite_ranges(ranges).await
        }
    }

    async fn truncate(&self, new_size: u64) -> Result<()> {
        // Tier Four: shrink the in-memory overlay BEFORE touching SQLite, so
        // a concurrent reader doesn't observe pending bytes past the new EOF
        // between the SQLite truncate and the batcher catching up.
        if let Some(drain) = &self.write_drain {
            drain.truncate_pending(self.ino, new_size);
        }
        // Drain remaining pending so the SQLite truncate sees a consistent
        // size. With truncate_pending called above, the only pending left is
        // for offsets < new_size, which will be applied by the timer / next
        // drain trigger. We still drain here so the SQLite size after this
        // call exactly matches `new_size`.
        self.drain_writes().await?;
        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result = store::truncate(&conn, self.ino, self.geometry(), new_size).await;
        match result {
            Ok(()) => {
                txn.commit().await?;
                self.attr_cache.remove(self.ino);
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn fsync(&self) -> Result<()> {
        // Tier Four: fsync remains the explicit durability barrier — drain the
        // batcher so the WAL checkpoint that follows captures every pending
        // write.
        self.drain_writes().await?;
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

    async fn fstat(&self) -> Result<Stats> {
        self.drain_writes().await?;
        if let Some(stats) = self.attr_cache.get(self.ino) {
            return Ok(stats);
        }

        let conn = self.pool.get_connection().await?;
        let generation = self
            .pending_view
            .as_ref()
            .map(|view| view.pending_generation(self.ino));
        let mut stmt = conn
            .prepare_cached("SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((self.ino,)).await?;

        if let Some(row) = rows.next().await? {
            let stats = store::stats_from_row(&row)?;
            if let (Some(view), Some(generation)) = (&self.pending_view, generation) {
                if view.pending_generation(stats.ino) == generation {
                    self.attr_cache.insert(stats.clone());
                }
            } else {
                self.attr_cache.insert(stats.clone());
            }
            Ok(stats)
        } else {
            Err(FsError::NotFound.into())
        }
    }

    async fn drain_writes(&self) -> Result<()> {
        if let Some(drain) = &self.write_drain {
            drain.drain_inode(self.ino).await?;
        }
        Ok(())
    }
}

impl AgentFSFile {
    async fn finish_enqueue(drain: &BatcherDrain, ino: i64, outcome: EnqueueOutcome) -> Result<()> {
        if outcome.drain_all {
            drain.drain_all_bytes().await
        } else if outcome.drain_inode {
            drain.drain_inode_bytes(ino).await
        } else {
            Ok(())
        }
    }

    fn geometry(&self) -> Geometry {
        Geometry {
            chunk_size: self.chunk_size,
            inline_threshold: self.inline_threshold,
        }
    }
}
