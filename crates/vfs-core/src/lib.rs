//! Vfs core: the SQLite-backed virtual filesystem engine.
//!
//! This is the only externally consumed crate. It owns the storage engine
//! (chunk/inline layout per docs/SPEC.md), the write batcher, inode
//! lifecycle and reap hooks, the overlay layer (whiteouts, origin tracking,
//! the partial-origin policy), scoped host-FS reads, schema authority
//! (`user_version` migrations plus the integrity battery), the typed config
//! system parsed at the crate edge (`config::EnvReader`), the telemetry
//! registry with its single report sink, and the `semantics` facade
//! (access, durability, handles) that the transport adapter crates build on.
//!
//! Owned invariants:
//!
//! - All virtual filesystem state lives in the single Vfs SQLite
//!   database. Sandboxed writes never touch the host filesystem; overlay
//!   reads are scoped to the configured read-only base directory.
//! - Buffered (volatile-ack) writes are acceleration state only: durable
//!   acks (`AckDurability::Committed`, commit barriers, shutdown finalize)
//!   return only after the bytes are committed to SQLite, metadata reads
//!   merge pending state, and deletions discard it.
//! - Errors are typed (`FsError` at the trait, `Error` above it); row
//!   decoding never fabricates defaults for corrupt data.
//! - Environment variables are read only inside `config`; everything
//!   downstream receives values.

pub mod config;
pub mod error;
pub mod fs;
pub mod kv;
pub mod mounts;
pub mod options;
pub mod pool;
pub mod schema;
pub mod semantics;
pub mod session;
pub mod telemetry;
pub mod toolcalls;

use error::{Error, Result};
use pool::{ConnectionPool, DatabaseType, PoolOptions, PooledConnection};
use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};
use turso::{Builder, EncryptionOpts, Value};

// Re-export turso sync types for CLI usage
pub use turso::sync::{DatabaseSyncStats, PartialBootstrapStrategy, PartialSyncOpts};

