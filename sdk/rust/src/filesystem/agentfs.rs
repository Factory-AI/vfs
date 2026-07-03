use crate::error::{Error, Result};
use async_trait::async_trait;
use lru::LruCache;
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, Value};

#[cfg(test)]
use super::DEFAULT_FILE_MODE;
use super::{
    BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, Stats, TimeChange, WriteRange,
    DEFAULT_DIR_MODE, MAX_NAME_LEN, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG,
};
use crate::config::{
    BatcherConfig, CoreConfig, Geometry, DEFAULT_CHUNK_SIZE, DEFAULT_INLINE_THRESHOLD,
};
use crate::connection_pool::{ConnectionPool, ConnectionPoolOptions};
use crate::schema::{self, AGENTFS_SCHEMA_VERSION};

const ROOT_INO: i64 = 1;
const STORAGE_CHUNKED: i64 = 0;
const STORAGE_INLINE: i64 = 1;
const DENTRY_CACHE_MAX_SIZE: usize = 10000;
const NEGATIVE_DENTRY_CACHE_MAX_SIZE: usize = 10000;
const FILE_BACKED_MAX_CONNECTIONS: usize = 8;
const TEMP_STORE_MEMORY_SQL: &str = "PRAGMA temp_store = MEMORY";
const BUSY_TIMEOUT_SQL: &str = "PRAGMA busy_timeout = 5000";
const WAL_MODE_SQL: &str = "PRAGMA journal_mode = WAL";
const BASELINE_SYNCHRONOUS_SQL: &str = "PRAGMA synchronous = NORMAL";
const DURABLE_SYNCHRONOUS_SQL: &str = "PRAGMA synchronous = FULL";
const WAL_CHECKPOINT_SQL: &str = "PRAGMA wal_checkpoint(TRUNCATE)";
const FILE_BACKED_SETUP_SQL: &[&str] = &[
    TEMP_STORE_MEMORY_SQL,
    BUSY_TIMEOUT_SQL,
    WAL_MODE_SQL,
    BASELINE_SYNCHRONOUS_SQL,
];
const ATTR_CACHE_MAX_SIZE: usize = 10000;

/// Production connection-pool options for local file-backed AgentFS databases.
pub(crate) fn file_backed_connection_pool_options() -> ConnectionPoolOptions {
    ConnectionPoolOptions {
        max_connections: FILE_BACKED_MAX_CONNECTIONS,
        ..ConnectionPoolOptions::default().with_setup_sql(FILE_BACKED_SETUP_SQL.iter().copied())
    }
}

async fn checkpoint_wal(conn: &Connection) -> Result<()> {
    let _checkpoint_timer =
        crate::profiling::timer(&crate::profiling::CORE_COUNTERS.wal_checkpoint);
    let mut rows = conn.query(WAL_CHECKPOINT_SQL, ()).await?;
    while rows.next().await?.is_some() {}
    Ok(())
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), suffix))
}

fn remove_checkpointed_sidecars(path: &Path) -> Result<()> {
    let wal = sqlite_sidecar_path(path, "-wal");
    if let Ok(metadata) = std::fs::metadata(&wal) {
        if metadata.len() == 0 {
            std::fs::remove_file(&wal)?;
        }
    }

    let shm = sqlite_sidecar_path(path, "-shm");
    if shm.exists() {
        std::fs::remove_file(&shm)?;
    }
    Ok(())
}

fn is_duplicate_column_error(err: &turso::Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("duplicate column")
}

#[derive(Clone, Copy)]
struct ColumnSpec {
    table_name: &'static str,
    column_name: &'static str,
    type_name: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
}

async fn add_column_idempotent(conn: &Connection, spec: ColumnSpec, sql: &str) -> Result<()> {
    match conn.execute(sql, ()).await {
        Ok(_) => Ok(()),
        Err(err) if is_duplicate_column_error(&err) => ensure_column_matches(conn, spec).await,
        Err(err) => Err(Error::Internal(format!(
            "schema ALTER failed while adding {}.{}: {err}",
            spec.table_name, spec.column_name
        ))),
    }
}

async fn ensure_column_matches(conn: &Connection, spec: ColumnSpec) -> Result<()> {
    let mut rows = conn
        .query(&format!("PRAGMA table_info({})", spec.table_name), ())
        .await?;

    while let Some(row) = rows.next().await? {
        let column_name: String = row.get(1)?;
        if column_name != spec.column_name {
            continue;
        }

        let type_name: String = row.get(2)?;
        let not_null: i64 = row.get(3)?;
        let default_value = match row.get_value(4).ok() {
            Some(Value::Text(value)) => Some(value.clone()),
            Some(Value::Integer(value)) => Some(value.to_string()),
            Some(Value::Null) | None => None,
            Some(value) => Some(format!("{value:?}")),
        };
        let type_matches = type_name.eq_ignore_ascii_case(spec.type_name);
        let not_null_matches = (not_null != 0) == spec.not_null;
        let default_matches = default_value.as_deref() == spec.default_value;

        if type_matches && not_null_matches && default_matches {
            return Ok(());
        }

        return Err(Error::Internal(format!(
            "schema column {}.{} already exists with incompatible definition: \
             expected type={} not_null={} default={:?}; \
             found type={} not_null={} default={:?}",
            spec.table_name,
            spec.column_name,
            spec.type_name,
            spec.not_null,
            spec.default_value,
            type_name,
            not_null != 0,
            default_value
        )));
    }

    Err(Error::Internal(format!(
        "schema ALTER reported duplicate column {}.{}, but PRAGMA table_info did not find it",
        spec.table_name, spec.column_name
    )))
}

/// LRU cache for directory entry lookups.
///
/// Maps (parent_ino, name) -> child_ino to avoid repeated database queries
/// during path resolution. For a path like `/a/b/c/d`, this reduces queries
/// from 4 to potentially 0 on cache hits.
struct DentryCache {
    // Mutex required because LruCache::get() mutates internal order
    entries: Mutex<LruCache<(i64, String), i64>>,
}

impl DentryCache {
    fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).expect("cache size must be > 0"),
            )),
        }
    }

    /// Look up a cached entry (updates LRU order)
    fn get(&self, parent_ino: i64, name: &str) -> Option<i64> {
        let entry = self
            .entries
            .lock()
            .unwrap()
            .get(&(parent_ino, name.to_string()))
            .copied();
        if entry.is_some() {
            crate::profiling::record_dentry_cache_hit();
            crate::profiling::record_path_cache_hit();
        } else {
            crate::profiling::record_dentry_cache_miss();
            crate::profiling::record_path_cache_miss();
        }
        entry
    }

    /// Insert an entry into the cache (evicts LRU entry if full)
    fn insert(&self, parent_ino: i64, name: &str, child_ino: i64) {
        self.entries
            .lock()
            .unwrap()
            .put((parent_ino, name.to_string()), child_ino);
    }

    /// Remove an entry from the cache
    fn remove(&self, parent_ino: i64, name: &str) {
        self.entries
            .lock()
            .unwrap()
            .pop(&(parent_ino, name.to_string()));
    }
}

/// LRU cache for safe negative directory entry lookups.
///
/// A negative entry means "this (parent, name) did not exist in the last
/// serialized AgentFS view". Every namespace mutation invalidates exactly the
/// affected key before the mutation reports success, so cached ENOENT results
/// cannot hide later creates or renames made through this filesystem.
struct NegativeDentryCache {
    entries: Mutex<LruCache<(i64, String), ()>>,
}

impl NegativeDentryCache {
    fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).expect("cache size must be > 0"),
            )),
        }
    }

    fn contains(&self, parent_ino: i64, name: &str) -> bool {
        let cached = self
            .entries
            .lock()
            .unwrap()
            .get(&(parent_ino, name.to_string()))
            .is_some();
        if cached {
            crate::profiling::record_negative_cache_hit();
        } else {
            crate::profiling::record_negative_cache_miss();
        }
        cached
    }

    fn insert(&self, parent_ino: i64, name: &str) {
        self.entries
            .lock()
            .unwrap()
            .put((parent_ino, name.to_string()), ());
    }

    fn remove(&self, parent_ino: i64, name: &str) {
        if self
            .entries
            .lock()
            .unwrap()
            .pop(&(parent_ino, name.to_string()))
            .is_some()
        {
            crate::profiling::record_negative_cache_invalidation();
        }
    }
}

/// LRU cache for inode attributes.
///
/// FUSE and SDK stat-heavy read paths often ask for the same inode metadata
/// repeatedly after lookup/readdir_plus. This cache is conservative: every
/// namespace, metadata, or size/content mutation invalidates the affected inode
/// and parent directory entries before the mutation is considered complete.
struct AttrCache {
    entries: Mutex<LruCache<i64, Stats>>,
}

impl AttrCache {
    fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(LruCache::new(
                NonZeroUsize::new(max_size).expect("cache size must be > 0"),
            )),
        }
    }

    fn get(&self, ino: i64) -> Option<Stats> {
        let stats = self.entries.lock().unwrap().get(&ino).cloned();
        if stats.is_some() {
            crate::profiling::record_attr_cache_hit();
        } else {
            crate::profiling::record_attr_cache_miss();
        }
        stats
    }

    fn insert(&self, stats: Stats) {
        self.entries.lock().unwrap().put(stats.ino, stats);
    }

    fn remove(&self, ino: i64) {
        self.entries.lock().unwrap().pop(&ino);
    }
}

