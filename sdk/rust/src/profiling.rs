//! Lightweight env-gated profiling counters for AgentFS hot paths.
//!
//! The public recording helpers are intentionally tiny when profiling is
//! disabled: each call performs one cached environment-gate check and returns.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static COUNTERS: ProfileCounters = ProfileCounters::new();

/// Snapshot of AgentFS profiling counters.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileSnapshot {
    pub connection_wait_count: u64,
    pub connection_wait_nanos: u64,
    pub connection_create_count: u64,
    pub connection_reuse_count: u64,
    pub lookup_count: u64,
    pub lookup_delta_count: u64,
    pub lookup_base_count: u64,
    pub lookup_whiteout_count: u64,
    pub getattr_count: u64,
    pub readdir_count: u64,
    pub readdir_plus_count: u64,
    pub path_resolution_count: u64,
    pub path_component_count: u64,
    pub path_cache_hits: u64,
    pub path_cache_misses: u64,
    pub negative_lookup_count: u64,
    pub attr_cache_hits: u64,
    pub attr_cache_misses: u64,
    pub dentry_cache_hits: u64,
    pub dentry_cache_misses: u64,
    pub chunk_read_queries: u64,
    pub chunk_read_chunks: u64,
    pub chunk_write_chunks: u64,
    pub wal_checkpoint_count: u64,
    pub wal_checkpoint_nanos: u64,
    pub fuse_callback_count: u64,
    pub fuse_lookup_count: u64,
    pub fuse_getattr_count: u64,
    pub fuse_readdir_count: u64,
    pub fuse_readdir_plus_count: u64,
    pub fuse_open_count: u64,
    pub fuse_read_count: u64,
    pub fuse_release_count: u64,
    pub fuse_write_count: u64,
    pub fuse_write_bytes: u64,
    pub fuse_flush_count: u64,
    pub fuse_flush_ranges: u64,
    pub fuse_flush_bytes: u64,
}

/// Atomic profiling counters.
#[derive(Debug)]
pub struct ProfileCounters {
    connection_wait_count: AtomicU64,
    connection_wait_nanos: AtomicU64,
    connection_create_count: AtomicU64,
    connection_reuse_count: AtomicU64,
    lookup_count: AtomicU64,
    lookup_delta_count: AtomicU64,
    lookup_base_count: AtomicU64,
    lookup_whiteout_count: AtomicU64,
    getattr_count: AtomicU64,
    readdir_count: AtomicU64,
    readdir_plus_count: AtomicU64,
    path_resolution_count: AtomicU64,
    path_component_count: AtomicU64,
    path_cache_hits: AtomicU64,
    path_cache_misses: AtomicU64,
    negative_lookup_count: AtomicU64,
    attr_cache_hits: AtomicU64,
    attr_cache_misses: AtomicU64,
    dentry_cache_hits: AtomicU64,
    dentry_cache_misses: AtomicU64,
    chunk_read_queries: AtomicU64,
    chunk_read_chunks: AtomicU64,
    chunk_write_chunks: AtomicU64,
    wal_checkpoint_count: AtomicU64,
    wal_checkpoint_nanos: AtomicU64,
    fuse_callback_count: AtomicU64,
    fuse_lookup_count: AtomicU64,
    fuse_getattr_count: AtomicU64,
    fuse_readdir_count: AtomicU64,
    fuse_readdir_plus_count: AtomicU64,
    fuse_open_count: AtomicU64,
    fuse_read_count: AtomicU64,
    fuse_release_count: AtomicU64,
    fuse_write_count: AtomicU64,
    fuse_write_bytes: AtomicU64,
    fuse_flush_count: AtomicU64,
    fuse_flush_ranges: AtomicU64,
    fuse_flush_bytes: AtomicU64,
}