// Re-export filesystem types
pub use config::{
    BatcherConfig, CoreConfig, EnvReader, Geometry, DEFAULT_JOURNAL_RETENTION_OPS,
    DEFAULT_WRITE_BATCH_BYTES, DEFAULT_WRITE_BATCH_GLOBAL_BYTES, DEFAULT_WRITE_BATCH_MS,
    DEFAULT_WRITE_BATCH_TXN_BYTES, DEFAULT_WRITE_BATCH_TXN_INODES,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use fs::HostFS;
pub use fs::{
    journal_gc, BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, HistoryStatus,
    HistoryTarget, ImportEntry, ImportOptions, ImportSession, ImportedEntry, OverlayFS,
    PartialOriginMode, PartialOriginPolicy, ReconstructionInfo, SnapshotHeader, Stats, TimeChange,
    ValidatedHistoryTarget, WriteRange, DEFAULT_DIR_MODE, DEFAULT_FILE_MODE,
    DEFAULT_PARTIAL_ORIGIN_THRESHOLD_BYTES, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT,
    S_IFREG, S_IFSOCK,
};
pub use kv::KvStore;
pub use mounts::{get_mounts, Mount};
pub use options::{vfs_dir, EncryptionConfig, SyncOptions, VfsOptions};
pub use schema::{SchemaVersion, CURRENT, VFS_SCHEMA_VERSION};
pub use semantics::{AckDurability, Semantics, WriteReceipt};
pub use session::{SessionMetadata, SessionStatusMetadata};
pub use toolcalls::{ToolCall, ToolCallStats, ToolCallStatus, ToolCalls};

/// The main Vfs SDK struct
///
/// This provides a unified interface to the filesystem, key-value store,
/// and tool calls tracking backed by a SQLite database.
pub struct Vfs {
    pool: ConnectionPool,
    sync_db: Option<turso::sync::Database>,
    pub kv: KvStore,
    pub fs: fs::Vfs,
    pub tools: ToolCalls,
}

impl Vfs {
    /// Open an immutable, digest-addressed Vfs artifact without modifying its
    /// SQLite file family.
    ///
    /// This constructor uses Turso's strict read-only open flags, applies only
    /// connection-local non-writing pragmas, validates the existing schema,
    /// and omits write batching and mount-orphan recovery. Its filesystem
    /// lifecycle barriers are no-ops, so reads and shutdown neither checkpoint
    /// nor remove `-wal`/`-shm` sidecars.
    ///
    /// Turso 0.5.3 caches databases by canonical path without considering open
    /// flags. Therefore the same path must never be opened writable in this
    /// process. Artifact paths passed here must be immutable and
    /// digest-addressed, and this API must be their only in-process open path.
    pub async fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(Error::DatabaseNotFound(path.display().to_string()));
        }
        let path_str = path
            .to_str()
            .ok_or_else(|| Error::InvalidUtf8Path(path.display().to_string()))?;
        let db = Builder::new_local(path_str).read_only(true).build().await?;
        let pool = ConnectionPool::with_options(
            DatabaseType::Local(db),
            fs::vfs::read_only_file_backed_connection_pool_options(),
        );
        let fs =
            fs::Vfs::from_read_only_pool(pool.clone(), path.to_path_buf(), CoreConfig::from_env())
                .await?;

        Ok(Self {
            pool: pool.clone(),
            sync_db: None,
            kv: KvStore::from_read_only_pool(pool.clone()),
            fs,
            tools: ToolCalls::from_read_only_pool(pool),
        })
    }

    /// Open a Vfs instance
    ///
    /// # Arguments
    /// * `options` - Configuration options (use Default::default() for ephemeral)
    ///
    /// # Examples
    /// ```no_run
    /// use vfs_core::{Vfs, VfsOptions};
    ///
    /// # async fn example() -> vfs_core::error::Result<()> {
    /// // Persistent storage
    /// let agent = Vfs::open(VfsOptions::with_id("my-agent")).await?;
    ///
    /// // Ephemeral in-memory
    /// let agent = Vfs::open(VfsOptions::ephemeral()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(options: VfsOptions) -> Result<Self> {
        let core_config = options
            .core_config
            .clone()
            .unwrap_or_else(CoreConfig::from_env);

        // Validate base directory if provided
        if let Some(ref path) = options.base {
            if !path.exists() {
                return Err(Error::BaseDirectoryNotFound(path.display().to_string()));
            }
            if !path.is_dir() {
                return Err(Error::NotADirectory(path.display().to_string()));
            }
        }

        // Encryption is not supported with sync
        if options.encryption.is_some() && options.sync.remote_url.is_some() {
            return Err(Error::EncryptionNotSupported(
                "Local encryption is not supported with cloud sync".to_string(),
            ));
        }

        let db_path = options.db_path()?;
        let meta_path = format!("{db_path}-info");

        // Determine if this is a synced database:
        // 1. If sync.remote_url is set, create a new synced database
        // 2. If {path}-info file exists, open as existing synced database
        // 3. Otherwise, open as local database (with optional encryption via URI)
        let (sync_db, pool) = if let Some(remote_url) = options.sync.remote_url {
            // Creating a new synced database
            let mut builder =
                turso::sync::Builder::new_remote(&db_path).with_remote_url(remote_url);
            if let Some(auth_token) = options.sync.auth_token {
                builder = builder.with_auth_token(auth_token);
            }
            if let Some(partial_sync) = options.sync.partial_sync {
                builder = builder.with_partial_sync_opts_experimental(partial_sync);
            }
            let db = builder.build().await?;
            let pool = ConnectionPool::with_options(
                DatabaseType::Sync(db.clone()),
                PoolOptions::single_connection(),
            );
            (Some(db), pool)
        } else if std::fs::exists(&meta_path).unwrap_or(false) {
            let mut builder = turso::sync::Builder::new_remote(&db_path);
            if let Some(auth_token) = options.sync.auth_token {
                builder = builder.with_auth_token(auth_token);
            }
            let db = builder.build().await?;
            let pool = ConnectionPool::with_options(
                DatabaseType::Sync(db.clone()),
                PoolOptions::single_connection(),
            );
            (Some(db), pool)
        } else {
            let db = if let Some(ref enc_config) = options.encryption {
                Builder::new_local(&db_path)
                    .experimental_encryption(true)
                    .with_encryption(EncryptionOpts {
                        cipher: enc_config.cipher.clone(),
                        hexkey: enc_config.hex_key.clone(),
                    })
                    .build()
                    .await?
            } else {
                Builder::new_local(&db_path).build().await?
            };
            let pool = if db_path == ":memory:" {
                ConnectionPool::with_options(
                    DatabaseType::Local(db),
                    fs::vfs::memory_connection_pool_options(),
                )
            } else {
                ConnectionPool::with_options(
                    DatabaseType::Local(db),
                    fs::vfs::file_backed_connection_pool_options(),
                )
            };
            (None, pool)
        };

        // Initialize or normalize schema for existing databases before any
        // schema-owned callers read or write sidecar sections. Old schema
        // versions are rejected here; upgrades are `vfs migrate`'s job.
        let mut conn = pool.get_connection().await?;
        if let Err(error) = schema::require_current(&conn).await {
            conn.mark_unhealthy_if_fatal(&error);
            return Err(error);
        }
        drop(conn);

        // Initialize overlay schema if base is provided
        if let Some(base_path) = options.base {
            let canonical_base = std::fs::canonicalize(base_path)?;
            let base_path_str = canonical_base.to_string_lossy().to_string();
            let mut conn = pool.get_connection().await?;
            if let Err(error) = OverlayFS::init_schema(&conn, &base_path_str).await {
                conn.mark_unhealthy_if_fatal(&error);
                return Err(error);
            }
        }

        let db_path_for_fs = if sync_db.is_none() && db_path != ":memory:" {
            Some(PathBuf::from(&db_path))
        } else {
            None
        };

        Self::build_from_pool_and_path(pool, sync_db, db_path_for_fs, core_config, Vec::new()).await
    }

    async fn build_from_pool_and_config(
        pool: ConnectionPool,
        sync_db: Option<turso::sync::Database>,
        core_config: CoreConfig,
    ) -> Result<Self> {
        let kv = KvStore::from_pool(pool.clone()).await?;
        let fs = fs::Vfs::from_pool_with_path_config_and_reap_hooks(
            pool.clone(),
            None,
            core_config,
            Vec::new(),
        )
        .await?;
        let tools = ToolCalls::from_pool(pool.clone()).await?;

        Ok(Self {
            pool,
            sync_db,
            kv,
            fs,
            tools,
        })
    }

    async fn build_from_pool_and_path(
        pool: ConnectionPool,
        sync_db: Option<turso::sync::Database>,
        db_path: Option<PathBuf>,
        core_config: CoreConfig,
        reap_hooks: Vec<Arc<dyn fs::vfs::ReapHook>>,
    ) -> Result<Self> {
        let kv = KvStore::from_pool(pool.clone()).await?;
        let fs = fs::Vfs::from_pool_with_path_config_and_reap_hooks(
            pool.clone(),
            db_path,
            core_config,
            reap_hooks,
        )
        .await?;
        let tools = ToolCalls::from_pool(pool.clone()).await?;

        Ok(Self {
            pool,
            sync_db,
            kv,
            fs,
            tools,
        })
    }
    /// Create a new Vfs instance (deprecated, use `open` instead)
    ///
    /// # Arguments
    /// * `db_path` - Path to the SQLite database file (use ":memory:" for in-memory database)
    #[deprecated(since = "0.2.0", note = "Use Vfs::open with VfsOptions instead")]
    pub async fn new(db_path: &str) -> Result<Self> {
        let db = Builder::new_local(db_path).build().await?;
        let pool = if db_path == ":memory:" {
            ConnectionPool::with_options(
                DatabaseType::Local(db),
                fs::vfs::memory_connection_pool_options(),
            )
        } else {
            ConnectionPool::with_options(
                DatabaseType::Local(db),
                fs::vfs::file_backed_connection_pool_options(),
            )
        };
        Self::build_from_pool_and_config(pool, None, CoreConfig::from_env()).await
    }

    /// Get a connection from the pool
    pub async fn get_connection(&self) -> Result<PooledConnection> {
        self.pool.get_connection().await
    }

    /// Get the connection pool
    pub fn get_pool(&self) -> ConnectionPool {
        self.pool.clone()
    }

    /// Capture an immutable root at the acknowledged journal head.
    pub async fn capture_root(&self, reason: &str) -> Result<SnapshotHeader> {
        self.fs.drain_all().await?;
        let conn = self.pool.get_connection().await?;
        let root = fs::history::capture_root(&conn, reason).await?;
        self.fs.journal_ctx().forget_chunks();
        Ok(root)
    }

    /// Return the retained replay range and complete transaction targets.
    pub async fn history_status(&self) -> Result<HistoryStatus> {
        let conn = self.pool.get_connection().await?;
        fs::history::status(&conn).await
    }

    /// Validate that `target_seq` is a complete, reconstructible history target.
    pub async fn validate_target(&self, target_seq: i64) -> Result<ValidatedHistoryTarget> {
        let conn = self.pool.get_connection().await?;
        fs::history::validate_target(&conn, target_seq).await
    }

    /// Replace the filesystem state in a private staged database with a target.
    ///
    /// The path must name a caller-owned staging copy that is not open
    /// elsewhere in this process. This constructor deliberately bypasses the
    /// normal writable-open epoch transition: replay validates and transforms
    /// the staged copy's already-recorded durable history markers.
    pub async fn reconstruct_to(
        staging_path: impl AsRef<Path>,
        target_seq: i64,
    ) -> Result<ReconstructionInfo> {
        let staging_path = staging_path.as_ref();
        if !staging_path.is_file() {
            return Err(Error::DatabaseNotFound(staging_path.display().to_string()));
        }
        let path = staging_path
            .to_str()
            .ok_or_else(|| Error::InvalidUtf8Path(staging_path.display().to_string()))?;
        let db = Builder::new_local(path).build().await?;
        let conn = db.connect()?;
        schema::require_current(&conn).await?;
        let info = fs::history::reconstruct(&conn, staging_path, target_seq).await?;
        let mut rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
        while rows.next().await?.is_some() {}
        Ok(info)
    }

    /// Establish the current state as a fresh generation-scoped history floor.
    pub async fn establish_history_floor(&self, reason: &str) -> Result<SnapshotHeader> {
        self.fs.drain_all().await?;
        let conn = self.pool.get_connection().await?;
        let root = fs::history::establish_fresh_floor(&conn, reason).await?;
        // Floor establishment collects unpinned chunks; drop cached digests
        // so no later commit pins one the collection removed.
        self.fs.journal_ctx().forget_chunks();
        Ok(root)
    }

    /// Check if sync is enabled for this database
    pub fn is_synced(&self) -> bool {
        self.sync_db.is_some()
    }

    /// Pull changes from remote database
    pub async fn pull(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.pull().await?;
        Ok(())
    }

    /// Push local changes to remote database
    pub async fn push(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.push().await?;
        Ok(())
    }

    /// Checkpoint the local database
    pub async fn checkpoint(&self) -> Result<()> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        db.checkpoint().await?;
        Ok(())
    }

    /// Get sync statistics
    pub async fn sync_stats(&self) -> Result<DatabaseSyncStats> {
        let db = self.sync_db.as_ref().ok_or(Error::SyncNotEnabled)?;
        let stats = db.stats().await?;
        Ok(stats)
    }

    /// Get all paths in the delta layer (files in fs_dentry)
    ///
    /// This returns all file and directory paths that exist in the overlay's
    /// delta layer, which represents files that have been added or modified.
    pub async fn get_delta_paths(&self) -> Result<HashSet<String>> {
        const ROOT_INO: i64 = 1;
        let conn = self.pool.get_connection().await?;

        let mut paths = HashSet::new();
        let mut queue: VecDeque<(i64, String)> = VecDeque::new();
        queue.push_back((ROOT_INO, String::new()));

        while let Some((parent_ino, prefix)) = queue.pop_front() {
            let query = format!(
                "SELECT d.name, d.ino, i.mode FROM fs_dentry d
                 JOIN fs_inode i ON d.ino = i.ino
                 WHERE d.parent_ino = {}
                 ORDER BY d.name",
                parent_ino
            );

            let mut rows = conn.query(&query, ()).await?;

            while let Some(row) = rows.next().await? {
                let name: String = row
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

                let ino: i64 = row
                    .get_value(1)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);

                let mode: u32 = row
                    .get_value(2)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;

                let full_path = if prefix.is_empty() {
                    format!("/{}", name)
                } else {
                    format!("{}/{}", prefix, name)
                };

                paths.insert(full_path.clone());

                let is_dir = mode & S_IFMT == S_IFDIR;
                if is_dir {
                    queue.push_back((ino, full_path));
                }
            }
        }

        Ok(paths)
    }

    /// Get the file mode for a path in the delta layer
    ///
    /// Returns the mode (file type and permissions) for a path, or None if
    /// the path doesn't exist in the delta layer.
    pub async fn get_file_mode(&self, path: &str) -> Result<Option<u32>> {
        const ROOT_INO: i64 = 1;
        let conn = self.pool.get_connection().await?;

        // Resolve path to inode
        let components: Vec<&str> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();

        if components.is_empty() {
            // Root directory
            let mut rows = conn
                .query("SELECT mode FROM fs_inode WHERE ino = ?", (ROOT_INO,))
                .await?;

            if let Some(row) = rows.next().await? {
                let mode = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0) as u32;
                return Ok(Some(mode));
            }
            return Ok(None);
        }

        let mut current_ino = ROOT_INO;
        for component in &components {
            let query = format!(
                "SELECT ino FROM fs_dentry WHERE parent_ino = {} AND name = '{}'",
                current_ino, component
            );

            let mut rows = conn.query(&query, ()).await?;

            if let Some(row) = rows.next().await? {
                current_ino = row
                    .get_value(0)
                    .ok()
                    .and_then(|v| v.as_integer().copied())
                    .unwrap_or(0);
            } else {
                return Ok(None);
            }
        }

        let mut rows = conn
            .query("SELECT mode FROM fs_inode WHERE ino = ?", (current_ino,))
            .await?;

        if let Some(row) = rows.next().await? {
            let mode = row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;
            return Ok(Some(mode));
        }

        Ok(None)
    }

    /// Get all whiteouts (deleted paths from base layer)
    ///
    /// Whiteouts mark paths that existed in the base layer but have been
    /// deleted in the overlay.
    pub async fn get_whiteouts(&self) -> Result<HashSet<String>> {
        let conn = self.pool.get_connection().await?;
        let mut whiteouts = HashSet::new();

        let result = conn.query("SELECT path FROM fs_whiteout", ()).await;

        if let Ok(mut rows) = result {
            while let Some(row) = rows.next().await? {
                if let Ok(Value::Text(path)) = row.get_value(0) {
                    whiteouts.insert(path.clone());
                }
            }
        } // Err case: Table doesn't exist, return empty set

        Ok(whiteouts)
    }

    /// Check if overlay is enabled for this filesystem
    ///
    /// Returns the base path if overlay is enabled, None otherwise.
    pub async fn is_overlay_enabled(&self) -> Result<Option<String>> {
        let conn = self.pool.get_connection().await?;
        // Check if fs_overlay_config table exists and has base_path
        let result = conn
            .query(
                "SELECT value FROM fs_overlay_config WHERE key = 'base_path'",
                (),
            )
            .await;

        match result {
            Ok(mut rows) => {
                if let Some(row) = rows.next().await? {
                    let base_path: String = row
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
                    Ok(Some(base_path))
                } else {
                    Ok(None)
                }
            }
            Err(_) => Ok(None), // Table doesn't exist
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    async fn create_frozen_artifact(temp_dir: &Path) -> PathBuf {
        let source_path = temp_dir.join("source.db");
        {
            let source = fs::Vfs::new(source_path.to_str().unwrap()).await.unwrap();
            let (_, file) =
                FileSystem::create_file(&source, 1, "artifact.txt", DEFAULT_FILE_MODE, 1000, 1000)
                    .await
                    .unwrap();
            file.pwrite(0, b"frozen artifact").await.unwrap();
            source.finalize().await.unwrap();
        }

        let artifact_path = temp_dir.join("artifact.db");
        std::fs::copy(source_path, &artifact_path).unwrap();
        artifact_path
    }

    fn file_family_snapshot(path: &Path) -> BTreeMap<String, Option<[u8; 32]>> {
        ["", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| {
                let family_path = PathBuf::from(format!("{}{suffix}", path.display()));
                let hash = family_path.exists().then(|| {
                    let bytes = std::fs::read(&family_path).unwrap();
                    Sha256::digest(bytes).into()
                });
                (suffix.to_string(), hash)
            })
            .collect()
    }

    #[tokio::test]
    async fn test_vfs_creation() {
        let vfs = Vfs::open(VfsOptions::ephemeral()).await.unwrap();
        // Just verify we can get the connection
        let _conn = vfs.get_connection().await.unwrap();
    }

    #[tokio::test]
    async fn open_read_only_preserves_single_file_artifact_family() {
        let temp_dir = tempfile::tempdir().unwrap();
        let artifact_path = create_frozen_artifact(temp_dir.path()).await;
        let before = file_family_snapshot(&artifact_path);

        {
            let vfs = Vfs::open_read_only(&artifact_path).await.unwrap();
            let stats = FileSystem::lookup(&vfs.fs, 1, "artifact.txt")
                .await
                .unwrap()
                .unwrap();
            let file = FileSystem::open(&vfs.fs, stats.ino, libc::O_RDONLY)
                .await
                .unwrap();
            assert_eq!(file.pread(0, 64).await.unwrap(), b"frozen artifact");
            assert_eq!(
                FileSystem::readdir(&vfs.fs, 1).await.unwrap().unwrap(),
                vec!["artifact.txt"]
            );
            FileSystem::finalize(&vfs.fs).await.unwrap();
        }

        assert_eq!(file_family_snapshot(&artifact_path), before);
        assert!(!PathBuf::from(format!("{}-wal", artifact_path.display())).exists());
        assert!(!PathBuf::from(format!("{}-shm", artifact_path.display())).exists());
    }

    #[tokio::test]
    async fn open_read_only_rejects_filesystem_write_without_mutating_family() {
        let temp_dir = tempfile::tempdir().unwrap();
        let artifact_path = create_frozen_artifact(temp_dir.path()).await;
        let before = file_family_snapshot(&artifact_path);

        {
            let vfs = Vfs::open_read_only(&artifact_path).await.unwrap();
            let error = FileSystem::mkdir(&vfs.fs, 1, "forbidden", DEFAULT_DIR_MODE, 1000, 1000)
                .await
                .unwrap_err();
            assert!(
                matches!(error, Error::Database(turso::Error::Readonly(_))),
                "unexpected write error: {error:?}"
            );
            FileSystem::drain_all(&vfs.fs).await.unwrap();
        }

        assert_eq!(file_family_snapshot(&artifact_path), before);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_read_only_reads_chmod_0444_artifact() {
        let temp_dir = tempfile::tempdir().unwrap();
        let artifact_path = create_frozen_artifact(temp_dir.path()).await;
        std::fs::set_permissions(&artifact_path, std::fs::Permissions::from_mode(0o444)).unwrap();
        let before = file_family_snapshot(&artifact_path);

        {
            let vfs = Vfs::open_read_only(&artifact_path).await.unwrap();
            let stats = FileSystem::lookup(&vfs.fs, 1, "artifact.txt")
                .await
                .unwrap()
                .unwrap();
            let file = FileSystem::open(&vfs.fs, stats.ino, libc::O_RDONLY)
                .await
                .unwrap();
            assert_eq!(file.pread(0, 64).await.unwrap(), b"frozen artifact");
        }

        assert_eq!(file_family_snapshot(&artifact_path), before);
    }

    #[tokio::test]
    async fn test_vfs_with_id() {
        let vfs = Vfs::open(VfsOptions::with_id("test-agent")).await.unwrap();
        // Just verify we can get the connection
        let _conn = vfs.get_connection().await.unwrap();

        // Cleanup
        let vfs_dir = vfs_dir();
        let file_names = ["test-agent.db", "test-agent.db-shm", "test-agent.db-wal"];
        for file_name in file_names {
            let _ = std::fs::remove_file(vfs_dir.join(file_name));
        }
    }

    #[tokio::test]
    async fn test_kv_operations() {
        let vfs = Vfs::open(VfsOptions::ephemeral()).await.unwrap();

        // Set a value
        vfs.kv.set("test_key", &"test_value").await.unwrap();

        // Get the value
        let value: Option<String> = vfs.kv.get("test_key").await.unwrap();
        assert_eq!(value, Some("test_value".to_string()));

        // Delete the value
        vfs.kv.delete("test_key").await.unwrap();

        // Verify deletion
        let value: Option<String> = vfs.kv.get("test_key").await.unwrap();
        assert_eq!(value, None);
    }

    #[tokio::test]
    async fn test_filesystem_operations() {
        let vfs = Vfs::open(VfsOptions::ephemeral()).await.unwrap();

        // Create a directory
        vfs.fs.mkdir("/test_dir", 0, 0).await.unwrap();

        // Check directory exists
        let stats = vfs.fs.stat("/test_dir").await.unwrap();
        assert!(stats.is_some());
        let dir_stats = stats.unwrap();
        assert!(dir_stats.is_directory());

        // Write a file
        let data = b"Hello, Vfs!";
        let (_, file) = vfs
            .fs
            .create_file("/test_dir/test.txt", DEFAULT_FILE_MODE, 0, 0)
            .await
            .unwrap();
        file.pwrite(0, data).await.unwrap();

        // Read the file
        let read_data = vfs
            .fs
            .read_file("/test_dir/test.txt")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read_data, data);

        // List directory
        let entries = vfs.fs.readdir(dir_stats.ino).await.unwrap().unwrap();
        assert_eq!(entries, vec!["test.txt"]);
    }

    #[tokio::test]
    async fn test_tool_calls() {
        let vfs = Vfs::open(VfsOptions::ephemeral()).await.unwrap();

        // Start a tool call
        let id = vfs
            .tools
            .start("test_tool", Some(serde_json::json!({"param": "value"})))
            .await
            .unwrap();

        // Mark it as successful
        vfs.tools
            .success(id, Some(serde_json::json!({"result": "success"})))
            .await
            .unwrap();

        // Get the tool call
        let call = vfs.tools.get(id).await.unwrap().unwrap();
        assert_eq!(call.name, "test_tool");
        assert_eq!(call.status, ToolCallStatus::Success);

        // Get stats
        let stats = vfs.tools.stats_for("test_tool").await.unwrap().unwrap();
        assert_eq!(stats.total_calls, 1);
        assert_eq!(stats.successful, 1);
    }

    #[test]
    fn test_resolve_memory() {
        let opts = VfsOptions::resolve(":memory:").unwrap();
        assert!(opts.id.is_none());
        assert!(opts.path.is_none());
    }

    #[test]
    fn test_resolve_existing_file_path() {
        // Create a temporary file to test with
        let temp_dir = std::env::temp_dir();
        let temp_file = temp_dir.join("test_resolve_existing.db");
        std::fs::write(&temp_file, b"test").unwrap();

        let opts = VfsOptions::resolve(temp_file.to_str().unwrap()).unwrap();
        assert!(opts.id.is_none());
        assert_eq!(opts.path, Some(temp_file.to_str().unwrap().to_string()));

        // Cleanup
        let _ = std::fs::remove_file(&temp_file);
    }

    #[test]
    fn test_resolve_valid_agent_id_with_existing_db() {
        // Setup: create .vfs directory and a test database
        let vfs_dir = vfs_dir();
        let _ = std::fs::create_dir_all(vfs_dir);
        let db_path = vfs_dir.join("test-resolve-agent.db");
        std::fs::write(&db_path, b"test").unwrap();

        let opts = VfsOptions::resolve("test-resolve-agent").unwrap();
        assert!(opts.id.is_none());
        assert_eq!(opts.path, Some(db_path.to_string_lossy().to_string()));

        // Cleanup
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn test_resolve_invalid_agent_id() {
        // Path traversal is path-shaped: rejected as a missing database, not
        // as a malformed agent ID.
        let result = VfsOptions::resolve("../evil");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("database not found"));

        // Agent IDs with spaces should be rejected
        let result = VfsOptions::resolve("invalid agent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid agent ID"));

        // Agent IDs with special characters should be rejected
        let result = VfsOptions::resolve("agent@test");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid agent ID"));
    }

    #[test]
    fn test_resolve_nonexistent_agent() {
        let result = VfsOptions::resolve("nonexistent-agent-12345");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_resolve_nonexistent_path_reports_database_not_found() {
        for arg in [
            "/definitely/missing.db",
            "definitely-missing-dir/agent.db",
            "definitely-missing.db",
        ] {
            let err = VfsOptions::resolve(arg).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("database not found") && message.contains(arg),
                "path-shaped argument {arg:?} must report a missing database, got: {message}"
            );
        }
    }

    #[test]
    fn test_db_path_is_absolute() {
        // Mount teardown chdirs the process to `/`; a relative db path handed
        // to turso would make every later by-path operation resolve wrong.
        let by_path = VfsOptions::with_path("some-dir/relative.db")
            .db_path()
            .unwrap();
        assert!(
            std::path::Path::new(&by_path).is_absolute(),
            "with_path must absolutize: {by_path}"
        );
        assert!(by_path.ends_with("/some-dir/relative.db"));

        let by_id = VfsOptions::with_id("db-path-absolute-test")
            .db_path()
            .unwrap();
        assert!(
            std::path::Path::new(&by_id).is_absolute(),
            "with_id must absolutize: {by_id}"
        );
        assert!(by_id.ends_with("/.vfs/db-path-absolute-test.db"));

        assert_eq!(VfsOptions::ephemeral().db_path().unwrap(), ":memory:");
        assert_eq!(
            VfsOptions::with_path(":memory:").db_path().unwrap(),
            ":memory:"
        );
    }

    #[test]
    fn test_resolve_valid_agent_id_formats() {
        // Setup: create .vfs directory and test databases
        let vfs_dir = vfs_dir();
        let _ = std::fs::create_dir_all(vfs_dir);

        // Test various valid ID formats
        let valid_ids = ["my-agent", "my_agent", "MyAgent123", "agent-123_test"];

        for id in valid_ids {
            let db_path = vfs_dir.join(format!("{}.db", id));
            std::fs::write(&db_path, b"test").unwrap();

            let opts = VfsOptions::resolve(id).unwrap();
            assert!(opts.id.is_none());
            assert_eq!(opts.path, Some(db_path.to_string_lossy().to_string()));

            // Cleanup
            let _ = std::fs::remove_file(&db_path);
        }
    }

    #[tokio::test]
    async fn test_encrypted_database_creation() {
        let hex_key = "b1bbfda4f589dc9daaf004fe21111e00dc00c98237102f5c7002a5669fc76327";
        let db_path = vfs_dir().join("test-encrypted-agent.db");

        let file_names = [
            "test-encrypted-agent.db",
            "test-encrypted-agent.db-shm",
            "test-encrypted-agent.db-wal",
        ];
        for file_name in file_names {
            let _ = std::fs::remove_file(vfs_dir().join(file_name));
        }

        // create encrypted database and write data
        {
            let vfs = Vfs::open(
                VfsOptions::with_id("test-encrypted-agent")
                    .with_encryption_key(hex_key, "aegis256"),
            )
            .await
            .unwrap();

            vfs.kv.set("test_key", &"encrypted_value").await.unwrap();
        }

        // verify database file exists
        assert!(db_path.exists(), "Database file should exist");

        // reopen with correct key - data should be readable
        {
            let vfs = Vfs::open(
                VfsOptions::with_path(db_path.to_str().unwrap())
                    .with_encryption_key(hex_key, "aegis256"),
            )
            .await
            .unwrap();

            let value: Option<String> = vfs.kv.get("test_key").await.unwrap();
            assert_eq!(value, Some("encrypted_value".to_string()));
        }

        // opening with wrong key should panic (turso panics on decryption failure)
        let wrong_key = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let path_clone = db_path.clone();
        let result = std::panic::catch_unwind(|| {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let vfs = Vfs::open(
                    VfsOptions::with_path(path_clone.to_str().unwrap())
                        .with_encryption_key(wrong_key, "aegis256"),
                )
                .await
                .unwrap();
                let _: Option<String> = vfs.kv.get("test_key").await.unwrap();
            })
        });
        assert!(result.is_err(), "Opening with wrong key should panic");

        // opening without key should panic (encrypted db read as plaintext)
        let path_clone = db_path.clone();
        let result = std::panic::catch_unwind(|| {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let vfs = Vfs::open(VfsOptions::with_path(path_clone.to_str().unwrap()))
                    .await
                    .unwrap();
                let _: Option<String> = vfs.kv.get("test_key").await.unwrap();
            })
        });
        assert!(result.is_err(), "Opening without key should panic");

        // cleanup
        for file_name in file_names {
            let _ = std::fs::remove_file(vfs_dir().join(file_name));
        }
    }
}
