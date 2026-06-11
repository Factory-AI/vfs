use crate::error::{Error, Result};
use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex, RwLock,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tracing::trace;
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

use super::{
    agentfs::AgentFS, BoxedFile, DirEntry, File, FileSystem, FilesystemStats, FsError, Stats,
    TimeChange, WriteRange,
};

/// Root inode number (matches FUSE convention)
const ROOT_INO: i64 = 1;
const STORAGE_CHUNKED: i64 = 0;
const PARTIAL_ORIGIN_ENV: &str = "AGENTFS_OVERLAY_PARTIAL_ORIGIN";
pub const DEFAULT_PARTIAL_ORIGIN_THRESHOLD_BYTES: u64 = 1024 * 1024;

/// Explicit policy for partial-origin copy-up of regular base files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartialOriginMode {
    /// Always use whole-file copy-up.
    Off,
    /// Use partial-origin copy-up for eligible regular base files.
    On,
    /// Use partial-origin copy-up for eligible regular base files at or above a threshold.
    Auto,
}

/// Runtime policy controlling when overlay writes may create partial-origin rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartialOriginPolicy {
    pub mode: PartialOriginMode,
    pub threshold_bytes: u64,
}

impl Default for PartialOriginPolicy {
    fn default() -> Self {
        Self {
            mode: PartialOriginMode::Off,
            threshold_bytes: DEFAULT_PARTIAL_ORIGIN_THRESHOLD_BYTES,
        }
    }
}

impl PartialOriginPolicy {
    pub fn new(mode: PartialOriginMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    pub fn with_threshold_bytes(mut self, threshold_bytes: u64) -> Self {
        self.threshold_bytes = threshold_bytes;
        self
    }

    /// Preserve legacy env-var opt-in while keeping ordinary defaults strict/off.
    pub fn from_env_compat() -> Self {
        if env_flag_enabled(PARTIAL_ORIGIN_ENV) {
            Self::new(PartialOriginMode::On)
        } else {
            Self::default()
        }
    }

    fn permits(&self, stats: &Stats) -> bool {
        if !stats.is_file() {
            return false;
        }

        match self.mode {
            PartialOriginMode::Off => false,
            PartialOriginMode::On => true,
            PartialOriginMode::Auto => u64::try_from(stats.size)
                .map(|size| size >= self.threshold_bytes)
                .unwrap_or(false),
        }
    }
}

fn current_timestamp() -> Result<(i64, i64)> {
    let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
    Ok((dur.as_secs() as i64, dur.subsec_nanos() as i64))
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

fn is_write_open(flags: i32) -> bool {
    (flags & libc::O_ACCMODE) != libc::O_RDONLY || (flags & libc::O_TRUNC) != 0
}

fn parent_path_for_whiteout(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }

    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
    }
}

/// Which layer an inode belongs to
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Layer {
    Delta,
    Base,
}

/// Information about an inode in the overlay filesystem
#[derive(Debug, Clone)]
struct InodeInfo {
    /// Which layer this inode lives in
    layer: Layer,
    /// The inode number in the underlying layer
    underlying_ino: i64,
    /// Virtual path (for whiteout and copy-up operations)
    path: String,
}

#[derive(Debug, Clone)]
struct PartialOrigin {
    base_path: String,
    base_fingerprint_size: i64,
    base_mtime: i64,
    base_mtime_nsec: u32,
    base_ctime: i64,
    base_ctime_nsec: u32,
}

struct OverlayPartialFile {
    delta: AgentFS,
    base: Arc<dyn FileSystem>,
    base_file: BoxedFile,
    origin: PartialOrigin,
    overlay_ino: i64,
    delta_ino: i64,
    chunk_size: usize,
}

/// A copy-on-write overlay filesystem using inode-based operations.
///
/// Combines a read-only base layer with a writable delta layer (AgentFS).
/// All modifications are written to the delta layer, while reads fall back
/// to the base layer if not found in delta.
pub struct OverlayFS {
    /// The underlying read-only base filesystem
    base: Arc<dyn FileSystem>,
    /// The delta layer where modifications go
    delta: AgentFS,
    /// Map from overlay inode to underlying layer info
    inode_map: RwLock<HashMap<i64, InodeInfo>>,
    /// Reverse map: (layer, underlying_ino) -> overlay_ino
    reverse_map: RwLock<HashMap<(Layer, i64), i64>>,
    /// Map from path to overlay inode (for path-based operations)
    path_map: RwLock<HashMap<String, i64>>,
    /// Serializes multi-map overlay inode updates.
    map_lock: Mutex<()>,
    /// Next inode number to allocate
    next_ino: AtomicI64,
    /// Set of whiteout paths (deleted from base)
    whiteouts: RwLock<HashSet<String>>,
    /// Origin mapping: delta_ino -> base_ino (for copy-up consistency)
    origin_map: RwLock<HashMap<i64, i64>>,
    /// Explicit policy for chunk-granularity base fallback.
    partial_origin_policy: PartialOriginPolicy,
}

impl OverlayFS {
    /// Create a new overlay filesystem
    pub fn new(base: Arc<dyn FileSystem>, delta: AgentFS) -> Self {
        Self::new_with_partial_origin_policy(base, delta, PartialOriginPolicy::from_env_compat())
    }

    pub fn new_with_partial_origin_policy(
        base: Arc<dyn FileSystem>,
        delta: AgentFS,
        partial_origin_policy: PartialOriginPolicy,
    ) -> Self {
        Self::new_with_partial_origin_policy_inner(base, delta, partial_origin_policy)
    }

    #[cfg(test)]
    fn new_with_partial_origin(
        base: Arc<dyn FileSystem>,
        delta: AgentFS,
        partial_origin_enabled: bool,
    ) -> Self {
        let mode = if partial_origin_enabled {
            PartialOriginMode::On
        } else {
            PartialOriginMode::Off
        };
        Self::new_with_partial_origin_policy_inner(base, delta, PartialOriginPolicy::new(mode))
    }

    fn new_with_partial_origin_policy_inner(
        base: Arc<dyn FileSystem>,
        delta: AgentFS,
        partial_origin_policy: PartialOriginPolicy,
    ) -> Self {
        let mut inode_map = HashMap::new();
        let mut reverse_map = HashMap::new();
        let mut path_map = HashMap::new();

        // Root inode maps to delta's root (inode 1)
        inode_map.insert(
            ROOT_INO,
            InodeInfo {
                layer: Layer::Delta,
                underlying_ino: 1,
                path: "/".to_string(),
            },
        );
        reverse_map.insert((Layer::Delta, 1), ROOT_INO);
        path_map.insert("/".to_string(), ROOT_INO);

        Self {
            base,
            delta,
            inode_map: RwLock::new(inode_map),
            reverse_map: RwLock::new(reverse_map),
            path_map: RwLock::new(path_map),
            map_lock: Mutex::new(()),
            next_ino: AtomicI64::new(2),
            whiteouts: RwLock::new(HashSet::new()),
            origin_map: RwLock::new(HashMap::new()),
            partial_origin_policy,
        }
    }

    /// Initialize the overlay filesystem schema
    pub async fn init_schema(conn: &Connection, base_path: &str) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_whiteout (
                path TEXT PRIMARY KEY,
                parent_path TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        Self::ensure_whiteout_parent_path(conn).await?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_fs_whiteout_parent ON fs_whiteout(parent_path)",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_overlay_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await?;
        conn.execute(
            "INSERT OR REPLACE INTO fs_overlay_config (key, value) VALUES ('base_path', ?1)",
            [Value::Text(base_path.to_string())],
        )
        .await?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_origin (
                delta_ino INTEGER PRIMARY KEY,
                base_ino INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_partial_origin (
                delta_ino INTEGER PRIMARY KEY,
                base_ino INTEGER NOT NULL,
                base_path TEXT NOT NULL,
                base_size INTEGER NOT NULL,
                base_fingerprint_size INTEGER NOT NULL DEFAULT -1,
                base_mtime INTEGER NOT NULL DEFAULT 0,
                base_mtime_nsec INTEGER NOT NULL DEFAULT 0,
                base_ctime INTEGER NOT NULL DEFAULT 0,
                base_ctime_nsec INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        conn.execute(
            "ALTER TABLE fs_partial_origin ADD COLUMN base_fingerprint_size INTEGER NOT NULL DEFAULT -1",
            (),
        )
        .await
        .ok();
        conn.execute(
            "ALTER TABLE fs_partial_origin ADD COLUMN base_mtime INTEGER NOT NULL DEFAULT 0",
            (),
        )
        .await
        .ok();
        conn.execute(
            "ALTER TABLE fs_partial_origin ADD COLUMN base_mtime_nsec INTEGER NOT NULL DEFAULT 0",
            (),
        )
        .await
        .ok();
        conn.execute(
            "ALTER TABLE fs_partial_origin ADD COLUMN base_ctime INTEGER NOT NULL DEFAULT 0",
            (),
        )
        .await
        .ok();
        conn.execute(
            "ALTER TABLE fs_partial_origin ADD COLUMN base_ctime_nsec INTEGER NOT NULL DEFAULT 0",
            (),
        )
        .await
        .ok();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS fs_chunk_override (
                delta_ino INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                PRIMARY KEY (delta_ino, chunk_index)
            )",
            (),
        )
        .await?;
        Ok(())
    }

    async fn ensure_whiteout_parent_path(conn: &Connection) -> Result<()> {
        let mut rows = conn.query("PRAGMA table_info(fs_whiteout)", ()).await?;
        let mut has_parent_path = false;
        while let Some(row) = rows.next().await? {
            if let Some(name) = row.get_value(1).ok().and_then(|value| match value {
                Value::Text(name) => Some(name.clone()),
                _ => None,
            }) {
                if name == "parent_path" {
                    has_parent_path = true;
                    break;
                }
            }
        }

        if !has_parent_path {
            conn.execute(
                "ALTER TABLE fs_whiteout ADD COLUMN parent_path TEXT NOT NULL DEFAULT '/'",
                (),
            )
            .await?;
            let mut rows = conn.query("SELECT path FROM fs_whiteout", ()).await?;
            let mut paths = Vec::new();
            while let Some(row) = rows.next().await? {
                if let Some(path) = row.get_value(0).ok().and_then(|value| match value {
                    Value::Text(path) => Some(path.clone()),
                    _ => None,
                }) {
                    paths.push(path);
                }
            }
            for path in paths {
                conn.execute(
                    "UPDATE fs_whiteout SET parent_path = ? WHERE path = ?",
                    (parent_path_for_whiteout(&path), path),
                )
                .await?;
            }
        }

        Ok(())
    }

    /// Initialize the overlay filesystem
    pub async fn init(&self, base_path: &str) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        Self::init_schema(&conn, base_path).await?;
        self.load_whiteouts(&conn).await?;
        self.load_origins(&conn).await?;
        Ok(())
    }

    /// Load whiteouts from database into memory
    async fn load_whiteouts(&self, conn: &Connection) -> Result<()> {
        let mut rows = conn.query("SELECT path FROM fs_whiteout", ()).await?;
        let mut paths = Vec::new();
        while let Some(row) = rows.next().await? {
            if let Some(path) = row.get_value(0).ok().and_then(|v| match v {
                Value::Text(s) => Some(s.clone()),
                _ => None,
            }) {
                paths.push(path);
            }
        }
        let mut whiteouts = self.whiteouts.write().unwrap();
        for path in paths {
            whiteouts.insert(path);
        }
        Ok(())
    }

