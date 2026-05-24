use crate::error::{Error, Result};
use async_trait::async_trait;
use lru::LruCache;
use std::collections::{BTreeMap, HashMap};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, Value};

use super::{
    BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, Stats, TimeChange, WriteRange,
    DEFAULT_DIR_MODE, DEFAULT_FILE_MODE, MAX_NAME_LEN, S_IFLNK, S_IFMT, S_IFREG,
};
use crate::connection_pool::{ConnectionPool, ConnectionPoolOptions};
use crate::schema::{self, AGENTFS_SCHEMA_VERSION};

const ROOT_INO: i64 = 1;
const DEFAULT_CHUNK_SIZE: usize = 65536;
const DEFAULT_INLINE_THRESHOLD: usize = 4096;
const STORAGE_CHUNKED: i64 = 0;
const STORAGE_INLINE: i64 = 1;
const DENTRY_CACHE_MAX_SIZE: usize = 10000;
const NEGATIVE_DENTRY_CACHE_MAX_SIZE: usize = 10000;
const FILE_BACKED_MAX_CONNECTIONS: usize = 8;
const BUSY_TIMEOUT_SQL: &str = "PRAGMA busy_timeout = 5000";
const WAL_MODE_SQL: &str = "PRAGMA journal_mode = WAL";
const BASELINE_SYNCHRONOUS_SQL: &str = "PRAGMA synchronous = NORMAL";
const DURABLE_SYNCHRONOUS_SQL: &str = "PRAGMA synchronous = FULL";
const WAL_CHECKPOINT_SQL: &str = "PRAGMA wal_checkpoint(TRUNCATE)";
const FILE_BACKED_SETUP_SQL: &[&str] = &[BUSY_TIMEOUT_SQL, WAL_MODE_SQL, BASELINE_SYNCHRONOUS_SQL];
const ATTR_CACHE_MAX_SIZE: usize = 10000;
const WRITE_BATCHER_ENABLE_ENV: &str = "AGENTFS_FUSE_WRITEBACK";
const WRITE_BATCHER_MS_ENV: &str = "AGENTFS_BATCH_MS";
const WRITE_BATCHER_BYTES_ENV: &str = "AGENTFS_BATCH_BYTES";
const DEFAULT_WRITE_BATCH_MS: u64 = 5;
const DEFAULT_WRITE_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// Production connection-pool options for local file-backed AgentFS databases.
pub(crate) fn file_backed_connection_pool_options() -> ConnectionPoolOptions {
    ConnectionPoolOptions {
        max_connections: FILE_BACKED_MAX_CONNECTIONS,
        ..ConnectionPoolOptions::default().with_setup_sql(FILE_BACKED_SETUP_SQL.iter().copied())
    }
}

async fn checkpoint_wal(conn: &Connection) -> Result<()> {
    let started = if crate::profiling::is_enabled() {
        Some(Instant::now())
    } else {
        None
    };
    let mut rows = conn.query(WAL_CHECKPOINT_SQL, ()).await?;
    while rows.next().await?.is_some() {}
    if let Some(started) = started {
        crate::profiling::record_wal_checkpoint(started.elapsed());
    }
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

fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn env_duration_millis(name: &str, default_ms: u64) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(default_ms))
}

fn env_usize(name: &str, default_value: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default_value)
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

struct PendingInodeWrites {
    ranges: Vec<WriteRange>,
    pending_bytes: usize,
    first_enqueue: Instant,
    last_enqueue: Instant,
    timer_scheduled: bool,
}

impl PendingInodeWrites {
    fn new(now: Instant) -> Self {
        Self {
            ranges: Vec::new(),
            pending_bytes: 0,
            first_enqueue: now,
            last_enqueue: now,
            timer_scheduled: false,
        }
    }

    fn push_ranges(
        &mut self,
        ranges: Vec<WriteRange>,
        byte_count: usize,
        now: Instant,
    ) -> Result<()> {
        self.pending_bytes = self
            .pending_bytes
            .checked_add(byte_count)
            .ok_or_else(|| Error::Internal("batched write byte count overflow".to_string()))?;
        self.last_enqueue = now;
        self.ranges.extend(ranges);
        Ok(())
    }
}

#[derive(Default)]
struct AgentFSWriteBatcherState {
    pending: HashMap<i64, PendingInodeWrites>,
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
    state: AsyncMutex<AgentFSWriteBatcherState>,
    commit_lock: AsyncMutex<()>,
}