#[derive(Debug, Clone, Copy)]
enum AgentFSWriteBatchDrainReason {
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
struct PendingTimeChange {
    /// (secs, nsec) for atime, when explicitly set.
    atime: Option<(i64, i64)>,
    /// (secs, nsec) for mtime, when explicitly set.
    mtime: Option<(i64, i64)>,
    /// (secs, nsec) for ctime (always bumped by the setattr that stashed).
    ctime: Option<(i64, i64)>,
}

impl PendingTimeChange {
    fn is_empty(&self) -> bool {
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
    fn merge_into(&self, stats: &mut Stats) {
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
fn write_commit_time_sets(
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
    fn new() -> Self {
        Self {
            ranges: Vec::new(),
            pending_bytes: 0,
            times_explicit: false,
            pending_times: None,
        }
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

#[derive(Default)]
struct AgentFSWriteBatcherState {
    pending: HashMap<i64, PendingInodeWrites>,
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
struct AgentFSWriteBatcher {
    pool: ConnectionPool,
    chunk_size: usize,
    inline_threshold: usize,
    attr_cache: Arc<AttrCache>,
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
    fn from_config(
        pool: ConnectionPool,
        chunk_size: usize,
        inline_threshold: usize,
        attr_cache: Arc<AttrCache>,
        config: &BatcherConfig,
    ) -> Self {
        Self {
            pool,
            chunk_size,
            inline_threshold,
            attr_cache,
            batch_ms: config.window,
            batch_bytes: config.inode_bytes,
            batch_global_bytes: config.global_bytes,
            txn_max_inodes: config.txn_max_inodes.max(1),
            txn_max_bytes: config.txn_max_bytes.max(1),
            state: RwLock::new(AgentFSWriteBatcherState::default()),
            commit_lock: AsyncMutex::new(()),
        }
    }

    async fn enqueue(self: &Arc<Self>, ino: i64, ranges: Vec<WriteRange>) -> Result<()> {
        let ranges: Vec<_> = ranges
            .into_iter()
            .filter(|range| !range.data.is_empty())
            .collect();
        if ranges.is_empty() {
            return Ok(());
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
                let entry = state
                    .pending
                    .entry(ino)
                    .or_insert_with(PendingInodeWrites::new);
                entry.push_ranges(ranges, byte_count)?;
                crate::profiling::record_agentfs_batcher_enqueue();
                crate::profiling::record_agentfs_batcher_pending_bytes(entry.pending_bytes as u64);

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
        self.attr_cache.remove(ino);

        if schedule_drain {
            self.spawn_drain_scheduler();
        }

        if drain_all_now {
            self.drain_all(AgentFSWriteBatchDrainReason::Bytes).await?;
        } else if drain_now {
            self.drain_pending_batched(AgentFSWriteBatchDrainReason::Bytes, Some(ino))
                .await?;
        }

        Ok(())
    }

    async fn drain_all(self: &Arc<Self>, reason: AgentFSWriteBatchDrainReason) -> Result<()> {
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
    async fn drain_pending_batched(
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
                crate::profiling::record_agentfs_batcher_coalesced_ranges(
                    ranges.len().saturating_sub(normalized.len()) as u64,
                );
                // Per-inode drain accounting (one tick per inode whose DATA we
                // actually commit, matching the old reporting cardinality —
                // time-only commits are not counted as drains).
                match reason {
                    AgentFSWriteBatchDrainReason::Timer => {
                        crate::profiling::record_agentfs_batcher_drain_timer();
                    }
                    AgentFSWriteBatchDrainReason::Bytes => {
                        crate::profiling::record_agentfs_batcher_drain_bytes();
                    }
                    AgentFSWriteBatchDrainReason::Explicit => {
                        crate::profiling::record_agentfs_batcher_drain_explicit();
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
                let file = AgentFSFile {
                    pool: self.pool.clone(),
                    ino: *ino,
                    chunk_size: self.chunk_size,
                    inline_threshold: self.inline_threshold,
                    attr_cache: self.attr_cache.clone(),
                    write_batcher: None,
                    overlay_reads: true,
                    _open_guard: None,
                };
                match file
                    .pwrite_ranges_inode_with_conn(
                        &conn,
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
            self.attr_cache.remove(*ino);
        }
        self.cleanup_empty_pending();
        // Anything still pending (ranges enqueued during the commit, inodes
        // beyond the per-txn bound, stashed times that arrived mid-commit) is
        // never stranded: the coalescing scheduler picks it up on its next
        // pass. Arm it if it is not already running (e.g. explicit drains
        // triggered by fsync / kill-switch paths outside the scheduler).
        self.ensure_drain_scheduled();
        crate::profiling::record_agentfs_batcher_commit_latency(started.elapsed());
        crate::profiling::record_agentfs_batcher_commit_txn(to_commit.len() as u64);

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
            let (removed_bytes, empty) = {
                let Some(entry) = state.pending.get_mut(&ino) else {
                    continue;
                };
                let n = count.min(entry.ranges.len());
                let removed_bytes: usize = entry.ranges.drain(..n).map(|r| r.data.len()).sum();
                entry.pending_bytes = entry.pending_bytes.saturating_sub(removed_bytes);
                // An entry with stashed explicit times but no ranges is NOT
                // drained yet — keep it so a later drain commits the times.
                (removed_bytes, entry.is_drained())
            };
            state.total_pending_bytes = state.total_pending_bytes.saturating_sub(removed_bytes);
            if empty {
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
                }
            }
            state.debug_assert_total();
            empties
        };
        for ino in removed {
            self.attr_cache.remove(ino);
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
    fn peek_pending(&self, ino: i64, offset: u64, size: u64) -> Vec<NormalizedWriteRange> {
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
    fn has_pending(&self, ino: i64) -> bool {
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
    fn peek_pending_max_end(&self, ino: i64) -> Option<u64> {
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
    fn mark_times_explicit(&self, ino: i64) {
        let mut state = self.state.write();
        if let Some(batch) = state.pending.get_mut(&ino) {
            batch.times_explicit = true;
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
    fn stash_pending_times(self: &Arc<Self>, ino: i64, change: PendingTimeChange) {
        if change.is_empty() {
            return;
        }
        {
            let mut state = self.state.write();
            state
                .pending
                .entry(ino)
                .or_insert_with(PendingInodeWrites::new)
                .pending_times
                .get_or_insert_with(PendingTimeChange::default)
                .apply(&change);
        }
        // A times-only entry still needs a scheduled drain to commit it.
        self.ensure_drain_scheduled();
    }

    /// Snapshot the stashed explicit times for `ino` (if any) without removing
    /// them. Drains read this AFTER their `BEGIN IMMEDIATE` holds the write
    /// lock and clear exactly what they applied once the commit succeeds
    /// (commit-then-remove, mirroring the data-range discipline). Readers use
    /// it to overlay not-yet-committed times onto fs_inode rows.
    fn peek_pending_times(&self, ino: i64) -> Option<PendingTimeChange> {
        let state = self.state.read();
        state.pending.get(&ino).and_then(|b| b.pending_times)
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
    }

    /// Drop any pending bytes beyond `new_size` and shrink ranges that span
    /// the truncation boundary. Called by `AgentFSFile::truncate` so the
    /// overlay agrees with the post-truncate file state without needing to
    /// drain first.
    fn truncate_pending(&self, ino: i64, new_size: u64) {
        let mut state = self.state.write();
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
            (old_bytes, new_bytes, batch.ranges.is_empty())
        };
        state.total_pending_bytes = state
            .total_pending_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if now_empty {
            state.pending.remove(&ino);
        }
        state.debug_assert_total();
    }

    /// Discard every pending write for `ino`. Used by `AgentFS::unlink`
    /// after the inode row is deleted, to avoid `fs_data` orphan rows when
    /// the timer later tries to commit ranges for a no-longer-existent ino.
    fn discard_pending(&self, ino: i64) {
        let mut state = self.state.write();
        if let Some(batch) = state.pending.remove(&ino) {
            state.total_pending_bytes = state
                .total_pending_bytes
                .saturating_sub(batch.pending_bytes);
        }
        state.debug_assert_total();
    }
}

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

/// A filesystem backed by SQLite
#[derive(Clone)]
pub struct AgentFS {
    pool: ConnectionPool,
    db_path: Option<Arc<PathBuf>>,
    chunk_size: usize,
    inline_threshold: usize,
    /// Cache for directory entry lookups (shared across clones)
    dentry_cache: Arc<DentryCache>,
    /// Cache for negative directory entry lookups (shared across clones)
    negative_dentry_cache: Arc<NegativeDentryCache>,
    /// Cache for inode attributes (shared across clones)
    attr_cache: Arc<AttrCache>,
    /// Optional write batcher used by FUSE writeback mode.
    write_batcher: Option<Arc<AgentFSWriteBatcher>>,
    /// Tier 4 escape hatch: when false (`AGENTFS_OVERLAY_READS=0`), the SDK
    /// behaves like Tier 3 — every pwrite drains, every pread drains,
    /// `merge_pending_view` is a no-op. ON by default.
    overlay_reads: bool,
    /// Typed runtime configuration captured once when the filesystem opens.
    core_config: Arc<CoreConfig>,
    /// Live open-handle registry for deferred orphan reaping (see
    /// [`OpenInodes`]).
    open_inodes: Arc<OpenInodes>,
}

/// Tracks inodes with live `AgentFSFile` handles so unlink and
/// rename-replace can defer row deletion: POSIX requires an
/// unlinked-but-open file to stay readable and writable until its last
/// handle closes. `nlink = 0` in `fs_inode` is the crash-safe orphan
/// marker — deferred inodes are queued here when their last handle drops
/// and reaped by `process_deferred_reaps` (unlink/rename/finalize) or, after
/// a crash, by the mount-time sweep.
#[derive(Default)]
pub(crate) struct OpenInodes {
    inner: Mutex<OpenInodesInner>,
}

#[derive(Default)]
struct OpenInodesInner {
    counts: HashMap<i64, u32>,
    orphaned: HashSet<i64>,
    reap_queue: Vec<i64>,
}

impl OpenInodes {
    fn guard(self: &Arc<Self>, ino: i64) -> OpenInodeGuard {
        let mut inner = self.inner.lock().unwrap();
        *inner.counts.entry(ino).or_insert(0) += 1;
        OpenInodeGuard {
            registry: Arc::clone(self),
            ino,
        }
    }

    /// Marks the inode for deferred reaping when handles are live.
    /// Returns true when the caller must NOT delete the rows yet.
    fn defer_reap_if_open(&self, ino: i64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        if inner.counts.contains_key(&ino) {
            inner.orphaned.insert(ino);
            true
        } else {
            false
        }
    }

    fn release(&self, ino: i64) {
        let mut inner = self.inner.lock().unwrap();
        match inner.counts.get_mut(&ino) {
            Some(count) if *count > 1 => *count -= 1,
            Some(_) => {
                inner.counts.remove(&ino);
                if inner.orphaned.remove(&ino) {
                    inner.reap_queue.push(ino);
                }
            }
            None => {}
        }
    }

    fn take_reap_queue(&self) -> Vec<i64> {
        let mut inner = self.inner.lock().unwrap();
        std::mem::take(&mut inner.reap_queue)
    }

    fn requeue_reaps(&self, inos: Vec<i64>) {
        let mut inner = self.inner.lock().unwrap();
        inner.reap_queue.extend(inos);
    }

    fn has_pending_reaps(&self) -> bool {
        !self.inner.lock().unwrap().reap_queue.is_empty()
    }
}

/// RAII registration of one `AgentFSFile` in [`OpenInodes`].
pub(crate) struct OpenInodeGuard {
    registry: Arc<OpenInodes>,
    ino: i64,
}

impl Drop for OpenInodeGuard {
    fn drop(&mut self) {
        self.registry.release(self.ino);
    }
}

/// An open file handle for AgentFS.
///
/// This struct holds the inode number resolved at open time, allowing
/// efficient read/write/fsync operations without path lookups.
pub struct AgentFSFile {
    pool: ConnectionPool,
    ino: i64,
    chunk_size: usize,
    inline_threshold: usize,
    attr_cache: Arc<AttrCache>,
    write_batcher: Option<Arc<AgentFSWriteBatcher>>,
    /// Same semantics as the field on `AgentFS`; cloned at open time so the
    /// hot read/write path doesn't have to chase an extra indirection.
    overlay_reads: bool,
    /// None only for the batcher's ephemeral internal handles; user-visible
    /// handles register so unlink defers inode reaping while they live.
    _open_guard: Option<OpenInodeGuard>,
}

struct FileStorage {
    size: u64,
    storage_kind: i64,
    inline_data: Option<Vec<u8>>,
}

struct WriteRangeRef<'a> {
    offset: u64,
    data: &'a [u8],
}

#[derive(Clone)]
struct NormalizedWriteRange {
    offset: u64,
    data: Vec<u8>,
}

impl NormalizedWriteRange {
    fn end(&self) -> u64 {
        self.offset + self.data.len() as u64
    }
}

fn current_timestamp() -> Result<(i64, i64)> {
    let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok((dur.as_secs() as i64, dur.subsec_nanos() as i64))
}

fn normalize_write_ranges(ranges: &[WriteRangeRef<'_>]) -> Result<Vec<NormalizedWriteRange>> {
    let mut merged_ranges: BTreeMap<u64, Vec<u8>> = BTreeMap::new();

    for range in ranges {
        if range.data.is_empty() {
            continue;
        }

        let data_len = u64::try_from(range.data.len())
            .map_err(|_| Error::Internal("file write length overflow".to_string()))?;
        let write_start = range.offset;
        let write_end = write_start
            .checked_add(data_len)
            .ok_or_else(|| Error::Internal("file write offset overflow".to_string()))?;
        let mut start = write_start;
        let mut end = write_end;
        let mut existing_ranges = Vec::new();

        if let Some((&prev_start, prev_data)) = merged_ranges.range(..=write_start).next_back() {
            let prev_end = prev_start
                .checked_add(prev_data.len() as u64)
                .ok_or_else(|| Error::Internal("file write offset overflow".to_string()))?;

            if prev_end >= write_start {
                let prev_data = prev_data.clone();
                merged_ranges.remove(&prev_start);

                start = prev_start;
                end = end.max(prev_end);
                existing_ranges.push((prev_start, prev_data));
            }
        }

        loop {
            let next = merged_ranges
                .range(start..)
                .next()
                .map(|(&next_start, next_data)| (next_start, next_data.clone()));

            let Some((next_start, next_data)) = next else {
                break;
            };

            if next_start > end {
                break;
            }

            let next_end = next_start
                .checked_add(next_data.len() as u64)
                .ok_or_else(|| Error::Internal("file write offset overflow".to_string()))?;
            merged_ranges.remove(&next_start);

            end = end.max(next_end);
            existing_ranges.push((next_start, next_data));
        }

        let merged_len = usize::try_from(end - start)
            .map_err(|_| Error::Internal("file write range too large".to_string()))?;
        let mut merged = vec![0; merged_len];
        for (range_start, range_data) in existing_ranges {
            let range_offset = usize::try_from(range_start - start)
                .map_err(|_| Error::Internal("file write range too large".to_string()))?;
            merged[range_offset..range_offset + range_data.len()].copy_from_slice(&range_data);
        }

        let write_offset = usize::try_from(write_start - start)
            .map_err(|_| Error::Internal("file write range too large".to_string()))?;
        merged[write_offset..write_offset + range.data.len()].copy_from_slice(range.data);

        merged_ranges.insert(start, merged);
    }

    Ok(merged_ranges
        .into_iter()
        .map(|(offset, data)| NormalizedWriteRange { offset, data })
        .collect())
}

fn dense_after_inline_write_batch(
    current_size: u64,
    new_size: u64,
    ranges: &[NormalizedWriteRange],
) -> bool {
    let mut covered_end = current_size;

    for range in ranges {
        let range_end = range.end();
        if range_end <= covered_end {
            continue;
        }
        if range.offset > covered_end {
            return false;
        }
        covered_end = range_end;
        if covered_end >= new_size {
            return true;
        }
    }

    covered_end >= new_size
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
        let pending_max_end = match &self.write_batcher {
            Some(batcher) if self.overlay_reads && batcher.has_pending(self.ino) => {
                batcher.peek_pending_max_end(self.ino)
            }
            _ => None,
        };
        let pending_ranges = match &self.write_batcher {
            Some(batcher) if pending_max_end.is_some() => {
                batcher.peek_pending(self.ino, offset, size)
            }
            _ => Vec::new(),
        };

        let conn = self.pool.get_connection().await?;
        let metadata = self.file_storage_with_conn(&conn).await?;
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
            let mut buf = self
                .read_inode_with_conn(&conn, offset, base_window)
                .await?;
            buf.resize(read_size as usize, 0);
            buf
        } else {
            vec![0u8; read_size as usize]
        };
        drop(conn);

        for range in pending_ranges {
            if range.offset >= offset + read_size {
                continue;
            }
            let dst_off = (range.offset - offset) as usize;
            if dst_off >= result.len() {
                continue;
            }
            let end = (dst_off + range.data.len()).min(result.len());
            result[dst_off..end].copy_from_slice(&range.data[..end - dst_off]);
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
        if let Some(batcher) = &self.write_batcher {
            if self.overlay_reads {
                return batcher
                    .enqueue(
                        self.ino,
                        vec![WriteRange {
                            offset,
                            data: data.to_vec(),
                        }],
                    )
                    .await;
            }
        }
        // Fallback (no batcher): direct commit. drain_writes is a no-op
        // when there's no batcher, but keeping the call here makes the
        // contract explicit.
        self.drain_writes().await?;
        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let ranges = [WriteRangeRef { offset, data }];
        let result = self
            .pwrite_ranges_inode_with_conn(&conn, &ranges, false, None)
            .await;
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
        if let Some(batcher) = &self.write_batcher {
            if self.overlay_reads {
                return batcher.enqueue(self.ino, ranges).await;
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
        let result = self
            .pwrite_ranges_inode_with_conn(&conn, &range_refs, false, None)
            .await;
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

        if let Some(batcher) = &self.write_batcher {
            batcher.enqueue(self.ino, ranges).await
        } else {
            self.pwrite_ranges(ranges).await
        }
    }

    async fn truncate(&self, new_size: u64) -> Result<()> {
        // Tier Four: shrink the in-memory overlay BEFORE touching SQLite, so
        // a concurrent reader doesn't observe pending bytes past the new EOF
        // between the SQLite truncate and the batcher catching up.
        if let Some(batcher) = &self.write_batcher {
            batcher.truncate_pending(self.ino, new_size);
        }
        // Drain remaining pending so the SQLite truncate sees a consistent
        // size. With truncate_pending called above, the only pending left is
        // for offsets < new_size, which will be applied by the timer / next
        // drain trigger. We still drain here so the SQLite size after this
        // call exactly matches `new_size`.
        self.drain_writes().await?;
        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result = self.truncate_inode_with_conn(&conn, new_size).await;
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
        let mut stmt = conn
            .prepare_cached("SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((self.ino,)).await?;

        if let Some(row) = rows.next().await? {
            let stats = AgentFS::build_stats_from_row(&row)?;
            self.attr_cache.insert(stats.clone());
            Ok(stats)
        } else {
            Err(FsError::NotFound.into())
        }
    }

    async fn drain_writes(&self) -> Result<()> {
        if let Some(batcher) = &self.write_batcher {
            batcher
                .drain_pending_batched(AgentFSWriteBatchDrainReason::Explicit, Some(self.ino))
                .await?;
        }
        Ok(())
    }
}

impl AgentFSFile {
    async fn read_inode_with_conn(
        &self,
        conn: &Connection,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>> {
        let metadata = self.file_storage_with_conn(conn).await?;

        if offset >= metadata.size || size == 0 {
            return Ok(Vec::new());
        }

        let size = std::cmp::min(size, metadata.size - offset);
        if metadata.storage_kind == STORAGE_INLINE {
            let mut result = Vec::with_capacity(size as usize);
            let inline_data = metadata.inline_data.unwrap_or_default();
            let start = offset as usize;
            let requested = size as usize;

            if start < inline_data.len() {
                let available = std::cmp::min(inline_data.len() - start, requested);
                result.extend_from_slice(&inline_data[start..start + available]);
            }

            if result.len() < requested {
                result.resize(requested, 0);
            }

            return Ok(result);
        }

        self.read_chunked_inode_with_conn(conn, offset, size).await
    }

    async fn read_chunked_inode_with_conn(
        &self,
        conn: &Connection,
        offset: u64,
        size: u64,
    ) -> Result<Vec<u8>> {
        let chunk_size = self.chunk_size as u64;
        let start_chunk = offset / chunk_size;
        let end_chunk = (offset + size).saturating_sub(1) / chunk_size;

        let mut stmt = conn
            .prepare_cached("SELECT chunk_index, data FROM fs_data WHERE ino = ? AND chunk_index >= ? AND chunk_index <= ? ORDER BY chunk_index")
            .await?;
        crate::profiling::record_chunk_read_query();
        let mut rows = stmt
            .query((self.ino, start_chunk as i64, end_chunk as i64))
            .await?;

        let mut result = Vec::with_capacity(size as usize);
        let start_offset_in_chunk = (offset % chunk_size) as usize;
        let mut next_expected_chunk = start_chunk;
        let mut chunks_read = 0u64;

        while let Some(row) = rows.next().await? {
            chunks_read += 1;
            let chunk_index = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64;

            while next_expected_chunk < chunk_index && result.len() < size as usize {
                let skip = if next_expected_chunk == start_chunk {
                    start_offset_in_chunk
                } else {
                    0
                };
                let zeros_needed =
                    std::cmp::min(chunk_size as usize - skip, size as usize - result.len());
                result.extend(std::iter::repeat_n(0u8, zeros_needed));
                next_expected_chunk += 1;
            }

            if let Ok(Value::Blob(chunk_data)) = row.get_value(1) {
                let skip = if chunk_index == start_chunk {
                    start_offset_in_chunk
                } else {
                    0
                };
                if skip >= chunk_data.len() {
                    let zeros_needed =
                        std::cmp::min(chunk_size as usize - skip, size as usize - result.len());
                    result.extend(std::iter::repeat_n(0u8, zeros_needed));
                } else {
                    let remaining = size as usize - result.len();
                    let take = std::cmp::min(chunk_data.len() - skip, remaining);
                    result.extend_from_slice(&chunk_data[skip..skip + take]);

                    let chunk_end = skip + take;
                    if chunk_end < chunk_size as usize && result.len() < size as usize {
                        let zeros_needed = std::cmp::min(
                            chunk_size as usize - chunk_end,
                            size as usize - result.len(),
                        );
                        result.extend(std::iter::repeat_n(0u8, zeros_needed));
                    }
                }
            }
            next_expected_chunk = chunk_index + 1;
        }

        if result.len() < size as usize {
            result.resize(size as usize, 0);
        }

        crate::profiling::record_chunk_read_chunks(chunks_read);
        Ok(result)
    }

    /// `preserve_times`: when true (deferred batcher commits racing an explicit
    /// chmod/chown/utimens), leave mtime/ctime untouched instead of stamping
    /// the commit time — the explicitly-set attributes logically happened
    /// after these writes and must win. `explicit_times`: stashed setattr
    /// values folded into the inode UPDATE itself (see
    /// `write_commit_time_sets`).
    async fn pwrite_ranges_inode_with_conn(
        &self,
        conn: &Connection,
        ranges: &[WriteRangeRef<'_>],
        preserve_times: bool,
        explicit_times: Option<&PendingTimeChange>,
    ) -> Result<()> {
        let ranges = normalize_write_ranges(ranges)?;
        if ranges.is_empty() {
            return Ok(());
        }

        let metadata = self.file_storage_with_conn(conn).await?;
        let write_end = ranges
            .iter()
            .map(NormalizedWriteRange::end)
            .max()
            .unwrap_or(metadata.size);
        let new_size = std::cmp::max(metadata.size, write_end);

        if metadata.storage_kind == STORAGE_INLINE
            && new_size <= self.inline_threshold as u64
            && dense_after_inline_write_batch(metadata.size, new_size, &ranges)
        {
            let mut inline_data = metadata.inline_data.unwrap_or_default();
            inline_data.resize(metadata.size as usize, 0);
            inline_data.resize(new_size as usize, 0);
            for range in &ranges {
                let start = range.offset as usize;
                inline_data[start..start + range.data.len()].copy_from_slice(&range.data);
            }

            conn.execute("DELETE FROM fs_data WHERE ino = ?", (self.ino,))
                .await?;
            let mut sets = vec!["size = ?", "data_inline = ?", "storage_kind = ?"];
            let mut values: Vec<Value> = vec![
                Value::Integer(new_size as i64),
                Value::Blob(inline_data),
                Value::Integer(STORAGE_INLINE),
            ];
            let (time_sets, time_values) = write_commit_time_sets(preserve_times, explicit_times)?;
            sets.extend(time_sets);
            values.extend(time_values);
            values.push(Value::Integer(self.ino));
            let sql = format!("UPDATE fs_inode SET {} WHERE ino = ?", sets.join(", "));
            conn.execute(&sql, values).await?;
            return Ok(());
        }

        let mut chunked_ranges = Vec::new();
        if metadata.storage_kind == STORAGE_INLINE {
            let mut inline_data = metadata.inline_data.unwrap_or_default();
            inline_data.resize(metadata.size as usize, 0);
            conn.execute("DELETE FROM fs_data WHERE ino = ?", (self.ino,))
                .await?;
            if !inline_data.is_empty() {
                chunked_ranges.push(NormalizedWriteRange {
                    offset: 0,
                    data: inline_data,
                });
            }
        } else {
            conn.execute(
                "UPDATE fs_inode SET data_inline = NULL, storage_kind = ? WHERE ino = ?",
                (STORAGE_CHUNKED, self.ino),
            )
            .await?;
        }

        chunked_ranges.extend(ranges);
        self.write_ranges_chunked_with_conn(conn, &chunked_ranges)
            .await?;

        let mut sets = vec!["size = ?", "data_inline = NULL", "storage_kind = ?"];
        let mut values: Vec<Value> = vec![
            Value::Integer(new_size as i64),
            Value::Integer(STORAGE_CHUNKED),
        ];
        let (time_sets, time_values) = write_commit_time_sets(preserve_times, explicit_times)?;
        sets.extend(time_sets);
        values.extend(time_values);
        values.push(Value::Integer(self.ino));
        let sql = format!("UPDATE fs_inode SET {} WHERE ino = ?", sets.join(", "));
        conn.execute(&sql, values).await?;

        Ok(())
    }

    async fn truncate_inode_with_conn(&self, conn: &Connection, new_size: u64) -> Result<()> {
        let metadata = self.file_storage_with_conn(conn).await?;

        if metadata.storage_kind == STORAGE_INLINE {
            if new_size <= self.inline_threshold as u64 {
                let mut inline_data = metadata.inline_data.unwrap_or_default();
                inline_data.resize(metadata.size as usize, 0);
                inline_data.resize(new_size as usize, 0);
                conn.execute("DELETE FROM fs_data WHERE ino = ?", (self.ino,))
                    .await?;
                let (now_secs, now_nsec) = current_timestamp()?;
                conn.execute(
                    "UPDATE fs_inode SET size = ?, data_inline = ?, storage_kind = ?, mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?",
                    (
                        new_size as i64,
                        Value::Blob(inline_data),
                        STORAGE_INLINE,
                        now_secs,
                        now_secs,
                        now_nsec,
                        now_nsec,
                        self.ino,
                    ),
                )
                .await?;
                return Ok(());
            }

            let mut inline_data = metadata.inline_data.unwrap_or_default();
            inline_data.resize(metadata.size as usize, 0);
            self.transition_inline_to_chunked_with_conn(conn, &inline_data)
                .await?;
            self.truncate_chunked_data_with_conn(conn, metadata.size, new_size)
                .await?;
            self.update_chunked_truncate_metadata(conn, new_size)
                .await?;
            return Ok(());
        }

        if new_size <= self.inline_threshold as u64 {
            if let Some(inline_data) = self.read_dense_prefix_for_inline(conn, new_size).await? {
                conn.execute("DELETE FROM fs_data WHERE ino = ?", (self.ino,))
                    .await?;
                let (now_secs, now_nsec) = current_timestamp()?;
                conn.execute(
                    "UPDATE fs_inode SET size = ?, data_inline = ?, storage_kind = ?, mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?",
                    (
                        new_size as i64,
                        Value::Blob(inline_data),
                        STORAGE_INLINE,
                        now_secs,
                        now_secs,
                        now_nsec,
                        now_nsec,
                        self.ino,
                    ),
                )
                .await?;
                return Ok(());
            }
        }

        self.truncate_chunked_data_with_conn(conn, metadata.size, new_size)
            .await?;
        self.update_chunked_truncate_metadata(conn, new_size)
            .await?;
        Ok(())
    }

    async fn file_storage_with_conn(&self, conn: &Connection) -> Result<FileStorage> {
        let mut stmt = conn
            .prepare_cached("SELECT size, storage_kind, data_inline FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((self.ino,)).await?;

        if let Some(row) = rows.next().await? {
            let size = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64;
            let storage_kind = row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(STORAGE_CHUNKED);
            let inline_data = match row.get_value(2) {
                Ok(Value::Blob(data)) => Some(data),
                _ => None,
            };
            Ok(FileStorage {
                size,
                storage_kind,
                inline_data,
            })
        } else {
            Err(FsError::NotFound.into())
        }
    }

    async fn transition_inline_to_chunked_with_conn(
        &self,
        conn: &Connection,
        inline_data: &[u8],
    ) -> Result<()> {
        conn.execute("DELETE FROM fs_data WHERE ino = ?", (self.ino,))
            .await?;

        if !inline_data.is_empty() {
            self.write_data_at_offset_with_conn(conn, 0, inline_data)
                .await?;
        }

        conn.execute(
            "UPDATE fs_inode SET data_inline = NULL, storage_kind = ? WHERE ino = ?",
            (STORAGE_CHUNKED, self.ino),
        )
        .await?;

        Ok(())
    }

    async fn read_dense_prefix_for_inline(
        &self,
        conn: &Connection,
        new_size: u64,
    ) -> Result<Option<Vec<u8>>> {
        if new_size == 0 {
            return Ok(Some(Vec::new()));
        }

        let chunk_size = self.chunk_size as u64;
        let last_chunk = (new_size - 1) / chunk_size;
        let mut inline_data = Vec::with_capacity(new_size as usize);

        let mut stmt = conn
            .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
            .await?;
        for chunk_idx in 0..=last_chunk {
            stmt.reset()?;
            let mut rows = stmt.query((self.ino, chunk_idx as i64)).await?;
            let Some(row) = rows.next().await? else {
                return Ok(None);
            };
            let chunk_data = match row.get_value(0) {
                Ok(Value::Blob(data)) => data,
                _ => return Ok(None),
            };
            let remaining = new_size as usize - inline_data.len();
            let needed = std::cmp::min(self.chunk_size, remaining);
            if chunk_data.len() < needed {
                return Ok(None);
            }
            inline_data.extend_from_slice(&chunk_data[..needed]);
        }

        Ok(Some(inline_data))
    }

    async fn truncate_chunked_data_with_conn(
        &self,
        conn: &Connection,
        current_size: u64,
        new_size: u64,
    ) -> Result<()> {
        let chunk_size = self.chunk_size as u64;

        if new_size == 0 {
            conn.execute("DELETE FROM fs_data WHERE ino = ?", (self.ino,))
                .await?;
        } else if new_size < current_size {
            let last_chunk_idx = (new_size - 1) / chunk_size;

            conn.execute(
                "DELETE FROM fs_data WHERE ino = ? AND chunk_index > ?",
                (self.ino, last_chunk_idx as i64),
            )
            .await?;

            let end_in_last_chunk = ((new_size - 1) % chunk_size + 1) as usize;
            if end_in_last_chunk < chunk_size as usize {
                let mut stmt = conn
                    .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
                    .await?;
                let mut rows = stmt.query((self.ino, last_chunk_idx as i64)).await?;

                if let Some(row) = rows.next().await? {
                    if let Ok(Value::Blob(chunk_data)) = row.get_value(0) {
                        if chunk_data.len() > end_in_last_chunk {
                            conn.execute(
                                "UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?",
                                (
                                    &chunk_data[..end_in_last_chunk],
                                    self.ino,
                                    last_chunk_idx as i64,
                                ),
                            )
                            .await?;
                        }
                    }
                }
            }
        } else if new_size > current_size {
            let last_existing_chunk = if current_size == 0 {
                None
            } else {
                Some((current_size - 1) / chunk_size)
            };
            let last_new_chunk = (new_size - 1) / chunk_size;

            if let Some(last_idx) = last_existing_chunk {
                let mut stmt = conn
                    .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
                    .await?;
                let mut rows = stmt.query((self.ino, last_idx as i64)).await?;

                if let Some(row) = rows.next().await? {
                    if let Ok(Value::Blob(chunk_data)) = row.get_value(0) {
                        let current_chunk_len = chunk_data.len();
                        let needed_len = if last_idx == last_new_chunk {
                            ((new_size - 1) % chunk_size + 1) as usize
                        } else {
                            chunk_size as usize
                        };

                        if needed_len > current_chunk_len {
                            let mut padded = chunk_data.clone();
                            padded.resize(needed_len, 0);
                            conn.execute(
                                "UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?",
                                (&padded[..], self.ino, last_idx as i64),
                            )
                            .await?;
                        }
                    }
                }
            }

            let start_new_chunk = last_existing_chunk.map(|i| i + 1).unwrap_or(0);
            for chunk_idx in start_new_chunk..=last_new_chunk {
                let chunk_len = if chunk_idx == last_new_chunk {
                    ((new_size - 1) % chunk_size + 1) as usize
                } else {
                    chunk_size as usize
                };
                let zeros = vec![0u8; chunk_len];
                conn.execute(
                    "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
                    (self.ino, chunk_idx as i64, &zeros[..]),
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn update_chunked_truncate_metadata(
        &self,
        conn: &Connection,
        new_size: u64,
    ) -> Result<()> {
        let (now_secs, now_nsec) = current_timestamp()?;
        conn.execute(
            "UPDATE fs_inode SET size = ?, data_inline = NULL, storage_kind = ?, mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?",
            (
                new_size as i64,
                STORAGE_CHUNKED,
                now_secs,
                now_secs,
                now_nsec,
                now_nsec,
                self.ino,
            ),
        )
        .await?;
        Ok(())
    }

    /// Write data at a specific offset, handling chunk boundaries.
    /// Uses a provided connection to allow reuse within a transaction.
    async fn write_data_at_offset_with_conn(
        &self,
        conn: &Connection,
        offset: u64,
        data: &[u8],
    ) -> Result<()> {
        let ranges = [WriteRangeRef { offset, data }];
        let ranges = normalize_write_ranges(&ranges)?;
        self.write_ranges_chunked_with_conn(conn, &ranges).await
    }

    async fn write_ranges_chunked_with_conn(
        &self,
        conn: &Connection,
        ranges: &[NormalizedWriteRange],
    ) -> Result<()> {
        let chunk_size = self.chunk_size as u64;

        if ranges.is_empty() {
            return Ok(());
        }

        let mut select_stmt = conn
            .prepare_cached("SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?")
            .await?;
        let mut insert_stmt = conn
            .prepare_cached(
                "INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
            )
            .await?;

        let mut chunks: BTreeMap<i64, Vec<u8>> = BTreeMap::new();

        for range in ranges {
            let mut written = 0usize;
            while written < range.data.len() {
                let current_offset = range.offset + written as u64;
                let chunk_index = (current_offset / chunk_size) as i64;
                let offset_in_chunk = (current_offset % chunk_size) as usize;

                let remaining_in_chunk = self.chunk_size - offset_in_chunk;
                let remaining_data = range.data.len() - written;
                let to_write = std::cmp::min(remaining_in_chunk, remaining_data);
                let write_slice = &range.data[written..written + to_write];

                if offset_in_chunk == 0 && to_write == self.chunk_size {
                    chunks.insert(chunk_index, write_slice.to_vec());
                    written += to_write;
                    continue;
                }

                if let std::collections::btree_map::Entry::Vacant(entry) = chunks.entry(chunk_index)
                {
                    let mut rows = select_stmt.query((self.ino, chunk_index)).await?;
                    let chunk_data = if let Some(row) = rows.next().await? {
                        row.get_value(0)
                            .ok()
                            .and_then(|v| {
                                if let Value::Blob(b) = v {
                                    Some(b)
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    select_stmt.reset()?;
                    entry.insert(chunk_data);
                }

                let chunk_data = chunks
                    .get_mut(&chunk_index)
                    .expect("chunk must be loaded before partial write");
                if chunk_data.len() < offset_in_chunk + to_write {
                    chunk_data.resize(offset_in_chunk + to_write, 0);
                }
                chunk_data[offset_in_chunk..offset_in_chunk + to_write]
                    .copy_from_slice(write_slice);

                written += to_write;
            }
        }

        let chunks_written = chunks.len() as u64;
        // Tier Three Axis H investigation: tried a multi-row VALUES batch
        // with up to 32 rows per execute() but measured slower wall-time in
        // 5-iter runs, suggesting libSQL doesn't share the
        // prepared-statement cost reduction across different VALUES
        // arities or that the per-execute setup cost dwarfed any saved
        // round-trips on our workload sizes. Reverted to the cached
        // single-row prepared statement.
        for (chunk_index, chunk_data) in chunks {
            insert_stmt
                .execute((self.ino, chunk_index, Value::Blob(chunk_data)))
                .await?;
            insert_stmt.reset()?;
        }

        crate::profiling::record_chunk_write_chunks(chunks_written);
        Ok(())
    }
}

impl AgentFS {
    /// Create a new filesystem
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let pool = if db_path == ":memory:" {
            ConnectionPool::new_single_connection(db)
        } else {
            ConnectionPool::with_options(db, file_backed_connection_pool_options())
        };
        let db_path = if db_path == ":memory:" {
            None
        } else {
            Some(PathBuf::from(db_path))
        };
        Self::from_pool_with_path_and_config(pool, db_path, CoreConfig::from_env()).await
    }

    /// Create a filesystem from a connection pool
    pub async fn from_pool(pool: ConnectionPool) -> Result<Self> {
        Self::from_pool_with_config(pool, CoreConfig::from_env()).await
    }

    /// Create a filesystem from a connection pool and explicit core config.
    pub async fn from_pool_with_config(pool: ConnectionPool, config: CoreConfig) -> Result<Self> {
        Self::from_pool_with_path_and_config(pool, None, config).await
    }

    pub(crate) async fn from_pool_with_path_and_config(
        pool: ConnectionPool,
        db_path: Option<PathBuf>,
        mut config: CoreConfig,
    ) -> Result<Self> {
        let conn = pool.get_connection().await?;

        // Refuse legacy schemas before initialization so v0.4 databases are not
        // silently mutated into v0.5. Copy migration is handled separately.
        schema::check_schema_version(&conn).await?;

        // Initialize schema first
        Self::initialize_schema(&conn).await?;

        // Get chunk_size from config (or use default)
        let chunk_size = Self::read_chunk_size(&conn).await?;
        let inline_threshold = Self::read_inline_threshold(&conn).await?;
        config.geometry = Geometry {
            chunk_size,
            inline_threshold,
        };
        let core_config = Arc::new(config);

        let attr_cache = Arc::new(AttrCache::new(ATTR_CACHE_MAX_SIZE));
        // Tier Three Axis D: default the write batcher to ON. CLI callers pass
        // the FUSE writeback decision through AgentFSOptions, while SDK callers
        // can supply CoreConfig directly.
        let write_batcher = if core_config.batcher.enabled {
            Some(Arc::new(AgentFSWriteBatcher::from_config(
                pool.clone(),
                chunk_size,
                inline_threshold,
                attr_cache.clone(),
                &core_config.batcher,
            )))
        } else {
            None
        };

        // Sweep POSIX orphans a crash stranded: nlink = 0 rows are files that
        // were unlinked while open (reap deferred) and never reaped. They are
        // invisible (no dentry), so deleting them before serving is safe.
        conn.execute(
            "DELETE FROM fs_data WHERE ino IN (SELECT ino FROM fs_inode WHERE nlink = 0)",
            (),
        )
        .await?;
        conn.execute(
            "DELETE FROM fs_symlink WHERE ino IN (SELECT ino FROM fs_inode WHERE nlink = 0)",
            (),
        )
        .await?;
        conn.execute("DELETE FROM fs_inode WHERE nlink = 0", ())
            .await?;

        let overlay_reads = core_config.overlay_reads;
        let fs = Self {
            pool,
            db_path: db_path.map(Arc::new),
            chunk_size,
            inline_threshold,
            dentry_cache: Arc::new(DentryCache::new(DENTRY_CACHE_MAX_SIZE)),
            negative_dentry_cache: Arc::new(NegativeDentryCache::new(
                NEGATIVE_DENTRY_CACHE_MAX_SIZE,
            )),
            attr_cache,
            write_batcher,
            overlay_reads,
            core_config,
            open_inodes: Arc::new(OpenInodes::default()),
        };
        Ok(fs)
    }

    /// Get the configured chunk size
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Get the configured inline threshold.
    pub fn inline_threshold(&self) -> usize {
        self.inline_threshold
    }

    pub fn core_config(&self) -> &CoreConfig {
        self.core_config.as_ref()
    }

    pub fn partial_origin_policy(&self) -> crate::filesystem::PartialOriginPolicy {
        self.core_config.partial_origin
    }

    /// Get a database connection from the pool
    pub async fn get_connection(&self) -> Result<crate::connection_pool::PooledConnection> {
        self.pool.get_connection().await
    }

    /// Get the connection pool
    pub fn get_pool(&self) -> ConnectionPool {
        self.pool.clone()
    }

    /// Initialize the database schema
    pub async fn initialize_schema(conn: &Connection) -> Result<()> {
        // Create config table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await?;

        // Create inode table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_inode (
                ino INTEGER PRIMARY KEY AUTOINCREMENT,
                mode INTEGER NOT NULL,
                nlink INTEGER NOT NULL DEFAULT 0,
                uid INTEGER NOT NULL DEFAULT 0,
                gid INTEGER NOT NULL DEFAULT 0,
                size INTEGER NOT NULL DEFAULT 0,
                atime INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                ctime INTEGER NOT NULL,
                rdev INTEGER NOT NULL DEFAULT 0,
                data_inline BLOB,
                storage_kind INTEGER NOT NULL DEFAULT 0
            )",
            (),
        )
        .await?;

        // Add columns idempotently for already-upgraded databases. Only the
        // duplicate-column case is safe to ignore; every other ALTER failure
        // indicates schema corruption or an environmental database error.
        add_column_idempotent(
            conn,
            ColumnSpec {
                table_name: "fs_inode",
                column_name: "atime_nsec",
                type_name: "INTEGER",
                not_null: true,
                default_value: Some("0"),
            },
            "ALTER TABLE fs_inode ADD COLUMN atime_nsec INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        add_column_idempotent(
            conn,
            ColumnSpec {
                table_name: "fs_inode",
                column_name: "mtime_nsec",
                type_name: "INTEGER",
                not_null: true,
                default_value: Some("0"),
            },
            "ALTER TABLE fs_inode ADD COLUMN mtime_nsec INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        add_column_idempotent(
            conn,
            ColumnSpec {
                table_name: "fs_inode",
                column_name: "ctime_nsec",
                type_name: "INTEGER",
                not_null: true,
                default_value: Some("0"),
            },
            "ALTER TABLE fs_inode ADD COLUMN ctime_nsec INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        add_column_idempotent(
            conn,
            ColumnSpec {
                table_name: "fs_inode",
                column_name: "data_inline",
                type_name: "BLOB",
                not_null: false,
                default_value: None,
            },
            "ALTER TABLE fs_inode ADD COLUMN data_inline BLOB",
        )
        .await?;
        add_column_idempotent(
            conn,
            ColumnSpec {
                table_name: "fs_inode",
                column_name: "storage_kind",
                type_name: "INTEGER",
                not_null: true,
                default_value: Some("0"),
            },
            "ALTER TABLE fs_inode ADD COLUMN storage_kind INTEGER NOT NULL DEFAULT 0",
        )
        .await?;

        // Create directory entry table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_dentry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                parent_ino INTEGER NOT NULL,
                ino INTEGER NOT NULL,
                UNIQUE(parent_ino, name)
            )",
            (),
        )
        .await?;

        // Create index for efficient path lookups
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_fs_dentry_parent
            ON fs_dentry(parent_ino, name)",
            (),
        )
        .await?;

        // Create data chunks table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_data (
                ino INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                data BLOB NOT NULL,
                PRIMARY KEY (ino, chunk_index)
            )",
            (),
        )
        .await?;

        // Create symlink table
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_symlink (
                ino INTEGER PRIMARY KEY,
                target TEXT NOT NULL
            )",
            (),
        )
        .await?;

        // Ensure chunk_size config exists
        let mut rows = conn
            .query("SELECT value FROM fs_config WHERE key = 'chunk_size'", ())
            .await?;

        if rows.next().await?.is_none() {
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES ('chunk_size', ?)",
                (DEFAULT_CHUNK_SIZE.to_string(),),
            )
            .await?;
        }

        // Ensure inline_threshold config exists
        let mut rows = conn
            .query(
                "SELECT value FROM fs_config WHERE key = 'inline_threshold'",
                (),
            )
            .await?;

        if rows.next().await?.is_none() {
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES ('inline_threshold', ?)",
                (DEFAULT_INLINE_THRESHOLD.to_string(),),
            )
            .await?;
        }

        // Set schema version
        conn.execute(
            "INSERT OR REPLACE INTO fs_config (key, value) VALUES ('schema_version', ?)",
            [AGENTFS_SCHEMA_VERSION],
        )
        .await?;

        // Ensure root directory exists with correct ownership
        let mut rows = conn
            .query("SELECT ino FROM fs_inode WHERE ino = ?", (ROOT_INO,))
            .await?;

        // SAFETY: getuid/getgid are always safe
        #[cfg(unix)]
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        #[cfg(not(unix))]
        let (uid, gid) = (0u32, 0u32);

        if rows.next().await?.is_none() {
            let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
            let now_secs = dur.as_secs() as i64;
            let now_nsec = dur.subsec_nanos() as i64;
            conn.execute(
                "INSERT INTO fs_inode (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, atime_nsec, mtime_nsec, ctime_nsec)
                VALUES (?, ?, 2, ?, ?, 0, ?, ?, ?, ?, ?, ?)",
                (ROOT_INO, DEFAULT_DIR_MODE as i64, uid, gid, now_secs, now_secs, now_secs, now_nsec, now_nsec, now_nsec),
            )
            .await?;
        } else {
            // Update existing root inode ownership to current user
            conn.execute(
                "UPDATE fs_inode SET uid = ?, gid = ? WHERE ino = ?",
                (uid, gid, ROOT_INO),
            )
            .await?;
        }

        Ok(())
    }

    /// Read chunk size from config
    async fn read_chunk_size(conn: &Connection) -> Result<usize> {
        let mut rows = conn
            .query("SELECT value FROM fs_config WHERE key = 'chunk_size'", ())
            .await?;

        if let Some(row) = rows.next().await? {
            let value = row
                .get_value(0)
                .ok()
                .and_then(|v| match v {
                    Value::Text(s) => s.parse::<usize>().ok(),
                    Value::Integer(i) => Some(i as usize),
                    _ => None,
                })
                .unwrap_or(DEFAULT_CHUNK_SIZE);
            Ok(value)
        } else {
            Ok(DEFAULT_CHUNK_SIZE)
        }
    }

    /// Read inline threshold from config
    async fn read_inline_threshold(conn: &Connection) -> Result<usize> {
        let mut rows = conn
            .query(
                "SELECT value FROM fs_config WHERE key = 'inline_threshold'",
                (),
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let value = row
                .get_value(0)
                .ok()
                .and_then(|v| match v {
                    Value::Text(s) => s.parse::<usize>().ok(),
                    Value::Integer(i) => Some(i as usize),
                    _ => None,
                })
                .unwrap_or(DEFAULT_INLINE_THRESHOLD);
            Ok(value)
        } else {
            Ok(DEFAULT_INLINE_THRESHOLD)
        }
    }

    /// Normalize a path
    fn normalize_path(&self, path: &str) -> String {
        let normalized = path.trim_end_matches('/');
        let normalized = if normalized.is_empty() {
            "/"
        } else if normalized.starts_with('/') {
            normalized
        } else {
            return format!("/{}", normalized);
        };

        // Handle . and .. components
        let components: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
        let mut result = Vec::new();

        for component in components {
            match component {
                "." => {
                    // Current directory - skip it
                    continue;
                }
                ".." => {
                    // Parent directory - only pop if there is a component to pop (don't traverse above root)
                    if !result.is_empty() {
                        result.pop();
                    }
                }
                _ => {
                    result.push(component);
                }
            }
        }

        if result.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", result.join("/"))
        }
    }

    /// Split path into components
    fn split_path(&self, path: &str) -> Vec<String> {
        let normalized = self.normalize_path(path);
        if normalized == "/" {
            return vec![];
        }
        normalized
            .split('/')
            .filter(|p| !p.is_empty())
            .map(|s| s.to_string())
            .collect()
    }

    /// Look up a child entry by parent inode and name using a provided connection.
    ///
    /// This is more efficient than `resolve_path` when you already have the parent inode,
    /// as it avoids re-resolving all parent path components.
    async fn lookup_child(
        &self,
        conn: &Connection,
        parent_ino: i64,
        name: &str,
    ) -> Result<Option<i64>> {
        if let Some(cached_ino) = self.dentry_cache.get(parent_ino, name) {
            return Ok(Some(cached_ino));
        }
        if self.negative_dentry_cache.contains(parent_ino, name) {
            return Ok(None);
        }

        let mut stmt = conn
            .prepare_cached("SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?")
            .await?;
        let mut rows = stmt.query((parent_ino, name)).await?;

        let mut found_ino = None;
        let mut row_count = 0;

        while let Some(row) = rows.next().await? {
            found_ino = row.get_value(0).ok().and_then(|v| v.as_integer().copied());
            row_count += 1;
        }

        if row_count > 1 {
            return Err(FsError::InvalidPath.into());
        }

        if let Some(ino) = found_ino {
            self.cache_dentry(parent_ino, name, ino);
        } else {
            self.cache_negative_dentry(parent_ino, name);
        }

        Ok(found_ino)
    }

    fn cache_attr(&self, stats: Stats) {
        self.attr_cache.insert(stats);
    }

    pub(crate) fn invalidate_attr(&self, ino: i64) {
        self.attr_cache.remove(ino);
    }

    /// Drain pending batched writes for one inode.
    pub async fn drain_inode_writes(&self, ino: i64) -> Result<()> {
        if let Some(batcher) = &self.write_batcher {
            batcher
                .drain_pending_batched(AgentFSWriteBatchDrainReason::Explicit, Some(ino))
                .await?;
        }
        Ok(())
    }

    /// Prelude shared by chmod / chown / utimens.
    ///
    /// Legacy drain-on-setattr behaviour synchronously commits the inode's
    /// pending batched writes so the deferred data commit can never re-stamp
    /// mtime/ctime after the explicit attribute change. With FUSE writeback
    /// caching the kernel issues one SETATTR per written file, so that drain
    /// serialised a SQLite commit per file on the clone path.
    ///
    /// Default: skip the drain and instead mark the pending entry so the
    /// eventual batched commit preserves mtime/ctime (`mark_times_explicit` /
    /// `preserve_times`). The mark happens BEFORE the caller's fs_inode
    /// UPDATE; combined with the commit path re-reading the flag after it
    /// holds the SQLite write lock, the explicitly-set attributes win in every
    /// interleaving.
    ///
    /// The deferral requires Tier-4 overlay reads: with
    /// overlay reads disabled, getattr/size are served straight from
    /// SQLite with no pending-size merge, so the legacy drain is kept to make
    /// the just-written size visible at close time (git reads files by
    /// `st_size`).
    async fn prepare_attr_change(&self, ino: i64) -> Result<()> {
        if self.core_config.drain_on_setattr || !self.overlay_reads {
            return self.drain_inode_writes(ino).await;
        }
        if let Some(batcher) = &self.write_batcher {
            batcher.mark_times_explicit(ino);
        }
        Ok(())
    }

    /// Tier Four helper: merge the batcher's pending state into a `Stats` row
    /// read from SQLite, so callers that hold a pool connection don't need to
    /// drain (which would deadlock on single-conn pools):
    /// - `size` is OR-ed with the pending max write end (mirrors the logic in
    ///   `AgentFS::getattr` and `AgentFSFile::pread`);
    /// - explicitly-set times stashed by `utimens` (`PendingTimeChange`) are
    ///   overlaid so a deferred SETATTR is visible before its drain commits.
    ///
    /// Fast-paths when the batcher has nothing pending for this inode (Tier 4
    /// read hot path: most reads pay zero cost beyond a read-lock HashMap hit).
    fn merge_pending_view(&self, ino: i64, stats: Option<&mut Stats>) {
        let Some(stats) = stats else {
            return;
        };
        // Escape hatch: when overlay reads are disabled, callers' SQLite
        // size view is already authoritative because pwrites went straight
        // to SQLite (see AgentFSFile::pwrite) and utimens never stashes.
        // No merge needed.
        if !self.overlay_reads {
            return;
        }
        let Some(batcher) = &self.write_batcher else {
            return;
        };
        if let Some(times) = batcher.peek_pending_times(ino) {
            times.merge_into(stats);
        }
        if !batcher.has_pending(ino) {
            return;
        }
        if let Some(pending_end) = batcher.peek_pending_max_end(ino) {
            let pending_end_i64 = i64::try_from(pending_end).unwrap_or(i64::MAX);
            if pending_end_i64 > stats.size {
                stats.size = pending_end_i64;
            }
        }
    }

    /// Drain all pending batched writes for this AgentFS instance.
    pub async fn drain_all(&self) -> Result<()> {
        if let Some(batcher) = &self.write_batcher {
            batcher
                .drain_all(AgentFSWriteBatchDrainReason::Explicit)
                .await?;
        }
        let conn = self.pool.get_connection().await?;
        checkpoint_wal(&conn).await?;
        Ok(())
    }

    /// Drain all writes and leave the database in single-file journal mode for clean shutdown.
    pub async fn finalize(&self) -> Result<()> {
        self.process_deferred_reaps().await?;
        self.drain_all().await?;
        if let Some(path) = &self.db_path {
            remove_checkpointed_sidecars(path.as_ref())?;
        }
        Ok(())
    }

    /// Reap inodes whose deletion unlink/rename deferred because open
    /// handles existed (POSIX unlink-while-open). Runs opportunistically at
    /// namespace mutations and at finalize; a crash is covered by the
    /// nlink=0 sweep at mount.
    pub async fn process_deferred_reaps(&self) -> Result<()> {
        if !self.open_inodes.has_pending_reaps() {
            return Ok(());
        }
        let inos = self.open_inodes.take_reap_queue();
        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let result: Result<()> = async {
            for ino in &inos {
                // The nlink=0 guard makes a stale queue entry (row already
                // reaped, or the rowid reused by a live file) a no-op.
                let changed = conn
                    .execute("DELETE FROM fs_inode WHERE ino = ? AND nlink = 0", (*ino,))
                    .await?;
                if changed == 0 {
                    continue;
                }
                if let Some(batcher) = &self.write_batcher {
                    batcher.discard_pending(*ino);
                }
                conn.execute("DELETE FROM fs_data WHERE ino = ?", (*ino,))
                    .await?;
                conn.execute("DELETE FROM fs_symlink WHERE ino = ?", (*ino,))
                    .await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                txn.commit().await?;
                for ino in &inos {
                    self.invalidate_attr(*ino);
                }
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                self.open_inodes.requeue_reaps(inos);
                Err(error)
            }
        }
    }

    fn invalidate_parent_attr(&self, parent_ino: i64) {
        self.invalidate_attr(parent_ino);
    }

    fn invalidate_dentry(&self, parent_ino: i64, name: &str) {
        self.dentry_cache.remove(parent_ino, name);
        self.negative_dentry_cache.remove(parent_ino, name);
    }

    fn cache_dentry(&self, parent_ino: i64, name: &str, child_ino: i64) {
        self.negative_dentry_cache.remove(parent_ino, name);
        self.dentry_cache.insert(parent_ino, name, child_ino);
    }

    fn cache_negative_dentry(&self, parent_ino: i64, name: &str) {
        self.dentry_cache.remove(parent_ino, name);
        self.negative_dentry_cache.insert(parent_ino, name);
    }

    /// Get link count for an inode
    async fn get_link_count(&self, conn: &Connection, ino: i64) -> Result<u32> {
        let mut stmt = conn
            .prepare_cached("SELECT nlink FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if let Some(row) = rows.next().await? {
            let nlink = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            Ok(nlink as u32)
        } else {
            Ok(0)
        }
    }

    /// Get file attributes by inode using an existing connection
    async fn getattr_with_conn(&self, conn: &Connection, ino: i64) -> Result<Option<Stats>> {
        if let Some(stats) = self.attr_cache.get(ino) {
            return Ok(Some(stats));
        }

        let mut stmt = conn
            .prepare_cached("SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if let Some(row) = rows.next().await? {
            let stats = Self::build_stats_from_row(&row)?;
            self.cache_attr(stats.clone());
            Ok(Some(stats))
        } else {
            Ok(None)
        }
    }

    /// Build a Stats object from a database row
    ///
    /// The row should contain columns in this order:
    /// ino, mode, nlink, uid, gid, size, atime, mtime, ctime
    fn build_stats_from_row(row: &turso::Row) -> Result<Stats> {
        Ok(Stats {
            ino: row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            mode: row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            nlink: row
                .get_value(2)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(1) as u32,
            uid: row
                .get_value(3)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            gid: row
                .get_value(4)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            size: row
                .get_value(5)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            atime: row
                .get_value(6)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            mtime: row
                .get_value(7)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            ctime: row
                .get_value(8)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0),
            atime_nsec: row
                .get_value(10)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            mtime_nsec: row
                .get_value(11)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            ctime_nsec: row
                .get_value(12)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32,
            rdev: row
                .get_value(9)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64,
        })
    }

    /// Resolve a path to an inode number
    async fn resolve_path(&self, path: &str) -> Result<Option<i64>> {
        let conn = self.pool.get_connection().await?;
        self.resolve_path_with_conn(&conn, path).await
    }

    /// Resolve a path to an inode number using a provided connection
    async fn resolve_path_with_conn(&self, conn: &Connection, path: &str) -> Result<Option<i64>> {
        let components = self.split_path(path);
        crate::profiling::record_path_resolution(components.len() as u64);
        if components.is_empty() {
            return Ok(Some(ROOT_INO));
        }

        let mut statement: Option<turso::Statement> = None;
        let mut current_ino = ROOT_INO;
        for component in components {
            // Check cache first
            if let Some(cached_ino) = self.dentry_cache.get(current_ino, &component) {
                current_ino = cached_ino;
                continue;
            }
            if self.negative_dentry_cache.contains(current_ino, &component) {
                crate::profiling::record_negative_lookup();
                return Ok(None);
            }

            // Cache miss - query database
            if let Some(statement) = &mut statement {
                statement.reset()?;
            } else {
                statement = Some(
                    conn.prepare_cached(
                        "SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?",
                    )
                    .await?,
                );
            }
            let statement = statement.as_mut().expect("statement was set above");
            let mut rows = statement.query((current_ino, component.as_str())).await?;

            let mut found_row = None;
            let mut row_count = 0;

            while let Some(row) = rows.next().await? {
                found_row = Some(row);
                row_count += 1;
            }

            if row_count > 1 {
                return Err(FsError::InvalidPath.into());
            }

            if let Some(row) = found_row {
                let child_ino = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);

                // Populate cache
                self.cache_dentry(current_ino, &component, child_ino);
                current_ino = child_ino;
            } else {
                crate::profiling::record_negative_lookup();
                self.cache_negative_dentry(current_ino, &component);
                return Ok(None);
            }
        }

        Ok(Some(current_ino))
    }

    async fn resolve_parent_and_name(&self, path: &str) -> Result<(i64, String)> {
        let path = self.normalize_path(path);
        let components = self.split_path(&path);
        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let parent_path = match components.len() {
            1 => "/".to_string(),
            _ => format!("/{}", components[..components.len() - 1].join("/")),
        };
        let parent_ino = self
            .resolve_path(&parent_path)
            .await?
            .ok_or(FsError::NotFound)?;
        let name = components.last().cloned().ok_or(FsError::InvalidPath)?;
        Ok((parent_ino, name))
    }

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

    /// List directory contents
    pub async fn readdir(&self, ino: i64) -> Result<Option<Vec<String>>> {
        let conn = self.pool.get_connection().await?;
        let mut rows = conn
            .query(
                "SELECT name FROM fs_dentry WHERE parent_ino = ? ORDER BY name",
                (ino,),
            )
            .await?;

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

    /// Read the target of a symbolic link
    pub async fn readlink(&self, path: &str) -> Result<Option<String>> {
        let conn = self.pool.get_connection().await?;
        self.readlink_with_conn(&conn, path).await
    }

    /// Read the target of a symbolic link using a provided connection
    async fn readlink_with_conn(&self, conn: &Connection, path: &str) -> Result<Option<String>> {
        let path = self.normalize_path(path);

        let ino = match self.resolve_path_with_conn(conn, &path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        // Check if it's a symlink by querying the inode
        let mut rows = conn
            .query("SELECT mode FROM fs_inode WHERE ino = ?", (ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

            // Check if it's a symlink
            if (mode & S_IFMT) != S_IFLNK {
                return Err(FsError::NotASymlink.into());
            }
        } else {
            return Ok(None);
        }

        // Read target from fs_symlink table
        let mut rows = conn
            .query("SELECT target FROM fs_symlink WHERE ino = ?", (ino,))
            .await?;

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

    /// Get filesystem statistics
    ///
    /// Returns the total number of inodes and bytes used by file contents.
    pub async fn statfs(&self) -> Result<FilesystemStats> {
        self.drain_all().await?;
        let conn = self.pool.get_connection().await?;
        // Count total inodes
        let mut stmt = conn.prepare_cached("SELECT COUNT(*) FROM fs_inode").await?;
        let mut rows = stmt.query(()).await?;

        let inodes = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64
        } else {
            0
        };

        // Sum total bytes used (from file sizes in inodes)
        let mut stmt = conn
            .prepare_cached("SELECT COALESCE(SUM(size), 0) FROM fs_inode")
            .await?;
        let mut rows = stmt.query(()).await?;

        let bytes_used = if let Some(row) = rows.next().await? {
            row.get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64
        } else {
            0
        };

        Ok(FilesystemStats { inodes, bytes_used })
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

    /// Open a file and return a file handle.
    ///
    /// The returned handle can be used for efficient read/write/fsync operations
    /// without requiring path lookups on each operation.
    pub async fn open(&self, path: &str) -> Result<BoxedFile> {
        let path = self.normalize_path(path);
        let ino = self.resolve_path(&path).await?.ok_or(FsError::NotFound)?;

        Ok(Arc::new(AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
            inline_threshold: self.inline_threshold,
            attr_cache: self.attr_cache.clone(),
            write_batcher: self.write_batcher.clone(),
            overlay_reads: self.overlay_reads,
            _open_guard: Some(self.open_inodes.guard(ino)),
        }))
    }

    /// Get the number of chunks for a given inode (for testing).
    /// Drains any pending batched writes first so the returned count reflects
    /// the full committed state — Tier 4 deferred SQLite commits until fsync
    /// or timer, so tests that inspect `fs_data` directly need a sync point.
    #[cfg(test)]
    async fn get_chunk_count(&self, ino: i64) -> Result<i64> {
        self.drain_inode_writes(ino).await?;
        let conn = self.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM fs_data WHERE ino = ?", (ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            Ok(row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0))
        } else {
            Ok(0)
        }
    }

    #[cfg(test)]
    async fn get_storage_state(&self, ino: i64) -> Result<(i64, Option<Vec<u8>>)> {
        self.drain_inode_writes(ino).await?;
        let conn = self.pool.get_connection().await?;
        let mut rows = conn
            .query(
                "SELECT storage_kind, data_inline FROM fs_inode WHERE ino = ?",
                (ino,),
            )
            .await?;

        if let Some(row) = rows.next().await? {
            let storage_kind = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(STORAGE_CHUNKED);
            let data_inline = match row.get_value(1) {
                Ok(Value::Blob(data)) => Some(data),
                _ => None,
            };
            Ok((storage_kind, data_inline))
        } else {
            Err(FsError::NotFound.into())
        }
    }
}

#[async_trait]
impl FileSystem for AgentFS {
    async fn lookup(&self, parent_ino: i64, name: &str) -> Result<Option<Stats>> {
        crate::profiling::record_lookup();
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
                crate::profiling::record_negative_lookup();
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
                crate::profiling::record_negative_lookup();
                return Ok(None);
            }
        };
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
            let mut stats = Self::build_stats_from_row(&row)?;
            self.merge_pending_view(child_ino, Some(&mut stats));
            // Cache the lookup result
            self.cache_dentry(parent_ino, name, child_ino);
            self.cache_attr(stats.clone());
            Ok(Some(stats))
        } else {
            Ok(None)
        }
    }

    async fn getattr(&self, ino: i64) -> Result<Option<Stats>> {
        crate::profiling::record_getattr();
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
        let mut stats = self.getattr_with_conn(&conn, ino).await?;
        if let Some(s) = stats.as_mut() {
            let pre = s.size;
            self.merge_pending_view(ino, Some(s));
            if s.size != pre {
                self.cache_attr(s.clone());
            }
        }
        Ok(stats)
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
        let mut stmt = conn
            .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

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
        crate::profiling::record_readdir();
        let conn = self.pool.get_connection().await?;

        // Check if inode exists and is a directory
        let mut stmt = conn
            .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

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
        crate::profiling::record_readdir_plus();
        self.drain_all().await?;
        let conn = self.pool.get_connection().await?;

        // Check if inode exists and is a directory
        let mut stmt = conn
            .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((ino,)).await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

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

            let entry_ino = row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);

            let stats = Stats {
                ino: entry_ino,
                mode: row
                    .get_value(2)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                nlink: row
                    .get_value(3)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(1) as u32,
                uid: row
                    .get_value(4)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                gid: row
                    .get_value(5)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                size: row
                    .get_value(6)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0),
                atime: row
                    .get_value(7)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0),
                mtime: row
                    .get_value(8)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0),
                ctime: row
                    .get_value(9)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0),
                atime_nsec: row
                    .get_value(11)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                mtime_nsec: row
                    .get_value(12)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                ctime_nsec: row
                    .get_value(13)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                rdev: row
                    .get_value(10)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u64,
            };

            self.cache_attr(stats.clone());
            entries.push(DirEntry { name, stats });
        }

        Ok(Some(entries))
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
            let mut stmt = conn
                .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
                .await?;
            let mut rows = stmt.query((ino,)).await?;

            let current_mode = if let Some(row) = rows.next().await? {
                row.get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32
            } else {
                return Err(FsError::NotFound.into());
            };

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
            if let Some(batcher) = &self.write_batcher {
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
                batcher.stash_pending_times(ino, change);
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
            write_batcher: self.write_batcher.clone(),
            overlay_reads: self.overlay_reads,
            _open_guard: Some(self.open_inodes.guard(ino)),
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
        let stash_parent_times = self.overlay_reads && self.write_batcher.is_some();
        if !stash_parent_times {
            conn.execute(
                "UPDATE fs_inode SET ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?",
                (now_secs, now_secs, now_nsec, now_nsec, parent_ino),
            )
            .await?;
        }

        txn.commit().await?;

        if stash_parent_times {
            if let Some(batcher) = &self.write_batcher {
                batcher.stash_pending_times(
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
            write_batcher: self.write_batcher.clone(),
            overlay_reads: self.overlay_reads,
            _open_guard: Some(self.open_inodes.guard(ino)),
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
        let result: Result<(i64, bool)> = async {
            // Look up the child inode
            let ino = self
                .lookup_child(&conn, parent_ino, name)
                .await?
                .ok_or(FsError::NotFound)?;

            // Check if it's a directory (use rmdir for directories)
            let mut stmt = conn
                .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
                .await?;
            let mut rows = stmt.query((ino,)).await?;

            if let Some(row) = rows.next().await? {
                let mode = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;

                if (mode & S_IFMT) == super::S_IFDIR {
                    return Err(FsError::IsADirectory.into());
                }
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
            let removed = link_count == 0 && !self.open_inodes.defer_reap_if_open(ino);
            if removed {
                // Delete data blocks
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_data WHERE ino = ?")
                    .await?;
                stmt.execute((ino,)).await?;

                // Delete symlink if exists
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_symlink WHERE ino = ?")
                    .await?;
                stmt.execute((ino,)).await?;

                // Delete inode
                let mut stmt = conn
                    .prepare_cached("DELETE FROM fs_inode WHERE ino = ?")
                    .await?;
                stmt.execute((ino,)).await?;
            }

            Ok((ino, removed))
        }
        .await;

        match result {
            Ok((ino, removed)) => {
                txn.commit().await?;
                if removed {
                    // Tier Four: discard any pending writes the batcher might
                    // still hold for this inode. The drains tolerate a deleted
                    // inode (NotFound is skipped, never inserted as orphan
                    // `fs_data` rows), so dropping the moot ranges after the
                    // commit keeps the overlay clean without risking data loss
                    // on a rolled-back unlink.
                    if let Some(batcher) = &self.write_batcher {
                        batcher.discard_pending(ino);
                    }
                }
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
            let mut stmt = conn
                .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
                .await?;
            let mut rows = stmt.query((ino,)).await?;

            if let Some(row) = rows.next().await? {
                let mode = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;

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
            let mut stmt = conn
                .prepare_cached("SELECT mode FROM fs_inode WHERE ino = ?")
                .await?;
            let mut rows = stmt.query((ino,)).await?;

            if let Some(row) = rows.next().await? {
                let mode = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;

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
                while ancestor_ino != ROOT_INO {
                    if ancestor_ino == src_ino {
                        return Err(FsError::InvalidRename.into());
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
                // open handles exist — see OpenInodes)
                let link_count = self.get_link_count(&conn, dst_ino).await?;
                if link_count == 0 && !self.open_inodes.defer_reap_if_open(dst_ino) {
                    // Tier Four: see public `rename` for rationale — drop
                    // pending batched writes for the deleted inode so a
                    // subsequent batched drain doesn't INSERT into a
                    // missing fs_inode row.
                    if let Some(batcher) = &self.write_batcher {
                        batcher.discard_pending(dst_ino);
                    }
                    let mut stmt = conn
                        .prepare_cached("DELETE FROM fs_data WHERE ino = ?")
                        .await?;
                    stmt.execute((dst_ino,)).await?;
                    let mut stmt = conn
                        .prepare_cached("DELETE FROM fs_symlink WHERE ino = ?")
                        .await?;
                    stmt.execute((dst_ino,)).await?;
                    let mut stmt = conn
                        .prepare_cached("DELETE FROM fs_inode WHERE ino = ?")
                        .await?;
                    stmt.execute((dst_ino,)).await?;
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

                Ok(())
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

    // `forget` deliberately uses the default no-op trait impl: a FORGET only
    // drops the kernel's reference to the inode. Pending batched writes stay
    // readable through the Tier-4 overlay and are committed by the batcher
    // timer/bytes triggers, fsync, or finalize — committing them here issued
    // one serial SQLite transaction per written file during clone workloads
    // (the kernel FORGETs each file shortly after our post-write entry
    // invalidation).
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Turso 0.5.x reports SQLite's standard numeric value for NORMAL.
    const TURSO_OBSERVED_SYNCHRONOUS_NORMAL: i64 = 1;

    async fn create_test_fs() -> Result<(AgentFS, tempfile::TempDir)> {
        create_test_fs_with_config(CoreConfig::from_env()).await
    }

    async fn create_test_fs_with_config(
        config: CoreConfig,
    ) -> Result<(AgentFS, tempfile::TempDir)> {
        let dir = tempdir()?;
        let db_path = dir.path().join("test.db");
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let pool = ConnectionPool::with_options(db, file_backed_connection_pool_options());
        let fs = AgentFS::from_pool_with_path_and_config(pool, Some(db_path), config).await?;
        Ok((fs, dir))
    }

    fn test_config_with_long_batch_window() -> CoreConfig {
        let mut config = CoreConfig::default();
        config.batcher.enabled = true;
        config.batcher.window = Duration::from_secs(60);
        config.batcher.inode_bytes = 1_048_576;
        config.batcher.global_bytes = 64 * 1024 * 1024;
        config
    }

    #[tokio::test]
    async fn core_config_batcher_enabled_flows_through_options() -> Result<()> {
        let dir = tempdir()?;

        let mut disabled = CoreConfig::default();
        disabled.batcher.enabled = false;
        let disabled_agent = crate::AgentFS::open(
            crate::AgentFSOptions::with_path(dir.path().join("disabled.db").to_string_lossy())
                .with_core_config(disabled),
        )
        .await?;
        assert!(
            disabled_agent.fs.write_batcher.is_none(),
            "AgentFSOptions CoreConfig should be able to disable the write batcher"
        );

        let enabled_agent = crate::AgentFS::open(
            crate::AgentFSOptions::with_path(dir.path().join("enabled.db").to_string_lossy())
                .with_core_config(test_config_with_long_batch_window()),
        )
        .await?;
        assert!(
            enabled_agent.fs.write_batcher.is_some(),
            "AgentFSOptions CoreConfig should be able to enable the write batcher"
        );

        Ok(())
    }

    async fn fs_inode_column_count(conn: &Connection, column_name: &str) -> Result<usize> {
        let mut rows = conn.query("PRAGMA table_info(fs_inode)", ()).await?;
        let mut count = 0;

        while let Some(row) = rows.next().await? {
            let name: String = row.get(1)?;
            if name == column_name {
                count += 1;
            }
        }

        Ok(count)
    }

    fn cached_attr(fs: &AgentFS, ino: i64) -> Option<Stats> {
        fs.attr_cache.get(ino)
    }

    fn negative_cached(fs: &AgentFS, parent_ino: i64, name: &str) -> bool {
        fs.negative_dentry_cache.contains(parent_ino, name)
    }

    async fn parent_and_name_for_test(fs: &AgentFS, path: &str) -> Result<(i64, String)> {
        let path = fs.normalize_path(path);
        let components = fs.split_path(&path);
        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };
        let parent_ino = fs
            .resolve_path(&parent_path)
            .await?
            .ok_or(FsError::NotFound)?;
        Ok((parent_ino, components.last().unwrap().clone()))
    }

    async fn rename_path_via_trait(fs: &AgentFS, from: &str, to: &str) -> Result<()> {
        let (oldparent_ino, oldname) = parent_and_name_for_test(fs, from).await?;
        let (newparent_ino, newname) = parent_and_name_for_test(fs, to).await?;
        FileSystem::rename(fs, oldparent_ino, &oldname, newparent_ino, &newname).await
    }

    #[tokio::test]
    async fn path_api_delegates_to_trait_semantics() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        fs.mkdir("/live", 42, 43).await?;
        let live = fs.stat("/live").await?.unwrap();
        assert!(live.is_directory());
        assert_eq!((live.uid, live.gid), (42, 43));

        let (created, file) = fs
            .create_file("/live/pending.txt", DEFAULT_FILE_MODE, 7, 9)
            .await?;
        assert_eq!((created.uid, created.gid), (7, 9));
        file.pwrite(0, b"pending bytes").await?;

        let pending = fs.stat("/live/pending.txt").await?.unwrap();
        assert_eq!(
            pending.size, 13,
            "path stat must observe pending batched writes before drain"
        );
        assert_eq!(
            fs.read_file("/live/pending.txt").await?.unwrap(),
            b"pending bytes"
        );

        let (doomed, doomed_file) = fs
            .create_file("/live/doomed.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        doomed_file
            .pwrite(0, b"discard these pending bytes")
            .await?;
        drop(doomed_file);

        fs.remove("/live/doomed.txt").await?;
        fs.drain_all().await?;
        assert!(fs.stat("/live/doomed.txt").await?.is_none());
        assert_eq!(
            count_rows(&fs, "fs_data", doomed.ino).await?,
            0,
            "path remove must discard pending writes for the deleted inode"
        );

        fs.remove("/live/pending.txt").await?;
        fs.remove("/live").await?;
        assert!(fs.stat("/live").await?.is_none());

        let missing = fs.remove("/missing.txt").await.unwrap_err();
        assert!(matches!(missing, Error::Fs(FsError::NotFound)));

        fs.fsync().await?;
        Ok(())
    }

    #[tokio::test]
    async fn import_entries_builds_tree_with_correct_content_and_stats() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let big = vec![0xabu8; DEFAULT_INLINE_THRESHOLD + DEFAULT_CHUNK_SIZE + 17];
        let entries = vec![
            ImportEntry {
                path: "sub".to_string(),
                mode: S_IFDIR | 0o755,
                data: Vec::new(),
            },
            ImportEntry {
                path: "sub/inner".to_string(),
                mode: S_IFDIR | 0o755,
                data: Vec::new(),
            },
            ImportEntry {
                path: "sub/small.txt".to_string(),
                mode: S_IFREG | 0o644,
                data: b"hello import".to_vec(),
            },
            ImportEntry {
                path: "sub/inner/big.bin".to_string(),
                mode: S_IFREG | 0o755,
                data: big.clone(),
            },
            ImportEntry {
                path: "sub/link".to_string(),
                mode: S_IFLNK | 0o777,
                data: b"small.txt".to_vec(),
            },
        ];
        let opts = ImportOptions {
            uid: 7,
            gid: 9,
            timestamp: (1_700_000_000, 123_456_789),
        };
        let imported = fs.import_entries(ROOT_INO, &entries, &opts).await?;
        assert_eq!(imported.len(), entries.len());

        assert_eq!(
            fs.read_file("/sub/small.txt").await?.unwrap(),
            b"hello import"
        );
        assert_eq!(fs.read_file("/sub/inner/big.bin").await?.unwrap(), big);
        assert_eq!(fs.readlink("/sub/link").await?.unwrap(), "small.txt");

        let small = fs.stat("/sub/small.txt").await?.unwrap();
        let reported = imported.iter().find(|e| e.path == "sub/small.txt").unwrap();
        assert_eq!(small.ino, reported.ino);
        assert_eq!(small.size as u64, reported.size);
        assert_eq!(small.mode, S_IFREG | 0o644);
        assert_eq!((small.uid, small.gid), (7, 9));
        assert_eq!(small.mtime, 1_700_000_000);
        assert_eq!(small.mtime_nsec, 123_456_789);
        assert_eq!(small.ctime, 1_700_000_000);

        let big_stat = fs.stat("/sub/inner/big.bin").await?.unwrap();
        assert_eq!(big_stat.size as usize, big.len());
        assert_eq!(big_stat.mode, S_IFREG | 0o755);

        let sub = fs.stat("/sub").await?.unwrap();
        assert_eq!(sub.nlink, 3); // "." + parent link + inner

        // Duplicate import collides on the dentry UNIQUE constraint.
        let dup = fs.import_entries(ROOT_INO, &entries[..1], &opts).await;
        assert!(matches!(dup, Err(Error::Fs(FsError::AlreadyExists))));

        Ok(())
    }

    #[tokio::test]
    async fn attr_cache_invalidates_mutations_and_preserves_visibility() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        assert!(cached_attr(&fs, ROOT_INO).is_some());

        let (created, file) =
            FileSystem::create_file(&fs, ROOT_INO, "cache.txt", DEFAULT_FILE_MODE, 7, 9).await?;
        let file_ino = created.ino;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert_eq!(cached_attr(&fs, file_ino).unwrap().size, 0);

        file.pwrite(0, b"hello").await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let written = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(written.size, 5);
        assert_eq!(cached_attr(&fs, file_ino).unwrap().size, 5);

        file.pwrite(5, b" world").await?;
        let after_append = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(after_append.size, 11);
        assert_eq!(file.pread(0, 11).await?, b"hello world");

        file.truncate(5).await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let truncated = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(truncated.size, 5);
        assert_eq!(file.pread(0, 16).await?, b"hello");

        FileSystem::chmod(&fs, file_ino, 0o600).await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let chmodded = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(chmodded.mode & 0o7777, 0o600);

        FileSystem::chown(&fs, file_ino, Some(11), Some(13)).await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let chowned = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!((chowned.uid, chowned.gid), (11, 13));

        FileSystem::utimens(
            &fs,
            file_ino,
            TimeChange::Set(1_700_000_001, 123),
            TimeChange::Set(1_700_000_002, 456),
        )
        .await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let timestamped = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(
            (timestamped.mtime, timestamped.mtime_nsec),
            (1_700_000_002, 456)
        );

        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        let linked = FileSystem::link(&fs, file_ino, ROOT_INO, "hard.txt").await?;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert_eq!(linked.nlink, 2);
        assert_eq!(
            FileSystem::lookup(&fs, ROOT_INO, "hard.txt")
                .await?
                .unwrap()
                .ino,
            file_ino
        );

        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        let symlink = FileSystem::symlink(&fs, ROOT_INO, "cache.link", "cache.txt", 11, 13).await?;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert!(symlink.is_symlink());
        assert_eq!(
            FileSystem::readlink(&fs, symlink.ino).await?,
            Some("cache.txt".to_string())
        );

        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        let dir = FileSystem::mkdir(&fs, ROOT_INO, "dir", 0o755, 11, 13).await?;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert!(cached_attr(&fs, dir.ino).is_some());
        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        FileSystem::rmdir(&fs, ROOT_INO, "dir").await?;
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert!(cached_attr(&fs, dir.ino).is_none());
        assert!(FileSystem::lookup(&fs, ROOT_INO, "dir").await?.is_none());

        FileSystem::getattr(&fs, file_ino).await?.unwrap();
        FileSystem::getattr(&fs, ROOT_INO).await?.unwrap();
        FileSystem::rename(&fs, ROOT_INO, "cache.txt", ROOT_INO, "renamed.txt").await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        assert!(cached_attr(&fs, ROOT_INO).is_none());
        assert!(FileSystem::lookup(&fs, ROOT_INO, "cache.txt")
            .await?
            .is_none());
        assert_eq!(
            FileSystem::lookup(&fs, ROOT_INO, "renamed.txt")
                .await?
                .unwrap()
                .ino,
            file_ino
        );
        assert_eq!(file.pread(0, 16).await?, b"hello");

        FileSystem::getattr(&fs, file_ino).await?.unwrap();
        FileSystem::unlink(&fs, ROOT_INO, "hard.txt").await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        let single_link = FileSystem::getattr(&fs, file_ino).await?.unwrap();
        assert_eq!(single_link.nlink, 1);

        FileSystem::unlink(&fs, ROOT_INO, "renamed.txt").await?;
        assert!(cached_attr(&fs, file_ino).is_none());
        assert!(FileSystem::lookup(&fs, ROOT_INO, "renamed.txt")
            .await?
            .is_none());

        Ok(())
    }

    #[tokio::test]
    async fn negative_dentry_cache_invalidates_on_namespace_mutations() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        assert!(FileSystem::lookup(&fs, ROOT_INO, "missing.txt")
            .await?
            .is_none());
        assert!(negative_cached(&fs, ROOT_INO, "missing.txt"));

        let (created, _file) =
            FileSystem::create_file(&fs, ROOT_INO, "missing.txt", DEFAULT_FILE_MODE, 7, 9).await?;
        assert!(!negative_cached(&fs, ROOT_INO, "missing.txt"));
        assert_eq!(
            FileSystem::lookup(&fs, ROOT_INO, "missing.txt")
                .await?
                .unwrap()
                .ino,
            created.ino
        );

        FileSystem::rename(&fs, ROOT_INO, "missing.txt", ROOT_INO, "renamed.txt").await?;
        assert!(negative_cached(&fs, ROOT_INO, "missing.txt"));
        assert!(!negative_cached(&fs, ROOT_INO, "renamed.txt"));
        assert!(FileSystem::lookup(&fs, ROOT_INO, "missing.txt")
            .await?
            .is_none());
        assert_eq!(
            FileSystem::lookup(&fs, ROOT_INO, "renamed.txt")
                .await?
                .unwrap()
                .ino,
            created.ino
        );

        FileSystem::unlink(&fs, ROOT_INO, "renamed.txt").await?;
        assert!(negative_cached(&fs, ROOT_INO, "renamed.txt"));
        assert!(FileSystem::lookup(&fs, ROOT_INO, "renamed.txt")
            .await?
            .is_none());

        assert!(FileSystem::lookup(&fs, ROOT_INO, "negdir").await?.is_none());
        assert!(negative_cached(&fs, ROOT_INO, "negdir"));
        FileSystem::mkdir(&fs, ROOT_INO, "negdir", 0o755, 7, 9).await?;
        assert!(!negative_cached(&fs, ROOT_INO, "negdir"));
        FileSystem::rmdir(&fs, ROOT_INO, "negdir").await?;
        assert!(negative_cached(&fs, ROOT_INO, "negdir"));

        Ok(())
    }

    async fn read_pragma_i64(conn: &Connection, sql: &str) -> i64 {
        let mut rows = conn.query(sql, ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        row.get_value(0)
            .ok()
            .and_then(|value| match value {
                Value::Integer(value) => Some(value),
                Value::Text(value) => value.parse().ok(),
                _ => None,
            })
            .unwrap()
    }

    async fn read_pragma_text(conn: &Connection, sql: &str) -> String {
        let mut rows = conn.query(sql, ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        row.get_value(0)
            .ok()
            .and_then(|value| match value {
                Value::Text(value) => Some(value.clone()),
                Value::Integer(value) => Some(value.to_string()),
                _ => None,
            })
            .unwrap()
    }

    // ==================== Chunk Size Boundary Tests ====================

    #[tokio::test]
    async fn test_file_smaller_than_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write a file smaller than chunk_size (100 bytes)
        let data = vec![0u8; 100];
        let (_, file) = fs
            .create_file("/small.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        // Read it back
        let read_data = fs.read_file("/small.txt").await?.unwrap();
        assert_eq!(read_data.len(), 100);
        assert_eq!(read_data, data);

        // Verify inline storage avoids chunks
        let ino = fs.resolve_path("/small.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 0);
        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(data));

        Ok(())
    }

    #[tokio::test]
    async fn test_file_exactly_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write exactly chunk_size bytes
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..chunk_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/exact.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        // Read it back
        let read_data = fs.read_file("/exact.txt").await?.unwrap();
        assert_eq!(read_data.len(), chunk_size);
        assert_eq!(read_data, data);

        // Verify only 1 chunk was created
        let ino = fs.resolve_path("/exact.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_one_byte_over_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write chunk_size + 1 bytes
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..=chunk_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/overflow.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        // Read it back
        let read_data = fs.read_file("/overflow.txt").await?.unwrap();
        assert_eq!(read_data.len(), chunk_size + 1);
        assert_eq!(read_data, data);

        // Verify 2 chunks were created
        let ino = fs.resolve_path("/overflow.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 2);

        Ok(())
    }

    #[tokio::test]
    async fn test_file_spanning_multiple_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write ~2.5 chunks worth of data
        let chunk_size = fs.chunk_size();
        let data_size = chunk_size * 2 + chunk_size / 2;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/multi.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        // Read it back
        let read_data = fs.read_file("/multi.txt").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data);

        // Verify 3 chunks were created
        let ino = fs.resolve_path("/multi.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 3);

        Ok(())
    }

    // ==================== Data Integrity Tests ====================

    #[tokio::test]
    async fn test_roundtrip_byte_for_byte() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create data that spans chunk boundaries with identifiable patterns
        let chunk_size = fs.chunk_size();
        let data_size = chunk_size * 3 + 123; // Odd size spanning 4 chunks

        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/roundtrip.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let read_data = fs.read_file("/roundtrip.bin").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data, "Data mismatch after roundtrip");

        Ok(())
    }

    #[tokio::test]
    async fn test_binary_data_with_null_bytes() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create data with null bytes at chunk boundaries
        let mut data = vec![0u8; chunk_size * 2 + 100];
        // Put nulls at the chunk boundary
        data[chunk_size - 1] = 0;
        data[chunk_size] = 0;
        data[chunk_size + 1] = 0;
        // Put some non-null bytes around
        data[chunk_size - 2] = 0xFF;
        data[chunk_size + 2] = 0xFF;

        let (_, file) = fs
            .create_file("/nulls.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;
        let read_data = fs.read_file("/nulls.bin").await?.unwrap();

        assert_eq!(read_data, data, "Null bytes at chunk boundary corrupted");

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_ordering() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create sequential bytes spanning multiple chunks
        let data_size = chunk_size * 5;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/sequential.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let read_data = fs.read_file("/sequential.bin").await?.unwrap();

        // Verify every byte is in the correct position
        for (i, (&expected, &actual)) in data.iter().zip(read_data.iter()).enumerate() {
            assert_eq!(
                expected, actual,
                "Byte mismatch at position {}: expected {}, got {}",
                i, expected, actual
            );
        }

        Ok(())
    }

    // ==================== Edge Case Tests ====================

    #[tokio::test]
    async fn test_empty_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write empty file
        let (_, file) = fs
            .create_file("/empty.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &[]).await?;

        // Read it back
        let read_data = fs.read_file("/empty.txt").await?.unwrap();
        assert!(read_data.is_empty());

        // Verify 0 chunks were created
        let ino = fs.resolve_path("/empty.txt").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 0);

        // Verify size is 0
        let stats = fs.stat("/empty.txt").await?.unwrap();
        assert_eq!(stats.size, 0);

        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(Vec::new()));

        Ok(())
    }

    #[tokio::test]
    async fn test_inline_small_file_and_overwrite() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/inline.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"hello world").await?;
        file.pwrite(6, b"agent").await?;

        let ino = fs.resolve_path("/inline.txt").await?.unwrap();
        assert_eq!(fs.read_file("/inline.txt").await?.unwrap(), b"hello agent");
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(b"hello agent".to_vec()));

        Ok(())
    }

    #[tokio::test]
    async fn test_inline_transitions_to_chunked_over_threshold() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let prefix = vec![1u8; DEFAULT_INLINE_THRESHOLD];
        let suffix = vec![2u8; 32];
        let (_, file) = fs
            .create_file("/transition.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &prefix).await?;

        let ino = fs.resolve_path("/transition.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?.0, STORAGE_INLINE);

        file.pwrite(DEFAULT_INLINE_THRESHOLD as u64, &suffix)
            .await?;

        let mut expected = prefix;
        expected.extend_from_slice(&suffix);
        assert_eq!(fs.read_file("/transition.bin").await?.unwrap(), expected);
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(fs.get_chunk_count(ino).await?, 1);

        Ok(())
    }

    #[tokio::test]
    async fn test_sparse_write_transitions_inline_to_chunked() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/sparse.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"abc").await?;
        file.pwrite(10, b"z").await?;

        let ino = fs.resolve_path("/sparse.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(fs.get_chunk_count(ino).await?, 1);

        let mut expected = b"abc".to_vec();
        expected.resize(10, 0);
        expected.push(b'z');
        let read_back = file.pread(0, expected.len() as u64).await?;
        assert_eq!(read_back, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_chunked_truncate_back_to_inline_when_dense() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let data = vec![7u8; DEFAULT_INLINE_THRESHOLD + 1];
        let (_, file) = fs
            .create_file("/dense.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/dense.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));

        file.truncate(128).await?;

        assert_eq!(fs.read_file("/dense.bin").await?.unwrap(), vec![7u8; 128]);
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(vec![7u8; 128]));

        Ok(())
    }

    #[tokio::test]
    async fn test_sparse_chunked_truncate_below_threshold_stays_chunked() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/sparse-truncate.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(fs.chunk_size() as u64 + 8, b"tail").await?;
        // Tier Four: ensure the sparse write reaches SQLite as chunked
        // storage before we truncate; otherwise truncate_pending strips it
        // in memory and the file never transitions out of INLINE.
        file.fsync().await?;
        file.truncate(4).await?;

        let ino = fs.resolve_path("/sparse-truncate.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(file.pread(0, 4).await?, vec![0u8; 4]);

        Ok(())
    }

    #[tokio::test]
    async fn test_64k_chunk_boundary_uses_single_default_chunk() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        assert_eq!(fs.chunk_size(), 64 * 1024);
        let data: Vec<u8> = (0..fs.chunk_size()).map(|i| (i % 251) as u8).collect();
        let (_, file) = fs
            .create_file("/boundary.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/boundary.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(fs.get_chunk_count(ino).await?, 1);
        assert_eq!(
            file.pread((fs.chunk_size() - 8) as u64, 16).await?,
            data[fs.chunk_size() - 8..].to_vec()
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overwrite_existing_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Write initial large file (3 chunks)
        let initial_data: Vec<u8> = (0..chunk_size * 3).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/overwrite.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &initial_data).await?;

        let ino = fs.resolve_path("/overwrite.txt").await?.unwrap();
        let initial_chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(initial_chunk_count, 3);

        // Overwrite with smaller file (1 chunk)
        let new_data = vec![42u8; 100];
        file.truncate(0).await?;
        file.pwrite(0, &new_data).await?;

        // Verify old chunks are gone and new data is correct
        let read_data = fs.read_file("/overwrite.txt").await?.unwrap();
        assert_eq!(read_data, new_data);

        let new_chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(new_chunk_count, 0);
        let (storage_kind, data_inline) = fs.get_storage_state(ino).await?;
        assert_eq!(storage_kind, STORAGE_INLINE);
        assert_eq!(data_inline, Some(new_data));

        // Verify size is updated
        let stats = fs.stat("/overwrite.txt").await?.unwrap();
        assert_eq!(stats.size, 100);

        Ok(())
    }

    #[tokio::test]
    async fn test_overwrite_with_larger_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Write initial small file (1 chunk)
        let initial_data = vec![1u8; 100];
        let (_, file) = fs.create_file("/grow.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &initial_data).await?;

        let ino = fs.resolve_path("/grow.txt").await?.unwrap();
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        assert_eq!(fs.get_storage_state(ino).await?.0, STORAGE_INLINE);

        // Overwrite with larger file (3 chunks)
        let new_data: Vec<u8> = (0..chunk_size * 3).map(|i| (i % 256) as u8).collect();
        file.truncate(0).await?;
        file.pwrite(0, &new_data).await?;

        // Verify data is correct
        let read_data = fs.read_file("/grow.txt").await?.unwrap();
        assert_eq!(read_data, new_data);
        assert_eq!(fs.get_chunk_count(ino).await?, 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_very_large_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write 1MB file
        let data_size = 1024 * 1024;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/large.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let read_data = fs.read_file("/large.bin").await?.unwrap();
        assert_eq!(read_data.len(), data_size);
        assert_eq!(read_data, data);

        // Verify correct number of chunks
        let chunk_size = fs.chunk_size();
        let expected_chunks = data_size.div_ceil(chunk_size);
        let ino = fs.resolve_path("/large.bin").await?.unwrap();
        let actual_chunks = fs.get_chunk_count(ino).await? as usize;
        assert_eq!(actual_chunks, expected_chunks);

        Ok(())
    }

    // ==================== Configuration Tests ====================

    #[tokio::test]
    async fn test_default_chunk_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        assert_eq!(fs.chunk_size(), DEFAULT_CHUNK_SIZE);
        assert_eq!(fs.chunk_size(), 65536);
        assert_eq!(fs.inline_threshold(), DEFAULT_INLINE_THRESHOLD);
        assert_eq!(fs.inline_threshold(), 16384);

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_size_accessor() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        assert!(chunk_size > 0);

        // Write data and verify chunks match expected based on chunk_size
        let data = vec![0u8; chunk_size * 2 + 1];
        let (_, file) = fs.create_file("/test.bin", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/test.bin").await?.unwrap();
        let chunk_count = fs.get_chunk_count(ino).await?;
        assert_eq!(chunk_count, 3);

        Ok(())
    }

    #[tokio::test]
    async fn test_config_persistence() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Query fs_config table directly
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT value FROM fs_config WHERE key = 'chunk_size'", ())
            .await?;

        let row = rows.next().await?.expect("chunk_size config should exist");
        let value = row
            .get_value(0)
            .ok()
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("chunk_size should be a text value");

        assert_eq!(value, "65536");

        let mut rows = conn
            .query(
                "SELECT value FROM fs_config WHERE key = 'inline_threshold'",
                (),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .expect("inline_threshold config should exist");
        let value = row
            .get_value(0)
            .ok()
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("inline_threshold should be a text value");

        assert_eq!(value, "16384");

        let mut rows = conn
            .query(
                "SELECT value FROM fs_config WHERE key = 'schema_version'",
                (),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .expect("schema_version config should exist");
        let value = row
            .get_value(0)
            .ok()
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("schema_version should be a text value");

        assert_eq!(value, "0.5");

        Ok(())
    }

    #[tokio::test]
    async fn schema_alter_non_duplicate_errors_propagate() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("malformed-view.db");
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("CREATE VIEW fs_inode AS SELECT 1 AS ino", ())
            .await?;

        let err = match add_column_idempotent(
            &conn,
            ColumnSpec {
                table_name: "fs_inode",
                column_name: "atime_nsec",
                type_name: "INTEGER",
                not_null: true,
                default_value: Some("0"),
            },
            "ALTER TABLE fs_inode ADD COLUMN atime_nsec INTEGER NOT NULL DEFAULT 0",
        )
        .await
        {
            Ok(_) => panic!("non-duplicate schema ALTER errors must propagate"),
            Err(err) => err,
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("fs_inode.atime_nsec") && err_msg.contains("no such table: fs_inode"),
            "error should preserve the failed ALTER reason, got: {err_msg}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn schema_alter_conflicting_column_definition_is_rejected() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("malformed-conflicting-column.db");

        {
            let db = Builder::new_local(db_path.to_str().unwrap())
                .build()
                .await?;
            let conn = db.connect()?;

            conn.execute(
                "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                (),
            )
            .await?;
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES
                    ('schema_version', '0.5'),
                    ('inline_threshold', '16384')",
                (),
            )
            .await?;
            conn.execute(
                "CREATE TABLE fs_inode (
                    ino INTEGER PRIMARY KEY,
                    mode INTEGER NOT NULL,
                    nlink INTEGER NOT NULL DEFAULT 0,
                    uid INTEGER NOT NULL DEFAULT 0,
                    gid INTEGER NOT NULL DEFAULT 0,
                    size INTEGER NOT NULL DEFAULT 0,
                    atime INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    ctime INTEGER NOT NULL,
                    rdev INTEGER NOT NULL DEFAULT 0,
                    atime_nsec TEXT NOT NULL DEFAULT 'bad',
                    mtime_nsec INTEGER NOT NULL DEFAULT 0,
                    ctime_nsec INTEGER NOT NULL DEFAULT 0,
                    data_inline BLOB,
                    storage_kind INTEGER NOT NULL DEFAULT 0
                )",
                (),
            )
            .await?;
            conn.execute(
                "INSERT INTO fs_inode
                    (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
                     atime_nsec, mtime_nsec, ctime_nsec, storage_kind)
                 VALUES (?, ?, 2, 0, 0, 0, 1, 1, 1, 0, 'bad', 0, 0, 0)",
                (ROOT_INO, DEFAULT_DIR_MODE as i64),
            )
            .await?;
        }

        let err = match AgentFS::new(db_path.to_str().unwrap()).await {
            Ok(_) => panic!("opening a database with a conflicting schema column must fail"),
            Err(err) => err,
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("fs_inode.atime_nsec") && err_msg.contains("incompatible definition"),
            "error should name the incompatible schema column, got: {err_msg}"
        );

        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        let mut rows = conn.query("PRAGMA table_info(fs_inode)", ()).await?;
        let mut found_conflicting_column = false;
        while let Some(row) = rows.next().await? {
            let name: String = row.get(1)?;
            if name == "atime_nsec" {
                let type_name: String = row.get(2)?;
                assert_eq!(type_name, "TEXT");
                found_conflicting_column = true;
            }
        }
        assert!(found_conflicting_column);

        Ok(())
    }

    #[tokio::test]
    async fn schema_alter_duplicate_columns_are_idempotent_on_reopen() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("already-upgraded.db");

        let first = AgentFS::new(db_path.to_str().unwrap()).await?;
        drop(first);

        let reopened = AgentFS::new(db_path.to_str().unwrap()).await?;
        let conn = reopened.pool.get_connection().await?;

        for column_name in [
            "atime_nsec",
            "mtime_nsec",
            "ctime_nsec",
            "data_inline",
            "storage_kind",
        ] {
            assert_eq!(
                fs_inode_column_count(&conn, column_name).await?,
                1,
                "fs_inode should contain exactly one {column_name} column"
            );
        }

        let mut rows = conn
            .query(
                "SELECT value FROM fs_config WHERE key = 'schema_version'",
                (),
            )
            .await?;
        let version: String = rows
            .next()
            .await?
            .expect("schema_version config should exist")
            .get(0)?;
        assert_eq!(version, AGENTFS_SCHEMA_VERSION);

        Ok(())
    }

    #[tokio::test]
    async fn test_v04_database_is_rejected_without_inline_migration() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("legacy-v04.db");

        {
            let db = Builder::new_local(db_path.to_str().unwrap())
                .build()
                .await?;
            let conn = db.connect()?;
            conn.execute(
                "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
                (),
            )
            .await?;
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES ('schema_version', '0.4')",
                (),
            )
            .await?;
            conn.execute(
                "CREATE TABLE fs_inode (
                    ino INTEGER PRIMARY KEY AUTOINCREMENT,
                    mode INTEGER NOT NULL,
                    nlink INTEGER NOT NULL DEFAULT 0,
                    uid INTEGER NOT NULL DEFAULT 0,
                    gid INTEGER NOT NULL DEFAULT 0,
                    size INTEGER NOT NULL DEFAULT 0,
                    atime INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    ctime INTEGER NOT NULL,
                    rdev INTEGER NOT NULL DEFAULT 0,
                    atime_nsec INTEGER NOT NULL DEFAULT 0,
                    mtime_nsec INTEGER NOT NULL DEFAULT 0,
                    ctime_nsec INTEGER NOT NULL DEFAULT 0
                )",
                (),
            )
            .await?;
        }

        let result =
            crate::AgentFS::open(crate::AgentFSOptions::with_path(db_path.to_string_lossy())).await;
        match result {
            Err(Error::SchemaVersionMismatch { found, expected }) => {
                assert_eq!(found, "0.4");
                assert_eq!(expected, "0.5");
            }
            Err(err) => panic!("expected schema version mismatch, got {err}"),
            Ok(_) => panic!("legacy v0.4 database should not open as v0.5"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_file_backed_connections_use_production_pragmas() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let conn1 = fs.pool.get_connection().await?;
        let conn2 = fs.pool.get_connection().await?;

        for conn in [&conn1, &conn2] {
            assert_eq!(
                read_pragma_i64(conn, "PRAGMA synchronous").await,
                TURSO_OBSERVED_SYNCHRONOUS_NORMAL
            );
            assert_eq!(read_pragma_i64(conn, "PRAGMA busy_timeout").await, 5000);
            assert_eq!(read_pragma_i64(conn, "PRAGMA temp_store").await, 2);
            assert_eq!(
                read_pragma_text(conn, "PRAGMA journal_mode")
                    .await
                    .to_lowercase(),
                "wal"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_file_backed_options_issue_durable_baseline_sql() {
        let options = file_backed_connection_pool_options();

        assert_eq!(options.max_connections, FILE_BACKED_MAX_CONNECTIONS);
        assert_eq!(options.setup_sql[0], TEMP_STORE_MEMORY_SQL);
        assert!(options.setup_sql.iter().any(|sql| sql == BUSY_TIMEOUT_SQL));
        assert!(options.setup_sql.iter().any(|sql| sql == WAL_MODE_SQL));
        assert!(options
            .setup_sql
            .iter()
            .any(|sql| sql == BASELINE_SYNCHRONOUS_SQL));
        assert!(!options
            .setup_sql
            .iter()
            .any(|sql| sql == "PRAGMA synchronous = OFF"));
    }

    #[tokio::test]
    async fn test_file_backed_agentfs_concurrent_operations_complete() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/seed.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"seed").await?;

        let mut handles = Vec::new();
        for worker in 0..8 {
            let fs = fs.clone();
            handles.push(tokio::spawn(async move {
                for iteration in 0..5 {
                    let data = fs.read_file("/seed.txt").await?.unwrap();
                    assert_eq!(data, b"seed");

                    let path = format!("/worker-{worker}-{iteration}");
                    fs.mkdir(&path, 0, 0).await?;
                }
                Ok::<(), Error>(())
            }));
        }

        for handle in handles {
            handle.await.unwrap()?;
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_fsync_restores_synchronous_normal() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let conn = fs.pool.get_connection().await?;
        conn.execute("PRAGMA synchronous = OFF", ()).await?;
        drop(conn);

        fs.fsync().await?;

        let conn = fs.pool.get_connection().await?;
        assert_eq!(
            read_pragma_i64(&conn, "PRAGMA synchronous").await,
            TURSO_OBSERVED_SYNCHRONOUS_NORMAL
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_file_fsync_restores_synchronous_normal() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs
            .create_file("/fsync.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        let conn = fs.pool.get_connection().await?;
        conn.execute("PRAGMA synchronous = OFF", ()).await?;
        drop(conn);

        file.fsync().await?;

        let conn = fs.pool.get_connection().await?;
        assert_eq!(
            read_pragma_i64(&conn, "PRAGMA synchronous").await,
            TURSO_OBSERVED_SYNCHRONOUS_NORMAL
        );

        Ok(())
    }

    // ==================== Schema Tests ====================

    #[tokio::test]
    async fn test_chunk_index_uniqueness() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write a file to create chunks
        let chunk_size = fs.chunk_size();
        let data = vec![0u8; chunk_size * 2];
        let (_, file) = fs
            .create_file("/unique.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/unique.txt").await?.unwrap();
        // Tier Four: pwrite is async-batched; drain so fs_data is populated
        // before we probe its primary-key constraint.
        fs.drain_inode_writes(ino).await?;

        // Try to insert a duplicate chunk - should fail due to PRIMARY KEY constraint
        let conn = fs.pool.get_connection().await?;
        let result = conn
            .execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, 0, ?)",
                (ino, vec![1u8; 10]),
            )
            .await;

        assert!(result.is_err(), "Duplicate chunk_index should be rejected");

        Ok(())
    }

    #[tokio::test]
    async fn test_chunk_ordering_in_database() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create 5 chunks with identifiable data
        let data_size = chunk_size * 5;
        let data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs
            .create_file("/ordered.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/ordered.bin").await?.unwrap();
        // Tier Four: drain so fs_data rows are present for the SELECT below.
        fs.drain_inode_writes(ino).await?;

        // Query chunks in order
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query(
                "SELECT chunk_index FROM fs_data WHERE ino = ? ORDER BY chunk_index",
                (ino,),
            )
            .await?;

        let mut indices = Vec::new();
        while let Some(row) = rows.next().await? {
            let idx = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(-1);
            indices.push(idx);
        }

        assert_eq!(indices, vec![0, 1, 2, 3, 4]);

        Ok(())
    }

    // ==================== Cleanup Tests ====================

    #[tokio::test]
    async fn test_delete_file_removes_all_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();
        // Create multi-chunk file
        let data = vec![0u8; chunk_size * 4];
        let (_, file) = fs
            .create_file("/deleteme.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &data).await?;

        let ino = fs.resolve_path("/deleteme.txt").await?.unwrap();
        assert_eq!(fs.get_chunk_count(ino).await?, 4);

        // Close the handle first: with it open, deletion is deferred (POSIX
        // unlink-while-open) and the chunks legitimately survive the remove.
        drop(file);

        // Delete the file
        fs.remove("/deleteme.txt").await?;

        // Verify all chunks are gone
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM fs_data WHERE ino = ?", (ino,))
            .await?;

        let count = rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1);

        assert_eq!(count, 0, "All chunks should be deleted");

        Ok(())
    }

    async fn count_rows(fs: &AgentFS, table: &str, ino: i64) -> Result<i64> {
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query(
                &format!("SELECT COUNT(*) FROM {table} WHERE ino = ?"),
                (ino,),
            )
            .await?;
        Ok(rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1))
    }

    #[tokio::test]
    async fn test_unlink_while_open_defers_reap() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (stats, file) = fs
            .create_file("/ghost.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let ino = stats.ino;
        file.pwrite(0, b"ghost").await?;

        FileSystem::unlink(&fs, ROOT_INO, "ghost.bin").await?;

        // POSIX: the open handle keeps the inode readable and writable.
        assert!(fs.resolve_path("/ghost.bin").await?.is_none());
        assert_eq!(file.pread(0, 5).await?, b"ghost");
        file.pwrite(5, b"-more").await?;
        assert_eq!(file.pread(0, 10).await?, b"ghost-more");
        assert_eq!(file.fstat().await?.nlink, 0);
        assert_eq!(count_rows(&fs, "fs_inode", ino).await?, 1);

        // Last handle drop queues the reap; the next namespace mutation
        // (or finalize) executes it.
        drop(file);
        fs.process_deferred_reaps().await?;
        assert_eq!(count_rows(&fs, "fs_inode", ino).await?, 0);
        assert_eq!(count_rows(&fs, "fs_data", ino).await?, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_mount_sweep_reaps_crashed_orphans() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("sweep.db");
        let db_path = db_path.to_str().unwrap();

        let ino = {
            let fs = AgentFS::new(db_path).await?;
            let (stats, file) = fs
                .create_file("/ghost.bin", DEFAULT_FILE_MODE, 0, 0)
                .await?;
            file.pwrite(0, b"ghost").await?;
            file.drain_writes().await?;
            FileSystem::unlink(&fs, ROOT_INO, "ghost.bin").await?;
            // Simulate a crash: the guard never releases, so the orphan is
            // neither queued nor reaped before the process "dies".
            std::mem::forget(file);
            stats.ino
        };

        let fs = AgentFS::new(db_path).await?;
        assert_eq!(count_rows(&fs, "fs_inode", ino).await?, 0);
        assert_eq!(count_rows(&fs, "fs_data", ino).await?, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_multiple_files_different_sizes() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let chunk_size = fs.chunk_size();

        // Create files of various sizes
        let files = vec![
            ("/tiny.txt", 10),
            ("/small.txt", chunk_size / 2),
            ("/exact.txt", chunk_size),
            ("/medium.txt", chunk_size * 2 + 100),
            ("/large.txt", chunk_size * 5),
        ];

        for (path, size) in &files {
            let data: Vec<u8> = (0..*size).map(|i| (i % 256) as u8).collect();
            let (_, file) = fs.create_file(path, DEFAULT_FILE_MODE, 0, 0).await?;
            file.pwrite(0, &data).await?;
        }

        // Verify each file has correct data and chunk count
        for (path, size) in &files {
            let read_data = fs.read_file(path).await?.unwrap();
            assert_eq!(read_data.len(), *size, "Size mismatch for {}", path);

            let expected_data: Vec<u8> = (0..*size).map(|i| (i % 256) as u8).collect();
            assert_eq!(read_data, expected_data, "Data mismatch for {}", path);

            let expected_chunks = if *size <= fs.inline_threshold() {
                0
            } else {
                size.div_ceil(chunk_size)
            };
            let ino = fs.resolve_path(path).await?.unwrap();
            let actual_chunks = fs.get_chunk_count(ino).await? as usize;
            assert_eq!(
                actual_chunks, expected_chunks,
                "Chunk count mismatch for {}",
                path
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn file_pread_basic() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        assert_eq!(file.pread(0, 10).await?, &data[0..10]);
        assert_eq!(file.pread(50, 20).await?, &data[50..70]);
        assert_eq!(file.pread(90, 10).await?, &data[90..100]);
        Ok(())
    }

    #[tokio::test]
    async fn file_pread_past_eof() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..50).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        assert!(file.pread(100, 10).await?.is_empty());
        assert_eq!(file.pread(40, 20).await?, &data[40..50]);
        Ok(())
    }

    #[tokio::test]
    async fn file_open_nonexistent_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let result = fs.open("/nonexistent.txt").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotFound))));
        Ok(())
    }

    #[tokio::test]
    async fn file_pread_across_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        let start = chunk_size - 10;
        assert_eq!(
            file.pread(start as u64, 20).await?,
            &data[start..start + 20]
        );

        let start = chunk_size / 2;
        let size = chunk_size * 2;
        assert_eq!(
            file.pread(start as u64, size as u64).await?,
            &data[start..start + size]
        );
        Ok(())
    }

    #[tokio::test]
    async fn file_pwrite_basic() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data = vec![0; 100];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.pwrite(50, &[1, 2, 3, 4, 5]).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[50..55], &[1, 2, 3, 4, 5]);
        assert_eq!(&result[0..50], &vec![0u8; 50][..]);
        assert_eq!(&result[55..100], &vec![0u8; 45][..]);
        Ok(())
    }

    #[tokio::test]
    async fn file_pwrite_extend_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data = vec![1; 50];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.pwrite(100, &[2, 2, 2, 2, 2]).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 105);
        assert_eq!(&result[0..50], &vec![1u8; 50][..]);
        assert_eq!(&result[50..100], &vec![0u8; 50][..]);
        assert_eq!(&result[100..105], &[2, 2, 2, 2, 2]);
        Ok(())
    }

