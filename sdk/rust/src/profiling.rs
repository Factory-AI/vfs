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
    pub dentry_cache_hits: u64,
    pub dentry_cache_misses: u64,
    pub chunk_read_queries: u64,
    pub chunk_read_chunks: u64,
    pub chunk_write_chunks: u64,
    pub wal_checkpoint_count: u64,
    pub wal_checkpoint_nanos: u64,
    pub fuse_write_count: u64,
    pub fuse_write_bytes: u64,
}

/// Atomic profiling counters.
#[derive(Debug)]
pub struct ProfileCounters {
    connection_wait_count: AtomicU64,
    connection_wait_nanos: AtomicU64,
    connection_create_count: AtomicU64,
    connection_reuse_count: AtomicU64,
    dentry_cache_hits: AtomicU64,
    dentry_cache_misses: AtomicU64,
    chunk_read_queries: AtomicU64,
    chunk_read_chunks: AtomicU64,
    chunk_write_chunks: AtomicU64,
    wal_checkpoint_count: AtomicU64,
    wal_checkpoint_nanos: AtomicU64,
    fuse_write_count: AtomicU64,
    fuse_write_bytes: AtomicU64,
}

impl ProfileCounters {
    pub const fn new() -> Self {
        Self {
            connection_wait_count: AtomicU64::new(0),
            connection_wait_nanos: AtomicU64::new(0),
            connection_create_count: AtomicU64::new(0),
            connection_reuse_count: AtomicU64::new(0),
            dentry_cache_hits: AtomicU64::new(0),
            dentry_cache_misses: AtomicU64::new(0),
            chunk_read_queries: AtomicU64::new(0),
            chunk_read_chunks: AtomicU64::new(0),
            chunk_write_chunks: AtomicU64::new(0),
            wal_checkpoint_count: AtomicU64::new(0),
            wal_checkpoint_nanos: AtomicU64::new(0),
            fuse_write_count: AtomicU64::new(0),
            fuse_write_bytes: AtomicU64::new(0),
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

    fn add_fuse_write(&self, bytes: u64) {
        self.fuse_write_count.fetch_add(1, Ordering::Relaxed);
        self.fuse_write_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ProfileSnapshot {
        ProfileSnapshot {
            connection_wait_count: self.connection_wait_count.load(Ordering::Relaxed),
            connection_wait_nanos: self.connection_wait_nanos.load(Ordering::Relaxed),
            connection_create_count: self.connection_create_count.load(Ordering::Relaxed),
            connection_reuse_count: self.connection_reuse_count.load(Ordering::Relaxed),
            dentry_cache_hits: self.dentry_cache_hits.load(Ordering::Relaxed),
            dentry_cache_misses: self.dentry_cache_misses.load(Ordering::Relaxed),
            chunk_read_queries: self.chunk_read_queries.load(Ordering::Relaxed),
            chunk_read_chunks: self.chunk_read_chunks.load(Ordering::Relaxed),
            chunk_write_chunks: self.chunk_write_chunks.load(Ordering::Relaxed),
            wal_checkpoint_count: self.wal_checkpoint_count.load(Ordering::Relaxed),
            wal_checkpoint_nanos: self.wal_checkpoint_nanos.load(Ordering::Relaxed),
            fuse_write_count: self.fuse_write_count.load(Ordering::Relaxed),
            fuse_write_bytes: self.fuse_write_bytes.load(Ordering::Relaxed),
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

pub fn record_fuse_write(bytes: u64) {
    if is_enabled() {
        COUNTERS.add_fuse_write(bytes);
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
        counters.add_dentry_cache_hit();
        counters.add_dentry_cache_miss();
        counters.add_chunk_read_query();
        counters.add_chunk_read_chunks(3);
        counters.add_chunk_write_chunks(5);
        counters.add_wal_checkpoint(Duration::from_nanos(11));
        counters.add_fuse_write(13);

        let snapshot = counters.snapshot();
        assert_eq!(snapshot.connection_wait_count, 1);
        assert_eq!(snapshot.connection_wait_nanos, 7);
        assert_eq!(snapshot.connection_create_count, 1);
        assert_eq!(snapshot.connection_reuse_count, 1);
        assert_eq!(snapshot.dentry_cache_hits, 1);
        assert_eq!(snapshot.dentry_cache_misses, 1);
        assert_eq!(snapshot.chunk_read_queries, 1);
        assert_eq!(snapshot.chunk_read_chunks, 3);
        assert_eq!(snapshot.chunk_write_chunks, 5);
        assert_eq!(snapshot.wal_checkpoint_count, 1);
        assert_eq!(snapshot.wal_checkpoint_nanos, 11);
        assert_eq!(snapshot.fuse_write_count, 1);
        assert_eq!(snapshot.fuse_write_bytes, 13);
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