impl ProfileCounters {
    pub const fn new() -> Self {
        Self {
            connection_wait_count: AtomicU64::new(0),
            connection_wait_nanos: AtomicU64::new(0),
            connection_create_count: AtomicU64::new(0),
            connection_reuse_count: AtomicU64::new(0),
            lookup_count: AtomicU64::new(0),
            lookup_delta_count: AtomicU64::new(0),
            lookup_base_count: AtomicU64::new(0),
            lookup_whiteout_count: AtomicU64::new(0),
            getattr_count: AtomicU64::new(0),
            readdir_count: AtomicU64::new(0),
            readdir_plus_count: AtomicU64::new(0),
            path_resolution_count: AtomicU64::new(0),
            path_component_count: AtomicU64::new(0),
            path_cache_hits: AtomicU64::new(0),
            path_cache_misses: AtomicU64::new(0),
            negative_lookup_count: AtomicU64::new(0),
            attr_cache_hits: AtomicU64::new(0),
            attr_cache_misses: AtomicU64::new(0),
            dentry_cache_hits: AtomicU64::new(0),
            dentry_cache_misses: AtomicU64::new(0),
            chunk_read_queries: AtomicU64::new(0),
            chunk_read_chunks: AtomicU64::new(0),
            chunk_write_chunks: AtomicU64::new(0),
            wal_checkpoint_count: AtomicU64::new(0),
            wal_checkpoint_nanos: AtomicU64::new(0),
            fuse_callback_count: AtomicU64::new(0),
            fuse_lookup_count: AtomicU64::new(0),
            fuse_getattr_count: AtomicU64::new(0),
            fuse_readdir_count: AtomicU64::new(0),
            fuse_readdir_plus_count: AtomicU64::new(0),
            fuse_open_count: AtomicU64::new(0),
            fuse_read_count: AtomicU64::new(0),
            fuse_release_count: AtomicU64::new(0),
            fuse_write_count: AtomicU64::new(0),
            fuse_write_bytes: AtomicU64::new(0),
            fuse_flush_count: AtomicU64::new(0),
            fuse_flush_ranges: AtomicU64::new(0),
            fuse_flush_bytes: AtomicU64::new(0),
        }
    }