    /// Load existing whiteouts (public interface)
    pub async fn load_whiteouts_public(&self) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        self.load_whiteouts(&conn).await
    }

    /// Load persisted state (whiteouts and origin mappings) from database.
    /// Call this after creating an OverlayFS for an existing database.
    pub async fn load(&self) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        self.load_whiteouts(&conn).await?;
        self.load_origins(&conn).await?;
        Ok(())
    }

    /// Load origin mappings from database
    async fn load_origins(&self, conn: &Connection) -> Result<()> {
        let result = conn
            .query("SELECT delta_ino, base_ino FROM fs_origin", ())
            .await;
        if let Ok(mut rows) = result {
            let mut mappings = Vec::new();
            while let Some(row) = rows.next().await? {
                let delta_ino = row.get_value(0).ok().and_then(|v| v.as_integer().copied());
                let base_ino = row.get_value(1).ok().and_then(|v| v.as_integer().copied());
                if let (Some(d), Some(b)) = (delta_ino, base_ino) {
                    mappings.push((d, b));
                }
            }
            let mut origins = self.origin_map.write().unwrap();
            for (d, b) in mappings {
                origins.insert(d, b);
            }
        }
        Ok(())
    }

    /// Check if a path is whiteout (deleted from base)
    fn is_whiteout(&self, path: &str) -> bool {
        let whiteouts = self.whiteouts.read().unwrap();
        // Check path and all ancestors
        let mut current = String::new();
        for component in path.split('/').filter(|s| !s.is_empty()) {
            current = format!("{}/{}", current, component);
            if whiteouts.contains(&current) {
                return true;
            }
        }
        false
    }

    /// Create a whiteout for a path
    async fn create_whiteout(&self, path: &str) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let parent_path = parent_path_for_whiteout(path);
        conn.execute(
            "INSERT OR REPLACE INTO fs_whiteout (path, parent_path, created_at) VALUES (?, ?, ?)",
            (path, parent_path, now),
        )
        .await?;
        self.whiteouts.write().unwrap().insert(path.to_string());
        Ok(())
    }

    /// Remove a whiteout
    async fn remove_whiteout(&self, path: &str) -> Result<()> {
        if !self.whiteouts.read().unwrap().contains(path) {
            return Ok(());
        }
        let conn = self.delta.get_connection().await?;
        conn.execute("DELETE FROM fs_whiteout WHERE path = ?", (path,))
            .await?;
        self.whiteouts.write().unwrap().remove(path);
        Ok(())
    }

    /// Get child whiteouts for a directory
    fn get_child_whiteouts(&self, dir_path: &str) -> HashSet<String> {
        let whiteouts = self.whiteouts.read().unwrap();
        let prefix = if dir_path == "/" {
            "/".to_string()
        } else {
            format!("{}/", dir_path)
        };
        whiteouts
            .iter()
            .filter_map(|p| {
                if dir_path == "/" {
                    // Direct children of root
                    let trimmed = p.trim_start_matches('/');
                    if !trimmed.contains('/') {
                        Some(trimmed.to_string())
                    } else {
                        None
                    }
                } else if p.starts_with(&prefix) {
                    let rest = &p[prefix.len()..];
                    if !rest.contains('/') {
                        Some(rest.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Allocate a new overlay inode number
    fn alloc_ino(&self) -> i64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    /// Get or create an overlay inode for a layer inode
    fn get_or_create_overlay_ino(&self, layer: Layer, underlying_ino: i64, path: &str) -> i64 {
        let _map_guard = self.map_lock.lock().unwrap();
        // Check reverse map first
        {
            let reverse = self.reverse_map.read().unwrap();
            if let Some(&ino) = reverse.get(&(layer, underlying_ino)) {
                return ino;
            }
        }

        // Allocate new inode
        let ino = self.alloc_ino();
        {
            let mut inode_map = self.inode_map.write().unwrap();
            inode_map.insert(
                ino,
                InodeInfo {
                    layer,
                    underlying_ino,
                    path: path.to_string(),
                },
            );
        }
        {
            let mut reverse = self.reverse_map.write().unwrap();
            reverse.insert((layer, underlying_ino), ino);
        }
        {
            let mut path_map = self.path_map.write().unwrap();
            path_map.insert(path.to_string(), ino);
        }

        ino
    }

    /// Refresh an existing overlay inode mapping to point at a new backing inode/path.
    ///
    /// This is used when we intentionally reuse an existing overlay inode number
    /// (for stability), but the underlying layer/path has changed (for example after
    /// a base file is copied-up and then renamed in delta).
    fn refresh_overlay_mapping(
        &self,
        overlay_ino: i64,
        new_layer: Layer,
        new_underlying_ino: i64,
        new_path: &str,
    ) {
        let _map_guard = self.map_lock.lock().unwrap();
        let old_path = {
            let mut inode_map = self.inode_map.write().unwrap();
            let Some(info) = inode_map.get_mut(&overlay_ino) else {
                return;
            };
            let old_path = info.path.clone();
            info.layer = new_layer;
            info.underlying_ino = new_underlying_ino;
            info.path = new_path.to_string();
            old_path
        };

        {
            let mut reverse = self.reverse_map.write().unwrap();
            reverse.insert((new_layer, new_underlying_ino), overlay_ino);
        }

        {
            let mut path_map = self.path_map.write().unwrap();
            if path_map.get(&old_path).copied() == Some(overlay_ino) {
                path_map.remove(&old_path);
            }
            path_map.insert(new_path.to_string(), overlay_ino);
        }
    }

    /// Get inode info for an overlay inode
    fn get_inode_info(&self, ino: i64) -> Option<InodeInfo> {
        self.inode_map.read().unwrap().get(&ino).cloned()
    }

    fn live_origin_overlay_ino(&self, base_ino: i64, path: &str) -> Option<i64> {
        let overlay_ino = {
            let reverse = self.reverse_map.read().unwrap();
            reverse.get(&(Layer::Base, base_ino)).copied()?
        };
        let info = self.get_inode_info(overlay_ino)?;
        if info.path == path {
            Some(overlay_ino)
        } else {
            None
        }
    }

    /// Build path from parent inode and name
    fn build_path(&self, parent_ino: i64, name: &str) -> Result<String> {
        let info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        Ok(if info.path == "/" {
            format!("/{}", name)
        } else {
            format!("{}/{}", info.path, name)
        })
    }

    /// Get a reference to the base layer
    pub fn base(&self) -> &Arc<dyn FileSystem> {
        &self.base
    }

    /// Get a reference to the delta layer
    pub fn delta(&self) -> &AgentFS {
        &self.delta
    }

    /// Store origin mapping for copy-up
    async fn add_origin_mapping(&self, delta_ino: i64, base_ino: i64) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        conn.execute(
            "INSERT OR REPLACE INTO fs_origin (delta_ino, base_ino) VALUES (?, ?)",
            (delta_ino, base_ino),
        )
        .await?;
        self.origin_map.write().unwrap().insert(delta_ino, base_ino);
        Ok(())
    }

    /// Get origin inode for a delta inode
    fn get_origin_ino(&self, delta_ino: i64) -> Option<i64> {
        self.origin_map.read().unwrap().get(&delta_ino).copied()
    }

    async fn partial_origin_for_delta(&self, delta_ino: i64) -> Result<Option<PartialOrigin>> {
        let conn = self.delta.get_connection().await?;
        let mut rows = conn
            .query(
                "SELECT base_path, base_size, base_fingerprint_size,
                        base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec
                 FROM fs_partial_origin WHERE delta_ino = ?",
                (delta_ino,),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            let base_path = match row.get_value(0)? {
                Value::Text(path) => path,
                _ => {
                    return Err(Error::Internal(
                        "invalid partial origin base_path".to_string(),
                    ))
                }
            };
            let base_size = row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .ok_or_else(|| Error::Internal("invalid partial origin base_size".to_string()))?;
            let base_fingerprint_size = row
                .get_value(2)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(base_size);
            let base_mtime = row
                .get_value(3)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            let base_mtime_nsec = row
                .get_value(4)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;
            let base_ctime = row
                .get_value(5)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);
            let base_ctime_nsec = row
                .get_value(6)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;
            Ok(Some(PartialOrigin {
                base_path,
                base_fingerprint_size: if base_fingerprint_size < 0 {
                    base_size
                } else {
                    base_fingerprint_size
                },
                base_mtime,
                base_mtime_nsec,
                base_ctime,
                base_ctime_nsec,
            }))
        } else {
            Ok(None)
        }
    }

    async fn add_partial_origin_mapping(
        &self,
        delta_ino: i64,
        base_ino: i64,
        base_path: &str,
        base_stats: &Stats,
    ) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        conn.execute(
            "INSERT OR REPLACE INTO fs_partial_origin (
                delta_ino, base_ino, base_path, base_size, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            (delta_ino, base_ino, base_path, base_stats.size, now),
        )
        .await?;
        conn.execute(
            "UPDATE fs_partial_origin
             SET base_fingerprint_size = ?1, base_mtime = ?2, base_mtime_nsec = ?3
             WHERE delta_ino = ?4",
            (
                base_stats.size,
                base_stats.mtime,
                base_stats.mtime_nsec as i64,
                delta_ino,
            ),
        )
        .await?;
        conn.execute(
            "UPDATE fs_partial_origin
             SET base_ctime = ?1, base_ctime_nsec = ?2
             WHERE delta_ino = ?3",
            (base_stats.ctime, base_stats.ctime_nsec as i64, delta_ino),
        )
        .await?;
        Ok(())
    }

    async fn resolve_base_path(&self, path: &str) -> Result<Option<Stats>> {
        let mut ino = ROOT_INO;
        if path == "/" {
            return self.base.getattr(ino).await;
        }

        let mut stats = None;
        for component in path.split('/').filter(|s| !s.is_empty()) {
            let Some(next) = self.base.lookup(ino, component).await? else {
                return Ok(None);
            };
            ino = next.ino;
            stats = Some(next);
        }
        Ok(stats)
    }

    fn validate_partial_origin(&self, origin: &PartialOrigin, stats: &Stats) -> Result<()> {
        if stats.size != origin.base_fingerprint_size {
            return Err(Error::Internal(format!(
                "partial-origin base changed for {} (stored size={}, current size={})",
                origin.base_path, origin.base_fingerprint_size, stats.size
            )));
        }
        if stats.mtime != origin.base_mtime
            || stats.mtime_nsec != origin.base_mtime_nsec
            || stats.ctime != origin.base_ctime
            || stats.ctime_nsec != origin.base_ctime_nsec
        {
            return Err(Error::Internal(format!(
                "partial-origin base changed for {} (stored mtime={}.{}, current mtime={}.{}, stored ctime={}.{}, current ctime={}.{})",
                origin.base_path,
                origin.base_mtime,
                origin.base_mtime_nsec,
                stats.mtime,
                stats.mtime_nsec,
                origin.base_ctime,
                origin.base_ctime_nsec,
                stats.ctime,
                stats.ctime_nsec
            )));
        }
        Ok(())
    }

    async fn cleanup_partial_origin_if_unlinked(&self, delta_ino: i64) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let mut rows = conn
            .query("SELECT 1 FROM fs_inode WHERE ino = ?", (delta_ino,))
            .await?;
        if rows.next().await?.is_some() {
            return Ok(());
        }

        conn.execute("DELETE FROM fs_origin WHERE delta_ino = ?", (delta_ino,))
            .await?;
        conn.execute(
            "DELETE FROM fs_chunk_override WHERE delta_ino = ?",
            (delta_ino,),
        )
        .await?;
        conn.execute(
            "DELETE FROM fs_partial_origin WHERE delta_ino = ?",
            (delta_ino,),
        )
        .await?;
        Ok(())
    }

    /// Promote an overlay inode from base layer to delta layer.
    ///
    /// When a directory that was originally looked up from base gets a
    /// corresponding directory created in delta (via ensure_parent_dirs),
    /// we need to update the overlay inode to point to delta. This ensures
    /// that operations like readdir and unlink will check the delta layer.
    fn promote_to_delta(&self, path: &str, delta_ino: i64) {
        let _map_guard = self.map_lock.lock().unwrap();
        let path_map = self.path_map.read().unwrap();
        let overlay_ino = match path_map.get(path) {
            Some(&ino) => ino,
            None => return, // No existing mapping, nothing to promote
        };
        drop(path_map);

        // Update the inode mapping to point to delta
        let mut inode_map = self.inode_map.write().unwrap();
        if let Some(info) = inode_map.get_mut(&overlay_ino) {
            if info.layer == Layer::Base {
                let old_base_ino = info.underlying_ino;
                info.layer = Layer::Delta;
                info.underlying_ino = delta_ino;

                // Update reverse map: add delta mapping (keep base mapping for origin lookups)
                drop(inode_map);
                let mut reverse = self.reverse_map.write().unwrap();
                reverse.remove(&(Layer::Base, old_base_ino));
                reverse.insert((Layer::Delta, delta_ino), overlay_ino);
            }
        }
    }

    /// Resolve the delta-layer inode for a parent directory.
    ///
    /// If the parent's overlay inode already maps to Delta, returns the underlying
    /// inode directly. Otherwise, walks the delta filesystem from root using the
    /// stored path. Returns Ok(None) if any path component is missing in delta.
    async fn resolve_delta_parent(&self, info: &InodeInfo) -> Result<Option<i64>> {
        if info.layer == Layer::Delta {
            return Ok(Some(info.underlying_ino));
        }
        let mut ino: i64 = 1;
        for comp in info.path.split('/').filter(|s| !s.is_empty()) {
            match FileSystem::lookup(&self.delta, ino, comp).await? {
                Some(s) if s.is_directory() => ino = s.ino,
                Some(_) => return Ok(None),
                None => return Ok(None),
            }
        }
        Ok(Some(ino))
    }

    /// Ensure parent directories exist in delta layer
    async fn ensure_parent_dirs(&self, path: &str, uid: u32, gid: u32) -> Result<()> {
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        let mut current_path = String::new();
        let mut current_delta_ino: i64 = 1; // Delta root
        let mut current_base_ino: i64 = 1; // Base root

        for component in components.iter().take(components.len().saturating_sub(1)) {
            current_path = format!("{}/{}", current_path, component);

            // Remove any whiteout for this path
            self.remove_whiteout(&current_path).await?;

            // Check if directory exists in delta
            if let Some(stats) =
                FileSystem::lookup(&self.delta, current_delta_ino, component).await?
            {
                if stats.is_directory() {
                    current_delta_ino = stats.ino;
                    // Advance base in parallel so it stays in sync
                    if let Some(bs) = self.base.lookup(current_base_ino, component).await? {
                        current_base_ino = bs.ino;
                    }
                    continue;
                } else {
                    return Err(FsError::NotADirectory.into());
                }
            }

            // Not in delta, check base (using the base inode, not delta inode)
            let base_stats = self.base.lookup(current_base_ino, component).await?;
            let (dir_uid, dir_gid, origin_base_ino) = if let Some(s) = &base_stats {
                let base_ino = s.ino;
                current_base_ino = base_ino;
                (s.uid, s.gid, Some(base_ino))
            } else {
                (uid, gid, None)
            };

            // Create directory in delta
            let new_stats = FileSystem::mkdir(
                &self.delta,
                current_delta_ino,
                component,
                0o755,
                dir_uid,
                dir_gid,
            )
            .await?;
            current_delta_ino = new_stats.ino;

            // Create origin mapping if directory exists in base, so that
            // lookups return consistent overlay inodes
            if let Some(base_ino) = origin_base_ino {
                self.add_origin_mapping(new_stats.ino, base_ino).await?;
                // Promote the overlay inode to delta so readdir/unlink will check delta
                self.promote_to_delta(&current_path, new_stats.ino);
            }
        }

        Ok(())
    }

    /// Copy a file from base to delta for modification
    async fn copy_up(&self, path: &str, base_ino: i64) -> Result<i64> {
        // Parse path to get parent and name
        let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let name = components.last().unwrap();

        // Check if already copied up - walk delta to find parent and check for file
        let mut parent_ino: i64 = 1;
        let mut found_parent = true;
        for comp in components.iter().take(components.len() - 1) {
            if let Some(stats) = FileSystem::lookup(&self.delta, parent_ino, comp).await? {
                parent_ino = stats.ino;
            } else {
                found_parent = false;
                break;
            }
        }

        // If parent exists in delta, check if file already exists there
        if found_parent {
            if let Some(stats) = FileSystem::lookup(&self.delta, parent_ino, name).await? {
                // Already copied up, return delta inode
                return Ok(stats.ino);
            }
        }

        // Get base stats
        let base_stats = self
            .base
            .getattr(base_ino)
            .await?
            .ok_or(FsError::NotFound)?;

        // Ensure parent directories exist
        self.ensure_parent_dirs(path, base_stats.uid, base_stats.gid)
            .await?;

        // Look up parent in delta by walking the path
        let mut parent_ino: i64 = 1; // Start at delta root
        for comp in components.iter().take(components.len() - 1) {
            let stats = FileSystem::lookup(&self.delta, parent_ino, comp)
                .await?
                .ok_or(FsError::NotFound)?;
            parent_ino = stats.ino;
        }

        // Copy based on file type
        let delta_ino = if base_stats.is_symlink() {
            let target = self
                .base
                .readlink(base_ino)
                .await?
                .ok_or(FsError::NotFound)?;
            let stats = FileSystem::symlink(
                &self.delta,
                parent_ino,
                name,
                &target,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            stats.ino
        } else if base_stats.is_directory() {
            let stats = FileSystem::mkdir(
                &self.delta,
                parent_ino,
                name,
                base_stats.mode & 0o7777,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            stats.ino
        } else {
            // Regular file - read content and create
            let base_file = self.base.open(base_ino, libc::O_RDONLY).await?;
            let content = base_file.pread(0, base_stats.size as u64).await?;

            let (stats, delta_file) = FileSystem::create_file(
                &self.delta,
                parent_ino,
                name,
                base_stats.mode,
                base_stats.uid,
                base_stats.gid,
            )
            .await?;
            delta_file.pwrite(0, &content).await?;
            stats.ino
        };

        // Store origin mapping
        self.add_origin_mapping(delta_ino, base_ino).await?;

        Ok(delta_ino)
    }

    /// Copy-up a file and update the inode mapping so subsequent operations
    /// go to the delta layer. Returns the delta inode.
    async fn copy_up_and_update_mapping(&self, overlay_ino: i64, info: &InodeInfo) -> Result<i64> {
        let delta_ino = self.copy_up(&info.path, info.underlying_ino).await?;

        // Update the inode mapping to point to delta
        let _map_guard = self.map_lock.lock().unwrap();
        {
            let mut inode_map = self.inode_map.write().unwrap();
            inode_map.insert(
                overlay_ino,
                InodeInfo {
                    layer: Layer::Delta,
                    underlying_ino: delta_ino,
                    path: info.path.clone(),
                },
            );
        }
        {
            let mut reverse_map = self.reverse_map.write().unwrap();
            // Keep the base mapping so lookups via origin still return the same overlay inode
            // (Layer::Base, base_ino) -> overlay_ino is kept
            // Add the delta mapping as well
            reverse_map.insert((Layer::Delta, delta_ino), overlay_ino);
        }

        Ok(delta_ino)
    }

    async fn partial_copy_up_and_update_mapping(
        &self,
        overlay_ino: i64,
        info: &InodeInfo,
    ) -> Result<i64> {
        let components: Vec<&str> = info.path.split('/').filter(|s| !s.is_empty()).collect();
        if components.is_empty() {
            return Err(FsError::RootOperation.into());
        }
        let name = components.last().unwrap();

        let base_stats = match self.resolve_base_path(&info.path).await? {
            Some(stats) => stats,
            None => self
                .base
                .getattr(info.underlying_ino)
                .await?
                .ok_or(FsError::NotFound)?,
        };
        if !base_stats.is_file() {
            return self.copy_up_and_update_mapping(overlay_ino, info).await;
        }

        self.ensure_parent_dirs(&info.path, base_stats.uid, base_stats.gid)
            .await?;

        let mut parent_ino = ROOT_INO;
        for comp in components.iter().take(components.len() - 1) {
            let stats = FileSystem::lookup(&self.delta, parent_ino, comp)
                .await?
                .ok_or(FsError::NotFound)?;
            parent_ino = stats.ino;
        }

        if let Some(stats) = FileSystem::lookup(&self.delta, parent_ino, name).await? {
            self.refresh_overlay_mapping(overlay_ino, Layer::Delta, stats.ino, &info.path);
            return Ok(stats.ino);
        }

        let (stats, _file) = FileSystem::create_file(
            &self.delta,
            parent_ino,
            name,
            base_stats.mode,
            base_stats.uid,
            base_stats.gid,
        )
        .await?;
        let delta_ino = stats.ino;

        let conn = self.delta.get_connection().await?;
        conn.execute(
            "UPDATE fs_inode
             SET mode = ?, uid = ?, gid = ?, size = ?, atime = ?, mtime = ?, ctime = ?,
                 atime_nsec = ?, mtime_nsec = ?, ctime_nsec = ?, data_inline = NULL, storage_kind = ?
             WHERE ino = ?",
            (
                base_stats.mode as i64,
                base_stats.uid as i64,
                base_stats.gid as i64,
                base_stats.size,
                base_stats.atime,
                base_stats.mtime,
                base_stats.ctime,
                base_stats.atime_nsec as i64,
                base_stats.mtime_nsec as i64,
                base_stats.ctime_nsec as i64,
                STORAGE_CHUNKED,
                delta_ino,
            ),
        )
        .await?;
        self.delta.invalidate_attr(delta_ino);

        self.add_origin_mapping(delta_ino, info.underlying_ino)
            .await?;
        self.add_partial_origin_mapping(delta_ino, info.underlying_ino, &info.path, &base_stats)
            .await?;
        self.refresh_overlay_mapping(overlay_ino, Layer::Delta, delta_ino, &info.path);

        Ok(delta_ino)
    }

    async fn partial_file_for_delta(
        &self,
        overlay_ino: i64,
        delta_ino: i64,
        flags: i32,
    ) -> Result<BoxedFile> {
        if let Some(origin) = self.partial_origin_for_delta(delta_ino).await? {
            let base_stats = self
                .resolve_base_path(&origin.base_path)
                .await?
                .ok_or(FsError::NotFound)?;
            self.validate_partial_origin(&origin, &base_stats)?;
            let base_file = self.base.open(base_stats.ino, libc::O_RDONLY).await?;

            // Tier Two Axis C: HostFS passthrough for unmodified delta files.
            //
            // A partial-origin delta inode that has zero chunk overrides, zero
            // full chunks, no inline override, and a size matching the base is
            // byte-identical to the base file. In that case the
            // OverlayPartialFile wrapper would do a chunk-merge that always
            // hits the "no override; read from base" branch -- the SQLite
            // round trip is pure overhead. Returning the HostFS fd directly
            // sends pread() straight to the kernel VFS for every read on this
            // handle, which is most of the cost on `git status` / `git diff`
            // / agent stat-storms over a working tree that was copy-up'd but
            // not modified.
            //
            // Restricted to read-only opens: a write open MUST go through the
            // OverlayPartialFile wrapper so writes land as `fs_chunk_override`
            // rows in the delta DB and never touch the real base file
            // (no-real-write invariant from Tier One).
            if !is_write_open(flags) {
                crate::profiling::record_base_fast_open_passthrough_attempted();
                if self
                    .delta_has_no_content_overrides(delta_ino, base_stats.size)
                    .await?
                {
                    crate::profiling::record_base_fast_open_passthrough_succeeded();
                    return Ok(base_file);
                }
                crate::profiling::record_base_fast_open_passthrough_fallback();
            }

            let file: BoxedFile = Arc::new(OverlayPartialFile {
                delta: self.delta.clone(),
                base: self.base.clone(),
                base_file,
                origin,
                overlay_ino,
                delta_ino,
                chunk_size: self.delta.chunk_size(),
            });
            if (flags & libc::O_TRUNC) != 0 {
                file.truncate(0).await?;
            }
            Ok(file)
        } else {
            FileSystem::open(&self.delta, delta_ino, flags).await
        }
    }

    /// Returns true if the delta inode has no content modifications: no chunk
    /// overrides, no full chunks, no inline override, and size matches the
    /// base. Such a delta is purely a metadata copy and reads can bypass the
    /// `OverlayPartialFile` merge path entirely.
    ///
    /// This is the cheap "is this file unmodified?" check that Tier Two Axis
    /// C uses to decide whether `partial_file_for_delta` can short-circuit to
    /// a HostFS fd.
    async fn delta_has_no_content_overrides(&self, delta_ino: i64, base_size: i64) -> Result<bool> {
        let conn = self.delta.get_connection().await?;

        // Any per-chunk override?
        let mut rows = conn
            .query(
                "SELECT 1 FROM fs_chunk_override WHERE delta_ino = ? LIMIT 1",
                (delta_ino,),
            )
            .await?;
        if rows.next().await?.is_some() {
            return Ok(false);
        }

        // Any full chunk in fs_data? (Should be implied by no overrides for
        // partial-origin files, but check defensively in case of a
        // partial-origin → fully-overridden transition.)
        let mut rows = conn
            .query("SELECT 1 FROM fs_data WHERE ino = ? LIMIT 1", (delta_ino,))
            .await?;
        if rows.next().await?.is_some() {
            return Ok(false);
        }

        // Size match + no inline override?
        let mut rows = conn
            .query(
                "SELECT size, data_inline FROM fs_inode WHERE ino = ?",
                (delta_ino,),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(false);
        };
        let delta_size: i64 = row
            .get(0)
            .map_err(|e| Error::Internal(format!("fs_inode.size read failed: {e}")))?;
        if delta_size != base_size {
            return Ok(false);
        }
        let inline_value = row
            .get_value(1)
            .map_err(|e| Error::Internal(format!("fs_inode.data_inline read failed: {e}")))?;
        let inline_empty = match inline_value {
            Value::Null => true,
            Value::Blob(blob) => blob.is_empty(),
            _ => true,
        };
        if !inline_empty {
            return Ok(false);
        }

        Ok(true)
    }
}

#[async_trait]
impl File for OverlayPartialFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        self.validate_current_origin().await?;
        let conn = self.delta.get_connection().await?;
        let file_size = self.delta_file_size_with_conn(&conn).await?;
        if offset >= file_size || size == 0 {
            return Ok(Vec::new());
        }

        let read_len = std::cmp::min(size, file_size - offset) as usize;
        let chunk_size = self.chunk_size as u64;
        let mut result = Vec::with_capacity(read_len);

        while result.len() < read_len {
            let current_offset = offset + result.len() as u64;
            let chunk_index = current_offset / chunk_size;
            let offset_in_chunk = (current_offset % chunk_size) as usize;
            let take = std::cmp::min(
                self.chunk_size - offset_in_chunk,
                read_len.saturating_sub(result.len()),
            );

            let chunk = self.read_merged_chunk_with_conn(&conn, chunk_index).await?;
            result.extend_from_slice(&chunk[offset_in_chunk..offset_in_chunk + take]);
        }

        Ok(result)
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.pwrite_ranges(vec![WriteRange {
            offset,
            data: data.to_vec(),
        }])
        .await
    }

    async fn pwrite_ranges(&self, ranges: Vec<WriteRange>) -> Result<()> {
        if ranges.iter().all(|range| range.data.is_empty()) {
            return Ok(());
        }
        let conn = self.delta.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;

        let result: Result<()> = async {
            let mut new_size = self.delta_file_size_with_conn(&conn).await?;
            for range in ranges {
                if range.data.is_empty() {
                    continue;
                }
                let write_end = range
                    .offset
                    .checked_add(range.data.len() as u64)
                    .ok_or_else(|| Error::Internal("file write offset overflow".to_string()))?;
                new_size = std::cmp::max(new_size, write_end);
                let chunk_size = self.chunk_size as u64;
                let mut written = 0usize;

                while written < range.data.len() {
                    let current_offset = range.offset + written as u64;
                    let chunk_index = current_offset / chunk_size;
                    let offset_in_chunk = (current_offset % chunk_size) as usize;
                    let remaining_in_chunk = self.chunk_size - offset_in_chunk;
                    let to_write = std::cmp::min(remaining_in_chunk, range.data.len() - written);

                    let mut chunk = self
                        .read_merged_chunk_with_conn(&conn, chunk_index)
                        .await?;
                    chunk[offset_in_chunk..offset_in_chunk + to_write]
                        .copy_from_slice(&range.data[written..written + to_write]);

                    conn.execute(
                        "INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
                        (
                            self.delta_ino,
                            chunk_index as i64,
                            Value::Blob(chunk),
                        ),
                    )
                    .await?;
                    conn.execute(
                        "INSERT OR IGNORE INTO fs_chunk_override (delta_ino, chunk_index) VALUES (?, ?)",
                        (self.delta_ino, chunk_index as i64),
                    )
                    .await?;

                    written += to_write;
                }
            }

            let (now_secs, now_nsec) = current_timestamp()?;
            conn.execute(
                "UPDATE fs_inode
                 SET size = ?, data_inline = NULL, storage_kind = ?, mtime = ?, ctime = ?,
                     mtime_nsec = ?, ctime_nsec = ?
                 WHERE ino = ?",
                (
                    new_size as i64,
                    STORAGE_CHUNKED,
                    now_secs,
                    now_secs,
                    now_nsec,
                    now_nsec,
                    self.delta_ino,
                ),
            )
            .await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.delta.invalidate_attr(self.delta_ino);
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        let conn = self.delta.get_connection().await?;
        let txn = Transaction::new_unchecked(&conn, TransactionBehavior::Immediate).await?;

        let result: Result<()> = async {
            let current_size = self.delta_file_size_with_conn(&conn).await?;
            let chunk_size = self.chunk_size as u64;

            if size == 0 {
                conn.execute("DELETE FROM fs_data WHERE ino = ?", (self.delta_ino,))
                    .await?;
                conn.execute(
                    "DELETE FROM fs_chunk_override WHERE delta_ino = ?",
                    (self.delta_ino,),
                )
                .await?;
            } else if size < current_size {
                let last_chunk = (size - 1) / chunk_size;
                conn.execute(
                    "DELETE FROM fs_data WHERE ino = ? AND chunk_index > ?",
                    (self.delta_ino, last_chunk as i64),
                )
                .await?;
                conn.execute(
                    "DELETE FROM fs_chunk_override WHERE delta_ino = ? AND chunk_index > ?",
                    (self.delta_ino, last_chunk as i64),
                )
                .await?;

                let end_in_last_chunk = ((size - 1) % chunk_size + 1) as usize;
                if self.chunk_is_override_with_conn(&conn, last_chunk).await? {
                    let mut chunk = self
                        .delta_chunk_with_conn(&conn, last_chunk)
                        .await?
                        .unwrap_or_default();
                    if chunk.len() > end_in_last_chunk {
                        chunk.truncate(end_in_last_chunk);
                        conn.execute(
                            "UPDATE fs_data SET data = ? WHERE ino = ? AND chunk_index = ?",
                            (Value::Blob(chunk), self.delta_ino, last_chunk as i64),
                        )
                        .await?;
                    }
                }
            }

            let origin_base_size = self.partial_base_size_with_conn(&conn).await?;
            if size < origin_base_size {
                conn.execute(
                    "UPDATE fs_partial_origin SET base_size = ? WHERE delta_ino = ?",
                    (size as i64, self.delta_ino),
                )
                .await?;
            }

            let (now_secs, now_nsec) = current_timestamp()?;
            conn.execute(
                "UPDATE fs_inode
                 SET size = ?, data_inline = NULL, storage_kind = ?, mtime = ?, ctime = ?,
                     mtime_nsec = ?, ctime_nsec = ?
                 WHERE ino = ?",
                (
                    size as i64,
                    STORAGE_CHUNKED,
                    now_secs,
                    now_secs,
                    now_nsec,
                    now_nsec,
                    self.delta_ino,
                ),
            )
            .await?;
            Ok(())
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                self.delta.invalidate_attr(self.delta_ino);
                Ok(())
            }
            Err(e) => {
                let _ = txn.rollback().await;
                Err(e)
            }
        }
    }

    async fn fsync(&self) -> Result<()> {
        self.delta.fsync("/").await
    }

    async fn fstat(&self) -> Result<Stats> {
        let mut stats = FileSystem::getattr(&self.delta, self.delta_ino)
            .await?
            .ok_or(FsError::NotFound)?;
        stats.ino = self.overlay_ino;
        Ok(stats)
    }
}

impl OverlayPartialFile {
    async fn resolve_origin_base_stats(&self) -> Result<Option<Stats>> {
        let mut ino = ROOT_INO;
        if self.origin.base_path == "/" {
            return self.base.getattr(ino).await;
        }

        let mut stats = None;
        for component in self.origin.base_path.split('/').filter(|s| !s.is_empty()) {
            let Some(next) = self.base.lookup(ino, component).await? else {
                return Ok(None);
            };
            ino = next.ino;
            stats = Some(next);
        }
        Ok(stats)
    }

    async fn validate_current_origin(&self) -> Result<()> {
        let stats = self
            .resolve_origin_base_stats()
            .await?
            .ok_or(FsError::NotFound)?;
        if stats.size != self.origin.base_fingerprint_size
            || stats.mtime != self.origin.base_mtime
            || stats.mtime_nsec != self.origin.base_mtime_nsec
            || stats.ctime != self.origin.base_ctime
            || stats.ctime_nsec != self.origin.base_ctime_nsec
        {
            return Err(Error::Internal(format!(
                "partial-origin base changed for {}",
                self.origin.base_path
            )));
        }
        Ok(())
    }

    async fn delta_file_size_with_conn(&self, conn: &Connection) -> Result<u64> {
        let mut rows = conn
            .query("SELECT size FROM fs_inode WHERE ino = ?", (self.delta_ino,))
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64)
        } else {
            Err(FsError::NotFound.into())
        }
    }

    async fn partial_base_size_with_conn(&self, conn: &Connection) -> Result<u64> {
        let mut rows = conn
            .query(
                "SELECT base_size FROM fs_partial_origin WHERE delta_ino = ?",
                (self.delta_ino,),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            Ok(row
                .get_value(0)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u64)
        } else {
            Err(FsError::NotFound.into())
        }
    }

    async fn chunk_is_override_with_conn(
        &self,
        conn: &Connection,
        chunk_index: u64,
    ) -> Result<bool> {
        let mut rows = conn
            .query(
                "SELECT 1 FROM fs_chunk_override WHERE delta_ino = ? AND chunk_index = ?",
                (self.delta_ino, chunk_index as i64),
            )
            .await?;
        Ok(rows.next().await?.is_some())
    }

    async fn delta_chunk_with_conn(
        &self,
        conn: &Connection,
        chunk_index: u64,
    ) -> Result<Option<Vec<u8>>> {
        let mut rows = conn
            .query(
                "SELECT data FROM fs_data WHERE ino = ? AND chunk_index = ?",
                (self.delta_ino, chunk_index as i64),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            match row.get_value(0) {
                Ok(Value::Blob(data)) => Ok(Some(data)),
                _ => Ok(Some(Vec::new())),
            }
        } else {
            Ok(None)
        }
    }

    async fn read_merged_chunk_with_conn(
        &self,
        conn: &Connection,
        chunk_index: u64,
    ) -> Result<Vec<u8>> {
        if self.chunk_is_override_with_conn(conn, chunk_index).await? {
            let mut chunk = self
                .delta_chunk_with_conn(conn, chunk_index)
                .await?
                .unwrap_or_default();
            chunk.resize(self.chunk_size, 0);
            return Ok(chunk);
        }

        let base_size = self.partial_base_size_with_conn(conn).await?;
        let chunk_start = chunk_index
            .checked_mul(self.chunk_size as u64)
            .ok_or_else(|| Error::Internal("chunk offset overflow".to_string()))?;
        let mut chunk = if chunk_start < base_size {
            self.validate_current_origin().await?;
            let readable = std::cmp::min(self.chunk_size as u64, base_size - chunk_start);
            self.base_file.pread(chunk_start, readable).await?
        } else {
            Vec::new()
        };
        chunk.resize(self.chunk_size, 0);
        Ok(chunk)
    }
}

