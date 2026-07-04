use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

use crate::config::{BatcherConfig, Geometry};
use crate::error::{Error, Result};
use crate::fs::{FsError, Stats, WriteRange};
use crate::pool::ConnectionPool;

use super::current_timestamp;
use super::store::{self, normalize_write_ranges, NormalizedWriteRange, WriteRangeRef};

pub(super) type Invalidate = Arc<dyn Fn(i64) + Send + Sync + 'static>;

#[cfg(test)]
pub(super) const MAX_RETIRED_GENERATIONS: usize = 64;
#[cfg(not(test))]
const MAX_RETIRED_GENERATIONS: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingGeneration(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct OverlayHit {
    pub(super) applied: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct EnqueueOutcome {
    pub(super) drain_inode: bool,
    pub(super) drain_all: bool,
}

pub(super) type PendingTimes = PendingTimeChange;

pub(super) trait PendingView: Send + Sync {
    fn has_pending(&self, ino: i64) -> bool;
    fn overlay_read(&self, ino: i64, off: u64, buf: &mut [u8]) -> OverlayHit;
    fn pending_max_end(&self, ino: i64) -> Option<u64>;
    fn pending_times(&self, ino: i64) -> Option<PendingTimes>;
    fn pending_generation(&self, ino: i64) -> PendingGeneration;
    fn merge_into_stats(&self, ino: i64, stats: &mut Stats);
}

#[async_trait]
pub(super) trait Drain: Send + Sync {
    async fn drain_inode(&self, ino: i64) -> Result<()>;
    async fn drain_all(&self) -> Result<()>;
    async fn drain_inode_bytes(&self, ino: i64) -> Result<()>;
    async fn drain_all_bytes(&self) -> Result<()>;
    fn enqueue(&self, ino: i64, ranges: Vec<WriteRange>) -> Result<EnqueueOutcome>;
    fn discard_pending(&self, ino: i64);
    fn mark_times_explicit(&self, ino: i64);
    fn truncate_pending(&self, ino: i64, size: u64);
    fn stash_times(&self, ino: i64, times: PendingTimes);
}

#[derive(Clone)]
pub(super) struct BatcherPendingView {
    batcher: Arc<AgentFSWriteBatcher>,
}

#[derive(Clone)]
pub(super) struct BatcherDrain {
    batcher: Arc<AgentFSWriteBatcher>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum AgentFSWriteBatchDrainReason {
    Timer,
    Bytes,
    Explicit,
}

/// Explicitly-set timestamps stashed by `utimens` while the inode still has
/// pending batched writes. Instead of paying a dedicated SQLite transaction
/// per SETATTR (the FUSE writeback cache sends one per written file during a
/// clone), the values ride along in the pending entry and the batcher applies
/// them inside the SAME drain transaction, right after the data UPDATE — so
/// the explicitly-set times win over the commit-time stamp without an extra
/// per-file transaction. `getattr`/`lookup` overlay these values onto the
/// SQLite row (`merge_pending_view`) so the change is visible immediately.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PendingTimeChange {
    /// (secs, nsec) for atime, when explicitly set.
    pub(super) atime: Option<(i64, i64)>,
    /// (secs, nsec) for mtime, when explicitly set.
    pub(super) mtime: Option<(i64, i64)>,
    /// (secs, nsec) for ctime (always bumped by the setattr that stashed).
    pub(super) ctime: Option<(i64, i64)>,
}

impl PendingTimeChange {
    pub(super) fn is_empty(&self) -> bool {
        self.atime.is_none() && self.mtime.is_none() && self.ctime.is_none()
    }

    /// Per-field "newer wins" merge: fields set by `newer` override ours.
    fn apply(&mut self, newer: &PendingTimeChange) {
        if newer.atime.is_some() {
            self.atime = newer.atime;
        }
        if newer.mtime.is_some() {
            self.mtime = newer.mtime;
        }
        if newer.ctime.is_some() {
            self.ctime = newer.ctime;
        }
    }

    /// Drop the fields a buffered data write would re-stamp (mtime/ctime). A
    /// write AFTER the explicit setattr means the file changed again, so the
    /// eventual commit must stamp fresh modification times; an explicitly-set
    /// atime is unaffected by writes and survives.
    fn clear_write_stamped(&mut self) {
        self.mtime = None;
        self.ctime = None;
    }

    /// Overlay the stashed values onto a `Stats` row read from SQLite, so
    /// `getattr`/`lookup` surface explicit `utimens` results immediately even
    /// though the row UPDATE is deferred to the next batched drain.
    pub(super) fn merge_into(&self, stats: &mut Stats) {
        if let Some((secs, nsec)) = self.atime {
            stats.atime = secs;
            stats.atime_nsec = nsec as u32;
        }
        if let Some((secs, nsec)) = self.mtime {
            stats.mtime = secs;
            stats.mtime_nsec = nsec as u32;
        }
        if let Some((secs, nsec)) = self.ctime {
            stats.ctime = secs;
            stats.ctime_nsec = nsec as u32;
        }
    }
}

/// Build the time-column SET fragments for a batched data-commit UPDATE so
/// stashed explicit times ride the same statement as size/storage/data
/// (one UPDATE per inode instead of two; ~4,700 extra UPDATEs per clone
/// otherwise). Precedence per column: a stashed explicit value wins; without
/// one, mtime/ctime are stamped with the commit time unless `preserve_times`
/// (an explicit setattr landed after the writes and its values must not be
/// clobbered); atime is only ever written explicitly.
pub(super) fn write_commit_time_sets(
    preserve_times: bool,
    explicit_times: Option<&PendingTimeChange>,
) -> Result<(Vec<&'static str>, Vec<Value>)> {
    let explicit_atime = explicit_times.and_then(|t| t.atime);
    let explicit_mtime = explicit_times.and_then(|t| t.mtime);
    let explicit_ctime = explicit_times.and_then(|t| t.ctime);
    let stamp = if !preserve_times && (explicit_mtime.is_none() || explicit_ctime.is_none()) {
        Some(current_timestamp()?)
    } else {
        None
    };
    let mut sets = Vec::new();
    let mut values = Vec::new();
    if let Some((secs, nsec)) = explicit_atime {
        sets.push("atime = ?");
        values.push(Value::Integer(secs));
        sets.push("atime_nsec = ?");
        values.push(Value::Integer(nsec));
    }
    if let Some((secs, nsec)) = explicit_mtime.or(stamp) {
        sets.push("mtime = ?");
        values.push(Value::Integer(secs));
        sets.push("mtime_nsec = ?");
        values.push(Value::Integer(nsec));
    }
    if let Some((secs, nsec)) = explicit_ctime.or(stamp) {
        sets.push("ctime = ?");
        values.push(Value::Integer(secs));
        sets.push("ctime_nsec = ?");
        values.push(Value::Integer(nsec));
    }
    Ok((sets, values))
}

/// Apply a stashed `PendingTimeChange` to fs_inode using the drain
/// transaction's connection. Used for time-only pending entries (no data
/// ranges to commit, so there is no data UPDATE to fold the times into);
/// runs inside the drain's `BEGIN IMMEDIATE`. A deleted inode simply matches
/// no row (the unlink already won).
async fn apply_pending_times_with_conn(
    conn: &Connection,
    ino: i64,
    times: &PendingTimeChange,
) -> Result<()> {
    let mut updates = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    if let Some((secs, nsec)) = times.atime {
        updates.push("atime = ?");
        values.push(Value::Integer(secs));
        updates.push("atime_nsec = ?");
        values.push(Value::Integer(nsec));
    }
    if let Some((secs, nsec)) = times.mtime {
        updates.push("mtime = ?");
        values.push(Value::Integer(secs));
        updates.push("mtime_nsec = ?");
        values.push(Value::Integer(nsec));
    }
    if let Some((secs, nsec)) = times.ctime {
        updates.push("ctime = ?");
        values.push(Value::Integer(secs));
        updates.push("ctime_nsec = ?");
        values.push(Value::Integer(nsec));
    }
    if updates.is_empty() {
        return Ok(());
    }
    values.push(Value::Integer(ino));
    let sql = format!("UPDATE fs_inode SET {} WHERE ino = ?", updates.join(", "));
    conn.execute(&sql, values).await?;
    Ok(())
}

struct PendingInodeWrites {
    ranges: Vec<WriteRange>,
    pending_bytes: usize,
    generation: u64,
    /// True when an explicit attribute change (chmod / chown / utimens) was
    /// applied to fs_inode AFTER the most recent enqueue for this inode. The
    /// deferred data commit normally stamps mtime/ctime with the commit time;
    /// when this flag is set it must preserve the explicitly-set times instead
    /// (the setattr logically happened after the buffered writes). Reset on
    /// every enqueue: a write after the setattr should bump the times again.
    times_explicit: bool,
    /// Explicit `utimens` values waiting to be committed together with the
    /// pending data (see `PendingTimeChange`). Cleared field-wise: a new
    /// enqueue drops mtime/ctime (write re-stamps them), the drain clears the
    /// values it actually committed.
    pending_times: Option<PendingTimeChange>,
}

impl PendingInodeWrites {
    fn new(generation: u64) -> Self {
        Self {
            ranges: Vec::new(),
            pending_bytes: 0,
            generation,
            times_explicit: false,
            pending_times: None,
        }
    }

    fn bump_generation(&mut self, generation: u64) {
        self.generation = generation;
    }

    /// True when nothing is left to commit for this inode (no data ranges and
    /// no stashed explicit times) — only then may the entry be dropped.
    fn is_drained(&self) -> bool {
        self.ranges.is_empty() && self.pending_times.is_none()
    }

    fn push_ranges(&mut self, ranges: Vec<WriteRange>, byte_count: usize) -> Result<()> {
        self.pending_bytes = self
            .pending_bytes
            .checked_add(byte_count)
            .ok_or_else(|| Error::Internal("batched write byte count overflow".to_string()))?;
        self.times_explicit = false;
        if let Some(times) = &mut self.pending_times {
            times.clear_write_stamped();
            if times.is_empty() {
                self.pending_times = None;
            }
        }
        self.ranges.extend(ranges);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct RetiredGeneration {
    generation: u64,
    retired_at: u64,
}

#[derive(Default)]
struct AgentFSWriteBatcherState {
    pending: HashMap<i64, PendingInodeWrites>,
    /// Monotonic per-inode version for the pending overlay. Cache fills that
    /// read SQLite before a concurrent drain must compare the generation they
    /// observed with the current one before inserting attrs.
    retired_generations: HashMap<i64, RetiredGeneration>,
    /// Fallback generation for inodes whose retired entry was pruned. It is
    /// advanced only by actual pruning, so never-batched inodes keep the
    /// stable zero fallback across unrelated write traffic while pruned
    /// retired entries remain ABA-safe.
    pruned_generation_watermark: u64,
    /// Global monotonic generation counter. This advances for every pending
    /// overlay mutation, but absent never-batched inodes do not report it
    /// directly (see `pruned_generation_watermark`).
    last_generation: u64,
    /// Logical retirement epoch used only to bound `retired_generations`.
    retired_generation_epoch: u64,
    /// Running sum of `pending_bytes` across every inode in `pending`. Kept in
    /// lock-step with the map so the enqueue path can enforce a global memory
    /// cap in O(1) instead of summing the map on every write. Every site that
    /// mutates a `PendingInodeWrites.pending_bytes` or inserts/removes an entry
    /// must keep this consistent (see `debug_assert_total`).
    total_pending_bytes: usize,
    /// True while the single coalescing drain scheduler task is armed (see
    /// `run_drain_scheduler`). Set by the enqueue that arms it, cleared by the
    /// scheduler itself under this same lock once nothing is pending — so an
    /// enqueue either observes it set (the running scheduler will pick the new
    /// write up) or arms a fresh scheduler. Pending work is never stranded.
    drain_scheduled: bool,
}

impl AgentFSWriteBatcherState {
    fn generation(&self, ino: i64) -> PendingGeneration {
        PendingGeneration(
            self.pending
                .get(&ino)
                .map(|entry| entry.generation)
                .or_else(|| {
                    self.retired_generations
                        .get(&ino)
                        .map(|retired| retired.generation)
                })
                .unwrap_or(self.pruned_generation_watermark),
        )
    }

    fn bump_generation(&mut self, ino: i64) {
        let generation = self.next_generation();
        if let Some(entry) = self.pending.get_mut(&ino) {
            entry.bump_generation(generation);
        } else {
            self.retire_generation(ino, generation);
        }
    }

    fn next_generation(&mut self) -> u64 {
        self.last_generation = self
            .last_generation
            .checked_add(1)
            .expect("AgentFS write batcher generation counter exhausted");
        self.last_generation
    }

    fn retire_generation(&mut self, ino: i64, generation: u64) {
        self.retired_generation_epoch = self
            .retired_generation_epoch
            .checked_add(1)
            .expect("AgentFS write batcher retired-generation epoch exhausted");
        self.retired_generations.insert(
            ino,
            RetiredGeneration {
                generation,
                retired_at: self.retired_generation_epoch,
            },
        );
        self.prune_retired_generations();
    }

    fn prune_retired_generations(&mut self) {
        let mut pruned = false;
        while self.retired_generations.len() > MAX_RETIRED_GENERATIONS {
            let Some(oldest_ino) = self
                .retired_generations
                .iter()
                .min_by_key(|(_, retired)| retired.retired_at)
                .map(|(ino, _)| *ino)
            else {
                break;
            };
            self.retired_generations.remove(&oldest_ino);
            pruned = true;
        }
        if pruned {
            self.pruned_generation_watermark = self.last_generation;
        }
    }

    #[cfg(debug_assertions)]
    fn debug_assert_total(&self) {
        let sum: usize = self.pending.values().map(|b| b.pending_bytes).sum();
        debug_assert_eq!(
            sum, self.total_pending_bytes,
            "batcher total_pending_bytes drifted from sum of pending entries"
        );
    }

    #[cfg(not(debug_assertions))]
    #[inline]
    fn debug_assert_total(&self) {}
}

/// In-memory write group-commit queue for FUSE writeback mode.
///
/// The batcher stores only transient `WriteRange` values and drains them into
/// the canonical SQLite tables. It never creates sidecars and normal durability
/// boundaries (`flush`, `fsync`, `release`, `destroy`) explicitly drain it.
pub(super) struct AgentFSWriteBatcher {
    pool: ConnectionPool,
    chunk_size: usize,
    inline_threshold: usize,
    invalidate: Invalidate,
    batch_ms: Duration,
    batch_bytes: usize,
    batch_global_bytes: usize,
    /// Per-transaction inode-count bound for batched drains
    /// (`AGENTFS_BATCH_TXN_INODES`). See `drain_pending_batched`.
    txn_max_inodes: usize,
    /// Per-transaction pending-bytes bound for batched drains
    /// (`AGENTFS_BATCH_TXN_BYTES`). See `drain_pending_batched`.
    txn_max_bytes: usize,
    /// Tier 4 mitigation: parking_lot `RwLock` so `peek_pending` /
    /// `peek_pending_max_end` can acquire read-only access without contending
    /// with writers. The lock is never held across an `.await`, so a sync
    /// lock is safe inside async fns. Holding it across an await would block
    /// the tokio worker — `take_pending_locked` and friends always extract
    /// owned state under the lock and drop the guard before any I/O.
    state: RwLock<AgentFSWriteBatcherState>,
    commit_lock: AsyncMutex<()>,
}

impl AgentFSWriteBatcher {
    pub(super) fn split(batcher: &Arc<Self>) -> (BatcherPendingView, BatcherDrain) {
        (
            BatcherPendingView {
                batcher: Arc::clone(batcher),
            },
            BatcherDrain {
                batcher: Arc::clone(batcher),
            },
        )
    }

    pub(super) fn from_config(
        pool: ConnectionPool,
        chunk_size: usize,
        inline_threshold: usize,
        invalidate: Invalidate,
        config: &BatcherConfig,
    ) -> Self {
        Self {
            pool,
            chunk_size,
            inline_threshold,
            invalidate,
            batch_ms: config.window,
            batch_bytes: config.inode_bytes,
            batch_global_bytes: config.global_bytes,
            txn_max_inodes: config.txn_max_inodes.max(1),
            txn_max_bytes: config.txn_max_bytes.max(1),
            state: RwLock::new(AgentFSWriteBatcherState::default()),
            commit_lock: AsyncMutex::new(()),
        }
    }

    fn queue(self: &Arc<Self>, ino: i64, ranges: Vec<WriteRange>) -> Result<EnqueueOutcome> {
        let ranges: Vec<_> = ranges
            .into_iter()
            .filter(|range| !range.data.is_empty())
            .collect();
        if ranges.is_empty() {
            return Ok(EnqueueOutcome::default());
        }

        let byte_count = ranges.iter().try_fold(0usize, |acc, range| {
            acc.checked_add(range.data.len())
                .ok_or_else(|| Error::Internal("batched write byte count overflow".to_string()))
        })?;
        let drain_now;
        let mut drain_all_now = false;
        let mut schedule_drain = false;

        {
            let mut state = self.state.write();
            drain_now = {
                let generation = state.next_generation();
                let entry = state
                    .pending
                    .entry(ino)
                    .or_insert_with(|| PendingInodeWrites::new(generation));
                entry.push_ranges(ranges, byte_count)?;
                entry.bump_generation(generation);
                crate::telemetry::record_agentfs_batcher_enqueue();
                crate::telemetry::record_agentfs_batcher_pending_bytes(entry.pending_bytes as u64);

                entry.pending_bytes >= self.batch_bytes
            };
            state.total_pending_bytes = state.total_pending_bytes.saturating_add(byte_count);
            // Global memory ceiling: a single full batched drain is cheaper and
            // frees more memory than draining just this inode, so it takes
            // precedence over the per-inode trigger.
            if state.total_pending_bytes >= self.batch_global_bytes {
                drain_all_now = true;
            }
            // Group commit: arm the single coalescing drain scheduler if it is
            // not already running, instead of one timer task per inode (which
            // degenerated into a storm of small serialized transactions during
            // a clone burst).
            if !state.drain_scheduled {
                state.drain_scheduled = true;
                schedule_drain = true;
            }
            state.debug_assert_total();
        }

        // Tier Four: invalidate the attr cache as soon as a write is queued,
        // not just when the batch commits to SQLite. getattr ORs in
        // peek_pending_max_end so the size view stays correct, but other
        // consumers (mtime/ctime, link count assumptions) must not see a
        // cached pre-write attr after a successful pwrite returns.
        (self.invalidate)(ino);

        if schedule_drain {
            self.spawn_drain_scheduler();
        }

        Ok(EnqueueOutcome {
            drain_inode: drain_now && !drain_all_now,
            drain_all: drain_all_now,
        })
    }

    #[cfg(test)]
    pub(super) async fn enqueue_for_test(
        self: &Arc<Self>,
        ino: i64,
        ranges: Vec<WriteRange>,
    ) -> Result<()> {
        let outcome = self.queue(ino, ranges)?;
        if outcome.drain_all {
            self.drain_all_reason(AgentFSWriteBatchDrainReason::Bytes)
                .await?;
        } else if outcome.drain_inode {
            self.drain_pending_batched(AgentFSWriteBatchDrainReason::Bytes, Some(ino))
                .await?;
        }
        Ok(())
    }

    pub(super) async fn drain_all_reason(
        self: &Arc<Self>,
        reason: AgentFSWriteBatchDrainReason,
    ) -> Result<()> {
        // Always batch on full drain: destroy / finalize / public AgentFS::drain_all.
        loop {
            self.drain_pending_batched(reason, None).await?;
            let still_pending = {
                let state = self.state.read();
                !state.pending.is_empty()
            };
            if !still_pending {
                return Ok(());
            }
        }
    }

    /// Drain currently-pending inode batches inside a single SQLite
    /// transaction. Holds one connection and one `BEGIN IMMEDIATE` / `COMMIT`
    /// pair across all per-inode chunk writes, so every drain trigger shares
    /// the same commit-then-remove discipline.
    ///
    /// One transaction is bounded by `txn_max_inodes` / `txn_max_bytes`
    /// (`AGENTFS_BATCH_TXN_INODES` / `AGENTFS_BATCH_TXN_BYTES`); when the
    /// pending map exceeds the bound the call commits a bounded subset and
    /// returns `Ok(true)` so the caller immediately drains again
    /// (back-to-back transactions) instead of building one unbounded txn.
    /// Returns `Ok(false)` when everything that was pending at snapshot time
    /// has been committed.
    ///
    /// `required_ino` lets per-inode drains (Bytes cap / explicit fsync,
    /// release, forget, setattr paths) express their caller contract: "the
    /// writes queued for this inode must be durable when this returns". If the
    /// inode is not in pending when we take the snapshot, it was committed by a
    /// concurrent drain and the contract is already met. If it IS pending, it
    /// is always selected into this transaction regardless of the
    /// per-transaction bounds.
    pub(super) async fn drain_pending_batched(
        self: &Arc<Self>,
        reason: AgentFSWriteBatchDrainReason,
        required_ino: Option<i64>,
    ) -> Result<bool> {
        let _commit_guard = self.commit_lock.lock().await;

        // Tier 4 corruption fix (commit-then-remove): SNAPSHOT pending ranges
        // by cloning, WITHOUT removing them from the overlay. `pread`/`getattr`
        // consult the overlay and then SQLite with no lock spanning the two; if
        // we removed the ranges here (as the original `mem::take` did), a read
        // landing between the take and `txn.commit` would find the write in
        // NEITHER the overlay nor committed SQLite and return stale data
        // (the intermittent git-clone corruption). Leaving the overlay
        // populated until after the commit guarantees every write is always
        // visible in the overlay OR in SQLite.
        let (snapshot, more_pending): (Vec<(i64, Vec<WriteRange>)>, bool) = {
            let state = self.state.read();
            let mut selected: Vec<(i64, Vec<WriteRange>)> = Vec::new();
            let mut selected_bytes = 0usize;
            let mut truncated = false;
            // The per-inode drain contract inode is always part of this
            // transaction, independent of the bounds.
            if let Some(req) = required_ino {
                if let Some(batch) = state.pending.get(&req) {
                    if !batch.ranges.is_empty() || batch.pending_times.is_some() {
                        selected_bytes = selected_bytes.saturating_add(batch.pending_bytes);
                        selected.push((req, batch.ranges.clone()));
                    }
                }
            }
            for (ino, batch) in state.pending.iter() {
                if Some(*ino) == required_ino {
                    continue;
                }
                if batch.ranges.is_empty() && batch.pending_times.is_none() {
                    continue;
                }
                // Bound the transaction by inode count and pending bytes; the
                // first selected inode is always admitted so progress is
                // guaranteed even when a single inode exceeds the byte bound.
                if selected.len() >= self.txn_max_inodes
                    || (!selected.is_empty()
                        && selected_bytes.saturating_add(batch.pending_bytes) > self.txn_max_bytes)
                {
                    truncated = true;
                    break;
                }
                selected_bytes = selected_bytes.saturating_add(batch.pending_bytes);
                selected.push((*ino, batch.ranges.clone()));
            }
            (selected, truncated)
        };

        if snapshot.is_empty() {
            self.cleanup_empty_pending();
            return Ok(false);
        }

        // (ino, committed_raw_range_count, normalized ranges to write).
        // Entries with an empty range list are still included: they carry
        // stashed explicit times (`pending_times`) that must be committed in
        // this transaction even though there is no data to write.
        let mut to_commit: Vec<(i64, usize, Vec<NormalizedWriteRange>)> =
            Vec::with_capacity(snapshot.len());
        for (ino, ranges) in &snapshot {
            let range_refs: Vec<_> = ranges
                .iter()
                .map(|range| WriteRangeRef {
                    offset: range.offset,
                    data: range.data.as_slice(),
                })
                .collect();
            // On normalize error the overlay is left intact (nothing removed),
            // so the ranges are simply retried on the next drain.
            let normalized = normalize_write_ranges(&range_refs)?;
            if !normalized.is_empty() {
                crate::telemetry::record_agentfs_batcher_coalesced_ranges(
                    ranges.len().saturating_sub(normalized.len()) as u64,
                );
                // Per-inode drain accounting (one tick per inode whose DATA we
                // actually commit, matching the old reporting cardinality —
                // time-only commits are not counted as drains).
                match reason {
                    AgentFSWriteBatchDrainReason::Timer => {
                        crate::telemetry::record_agentfs_batcher_drain_timer();
                    }
                    AgentFSWriteBatchDrainReason::Bytes => {
                        crate::telemetry::record_agentfs_batcher_drain_bytes();
                    }
                    AgentFSWriteBatchDrainReason::Explicit => {
                        crate::telemetry::record_agentfs_batcher_drain_explicit();
                    }
                }
            }
            to_commit.push((*ino, ranges.len(), normalized));
        }

        if to_commit.is_empty() {
            self.cleanup_empty_pending();
            return Ok(more_pending);
        }

        let started = Instant::now();
        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;

        // Read times_explicit and the stashed explicit times only AFTER the
        // IMMEDIATE transaction holds the SQLite write lock: explicit
        // chmod/chown/utimens that already landed marked the flag / stash
        // before their effect, later ones are blocked behind us (or stay
        // stashed for the next drain).
        let (preserve_times, pending_times): (HashMap<i64, bool>, HashMap<i64, PendingTimeChange>) = {
            let state = self.state.read();
            let preserve = to_commit
                .iter()
                .map(|(ino, _, _)| {
                    (
                        *ino,
                        state
                            .pending
                            .get(ino)
                            .map(|batch| batch.times_explicit)
                            .unwrap_or(false),
                    )
                })
                .collect();
            let times = to_commit
                .iter()
                .filter_map(|(ino, _, _)| {
                    state
                        .pending
                        .get(ino)
                        .and_then(|batch| batch.pending_times)
                        .map(|t| (*ino, t))
                })
                .collect();
            (preserve, times)
        };

        // Stashed times we actually applied inside this transaction; cleared
        // from the pending entries only after the commit succeeds.
        let mut applied_times: Vec<(i64, PendingTimeChange)> = Vec::new();

        for (ino, _count, normalized) in &to_commit {
            let mut inode_missing = false;
            if !normalized.is_empty() {
                let normalized_refs: Vec<_> = normalized
                    .iter()
                    .map(|range| WriteRangeRef {
                        offset: range.offset,
                        data: range.data.as_slice(),
                    })
                    .collect();
                let geometry = Geometry {
                    chunk_size: self.chunk_size,
                    inline_threshold: self.inline_threshold,
                };
                match store::write_ranges(
                    &conn,
                    *ino,
                    geometry,
                    &normalized_refs,
                    preserve_times.get(ino).copied().unwrap_or(false),
                    pending_times.get(ino),
                )
                .await
                {
                    Ok(()) => {}
                    // The file was unlinked / renamed-over while its writes were
                    // still pending (git lock and temp files routinely live and
                    // die within the batch window). Its data is moot: skip it and
                    // let the post-commit cleanup drop the orphaned ranges
                    // instead of aborting the whole multi-inode batch.
                    Err(Error::Fs(FsError::NotFound)) => {
                        tracing::debug!(
                            "AgentFS write batcher: dropping pending writes for deleted inode {}",
                            ino
                        );
                        inode_missing = true;
                    }
                    Err(error) => {
                        let _ = txn.rollback().await;
                        // Overlay was never modified; ranges remain pending and are
                        // retried on the next drain. No restore needed.
                        return Err(error);
                    }
                }
            }

            // Stashed explicit times ride the data UPDATE above
            // (`write_commit_time_sets`); a time-only entry has no data UPDATE
            // to fold into, so it pays one standalone UPDATE inside this same
            // transaction.
            if !inode_missing {
                if let Some(times) = pending_times.get(ino) {
                    if normalized.is_empty() {
                        if let Err(error) = apply_pending_times_with_conn(&conn, *ino, times).await
                        {
                            let _ = txn.rollback().await;
                            return Err(error);
                        }
                    }
                    applied_times.push((*ino, *times));
                }
            }
        }

        txn.commit().await?;

        // Durable now: drop exactly the committed ranges and applied times
        // from the overlay, preserving anything enqueued during the commit.
        for (ino, times) in &applied_times {
            self.clear_applied_times(*ino, times);
        }
        let committed_counts: Vec<(i64, usize)> = to_commit
            .iter()
            .map(|(ino, count, _)| (*ino, *count))
            .collect();
        self.remove_committed_prefix(&committed_counts);
        for (ino, _, _) in &to_commit {
            (self.invalidate)(*ino);
        }
        self.cleanup_empty_pending();
        // Anything still pending (ranges enqueued during the commit, inodes
        // beyond the per-txn bound, stashed times that arrived mid-commit) is
        // never stranded: the coalescing scheduler picks it up on its next
        // pass. Arm it if it is not already running (e.g. explicit drains
        // triggered by fsync / kill-switch paths outside the scheduler).
        self.ensure_drain_scheduled();
        crate::telemetry::record_agentfs_batcher_commit_latency(started.elapsed());
        crate::telemetry::record_agentfs_batcher_commit_txn(to_commit.len() as u64);

        Ok(more_pending)
    }

    /// Arm the single coalescing drain scheduler if it is not already armed
    /// and there is pending work left. Cheap safety net used after drains so
    /// residual pending state (ranges enqueued mid-commit, stashed times,
    /// inodes beyond a bounded transaction) always has a scheduled commit.
    fn ensure_drain_scheduled(self: &Arc<Self>) {
        let arm = {
            let mut state = self.state.write();
            if !state.drain_scheduled && !state.pending.is_empty() {
                state.drain_scheduled = true;
                true
            } else {
                false
            }
        };
        if arm {
            self.spawn_drain_scheduler();
        }
    }

    /// Spawn the coalescing drain scheduler task. Exactly one instance runs
    /// while `state.drain_scheduled` is true; it exits (and clears the flag)
    /// only once nothing is pending.
    fn spawn_drain_scheduler(self: &Arc<Self>) {
        let batcher = Arc::clone(self);
        tokio::spawn(async move {
            batcher.run_drain_scheduler().await;
        });
    }

    /// Single coalescing drain scheduler (cross-inode group commit).
    ///
    /// Instead of one timer task per written inode — which degenerated into a
    /// storm of small, serialized SQLite transactions during a git-clone burst
    /// — ONE task is armed when the first pending write arrives. Each cycle it
    /// sleeps `AGENTFS_BATCH_MS` so concurrent writers coalesce, then commits
    /// everything that is pending at that instant in as few `BEGIN IMMEDIATE`
    /// transactions as the per-transaction bounds allow
    /// (`AGENTFS_BATCH_TXN_INODES` / `AGENTFS_BATCH_TXN_BYTES`), back-to-back.
    /// Writes that arrive while a commit is in flight are picked up by the
    /// next cycle. The task exits only when nothing is pending; an enqueue
    /// that observes `drain_scheduled == false` arms a fresh one. Explicit
    /// drains (fsync, finalize, kill-switch paths) and the Bytes triggers are
    /// unaffected — they keep draining synchronously on their own call sites.
    async fn run_drain_scheduler(self: Arc<Self>) {
        loop {
            tokio::time::sleep(self.batch_ms).await;

            // One pass: commit everything pending at this instant, splitting
            // into bounded back-to-back transactions when over the per-txn
            // bounds. On error the overlay is left intact (commit-then-remove)
            // and the pass is retried on the next cycle.
            loop {
                match self
                    .drain_pending_batched(AgentFSWriteBatchDrainReason::Timer, None)
                    .await
                {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(error) => {
                        tracing::warn!(
                            "AgentFS write batcher: scheduled group drain failed (will retry): {}",
                            error
                        );
                        break;
                    }
                }
            }

            // Exit only when nothing is pending. The flag is cleared under the
            // same write lock that observes the empty map, so a concurrent
            // enqueue either sees the flag still set (this loop continues and
            // commits it) or sees it cleared and arms a fresh scheduler.
            let exit = {
                let mut state = self.state.write();
                if state.pending.is_empty() {
                    state.drain_scheduled = false;
                    true
                } else {
                    false
                }
            };
            if exit {
                return;
            }
        }
    }

    /// Tier 4 corruption fix: after a drain has durably committed a snapshot of
    /// pending ranges to SQLite, drop exactly those ranges from the overlay.
    /// Snapshots are taken from the front of each inode's append-only `ranges`
    /// vec, and enqueues only ever append, so the committed ranges are the first
    /// `count` entries; ranges appended during the commit are preserved. The
    /// `.min(len)` guard tolerates a concurrent `truncate_pending`/`discard_pending`
    /// having shrunk or removed the entry. Entries that still have ranges or
    /// stashed explicit times are kept; the coalescing scheduler (or the
    /// caller's `ensure_drain_scheduled`) commits them on a later pass.
    fn remove_committed_prefix(&self, committed: &[(i64, usize)]) {
        let mut state = self.state.write();
        for &(ino, count) in committed {
            let (removed_bytes, empty, should_bump) = {
                let Some(entry) = state.pending.get_mut(&ino) else {
                    continue;
                };
                let n = count.min(entry.ranges.len());
                let removed_bytes: usize = entry.ranges.drain(..n).map(|r| r.data.len()).sum();
                entry.pending_bytes = entry.pending_bytes.saturating_sub(removed_bytes);
                // An entry with stashed explicit times but no ranges is NOT
                // drained yet — keep it so a later drain commits the times.
                let empty = entry.is_drained();
                (removed_bytes, empty, removed_bytes > 0 || empty)
            };
            let generation = if should_bump {
                let generation = state.next_generation();
                if let Some(entry) = state.pending.get_mut(&ino) {
                    entry.bump_generation(generation);
                }
                Some(generation)
            } else {
                None
            };
            state.total_pending_bytes = state.total_pending_bytes.saturating_sub(removed_bytes);
            if empty {
                if let Some(generation) = generation {
                    state.retire_generation(ino, generation);
                }
                state.pending.remove(&ino);
            }
        }
        state.debug_assert_total();
    }

    /// Remove pending entries that have nothing left to commit (no ranges and
    /// no stashed explicit times). Clears their attr cache entry too.
    fn cleanup_empty_pending(&self) {
        let removed: Vec<i64> = {
            let mut state = self.state.write();
            let empties: Vec<i64> = state
                .pending
                .iter()
                .filter(|(_, b)| b.is_drained())
                .map(|(ino, _)| *ino)
                .collect();
            for ino in &empties {
                if let Some(b) = state.pending.remove(ino) {
                    state.total_pending_bytes =
                        state.total_pending_bytes.saturating_sub(b.pending_bytes);
                    state.retire_generation(*ino, b.generation);
                }
            }
            state.debug_assert_total();
            empties
        };
        for ino in removed {
            (self.invalidate)(ino);
        }
    }

    // ----- Tier Four: in-memory overlay read API -----
    //
    // These methods let `AgentFSFile::pread` / `getattr` / `truncate` consult
    // the batcher's pending state directly, instead of forcing a synchronous
    // SQLite drain for read-after-write consistency. The drain becomes a
    // pure durability operation, only triggered by explicit `fsync` /
    // destroy / timer / bytes triggers.

    /// Snapshot pending writes for `ino` overlapping `[offset, offset+size)`.
    /// Returned ranges are normalised (non-overlapping, sorted) and clipped
    /// to the requested window. The batcher's pending state is not modified.
    /// Callers merge the result over SQLite data with "pending wins"
    /// semantics; see `AgentFSFile::pread`.
    fn pending_ranges(&self, ino: i64, offset: u64, size: u64) -> Vec<NormalizedWriteRange> {
        if size == 0 {
            return Vec::new();
        }
        let read_end = match offset.checked_add(size) {
            Some(end) => end,
            None => return Vec::new(),
        };
        // Read-lock: many concurrent readers OK; writers block briefly during
        // enqueue. Crucially, no `.await` is performed while the guard is
        // held, so a sync `parking_lot::RwLock` is safe inside an async fn.
        let state = self.state.read();
        let Some(batch) = state.pending.get(&ino) else {
            return Vec::new();
        };
        if batch.ranges.is_empty() {
            return Vec::new();
        }
        let refs: Vec<_> = batch
            .ranges
            .iter()
            .map(|r| WriteRangeRef {
                offset: r.offset,
                data: r.data.as_slice(),
            })
            .collect();
        let normalized = match normalize_write_ranges(&refs) {
            Ok(n) => n,
            Err(_) => return Vec::new(),
        };
        normalized
            .into_iter()
            .filter_map(|range| {
                let r_end = range.offset + range.data.len() as u64;
                if r_end <= offset || range.offset >= read_end {
                    return None;
                }
                let clip_start = offset.max(range.offset);
                let clip_end = read_end.min(r_end);
                if clip_end <= clip_start {
                    return None;
                }
                let skip = (clip_start - range.offset) as usize;
                let take = (clip_end - clip_start) as usize;
                Some(NormalizedWriteRange {
                    offset: clip_start,
                    data: range.data[skip..skip + take].to_vec(),
                })
            })
            .collect()
    }

    /// Fast path for "does this inode have ANY pending write?" — used by
    /// readers to skip the heavier `peek_pending_max_end` / `peek_pending`
    /// calls entirely when the batcher has nothing for the inode. Read lock,
    /// O(1) HashMap hit.
    pub(super) fn has_pending(&self, ino: i64) -> bool {
        let state = self.state.read();
        state
            .pending
            .get(&ino)
            .map(|b| !b.ranges.is_empty())
            .unwrap_or(false)
    }

    /// Largest write end (offset + length) for `ino` across all pending
    /// ranges. Returns `None` if no pending writes for this inode. Callers
    /// OR this with the SQLite-stored `fs_inode.size` to compute the
    /// file-size view exposed to readers (so a write that grows the file is
    /// visible to subsequent `getattr` even before the timer drain commits
    /// it to SQLite).
    pub(super) fn pending_max_end_raw(&self, ino: i64) -> Option<u64> {
        let state = self.state.read();
        let batch = state.pending.get(&ino)?;
        batch
            .ranges
            .iter()
            .map(|r| r.offset.saturating_add(r.data.len() as u64))
            .max()
    }

    /// Record that an explicit attribute change (chmod / chown / utimens) was
    /// applied to fs_inode for `ino` after the writes currently pending for
    /// it. The eventual data commit must then preserve mtime/ctime instead of
    /// stamping the commit time (see `drain_pending_batched`). No-op when the
    /// inode has nothing pending — there is no deferred commit to clobber the
    /// attributes in that case.
    pub(super) fn mark_times_explicit(&self, ino: i64) {
        let mut state = self.state.write();
        if let Some(batch) = state.pending.get_mut(&ino) {
            batch.times_explicit = true;
            state.bump_generation(ino);
        }
    }

    /// Stash explicitly-set `utimens` values in the inode's pending entry so
    /// the next batched drain commits them inside the SAME transaction as any
    /// pending data (one extra UPDATE statement, zero dedicated foreground
    /// transactions). The entry is created when the inode has nothing pending
    /// yet: the kernel's writeback SETATTR routinely lands after the inode's
    /// data already drained (or before its flush arrives), and creating the
    /// entry both removes the per-file fallback transaction and guarantees a
    /// scheduled drain applies the times. `merge_pending_view` keeps the
    /// change visible to getattr/lookup immediately.
    pub(super) fn stash_pending_times(self: &Arc<Self>, ino: i64, change: PendingTimeChange) {
        if change.is_empty() {
            return;
        }
        {
            let mut state = self.state.write();
            let generation = state.next_generation();
            state
                .pending
                .entry(ino)
                .or_insert_with(|| PendingInodeWrites::new(generation))
                .pending_times
                .get_or_insert_with(PendingTimeChange::default)
                .apply(&change);
            if let Some(entry) = state.pending.get_mut(&ino) {
                entry.bump_generation(generation);
            }
        }
        (self.invalidate)(ino);
        // A times-only entry still needs a scheduled drain to commit it.
        self.ensure_drain_scheduled();
    }

    /// Snapshot the stashed explicit times for `ino` (if any) without removing
    /// them. Drains read this AFTER their `BEGIN IMMEDIATE` holds the write
    /// lock and clear exactly what they applied once the commit succeeds
    /// (commit-then-remove, mirroring the data-range discipline). Readers use
    /// it to overlay not-yet-committed times onto fs_inode rows.
    pub(super) fn pending_times_raw(&self, ino: i64) -> Option<PendingTimeChange> {
        let state = self.state.read();
        state.pending.get(&ino).and_then(|b| b.pending_times)
    }

    pub(super) fn pending_generation_raw(&self, ino: i64) -> PendingGeneration {
        let state = self.state.read();
        state.generation(ino)
    }

    /// After a successful commit, drop the stashed time fields that were
    /// actually applied. Field-wise equality keeps any NEWER stash that
    /// arrived while the transaction was in flight (it will be committed by
    /// the next drain instead of being silently lost).
    fn clear_applied_times(&self, ino: i64, applied: &PendingTimeChange) {
        let mut state = self.state.write();
        let Some(batch) = state.pending.get_mut(&ino) else {
            return;
        };
        let Some(times) = &mut batch.pending_times else {
            return;
        };
        if times.atime == applied.atime {
            times.atime = None;
        }
        if times.mtime == applied.mtime {
            times.mtime = None;
        }
        if times.ctime == applied.ctime {
            times.ctime = None;
        }
        if times.is_empty() {
            batch.pending_times = None;
        }
        state.bump_generation(ino);
    }

    /// Drop any pending bytes beyond `new_size` and shrink ranges that span
    /// the truncation boundary. Called by `AgentFSFile::truncate` so the
    /// overlay agrees with the post-truncate file state without needing to
    /// drain first.
    pub(super) fn truncate_pending(&self, ino: i64, new_size: u64) {
        let mut state = self.state.write();
        if !state.pending.contains_key(&ino) {
            return;
        }
        let generation = state.next_generation();
        let (old_bytes, new_bytes, now_empty) = {
            let Some(batch) = state.pending.get_mut(&ino) else {
                return;
            };
            let old_bytes = batch.pending_bytes;
            let mut new_bytes = 0usize;
            batch.ranges.retain_mut(|range| {
                let r_end = range.offset.saturating_add(range.data.len() as u64);
                if range.offset >= new_size {
                    return false;
                }
                if r_end > new_size {
                    let keep = (new_size - range.offset) as usize;
                    range.data.truncate(keep);
                }
                new_bytes = new_bytes.saturating_add(range.data.len());
                !range.data.is_empty()
            });
            batch.pending_bytes = new_bytes;
            batch.bump_generation(generation);
            (old_bytes, new_bytes, batch.ranges.is_empty())
        };
        state.total_pending_bytes = state
            .total_pending_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if now_empty {
            state.retire_generation(ino, generation);
            state.pending.remove(&ino);
        }
        state.debug_assert_total();
    }

    /// Discard every pending write for a reaped inode once the deletion is
    /// committed (or before a deferred reap transaction opens). This prevents
    /// stale pending data from surviving past the inode's logical lifetime.
    pub(super) fn discard_pending(&self, ino: i64) {
        let mut state = self.state.write();
        if let Some(batch) = state.pending.remove(&ino) {
            state.total_pending_bytes = state
                .total_pending_bytes
                .saturating_sub(batch.pending_bytes);
            let generation = state.next_generation();
            state.retire_generation(ino, generation);
        }
        state.debug_assert_total();
    }

    #[cfg(test)]
    pub(super) fn total_pending_bytes(&self) -> usize {
        self.state.read().total_pending_bytes
    }

    #[cfg(test)]
    pub(super) fn retired_generation_count(&self) -> usize {
        self.state.read().retired_generations.len()
    }

    #[cfg(test)]
    pub(super) fn retired_generation_contains(&self, ino: i64) -> bool {
        self.state.read().retired_generations.contains_key(&ino)
    }
}

impl PendingView for BatcherPendingView {
    fn has_pending(&self, ino: i64) -> bool {
        self.batcher.has_pending(ino)
    }

    fn overlay_read(&self, ino: i64, off: u64, buf: &mut [u8]) -> OverlayHit {
        let mut applied = false;
        for range in self.batcher.pending_ranges(ino, off, buf.len() as u64) {
            let dst_off = (range.offset - off) as usize;
            if dst_off >= buf.len() {
                continue;
            }
            let end = (dst_off + range.data.len()).min(buf.len());
            buf[dst_off..end].copy_from_slice(&range.data[..end - dst_off]);
            applied = true;
        }
        OverlayHit { applied }
    }

    fn pending_max_end(&self, ino: i64) -> Option<u64> {
        self.batcher.pending_max_end_raw(ino)
    }

    fn pending_times(&self, ino: i64) -> Option<PendingTimes> {
        self.batcher.pending_times_raw(ino)
    }

    fn pending_generation(&self, ino: i64) -> PendingGeneration {
        self.batcher.pending_generation_raw(ino)
    }

    fn merge_into_stats(&self, ino: i64, stats: &mut Stats) {
        if let Some(times) = self.pending_times(ino) {
            times.merge_into(stats);
        }
        if let Some(pending_end) = self.pending_max_end(ino) {
            let pending_end_i64 = i64::try_from(pending_end).unwrap_or(i64::MAX);
            if pending_end_i64 > stats.size {
                stats.size = pending_end_i64;
            }
        }
    }
}

#[async_trait]
impl Drain for BatcherDrain {
    async fn drain_inode(&self, ino: i64) -> Result<()> {
        self.batcher
            .drain_pending_batched(AgentFSWriteBatchDrainReason::Explicit, Some(ino))
            .await?;
        Ok(())
    }

    async fn drain_all(&self) -> Result<()> {
        self.batcher
            .drain_all_reason(AgentFSWriteBatchDrainReason::Explicit)
            .await
    }

    async fn drain_inode_bytes(&self, ino: i64) -> Result<()> {
        self.batcher
            .drain_pending_batched(AgentFSWriteBatchDrainReason::Bytes, Some(ino))
            .await?;
        Ok(())
    }

    async fn drain_all_bytes(&self) -> Result<()> {
        self.batcher
            .drain_all_reason(AgentFSWriteBatchDrainReason::Bytes)
            .await
    }

    fn enqueue(&self, ino: i64, ranges: Vec<WriteRange>) -> Result<EnqueueOutcome> {
        self.batcher.queue(ino, ranges)
    }

    fn discard_pending(&self, ino: i64) {
        self.batcher.discard_pending(ino);
    }

    fn mark_times_explicit(&self, ino: i64) {
        self.batcher.mark_times_explicit(ino);
    }

    fn truncate_pending(&self, ino: i64, size: u64) {
        self.batcher.truncate_pending(ino, size);
    }

    fn stash_times(&self, ino: i64, times: PendingTimes) {
        self.batcher.stash_pending_times(ino, times);
    }
}
