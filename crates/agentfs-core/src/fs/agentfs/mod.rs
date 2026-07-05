//! AgentFS core facade and module spine.
//!
//! This module owns the shared `AgentFS` state, connection setup, path
//! resolution helpers, cache invalidation hooks, and lifecycle spine. Focused
//! child modules implement caches, file handles, bulk import, path delegates,
//! and the canonical `FileSystem` trait implementation.

#[cfg(test)]
use crate::error::Error;
use crate::error::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use turso::{Builder, Connection, Value};

use super::{
    BoxedFile, FilesystemStats, FsError, Stats, DEFAULT_DIR_MODE, MAX_NAME_LEN, S_IFDIR, S_IFLNK,
    S_IFMT, S_IFREG,
};
#[cfg(test)]
use super::{FileSystem, TimeChange, WriteRange, DEFAULT_FILE_MODE};
#[cfg(test)]
use crate::config::BatcherConfig;
use crate::config::{CoreConfig, Geometry, DEFAULT_CHUNK_SIZE, DEFAULT_INLINE_THRESHOLD};
use crate::pool::{ConnectionPool, DatabaseType, PoolOptions};
use crate::schema;

mod batcher;
mod caches;
mod file;
mod fs;
mod import;
mod lifecycle;
mod path_api;
pub(in crate::fs) mod store;

use batcher::{
    AgentFSWriteBatcher, BatcherDrain, BatcherPendingView, Drain, PendingGeneration, PendingView,
};
use caches::{AttrCache, DentryCache, NegativeDentryCache};
pub use file::AgentFSFile;
pub use import::{ImportEntry, ImportOptions, ImportSession, ImportedEntry};
pub use lifecycle::ReapHook;
use lifecycle::{Lifecycle, OpenInodeGuard};

#[cfg(test)]
use store::{
    dense_after_inline_write_batch, normalize_write_ranges, NormalizedWriteRange, WriteRangeRef,
};

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
pub(crate) fn file_backed_connection_pool_options() -> PoolOptions {
    PoolOptions {
        max_connections: FILE_BACKED_MAX_CONNECTIONS,
        ..PoolOptions::default().with_setup_sql(FILE_BACKED_SETUP_SQL.iter().copied())
    }
}

/// Production connection-pool options for local in-memory AgentFS databases.
pub(crate) fn memory_connection_pool_options() -> PoolOptions {
    PoolOptions::single_connection().with_setup_sql(MEMORY_SETUP_SQL.iter().copied())
}

async fn checkpoint_wal(conn: &Connection) -> Result<()> {
    let _checkpoint_timer =
        crate::telemetry::timer(&crate::telemetry::CORE_COUNTERS.wal_checkpoint);
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
    /// Bulk-import transaction sizes observed by white-box tests.
    #[cfg(test)]
    import_commit_sizes: Arc<Mutex<Vec<usize>>>,
    /// Tier 4 escape hatch: when false (`AGENTFS_OVERLAY_READS=0`), the SDK
    /// behaves like Tier 3 — every pwrite drains, every pread drains,
    /// `merge_pending_view` is a no-op. ON by default.
    overlay_reads: bool,
    /// Typed runtime configuration captured once when the filesystem opens.
    core_config: Arc<CoreConfig>,
    /// Open-handle registry, deferred orphan queue, and reap hooks.
    lifecycle: Arc<Lifecycle>,
}

fn current_timestamp() -> Result<(i64, i64)> {
    let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok((dur.as_secs() as i64, dur.subsec_nanos() as i64))
}

impl AgentFS {
    /// Create a new filesystem
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let pool = if db_path == ":memory:" {
            ConnectionPool::with_options(DatabaseType::Local(db), memory_connection_pool_options())
        } else {
            ConnectionPool::with_options(
                DatabaseType::Local(db),
                file_backed_connection_pool_options(),
            )
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
            #[cfg(test)]
            import_commit_sizes: Arc::new(Mutex::new(Vec::new())),
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

    pub fn partial_origin_policy(&self) -> crate::fs::PartialOriginPolicy {
        self.core_config.partial_origin
    }

    pub fn register_reap_hook(&self, hook: Arc<dyn ReapHook>) -> bool {
        self.lifecycle.register_reap_hook(hook)
    }

    #[cfg(test)]
    pub(crate) fn reap_hook_count(&self) -> usize {
        self.lifecycle.reap_hook_count()
    }

    /// Get a database connection from the pool
    pub async fn get_connection(&self) -> Result<crate::pool::PooledConnection> {
        self.pool.get_connection().await
    }

    /// Get the connection pool
    pub fn get_pool(&self) -> ConnectionPool {
        self.pool.clone()
    }

    /// Initialize the database schema
    pub async fn initialize_schema(conn: &Connection) -> Result<()> {
        schema::require_current(conn).await?;

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
            found_ino = Some(
                row.get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .ok_or_else(|| {
                        FsError::Corrupt(format!(
                            "invalid ino for dentry {parent_ino}/{name}: expected integer"
                        ))
                    })?,
            );
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
        let reaped = self
            .lifecycle
            .process_deferred_reaps(&self.pool, |ino| {
                self.discard_pending_for_reaped_inode(ino);
            })
            .await?;
        for ino in reaped {
            self.invalidate_attr(ino);
        }
        Ok(())
    }

    async fn reap_inode_with_conn(&self, conn: &Connection, ino: i64) -> Result<bool> {
        self.lifecycle.reap_inode_with_conn(conn, ino).await
    }

    /// Drop batcher state for an inode that is being reaped. Inline unlink /
    /// rename-replace callers invoke this only after their metadata
    /// transaction commits, so a reap-hook rollback leaves pending writes
    /// intact with the still-live inode. Deferred reaps call it before the
    /// transaction opens because the inode is already nlink=0 and invisible.
    fn discard_pending_for_reaped_inode(&self, ino: i64) {
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
        crate::telemetry::record_path_resolution(components.len() as u64);
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
                crate::telemetry::record_negative_lookup();
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
                    .ok_or_else(|| FsError::Corrupt("invalid ino: expected integer".to_string()))?;

                // Populate cache
                self.cache_dentry(current_ino, &component, child_ino);
                current_ino = child_ino;
            } else {
                crate::telemetry::record_negative_lookup();
                self.cache_negative_dentry(current_ino, &component);
                return Ok(None);
            }
        }

        Ok(Some(current_ino))
    }

    /// Resolve a path to its parent directory inode and final component name.
    ///
    /// This is the canonical parent/name resolver backing every path-based
    /// mutation helper; external path consumers (e.g. the CLI MCP server)
    /// must use it rather than re-deriving parent inodes.
    pub async fn resolve_parent_and_name(&self, path: &str) -> Result<(i64, String)> {
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

#[cfg(test)]
// Keep the extracted test body byte-for-byte; this feature is a pure move.
#[rustfmt::skip]
#[path = "tests.rs"]
mod agentfs_tests;