#[async_trait]
impl FileSystem for OverlayFS {
    async fn lookup(&self, parent_ino: i64, name: &str) -> Result<Option<Stats>> {
        crate::profiling::record_lookup();
        trace!(
            "OverlayFS::lookup: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check for whiteout
        if self.is_whiteout(&path) {
            crate::profiling::record_lookup_whiteout();
            crate::profiling::record_negative_lookup();
            return Ok(None);
        }

        // Try delta first
        let delta_parent_ino = self.resolve_delta_parent(&parent_info).await?;

        // Look up in delta (only if we resolved the correct parent)
        if let Some(delta_stats) = match delta_parent_ino {
            Some(ino) => {
                crate::profiling::record_lookup_delta();
                self.delta.lookup(ino, name).await?
            }
            None => None,
        } {
            let delta_ino = delta_stats.ino;
            let ino = self.get_or_create_overlay_ino(Layer::Delta, delta_ino, &path);
            let mut stats = delta_stats;

            // Origin mapping: reuse an existing Base overlay inode for stable
            // numbering within a session.  After remount the base_ino stored in
            // the mapping may be stale (the new HostFS has a fresh inode cache),
            // so only use it when the reverse_map already contains a live entry.
            // Otherwise keep the Delta overlay inode — the downstream code
            // already walks base from root when the parent is tagged Delta.
            if let Some(base_ino) = self.get_origin_ino(stats.ino) {
                if let Some(existing_ino) = self.live_origin_overlay_ino(base_ino, &path) {
                    self.refresh_overlay_mapping(existing_ino, Layer::Delta, delta_ino, &path);
                    stats.ino = existing_ino;
                } else {
                    stats.ino = ino;
                }
            } else {
                stats.ino = ino;
            }

            return Ok(Some(stats));
        }

        // Try base
        let base_parent_ino = if parent_info.layer == Layer::Base {
            parent_info.underlying_ino
        } else {
            // Need to find corresponding base parent by path
            // For root, use base root (1)
            if parent_info.path == "/" {
                1
            } else {
                // Walk the base to find the parent
                let mut base_ino: i64 = 1;
                let components: Vec<_> = parent_info
                    .path
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .collect();
                crate::profiling::record_path_resolution(components.len() as u64);
                for comp in components {
                    if let Some(s) = self.base.lookup(base_ino, comp).await? {
                        base_ino = s.ino;
                    } else {
                        crate::profiling::record_negative_lookup();
                        return Ok(None);
                    }
                }
                base_ino
            }
        };

        crate::profiling::record_lookup_base();
        if let Some(base_stats) = self.base.lookup(base_parent_ino, name).await? {
            let ino = self.get_or_create_overlay_ino(Layer::Base, base_stats.ino, &path);
            let mut stats = base_stats;
            stats.ino = ino;
            return Ok(Some(stats));
        }

        crate::profiling::record_negative_lookup();
        Ok(None)
    }

    async fn getattr(&self, ino: i64) -> Result<Option<Stats>> {
        crate::profiling::record_getattr();
        crate::profiling::record_attr_cache_miss();
        trace!("OverlayFS::getattr: ino={}", ino);

        let info = match self.get_inode_info(ino) {
            Some(i) => i,
            None => return Ok(None),
        };
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            crate::profiling::record_lookup_whiteout();
            return Ok(None);
        }

        let stats = match info.layer {
            Layer::Delta => FileSystem::getattr(&self.delta, info.underlying_ino).await?,
            Layer::Base => self.base.getattr(info.underlying_ino).await?,
        };

        Ok(stats.map(|mut s| {
            s.ino = ino;
            s
        }))
    }

