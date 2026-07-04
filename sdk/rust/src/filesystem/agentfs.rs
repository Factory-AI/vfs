use crate::error::{Error, Result};
use async_trait::async_trait;
use lru::LruCache;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, Value};

#[cfg(test)]
use super::DEFAULT_FILE_MODE;
use super::{
    BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, Stats, TimeChange, WriteRange,
    DEFAULT_DIR_MODE, MAX_NAME_LEN, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG,
};
#[cfg(test)]
use crate::config::BatcherConfig;
use crate::config::{CoreConfig, Geometry, DEFAULT_CHUNK_SIZE, DEFAULT_INLINE_THRESHOLD};
use crate::connection_pool::{ConnectionPool, ConnectionPoolOptions};
use crate::schema;

mod batcher;
mod lifecycle;
mod store;
use batcher::{
    AgentFSWriteBatcher, BatcherDrain, BatcherPendingView, Drain, EnqueueOutcome,
    PendingGeneration, PendingTimeChange, PendingView,
};
pub use lifecycle::ReapHook;
use lifecycle::{Lifecycle, OpenInodeGuard};
use store::WriteRangeRef;

#[cfg(test)]
use store::{dense_after_inline_write_batch, normalize_write_ranges, NormalizedWriteRange};

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
const MEMORY_SETUP_SQL: &[&str] = &[TEMP_STORE_MEMORY_SQL];
const ATTR_CACHE_MAX_SIZE: usize = 10000;

/// Production connection-pool options for local file-backed AgentFS databases.
pub(crate) fn file_backed_connection_pool_options() -> ConnectionPoolOptions {
    ConnectionPoolOptions {
        max_connections: FILE_BACKED_MAX_CONNECTIONS,
        ..ConnectionPoolOptions::default().with_setup_sql(FILE_BACKED_SETUP_SQL.iter().copied())
    }
}

/// Production connection-pool options for local in-memory AgentFS databases.
pub(crate) fn memory_connection_pool_options() -> ConnectionPoolOptions {
    ConnectionPoolOptions::single_connection().with_setup_sql(MEMORY_SETUP_SQL.iter().copied())
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
    /// Synchronous pending view, safe to consult while a pooled connection is held.
    pending_view: Option<BatcherPendingView>,
    /// Async drain/enqueue surface. Code holding a pooled connection must not
    /// have access to this surface.
    write_drain: Option<BatcherDrain>,
    /// Concrete batcher retained only for white-box unit tests.
    #[cfg(test)]
    write_batcher: Option<Arc<AgentFSWriteBatcher>>,
    /// Tier 4 escape hatch: when false (`AGENTFS_OVERLAY_READS=0`), the SDK
    /// behaves like Tier 3 — every pwrite drains, every pread drains,
    /// `merge_pending_view` is a no-op. ON by default.
    overlay_reads: bool,
    /// Typed runtime configuration captured once when the filesystem opens.
    core_config: Arc<CoreConfig>,
    /// Open-handle registry, deferred orphan queue, and reap hooks.
    lifecycle: Arc<Lifecycle>,
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
    pending_view: Option<BatcherPendingView>,
    write_drain: Option<BatcherDrain>,
    /// Same semantics as the field on `AgentFS`; cloned at open time so the
    /// hot read/write path doesn't have to chase an extra indirection.
    overlay_reads: bool,
    /// Present for user-visible handles so unlink defers inode reaping while
    /// they live. This remains optional until lifecycle extraction flattens
    /// the handle construction API.
    _open_guard: Option<OpenInodeGuard>,
}