    #[tokio::test]
    async fn file_create_then_pwrite_writes_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/new.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &[1, 2, 3]).await?;

        assert_eq!(fs.read_file("/new.txt").await?.unwrap(), &[1, 2, 3]);
        Ok(())
    }

    #[tokio::test]
    async fn file_pwrite_across_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let data = vec![0; chunk_size * 3];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        let write_data: Vec<u8> = (0..20).collect();
        let start = chunk_size - 10;
        file.pwrite(start as u64, &write_data).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(&result[start..start + 20], &write_data[..]);
        assert_eq!(&result[0..start], &vec![0u8; start][..]);
        assert_eq!(
            &result[start + 20..],
            &vec![0u8; chunk_size * 3 - start - 20][..]
        );
        Ok(())
    }

    #[tokio::test]
    async fn file_pread_pwrite_roundtrip() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let initial: Vec<u8> = (0..(chunk_size * 2)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &initial).await?;

        let patches = [
            (0u64, vec![0xAAu8; 10]),
            (chunk_size as u64 - 5, vec![0xBB; 10]),
            (chunk_size as u64 * 2 - 1, vec![0xCC; 1]),
        ];

        for (offset, data) in &patches {
            file.pwrite(*offset, data).await?;
        }
        for (offset, expected) in &patches {
            assert_eq!(
                file.pread(*offset, expected.len() as u64).await?,
                expected.as_slice()
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_ranges_preserves_order_and_inline_storage() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/batch-inline.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite_ranges(vec![
            WriteRange {
                offset: 0,
                data: b"abcdef".to_vec(),
            },
            WriteRange {
                offset: 2,
                data: b"ZZ".to_vec(),
            },
            WriteRange {
                offset: 6,
                data: b"!".to_vec(),
            },
        ])
        .await?;

        let ino = fs.resolve_path("/batch-inline.txt").await?.unwrap();
        assert_eq!(file.pread(0, 16).await?, b"abZZef!");
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        assert_eq!(
            fs.get_storage_state(ino).await?,
            (STORAGE_INLINE, Some(b"abZZef!".to_vec()))
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_ranges_disjoint_inplace_writes_stay_inline() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let initial: Vec<u8> = (0..128).collect();
        let (_, file) = fs
            .create_file("/batch-inplace.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &initial).await?;

        file.pwrite_ranges(vec![
            WriteRange {
                offset: 8,
                data: b"ABCD".to_vec(),
            },
            WriteRange {
                offset: 64,
                data: b"WXYZ".to_vec(),
            },
        ])
        .await?;

        let mut expected = initial;
        expected[8..12].copy_from_slice(b"ABCD");
        expected[64..68].copy_from_slice(b"WXYZ");

        let ino = fs.resolve_path("/batch-inplace.bin").await?.unwrap();
        assert_eq!(file.pread(0, expected.len() as u64).await?, expected);
        assert_eq!(fs.get_chunk_count(ino).await?, 0);
        assert_eq!(fs.get_storage_state(ino).await?.0, STORAGE_INLINE);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_ranges_sparse_write_transitions_to_chunked() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/batch-sparse.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite_ranges(vec![
            WriteRange {
                offset: 0,
                data: b"head".to_vec(),
            },
            WriteRange {
                offset: fs.chunk_size() as u64 + 4,
                data: b"tail".to_vec(),
            },
        ])
        .await?;

        let ino = fs.resolve_path("/batch-sparse.bin").await?.unwrap();
        assert_eq!(fs.get_storage_state(ino).await?, (STORAGE_CHUNKED, None));
        assert_eq!(fs.get_chunk_count(ino).await?, 2);

        let mut expected = b"head".to_vec();
        expected.resize(fs.chunk_size() + 4, 0);
        expected.extend_from_slice(b"tail");
        assert_eq!(file.pread(0, expected.len() as u64).await?, expected);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_ranges_batched_drains_explicitly() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/batched.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite_ranges_batched(vec![
            WriteRange {
                offset: 0,
                data: b"hello".to_vec(),
            },
            WriteRange {
                offset: 5,
                data: b" world".to_vec(),
            },
        ])
        .await?;

        let flushed_stats = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(
            flushed_stats.size, 11,
            "metadata reads should drain pending batched writes before reporting size"
        );

        file.drain_writes().await?;
        assert_eq!(file.pread(0, 32).await?, b"hello world");

        Ok(())
    }

    #[tokio::test]
    async fn test_setattr_after_batched_write_preserves_explicit_times() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/setattr-after-write.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        // Buffered write stays in the overlay (long timer, no drain).
        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 0,
            data: b"deferred body".to_vec(),
        }])
        .await?;

        // Explicit setattr (the kernel's writeback mtime update) lands while
        // the data is still pending. No drain happens here by default.
        let explicit_secs = 1_234_567_890;
        let explicit_nsec = 42;
        FileSystem::utimens(
            &fs,
            stats.ino,
            TimeChange::Omit,
            TimeChange::Set(explicit_secs, explicit_nsec),
        )
        .await?;

        // The deferred commit must NOT re-stamp mtime/ctime over the explicit
        // value the setattr just wrote.
        file.drain_writes().await?;

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(
            after.mtime, explicit_secs,
            "explicit mtime must survive the deferred data commit"
        );
        assert_eq!(
            after.mtime_nsec, explicit_nsec,
            "explicit mtime_nsec must survive the deferred data commit"
        );
        assert_eq!(after.size, 13);
        assert_eq!(file.pread(0, 32).await?, b"deferred body");

        Ok(())
    }

    #[tokio::test]
    async fn test_write_after_setattr_restamps_times_on_commit() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/write-after-setattr.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 0,
            data: b"first".to_vec(),
        }])
        .await?;

        let stale_secs = 1_111_111_111;
        FileSystem::utimens(
            &fs,
            stats.ino,
            TimeChange::Omit,
            TimeChange::Set(stale_secs, 0),
        )
        .await?;

        // A write AFTER the setattr means the file changed again: the commit
        // must stamp fresh mtime/ctime, not preserve the stale explicit value.
        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 5,
            data: b" second".to_vec(),
        }])
        .await?;

        file.drain_writes().await?;

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert!(
            after.mtime > stale_secs,
            "a write after the explicit setattr must bump mtime again (got {}, explicit was {})",
            after.mtime,
            stale_secs
        );
        assert_eq!(file.pread(0, 32).await?, b"first second");

        Ok(())
    }

    #[tokio::test]
    async fn test_utimens_with_pending_writes_is_visible_and_committed_with_data() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/stash-times.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        // Buffered write stays in the overlay (long timer, no drain).
        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 0,
            data: b"stash body".to_vec(),
        }])
        .await?;

        // The explicit setattr is stashed in the pending entry instead of
        // paying its own SQLite transaction.
        let explicit_secs = 1_999_999_999;
        let explicit_nsec: u32 = 7;
        FileSystem::utimens(
            &fs,
            stats.ino,
            TimeChange::Set(11, 13),
            TimeChange::Set(explicit_secs, explicit_nsec),
        )
        .await?;

        // Visible immediately, before any drain commits the row UPDATE.
        let before = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(
            before.mtime, explicit_secs,
            "stashed mtime must be visible before the drain commits it"
        );
        assert_eq!(before.mtime_nsec, explicit_nsec);
        assert_eq!(before.atime, 11);
        assert_eq!(before.atime_nsec, 13);
        assert_eq!(before.size, 10, "pending data size must still be merged");

        // The drain commits the data and the stashed times in one transaction.
        file.drain_writes().await?;

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(
            after.mtime, explicit_secs,
            "explicit mtime must survive the deferred data commit"
        );
        assert_eq!(after.mtime_nsec, explicit_nsec);
        assert_eq!(after.atime, 11);
        assert_eq!(after.atime_nsec, 13);
        assert_eq!(after.size, 10);
        assert_eq!(file.pread(0, 32).await?, b"stash body");

        Ok(())
    }

    #[tokio::test]
    async fn test_write_after_stashed_utimens_restamps_mtime_keeps_atime() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (stats, file) = fs
            .create_file("/stash-then-write.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 0,
            data: b"first".to_vec(),
        }])
        .await?;

        let stale_secs = 1_222_222_222;
        FileSystem::utimens(
            &fs,
            stats.ino,
            TimeChange::Set(33, 44),
            TimeChange::Set(stale_secs, 0),
        )
        .await?;

        // A write AFTER the stashed setattr means the file changed again: the
        // commit must stamp fresh mtime/ctime. The explicitly-set atime is not
        // affected by writes and must survive.
        file.pwrite_ranges_batched(vec![WriteRange {
            offset: 5,
            data: b" second".to_vec(),
        }])
        .await?;

        file.drain_writes().await?;

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert!(
            after.mtime > stale_secs,
            "a write after the stashed setattr must bump mtime again (got {}, explicit was {})",
            after.mtime,
            stale_secs
        );
        assert_eq!(after.atime, 33, "explicit atime must survive a later write");
        assert_eq!(after.atime_nsec, 44);
        assert_eq!(file.pread(0, 32).await?, b"first second");

        Ok(())
    }

    // Build a batcher with an explicit config so the test is independent of the
    // process-global AGENTFS_BATCH_* env vars (which other tests mutate
    // concurrently). Reuses `fs`'s pool/attr cache so commits hit real inodes.
    fn test_batcher(
        fs: &AgentFS,
        batch_ms_secs: u64,
        batch_bytes: usize,
        batch_global_bytes: usize,
    ) -> Arc<AgentFSWriteBatcher> {
        Arc::new(AgentFSWriteBatcher {
            pool: fs.pool.clone(),
            chunk_size: fs.chunk_size,
            inline_threshold: fs.inline_threshold,
            attr_cache: fs.attr_cache.clone(),
            batch_ms: Duration::from_secs(batch_ms_secs),
            batch_bytes,
            batch_global_bytes,
            txn_max_inodes: crate::config::DEFAULT_WRITE_BATCH_TXN_INODES,
            txn_max_bytes: crate::config::DEFAULT_WRITE_BATCH_TXN_BYTES,
            state: RwLock::new(AgentFSWriteBatcherState::default()),
            commit_lock: AsyncMutex::new(()),
        })
    }

    #[tokio::test]
    async fn test_batcher_bytes_trigger_restamps_after_explicit_times() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (stats, _file) = fs
            .create_file("/bytes-trigger-times.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        // Long timer and huge global cap make the per-inode byte cap the only
        // reachable synchronous drain trigger in this test.
        let batcher = test_batcher(&fs, 600, 8, 1 << 30);

        batcher
            .enqueue(
                stats.ino,
                vec![WriteRange {
                    offset: 0,
                    data: b"abcd".to_vec(),
                }],
            )
            .await?;
        assert!(
            batcher.has_pending(stats.ino),
            "below the per-inode byte cap, the write must stay pending"
        );

        let explicit_secs = 1_345_678_901;
        let explicit_nsec = 123;
        let conn = fs.pool.get_connection().await?;
        conn.execute(
            "UPDATE fs_inode SET mtime = ?, mtime_nsec = ?, ctime = ?, ctime_nsec = ? WHERE ino = ?",
            (
                explicit_secs,
                explicit_nsec,
                explicit_secs,
                explicit_nsec,
                stats.ino,
            ),
        )
        .await?;
        batcher.mark_times_explicit(stats.ino);

        batcher
            .enqueue(
                stats.ino,
                vec![WriteRange {
                    offset: 4,
                    data: b"efgh".to_vec(),
                }],
            )
            .await?;
        assert!(
            !batcher.has_pending(stats.ino),
            "crossing the per-inode byte cap must drain this inode"
        );

        let after = FileSystem::getattr(&fs, stats.ino).await?.unwrap();
        assert_eq!(after.size, 8);
        assert!(
            after.mtime > explicit_secs,
            "the second write crosses the Bytes cap after the explicit setattr, \
             so the drain must stamp a fresh mtime (got {}, explicit was {})",
            after.mtime,
            explicit_secs
        );
        assert_eq!(
            fs.read_file("/bytes-trigger-times.txt").await?.unwrap(),
            b"abcdefgh"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_batcher_global_cap_triggers_full_drain_and_tracks_total() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (sa, _fa) = fs.create_file("/a.bin", DEFAULT_FILE_MODE, 0, 0).await?;
        let (sb, _fb) = fs.create_file("/b.bin", DEFAULT_FILE_MODE, 0, 0).await?;

        // 10-minute timer and huge per-inode trigger so the ONLY drain path is
        // the 64-byte global cross-inode cap.
        let batcher = test_batcher(&fs, 600, 1 << 20, 64);

        // Write below the cap to inode A: stays pending.
        batcher
            .enqueue(
                sa.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'x'; 50],
                }],
            )
            .await?;
        assert_eq!(
            batcher.state.read().total_pending_bytes,
            50,
            "write below the global cap must remain in the overlay"
        );

        // Truncating into the pending range shrinks the tracked total.
        batcher.truncate_pending(sa.ino, 20);
        assert_eq!(
            batcher.state.read().total_pending_bytes,
            20,
            "truncate_pending must shrink the running total to the kept prefix"
        );

        // Write to inode B crosses the cap (20 + 50 >= 64): a full batched drain
        // commits every pending inode and resets the running total to zero.
        batcher
            .enqueue(
                sb.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'y'; 50],
                }],
            )
            .await?;
        assert_eq!(
            batcher.state.read().total_pending_bytes,
            0,
            "crossing the global cap must drain all pending inodes"
        );

        // Committed data is intact and reflects the truncate.
        assert_eq!(fs.read_file("/a.bin").await?.unwrap(), vec![b'x'; 20]);
        assert_eq!(fs.read_file("/b.bin").await?.unwrap(), vec![b'y'; 50]);
        Ok(())
    }

    #[tokio::test]
    async fn test_batcher_discard_pending_updates_total() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (sa, _fa) = fs.create_file("/c.bin", DEFAULT_FILE_MODE, 0, 0).await?;

        // No timer/bytes/global drain: writes accumulate so we can observe the
        // total before discarding.
        let batcher = test_batcher(&fs, 600, 1 << 20, 1 << 30);
        batcher
            .enqueue(
                sa.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'z'; 100],
                }],
            )
            .await?;
        assert_eq!(batcher.state.read().total_pending_bytes, 100);

        batcher.discard_pending(sa.ino);
        assert_eq!(
            batcher.state.read().total_pending_bytes,
            0,
            "discard_pending must subtract the discarded inode's bytes"
        );
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_to_zero() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.truncate(0).await?;

        assert!(fs.read_file("/test.txt").await?.unwrap().is_empty());
        assert_eq!(fs.stat("/test.txt").await?.unwrap().size, 0);
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_smaller_within_chunk() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.truncate(50).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 50);
        assert_eq!(result, &data[..50]);
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_across_chunk_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        let new_size = chunk_size + chunk_size / 2;
        file.truncate(new_size as u64).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), new_size);
        assert_eq!(result, &data[..new_size]);
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_extend_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data: Vec<u8> = (0..50).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.truncate(100).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(stats.size, 100);
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[..50], &data[..]);
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_nonexistent_open_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let result = fs.open("/nonexistent.txt").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotFound))));
        Ok(())
    }

    #[tokio::test]
    async fn file_truncate_at_chunk_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;
        file.truncate(chunk_size as u64).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), chunk_size);
        assert_eq!(result, &data[..chunk_size]);
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_file_same_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let data = b"hello world";
        let (_, file) = fs.create_file("/old.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, data).await?;
        rename_path_via_trait(&fs, "/old.txt", "/new.txt").await?;

        assert!(fs.stat("/old.txt").await?.is_none());
        assert_eq!(fs.read_file("/new.txt").await?.unwrap(), data);
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_file_to_different_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/subdir", 0, 0).await?;
        let data = b"test data";
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, data).await?;
        rename_path_via_trait(&fs, "/file.txt", "/subdir/file.txt").await?;

        assert!(fs.stat("/file.txt").await?.is_none());
        assert_eq!(fs.read_file("/subdir/file.txt").await?.unwrap(), data);
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_overwrite_existing_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/src.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"source").await?;
        let (_, file) = fs.create_file("/dst.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"destination").await?;
        rename_path_via_trait(&fs, "/src.txt", "/dst.txt").await?;

        assert!(fs.stat("/src.txt").await?.is_none());
        assert_eq!(fs.read_file("/dst.txt").await?.unwrap(), b"source");
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/olddir", 0, 0).await?;
        let (_, file) = fs
            .create_file("/olddir/file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;
        rename_path_via_trait(&fs, "/olddir", "/newdir").await?;

        assert!(fs.stat("/olddir").await?.is_none());
        assert!(fs.stat("/newdir").await?.is_some());
        assert_eq!(fs.read_file("/newdir/file.txt").await?.unwrap(), b"content");
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_directory_into_own_subtree_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/parent", 0, 0).await?;
        fs.mkdir("/parent/child", 0, 0).await?;

        let parent_ino = fs.stat("/parent").await?.unwrap().ino;
        let child_ino = fs.stat("/parent/child").await?.unwrap().ino;
        let root_before = fs.readdir(ROOT_INO).await?.unwrap();
        let parent_before = fs.readdir(parent_ino).await?.unwrap();
        let child_before = fs.readdir(child_ino).await?.unwrap();

        let result = rename_path_via_trait(&fs, "/parent", "/parent/child/parent").await;

        assert!(matches!(result, Err(Error::Fs(FsError::InvalidRename))));
        assert_eq!(fs.readdir(ROOT_INO).await?.unwrap(), root_before);
        assert_eq!(fs.readdir(parent_ino).await?.unwrap(), parent_before);
        assert_eq!(fs.readdir(child_ino).await?.unwrap(), child_before);
        assert!(fs.stat("/parent").await?.is_some());
        assert!(fs.stat("/parent/child").await?.is_some());
        assert!(fs.stat("/parent/child/parent").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_root_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let result = rename_path_via_trait(&fs, "/", "/newroot").await;
        assert!(matches!(result, Err(Error::Fs(FsError::RootOperation))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_to_root_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;
        let result = rename_path_via_trait(&fs, "/file.txt", "/").await;
        assert!(matches!(result, Err(Error::Fs(FsError::RootOperation))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_nonexistent_source_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let result = rename_path_via_trait(&fs, "/nonexistent.txt", "/new.txt").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotFound))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_overwrite_nonempty_directory_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/src", 0, 0).await?;
        fs.mkdir("/dst", 0, 0).await?;
        let (_, file) = fs
            .create_file("/dst/file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;

        let result = rename_path_via_trait(&fs, "/src", "/dst").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotEmpty))));
        assert!(fs.stat("/src").await?.is_some());
        assert!(fs.stat("/dst").await?.is_some());
        assert!(fs.stat("/dst/file.txt").await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_file_to_directory_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;
        fs.mkdir("/dir", 0, 0).await?;

        let result = rename_path_via_trait(&fs, "/file.txt", "/dir").await;
        assert!(matches!(result, Err(Error::Fs(FsError::IsADirectory))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_directory_to_file_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/dir", 0, 0).await?;
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;

        let result = rename_path_via_trait(&fs, "/dir", "/file.txt").await;
        assert!(matches!(result, Err(Error::Fs(FsError::NotADirectory))));
        Ok(())
    }

    #[tokio::test]
    async fn trait_rename_updates_ctime() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/old.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;
        let stats_before = fs.stat("/old.txt").await?.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        rename_path_via_trait(&fs, "/old.txt", "/new.txt").await?;

        let stats_after = fs.stat("/new.txt").await?.unwrap();
        assert!(stats_after.ctime >= stats_before.ctime);
        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_regular_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file with default permissions
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"content").await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        let ino = stats.ino;
        assert_eq!(
            stats.mode & 0o7777,
            0o644,
            "Default file mode should be 0o644"
        );

        // Change to executable
        fs.chmod(ino, 0o755).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o7777,
            0o755,
            "Mode should be 0o755 after chmod"
        );
        assert!(stats.is_file(), "Should still be a regular file");

        // Change to read-only
        fs.chmod(ino, 0o444).await?;

        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(
            stats.mode & 0o7777,
            0o444,
            "Mode should be 0o444 after chmod"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_preserves_file_type() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a regular file
        let (file_stats, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"content").await?;
        fs.chmod(file_stats.ino, 0o755).await?;
        let stats = fs.stat("/file.txt").await?.unwrap();
        assert!(stats.is_file(), "Should remain a regular file after chmod");

        // Create a directory
        fs.mkdir("/dir", 0, 0).await?;
        let dir_stats = fs.stat("/dir").await?.unwrap();
        fs.chmod(dir_stats.ino, 0o700).await?;
        let stats = fs.stat("/dir").await?.unwrap();
        assert!(
            stats.is_directory(),
            "Should remain a directory after chmod"
        );
        assert_eq!(stats.mode & 0o7777, 0o700, "Directory mode should be 0o700");

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_nonexistent_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Use a non-existent inode
        let result = fs.chmod(999999, 0o755).await;
        assert!(result.is_err(), "chmod on nonexistent inode should fail");

        Ok(())
    }

    #[tokio::test]
    async fn test_chmod_symlink() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create target and symlink
        let (_, file) = fs
            .create_file("/target.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;
        FileSystem::symlink(&fs, ROOT_INO, "link.txt", "/target.txt", 0, 0).await?;
        let link_stats = FileSystem::lookup(&fs, ROOT_INO, "link.txt")
            .await?
            .unwrap();

        // chmod the symlink (should work on the symlink inode)
        fs.chmod(link_stats.ino, 0o755).await?;

        let stats = FileSystem::lookup(&fs, ROOT_INO, "link.txt")
            .await?
            .unwrap();
        assert!(stats.is_symlink(), "Should still be a symlink");

        Ok(())
    }

    // ==================== Tier Four: Overlay Read-After-Write ====================
    //
    // These exercise the Tier 4 invariant that `pread` / `getattr` /
    // `truncate` reflect pending batched writes BEFORE the SQLite drain
    // commits them — i.e. the per-fd write-then-read story works without
    // forcing a synchronous SQLite transaction on every read.

    #[tokio::test]
    async fn pread_after_uncommitted_pwrite_sees_pending() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs
            .create_file("/overlay.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"hello world").await?;
        // No fsync — Tier 4 says the same fd must see its own writes via
        // the in-memory overlay, regardless of whether SQLite has them yet.
        assert_eq!(file.pread(0, 11).await?, b"hello world");
        assert_eq!(file.pread(6, 5).await?, b"world");
        Ok(())
    }

    #[tokio::test]
    async fn pread_after_uncommitted_pwrite_partial_overlap() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/over.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"AAAAAAAAAA").await?;
        file.fsync().await?;
        file.pwrite(4, b"BBB").await?;
        // Read spans SQLite-resident (A) and pending (B) regions.
        assert_eq!(file.pread(2, 6).await?, b"AABBBA");
        Ok(())
    }

    #[tokio::test]
    async fn pread_in_unwritten_region_returns_sqlite() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/hole.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &[0xCDu8; 64]).await?;
        file.fsync().await?;
        file.pwrite(80, b"tail").await?;
        // Read [16, 32) — entirely SQLite, no pending overlap.
        assert_eq!(file.pread(16, 16).await?, vec![0xCDu8; 16]);
        Ok(())
    }

    #[tokio::test]
    async fn truncate_drops_pending_beyond_new_size() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs
            .create_file("/trunc.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"abcdef").await?;
        file.truncate(3).await?;
        assert_eq!(file.pread(0, 16).await?, b"abc");
        let attrs = FileSystem::getattr(&fs, fs.resolve_path("/trunc.txt").await?.unwrap())
            .await?
            .unwrap();
        assert_eq!(attrs.size, 3);
        Ok(())
    }

    #[tokio::test]
    async fn truncate_clips_range_spanning_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs.create_file("/clip.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(2, b"PPPPPP").await?;
        // pending occupies [2, 8). Truncate to 5 should keep [2, 5).
        file.truncate(5).await?;
        assert_eq!(file.pread(0, 16).await?, vec![0, 0, b'P', b'P', b'P']);
        Ok(())
    }

    #[tokio::test]
    async fn getattr_reflects_pending_size_growth() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (created, file) = fs.create_file("/grow.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        let pre = FileSystem::getattr(&fs, created.ino).await?.unwrap();
        assert_eq!(pre.size, 0);
        file.pwrite(0, b"abcdefghij").await?;
        let post = FileSystem::getattr(&fs, created.ino).await?.unwrap();
        assert_eq!(post.size, 10);
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_writers_overlay_merge() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, fh_a) = fs
            .create_file("/multi.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let ino = fs.resolve_path("/multi.txt").await?.unwrap();
        let fh_b = fs.open("/multi.txt").await?;
        fh_a.pwrite(0, b"AAAA").await?;
        fh_b.pwrite(4, b"BBBB").await?;
        // Either fd should see both writes merged via the overlay.
        assert_eq!(fh_a.pread(0, 8).await?, b"AAAABBBB");
        assert_eq!(fh_b.pread(0, 8).await?, b"AAAABBBB");
        // And getattr reflects the combined size.
        let attrs = FileSystem::getattr(&fs, ino).await?.unwrap();
        assert_eq!(attrs.size, 8);
        Ok(())
    }

    #[tokio::test]
    async fn unlink_during_pending_writes_no_orphan() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (created, file) = fs
            .create_file("/doomed.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"these bytes never reach SQLite").await?;
        // Unlink before any drain. Tier 4 hooks discard_pending here.
        fs.remove("/doomed.txt").await?;
        // Force a batched drain. If pending was not discarded, the drain
        // would hit NotFound while looking up fs_inode for the
        // unlinked ino. The drain must therefore succeed.
        fs.drain_all().await?;
        // And the row truly is gone.
        assert!(fs.stat("/doomed.txt").await?.is_none());
        let conn = fs.pool.get_connection().await?;
        let count: i64 = {
            let mut rows = conn
                .query("SELECT COUNT(*) FROM fs_data WHERE ino = ?", (created.ino,))
                .await?;
            rows.next()
                .await?
                .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
                .unwrap_or(-1)
        };
        assert_eq!(count, 0, "no orphan fs_data rows for unlinked ino");
        Ok(())
    }

    #[tokio::test]
    async fn fsync_drains_overlay_to_sqlite() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (created, file) = fs
            .create_file("/durable.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"persist me").await?;
        // Before fsync, the bytes are in the overlay; get_chunk_count drains
        // them as part of the test helper (Tier 4 sync helper change).
        // After fsync, the chunk count should be observable without any
        // helper drain prelude.
        file.fsync().await?;
        let conn = fs.pool.get_connection().await?;
        let count: i64 = {
            let mut rows = conn
                .query("SELECT size FROM fs_inode WHERE ino = ?", (created.ino,))
                .await?;
            rows.next()
                .await?
                .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
                .unwrap_or(-1)
        };
        assert_eq!(count, 10, "fsync committed pending size to fs_inode");
        Ok(())
    }

    /// Spec acceptance criterion for Tier 4:
    /// "`agentfs_batcher_drains_explicit / agentfs_batcher_enqueues` ratio
    /// drops to <0.2 (vs ~1.0 today) — confirms read path no longer triggers
    /// Explicit drains."
    ///
    /// We simulate a read-after-write workload (write, read, write, read, ...)
    /// and assert that the SDK does NOT call drain_inode_writes
    /// (Explicit drain) on every read. With Tier 4 the read path peeks the
    /// overlay; with Tier 3 each read forces drain → ratio ≈ 1.0.
    #[tokio::test]
    async fn tier_four_drains_explicit_to_enqueues_ratio_under_0_2() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (_, file) = fs
            .create_file("/ratio.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        let pre = crate::profiling::snapshot();
        let pre_enq = pre.counter("agentfs_batcher_enqueues");
        let pre_explicit = pre.counter("agentfs_batcher_drains_explicit");

        // 200 write-then-read cycles, no intervening fsync. Tier 3 would
        // drain Explicit on every read; Tier 4 must not.
        for i in 0..200u64 {
            file.pwrite(i * 4, b"abcd").await?;
            let _ = file.pread(i * 4, 4).await?;
        }

        let post = crate::profiling::snapshot();
        let enq = post.counter("agentfs_batcher_enqueues") - pre_enq;
        let explicit = post.counter("agentfs_batcher_drains_explicit") - pre_explicit;
        assert!(enq >= 200, "expected ≥200 enqueues, got {enq}");
        let ratio = explicit as f64 / enq.max(1) as f64;
        assert!(
            ratio < 0.2,
            "Tier 4 acceptance: drains_explicit/enqueues should be <0.2; \
             got {explicit}/{enq} = {ratio:.3}"
        );
        Ok(())
    }

    /// Spec escape-hatch verification: with the overlay disabled, the SDK
    /// reverts to Tier 3 drain-on-write semantics. `pwrite` should commit
    /// straight to SQLite (no batcher enqueue), and `pread` should see the
    /// value without ever consulting `peek_pending`. This locks in the kill
    /// switch the spec's risk table called for.
    #[tokio::test]
    async fn overlay_reads_flag_off_falls_back_to_drain_on_write() -> Result<()> {
        let (mut fs, _dir) = create_test_fs().await?;
        fs.overlay_reads = false;
        let (_, file) = fs
            .create_file("/escape.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite(0, b"hello world").await?;
        // Per-inode check rather than the global enqueue counter: parallel
        // tests share the profiling globals, so counter deltas race.
        let escape_ino = fs.resolve_path("/escape.bin").await?.unwrap();
        if let Some(batcher) = &fs.write_batcher {
            assert!(
                !batcher.has_pending(escape_ino),
                "with overlay_reads=false, pwrite must not enqueue"
            );
        }
        let got = file.pread(0, 11).await?;
        assert_eq!(&got, b"hello world");

        // And the file is durably in SQLite without an explicit fsync —
        // the Tier 3 contract.
        let ino = fs.resolve_path("/escape.bin").await?.unwrap();
        let conn = fs.pool.get_connection().await?;
        let size: i64 = {
            let mut rows = conn
                .query("SELECT size FROM fs_inode WHERE ino = ?", (ino,))
                .await?;
            rows.next()
                .await?
                .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
                .unwrap_or(-1)
        };
        assert_eq!(
            size, 11,
            "overlay_reads=false → SQLite has full size after pwrite"
        );
        Ok(())
    }
}
