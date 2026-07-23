//! Overlay filesystem: base layer + delta DB with whiteouts and origin maps.
//!
//! Lock order: the overlay's sync locks (`maps`, `whiteouts`, `origin_map`,
//! `whiteout_fault`) are leaf locks. Each is taken alone for a short critical
//! section, they never nest with one another, and DB awaits complete before
//! any guard is taken (`clippy::await_holding_lock` is deny-by-workspace).

mod copyup;
mod fs;
mod maps;
mod partial;
mod whiteouts;

use crate::error::{Error, Result};
use crate::schema;
use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use turso::{Connection, Value};

use super::{
    agentfs::{AgentFS, ReapHook},
    BoxedFile, File, FileSystem, Stats, WriteRange,
};

use maps::{InodeInfo, Layer, OverlayMaps};
use partial::{OverlayPartialFile, PartialOrigin};

/// Root inode number (matches FUSE convention)
pub(super) const ROOT_INO: i64 = 1;
const STORAGE_CHUNKED: i64 = 0;
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
    threshold_bytes: u64,
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

fn is_write_open(flags: i32) -> bool {
    (flags & libc::O_ACCMODE) != libc::O_RDONLY || (flags & libc::O_TRUNC) != 0
}

fn mount_visible_file(inner: BoxedFile, overlay_ino: i64) -> BoxedFile {
    Arc::new(MountVisibleFile { inner, overlay_ino })
}

struct MountVisibleFile {
    inner: BoxedFile,
    overlay_ino: i64,
}

#[async_trait]
impl File for MountVisibleFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        self.inner.pread(offset, size).await
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        self.inner.pwrite(offset, data).await
    }

    async fn pwrite_ranges(&self, ranges: Vec<WriteRange>) -> Result<()> {
        self.inner.pwrite_ranges(ranges).await
    }

    async fn pwrite_ranges_batched(&self, ranges: Vec<WriteRange>) -> Result<()> {
        self.inner.pwrite_ranges_batched(ranges).await
    }

    async fn drain_writes(&self) -> Result<()> {
        self.inner.drain_writes().await
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        self.inner.truncate(size).await
    }

    async fn fsync(&self) -> Result<()> {
        self.inner.fsync().await
    }

    async fn fstat(&self) -> Result<Stats> {
        let mut stats = self.inner.fstat().await?;
        stats.ino = self.overlay_ino;
        Ok(stats)
    }
}

struct OverlaySidecarReapHook;

#[async_trait]
impl ReapHook for OverlaySidecarReapHook {
    fn dedup_key(&self) -> Option<&'static str> {
        Some("overlay-sidecar")
    }

    async fn on_reap(&self, conn: &Connection, ino: i64) -> Result<()> {
        conn.execute("DELETE FROM fs_origin WHERE delta_ino = ?", (ino,))
            .await?;
        conn.execute("DELETE FROM fs_chunk_override WHERE delta_ino = ?", (ino,))
            .await?;
        conn.execute("DELETE FROM fs_partial_origin WHERE delta_ino = ?", (ino,))
            .await?;
        Ok(())
    }
}

/// A copy-on-write overlay filesystem using inode-based operations.
///
/// Combines a read-only base layer with a writable delta layer (`AgentFS`).
/// All modifications are written to the delta layer, while reads fall back
/// to the base layer if not found in delta.
///
/// # Directory opacity
///
/// This overlay deliberately has no opaque-directory concept. If a base
/// directory is removed through the overlay and later recreated at the same
/// path, base children can resurface on `readdir`, and the base-directory
/// rename guard will still return `EXDEV` while that base path is visible.
/// That diverges from kernel overlayfs, but it is the accepted AgentFS
/// behavior. The `resolves_to_visible_base_directory` signal must stay shared
/// by both readdir base-child merging and the rename guard if opacity is ever
/// revisited.
pub struct OverlayFS {
    /// The underlying read-only base filesystem
    base: Arc<dyn FileSystem>,
    /// The delta layer where modifications go
    delta: AgentFS,
    /// Overlay inode maps, reverse maps, path maps, allocator, and lookup refs.
    maps: Mutex<OverlayMaps>,
    /// Set of whiteout paths (deleted from base)
    whiteouts: RwLock<HashSet<String>>,
    /// Origin mapping: delta_ino -> base_ino (for copy-up consistency)
    origin_map: RwLock<HashMap<i64, i64>>,
    /// Explicit policy for chunk-granularity base fallback.
    partial_origin_policy: PartialOriginPolicy,
    /// Test-only fault injection for whiteout transaction rollback coverage.
    #[cfg(test)]
    whiteout_fault: Mutex<Option<String>>,
}

impl OverlayFS {
    /// Create a new overlay filesystem
    pub fn new(base: Arc<dyn FileSystem>, delta: AgentFS) -> Self {
        let partial_origin_policy = delta.partial_origin_policy();
        Self::new_with_partial_origin_policy(base, delta, partial_origin_policy)
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
        delta.register_reap_hook(Self::sidecar_reap_hook());

        Self {
            base,
            delta,
            maps: Mutex::new(OverlayMaps::new()),
            whiteouts: RwLock::new(HashSet::new()),
            origin_map: RwLock::new(HashMap::new()),
            partial_origin_policy,
            #[cfg(test)]
            whiteout_fault: Mutex::new(None),
        }
    }

    pub(crate) fn sidecar_reap_hook() -> Arc<dyn ReapHook> {
        Arc::new(OverlaySidecarReapHook)
    }

    /// Initialize the overlay filesystem schema
    pub async fn init_schema(conn: &Connection, base_path: &str) -> Result<()> {
        schema::set_overlay_base_path(conn, base_path).await
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
        let mut whiteouts = self.whiteouts.write();
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

    /// Load origin mappings from database.
    async fn load_origins(&self, conn: &Connection) -> Result<()> {
        let mut rows = conn
            .query("SELECT delta_ino, base_ino FROM fs_origin", ())
            .await?;
        let mut mappings = Vec::new();
        while let Some(row) = rows.next().await? {
            let delta_ino = row.get_value(0).ok().and_then(|v| v.as_integer().copied());
            let base_ino = row.get_value(1).ok().and_then(|v| v.as_integer().copied());
            if let (Some(d), Some(b)) = (delta_ino, base_ino) {
                mappings.push((d, b));
            }
        }
        let mut origins = self.origin_map.write();
        for (d, b) in mappings {
            origins.insert(d, b);
        }
        Ok(())
    }

    /// Get a reference to the base layer
    pub fn base(&self) -> &Arc<dyn FileSystem> {
        &self.base
    }

    /// Get a reference to the delta layer
    #[cfg(test)]
    fn delta(&self) -> &AgentFS {
        &self.delta
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
// Keep the extracted test body byte-for-byte; this feature is a pure move.
#[rustfmt::skip]
#[path = "tests.rs"]
mod overlay_tests;