    async fn readlink(&self, ino: i64) -> Result<Option<String>> {
        trace!("OverlayFS::readlink: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Ok(None);
        }

        match info.layer {
            Layer::Delta => FileSystem::readlink(&self.delta, info.underlying_ino).await,
            Layer::Base => self.base.readlink(info.underlying_ino).await,
        }
    }

    async fn readdir(&self, ino: i64) -> Result<Option<Vec<String>>> {
        crate::profiling::record_readdir();
        trace!("OverlayFS::readdir: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let child_whiteouts = self.get_child_whiteouts(&info.path);

        let mut entries = HashSet::new();

        // Get delta entries
        if info.layer == Layer::Delta {
            if let Some(delta_entries) = self.delta.readdir(info.underlying_ino).await? {
                for entry in delta_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry)
                    } else {
                        format!("{}/{}", info.path, entry)
                    };
                    if !self.is_whiteout(&entry_path) && !child_whiteouts.contains(&entry) {
                        entries.insert(entry);
                    }
                }
            }
        }

        // Get base entries (need to resolve base inode from path)
        let base_ino = if info.layer == Layer::Base {
            Some(info.underlying_ino)
        } else {
            // Walk base to find corresponding directory
            let components: Vec<&str> = info.path.split('/').filter(|s| !s.is_empty()).collect();
            let mut ino: i64 = 1;
            let mut found_all = true;
            crate::profiling::record_path_resolution(components.len() as u64);
            for comp in &components {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    found_all = false;
                    break;
                }
            }
            if found_all {
                Some(ino)
            } else {
                None
            }
        };

        if let Some(base_ino) = base_ino {
            if let Some(base_entries) = self.base.readdir(base_ino).await? {
                for entry in base_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry)
                    } else {
                        format!("{}/{}", info.path, entry)
                    };
                    if !self.is_whiteout(&entry_path) && !child_whiteouts.contains(&entry) {
                        entries.insert(entry);
                    }
                }
            }
        }

        let mut result: Vec<_> = entries.into_iter().collect();
        result.sort();
        Ok(Some(result))
    }

    async fn readdir_plus(&self, ino: i64) -> Result<Option<Vec<DirEntry>>> {
        crate::profiling::record_readdir_plus();
        trace!("OverlayFS::readdir_plus: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let child_whiteouts = self.get_child_whiteouts(&info.path);

        let mut entries_map: HashMap<String, DirEntry> = HashMap::new();

        // Get base entries first (so delta can override)
        let base_ino = if info.layer == Layer::Base {
            Some(info.underlying_ino)
        } else {
            let components: Vec<&str> = info.path.split('/').filter(|s| !s.is_empty()).collect();
            let mut ino: i64 = 1;
            let mut found_all = true;
            crate::profiling::record_path_resolution(components.len() as u64);
            for comp in &components {
                if let Some(s) = self.base.lookup(ino, comp).await? {
                    ino = s.ino;
                } else {
                    found_all = false;
                    break;
                }
            }
            if found_all {
                Some(ino)
            } else {
                None
            }
        };

        if let Some(base_ino) = base_ino {
            if let Some(base_entries) = self.base.readdir_plus(base_ino).await? {
                for mut entry in base_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", info.path, entry.name)
                    };

                    if !self.is_whiteout(&entry_path) && !child_whiteouts.contains(&entry.name) {
                        let overlay_ino = self.get_or_create_overlay_ino(
                            Layer::Base,
                            entry.stats.ino,
                            &entry_path,
                        );
                        entry.stats.ino = overlay_ino;
                        entries_map.insert(entry.name.clone(), entry);
                    }
                }
            }
        }

        // Get delta entries (override base)
        if info.layer == Layer::Delta {
            if let Some(delta_entries) = self.delta.readdir_plus(info.underlying_ino).await? {
                for mut entry in delta_entries {
                    let entry_path = if info.path == "/" {
                        format!("/{}", entry.name)
                    } else {
                        format!("{}/{}", info.path, entry.name)
                    };
                    if self.is_whiteout(&entry_path) || child_whiteouts.contains(&entry.name) {
                        continue;
                    }

                    // Check for origin mapping
                    let delta_ino = entry.stats.ino;
                    if let Some(base_ino) = self.get_origin_ino(entry.stats.ino) {
                        let overlay_ino =
                            self.get_or_create_overlay_ino(Layer::Delta, delta_ino, &entry_path);
                        if let Some(existing_ino) =
                            self.live_origin_overlay_ino(base_ino, &entry_path)
                        {
                            self.refresh_overlay_mapping(
                                existing_ino,
                                Layer::Delta,
                                delta_ino,
                                &entry_path,
                            );
                            entry.stats.ino = existing_ino;
                        } else {
                            entry.stats.ino = overlay_ino;
                        }
                    } else {
                        let overlay_ino = self.get_or_create_overlay_ino(
                            Layer::Delta,
                            entry.stats.ino,
                            &entry_path,
                        );
                        entry.stats.ino = overlay_ino;
                    }

                    entries_map.insert(entry.name.clone(), entry);
                }
            }
        }

        let mut result: Vec<_> = entries_map.into_values().collect();
        result.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(result))
    }

    async fn chmod(&self, ino: i64, mode: u32) -> Result<()> {
        trace!("OverlayFS::chmod: ino={}, mode={:o}", ino, mode);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => {
                let base_stats = self
                    .base
                    .getattr(info.underlying_ino)
                    .await?
                    .ok_or(FsError::NotFound)?;
                if self.partial_origin_policy.permits(&base_stats) {
                    self.partial_copy_up_and_update_mapping(ino, &info).await?
                } else {
                    self.copy_up_and_update_mapping(ino, &info).await?
                }
            }
        };

        self.delta.chmod(delta_ino, mode).await
    }

    async fn chown(&self, ino: i64, uid: Option<u32>, gid: Option<u32>) -> Result<()> {
        trace!(
            "OverlayFS::chown: ino={}, uid={:?}, gid={:?}",
            ino,
            uid,
            gid
        );

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => {
                let base_stats = self
                    .base
                    .getattr(info.underlying_ino)
                    .await?
                    .ok_or(FsError::NotFound)?;
                if self.partial_origin_policy.permits(&base_stats) {
                    self.partial_copy_up_and_update_mapping(ino, &info).await?
                } else {
                    self.copy_up_and_update_mapping(ino, &info).await?
                }
            }
        };

        self.delta.chown(delta_ino, uid, gid).await
    }

    async fn utimens(&self, ino: i64, atime: TimeChange, mtime: TimeChange) -> Result<()> {
        trace!("OverlayFS::utimens: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        let delta_ino = match info.layer {
            Layer::Delta => info.underlying_ino,
            Layer::Base => {
                let base_stats = self
                    .base
                    .getattr(info.underlying_ino)
                    .await?
                    .ok_or(FsError::NotFound)?;
                if self.partial_origin_policy.permits(&base_stats) {
                    self.partial_copy_up_and_update_mapping(ino, &info).await?
                } else {
                    self.copy_up_and_update_mapping(ino, &info).await?
                }
            }
        };

        self.delta.utimens(delta_ino, atime, mtime).await
    }

    async fn keep_cache_for_read_open(&self, ino: i64, flags: i32) -> Result<bool> {
        if is_write_open(flags) {
            return Ok(false);
        }

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        match info.layer {
            Layer::Base => {
                if self.is_whiteout(&info.path) {
                    return Ok(false);
                }
                let Some(stats) = self.base.getattr(info.underlying_ino).await? else {
                    return Ok(false);
                };
                Ok(stats.is_file())
            }
            // Delta (DB-backed) files inherit the AgentFS keep-cache policy:
            // the adapter fingerprint guard revalidates per open.
            Layer::Delta => {
                FileSystem::keep_cache_for_read_open(&self.delta, info.underlying_ino, flags).await
            }
        }
    }

    async fn open(&self, ino: i64, flags: i32) -> Result<BoxedFile> {
        trace!("OverlayFS::open: ino={}", ino);

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        if info.layer == Layer::Base && self.is_whiteout(&info.path) {
            return Err(FsError::NotFound.into());
        }

        match info.layer {
            Layer::Delta => {
                return self
                    .partial_file_for_delta(ino, info.underlying_ino, flags)
                    .await;
            }
            Layer::Base if !is_write_open(flags) => {
                return self.base.open(info.underlying_ino, flags).await;
            }
            Layer::Base => {
                let base_stats = self
                    .base
                    .getattr(info.underlying_ino)
                    .await?
                    .ok_or(FsError::NotFound)?;
                if self.partial_origin_policy.permits(&base_stats) {
                    let delta_ino = self.partial_copy_up_and_update_mapping(ino, &info).await?;
                    return self.partial_file_for_delta(ino, delta_ino, flags).await;
                }
            }
        }

        let delta_ino = self.copy_up_and_update_mapping(ino, &info).await?;

        FileSystem::open(&self.delta, delta_ino, flags).await
    }

    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Stats> {
        trace!("OverlayFS::mkdir: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if already exists
        if self.lookup(parent_ino, name).await?.is_some() {
            return Err(FsError::AlreadyExists.into());
        }

        // Remove whiteout if exists
        self.remove_whiteout(&path).await?;

        // Ensure parent dirs exist in delta
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats =
            FileSystem::mkdir(&self.delta, delta_parent_ino, name, mode, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

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
        trace!(
            "OverlayFS::create_file: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Remove whiteout if exists
        self.remove_whiteout(&path).await?;

        // Ensure parent dirs exist in delta
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let (mut stats, file) =
            FileSystem::create_file(&self.delta, delta_parent_ino, name, mode, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

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
        trace!("OverlayFS::mknod: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        self.remove_whiteout(&path).await?;
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats =
            FileSystem::mknod(&self.delta, delta_parent_ino, name, mode, rdev, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

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
        trace!(
            "OverlayFS::symlink: parent_ino={}, name={}, target={}",
            parent_ino,
            name,
            target
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        self.remove_whiteout(&path).await?;
        self.ensure_parent_dirs(&path, uid, gid).await?;

        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats =
            FileSystem::symlink(&self.delta, delta_parent_ino, name, target, uid, gid).await?;
        let overlay_ino = self.get_or_create_overlay_ino(Layer::Delta, stats.ino, &path);
        stats.ino = overlay_ino;

        Ok(stats)
    }

    async fn unlink(&self, parent_ino: i64, name: &str) -> Result<()> {
        trace!(
            "OverlayFS::unlink: parent_ino={}, name={}",
            parent_ino,
            name
        );

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if it exists
        let stats = self
            .lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound)?;
        if stats.is_directory() {
            return Err(FsError::IsADirectory.into());
        }

        // Try to remove from delta. Walk the delta layer to find the parent,
        // since the overlay parent may map to Base even when a copy-up exists in delta.
        if let Some(dpi) = self.resolve_delta_parent(&parent_info).await? {
            let removed_delta_ino = FileSystem::lookup(&self.delta, dpi, name)
                .await?
                .map(|stats| stats.ino);
            match FileSystem::unlink(&self.delta, dpi, name).await {
                Ok(()) => {}
                Err(crate::error::Error::Fs(FsError::NotFound)) => {}
                Err(e) => return Err(e),
            }
            if let Some(delta_ino) = removed_delta_ino {
                self.cleanup_partial_origin_if_unlinked(delta_ino).await?;
            }
        }

        // If the file is still visible through the overlay after delta removal,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(parent_ino, name).await?.is_some() {
            self.create_whiteout(&path).await?;
        }

        Ok(())
    }

    async fn rmdir(&self, parent_ino: i64, name: &str) -> Result<()> {
        trace!("OverlayFS::rmdir: parent_ino={}, name={}", parent_ino, name);

        let parent_info = self.get_inode_info(parent_ino).ok_or(FsError::NotFound)?;
        let path = self.build_path(parent_ino, name)?;

        // Check if it exists and is a directory
        let stats = self
            .lookup(parent_ino, name)
            .await?
            .ok_or(FsError::NotFound)?;
        if !stats.is_directory() {
            return Err(FsError::NotADirectory.into());
        }

        // Check if directory is empty (in overlay view)
        let dir_entries = self.readdir(stats.ino).await?.unwrap_or_default();
        if !dir_entries.is_empty() {
            return Err(FsError::NotEmpty.into());
        }

        // Try to remove from delta. Walk the delta layer to find the parent,
        // since the overlay parent may map to Base even when a copy-up exists in delta.
        if let Some(dpi) = self.resolve_delta_parent(&parent_info).await? {
            match FileSystem::rmdir(&self.delta, dpi, name).await {
                Ok(()) => {}
                Err(crate::error::Error::Fs(FsError::NotFound)) => {}
                Err(e) => return Err(e),
            }
        }

        // If the directory is still visible through the overlay after delta removal,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(parent_ino, name).await?.is_some() {
            self.create_whiteout(&path).await?;
        }

        Ok(())
    }

    async fn link(&self, ino: i64, newparent_ino: i64, newname: &str) -> Result<Stats> {
        trace!(
            "OverlayFS::link: ino={}, newparent_ino={}, newname={}",
            ino,
            newparent_ino,
            newname
        );

        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        let parent_info = self
            .get_inode_info(newparent_ino)
            .ok_or(FsError::NotFound)?;
        let new_path = self.build_path(newparent_ino, newname)?;

        // Ensure file is in delta (copy up if needed)
        let delta_ino = if info.layer == Layer::Delta {
            info.underlying_ino
        } else {
            self.copy_up(&info.path, info.underlying_ino).await?
        };

        self.remove_whiteout(&new_path).await?;
        self.ensure_parent_dirs(&new_path, 0, 0).await?;

        // Resolve delta parent AFTER ensure_parent_dirs so the directories exist.
        let delta_parent_ino = self
            .resolve_delta_parent(&parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        let mut stats = FileSystem::link(&self.delta, delta_ino, delta_parent_ino, newname).await?;
        stats.ino = ino; // Keep original overlay inode

        Ok(stats)
    }

    async fn rename(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> Result<()> {
        trace!(
            "OverlayFS::rename: oldparent={}, oldname={}, newparent={}, newname={}",
            oldparent_ino,
            oldname,
            newparent_ino,
            newname
        );

        let old_parent_info = self
            .get_inode_info(oldparent_ino)
            .ok_or(FsError::NotFound)?;
        let new_parent_info = self
            .get_inode_info(newparent_ino)
            .ok_or(FsError::NotFound)?;
        let old_path = self.build_path(oldparent_ino, oldname)?;
        let new_path = self.build_path(newparent_ino, newname)?;

        // Get source stats
        let src_stats = self
            .lookup(oldparent_ino, oldname)
            .await?
            .ok_or(FsError::NotFound)?;
        let src_info = self
            .get_inode_info(src_stats.ino)
            .ok_or(FsError::NotFound)?;

        // Ensure source is in delta first.
        let delta_src_ino = if src_info.layer == Layer::Base {
            self.copy_up(&old_path, src_info.underlying_ino).await?
        } else {
            src_info.underlying_ino
        };

        // Remove whiteout at destination
        self.remove_whiteout(&new_path).await?;
        self.ensure_parent_dirs(&new_path, 0, 0).await?;

        // Resolve delta parents AFTER copy_up / ensure_parent_dirs,
        // since those create the parent directories in delta.
        let delta_src_parent_ino = self
            .resolve_delta_parent(&old_parent_info)
            .await?
            .ok_or(FsError::NotFound)?;
        let delta_dst_parent_ino = self
            .resolve_delta_parent(&new_parent_info)
            .await?
            .ok_or(FsError::NotFound)?;

        // Perform rename in delta
        FileSystem::rename(
            &self.delta,
            delta_src_parent_ino,
            oldname,
            delta_dst_parent_ino,
            newname,
        )
        .await?;
        self.refresh_overlay_mapping(src_stats.ino, Layer::Delta, delta_src_ino, &new_path);

        // If the old file is still visible through the overlay after the rename,
        // it must be coming from the base layer — create a whiteout to hide it.
        if self.lookup(oldparent_ino, oldname).await?.is_some() {
            self.create_whiteout(&old_path).await?;
        }

        Ok(())
    }

    async fn statfs(&self) -> Result<FilesystemStats> {
        FileSystem::statfs(&self.delta).await
    }

    async fn drain_inode_writes(&self, ino: i64) -> Result<()> {
        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        match info.layer {
            Layer::Delta => FileSystem::drain_inode_writes(&self.delta, info.underlying_ino).await,
            Layer::Base => self.base.drain_inode_writes(info.underlying_ino).await,
        }
    }

    async fn drain_all(&self) -> Result<()> {
        FileSystem::drain_all(&self.delta).await?;
        self.base.drain_all().await?;
        Ok(())
    }

    async fn finalize(&self) -> Result<()> {
        FileSystem::finalize(&self.delta).await?;
        self.base.finalize().await?;
        Ok(())
    }

    async fn retain_lookup(&self, ino: i64, nlookup: u64) -> Result<()> {
        let info = self.get_inode_info(ino).ok_or(FsError::NotFound)?;
        match info.layer {
            Layer::Delta => {
                FileSystem::retain_lookup(&self.delta, info.underlying_ino, nlookup).await
            }
            Layer::Base => self.base.retain_lookup(info.underlying_ino, nlookup).await,
        }
    }

    async fn forget(&self, ino: i64, nlookup: u64) {
        // Look up the inode info to determine which layer it belongs to
        let info = match self.get_inode_info(ino) {
            Some(i) => i,
            None => return, // Unknown inode, nothing to forget
        };

        // Pass through to the appropriate layer
        match info.layer {
            Layer::Delta => {
                // Delta (AgentFS) doesn't cache fds, but call it anyway for completeness
                FileSystem::forget(&self.delta, info.underlying_ino, nlookup).await;
            }
            Layer::Base => {
                // Base layer (HostFS) caches O_PATH fds and needs forget
                self.base.forget(info.underlying_ino, nlookup).await;
            }
        }

        // Note: We don't remove from inode_map here because the overlay layer's
        // inode mapping is relatively lightweight (no fd). The base layer's
        // forget handles the actual fd cleanup.
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use crate::filesystem::HostFS;
    use crate::DEFAULT_FILE_MODE;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    async fn create_test_overlay() -> Result<(OverlayFS, tempfile::TempDir, tempfile::TempDir)> {
        let base_dir = tempdir()?;
        std::fs::write(base_dir.path().join("base.txt"), b"base content")?;
        std::fs::create_dir(base_dir.path().join("subdir"))?;
        std::fs::write(base_dir.path().join("subdir/nested.txt"), b"nested")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        Ok((overlay, base_dir, delta_dir))
    }

    #[tokio::test]
    async fn test_overlay_lookup_base() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup file from base
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert!(stats.is_file());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_create_in_delta() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Create file in delta
        let (stats, file) = overlay
            .create_file(ROOT_INO, "new.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"new content").await?;

        // Verify it exists
        let lookup_stats = overlay.lookup(ROOT_INO, "new.txt").await?.unwrap();
        assert_eq!(lookup_stats.ino, stats.ino);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_whiteout() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // File exists initially
        assert!(overlay.lookup(ROOT_INO, "base.txt").await?.is_some());

        // Delete it
        overlay.unlink(ROOT_INO, "base.txt").await?;

        // File should be gone
        assert!(overlay.lookup(ROOT_INO, "base.txt").await?.is_none());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert!(stats.is_file());

        // Open and write to it (should trigger copy-up)
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"modified content").await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("base.txt"))?;
        assert_eq!(
            base_content, b"base content",
            "base file should be unchanged"
        );

        // Verify reading through overlay returns modified content
        let read_back = file.pread(0, 100).await?;
        assert_eq!(
            read_back, b"modified content",
            "overlay should return modified content"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_read_only_base_open_does_not_copy_up() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDONLY).await?;

        assert_eq!(file.pread(0, 100).await?, b"base content");
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_origin").await?,
            0,
            "read-only open of a base file should not create origin mappings"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await?,
            0,
            "read-only open of a base file should not copy file bytes into delta"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_keep_cache_only_for_read_only_base_files() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert!(
            overlay
                .keep_cache_for_read_open(stats.ino, libc::O_RDONLY)
                .await?,
            "read-only base files are eligible for FOPEN_KEEP_CACHE"
        );
        assert!(
            !overlay
                .keep_cache_for_read_open(stats.ino, libc::O_RDWR)
                .await?,
            "writable opens must not keep the base page cache"
        );

        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"modified content").await?;
        assert!(
            overlay
                .keep_cache_for_read_open(stats.ino, libc::O_RDONLY)
                .await?,
            "delta-backed files stay keep-cache eligible; staleness is the \
             adapter fingerprint guard's job"
        );
        // The fingerprint inputs must have moved across the copy-up + write so
        // the adapter rejects any pages cached against the base version.
        let after = overlay.getattr(stats.ino).await?.unwrap();
        assert!(
            (after.size, after.mtime, after.mtime_nsec, after.ctime)
                != (stats.size, stats.mtime, stats.mtime_nsec, stats.ctime),
            "copy-up + write must change the stats fingerprint"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_inode_stability() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file and record its inode
        let stats_before = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let ino_before = stats_before.ino;

        // Open triggers copy-up
        let file = overlay.open(stats_before.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"modified").await?;

        // Lookup again - inode should be the same
        let stats_after = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert_eq!(
            stats_after.ino, ino_before,
            "inode should remain stable after copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_origin_mapping_rejects_wrong_path_base_inode() -> Result<()> {
        let (overlay, _base_dir, _delta_dir) = create_test_overlay().await?;

        let subdir = overlay.lookup(ROOT_INO, "subdir").await?.unwrap();
        let nested = overlay.lookup(subdir.ino, "nested.txt").await?.unwrap();
        let nested_base_ino = overlay.get_inode_info(nested.ino).unwrap().underlying_ino;

        let (delta_stats, _file) = <AgentFS as FileSystem>::create_file(
            overlay.delta(),
            ROOT_INO,
            "base.txt",
            DEFAULT_FILE_MODE,
            0,
            0,
        )
        .await?;
        overlay
            .add_origin_mapping(delta_stats.ino, nested_base_ino)
            .await?;

        let resolved = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert_ne!(
            resolved.ino, nested.ino,
            "origin mapping must not reuse a live base inode for a different path"
        );
        assert_eq!(
            overlay.get_inode_info(resolved.ino).unwrap().path,
            "/base.txt"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_single_byte_write_stores_one_chunk() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size * 3 + 17, 0x21);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        let write_offset = chunk_size as u64 + 123;
        file.pwrite(write_offset, b"Z").await?;

        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await?,
            1,
            "single-byte partial-origin write should materialize one chunk"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            1,
            "single-byte partial-origin write should record one chunk override"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT SUM(LENGTH(data)) FROM fs_data").await?,
            chunk_size as i64,
            "materialized chunk should be bounded to the configured chunk size"
        );

        let read_back = file.pread(write_offset - 2, 5).await?;
        let mut expected =
            base_content[write_offset as usize - 2..write_offset as usize + 3].to_vec();
        expected[2] = b'Z';
        assert_eq!(read_back, expected);
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            base_content,
            "base file should remain unchanged"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_policy_off_uses_whole_copy_up() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size * 2 + 11, 0x17);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin_policy(
            base,
            delta,
            PartialOriginPolicy::new(PartialOriginMode::Off),
        );
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(chunk_size as u64 + 3, b"X").await?;

        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            0,
            "explicit off policy must keep whole-file copy-up semantics"
        );
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            base_content,
            "whole-file copy-up must not mutate the base file"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_policy_auto_threshold() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let threshold = (chunk_size * 2) as u64;
        let small_content = patterned_bytes(chunk_size + 31, 0x05);
        let large_content = patterned_bytes(chunk_size * 2 + 31, 0x55);
        std::fs::write(base_dir.path().join("small.bin"), &small_content)?;
        std::fs::write(base_dir.path().join("large.bin"), &large_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin_policy(
            base,
            delta,
            PartialOriginPolicy::new(PartialOriginMode::Auto).with_threshold_bytes(threshold),
        );
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let small_stats = overlay.lookup(ROOT_INO, "small.bin").await?.unwrap();
        let small_file = overlay.open(small_stats.ino, libc::O_RDWR).await?;
        small_file.pwrite(3, b"s").await?;
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            0,
            "auto policy should whole-copy files below the threshold"
        );

        let large_stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let large_file = overlay.open(large_stats.ino, libc::O_RDWR).await?;
        large_file.pwrite(chunk_size as u64 + 7, b"L").await?;
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1,
            "auto policy should use partial-origin at or above the threshold"
        );
        assert_eq!(
            std::fs::read(base_dir.path().join("small.bin"))?,
            small_content,
            "small-file write must not mutate the base file"
        );
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            large_content,
            "large-file partial-origin write must not mutate the base file"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_metadata_paths_do_not_mutate_base() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let base_path = base_dir.path().join("file.txt");
        std::fs::write(&base_path, b"metadata base")?;

        let base_meta_before = std::fs::metadata(&base_path)?;
        let base_mode_before = base_meta_before.permissions().mode() & 0o777;
        let base_modified_before = base_meta_before.modified()?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin_policy(
            base,
            delta,
            PartialOriginPolicy::new(PartialOriginMode::On),
        );
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "file.txt").await?.unwrap();
        overlay.chmod(stats.ino, 0o600).await?;
        overlay
            .utimens(
                stats.ino,
                TimeChange::Set(123, 456),
                TimeChange::Set(789, 123),
            )
            .await?;

        let overlay_stats = overlay.getattr(stats.ino).await?.unwrap();
        assert_eq!(overlay_stats.mode & 0o777, 0o600);
        assert_eq!(overlay_stats.atime, 123);
        assert_eq!(overlay_stats.atime_nsec, 456);
        assert_eq!(overlay_stats.mtime, 789);
        assert_eq!(overlay_stats.mtime_nsec, 123);
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1,
            "metadata-only paths should target partial-origin delta metadata"
        );

        let base_meta_after = std::fs::metadata(&base_path)?;
        assert_eq!(
            base_meta_after.permissions().mode() & 0o777,
            base_mode_before,
            "chmod through overlay must not mutate base permissions"
        );
        assert_eq!(
            base_meta_after.modified()?,
            base_modified_before,
            "utimens through overlay must not mutate base mtime"
        );
        assert_eq!(std::fs::read(&base_path)?, b"metadata base");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_reads_across_override_boundaries() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size * 2 + 32, 0x42);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        let write_offset = chunk_size as u64 - 2;
        file.pwrite(write_offset, b"WXYZ").await?;
        expected[write_offset as usize..write_offset as usize + 4].copy_from_slice(b"WXYZ");

        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await?,
            2,
            "cross-boundary write should materialize only the two touched chunks"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            2
        );

        let read_back = file.pread(chunk_size as u64 - 4, 8).await?;
        assert_eq!(
            read_back,
            expected[chunk_size - 4..chunk_size + 4],
            "read should merge delta-owned chunks with base fallback bytes"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_truncate_extend_does_not_reexpose_base_tail() -> Result<()>
    {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size * 2, 0x63);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.truncate((chunk_size + 5) as u64).await?;
        file.truncate((chunk_size + 12) as u64).await?;

        let after_extend = file.pread(chunk_size as u64 + 4, 8).await?;
        let mut expected = vec![base_content[chunk_size + 4]];
        expected.extend(std::iter::repeat_n(0u8, 7));
        assert_eq!(
            after_extend, expected,
            "extend after shrink should return zeros instead of base fallback past the shrink point"
        );
        assert_eq!(file.fstat().await?.size, (chunk_size + 12) as i64);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_open_truncates_base_file_mapping() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        std::fs::write(base_dir.path().join("large.bin"), b"base contents")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay
            .open(stats.ino, libc::O_RDWR | libc::O_TRUNC)
            .await?;
        assert_eq!(file.fstat().await?.size, 0);
        assert_eq!(file.pread(0, 32).await?, b"");
        assert_eq!(overlay.getattr(stats.ino).await?.unwrap().size, 0);
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            b"base contents",
            "O_TRUNC through the overlay must not mutate the base file"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_open_truncates_existing_partial_file() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        std::fs::write(base_dir.path().join("large.bin"), b"base contents")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(5, b"X").await?;
        assert_eq!(file.pread(0, 16).await?, b"base Xontents");

        let truncated = overlay
            .open(stats.ino, libc::O_RDWR | libc::O_TRUNC)
            .await?;
        assert_eq!(truncated.fstat().await?.size, 0);
        assert_eq!(truncated.pread(0, 32).await?, b"");
        assert_eq!(overlay.getattr(stats.ino).await?.unwrap().size, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_survives_remount() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size * 2 + 9, 0x31);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        let write_offset = chunk_size as u64 + 7;
        file.pwrite(write_offset, b"R").await?;
        file.fsync().await?;
        expected[write_offset as usize] = b'R';

        drop(file);
        drop(overlay);

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        let stats = reopened.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = reopened.open(stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            file.pread(chunk_size as u64 + 4, 8).await?,
            expected[chunk_size + 4..chunk_size + 12],
            "partial-origin reads must resolve persisted base_path after remount"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_readdir_plus_survives_remount() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size + 9, 0x41);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(4, b"Q").await?;
        file.fsync().await?;
        expected[4] = b'Q';
        drop(file);
        drop(overlay);

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        let entries = reopened.readdir_plus(ROOT_INO).await?.unwrap();
        let entry = entries
            .into_iter()
            .find(|entry| entry.name == "large.bin")
            .expect("large.bin from readdir_plus");
        let file = reopened.open(entry.stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            file.pread(0, 8).await?,
            expected[..8],
            "readdir_plus inode should open the partial-origin delta view after remount"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_rename_keeps_live_mapping() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size + 16, 0x51);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(3, b"Z").await?;
        expected[3] = b'Z';
        drop(file);

        overlay
            .rename(ROOT_INO, "large.bin", ROOT_INO, "renamed.bin")
            .await?;
        assert!(overlay.lookup(ROOT_INO, "large.bin").await?.is_none());
        let renamed = overlay.lookup(ROOT_INO, "renamed.bin").await?.unwrap();
        let file = overlay.open(renamed.ino, libc::O_RDONLY).await?;
        assert_eq!(file.pread(0, 8).await?, expected[..8]);

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_detects_base_drift() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size + 16, 0x71);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"Z").await?;
        drop(file);
        drop(overlay);

        std::fs::write(base_dir.path().join("large.bin"), b"changed base")?;

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        let stats = reopened.lookup(ROOT_INO, "large.bin").await?.unwrap();
        assert!(
            reopened.open(stats.ino, libc::O_RDONLY).await.is_err(),
            "partial-origin files should fail loudly when the base fallback changed"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_detects_base_drift_after_open() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_file = base_dir.path().join("large.bin");
        let base_content = patterned_bytes(chunk_size * 2, 0x37);
        std::fs::write(&base_file, &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(chunk_size as u64, b"X").await?;

        let read_handle = overlay.open(stats.ino, libc::O_RDONLY).await?;
        std::fs::write(&base_file, patterned_bytes(chunk_size * 2, 0x91))?;

        let err = read_handle.pread(0, 8).await.unwrap_err();
        assert!(
            err.to_string().contains("partial-origin base changed"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_detects_same_size_base_drift() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size + 16, 0x73);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"Z").await?;
        drop(file);
        drop(overlay);

        std::thread::sleep(std::time::Duration::from_millis(10));
        let changed_same_size = patterned_bytes(base_content.len(), 0x74);
        std::fs::write(base_dir.path().join("large.bin"), changed_same_size)?;

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        let stats = reopened.lookup(ROOT_INO, "large.bin").await?.unwrap();
        assert!(
            reopened.open(stats.ino, libc::O_RDONLY).await.is_err(),
            "partial-origin files should fail loudly when same-size base fallback content changed"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_main_db_snapshot_restore() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let restored_db_path = delta_dir.path().join("restored.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size * 2 + 33, 0x91);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        let write_offset = chunk_size as u64 + 11;
        file.pwrite(write_offset, b"S").await?;
        file.fsync().await?;
        expected[write_offset as usize] = b'S';
        drop(file);
        drop(overlay);

        std::fs::copy(&db_path, &restored_db_path)?;

        let restored_delta = AgentFS::new(restored_db_path.to_str().unwrap()).await?;
        let restored_base = Arc::new(HostFS::new(base_dir.path())?);
        let restored = OverlayFS::new_with_partial_origin(restored_base, restored_delta, true);
        restored.init(base_dir.path().to_str().unwrap()).await?;

        let restored_stats = restored.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let restored_file = restored.open(restored_stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            restored_file.pread(chunk_size as u64 + 8, 8).await?,
            expected[chunk_size + 8..chunk_size + 16],
            "main-db snapshot restore should preserve partial-origin metadata and chunk overrides"
        );
        assert_eq!(
            scalar_i64(&restored, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1
        );
        assert_eq!(
            scalar_i64(&restored, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            1
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_unlink_cleans_metadata_and_whiteouts_base() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size + 19, 0xa1);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(chunk_size as u64 + 1, b"U").await?;
        drop(file);
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1
        );

        overlay.unlink(ROOT_INO, "large.bin").await?;

        assert!(overlay.lookup(ROOT_INO, "large.bin").await?.is_none());
        assert_eq!(
            std::fs::read(base_dir.path().join("large.bin"))?,
            base_content,
            "unlink should not mutate the base file"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            0,
            "last unlink should remove partial-origin rows"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_chunk_override").await?,
            0,
            "last unlink should remove chunk override rows"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_origin").await?,
            0,
            "last unlink should remove origin rows"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_hardlink_survives_source_unlink() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size + 21, 0xb1);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(5, b"H").await?;
        expected[5] = b'H';
        drop(file);

        overlay.link(stats.ino, ROOT_INO, "linked.bin").await?;
        let linked = overlay.lookup(ROOT_INO, "linked.bin").await?.unwrap();
        assert_eq!(linked.ino, stats.ino);
        assert_eq!(linked.nlink, 2);
        let linked_file = overlay.open(linked.ino, libc::O_RDONLY).await?;
        assert_eq!(linked_file.pread(0, 8).await?, expected[..8]);
        drop(linked_file);

        overlay.unlink(ROOT_INO, "large.bin").await?;
        assert!(overlay.lookup(ROOT_INO, "large.bin").await?.is_none());
        let linked_after = overlay.lookup(ROOT_INO, "linked.bin").await?.unwrap();
        let linked_file = overlay.open(linked_after.ino, libc::O_RDONLY).await?;
        assert_eq!(
            linked_file.pread(0, 8).await?,
            expected[..8],
            "hardlink should retain merged partial-origin contents after source unlink"
        );
        assert_eq!(linked_file.fstat().await?.nlink, 1);
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            1,
            "partial-origin metadata should remain while a hardlink keeps the inode alive"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_partial_origin_renamed_file_readdir_plus_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let mut expected = patterned_bytes(chunk_size + 23, 0xc1);
        std::fs::write(base_dir.path().join("large.bin"), &expected)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, true);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(7, b"N").await?;
        file.fsync().await?;
        expected[7] = b'N';
        drop(file);

        overlay
            .rename(ROOT_INO, "large.bin", ROOT_INO, "renamed.bin")
            .await?;
        drop(overlay);

        let reopened_delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let reopened_base = Arc::new(HostFS::new(base_dir.path())?);
        let reopened = OverlayFS::new_with_partial_origin(reopened_base, reopened_delta, true);
        reopened.init(base_dir.path().to_str().unwrap()).await?;

        assert!(reopened.lookup(ROOT_INO, "large.bin").await?.is_none());
        let entries = reopened.readdir_plus(ROOT_INO).await?.unwrap();
        let renamed = entries
            .into_iter()
            .find(|entry| entry.name == "renamed.bin")
            .expect("renamed.bin from readdir_plus");
        let file = reopened.open(renamed.stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            file.pread(0, 10).await?,
            expected[..10],
            "renamed partial-origin file from readdir_plus should open after remount"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_default_copy_up_still_copies_whole_base_file() -> Result<()> {
        let base_dir = tempdir()?;
        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let chunk_size = delta.chunk_size();
        let base_content = patterned_bytes(chunk_size * 3 + 17, 0x84);
        std::fs::write(base_dir.path().join("large.bin"), &base_content)?;

        let base = Arc::new(HostFS::new(base_dir.path())?);
        let overlay = OverlayFS::new_with_partial_origin(base, delta, false);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let stats = overlay.lookup(ROOT_INO, "large.bin").await?.unwrap();
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.pwrite(chunk_size as u64 + 123, b"Z").await?;
        // Tier Four: pwrite is batched in the delta SDK now; flush so the
        // fs_data row count below reflects the committed copy-up chunks.
        file.fsync().await?;

        assert!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_data").await? > 1,
            "default overlay open/write path should keep whole-file copy-up behavior"
        );
        assert_eq!(
            scalar_i64(&overlay, "SELECT COUNT(*) FROM fs_partial_origin").await?,
            0,
            "partial-origin metadata must stay opt-in"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_chmod() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        let ino_before = stats.ino;

        // chmod should trigger copy-up
        overlay.chmod(stats.ino, 0o755).await?;

        // Verify base file mode is UNCHANGED
        let base_meta = std::fs::metadata(base_dir.path().join("base.txt"))?;
        assert_ne!(
            base_meta.permissions().mode() & 0o777,
            0o755,
            "base file mode should be unchanged"
        );

        // Verify overlay returns new mode
        let stats_after = overlay.getattr(stats.ino).await?.unwrap();
        assert_eq!(
            stats_after.mode & 0o777,
            0o755,
            "overlay should return new mode"
        );

        // Inode should remain stable
        assert_eq!(
            stats_after.ino, ino_before,
            "inode should remain stable after chmod copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_truncate() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file
        let stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();
        assert_eq!(stats.size, 12); // "base content"

        // Open and truncate (triggers copy-up via open)
        let file = overlay.open(stats.ino, libc::O_RDWR).await?;
        file.truncate(5).await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("base.txt"))?;
        assert_eq!(
            base_content, b"base content",
            "base file should be unchanged"
        );

        // Verify overlay returns truncated size
        let stats_after = file.fstat().await?;
        assert_eq!(stats_after.size, 5, "overlay should return truncated size");

        // Verify content is truncated
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"base ", "content should be truncated");

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_rename() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup base file (to populate overlay state)
        let _stats = overlay.lookup(ROOT_INO, "base.txt").await?.unwrap();

        // Rename should trigger copy-up
        overlay
            .rename(ROOT_INO, "base.txt", ROOT_INO, "renamed.txt")
            .await?;

        // Base file should still exist (we don't modify base)
        assert!(
            base_dir.path().join("base.txt").exists(),
            "base file should still exist"
        );

        // Old name should be gone in overlay (whiteout)
        assert!(
            overlay.lookup(ROOT_INO, "base.txt").await?.is_none(),
            "old name should be gone"
        );

        // New name should exist in overlay
        let renamed_stats = overlay.lookup(ROOT_INO, "renamed.txt").await?.unwrap();
        assert!(renamed_stats.is_file());

        // Content should be preserved
        let file = overlay.open(renamed_stats.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(
            content, b"base content",
            "content should be preserved after rename"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_nested_file() -> Result<()> {
        let (overlay, base_dir, _delta_dir) = create_test_overlay().await?;

        // Lookup nested file in subdir
        let subdir_stats = overlay.lookup(ROOT_INO, "subdir").await?.unwrap();
        let nested_stats = overlay
            .lookup(subdir_stats.ino, "nested.txt")
            .await?
            .unwrap();

        // Open and modify (triggers copy-up, should also create parent dir in delta)
        let file = overlay.open(nested_stats.ino, libc::O_RDWR).await?;
        file.pwrite(0, b"modified nested").await?;

        // Verify base file is UNCHANGED
        let base_content = std::fs::read(base_dir.path().join("subdir/nested.txt"))?;
        assert_eq!(
            base_content, b"nested",
            "base nested file should be unchanged"
        );

        // Verify overlay returns modified content
        let content = file.pread(0, 100).await?;
        assert_eq!(
            content, b"modified nested",
            "overlay should return modified content"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_copy_on_write_symlink() -> Result<()> {
        // Create overlay with a symlink in base
        let base_dir = tempdir()?;
        std::fs::write(base_dir.path().join("target.txt"), b"target content")?;
        std::os::unix::fs::symlink("target.txt", base_dir.path().join("link.txt"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup symlink
        let link_stats = overlay.lookup(ROOT_INO, "link.txt").await?.unwrap();
        assert!(link_stats.is_symlink());

        // Read the symlink target
        let target = overlay.readlink(link_stats.ino).await?.unwrap();
        assert_eq!(target, "target.txt");

        // chmod on symlink triggers copy-up
        overlay.chmod(link_stats.ino, 0o755).await?;

        // Verify symlink target is preserved after copy-up
        let target_after = overlay.readlink(link_stats.ino).await?.unwrap();
        assert_eq!(
            target_after, "target.txt",
            "symlink target should be preserved after copy-up"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_create_file_in_deeply_nested_base_dir() -> Result<()> {
        // This test reproduces a bug where ensure_parent_dirs uses delta inodes
        // to lookup in base layer, which breaks for paths deeper than one level.
        //
        // Setup: base has /a/b/c/ directory structure
        // Test: create a new file at /a/b/c/new.txt
        // Bug: ensure_parent_dirs would use delta inode for "a" to lookup "b" in base
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;
        std::fs::write(base_dir.path().join("a/b/c/existing.txt"), b"existing")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to the nested directory
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        assert!(a_stats.is_directory());
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        assert!(b_stats.is_directory());
        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();
        assert!(c_stats.is_directory());

        // Create a new file in the deeply nested directory
        // This should trigger ensure_parent_dirs to create /a/b/c in delta
        let (new_stats, file) = overlay
            .create_file(c_stats.ino, "new.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"new content").await?;

        // Verify the file was created
        assert!(new_stats.is_file());

        // Verify we can read it back
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"new content");

        // Verify the existing file in base is still accessible
        let existing_stats = overlay.lookup(c_stats.ino, "existing.txt").await?.unwrap();
        let existing_file = overlay.open(existing_stats.ino, libc::O_RDONLY).await?;
        let existing_content = existing_file.pread(0, 100).await?;
        assert_eq!(existing_content, b"existing");

        // Verify base is unchanged
        assert!(base_dir.path().join("a/b/c/existing.txt").exists());
        assert!(!base_dir.path().join("a/b/c/new.txt").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_mkdir_in_deeply_nested_base_dir() -> Result<()> {
        // Similar test but for mkdir instead of create_file
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to the nested directory
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();

        // Create a new subdirectory in the deeply nested directory
        let new_dir_stats = overlay.mkdir(c_stats.ino, "newdir", 0o755, 0, 0).await?;
        assert!(new_dir_stats.is_directory());

        // Verify we can create a file inside the new directory
        let (file_stats, file) = overlay
            .create_file(new_dir_stats.ino, "file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"nested file").await?;
        assert!(file_stats.is_file());

        // Verify base is unchanged
        assert!(!base_dir.path().join("a/b/c/newdir").exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_overlay_lookup_after_mkdir_in_base_parent() -> Result<()> {
        // This test reproduces a bug where lookup uses delta root (inode 1)
        // when parent is in Base layer, instead of walking the delta path.
        //
        // Scenario (mimics FUSE behavior):
        // 1. Lookup "target" in root → gets base layer inode
        // 2. mkdir("debug") inside "target" → creates /target/debug in delta
        // 3. Lookup "debug" in "target" → should find it, but bug causes it to
        //    look at delta root instead of delta's "/target"
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("target"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup "target" - this creates a Base layer mapping
        let target_stats = overlay.lookup(ROOT_INO, "target").await?.unwrap();
        assert!(target_stats.is_directory());

        // Step 2: Create "debug" inside "target"
        // This should create /target in delta, then /target/debug in delta
        let debug_stats = overlay
            .mkdir(target_stats.ino, "debug", 0o755, 0, 0)
            .await?;
        assert!(debug_stats.is_directory());

        // Step 3: Lookup "debug" inside "target" - this is where the bug manifests!
        // The bug: lookup uses delta root (1) when parent is Base layer,
        // so it looks for "debug" at delta root instead of delta's "/target"
        let debug_lookup = overlay.lookup(target_stats.ino, "debug").await?;
        assert!(
            debug_lookup.is_some(),
            "Should find 'debug' inside 'target' after mkdir"
        );
        assert!(debug_lookup.unwrap().is_directory());

        // Also verify we can create files inside the new directory
        let (file_stats, file) = overlay
            .create_file(debug_stats.ino, "test.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"test content").await?;
        assert!(file_stats.is_file());

        // And lookup should find the file too
        let file_lookup = overlay.lookup(debug_stats.ino, "test.txt").await?;
        assert!(
            file_lookup.is_some(),
            "Should find 'test.txt' inside 'debug'"
        );

        Ok(())
    }

    /// Test that lookup in a base subdirectory does not return an unrelated
    /// delta entry with the same name from a wrong parent.
    ///
    /// Reproduces the ENOTDIR bug:
    ///   1. Base has /sdk/rust/ (directories)
    ///   2. Delta has a *file* named "rust" under delta root (from some unrelated op)
    ///   3. lookup(sdk_ino, "rust") should return the base *directory*, not the delta file
    ///
    /// The bug: when parent is Base layer, the delta path walk breaks early
    /// (because "sdk" doesn't exist in delta) and uses delta root as parent.
    /// Then delta.lookup(root, "rust") finds the unrelated file and returns it.
    #[tokio::test]
    async fn test_overlay_lookup_base_subdir_not_shadowed_by_wrong_delta_parent() -> Result<()> {
        // Base: /sdk/rust/Cargo.toml (nested directories + file)
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("sdk/rust"))?;
        std::fs::write(
            base_dir.path().join("sdk/rust/Cargo.toml"),
            b"[package]\nname = \"test\"",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create a *file* named "rust" at the overlay root (in delta).
        // This is the entry that could shadow the base directory if the delta
        // path walk uses the wrong parent inode.
        let (_file_stats, file) = overlay
            .create_file(ROOT_INO, "rust", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"this is a file, not a directory").await?;

        // Lookup "sdk" from root — should be a base directory
        let sdk_stats = overlay.lookup(ROOT_INO, "sdk").await?.unwrap();
        assert!(sdk_stats.is_directory(), "sdk should be a directory");

        // Lookup "rust" under "sdk" — MUST return the base *directory*, not
        // the delta *file* named "rust" that lives under root.
        let rust_stats = overlay.lookup(sdk_stats.ino, "rust").await?.unwrap();
        assert!(
            rust_stats.is_directory(),
            "sdk/rust should be a directory from base, not the file from delta root"
        );

        // Verify we can traverse further into sdk/rust/Cargo.toml
        let toml_stats = overlay.lookup(rust_stats.ino, "Cargo.toml").await?.unwrap();
        assert!(toml_stats.is_file(), "Cargo.toml should be a file");

        Ok(())
    }

    /// Test that readdir_plus and lookup agree on entry types for base dirs.
    ///
    /// readdir_plus for a Base-layer directory only returns base entries,
    /// while lookup checks delta first. They must agree on types.
    #[tokio::test]
    async fn test_overlay_readdir_plus_consistent_with_lookup_for_base_dir() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("sdk/rust"))?;
        std::fs::write(base_dir.path().join("sdk/rust/lib.rs"), b"fn main() {}")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create a file named "rust" at the root in delta (wrong-parent scenario)
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "rust", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy").await?;

        // Lookup "sdk" to get its overlay inode
        let sdk_stats = overlay.lookup(ROOT_INO, "sdk").await?.unwrap();

        // readdir_plus on "sdk" should list "rust" as a directory
        let entries = overlay
            .readdir_plus(sdk_stats.ino)
            .await?
            .expect("readdir_plus should succeed on sdk");
        let rust_entry = entries.iter().find(|e| e.name == "rust");
        assert!(rust_entry.is_some(), "readdir_plus should list 'rust'");
        assert!(
            rust_entry.unwrap().stats.is_directory(),
            "readdir_plus should report 'rust' as directory"
        );

        // lookup on "sdk" for "rust" should also return a directory
        let rust_lookup = overlay.lookup(sdk_stats.ino, "rust").await?.unwrap();
        assert!(
            rust_lookup.is_directory(),
            "lookup should also report 'rust' as directory (consistent with readdir_plus)"
        );

        Ok(())
    }

    /// Test lookup through deeply nested base directories when an unrelated
    /// file exists at an intermediate name in the delta root.
    ///
    /// Base: /a/b/c/file.txt
    /// Delta root has file named "b"
    /// lookup(a_ino, "b") must return the base directory, not the delta file.
    #[tokio::test]
    async fn test_overlay_lookup_deep_nesting_with_delta_name_collision() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b/c"))?;
        std::fs::write(base_dir.path().join("a/b/c/file.txt"), b"deep content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Create files named "b" and "c" at delta root — potential collisions
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "b", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy b").await?;
        let (_stats, file) = overlay
            .create_file(ROOT_INO, "c", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"decoy c").await?;

        // Walk the base path: root → a → b → c → file.txt
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        assert!(a_stats.is_directory(), "a should be a directory");

        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();
        assert!(
            b_stats.is_directory(),
            "a/b should be a directory, not the delta file 'b'"
        );

        let c_stats = overlay.lookup(b_stats.ino, "c").await?.unwrap();
        assert!(
            c_stats.is_directory(),
            "a/b/c should be a directory, not the delta file 'c'"
        );

        let file_stats = overlay.lookup(c_stats.ino, "file.txt").await?.unwrap();
        assert!(file_stats.is_file());

        // Read the file to verify correct traversal
        let file = overlay.open(file_stats.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"deep content");

        Ok(())
    }

    /// Test that after a copy-up creates directories in delta, lookup still
    /// returns correct types for sibling entries in the base.
    ///
    /// Scenario:
    ///   1. Base has /sdk/rust/ and /sdk/python/ (two sibling dirs)
    ///   2. Modify a file under /sdk/rust/ → triggers copy-up, creates
    ///      "sdk" and "rust" dirs in delta
    ///   3. Lookup /sdk/python/ must still work (base directory)
    #[tokio::test]
    async fn test_overlay_lookup_sibling_base_dirs_after_copy_up() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("sdk/rust"))?;
        std::fs::create_dir_all(base_dir.path().join("sdk/python"))?;
        std::fs::write(base_dir.path().join("sdk/rust/lib.rs"), b"fn main() {}")?;
        std::fs::write(base_dir.path().join("sdk/python/main.py"), b"print('hi')")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Navigate to sdk/rust/lib.rs and modify it (triggers copy-up)
        let sdk_stats = overlay.lookup(ROOT_INO, "sdk").await?.unwrap();
        let rust_stats = overlay.lookup(sdk_stats.ino, "rust").await?.unwrap();
        let lib_stats = overlay.lookup(rust_stats.ino, "lib.rs").await?.unwrap();
        let lib_file = overlay.open(lib_stats.ino, libc::O_RDWR).await?;
        lib_file
            .pwrite(0, b"fn main() { println!(\"hello\"); }")
            .await?;

        // Now lookup the sibling: sdk/python must still be a directory
        let python_stats = overlay.lookup(sdk_stats.ino, "python").await?.unwrap();
        assert!(
            python_stats.is_directory(),
            "sdk/python should still be a directory after copy-up of sdk/rust/lib.rs"
        );

        // And sdk/python/main.py must be accessible
        let main_py = overlay.lookup(python_stats.ino, "main.py").await?.unwrap();
        assert!(main_py.is_file());
        let file = overlay.open(main_py.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"print('hi')");

        Ok(())
    }

    /// Test the exact cargo scenario: path dependency at ../sdk/rust/Cargo.toml
    /// accessed after some delta writes have occurred.
    #[tokio::test]
    async fn test_overlay_cargo_path_dependency_scenario() -> Result<()> {
        // Simulate the agentfs repo structure:
        // /cli/Cargo.toml
        // /sdk/rust/Cargo.toml
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("cli/src"))?;
        std::fs::write(
            base_dir.path().join("cli/Cargo.toml"),
            b"[package]\nname = \"cli\"",
        )?;
        std::fs::write(base_dir.path().join("cli/src/main.rs"), b"fn main() {}")?;
        std::fs::create_dir_all(base_dir.path().join("sdk/rust/src"))?;
        std::fs::write(
            base_dir.path().join("sdk/rust/Cargo.toml"),
            b"[package]\nname = \"sdk\"",
        )?;
        std::fs::write(
            base_dir.path().join("sdk/rust/src/lib.rs"),
            b"pub fn hello() {}",
        )?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Simulate some writes in cli/ (like cargo creating target/)
        let cli_stats = overlay.lookup(ROOT_INO, "cli").await?.unwrap();
        let _target_stats = overlay.mkdir(cli_stats.ino, "target", 0o755, 0, 0).await?;

        // Now simulate cargo resolving ../sdk/rust/Cargo.toml
        // This is the path that fails with ENOTDIR in the bug report
        let sdk_stats = overlay.lookup(ROOT_INO, "sdk").await?.unwrap();
        assert!(sdk_stats.is_directory(), "sdk must be a directory");

        let rust_stats = overlay.lookup(sdk_stats.ino, "rust").await?.unwrap();
        assert!(
            rust_stats.is_directory(),
            "sdk/rust must be a directory (ENOTDIR bug)"
        );

        let toml_stats = overlay.lookup(rust_stats.ino, "Cargo.toml").await?.unwrap();
        assert!(toml_stats.is_file(), "Cargo.toml must be a file");

        // Also verify reading the file works
        let file = overlay.open(toml_stats.ino, libc::O_RDONLY).await?;
        let content = file.pread(0, 100).await?;
        assert_eq!(content, b"[package]\nname = \"sdk\"");

        Ok(())
    }

    /// Test that files created in delta layer under a base directory are visible
    /// in readdir and can be deleted with unlink.
    ///
    /// This test reproduces a bug where:
    /// 1. Base has a directory (e.g., `.git/`)
    /// 2. A file is created in that directory via overlay (e.g., `.git/index.lock`)
    /// 3. `ensure_parent_dirs` creates `.git` in delta with origin mapping
    /// 4. But the overlay inode for `.git` still has `layer: Layer::Base`
    /// 5. readdir only checks delta if layer == Delta, so the new file is invisible
    /// 6. unlink only deletes from delta if parent layer == Delta, so deletion fails
    #[tokio::test]
    async fn test_overlay_readdir_and_unlink_delta_file_in_base_dir() -> Result<()> {
        // Setup: base has a .git directory with some files
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join(".git"))?;
        std::fs::write(base_dir.path().join(".git/config"), b"[core]\n")?;
        std::fs::write(base_dir.path().join(".git/HEAD"), b"ref: refs/heads/main")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup .git directory (creates Base layer mapping)
        let git_stats = overlay.lookup(ROOT_INO, ".git").await?.unwrap();
        assert!(git_stats.is_directory());

        // Step 2: Create a new file in .git (triggers ensure_parent_dirs)
        let (lock_stats, lock_file) = overlay
            .create_file(git_stats.ino, "index.lock", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        lock_file.pwrite(0, b"lock content").await?;
        assert!(lock_stats.is_file());

        // Step 3: Verify readdir shows the new file (BUG: was invisible)
        let entries = overlay.readdir(git_stats.ino).await?.unwrap();
        assert!(
            entries.contains(&"index.lock".to_string()),
            "readdir should show index.lock, got: {:?}",
            entries
        );
        // Also verify base files are still visible
        assert!(entries.contains(&"config".to_string()));
        assert!(entries.contains(&"HEAD".to_string()));

        // Step 4: Verify lookup also works
        let lookup_stats = overlay.lookup(git_stats.ino, "index.lock").await?.unwrap();
        assert!(lookup_stats.is_file());

        // Step 5: Delete the file
        overlay.unlink(git_stats.ino, "index.lock").await?;

        // Step 6: Verify the file is actually gone (BUG: persisted after unlink)
        let deleted = overlay.lookup(git_stats.ino, "index.lock").await?;
        assert!(
            deleted.is_none(),
            "index.lock should be deleted, but lookup still finds it"
        );

        // Also verify readdir no longer shows it
        let entries_after = overlay.readdir(git_stats.ino).await?.unwrap();
        assert!(
            !entries_after.contains(&"index.lock".to_string()),
            "readdir should not show index.lock after deletion"
        );

        // Base files should still be there
        assert!(entries_after.contains(&"config".to_string()));
        assert!(entries_after.contains(&"HEAD".to_string()));

        Ok(())
    }

    /// Test readdir_plus also shows delta files in base directories.
    #[tokio::test]
    async fn test_overlay_readdir_plus_delta_file_in_base_dir() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("mydir"))?;
        std::fs::write(base_dir.path().join("mydir/base.txt"), b"base")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup the directory (Base layer)
        let dir_stats = overlay.lookup(ROOT_INO, "mydir").await?.unwrap();

        // Create a file in the directory
        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // readdir_plus should show both base and delta files
        let entries = overlay.readdir_plus(dir_stats.ino).await?.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert!(
            names.contains(&"base.txt"),
            "readdir_plus should show base.txt"
        );
        assert!(
            names.contains(&"delta.txt"),
            "readdir_plus should show delta.txt"
        );

        Ok(())
    }

    /// After remount, origin mappings can leave overlay inodes tagged as
    /// Layer::Base with stale base inode numbers. Verify that base files
    /// in directories with origin mappings remain accessible.
    #[tokio::test]
    async fn test_overlay_base_file_accessible_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;
        std::fs::write(base_dir.path().join("dir/keep.txt"), b"keep")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: create delta file (creates origin mapping for /dir/)
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let (_s, f) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"delta").await?;

        // Session 2: remount and verify base files are still accessible
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let keep = overlay.lookup(dir_stats.ino, "keep.txt").await?;
        assert!(keep.is_some(), "keep.txt should be visible after remount");

        Ok(())
    }

    /// Test unlink of a BASE file after the parent directory has been promoted
    /// from Base to Delta layer.
    ///
    /// Scenario:
    /// 1. Base has /dir/base.txt and /dir/other.txt
    /// 2. Lookup /dir/ (creates Base layer mapping)
    /// 3. Create /dir/delta.txt (triggers ensure_parent_dirs, promotes /dir/ to Delta)
    /// 4. Unlink /dir/base.txt (base file in promoted parent)
    /// 5. base.txt should be gone (whiteout must be created)
    ///
    /// Bug: The base-walk loop in unlink() returns Ok(()) when a path component
    /// lookup fails in HostFS, skipping whiteout creation.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;
        std::fs::write(base_dir.path().join("dir/other.txt"), b"other content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Step 1: Lookup the directory (creates Base layer mapping)
        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        assert!(dir_stats.is_directory());

        // Step 2: Create a file in the directory (promotes /dir/ from Base to Delta)
        let (_delta_stats, delta_file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        delta_file.pwrite(0, b"delta content").await?;

        // Step 3: Unlink the BASE file
        overlay.unlink(dir_stats.ino, "base.txt").await?;

        // Step 4: Verify the base file is gone via lookup
        let deleted = overlay.lookup(dir_stats.ino, "base.txt").await?;
        assert!(
            deleted.is_none(),
            "base.txt should be deleted after unlink, but lookup still finds it"
        );

        // Step 5: Verify readdir no longer shows it
        let entries = overlay.readdir(dir_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"base.txt".to_string()),
            "readdir should not show base.txt after unlink, got: {:?}",
            entries
        );

        // Other files should still be visible
        assert!(entries.contains(&"other.txt".to_string()));
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Unlink of a base file must create a whiteout even when the parent
    /// directory has a stale origin mapping from a previous session.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_whiteout_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/base.txt"), b"base content")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: create delta file (creates origin mapping for /dir/)
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let (_s, f) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"delta").await?;

        // Session 2: remount and unlink the base file
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "base.txt").await?;
        assert!(
            overlay.lookup(dir_stats.ino, "base.txt").await?.is_none(),
            "base.txt should be whiteout-deleted after unlink"
        );

        Ok(())
    }

    /// Test unlink of a BASE file in a deeply nested promoted parent.
    ///
    /// Scenario: base has /a/b/file.txt, promote /a/b/ by creating a delta
    /// file there, then unlink /a/b/file.txt. The base-walk must resolve
    /// both "a" and "b" in the HostFS to find the base parent.
    #[tokio::test]
    async fn test_overlay_unlink_base_file_in_nested_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("a/b"))?;
        std::fs::write(base_dir.path().join("a/b/base.txt"), b"deep base")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Walk down to /a/b/ (creates Base layer mappings)
        let a_stats = overlay.lookup(ROOT_INO, "a").await?.unwrap();
        let b_stats = overlay.lookup(a_stats.ino, "b").await?.unwrap();

        // Create a delta file in /a/b/ (promotes /a/ and /a/b/ to Delta)
        let (_stats, file) = overlay
            .create_file(b_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // Unlink the base file
        overlay.unlink(b_stats.ino, "base.txt").await?;

        // Verify it's gone
        let deleted = overlay.lookup(b_stats.ino, "base.txt").await?;
        assert!(
            deleted.is_none(),
            "base.txt should be deleted after unlink in nested promoted parent"
        );

        let entries = overlay.readdir(b_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"base.txt".to_string()),
            "readdir should not show base.txt after unlink, got: {:?}",
            entries
        );
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Test rmdir of a BASE directory after the parent has been promoted
    /// from Base to Delta layer.
    ///
    /// Scenario: base has /parent/emptydir/, promote /parent/ by creating a
    /// delta file, then rmdir /parent/emptydir/. The whiteout must be created
    /// so the directory doesn't reappear.
    #[tokio::test]
    async fn test_overlay_rmdir_base_dir_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("parent"))?;
        std::fs::create_dir(base_dir.path().join("parent/emptydir"))?;
        std::fs::write(base_dir.path().join("parent/keep.txt"), b"keep")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup parent directory (Base layer)
        let parent_stats = overlay.lookup(ROOT_INO, "parent").await?.unwrap();

        // Lookup emptydir so overlay knows about it
        let emptydir_stats = overlay.lookup(parent_stats.ino, "emptydir").await?.unwrap();
        assert!(emptydir_stats.is_directory());

        // Create a delta file in /parent/ (promotes /parent/ to Delta)
        let (_stats, file) = overlay
            .create_file(parent_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // rmdir the base directory
        overlay.rmdir(parent_stats.ino, "emptydir").await?;

        // Verify it's gone
        let deleted = overlay.lookup(parent_stats.ino, "emptydir").await?;
        assert!(
            deleted.is_none(),
            "emptydir should be deleted after rmdir, but lookup still finds it"
        );

        let entries = overlay.readdir(parent_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"emptydir".to_string()),
            "readdir should not show emptydir after rmdir, got: {:?}",
            entries
        );
        assert!(entries.contains(&"keep.txt".to_string()));
        assert!(entries.contains(&"delta.txt".to_string()));

        Ok(())
    }

    /// Test rename of a BASE file creates a whiteout at the source when the
    /// parent directory has been promoted from Base to Delta layer.
    ///
    /// Scenario: base has /dir/original.txt, promote /dir/ by creating a delta
    /// file, rename /dir/original.txt to /dir/renamed.txt. The source path
    /// must get a whiteout so original.txt doesn't reappear.
    #[tokio::test]
    async fn test_overlay_rename_base_file_whiteout_in_promoted_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/original.txt"), b"original")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup directory (Base layer)
        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();

        // Lookup original.txt so overlay has the inode
        let orig_stats = overlay
            .lookup(dir_stats.ino, "original.txt")
            .await?
            .unwrap();
        assert!(orig_stats.is_file());

        // Create a delta file to promote /dir/ from Base to Delta
        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta").await?;

        // Rename the base file within the same promoted directory
        overlay
            .rename(dir_stats.ino, "original.txt", dir_stats.ino, "renamed.txt")
            .await?;

        // Verify original.txt is gone (whiteout must exist)
        let deleted = overlay.lookup(dir_stats.ino, "original.txt").await?;
        assert!(
            deleted.is_none(),
            "original.txt should be gone after rename, but lookup still finds it"
        );

        // Verify renamed.txt exists
        let renamed = overlay.lookup(dir_stats.ino, "renamed.txt").await?;
        assert!(renamed.is_some(), "renamed.txt should exist after rename");

        // Verify readdir shows the right state
        let entries = overlay.readdir(dir_stats.ino).await?.unwrap();
        assert!(
            !entries.contains(&"original.txt".to_string()),
            "readdir should not show original.txt after rename, got: {:?}",
            entries
        );
        assert!(
            entries.contains(&"renamed.txt".to_string()),
            "readdir should show renamed.txt after rename, got: {:?}",
            entries
        );

        Ok(())
    }

    /// After remount, unlink must clean up both the delta entry and create
    /// a whiteout for the base entry — even when the parent is tagged Delta
    /// rather than Base.
    #[tokio::test]
    async fn test_overlay_unlink_removes_delta_entry_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/file.txt"), b"original base")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: copy-up file.txt to delta via write
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let file_stats = overlay.lookup(dir_stats.ino, "file.txt").await?.unwrap();
        let file = overlay.open(file_stats.ino, libc::O_WRONLY).await?;
        file.pwrite(0, b"modified in delta").await?;

        // Session 2: remount, unlink, recreate, verify new content
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "file.txt").await?;
        assert!(overlay.lookup(dir_stats.ino, "file.txt").await?.is_none());

        let (_stats, new_file) = overlay
            .create_file(dir_stats.ino, "file.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        new_file.pwrite(0, b"brand new content").await?;

        let read_stats = overlay.lookup(dir_stats.ino, "file.txt").await?.unwrap();
        let read_file = overlay.open(read_stats.ino, libc::O_RDONLY).await?;
        let content = read_file.pread(0, 1024).await?;
        assert_eq!(std::str::from_utf8(&content).unwrap(), "brand new content");

        Ok(())
    }

    /// Hard-link copy-up in session 1, then unlink source in session 2.
    /// The link target must survive even though the parent has a stale
    /// origin mapping.
    #[tokio::test]
    async fn test_overlay_link_copy_up_then_unlink_after_remount() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;
        std::fs::write(base_dir.path().join("dir/src.txt"), b"link source")?;

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");

        // Session 1: hard-link triggers copy_up of src.txt
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        let src_stats = overlay.lookup(dir_stats.ino, "src.txt").await?.unwrap();
        overlay
            .link(src_stats.ino, dir_stats.ino, "dst.txt")
            .await?;

        // Session 2: remount, unlink source, verify link survives
        let base = Arc::new(HostFS::new(base_dir.path())?);
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;
        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();
        overlay.unlink(dir_stats.ino, "src.txt").await?;
        assert!(overlay.lookup(dir_stats.ino, "src.txt").await?.is_none());
        assert!(overlay.lookup(dir_stats.ino, "dst.txt").await?.is_some());

        Ok(())
    }

    /// Test rename of base file across directories where both parents have
    /// been promoted. Source directory must get a whiteout for the original
    /// file, even though the base-walk must resolve through promoted parents.
    #[tokio::test]
    async fn test_overlay_rename_base_file_across_promoted_parents() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("src"))?;
        std::fs::create_dir(base_dir.path().join("dst"))?;
        std::fs::write(base_dir.path().join("src/moveme.txt"), b"moving")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup both directories
        let src_stats = overlay.lookup(ROOT_INO, "src").await?.unwrap();
        let dst_stats = overlay.lookup(ROOT_INO, "dst").await?.unwrap();

        // Promote /src/ by creating a delta file
        let (_s, f) = overlay
            .create_file(src_stats.ino, "trigger1.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"t").await?;

        // Promote /dst/ by creating a delta file
        let (_s, f) = overlay
            .create_file(dst_stats.ino, "trigger2.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        f.pwrite(0, b"t").await?;

        // Lookup moveme.txt so overlay knows it
        let moveme = overlay.lookup(src_stats.ino, "moveme.txt").await?.unwrap();
        assert!(moveme.is_file());

        // Rename across promoted parents
        overlay
            .rename(src_stats.ino, "moveme.txt", dst_stats.ino, "moved.txt")
            .await?;

        // Source must be gone (whiteout at /src/moveme.txt)
        let src_lookup = overlay.lookup(src_stats.ino, "moveme.txt").await?;
        assert!(
            src_lookup.is_none(),
            "moveme.txt should be gone from /src/ after rename"
        );

        // Destination must exist
        let dst_lookup = overlay.lookup(dst_stats.ino, "moved.txt").await?;
        assert!(
            dst_lookup.is_some(),
            "moved.txt should exist in /dst/ after rename"
        );

        // readdir /src/ should not show moveme.txt
        let src_entries = overlay.readdir(src_stats.ino).await?.unwrap();
        assert!(
            !src_entries.contains(&"moveme.txt".to_string()),
            "readdir /src/ should not show moveme.txt, got: {:?}",
            src_entries
        );

        Ok(())
    }

    /// Test rename of a BASE file in a deeply nested directory that has not
    /// been promoted to Delta.
    ///
    /// Scenario: base has /deep/nested/file.txt, lookup the path (Base layer
    /// only, no promotion), then rename file.txt within the same directory.
    /// The delta parent must be resolved after copy_up creates it.
    #[tokio::test]
    async fn test_overlay_rename_base_file_delta_src_parent_before_copyup() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir_all(base_dir.path().join("deep/nested"))?;
        std::fs::write(base_dir.path().join("deep/nested/file.txt"), b"content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Walk down to /deep/nested/ (creates Base layer mappings)
        let deep_stats = overlay.lookup(ROOT_INO, "deep").await?.unwrap();
        let nested_stats = overlay.lookup(deep_stats.ino, "nested").await?.unwrap();
        let file_stats = overlay.lookup(nested_stats.ino, "file.txt").await?.unwrap();
        assert!(file_stats.is_file());

        // Rename within same directory — parents only exist in base
        overlay
            .rename(
                nested_stats.ino,
                "file.txt",
                nested_stats.ino,
                "renamed.txt",
            )
            .await?;

        let renamed = overlay.lookup(nested_stats.ino, "renamed.txt").await?;
        assert!(renamed.is_some(), "renamed.txt should exist after rename");

        let original = overlay.lookup(nested_stats.ino, "file.txt").await?;
        assert!(original.is_none(), "file.txt should be gone after rename");

        let entries = overlay.readdir(nested_stats.ino).await?.unwrap();
        assert!(
            entries.contains(&"renamed.txt".to_string()),
            "readdir should show renamed.txt, got: {:?}",
            entries
        );
        assert!(
            !entries.contains(&"file.txt".to_string()),
            "readdir should not show file.txt after rename, got: {:?}",
            entries
        );

        Ok(())
    }

    /// Test rename of a BASE file across directories when neither parent has
    /// been promoted to Delta.
    ///
    /// Scenario: base has /src/base.txt and /dst/, neither exists in delta.
    /// Rename /src/base.txt to /dst/moved.txt. Both source and destination
    /// delta parents must be correctly resolved after copy_up.
    #[tokio::test]
    async fn test_overlay_rename_base_file_across_dirs_no_found_guard() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("src"))?;
        std::fs::create_dir(base_dir.path().join("dst"))?;
        std::fs::write(base_dir.path().join("src/base.txt"), b"source content")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let src_stats = overlay.lookup(ROOT_INO, "src").await?.unwrap();
        let dst_stats = overlay.lookup(ROOT_INO, "dst").await?.unwrap();
        let file_stats = overlay.lookup(src_stats.ino, "base.txt").await?.unwrap();
        assert!(file_stats.is_file());
        overlay
            .rename(src_stats.ino, "base.txt", dst_stats.ino, "moved.txt")
            .await?;

        let moved = overlay.lookup(dst_stats.ino, "moved.txt").await?;
        assert!(
            moved.is_some(),
            "moved.txt should exist in /dst/ after rename"
        );

        let original = overlay.lookup(src_stats.ino, "base.txt").await?;
        assert!(
            original.is_none(),
            "base.txt should be gone from /src/ after rename"
        );

        let file = overlay.open(moved.unwrap().ino, libc::O_RDONLY).await?;
        let data = file.pread(0, 1024).await?;
        assert_eq!(data, b"source content");

        Ok(())
    }

    /// Test unlink of a delta-only file does not create a spurious whiteout.
    ///
    /// Scenario: base has /dir/ (empty), create delta_only.txt in delta,
    /// unlink it, then recreate with the same name. The recreated file must
    /// be visible — no whiteout should have been left behind.
    #[tokio::test]
    async fn test_overlay_unlink_delta_only_file_no_spurious_whiteout() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("dir"))?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        let dir_stats = overlay.lookup(ROOT_INO, "dir").await?.unwrap();

        let (_stats, file) = overlay
            .create_file(dir_stats.ino, "delta_only.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, b"delta only").await?;

        overlay.unlink(dir_stats.ino, "delta_only.txt").await?;

        let deleted = overlay.lookup(dir_stats.ino, "delta_only.txt").await?;
        assert!(
            deleted.is_none(),
            "delta_only.txt should be gone after unlink"
        );

        // Recreate with the same name
        let (_stats2, file2) = overlay
            .create_file(dir_stats.ino, "delta_only.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file2.pwrite(0, b"recreated").await?;

        let recreated = overlay.lookup(dir_stats.ino, "delta_only.txt").await?;
        assert!(
            recreated.is_some(),
            "recreated delta_only.txt should be visible (no spurious whiteout)"
        );

        let f = overlay.open(recreated.unwrap().ino, libc::O_RDONLY).await?;
        let data = f.pread(0, 1024).await?;
        assert_eq!(data, b"recreated");

        Ok(())
    }

    /// Test rmdir works for directories created in delta under base parent.
    #[tokio::test]
    async fn test_overlay_rmdir_delta_dir_in_base_parent() -> Result<()> {
        let base_dir = tempdir()?;
        std::fs::create_dir(base_dir.path().join("parent"))?;
        std::fs::write(base_dir.path().join("parent/existing.txt"), b"existing")?;

        let base = Arc::new(HostFS::new(base_dir.path())?);

        let delta_dir = tempdir()?;
        let db_path = delta_dir.path().join("delta.db");
        let delta = AgentFS::new(db_path.to_str().unwrap()).await?;

        let overlay = OverlayFS::new(base, delta);
        overlay.init(base_dir.path().to_str().unwrap()).await?;

        // Lookup base directory
        let parent_stats = overlay.lookup(ROOT_INO, "parent").await?.unwrap();

        // Create a subdirectory in delta
        let subdir_stats = overlay
            .mkdir(parent_stats.ino, "newsubdir", 0o755, 0, 0)
            .await?;
        assert!(subdir_stats.is_directory());

        // Verify it exists
        let lookup = overlay.lookup(parent_stats.ino, "newsubdir").await?;
        assert!(lookup.is_some());

        // Delete it with rmdir
        overlay.rmdir(parent_stats.ino, "newsubdir").await?;

        // Verify it's gone
        let deleted = overlay.lookup(parent_stats.ino, "newsubdir").await?;
        assert!(deleted.is_none(), "newsubdir should be deleted after rmdir");

        Ok(())
    }

    async fn scalar_i64(overlay: &OverlayFS, sql: &str) -> Result<i64> {
        let conn = overlay.delta().get_connection().await?;
        let mut rows = conn.query(sql, ()).await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| Error::Internal(format!("no row for scalar query: {sql}")))?;
        Ok(row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .unwrap_or(0))
    }

    fn patterned_bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| {
                seed.wrapping_add((index % 251) as u8)
                    .wrapping_add((index / 251) as u8)
            })
            .collect()
    }
}