impl AgentFSWriteBatcher {
    fn from_env(
        pool: ConnectionPool,
        chunk_size: usize,
        inline_threshold: usize,
        attr_cache: Arc<AttrCache>,
    ) -> Self {
        Self {
            pool,
            chunk_size,
            inline_threshold,
            attr_cache,
            batch_ms: env_duration_millis(WRITE_BATCHER_MS_ENV, DEFAULT_WRITE_BATCH_MS),
            batch_bytes: env_usize(WRITE_BATCHER_BYTES_ENV, DEFAULT_WRITE_BATCH_BYTES),
            state: AsyncMutex::new(AgentFSWriteBatcherState::default()),
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
        let now = Instant::now();
        let drain_now;
        let mut schedule_timer = false;

        {
            let mut state = self.state.lock().await;
            drain_now = {
                let entry = state
                    .pending
                    .entry(ino)
                    .or_insert_with(|| PendingInodeWrites::new(now));
                entry.push_ranges(ranges, byte_count, now)?;
                crate::profiling::record_agentfs_batcher_enqueue();
                crate::profiling::record_agentfs_batcher_pending_bytes(entry.pending_bytes as u64);

                if entry.pending_bytes >= self.batch_bytes {
                    true
                } else {
                    if !entry.timer_scheduled {
                        entry.timer_scheduled = true;
                        schedule_timer = true;
                    }
                    false
                }
            };
        }

        if schedule_timer {
            self.schedule_timer_after(ino, self.batch_ms);
        }

        if drain_now {
            self.drain_inode(ino, AgentFSWriteBatchDrainReason::Bytes)
                .await?;
        }

        Ok(())
    }

    async fn drain_inode(
        self: &Arc<Self>,
        ino: i64,
        reason: AgentFSWriteBatchDrainReason,
    ) -> Result<()> {
        let _commit_guard = self.commit_lock.lock().await;
        loop {
            let batch = {
                let mut state = self.state.lock().await;
                Self::take_inode_locked(&mut state, ino)
            };

            let Some(batch) = batch else {
                return Ok(());
            };

            self.commit_batch(ino, batch, reason).await?;
        }
    }

    async fn drain_all(self: &Arc<Self>, reason: AgentFSWriteBatchDrainReason) -> Result<()> {
        let _commit_guard = self.commit_lock.lock().await;
        loop {
            let batches = {
                let mut state = self.state.lock().await;
                std::mem::take(&mut state.pending)
                    .into_iter()
                    .map(|(ino, mut batch)| {
                        batch.timer_scheduled = false;
                        (ino, batch)
                    })
                    .collect::<Vec<_>>()
            };

            if batches.is_empty() {
                return Ok(());
            }

            for (ino, batch) in batches {
                self.commit_batch(ino, batch, reason).await?;
            }
        }
    }

    async fn drain_due_timer(self: Arc<Self>, ino: i64) -> Result<()> {
        let _commit_guard = self.commit_lock.lock().await;
        let mut reschedule_after = None;
        let batch = {
            let mut state = self.state.lock().await;
            let Some(elapsed) = state
                .pending
                .get(&ino)
                .map(|entry| entry.first_enqueue.elapsed())
            else {
                return Ok(());
            };

            if elapsed >= self.batch_ms {
                Self::take_inode_locked(&mut state, ino)
            } else {
                if let Some(entry) = state.pending.get_mut(&ino) {
                    entry.timer_scheduled = true;
                }
                reschedule_after = Some(self.batch_ms - elapsed);
                None
            }
        };

        if let Some(delay) = reschedule_after {
            self.schedule_timer_after(ino, delay);
        }

        if let Some(batch) = batch {
            self.commit_batch(ino, batch, AgentFSWriteBatchDrainReason::Timer)
                .await?;
        }

        Ok(())
    }

    fn schedule_timer_after(self: &Arc<Self>, ino: i64, delay: Duration) {
        let batcher = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(error) = batcher.drain_due_timer(ino).await {
                tracing::warn!(
                    "AgentFS write batcher timer drain failed for inode {}: {}",
                    ino,
                    error
                );
            }
        });
    }

    fn take_inode_locked(
        state: &mut AgentFSWriteBatcherState,
        ino: i64,
    ) -> Option<PendingInodeWrites> {
        state.pending.remove(&ino).map(|mut batch| {
            batch.timer_scheduled = false;
            batch
        })
    }

    async fn restore_batch(self: &Arc<Self>, ino: i64, mut batch: PendingInodeWrites) {
        let mut schedule_timer = false;
        {
            let mut state = self.state.lock().await;
            if let Some(existing) = state.pending.remove(&ino) {
                batch.pending_bytes = batch.pending_bytes.saturating_add(existing.pending_bytes);
                batch.last_enqueue = existing.last_enqueue;
                batch.ranges.extend(existing.ranges);
                batch.timer_scheduled = existing.timer_scheduled;
            }
            if !batch.timer_scheduled {
                batch.timer_scheduled = true;
                schedule_timer = true;
            }
            state.pending.insert(ino, batch);
        }

        if schedule_timer {
            self.schedule_timer_after(ino, self.batch_ms);
        }
    }

    async fn commit_batch(
        self: &Arc<Self>,
        ino: i64,
        batch: PendingInodeWrites,
        reason: AgentFSWriteBatchDrainReason,
    ) -> Result<()> {
        if batch.ranges.is_empty() {
            return Ok(());
        }

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

        let result = self.commit_inode_ranges(ino, &batch.ranges).await;
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                self.restore_batch(ino, batch).await;
                Err(error)
            }
        }
    }

    async fn commit_inode_ranges(&self, ino: i64, ranges: &[WriteRange]) -> Result<()> {
        let range_refs: Vec<_> = ranges
            .iter()
            .map(|range| WriteRangeRef {
                offset: range.offset,
                data: range.data.as_slice(),
            })
            .collect();
        let normalized = normalize_write_ranges(&range_refs)?;
        if normalized.is_empty() {
            return Ok(());
        }

        crate::profiling::record_agentfs_batcher_coalesced_ranges(
            ranges.len().saturating_sub(normalized.len()) as u64,
        );

        let started = Instant::now();
        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let file = AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
            inline_threshold: self.inline_threshold,
            attr_cache: self.attr_cache.clone(),
            write_batcher: None,
        };
        let normalized_refs: Vec<_> = normalized
            .iter()
            .map(|range| WriteRangeRef {
                offset: range.offset,
                data: range.data.as_slice(),
            })
            .collect();
        let result = file
            .pwrite_ranges_inode_with_conn(&conn, &normalized_refs)
            .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.attr_cache.remove(ino);
                crate::profiling::record_agentfs_batcher_commit_latency(started.elapsed());
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
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
    /// Emits a profiling summary when the final filesystem clone is dropped.
    _profile_report: Arc<crate::profiling::ProfileReportGuard>,
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
        self.drain_writes().await?;
        let conn = self.pool.get_connection().await?;
        self.read_inode_with_conn(&conn, offset, size).await
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.drain_writes().await?;

        let conn = self.pool.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let ranges = [WriteRangeRef { offset, data }];
        let result = self.pwrite_ranges_inode_with_conn(&conn, &ranges).await;
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
        let result = self.pwrite_ranges_inode_with_conn(&conn, &range_refs).await;
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
                .drain_inode(self.ino, AgentFSWriteBatchDrainReason::Explicit)
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

    async fn pwrite_ranges_inode_with_conn(
        &self,
        conn: &Connection,
        ranges: &[WriteRangeRef<'_>],
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
        Self::from_pool_with_path(pool, db_path).await
    }

    /// Create a filesystem from a connection pool
    pub async fn from_pool(pool: ConnectionPool) -> Result<Self> {
        Self::from_pool_with_path(pool, None).await
    }

    pub(crate) async fn from_pool_with_path(
        pool: ConnectionPool,
        db_path: Option<PathBuf>,
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

        let attr_cache = Arc::new(AttrCache::new(ATTR_CACHE_MAX_SIZE));
        let write_batcher = if env_flag_enabled(WRITE_BATCHER_ENABLE_ENV) {
            Some(Arc::new(AgentFSWriteBatcher::from_env(
                pool.clone(),
                chunk_size,
                inline_threshold,
                attr_cache.clone(),
            )))
        } else {
            None
        };

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
            _profile_report: Arc::new(crate::profiling::ProfileReportGuard::new("agentfs")),
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

        // Add nanosecond timestamp columns (backward compatible migration)
        conn.execute(
            "ALTER TABLE fs_inode ADD COLUMN atime_nsec INTEGER NOT NULL DEFAULT 0",
            (),
        )
        .await
        .ok();
        conn.execute(
            "ALTER TABLE fs_inode ADD COLUMN mtime_nsec INTEGER NOT NULL DEFAULT 0",
            (),
        )
        .await
        .ok();
        conn.execute(
            "ALTER TABLE fs_inode ADD COLUMN ctime_nsec INTEGER NOT NULL DEFAULT 0",
            (),
        )
        .await
        .ok();
        conn.execute("ALTER TABLE fs_inode ADD COLUMN data_inline BLOB", ())
            .await
            .ok();
        conn.execute(
            "ALTER TABLE fs_inode ADD COLUMN storage_kind INTEGER NOT NULL DEFAULT 0",
            (),
        )
        .await
        .ok();

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
                .drain_inode(ino, AgentFSWriteBatchDrainReason::Explicit)
                .await?;
        }
        Ok(())
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
        self.drain_all().await?;
        if let Some(path) = &self.db_path {
            remove_checkpointed_sidecars(path.as_ref())?;
        }
        Ok(())
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

    /// Get file statistics without following symlinks
    pub async fn lstat(&self, path: &str) -> Result<Option<Stats>> {
        let conn = self.pool.get_connection().await?;
        let path = self.normalize_path(path);
        let ino = match self.resolve_path_with_conn(&conn, &path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };

        self.drain_inode_writes(ino).await?;
        self.getattr_with_conn(&conn, ino).await
    }

    /// Get file statistics, following symlinks
    pub async fn stat(&self, path: &str) -> Result<Option<Stats>> {
        let conn = self.pool.get_connection().await?;
        let path = self.normalize_path(path);

        // Follow symlinks with a maximum depth to prevent infinite loops
        let mut current_path = path;
        let max_symlink_depth = 40; // Standard limit for symlink following

        for _ in 0..max_symlink_depth {
            let ino = match self.resolve_path_with_conn(&conn, &current_path).await? {
                Some(ino) => ino,
                None => return Ok(None),
            };
            self.drain_inode_writes(ino).await?;

            if let Some(stats) = self.getattr_with_conn(&conn, ino).await? {
                // Check if this is a symlink
                if (stats.mode & S_IFMT) == S_IFLNK {
                    // Read the symlink target
                    let target = self
                        .readlink_with_conn(&conn, &current_path)
                        .await?
                        .ok_or(FsError::NotFound)?;

                    // Resolve target path (handle both absolute and relative paths)
                    current_path = if target.starts_with('/') {
                        target
                    } else {
                        // Relative path - resolve relative to the symlink's directory
                        let base_path = Path::new(&current_path);
                        let parent = base_path.parent().unwrap_or(Path::new("/"));
                        let joined = parent.join(&target);
                        joined.to_string_lossy().into_owned()
                    };
                    current_path = self.normalize_path(&current_path);
                    continue; // Follow the symlink
                }

                // Not a symlink, return the stats
                return Ok(Some(stats));
            } else {
                return Ok(None);
            }
        }

        // Too many symlinks
        Err(FsError::SymlinkLoop.into())
    }

    /// Get file statistics, following symlinks (using provided connection)
    async fn stat_with_conn(&self, conn: &Connection, path: &str) -> Result<Option<Stats>> {
        let path = self.normalize_path(path);

        // Follow symlinks with a maximum depth to prevent infinite loops
        let mut current_path = path;
        let max_symlink_depth = 40; // Standard limit for symlink following

        for _ in 0..max_symlink_depth {
            let ino = match self.resolve_path_with_conn(conn, &current_path).await? {
                Some(ino) => ino,
                None => return Ok(None),
            };

            if let Some(stats) = self.getattr_with_conn(conn, ino).await? {
                // Check if this is a symlink
                if (stats.mode & S_IFMT) == S_IFLNK {
                    // Read the symlink target
                    let target = self
                        .readlink_with_conn(conn, &current_path)
                        .await?
                        .ok_or(FsError::InvalidPath)?;

                    // Resolve target path (handle both absolute and relative paths)
                    current_path = if target.starts_with('/') {
                        target
                    } else {
                        // Relative path - resolve relative to the symlink's directory
                        let base_path = Path::new(&current_path);
                        let parent = base_path.parent().unwrap_or(Path::new("/"));
                        let joined = parent.join(&target);
                        joined.to_string_lossy().into_owned()
                    };
                    current_path = self.normalize_path(&current_path);
                    continue; // Follow the symlink
                }

                // Not a symlink, return the stats
                return Ok(Some(stats));
            } else {
                return Ok(None);
            }
        }

        // Too many symlinks
        Err(FsError::SymlinkLoop.into())
    }

    /// Create a directory
    pub async fn mkdir(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        // Check if already exists (single query using parent_ino we already have)
        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Create inode with default directory mode (path-based API doesn't accept mode)
        let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
        let now_secs = dur.as_secs() as i64;
        let now_nsec = dur.subsec_nanos() as i64;
        let mut stmt = conn
            .prepare_cached(
                "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime, atime_nsec, mtime_nsec, ctime_nsec)
                VALUES (?, ?, ?, 0, ?, ?, ?, ?, ?, ?) RETURNING ino",
            )
            .await?;
        let row = stmt
            .query_row((
                DEFAULT_DIR_MODE as i64,
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
        stmt.execute((name.as_str(), parent_ino, ino)).await?;

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

        // Populate dentry cache
        self.cache_dentry(parent_ino, name, ino);
        self.invalidate_parent_attr(parent_ino);

        Ok(())
    }

    /// Create a special file node (FIFO, device, socket, or regular file)
    pub async fn mknod(&self, path: &str, mode: u32, rdev: u64, uid: u32, gid: u32) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

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
        stmt.execute((name.as_str(), parent_ino, ino)).await?;

        // Increment link count
        let mut stmt = conn
            .prepare_cached("UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?")
            .await?;
        stmt.execute((ino,)).await?;

        // Populate dentry cache
        self.cache_dentry(parent_ino, name, ino);
        self.invalidate_parent_attr(parent_ino);

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
        let conn = self.pool.get_connection().await?;
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
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

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

        dentry_stmt
            .execute((name.as_str(), parent_ino, ino))
            .await?;

        txn.commit().await?;

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
        });

        Ok((stats, file))
    }

    /// Read data from a file
    pub async fn read_file(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let conn = self.pool.get_connection().await?;
        let ino = match self.resolve_path_with_conn(&conn, path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };
        drop(conn);
        self.drain_inode_writes(ino).await?;
        let conn = self.pool.get_connection().await?;

        let file = AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
            inline_threshold: self.inline_threshold,
            attr_cache: self.attr_cache.clone(),
            write_batcher: self.write_batcher.clone(),
        };
        Ok(Some(file.read_inode_with_conn(&conn, 0, u64::MAX).await?))
    }

    /// Reads from a file at a given offset.
    ///
    /// Similar to POSIX `pread`, this reads up to `size` bytes from the file
    /// starting at `offset`, without modifying any file cursor.
    ///
    /// Returns `Ok(None)` if the file does not exist.
    pub async fn pread(&self, path: &str, offset: u64, size: u64) -> Result<Option<Vec<u8>>> {
        let conn = self.pool.get_connection().await?;
        let ino = match self.resolve_path_with_conn(&conn, path).await? {
            Some(ino) => ino,
            None => return Ok(None),
        };
        drop(conn);
        self.drain_inode_writes(ino).await?;
        let conn = self.pool.get_connection().await?;

        let file = AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
            inline_threshold: self.inline_threshold,
            attr_cache: self.attr_cache.clone(),
            write_batcher: self.write_batcher.clone(),
        };
        Ok(Some(file.read_inode_with_conn(&conn, offset, size).await?))
    }

    /// Writes to a file at a given offset.
    ///
    /// Similar to POSIX `pwrite`, this writes `data` to the file starting at
    /// `offset`, without modifying any file cursor.
    ///
    /// If the offset is beyond the current file size, the file is extended with zeros.
    /// If the file does not exist, it will be created.
    pub async fn pwrite(&self, path: &str, offset: u64, data: &[u8]) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        let existing_ino = self.resolve_path_with_conn(&conn, &path).await?;
        drop(conn);
        if let Some(existing_ino) = existing_ino {
            self.drain_inode_writes(existing_ino).await?;
        }
        let conn = self.pool.get_connection().await?;

        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;

        let result: Result<(i64, bool)> = async {
            // Get or create the inode
            let (ino, created) = if let Some(ino) = self.resolve_path_with_conn(&conn, &path).await? {
                (ino, false)
            } else {
                let (now_secs, now_nsec) = current_timestamp()?;
                let mut stmt = conn
                    .prepare_cached(
                        "INSERT INTO fs_inode (mode, uid, gid, size, atime, mtime, ctime, nlink, atime_nsec, mtime_nsec, ctime_nsec, data_inline, storage_kind)
                        VALUES (?, 0, 0, 0, ?, ?, ?, 1, ?, ?, ?, ?, ?) RETURNING ino",
                    )
                    .await?;
                let row = stmt
                    .query_row((
                        DEFAULT_FILE_MODE as i64,
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

                let mut stmt = conn
                    .prepare_cached("INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)")
                    .await?;
                stmt.execute((name.as_str(), parent_ino, ino)).await?;
                (ino, true)
            };

            if data.is_empty() {
                let (now_secs, now_nsec) = current_timestamp()?;
                conn.prepare_cached("UPDATE fs_inode SET mtime = ?, mtime_nsec = ? WHERE ino = ?")
                    .await?
                    .execute((now_secs, now_nsec, ino))
                    .await?;
                return Ok((ino, created));
            }

            let file = AgentFSFile {
                pool: self.pool.clone(),
                ino,
                chunk_size: self.chunk_size,
                inline_threshold: self.inline_threshold,
                attr_cache: self.attr_cache.clone(),
                write_batcher: self.write_batcher.clone(),
            };
            let ranges = [WriteRangeRef { offset, data }];
            file.pwrite_ranges_inode_with_conn(&conn, &ranges).await?;

            Ok((ino, created))
        }
        .await;

        match result {
            Ok((ino, created)) => {
                txn.commit().await?;
                self.invalidate_attr(ino);
                if created {
                    self.cache_dentry(parent_ino, name, ino);
                    self.invalidate_parent_attr(parent_ino);
                }
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    /// Truncate a file to a specific size.
    ///
    /// This operates directly on chunks without loading the entire file into memory:
    /// - Shrinking: deletes chunks beyond new size, truncates the last chunk if needed
    /// - Extending: pads with zeros up to the new size
    pub async fn truncate(&self, path: &str, new_size: u64) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let path = self.normalize_path(path);
        let ino = self
            .resolve_path_with_conn(&conn, &path)
            .await?
            .ok_or(FsError::NotFound)?;
        drop(conn);
        self.drain_inode_writes(ino).await?;
        let conn = self.pool.get_connection().await?;

        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;
        let file = AgentFSFile {
            pool: self.pool.clone(),
            ino,
            chunk_size: self.chunk_size,
            inline_threshold: self.inline_threshold,
            attr_cache: self.attr_cache.clone(),
            write_batcher: self.write_batcher.clone(),
        };
        let result = file.truncate_inode_with_conn(&conn, new_size).await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.invalidate_attr(ino);
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
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

    /// List directory contents with full statistics (optimized batch query)
    ///
    /// Returns entries with their stats in a single JOIN query, avoiding N+1 queries.
    pub async fn readdir_plus(&self, ino: i64) -> Result<Option<Vec<DirEntry>>> {
        let conn = self.pool.get_connection().await?;
        let mut stmt = conn.prepare_cached("SELECT d.name, i.ino, i.mode, i.nlink, i.uid, i.gid, i.size, i.atime, i.mtime, i.ctime, i.rdev, i.atime_nsec, i.mtime_nsec, i.ctime_nsec
            FROM fs_dentry d
            JOIN fs_inode i ON d.ino = i.ino
            WHERE d.parent_ino = ?
            ORDER BY d.name"
        ).await?;
        // Single JOIN query to get all entry names and their stats (including link count)
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

            let nlink = row
                .get_value(3)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(1) as u32;

            let stats = Stats {
                ino: entry_ino,
                mode: row
                    .get_value(2)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32,
                nlink,
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

    /// Create a symbolic link with the specified ownership
    pub async fn symlink(&self, target: &str, linkpath: &str, uid: u32, gid: u32) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let linkpath = self.normalize_path(linkpath);
        let components = self.split_path(&linkpath);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        // Get parent directory
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        // Check if entry already exists (single query using parent_ino we already have)
        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Create inode for symlink
        let dur = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
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
                mode, uid, gid, size, now_secs, now_secs, now_secs, now_nsec, now_nsec, now_nsec,
            ))
            .await?;

        // Get the newly created inode
        let ino = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0);

        // Store symlink target
        conn.execute(
            "INSERT INTO fs_symlink (ino, target) VALUES (?, ?)",
            (ino, target),
        )
        .await?;

        // Create directory entry
        conn.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
            (name.as_str(), parent_ino, ino),
        )
        .await?;

        // Increment link count
        conn.execute(
            "UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?",
            (ino,),
        )
        .await?;

        // Populate dentry cache
        self.cache_dentry(parent_ino, name, ino);
        self.invalidate_parent_attr(parent_ino);

        Ok(())
    }

    /// Create a hard link
    ///
    /// Creates a new directory entry `newpath` that refers to the same inode as `oldpath`.
    /// Both paths will share the same file data and metadata (except for the name).
    /// The link count (nlink) of the inode is incremented.
    pub async fn link(&self, oldpath: &str, newpath: &str) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let oldpath = self.normalize_path(oldpath);
        let newpath = self.normalize_path(newpath);
        let components = self.split_path(&newpath);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        // Resolve old path to get its inode
        let ino = self
            .resolve_path_with_conn(&conn, &oldpath)
            .await?
            .ok_or(FsError::NotFound)?;

        // Check if source is a directory (hard links to directories are not allowed)
        let mut rows = conn
            .query("SELECT mode FROM fs_inode WHERE ino = ?", (ino,))
            .await?;

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

        // Get parent directory of new path
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        // Check if new path already exists (single query using parent_ino we already have)
        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Create directory entry pointing to the same inode
        conn.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES (?, ?, ?)",
            (name.as_str(), parent_ino, ino),
        )
        .await?;

        // Increment link count
        conn.execute(
            "UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?",
            (ino,),
        )
        .await?;

        // Populate dentry cache
        self.cache_dentry(parent_ino, name, ino);
        self.invalidate_parent_attr(parent_ino);
        self.invalidate_attr(ino);

        Ok(())
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
        let conn = self.pool.get_connection().await?;
        let path = self.normalize_path(path);
        let components = self.split_path(&path);

        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }

        let ino = self
            .resolve_path_with_conn(&conn, &path)
            .await?
            .ok_or(FsError::NotFound)?;

        if ino == ROOT_INO {
            return Err(FsError::RootOperation.into());
        }

        // Get stats to check if it's a directory
        let stats = self
            .stat_with_conn(&conn, &path)
            .await?
            .ok_or(FsError::NotFound)?;

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

        // Get parent directory and name
        let parent_path = if components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", components[..components.len() - 1].join("/"))
        };

        let parent_ino = self
            .resolve_path_with_conn(&conn, &parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        let name = components.last().unwrap();

        // Delete the specific directory entry (not all entries pointing to this inode)
        let mut stmt = conn
            .prepare_cached("DELETE FROM fs_dentry WHERE parent_ino = ? AND name = ?")
            .await?;
        stmt.execute((parent_ino, name.as_str())).await?;

        // Invalidate cache for this entry
        self.invalidate_dentry(parent_ino, name);
        self.invalidate_parent_attr(parent_ino);
        self.invalidate_attr(ino);

        // Decrement link count
        let mut stmt = conn
            .prepare_cached("UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?")
            .await?;
        stmt.execute((ino,)).await?;

        // If removing a directory, decrement parent nlink (removed dir's ".." link)
        if stats.is_directory() {
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
        }

        // Check if this was the last link to the inode
        let link_count = self.get_link_count(&conn, ino).await?;
        if link_count == 0 {
            // Manually handle cascading deletes since we don't use foreign keys
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

        self.invalidate_dentry(parent_ino, name);
        self.invalidate_parent_attr(parent_ino);
        self.invalidate_attr(ino);
        self.cache_negative_dentry(parent_ino, name);
        Ok(())
    }

    /// Change file ownership
    ///
    /// Changes the user and/or group ownership of a file.
    /// Pass None for uid or gid to leave that value unchanged.
    pub async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        if uid.is_none() && gid.is_none() {
            return Ok(());
        }

        let conn = self.pool.get_connection().await?;

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

        values.push(Value::Integer(ino));
        let sql = format!("UPDATE fs_inode SET {} WHERE ino = ?", updates.join(", "));
        conn.execute(&sql, values).await?;
        self.invalidate_attr(ino);

        Ok(())
    }

    /// Rename/move a file or directory.
    ///
    /// This operation is atomic - either all changes succeed or none do.
    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let from_path = self.normalize_path(from);
        let to_path = self.normalize_path(to);

        // Cannot rename root
        if from_path == "/" {
            return Err(FsError::RootOperation.into());
        }

        // Get source inode
        let src_ino = self
            .resolve_path_with_conn(&conn, &from_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Get source stats to check if it's a directory
        let src_stats = self
            .stat_with_conn(&conn, &from_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Prevent renaming a directory into its own subtree (would create a cycle)
        if src_stats.is_directory() {
            let from_prefix = format!("{}/", from_path);
            if to_path.starts_with(&from_prefix) || to_path == from_path {
                return Err(FsError::InvalidRename.into());
            }
        }

        // Parse source path to get parent and name
        let from_components = self.split_path(&from_path);
        let src_name = from_components.last().ok_or(FsError::InvalidPath)?;
        let src_parent_path = if from_components.len() == 1 {
            "/".to_string()
        } else {
            format!(
                "/{}",
                from_components[..from_components.len() - 1].join("/")
            )
        };
        let src_parent_ino = self
            .resolve_path_with_conn(&conn, &src_parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Parse destination path to get parent and name
        let to_components = self.split_path(&to_path);
        if to_components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let dst_name = to_components.last().unwrap();
        let dst_parent_path = if to_components.len() == 1 {
            "/".to_string()
        } else {
            format!("/{}", to_components[..to_components.len() - 1].join("/"))
        };
        let dst_parent_ino = self
            .resolve_path_with_conn(&conn, &dst_parent_path)
            .await?
            .ok_or(FsError::NotFound)?;

        // Clone strings for use inside the transaction closure
        let src_name = src_name.clone();
        let dst_name = dst_name.clone();

        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;

        let result: Result<Option<i64>> = async {
            let mut replaced_dst_ino = None;
            // Check if destination exists (inside transaction for atomicity)
            if let Some(dst_ino) = self.resolve_path_with_conn(&conn, &to_path).await? {
                replaced_dst_ino = Some(dst_ino);
                let dst_stats = self.stat_with_conn(&conn, &to_path).await?.ok_or(FsError::NotFound)?;

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
                stmt.execute((dst_parent_ino, dst_name.as_str())).await?;

                // Decrement link count
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?")
                    .await?;
                stmt.execute((dst_ino,)).await?;

                // Clean up destination inode if no more links
                let link_count = self.get_link_count(&conn, dst_ino).await?;
                if link_count == 0 {
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
            stmt.execute((
                dst_parent_ino,
                dst_name.as_str(),
                src_parent_ino,
                src_name.as_str(),
            ))
            .await?;

            // If renaming a directory across parents, adjust parent nlink counts
            if src_stats.is_directory() && src_parent_ino != dst_parent_ino {
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink - 1 WHERE ino = ?")
                    .await?;
                stmt.execute((src_parent_ino,)).await?;

                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET nlink = nlink + 1 WHERE ino = ?")
                    .await?;
                stmt.execute((dst_parent_ino,)).await?;
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
            stmt.execute((now_secs, now_secs, now_nsec, now_nsec, src_parent_ino)).await?;

            // Update destination parent directory timestamps
            if dst_parent_ino != src_parent_ino {
                let mut stmt = conn
                    .prepare_cached("UPDATE fs_inode SET mtime = ?, ctime = ?, mtime_nsec = ?, ctime_nsec = ? WHERE ino = ?")
                    .await?;
                stmt.execute((now_secs, now_secs, now_nsec, now_nsec, dst_parent_ino)).await?;
            }

            Ok(replaced_dst_ino)
        }
        .await;

        match result {
            Ok(replaced_dst_ino) => {
                txn.commit().await?;

                // Invalidate cache for source and destination
                self.invalidate_dentry(src_parent_ino, &src_name);
                self.invalidate_dentry(dst_parent_ino, &dst_name);
                self.invalidate_attr(src_ino);
                self.invalidate_parent_attr(src_parent_ino);
                self.invalidate_parent_attr(dst_parent_ino);
                if let Some(dst_ino) = replaced_dst_ino {
                    self.invalidate_attr(dst_ino);
                }

                // Add exact post-rename namespace state to the caches.
                if src_parent_ino != dst_parent_ino || src_name != dst_name {
                    self.cache_negative_dentry(src_parent_ino, &src_name);
                }
                self.cache_dentry(dst_parent_ino, &dst_name, src_ino);

                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
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
    /// Note: The path parameter is ignored since all data is in a single database.
    pub async fn fsync(&self, _path: &str) -> Result<()> {
        self.drain_all().await?;
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
        }))
    }

    /// Get the number of chunks for a given inode (for testing)
    #[cfg(test)]
    async fn get_chunk_count(&self, ino: i64) -> Result<i64> {
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
        self.drain_inode_writes(child_ino).await?;

        // Get stats for the child inode
        let mut stmt = conn
            .prepare_cached("SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec FROM fs_inode WHERE ino = ?")
            .await?;
        let mut rows = stmt.query((child_ino,)).await?;

        if let Some(row) = rows.next().await? {
            let stats = Self::build_stats_from_row(&row)?;
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
        self.drain_inode_writes(ino).await?;
        let conn = self.pool.get_connection().await?;
        self.getattr_with_conn(&conn, ino).await
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
        self.drain_inode_writes(ino).await?;
        let conn = self.pool.get_connection().await?;

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
            .prepare_cached("UPDATE fs_inode SET mode = ?, ctime = ?, ctime_nsec = ? WHERE ino = ?")
            .await?;
        stmt.execute((new_mode as i64, now_secs, now_nsec, ino))
            .await?;
        self.invalidate_attr(ino);

        Ok(())
    }

    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        if uid.is_none() && gid.is_none() {
            return Ok(());
        }
        self.drain_inode_writes(ino).await?;

        let conn = self.pool.get_connection().await?;

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
        self.invalidate_attr(ino);

        Ok(())
    }

    async fn utimens(&self, ino: i64, atime: TimeChange, mtime: TimeChange) -> Result<()> {
        self.drain_inode_writes(ino).await?;
        let conn = self.pool.get_connection().await?;

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

        if updates.is_empty() {
            return Ok(());
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
        self.invalidate_attr(ino);

        Ok(())
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

        // Populate dentry cache
        self.cache_dentry(parent_ino, name, ino);
        self.invalidate_parent_attr(parent_ino);

        let stats = Stats {
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
        };
        self.cache_attr(stats.clone());
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
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;

        // Check if already exists
        if self.lookup_child(&conn, parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

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

        dentry_stmt.execute((name, parent_ino, ino)).await?;

        // Update parent directory ctime and mtime
        conn.execute(
            "UPDATE fs_inode SET ctime = ?, mtime = ?, ctime_nsec = ?, mtime_nsec = ? WHERE ino = ?",
            (now_secs, now_secs, now_nsec, now_nsec, parent_ino),
        )
        .await?;

        txn.commit().await?;

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

        // Populate dentry cache
        self.cache_dentry(parent_ino, name, ino);
        self.invalidate_parent_attr(parent_ino);

        let stats = Stats {
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
        };
        self.cache_attr(stats.clone());
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
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;

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
                mode, uid, gid, size, now_secs, now_secs, now_secs, now_nsec, now_nsec, now_nsec,
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

        // Populate dentry cache
        self.cache_dentry(parent_ino, name, ino);
        self.invalidate_parent_attr(parent_ino);

        let stats = Stats {
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
        };
        self.cache_attr(stats.clone());
        Ok(stats)
    }

    async fn unlink(&self, parent_ino: i64, name: &str) -> Result<()> {
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;

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

        // Invalidate cache
        self.invalidate_dentry(parent_ino, name);
        self.invalidate_parent_attr(parent_ino);
        self.invalidate_attr(ino);

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

        // Check if this was the last link to the inode
        let link_count = self.get_link_count(&conn, ino).await?;
        if link_count == 0 {
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

        self.invalidate_dentry(parent_ino, name);
        self.invalidate_parent_attr(parent_ino);
        self.invalidate_attr(ino);
        self.cache_negative_dentry(parent_ino, name);
        Ok(())
    }

    async fn rmdir(&self, parent_ino: i64, name: &str) -> Result<()> {
        if name.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;

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

        // Invalidate cache
        self.invalidate_dentry(parent_ino, name);
        self.invalidate_parent_attr(parent_ino);
        self.invalidate_attr(ino);

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

        self.invalidate_dentry(parent_ino, name);
        self.invalidate_parent_attr(parent_ino);
        self.invalidate_attr(ino);
        self.cache_negative_dentry(parent_ino, name);
        Ok(())
    }

    async fn link(&self, ino: i64, newparent_ino: i64, newname: &str) -> Result<Stats> {
        if newname.len() > MAX_NAME_LEN {
            return Err(FsError::NameTooLong.into());
        }
        let conn = self.pool.get_connection().await?;

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

        // Populate dentry cache
        self.cache_dentry(newparent_ino, newname, ino);
        self.invalidate_parent_attr(newparent_ino);
        self.invalidate_attr(ino);

        // Return updated stats
        self.getattr_with_conn(&conn, ino)
            .await?
            .ok_or(FsError::NotFound.into())
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

                // Clean up destination inode if no more links
                let link_count = self.get_link_count(&conn, dst_ino).await?;
                if link_count == 0 {
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

    async fn forget(&self, ino: i64, _nlookup: u64) {
        if let Err(error) = AgentFS::drain_inode_writes(self, ino).await {
            tracing::warn!(
                "AgentFS write batcher forget drain failed for inode {}: {}",
                ino,
                error
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // Turso 0.5.x reports SQLite's standard numeric value for NORMAL.
    const TURSO_OBSERVED_SYNCHRONOUS_NORMAL: i64 = 1;

    async fn create_test_fs() -> Result<(AgentFS, tempfile::TempDir)> {
        let dir = tempdir()?;
        let db_path = dir.path().join("test.db");
        let fs = AgentFS::new(db_path.to_str().unwrap()).await?;
        Ok((fs, dir))
    }

    fn cached_attr(fs: &AgentFS, ino: i64) -> Option<Stats> {
        fs.attr_cache.get(ino)
    }

    fn negative_cached(fs: &AgentFS, parent_ino: i64, name: &str) -> bool {
        fs.negative_dentry_cache.contains(parent_ino, name)
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
        fs.truncate("/overwrite.txt", 0).await?;
        let file = fs.open("/overwrite.txt").await?;
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
        fs.truncate("/grow.txt", 0).await?;
        let file = fs.open("/grow.txt").await?;
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
        assert_eq!(fs.inline_threshold(), 4096);

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

        assert_eq!(value, "4096");

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
        assert_eq!(options.setup_sql[0], BUSY_TIMEOUT_SQL);
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

        fs.fsync("/").await?;

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
    async fn test_pread_basic() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write a file with known content
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Read from the beginning
        let result = fs.pread("/test.txt", 0, 10).await?.unwrap();
        assert_eq!(result, &data[0..10]);

        // Read from the middle
        let result = fs.pread("/test.txt", 50, 20).await?.unwrap();
        assert_eq!(result, &data[50..70]);

        // Read from near the end
        let result = fs.pread("/test.txt", 90, 10).await?.unwrap();
        assert_eq!(result, &data[90..100]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_past_eof() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let data: Vec<u8> = (0..50).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Read starting past EOF should return empty
        let result = fs.pread("/test.txt", 100, 10).await?.unwrap();
        assert!(result.is_empty());

        // Read that extends past EOF should return only available data
        let result = fs.pread("/test.txt", 40, 20).await?.unwrap();
        assert_eq!(result, &data[40..50]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_nonexistent_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let result = fs.pread("/nonexistent.txt", 0, 10).await?;
        assert!(result.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_across_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create data spanning multiple chunks
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Read across chunk boundary
        let start = chunk_size - 10;
        let result = fs.pread("/test.txt", start as u64, 20).await?.unwrap();
        assert_eq!(result, &data[start..start + 20]);

        // Read spanning multiple chunks
        let start = chunk_size / 2;
        let size = chunk_size * 2;
        let result = fs
            .pread("/test.txt", start as u64, size as u64)
            .await?
            .unwrap();
        assert_eq!(result, &data[start..start + size]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_basic() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write initial data
        let data: Vec<u8> = vec![0; 100];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Overwrite in the middle
        fs.pwrite("/test.txt", 50, &[1, 2, 3, 4, 5]).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[50..55], &[1, 2, 3, 4, 5]);
        assert_eq!(&result[0..50], &vec![0u8; 50][..]);
        assert_eq!(&result[55..100], &vec![0u8; 45][..]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_extend_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Write initial data
        let data: Vec<u8> = vec![1; 50];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Write past EOF - should extend with zeros
        fs.pwrite("/test.txt", 100, &[2, 2, 2, 2, 2]).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 105);
        assert_eq!(&result[0..50], &vec![1u8; 50][..]);
        assert_eq!(&result[50..100], &vec![0u8; 50][..]);
        assert_eq!(&result[100..105], &[2, 2, 2, 2, 2]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_creates_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // pwrite to a non-existent file should create it
        fs.pwrite("/new.txt", 0, &[1, 2, 3]).await?;

        let result = fs.read_file("/new.txt").await?.unwrap();
        assert_eq!(result, &[1, 2, 3]);

        Ok(())
    }

    #[tokio::test]
    async fn test_pwrite_across_chunks() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create initial data spanning multiple chunks
        let data: Vec<u8> = vec![0; chunk_size * 3];
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Write across chunk boundary
        let write_data: Vec<u8> = (0..20).collect();
        let start = chunk_size - 10;
        fs.pwrite("/test.txt", start as u64, &write_data).await?;

        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(&result[start..start + 20], &write_data[..]);

        // Verify surrounding data is unchanged
        assert_eq!(&result[0..start], &vec![0u8; start][..]);
        assert_eq!(
            &result[start + 20..],
            &vec![0u8; chunk_size * 3 - start - 20][..]
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_pread_pwrite_roundtrip() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create a file
        let initial: Vec<u8> = (0..(chunk_size * 2)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &initial).await?;

        // Write some data at various offsets
        let patches = vec![
            (0u64, vec![0xAAu8; 10]),
            (chunk_size as u64 - 5, vec![0xBB; 10]),
            (chunk_size as u64 * 2 - 1, vec![0xCC; 1]),
        ];

        for (offset, data) in &patches {
            fs.pwrite("/test.txt", *offset, data).await?;
        }

        // Verify with pread
        for (offset, expected) in &patches {
            let result = fs
                .pread("/test.txt", *offset, expected.len() as u64)
                .await?
                .unwrap();
            assert_eq!(&result, expected);
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
        std::env::set_var(WRITE_BATCHER_ENABLE_ENV, "1");
        std::env::set_var(WRITE_BATCHER_MS_ENV, "60000");
        std::env::set_var(WRITE_BATCHER_BYTES_ENV, "1048576");

        let (fs, _dir) = create_test_fs().await?;
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

    // ─────────────────────────────────────────────────────────────
    // Truncate Tests
    // ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_truncate_to_zero() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file with some data
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Truncate to zero
        fs.truncate("/test.txt", 0).await?;

        // Verify file is empty
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert!(result.is_empty());

        // Verify stat shows size 0
        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(stats.size, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_smaller_within_chunk() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file smaller than chunk size
        let data: Vec<u8> = (0..100).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Truncate to 50 bytes
        fs.truncate("/test.txt", 50).await?;

        // Verify data is truncated correctly
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 50);
        assert_eq!(result, &data[..50]);

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_across_chunk_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create a file spanning multiple chunks
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Truncate to middle of second chunk
        let new_size = chunk_size + chunk_size / 2;
        fs.truncate("/test.txt", new_size as u64).await?;

        // Verify data
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), new_size);
        assert_eq!(result, &data[..new_size]);

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_extend_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a small file
        let data: Vec<u8> = (0..50).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Extend to 100 bytes
        fs.truncate("/test.txt", 100).await?;

        // Verify size increased
        let stats = fs.stat("/test.txt").await?.unwrap();
        assert_eq!(stats.size, 100);

        // Original data should be preserved, rest should be zeros (sparse)
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), 100);
        assert_eq!(&result[..50], &data[..]);

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_nonexistent_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Truncate non-existent file should fail
        let result = fs.truncate("/nonexistent.txt", 100).await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_truncate_at_chunk_boundary() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let chunk_size = fs.chunk_size();

        // Create a file spanning multiple chunks
        let data: Vec<u8> = (0..(chunk_size * 3)).map(|i| (i % 256) as u8).collect();
        let (_, file) = fs.create_file("/test.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, &data).await?;

        // Truncate exactly at chunk boundary
        fs.truncate("/test.txt", chunk_size as u64).await?;

        // Verify
        let result = fs.read_file("/test.txt").await?.unwrap();
        assert_eq!(result.len(), chunk_size);
        assert_eq!(result, &data[..chunk_size]);

        Ok(())
    }

    // ─────────────────────────────────────────────────────────────
    // Rename Tests
    // ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_rename_file_same_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file
        let data = b"hello world";
        let (_, file) = fs.create_file("/old.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, data).await?;

        // Rename it
        fs.rename("/old.txt", "/new.txt").await?;

        // Old path should not exist
        assert!(fs.stat("/old.txt").await?.is_none());

        // New path should exist with same data
        let result = fs.read_file("/new.txt").await?.unwrap();
        assert_eq!(result, data);

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_file_to_different_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create directory and file
        fs.mkdir("/subdir", 0, 0).await?;
        let data = b"test data";
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, data).await?;

        // Move file to subdirectory
        fs.rename("/file.txt", "/subdir/file.txt").await?;

        // Old path should not exist
        assert!(fs.stat("/file.txt").await?.is_none());

        // New path should exist
        let result = fs.read_file("/subdir/file.txt").await?.unwrap();
        assert_eq!(result, data);

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_overwrite_existing_file() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create two files
        let (_, file) = fs.create_file("/src.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"source").await?;
        let (_, file) = fs.create_file("/dst.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"destination").await?;

        // Rename src to dst (overwrites dst)
        fs.rename("/src.txt", "/dst.txt").await?;

        // Only dst should exist with src's content
        assert!(fs.stat("/src.txt").await?.is_none());
        let result = fs.read_file("/dst.txt").await?.unwrap();
        assert_eq!(result, b"source");

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_directory() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create directory with a file inside
        fs.mkdir("/olddir", 0, 0).await?;
        let (_, file) = fs
            .create_file("/olddir/file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;

        // Rename directory
        fs.rename("/olddir", "/newdir").await?;

        // Old path should not exist
        assert!(fs.stat("/olddir").await?.is_none());

        // New path should exist and contain the file
        assert!(fs.stat("/newdir").await?.is_some());
        let result = fs.read_file("/newdir/file.txt").await?.unwrap();
        assert_eq!(result, b"content");

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_directory_into_own_subtree_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create nested directories
        fs.mkdir("/parent", 0, 0).await?;
        fs.mkdir("/parent/child", 0, 0).await?;

        // Try to rename parent into its child - should fail
        let result = fs.rename("/parent", "/parent/child/parent").await;
        assert!(result.is_err());

        // Original structure should be intact
        assert!(fs.stat("/parent").await?.is_some());
        assert!(fs.stat("/parent/child").await?.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_root_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Try to rename root - should fail
        let result = fs.rename("/", "/newroot").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_to_root_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;

        // Try to rename to root - should fail
        let result = fs.rename("/file.txt", "/").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_nonexistent_source_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Try to rename non-existent file
        let result = fs.rename("/nonexistent.txt", "/new.txt").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_overwrite_nonempty_directory_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create source directory and target directory with content
        fs.mkdir("/src", 0, 0).await?;
        fs.mkdir("/dst", 0, 0).await?;
        let (_, file) = fs
            .create_file("/dst/file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"content").await?;

        // Try to rename src to dst (dst is not empty) - should fail
        let result = fs.rename("/src", "/dst").await;
        assert!(result.is_err());

        // Both directories should still exist
        assert!(fs.stat("/src").await?.is_some());
        assert!(fs.stat("/dst").await?.is_some());
        assert!(fs.stat("/dst/file.txt").await?.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_file_to_directory_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file and an empty directory
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;
        fs.mkdir("/dir", 0, 0).await?;

        // Try to rename file over directory - should fail
        let result = fs.rename("/file.txt", "/dir").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_directory_to_file_fails() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a directory and a file
        fs.mkdir("/dir", 0, 0).await?;
        let (_, file) = fs.create_file("/file.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;

        // Try to rename directory over file - should fail
        let result = fs.rename("/dir", "/file.txt").await;
        assert!(result.is_err());

        Ok(())
    }

    #[tokio::test]
    async fn test_rename_updates_ctime() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        // Create a file
        let (_, file) = fs.create_file("/old.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, b"data").await?;
        let stats_before = fs.stat("/old.txt").await?.unwrap();

        // Small delay to ensure time changes
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Rename it
        fs.rename("/old.txt", "/new.txt").await?;

        // ctime should be updated
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
        fs.symlink("/target.txt", "/link.txt", 0, 0).await?;
        let link_stats = fs.lstat("/link.txt").await?.unwrap();

        // chmod the symlink (should work on the symlink inode)
        fs.chmod(link_stats.ino, 0o755).await?;

        let stats = fs.lstat("/link.txt").await?.unwrap();
        assert!(stats.is_symlink(), "Should still be a symlink");

        Ok(())
    }
}