fn current_timestamp() -> Result<(i64, i64)> {
    let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok((dur.as_secs() as i64, dur.subsec_nanos() as i64))
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

impl AgentFS {
    /// Create a new filesystem
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let pool = if db_path == ":memory:" {
            ConnectionPool::with_options(db, memory_connection_pool_options())
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
        config: CoreConfig,
    ) -> Result<Self> {
        Self::from_pool_with_path_config_and_reap_hooks(pool, db_path, config, Vec::new()).await
    }

    pub(crate) async fn from_pool_with_path_config_and_reap_hooks(
        pool: ConnectionPool,
        db_path: Option<PathBuf>,
        mut config: CoreConfig,
        reap_hooks: Vec<Arc<dyn ReapHook>>,
    ) -> Result<Self> {
        let conn = pool.get_connection().await?;

        // Initialize or migrate schema first. The schema module owns DDL and
        // stamps SQLite's user-version inside the DDL transaction.
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
        let (pending_view, write_drain, _write_batcher) = if core_config.batcher.enabled {
            let invalidate = {
                let attr_cache = Arc::clone(&attr_cache);
                Arc::new(move |ino| attr_cache.remove(ino)) as batcher::Invalidate
            };
            let batcher = Arc::new(AgentFSWriteBatcher::from_config(
                pool.clone(),
                chunk_size,
                inline_threshold,
                invalidate,
                &core_config.batcher,
            ));
            let (pending_view, write_drain) = AgentFSWriteBatcher::split(&batcher);
            (Some(pending_view), Some(write_drain), Some(batcher))
        } else {
            (None, None, None)
        };

        let lifecycle = Arc::new(Lifecycle::default());
        for hook in reap_hooks {
            lifecycle.register_reap_hook(hook);
        }
        lifecycle.sweep_mount_orphans(&conn).await?;

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
            pending_view,
            write_drain,
            #[cfg(test)]
            write_batcher: _write_batcher,
            overlay_reads,
            core_config,
            lifecycle,
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

    pub fn register_reap_hook(&self, hook: Arc<dyn ReapHook>) {
        self.lifecycle.register_reap_hook(hook);
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
        schema::ensure_current(conn).await?;

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

    fn pending_generation(&self, ino: i64) -> Option<PendingGeneration> {
        self.pending_view
            .as_ref()
            .map(|view| view.pending_generation(ino))
    }

    fn cache_attr_if_pending_generation(
        &self,
        stats: Stats,
        generation: Option<PendingGeneration>,
    ) {
        if let (Some(view), Some(generation)) = (&self.pending_view, generation) {
            if view.pending_generation(stats.ino) != generation {
                return;
            }
        }
        self.cache_attr(stats);
    }

    pub(crate) fn invalidate_attr(&self, ino: i64) {
        self.attr_cache.remove(ino);
    }

    /// Drain pending batched writes for one inode.
    pub async fn drain_inode_writes(&self, ino: i64) -> Result<()> {
        if let Some(drain) = &self.write_drain {
            drain.drain_inode(ino).await?;
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
        if let Some(drain) = &self.write_drain {
            drain.mark_times_explicit(ino);
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
        let Some(view) = &self.pending_view else {
            return;
        };
        view.merge_into_stats(ino, stats);
    }

    /// Drain all pending batched writes for this AgentFS instance.
    pub async fn drain_all(&self) -> Result<()> {
        if let Some(drain) = &self.write_drain {
            drain.drain_all().await?;
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
        let reaped = self.lifecycle.process_deferred_reaps(&self.pool).await?;
        for ino in reaped {
            self.discard_pending_after_reap(ino);
            self.invalidate_attr(ino);
        }
        Ok(())
    }

    async fn reap_inode_with_conn(&self, conn: &Connection, ino: i64) -> Result<bool> {
        self.lifecycle.reap_inode_with_conn(conn, ino).await
    }

    fn discard_pending_after_reap(&self, ino: i64) {
        if let Some(drain) = &self.write_drain {
            drain.discard_pending(ino);
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
        store::link_count(conn, ino).await
    }

    /// Get file attributes by inode using an existing connection
    async fn getattr_with_conn(&self, conn: &Connection, ino: i64) -> Result<Option<Stats>> {
        if let Some(stats) = self.attr_cache.get(ino) {
            return Ok(Some(stats));
        }

        let generation = self.pending_generation(ino);
        if let Some(mut stats) = store::getattr(conn, ino).await? {
            self.merge_pending_view(ino, Some(&mut stats));
            self.cache_attr_if_pending_generation(stats.clone(), generation);
            Ok(Some(stats))
        } else {
            Ok(None)
        }
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
        if let Some(mode) = store::mode(conn, ino).await? {
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
            pending_view: self.pending_view.clone(),
            write_drain: self.write_drain.clone(),
            overlay_reads: self.overlay_reads,
            _open_guard: Some(self.lifecycle.guard(ino)),
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
        crate::profiling::record_readdir();
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
        crate::profiling::record_readdir_plus();
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
            let removed = link_count == 0 && !self.lifecycle.defer_reap_if_open(ino);
            if removed {
                self.reap_inode_with_conn(&conn, ino).await?;
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
                    self.discard_pending_after_reap(ino);
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

        let result: Result<(Option<i64>, Option<i64>)> = async {
            let mut replaced_dst_ino = None;
            let mut reaped_dst_ino = None;

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
                    && self.reap_inode_with_conn(&conn, dst_ino).await?
                {
                    reaped_dst_ino = Some(dst_ino);
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

            Ok((replaced_dst_ino, reaped_dst_ino))
        }
        .await;

        match result {
            Ok((replaced_dst_ino, reaped_dst_ino)) => {
                txn.commit().await?;
                if let Some(dst_ino) = reaped_dst_ino {
                    // Tier Four: see public `rename` for rationale — drop
                    // pending batched writes for the deleted inode so a
                    // subsequent batched drain doesn't INSERT into a
                    // missing fs_inode row.
                    self.discard_pending_after_reap(dst_ino);
                }

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
    use std::time::Duration;
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

    fn assert_normalized_ranges(actual: &[NormalizedWriteRange], expected: &[(u64, &[u8])]) {
        assert_eq!(actual.len(), expected.len());
        for (range, (offset, data)) in actual.iter().zip(expected.iter()) {
            assert_eq!(range.offset, *offset);
            assert_eq!(range.data, *data);
        }
    }

    #[test]
    fn store_characterization_range_normalization_merges_overlaps_in_order() -> Result<()> {
        let ranges = [
            WriteRangeRef {
                offset: 4,
                data: b"CCCC",
            },
            WriteRangeRef {
                offset: 0,
                data: b"aaaaaa",
            },
            WriteRangeRef {
                offset: 2,
                data: b"ZZ",
            },
            WriteRangeRef {
                offset: 8,
                data: b"!",
            },
            WriteRangeRef {
                offset: 12,
                data: b"",
            },
        ];

        let normalized = normalize_write_ranges(&ranges)?;
        assert_normalized_ranges(&normalized, &[(0, b"aaZZaaCC!")]);
        Ok(())
    }

    #[test]
    fn store_characterization_range_normalization_keeps_sparse_gaps() -> Result<()> {
        let ranges = [
            WriteRangeRef {
                offset: 0,
                data: b"ab",
            },
            WriteRangeRef {
                offset: 5,
                data: b"xy",
            },
        ];

        let normalized = normalize_write_ranges(&ranges)?;
        assert_normalized_ranges(&normalized, &[(0, b"ab"), (5, b"xy")]);
        assert!(!dense_after_inline_write_batch(0, 7, &normalized));

        let mut bridged_refs: Vec<_> = normalized
            .iter()
            .map(|range| WriteRangeRef {
                offset: range.offset,
                data: range.data.as_slice(),
            })
            .collect();
        bridged_refs.push(WriteRangeRef {
            offset: 2,
            data: b"345",
        });

        let bridged = normalize_write_ranges(&bridged_refs)?;
        assert_normalized_ranges(&bridged, &[(0, b"ab345xy")]);
        assert!(dense_after_inline_write_batch(0, 7, &bridged));
        Ok(())
    }

    #[test]
    fn store_characterization_range_normalization_rejects_offset_overflow() {
        let ranges = [WriteRangeRef {
            offset: u64::MAX,
            data: b"x",
        }];

        match normalize_write_ranges(&ranges) {
            Ok(_) => panic!("overflowing write range should fail"),
            Err(error) => assert!(matches!(error, Error::Internal(_))),
        }
    }

    fn assert_corrupt_error(error: Error, expected_column: &str) {
        match error {
            Error::Fs(FsError::Corrupt(message)) => assert!(
                message.contains(expected_column),
                "corruption message {message:?} should mention {expected_column:?}"
            ),
            other => panic!("expected corrupt row error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn row_decode_corruption_returns_error() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let (created, file) = fs
            .create_file("/corrupt.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"hello").await?;
        file.drain_writes().await?;

        let conn = fs.pool.get_connection().await?;
        conn.execute(
            "UPDATE fs_inode SET mode = ? WHERE ino = ?",
            (Value::Text("not-an-integer".to_string()), created.ino),
        )
        .await?;
        fs.invalidate_attr(created.ino);

        assert_corrupt_error(
            FileSystem::getattr(&fs, created.ino).await.unwrap_err(),
            "mode",
        );
        assert_corrupt_error(
            FileSystem::lookup(&fs, ROOT_INO, "corrupt.txt")
                .await
                .unwrap_err(),
            "mode",
        );
        assert_corrupt_error(
            FileSystem::readdir_plus(&fs, ROOT_INO).await.unwrap_err(),
            "mode",
        );

        let corrupt_dir = FileSystem::mkdir(&fs, ROOT_INO, "corrupt-dir", 0o755, 0, 0).await?;
        conn.execute(
            "UPDATE fs_inode SET mode = ? WHERE ino = ?",
            (Value::Text("not-an-integer".to_string()), corrupt_dir.ino),
        )
        .await?;
        fs.invalidate_attr(corrupt_dir.ino);
        assert_corrupt_error(
            FileSystem::readdir(&fs, corrupt_dir.ino).await.unwrap_err(),
            "mode",
        );

        conn.execute(
            "UPDATE fs_inode SET mode = ?, storage_kind = ? WHERE ino = ?",
            (
                (S_IFREG | 0o644) as i64,
                Value::Text("not-an-integer".to_string()),
                created.ino,
            ),
        )
        .await?;
        fs.invalidate_attr(created.ino);

        assert_corrupt_error(file.pread(0, 5).await.unwrap_err(), "storage_kind");
        Ok(())
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
    async fn read_file_follows_terminal_symlink() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;

        let (_, file) = fs
            .create_file("/target.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"target contents").await?;
        file.fsync().await?;
        FileSystem::symlink(&fs, ROOT_INO, "target.link", "target.txt", 0, 0).await?;

        let link = FileSystem::lookup(&fs, ROOT_INO, "target.link")
            .await?
            .unwrap();
        assert!(link.is_symlink());
        assert_eq!(
            fs.read_file("/target.link").await?.unwrap(),
            b"target contents"
        );

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
                "SELECT value FROM fs_config WHERE key = ?",
                (schema::CONFIG_SCHEMA_VERSION_KEY,),
            )
            .await?;
        let row = rows
            .next()
            .await?
            .expect("schema version config should exist");
        let value = row
            .get_value(0)
            .ok()
            .and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .expect("schema version should be a text value");

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

        let err = match schema::ensure_current(&conn).await {
            Ok(_) => panic!("non-duplicate schema DDL errors must propagate"),
            Err(err) => err,
        };
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("fs_inode"),
            "error should preserve the failed DDL target, got: {err_msg}"
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
                    (?, '0.5'),
                    (?, '16384')",
                (
                    schema::CONFIG_SCHEMA_VERSION_KEY,
                    schema::CONFIG_INLINE_THRESHOLD_KEY,
                ),
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
                "SELECT value FROM fs_config WHERE key = ?",
                (schema::CONFIG_SCHEMA_VERSION_KEY,),
            )
            .await?;
        let version: String = rows
            .next()
            .await?
            .expect("schema version config should exist")
            .get(0)?;
        assert_eq!(version, schema::AGENTFS_SCHEMA_VERSION);

        Ok(())
    }

    #[tokio::test]
    async fn test_v04_database_migrates_to_current_on_open() -> Result<()> {
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
                "INSERT INTO fs_config (key, value) VALUES (?, '0.4')",
                (schema::CONFIG_SCHEMA_VERSION_KEY,),
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

        let agent =
            crate::AgentFS::open(crate::AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await?;
        let conn = agent.get_connection().await?;
        assert_eq!(
            schema::detect_schema_version(&conn).await?,
            Some(schema::CURRENT)
        );

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
    async fn test_memory_agentfs_connections_use_temp_store_memory() -> Result<()> {
        let agentfs = crate::AgentFS::open(crate::AgentFSOptions::ephemeral()).await?;

        let conn = agentfs.get_connection().await?;
        assert_eq!(read_pragma_i64(&conn, "PRAGMA temp_store").await, 2);
        drop(conn);

        let core_agentfs = AgentFS::new(":memory:").await?;
        let core_conn = core_agentfs.pool.get_connection().await?;
        assert_eq!(read_pragma_i64(&core_conn, "PRAGMA temp_store").await, 2);

        Ok(())
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

    #[derive(Default)]
    struct FailOnceReapHook {
        failed: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl ReapHook for FailOnceReapHook {
        async fn on_reap(&self, conn: &Connection, ino: i64) -> Result<()> {
            conn.execute("INSERT INTO reap_hook_probe (ino) VALUES (?)", (ino,))
                .await?;
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(Error::Internal("intentional reap hook failure".to_string()));
            }
            Ok(())
        }
    }

    async fn count_probe_rows(fs: &AgentFS, ino: i64) -> Result<i64> {
        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM reap_hook_probe WHERE ino = ?", (ino,))
            .await?;
        Ok(rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1))
    }

    #[tokio::test]
    async fn lifecycle_reap_hook_fires_atomically() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        let conn = fs.pool.get_connection().await?;
        conn.execute("CREATE TABLE reap_hook_probe (ino INTEGER PRIMARY KEY)", ())
            .await?;
        drop(conn);

        fs.register_reap_hook(Arc::new(FailOnceReapHook::default()));

        let (stats, file) = fs
            .create_file("/hooked.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let ino = stats.ino;
        file.pwrite(0, b"hooked").await?;

        FileSystem::unlink(&fs, ROOT_INO, "hooked.bin").await?;
        assert_eq!(file.pread(0, 6).await?, b"hooked");
        drop(file);

        let err = fs
            .process_deferred_reaps()
            .await
            .expect_err("first hook invocation should fail");
        assert!(
            err.to_string().contains("intentional reap hook failure"),
            "unexpected reap hook error: {err}"
        );
        assert_eq!(
            count_rows(&fs, "fs_inode", ino).await?,
            1,
            "failed hook must roll back the inode deletion"
        );
        assert_eq!(
            count_probe_rows(&fs, ino).await?,
            0,
            "hook writes must be in the same transaction as the reap"
        );

        fs.process_deferred_reaps().await?;
        assert_eq!(count_probe_rows(&fs, ino).await?, 1);
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
    async fn lifecycle_reaps_open_unlink_and_mount_sweep() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("lifecycle.db");
        let db_path = db_path.to_str().unwrap();

        let deferred_ino = {
            let fs = AgentFS::new(db_path).await?;
            let (stats, file) = fs
                .create_file("/deferred.bin", DEFAULT_FILE_MODE, 0, 0)
                .await?;
            file.pwrite(0, b"deferred").await?;
            FileSystem::unlink(&fs, ROOT_INO, "deferred.bin").await?;
            assert!(fs.resolve_path("/deferred.bin").await?.is_none());
            assert_eq!(file.pread(0, 8).await?, b"deferred");
            assert_eq!(file.fstat().await?.nlink, 0);
            drop(file);
            fs.process_deferred_reaps().await?;
            stats.ino
        };

        let crashed_ino = {
            let fs = AgentFS::new(db_path).await?;
            let (stats, file) = fs
                .create_file("/crashed.bin", DEFAULT_FILE_MODE, 0, 0)
                .await?;
            file.pwrite(0, b"crashed").await?;
            file.drain_writes().await?;
            FileSystem::unlink(&fs, ROOT_INO, "crashed.bin").await?;
            std::mem::forget(file);
            stats.ino
        };

        let fs = AgentFS::new(db_path).await?;
        for ino in [deferred_ino, crashed_ino] {
            assert_eq!(
                count_rows(&fs, "fs_inode", ino).await?,
                0,
                "reaped inode {ino} should not remain in fs_inode"
            );
            assert_eq!(
                count_rows(&fs, "fs_data", ino).await?,
                0,
                "reaped inode {ino} should not leave fs_data rows"
            );
            assert_eq!(
                count_rows(&fs, "fs_symlink", ino).await?,
                0,
                "reaped inode {ino} should not leave fs_symlink rows"
            );
        }

        let conn = fs.pool.get_connection().await?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM fs_inode WHERE nlink = 0", ())
            .await?;
        let nlink_zero = rows
            .next()
            .await?
            .and_then(|r| r.get_value(0).ok().and_then(|v| v.as_integer().copied()))
            .unwrap_or(-1);
        assert_eq!(nlink_zero, 0, "mount sweep should leave no nlink=0 rows");

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
        let config = BatcherConfig {
            window: std::time::Duration::from_secs(batch_ms_secs),
            inode_bytes: batch_bytes,
            global_bytes: batch_global_bytes,
            ..BatcherConfig::default()
        };
        Arc::new(AgentFSWriteBatcher::from_config(
            fs.pool.clone(),
            fs.chunk_size,
            fs.inline_threshold,
            {
                let attr_cache = Arc::clone(&fs.attr_cache);
                Arc::new(move |ino| attr_cache.remove(ino))
            },
            &config,
        ))
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
            .enqueue_for_test(
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
            .enqueue_for_test(
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
            .enqueue_for_test(
                sa.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'x'; 50],
                }],
            )
            .await?;
        assert_eq!(
            batcher.total_pending_bytes(),
            50,
            "write below the global cap must remain in the overlay"
        );

        // Truncating into the pending range shrinks the tracked total.
        batcher.truncate_pending(sa.ino, 20);
        assert_eq!(
            batcher.total_pending_bytes(),
            20,
            "truncate_pending must shrink the running total to the kept prefix"
        );

        // Write to inode B crosses the cap (20 + 50 >= 64): a full batched drain
        // commits every pending inode and resets the running total to zero.
        batcher
            .enqueue_for_test(
                sb.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'y'; 50],
                }],
            )
            .await?;
        assert_eq!(
            batcher.total_pending_bytes(),
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
            .enqueue_for_test(
                sa.ino,
                vec![WriteRange {
                    offset: 0,
                    data: vec![b'z'; 100],
                }],
            )
            .await?;
        assert_eq!(batcher.total_pending_bytes(), 100);

        batcher.discard_pending(sa.ino);
        assert_eq!(
            batcher.total_pending_bytes(),
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
    async fn trait_rename_corrupt_dentry_cycle_returns_invalid_path() -> Result<()> {
        let (fs, _dir) = create_test_fs().await?;
        fs.mkdir("/src", 0, 0).await?;
        fs.mkdir("/cycle-a", 0, 0).await?;
        fs.mkdir("/cycle-a/cycle-b", 0, 0).await?;

        let cycle_a = fs.stat("/cycle-a").await?.unwrap().ino;
        let cycle_b = fs.stat("/cycle-a/cycle-b").await?.unwrap().ino;
        let conn = fs.pool.get_connection().await?;
        conn.execute(
            "UPDATE fs_dentry SET parent_ino = ? WHERE ino = ?",
            (cycle_b, cycle_a),
        )
        .await?;
        fs.invalidate_dentry(ROOT_INO, "cycle-a");
        fs.invalidate_dentry(cycle_a, "cycle-b");

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            FileSystem::rename(&fs, ROOT_INO, "src", cycle_a, "moved"),
        )
        .await
        .expect("corrupt dentry cycles must error instead of looping forever");

        assert!(matches!(result, Err(Error::Fs(FsError::InvalidPath))));
        assert!(
            FileSystem::lookup(&fs, ROOT_INO, "src").await?.is_some(),
            "failed guarded rename must preserve the source directory"
        );
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
    async fn stat_coherent_before_drain() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (created, file) = fs.create_file("/grow.txt", DEFAULT_FILE_MODE, 0, 0).await?;
        let pre = FileSystem::getattr(&fs, created.ino).await?.unwrap();
        assert_eq!(pre.size, 0);
        file.pwrite(0, b"abcdefghij").await?;
        assert!(
            fs.write_batcher
                .as_ref()
                .is_some_and(|batcher| batcher.has_pending(created.ino)),
            "long-window write should still be pending before stat"
        );
        let conn = fs.pool.get_connection().await?;
        let sqlite = store::getattr(&conn, created.ino)
            .await?
            .expect("file should exist");
        assert_eq!(
            sqlite.size, 0,
            "SQLite row should not be drained before the stat coherence check"
        );
        drop(conn);
        let post = FileSystem::getattr(&fs, created.ino).await?.unwrap();
        assert_eq!(post.size, 10);
        Ok(())
    }

    #[tokio::test]
    async fn versioned_cache_fill_skips_stale_attrs_after_drain() -> Result<()> {
        let (fs, _dir) = create_test_fs_with_config(test_config_with_long_batch_window()).await?;
        let (created, file) = fs
            .create_file("/stale-cache.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;

        file.pwrite(0, b"pending").await?;

        let conn = fs.pool.get_connection().await?;
        let generation = fs.pending_generation(created.ino);
        let mut stale_stats = store::getattr(&conn, created.ino)
            .await?
            .expect("file should exist");
        self::AgentFS::merge_pending_view(&fs, created.ino, Some(&mut stale_stats));
        drop(conn);

        file.drain_writes().await?;
        fs.cache_attr_if_pending_generation(stale_stats, generation);

        assert!(
            cached_attr(&fs, created.ino).is_none(),
            "a cache fill captured before a racing drain must be skipped"
        );

        let current = FileSystem::getattr(&fs, created.ino)
            .await?
            .expect("file should still exist");
        assert_eq!(current.size, 7);
        assert_eq!(file.pread(0, 16).await?, b"pending");
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