    fn add_connection_wait(&self, duration: Duration) {
        self.connection_wait_count.fetch_add(1, Ordering::Relaxed);
        self.connection_wait_nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    fn add_connection_create(&self) {
        self.connection_create_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_connection_reuse(&self) {
        self.connection_reuse_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_lookup(&self) {
        self.lookup_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_lookup_delta(&self) {
        self.lookup_delta_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_lookup_base(&self) {
        self.lookup_base_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_lookup_whiteout(&self) {
        self.lookup_whiteout_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_getattr(&self) {
        self.getattr_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_readdir(&self) {
        self.readdir_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_readdir_plus(&self) {
        self.readdir_plus_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_path_resolution(&self, components: u64) {
        self.path_resolution_count.fetch_add(1, Ordering::Relaxed);
        self.path_component_count
            .fetch_add(components, Ordering::Relaxed);
    }

    fn add_path_cache_hit(&self) {
        self.path_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn add_path_cache_miss(&self) {
        self.path_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    fn add_negative_lookup(&self) {
        self.negative_lookup_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_attr_cache_hit(&self) {
        self.attr_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn add_attr_cache_miss(&self) {
        self.attr_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    fn add_dentry_cache_hit(&self) {
        self.dentry_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn add_dentry_cache_miss(&self) {
        self.dentry_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    fn add_chunk_read_query(&self) {
        self.chunk_read_queries.fetch_add(1, Ordering::Relaxed);
    }

    fn add_chunk_read_chunks(&self, chunks: u64) {
        self.chunk_read_chunks.fetch_add(chunks, Ordering::Relaxed);
    }

    fn add_chunk_write_chunks(&self, chunks: u64) {
        self.chunk_write_chunks.fetch_add(chunks, Ordering::Relaxed);
    }

    fn add_wal_checkpoint(&self, duration: Duration) {
        self.wal_checkpoint_count.fetch_add(1, Ordering::Relaxed);
        self.wal_checkpoint_nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    fn add_fuse_callback(&self) {
        self.fuse_callback_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_lookup(&self) {
        self.add_fuse_callback();
        self.fuse_lookup_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_getattr(&self) {
        self.add_fuse_callback();
        self.fuse_getattr_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_readdir(&self) {
        self.add_fuse_callback();
        self.fuse_readdir_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_readdir_plus(&self) {
        self.add_fuse_callback();
        self.fuse_readdir_plus_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_open(&self) {
        self.add_fuse_callback();
        self.fuse_open_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_read(&self) {
        self.add_fuse_callback();
        self.fuse_read_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_release(&self) {
        self.add_fuse_callback();
        self.fuse_release_count.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_write(&self, bytes: u64) {
        self.add_fuse_callback();
        self.fuse_write_count.fetch_add(1, Ordering::Relaxed);
        self.fuse_write_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn add_fuse_flush(&self, ranges: u64, bytes: u64) {
        self.add_fuse_callback();
        self.fuse_flush_count.fetch_add(1, Ordering::Relaxed);
        self.fuse_flush_ranges.fetch_add(ranges, Ordering::Relaxed);
        self.fuse_flush_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProfileSnapshot {
        ProfileSnapshot {
            connection_wait_count: self.connection_wait_count.load(Ordering::Relaxed),
            connection_wait_nanos: self.connection_wait_nanos.load(Ordering::Relaxed),
            connection_create_count: self.connection_create_count.load(Ordering::Relaxed),
            connection_reuse_count: self.connection_reuse_count.load(Ordering::Relaxed),
            lookup_count: self.lookup_count.load(Ordering::Relaxed),
            lookup_delta_count: self.lookup_delta_count.load(Ordering::Relaxed),
            lookup_base_count: self.lookup_base_count.load(Ordering::Relaxed),
            lookup_whiteout_count: self.lookup_whiteout_count.load(Ordering::Relaxed),
            getattr_count: self.getattr_count.load(Ordering::Relaxed),
            readdir_count: self.readdir_count.load(Ordering::Relaxed),
            readdir_plus_count: self.readdir_plus_count.load(Ordering::Relaxed),
            path_resolution_count: self.path_resolution_count.load(Ordering::Relaxed),
            path_component_count: self.path_component_count.load(Ordering::Relaxed),
            path_cache_hits: self.path_cache_hits.load(Ordering::Relaxed),
            path_cache_misses: self.path_cache_misses.load(Ordering::Relaxed),
            negative_lookup_count: self.negative_lookup_count.load(Ordering::Relaxed),
            attr_cache_hits: self.attr_cache_hits.load(Ordering::Relaxed),
            attr_cache_misses: self.attr_cache_misses.load(Ordering::Relaxed),
            dentry_cache_hits: self.dentry_cache_hits.load(Ordering::Relaxed),
            dentry_cache_misses: self.dentry_cache_misses.load(Ordering::Relaxed),
            chunk_read_queries: self.chunk_read_queries.load(Ordering::Relaxed),
            chunk_read_chunks: self.chunk_read_chunks.load(Ordering::Relaxed),
            chunk_write_chunks: self.chunk_write_chunks.load(Ordering::Relaxed),
            wal_checkpoint_count: self.wal_checkpoint_count.load(Ordering::Relaxed),
            wal_checkpoint_nanos: self.wal_checkpoint_nanos.load(Ordering::Relaxed),
            fuse_callback_count: self.fuse_callback_count.load(Ordering::Relaxed),
            fuse_lookup_count: self.fuse_lookup_count.load(Ordering::Relaxed),
            fuse_getattr_count: self.fuse_getattr_count.load(Ordering::Relaxed),
            fuse_readdir_count: self.fuse_readdir_count.load(Ordering::Relaxed),
            fuse_readdir_plus_count: self.fuse_readdir_plus_count.load(Ordering::Relaxed),
            fuse_open_count: self.fuse_open_count.load(Ordering::Relaxed),
            fuse_read_count: self.fuse_read_count.load(Ordering::Relaxed),
            fuse_release_count: self.fuse_release_count.load(Ordering::Relaxed),
            fuse_write_count: self.fuse_write_count.load(Ordering::Relaxed),
            fuse_write_bytes: self.fuse_write_bytes.load(Ordering::Relaxed),
            fuse_flush_count: self.fuse_flush_count.load(Ordering::Relaxed),
            fuse_flush_ranges: self.fuse_flush_ranges.load(Ordering::Relaxed),
            fuse_flush_bytes: self.fuse_flush_bytes.load(Ordering::Relaxed),
        }
    }
}

impl Default for ProfileCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns true when profiling is enabled with `AGENTFS_PROFILE=1`.
pub fn is_enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("AGENTFS_PROFILE")
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(false)
    })
}

pub fn record_connection_wait(duration: Duration) {
    if is_enabled() {
        COUNTERS.add_connection_wait(duration);
    }
}

pub fn record_connection_create() {
    if is_enabled() {
        COUNTERS.add_connection_create();
    }
}

pub fn record_connection_reuse() {
    if is_enabled() {
        COUNTERS.add_connection_reuse();
    }
}

pub fn record_lookup() {
    if is_enabled() {
        COUNTERS.add_lookup();
    }
}

pub fn record_lookup_delta() {
    if is_enabled() {
        COUNTERS.add_lookup_delta();
    }
}

pub fn record_lookup_base() {
    if is_enabled() {
        COUNTERS.add_lookup_base();
    }
}

pub fn record_lookup_whiteout() {
    if is_enabled() {
        COUNTERS.add_lookup_whiteout();
    }
}

pub fn record_getattr() {
    if is_enabled() {
        COUNTERS.add_getattr();
    }
}

pub fn record_readdir() {
    if is_enabled() {
        COUNTERS.add_readdir();
    }
}

pub fn record_readdir_plus() {
    if is_enabled() {
        COUNTERS.add_readdir_plus();
    }
}

pub fn record_path_resolution(components: u64) {
    if is_enabled() {
        COUNTERS.add_path_resolution(components);
    }
}

pub fn record_path_cache_hit() {
    if is_enabled() {
        COUNTERS.add_path_cache_hit();
    }
}

pub fn record_path_cache_miss() {
    if is_enabled() {
        COUNTERS.add_path_cache_miss();
    }
}

pub fn record_negative_lookup() {
    if is_enabled() {
        COUNTERS.add_negative_lookup();
    }
}

pub fn record_attr_cache_hit() {
    if is_enabled() {
        COUNTERS.add_attr_cache_hit();
    }
}

pub fn record_attr_cache_miss() {
    if is_enabled() {
        COUNTERS.add_attr_cache_miss();
    }
}

pub fn record_dentry_cache_hit() {
    if is_enabled() {
        COUNTERS.add_dentry_cache_hit();
    }
}

pub fn record_dentry_cache_miss() {
    if is_enabled() {
        COUNTERS.add_dentry_cache_miss();
    }
}

pub fn record_chunk_read_query() {
    if is_enabled() {
        COUNTERS.add_chunk_read_query();
    }
}

pub fn record_chunk_read_chunks(chunks: u64) {
    if is_enabled() {
        COUNTERS.add_chunk_read_chunks(chunks);
    }
}

pub fn record_chunk_write_chunks(chunks: u64) {
    if is_enabled() {
        COUNTERS.add_chunk_write_chunks(chunks);
    }
}

pub fn record_wal_checkpoint(duration: Duration) {
    if is_enabled() {
        COUNTERS.add_wal_checkpoint(duration);
    }
}

pub fn record_fuse_lookup() {
    if is_enabled() {
        COUNTERS.add_fuse_lookup();
    }
}

pub fn record_fuse_getattr() {
    if is_enabled() {
        COUNTERS.add_fuse_getattr();
    }
}

pub fn record_fuse_readdir() {
    if is_enabled() {
        COUNTERS.add_fuse_readdir();
    }
}

pub fn record_fuse_readdir_plus() {
    if is_enabled() {
        COUNTERS.add_fuse_readdir_plus();
    }
}

pub fn record_fuse_open() {
    if is_enabled() {
        COUNTERS.add_fuse_open();
    }
}

pub fn record_fuse_read() {
    if is_enabled() {
        COUNTERS.add_fuse_read();
    }
}

pub fn record_fuse_release() {
    if is_enabled() {
        COUNTERS.add_fuse_release();
    }
}

pub fn record_fuse_write(bytes: u64) {
    if is_enabled() {
        COUNTERS.add_fuse_write(bytes);
    }
}

pub fn record_fuse_flush(ranges: u64, bytes: u64) {
    if is_enabled() {
        COUNTERS.add_fuse_flush(ranges, bytes);
    }
}

pub fn snapshot() -> ProfileSnapshot {
    COUNTERS.snapshot()
}

fn summary_json(source: &str, snapshot: &ProfileSnapshot) -> String {
    serde_json::json!({
        "event": "agentfs_profile_summary",
        "source": source,
        "counters": snapshot,
    })
    .to_string()
}

/// Emit a structured profile summary to stderr if profiling is enabled.
pub fn report_summary(source: &str) {
    if !is_enabled() {
        return;
    }

    eprintln!("{}", summary_json(source, &snapshot()));
}

/// Drop guard that emits the current profiling summary.
#[derive(Debug)]
pub struct ProfileReportGuard {
    source: &'static str,
}

impl ProfileReportGuard {
    pub fn new(source: &'static str) -> Self {
        Self { source }
    }
}

impl Drop for ProfileReportGuard {
    fn drop(&mut self) {
        report_summary(self.source);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn counters_accumulate_expected_values() {
        let counters = ProfileCounters::new();

        counters.add_connection_wait(Duration::from_nanos(7));
        counters.add_connection_create();
        counters.add_connection_reuse();
        counters.add_lookup();
        counters.add_lookup_delta();
        counters.add_lookup_base();
        counters.add_lookup_whiteout();
        counters.add_getattr();
        counters.add_readdir();
        counters.add_readdir_plus();
        counters.add_path_resolution(4);
        counters.add_path_cache_hit();
        counters.add_path_cache_miss();
        counters.add_negative_lookup();
        counters.add_attr_cache_hit();
        counters.add_attr_cache_miss();
        counters.add_dentry_cache_hit();
        counters.add_dentry_cache_miss();
        counters.add_chunk_read_query();
        counters.add_chunk_read_chunks(3);
        counters.add_chunk_write_chunks(5);
        counters.add_wal_checkpoint(Duration::from_nanos(11));
        counters.add_fuse_lookup();
        counters.add_fuse_getattr();
        counters.add_fuse_readdir();
        counters.add_fuse_readdir_plus();
        counters.add_fuse_open();
        counters.add_fuse_read();
        counters.add_fuse_release();
        counters.add_fuse_write(13);
        counters.add_fuse_flush(2, 21);

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.connection_wait_count, 1);
        assert_eq!(snapshot.connection_wait_nanos, 7);
        assert_eq!(snapshot.connection_create_count, 1);
        assert_eq!(snapshot.connection_reuse_count, 1);
        assert_eq!(snapshot.lookup_count, 1);
        assert_eq!(snapshot.lookup_delta_count, 1);
        assert_eq!(snapshot.lookup_base_count, 1);
        assert_eq!(snapshot.lookup_whiteout_count, 1);
        assert_eq!(snapshot.getattr_count, 1);
        assert_eq!(snapshot.readdir_count, 1);
        assert_eq!(snapshot.readdir_plus_count, 1);
        assert_eq!(snapshot.path_resolution_count, 1);
        assert_eq!(snapshot.path_component_count, 4);
        assert_eq!(snapshot.path_cache_hits, 1);
        assert_eq!(snapshot.path_cache_misses, 1);
        assert_eq!(snapshot.negative_lookup_count, 1);
        assert_eq!(snapshot.attr_cache_hits, 1);
        assert_eq!(snapshot.attr_cache_misses, 1);
        assert_eq!(snapshot.dentry_cache_hits, 1);
        assert_eq!(snapshot.dentry_cache_misses, 1);
        assert_eq!(snapshot.chunk_read_queries, 1);
        assert_eq!(snapshot.chunk_read_chunks, 3);
        assert_eq!(snapshot.chunk_write_chunks, 5);
        assert_eq!(snapshot.wal_checkpoint_count, 1);
        assert_eq!(snapshot.wal_checkpoint_nanos, 11);
        assert_eq!(snapshot.fuse_callback_count, 9);
        assert_eq!(snapshot.fuse_lookup_count, 1);
        assert_eq!(snapshot.fuse_getattr_count, 1);
        assert_eq!(snapshot.fuse_readdir_count, 1);
        assert_eq!(snapshot.fuse_readdir_plus_count, 1);
        assert_eq!(snapshot.fuse_open_count, 1);
        assert_eq!(snapshot.fuse_read_count, 1);
        assert_eq!(snapshot.fuse_release_count, 1);
        assert_eq!(snapshot.fuse_write_count, 1);
        assert_eq!(snapshot.fuse_write_bytes, 13);
        assert_eq!(snapshot.fuse_flush_count, 1);
        assert_eq!(snapshot.fuse_flush_ranges, 2);
        assert_eq!(snapshot.fuse_flush_bytes, 21);
    }

    #[test]
    fn summary_json_is_structured() {
        let counters = ProfileCounters::new();
        counters.add_chunk_read_query();

        let value: Value = serde_json::from_str(&summary_json("unit-test", &counters.snapshot()))
            .expect("summary JSON should parse");

        assert_eq!(value["event"], "agentfs_profile_summary");
        assert_eq!(value["source"], "unit-test");
        assert_eq!(value["counters"]["chunk_read_queries"], 1);
    }
}
