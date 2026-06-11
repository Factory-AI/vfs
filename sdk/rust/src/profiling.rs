//! Lightweight env-gated profiling counters for AgentFS hot paths.
//!
//! The public recording helpers are intentionally tiny when profiling is
//! disabled: each call performs one cached environment-gate check and returns.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[cfg(not(test))]
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
    pub negative_cache_hits: u64,
    pub negative_cache_misses: u64,
    pub negative_cache_invalidations: u64,
    pub attr_cache_hits: u64,
    pub attr_cache_misses: u64,
    pub dentry_cache_hits: u64,
    pub dentry_cache_misses: u64,
    pub chunk_read_queries: u64,
    pub chunk_read_chunks: u64,
    pub chunk_write_chunks: u64,
    pub agentfs_batcher_enqueues: u64,
    pub agentfs_batcher_drains_timer: u64,
    pub agentfs_batcher_drains_bytes: u64,
    pub agentfs_batcher_drains_explicit: u64,
    pub agentfs_batcher_pending_max_bytes: u64,
    pub agentfs_batcher_coalesced_ranges: u64,
    pub agentfs_batcher_commit_latency_ns_total: u64,
    pub agentfs_batcher_commit_txns: u64,
    pub agentfs_batcher_txn_inodes_total: u64,
    pub agentfs_batcher_txn_inodes_max: u64,
    pub wal_checkpoint_count: u64,
    pub wal_checkpoint_nanos: u64,
    pub fuse_callback_count: u64,
    pub fuse_lookup_count: u64,
    pub fuse_getattr_count: u64,
    pub fuse_readdir_count: u64,
    pub fuse_readdir_plus_count: u64,
    pub fuse_open_count: u64,
    pub fuse_uring_requests: u64,
    pub fuse_read_count: u64,
    pub fuse_release_count: u64,
    pub fuse_write_count: u64,
    pub fuse_write_bytes: u64,
    pub fuse_flush_count: u64,
    pub fuse_flush_ranges: u64,
    pub fuse_flush_bytes: u64,
    pub fuse_sync_inval_inode_ok: u64,
    pub fuse_sync_inval_inode_err: u64,
    pub fuse_sync_inval_entry_ok: u64,
    pub fuse_sync_inval_entry_err: u64,
    pub fuse_sync_inval_latency_ns_total: u64,
    pub fuse_dispatch_wait_count: u64,
    pub fuse_dispatch_wait_nanos: u64,
    pub fuse_adapter_lock_wait_count: u64,
    pub fuse_adapter_lock_wait_nanos: u64,
    pub fuse_read_lane_wait_count: u64,
    pub fuse_read_lane_wait_nanos: u64,
    pub fuse_write_lane_wait_count: u64,
    pub fuse_write_lane_wait_nanos: u64,
    pub fuse_read_lane_max_concurrent: u64,
    pub fuse_exclusive_fallback_count: u64,
    pub fuse_workers_configured: u64,
    pub fuse_worker_queue_depth_peak: u64,
    pub fuse_dispatch_inline_fallback: u64,
    pub fuse_dispatch_parallel_tasks: u64,
    pub fuse_dispatch_max_concurrent: u64,
    pub fuse_readdirplus_auto_requested: u64,
    pub fuse_readdirplus_auto_enabled: u64,
    pub fuse_readdirplus_do_requested: u64,
    pub fuse_readdirplus_do_enabled: u64,
    pub fuse_readdirplus_unsupported: u64,
    pub fuse_readdirplus_mode: u64,
    pub fuse_ttl_entry_ms: u64,
    pub fuse_ttl_attr_ms: u64,
    pub fuse_ttl_neg_ms: u64,
    pub fuse_writeback_cache_enabled: u64,
    pub fuse_keepcache_enabled: u64,
    pub fuse_keepcache_eligibility_drops: u64,
    pub fuse_adapter_entry_hits: u64,
    pub fuse_adapter_entry_misses: u64,
    pub fuse_adapter_attr_hits: u64,
    pub fuse_adapter_attr_misses: u64,
    pub fuse_adapter_negative_hits: u64,
    pub fuse_adapter_negative_misses: u64,
    pub fuse_adapter_inval_inode_notifications: u64,
    pub fuse_adapter_inval_entry_notifications: u64,
    pub base_fast_open_eligible: u64,
    pub base_fast_open_keep_cache: u64,
    pub base_fast_open_passthrough_attempted: u64,
    pub base_fast_open_passthrough_succeeded: u64,
    pub base_fast_open_passthrough_fallback: u64,
    pub base_fast_open_rejected: u64,
    pub base_fast_inode_invalidations: u64,
    pub base_fast_stale_rejections: u64,
    /// Per-opcode dispatch latency, flattened as
    /// `fuse_op_<name>_count` / `fuse_op_<name>_nanos` keys (zero slots
    /// omitted) so generic counter tooling sees plain integers. Measured
    /// around the whole dispatch: parse → handler → reply send.
    #[serde(flatten)]
    pub fuse_op_latency: std::collections::BTreeMap<String, u64>,
}

/// Dispatch-level FUSE opcode slots for per-op latency accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseOpSlot {
    Lookup,
    GetAttr,
    SetAttr,
    Open,
    Create,
    Read,
    Write,
    Flush,
    Release,
    ReadDirPlus,
    Forget,
    Other,
}

const FUSE_OP_SLOT_COUNT: usize = 12;
const FUSE_OP_SLOT_NAMES: [&str; FUSE_OP_SLOT_COUNT] = [
    "lookup",
    "getattr",
    "setattr",
    "open",
    "create",
    "read",
    "write",
    "flush",
    "release",
    "readdirplus",
    "forget",
    "other",
];

/// Atomic profiling counters.
#[derive(Debug)]
pub struct ProfileCounters {
    fuse_op_counts: [AtomicU64; FUSE_OP_SLOT_COUNT],
    fuse_op_nanos: [AtomicU64; FUSE_OP_SLOT_COUNT],
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
    negative_cache_hits: AtomicU64,
    negative_cache_misses: AtomicU64,
    negative_cache_invalidations: AtomicU64,
    attr_cache_hits: AtomicU64,
    attr_cache_misses: AtomicU64,
    dentry_cache_hits: AtomicU64,
    dentry_cache_misses: AtomicU64,
    chunk_read_queries: AtomicU64,
    chunk_read_chunks: AtomicU64,
    chunk_write_chunks: AtomicU64,
    agentfs_batcher_enqueues: AtomicU64,
    agentfs_batcher_drains_timer: AtomicU64,
    agentfs_batcher_drains_bytes: AtomicU64,
    agentfs_batcher_drains_explicit: AtomicU64,
    agentfs_batcher_pending_max_bytes: AtomicU64,
    agentfs_batcher_coalesced_ranges: AtomicU64,
    agentfs_batcher_commit_latency_ns_total: AtomicU64,
    agentfs_batcher_commit_txns: AtomicU64,
    agentfs_batcher_txn_inodes_total: AtomicU64,
    agentfs_batcher_txn_inodes_max: AtomicU64,
    wal_checkpoint_count: AtomicU64,
    wal_checkpoint_nanos: AtomicU64,
    fuse_callback_count: AtomicU64,
    fuse_lookup_count: AtomicU64,
    fuse_getattr_count: AtomicU64,
    fuse_readdir_count: AtomicU64,
    fuse_readdir_plus_count: AtomicU64,
    fuse_open_count: AtomicU64,
    fuse_uring_requests: AtomicU64,
    fuse_read_count: AtomicU64,
    fuse_release_count: AtomicU64,
    fuse_write_count: AtomicU64,
    fuse_write_bytes: AtomicU64,
    fuse_flush_count: AtomicU64,
    fuse_flush_ranges: AtomicU64,
    fuse_flush_bytes: AtomicU64,
    fuse_sync_inval_inode_ok: AtomicU64,
    fuse_sync_inval_inode_err: AtomicU64,
    fuse_sync_inval_entry_ok: AtomicU64,
    fuse_sync_inval_entry_err: AtomicU64,
    fuse_sync_inval_latency_ns_total: AtomicU64,
    fuse_dispatch_wait_count: AtomicU64,
    fuse_dispatch_wait_nanos: AtomicU64,
    fuse_adapter_lock_wait_count: AtomicU64,
    fuse_adapter_lock_wait_nanos: AtomicU64,
    fuse_read_lane_wait_count: AtomicU64,
    fuse_read_lane_wait_nanos: AtomicU64,
    fuse_write_lane_wait_count: AtomicU64,
    fuse_write_lane_wait_nanos: AtomicU64,
    fuse_read_lane_max_concurrent: AtomicU64,
    fuse_exclusive_fallback_count: AtomicU64,
    fuse_workers_configured: AtomicU64,
    fuse_worker_queue_depth_peak: AtomicU64,
    fuse_dispatch_inline_fallback: AtomicU64,
    fuse_dispatch_parallel_tasks: AtomicU64,
    fuse_dispatch_max_concurrent: AtomicU64,
    fuse_readdirplus_auto_requested: AtomicU64,
    fuse_readdirplus_auto_enabled: AtomicU64,
    fuse_readdirplus_do_requested: AtomicU64,
    fuse_readdirplus_do_enabled: AtomicU64,
    fuse_readdirplus_unsupported: AtomicU64,
    fuse_readdirplus_mode: AtomicU64,
    fuse_ttl_entry_ms: AtomicU64,
    fuse_ttl_attr_ms: AtomicU64,
    fuse_ttl_neg_ms: AtomicU64,
    fuse_writeback_cache_enabled: AtomicU64,
    fuse_keepcache_enabled: AtomicU64,
    fuse_keepcache_eligibility_drops: AtomicU64,
    fuse_adapter_entry_hits: AtomicU64,
    fuse_adapter_entry_misses: AtomicU64,
    fuse_adapter_attr_hits: AtomicU64,
    fuse_adapter_attr_misses: AtomicU64,
    fuse_adapter_negative_hits: AtomicU64,
    fuse_adapter_negative_misses: AtomicU64,
    fuse_adapter_inval_inode_notifications: AtomicU64,
    fuse_adapter_inval_entry_notifications: AtomicU64,
    base_fast_open_eligible: AtomicU64,
    base_fast_open_keep_cache: AtomicU64,
    base_fast_open_passthrough_attempted: AtomicU64,
    base_fast_open_passthrough_succeeded: AtomicU64,
    base_fast_open_passthrough_fallback: AtomicU64,
    base_fast_open_rejected: AtomicU64,
    base_fast_inode_invalidations: AtomicU64,
    base_fast_stale_rejections: AtomicU64,
}

impl ProfileCounters {
    pub const fn new() -> Self {
        Self {
            fuse_op_counts: [const { AtomicU64::new(0) }; FUSE_OP_SLOT_COUNT],
            fuse_op_nanos: [const { AtomicU64::new(0) }; FUSE_OP_SLOT_COUNT],
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
            negative_cache_hits: AtomicU64::new(0),
            negative_cache_misses: AtomicU64::new(0),
            negative_cache_invalidations: AtomicU64::new(0),
            attr_cache_hits: AtomicU64::new(0),
            attr_cache_misses: AtomicU64::new(0),
            dentry_cache_hits: AtomicU64::new(0),
            dentry_cache_misses: AtomicU64::new(0),
            chunk_read_queries: AtomicU64::new(0),
            chunk_read_chunks: AtomicU64::new(0),
            chunk_write_chunks: AtomicU64::new(0),
            agentfs_batcher_enqueues: AtomicU64::new(0),
            agentfs_batcher_drains_timer: AtomicU64::new(0),
            agentfs_batcher_drains_bytes: AtomicU64::new(0),
            agentfs_batcher_drains_explicit: AtomicU64::new(0),
            agentfs_batcher_pending_max_bytes: AtomicU64::new(0),
            agentfs_batcher_coalesced_ranges: AtomicU64::new(0),
            agentfs_batcher_commit_latency_ns_total: AtomicU64::new(0),
            agentfs_batcher_commit_txns: AtomicU64::new(0),
            agentfs_batcher_txn_inodes_total: AtomicU64::new(0),
            agentfs_batcher_txn_inodes_max: AtomicU64::new(0),
            wal_checkpoint_count: AtomicU64::new(0),
            wal_checkpoint_nanos: AtomicU64::new(0),
            fuse_callback_count: AtomicU64::new(0),
            fuse_lookup_count: AtomicU64::new(0),
            fuse_getattr_count: AtomicU64::new(0),
            fuse_readdir_count: AtomicU64::new(0),
            fuse_readdir_plus_count: AtomicU64::new(0),
            fuse_open_count: AtomicU64::new(0),
            fuse_uring_requests: AtomicU64::new(0),
            fuse_read_count: AtomicU64::new(0),
            fuse_release_count: AtomicU64::new(0),
            fuse_write_count: AtomicU64::new(0),
            fuse_write_bytes: AtomicU64::new(0),
            fuse_flush_count: AtomicU64::new(0),
            fuse_flush_ranges: AtomicU64::new(0),
            fuse_flush_bytes: AtomicU64::new(0),
            fuse_sync_inval_inode_ok: AtomicU64::new(0),
            fuse_sync_inval_inode_err: AtomicU64::new(0),
            fuse_sync_inval_entry_ok: AtomicU64::new(0),
            fuse_sync_inval_entry_err: AtomicU64::new(0),
            fuse_sync_inval_latency_ns_total: AtomicU64::new(0),
            fuse_dispatch_wait_count: AtomicU64::new(0),
            fuse_dispatch_wait_nanos: AtomicU64::new(0),
            fuse_adapter_lock_wait_count: AtomicU64::new(0),
            fuse_adapter_lock_wait_nanos: AtomicU64::new(0),
            fuse_read_lane_wait_count: AtomicU64::new(0),
            fuse_read_lane_wait_nanos: AtomicU64::new(0),
            fuse_write_lane_wait_count: AtomicU64::new(0),
            fuse_write_lane_wait_nanos: AtomicU64::new(0),
            fuse_read_lane_max_concurrent: AtomicU64::new(0),
            fuse_exclusive_fallback_count: AtomicU64::new(0),
            fuse_workers_configured: AtomicU64::new(0),
            fuse_worker_queue_depth_peak: AtomicU64::new(0),
            fuse_dispatch_inline_fallback: AtomicU64::new(0),
            fuse_dispatch_parallel_tasks: AtomicU64::new(0),
            fuse_dispatch_max_concurrent: AtomicU64::new(0),
            fuse_readdirplus_auto_requested: AtomicU64::new(0),
            fuse_readdirplus_auto_enabled: AtomicU64::new(0),
            fuse_readdirplus_do_requested: AtomicU64::new(0),
            fuse_readdirplus_do_enabled: AtomicU64::new(0),
            fuse_readdirplus_unsupported: AtomicU64::new(0),
            fuse_readdirplus_mode: AtomicU64::new(0),
            fuse_ttl_entry_ms: AtomicU64::new(0),
            fuse_ttl_attr_ms: AtomicU64::new(0),
            fuse_ttl_neg_ms: AtomicU64::new(0),
            fuse_writeback_cache_enabled: AtomicU64::new(0),
            fuse_keepcache_enabled: AtomicU64::new(0),
            fuse_keepcache_eligibility_drops: AtomicU64::new(0),
            fuse_adapter_entry_hits: AtomicU64::new(0),
            fuse_adapter_entry_misses: AtomicU64::new(0),
            fuse_adapter_attr_hits: AtomicU64::new(0),
            fuse_adapter_attr_misses: AtomicU64::new(0),
            fuse_adapter_negative_hits: AtomicU64::new(0),
            fuse_adapter_negative_misses: AtomicU64::new(0),
            fuse_adapter_inval_inode_notifications: AtomicU64::new(0),
            fuse_adapter_inval_entry_notifications: AtomicU64::new(0),
            base_fast_open_eligible: AtomicU64::new(0),
            base_fast_open_keep_cache: AtomicU64::new(0),
            base_fast_open_passthrough_attempted: AtomicU64::new(0),
            base_fast_open_passthrough_succeeded: AtomicU64::new(0),
            base_fast_open_passthrough_fallback: AtomicU64::new(0),
            base_fast_open_rejected: AtomicU64::new(0),
            base_fast_inode_invalidations: AtomicU64::new(0),
            base_fast_stale_rejections: AtomicU64::new(0),
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

    fn add_negative_cache_hit(&self) {
        self.negative_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn add_negative_cache_miss(&self) {
        self.negative_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    fn add_negative_cache_invalidation(&self) {
        self.negative_cache_invalidations
            .fetch_add(1, Ordering::Relaxed);
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

    fn add_agentfs_batcher_enqueue(&self) {
        self.agentfs_batcher_enqueues
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_agentfs_batcher_drain_timer(&self) {
        self.agentfs_batcher_drains_timer
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_agentfs_batcher_drain_bytes(&self) {
        self.agentfs_batcher_drains_bytes
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_agentfs_batcher_drain_explicit(&self) {
        self.agentfs_batcher_drains_explicit
            .fetch_add(1, Ordering::Relaxed);
    }

    fn update_agentfs_batcher_pending_max_bytes(&self, pending_bytes: u64) {
        let mut current = self
            .agentfs_batcher_pending_max_bytes
            .load(Ordering::Relaxed);
        while pending_bytes > current {
            match self
                .agentfs_batcher_pending_max_bytes
                .compare_exchange_weak(current, pending_bytes, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    fn add_agentfs_batcher_coalesced_ranges(&self, ranges: u64) {
        self.agentfs_batcher_coalesced_ranges
            .fetch_add(ranges, Ordering::Relaxed);
    }

    fn add_agentfs_batcher_commit_latency(&self, duration: Duration) {
        self.agentfs_batcher_commit_latency_ns_total
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    /// One batcher SQLite commit transaction covering `inodes` inodes. Counts
    /// actual `BEGIN IMMEDIATE`/`COMMIT` pairs (not per-inode drain ticks) so
    /// the transaction shape of the write batcher is directly observable.
    fn add_agentfs_batcher_commit_txn(&self, inodes: u64) {
        self.agentfs_batcher_commit_txns
            .fetch_add(1, Ordering::Relaxed);
        self.agentfs_batcher_txn_inodes_total
            .fetch_add(inodes, Ordering::Relaxed);
        let mut current = self.agentfs_batcher_txn_inodes_max.load(Ordering::Relaxed);
        while inodes > current {
            match self.agentfs_batcher_txn_inodes_max.compare_exchange_weak(
                current,
                inodes,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    fn add_fuse_op(&self, slot: FuseOpSlot, duration: Duration) {
        let idx = slot as usize;
        self.fuse_op_counts[idx].fetch_add(1, Ordering::Relaxed);
        self.fuse_op_nanos[idx].fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
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

    fn add_fuse_uring_request(&self) {
        self.fuse_uring_requests.fetch_add(1, Ordering::Relaxed);
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
        self.fuse_flush_count.fetch_add(1, Ordering::Relaxed);
        self.fuse_flush_ranges.fetch_add(ranges, Ordering::Relaxed);
        self.fuse_flush_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn add_fuse_sync_inval_inode_ok(&self) {
        self.fuse_sync_inval_inode_ok
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_sync_inval_inode_err(&self) {
        self.fuse_sync_inval_inode_err
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_sync_inval_entry_ok(&self) {
        self.fuse_sync_inval_entry_ok
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_sync_inval_entry_err(&self) {
        self.fuse_sync_inval_entry_err
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_sync_inval_latency(&self, duration: Duration) {
        self.fuse_sync_inval_latency_ns_total
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    fn add_fuse_dispatch_wait(&self, duration: Duration) {
        self.fuse_dispatch_wait_count
            .fetch_add(1, Ordering::Relaxed);
        self.fuse_dispatch_wait_nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    fn add_fuse_adapter_lock_wait(&self, duration: Duration) {
        self.fuse_adapter_lock_wait_count
            .fetch_add(1, Ordering::Relaxed);
        self.fuse_adapter_lock_wait_nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    fn add_fuse_read_lane_wait(&self, duration: Duration) {
        self.fuse_read_lane_wait_count
            .fetch_add(1, Ordering::Relaxed);
        self.fuse_read_lane_wait_nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    fn add_fuse_write_lane_wait(&self, duration: Duration) {
        self.fuse_write_lane_wait_count
            .fetch_add(1, Ordering::Relaxed);
        self.fuse_write_lane_wait_nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    fn update_fuse_read_lane_max_concurrent(&self, concurrent: u64) {
        let mut current = self.fuse_read_lane_max_concurrent.load(Ordering::Relaxed);
        while concurrent > current {
            match self.fuse_read_lane_max_concurrent.compare_exchange_weak(
                current,
                concurrent,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    fn add_fuse_exclusive_fallback(&self) {
        self.fuse_exclusive_fallback_count
            .fetch_add(1, Ordering::Relaxed);
    }

    fn set_fuse_workers_configured(&self, workers: u64) {
        self.fuse_workers_configured
            .store(workers, Ordering::Relaxed);
    }

    fn update_fuse_worker_queue_depth_peak(&self, depth: u64) {
        let mut current = self.fuse_worker_queue_depth_peak.load(Ordering::Relaxed);
        while depth > current {
            match self.fuse_worker_queue_depth_peak.compare_exchange_weak(
                current,
                depth,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    fn add_fuse_dispatch_inline_fallback(&self) {
        self.fuse_dispatch_inline_fallback
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_dispatch_parallel_task(&self) {
        self.fuse_dispatch_parallel_tasks
            .fetch_add(1, Ordering::Relaxed);
    }

    fn update_fuse_dispatch_max_concurrent(&self, concurrent: u64) {
        let mut current = self.fuse_dispatch_max_concurrent.load(Ordering::Relaxed);
        while concurrent > current {
            match self.fuse_dispatch_max_concurrent.compare_exchange_weak(
                current,
                concurrent,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    fn add_fuse_readdirplus_auto_requested(&self) {
        self.fuse_readdirplus_auto_requested
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_readdirplus_auto_enabled(&self) {
        self.fuse_readdirplus_auto_enabled
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_readdirplus_do_requested(&self) {
        self.fuse_readdirplus_do_requested
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_readdirplus_do_enabled(&self) {
        self.fuse_readdirplus_do_enabled
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_readdirplus_unsupported(&self) {
        self.fuse_readdirplus_unsupported
            .fetch_add(1, Ordering::Relaxed);
    }

    fn set_fuse_readdirplus_mode(&self, mode: u64) {
        self.fuse_readdirplus_mode.store(mode, Ordering::Relaxed);
    }

    fn set_fuse_ttl_ms(&self, entry_ms: u64, attr_ms: u64, neg_ms: u64) {
        self.fuse_ttl_entry_ms.store(entry_ms, Ordering::Relaxed);
        self.fuse_ttl_attr_ms.store(attr_ms, Ordering::Relaxed);
        self.fuse_ttl_neg_ms.store(neg_ms, Ordering::Relaxed);
    }

    fn set_fuse_writeback_cache_enabled(&self, enabled: bool) {
        self.fuse_writeback_cache_enabled
            .store(u64::from(enabled), Ordering::Relaxed);
    }

    fn set_fuse_keepcache_enabled(&self, enabled: bool) {
        self.fuse_keepcache_enabled
            .store(u64::from(enabled), Ordering::Relaxed);
    }

    fn add_fuse_keepcache_eligibility_drop(&self) {
        self.fuse_keepcache_eligibility_drops
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_adapter_entry_hit(&self) {
        self.fuse_adapter_entry_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_adapter_entry_miss(&self) {
        self.fuse_adapter_entry_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_adapter_attr_hit(&self) {
        self.fuse_adapter_attr_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_adapter_attr_miss(&self) {
        self.fuse_adapter_attr_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_adapter_negative_hit(&self) {
        self.fuse_adapter_negative_hits
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_adapter_negative_miss(&self) {
        self.fuse_adapter_negative_misses
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_adapter_inval_inode_notification(&self) {
        self.fuse_adapter_inval_inode_notifications
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_fuse_adapter_inval_entry_notification(&self) {
        self.fuse_adapter_inval_entry_notifications
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_base_fast_open_eligible(&self) {
        self.base_fast_open_eligible.fetch_add(1, Ordering::Relaxed);
    }

    fn add_base_fast_open_keep_cache(&self) {
        self.base_fast_open_keep_cache
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_base_fast_open_passthrough_attempted(&self) {
        self.base_fast_open_passthrough_attempted
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_base_fast_open_passthrough_succeeded(&self) {
        self.base_fast_open_passthrough_succeeded
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_base_fast_open_passthrough_fallback(&self) {
        self.base_fast_open_passthrough_fallback
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_base_fast_open_rejected(&self) {
        self.base_fast_open_rejected.fetch_add(1, Ordering::Relaxed);
    }

    fn add_base_fast_inode_invalidation(&self) {
        self.base_fast_inode_invalidations
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_base_fast_stale_rejection(&self) {
        self.base_fast_stale_rejections
            .fetch_add(1, Ordering::Relaxed);
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
            negative_cache_hits: self.negative_cache_hits.load(Ordering::Relaxed),
            negative_cache_misses: self.negative_cache_misses.load(Ordering::Relaxed),
            negative_cache_invalidations: self.negative_cache_invalidations.load(Ordering::Relaxed),
            attr_cache_hits: self.attr_cache_hits.load(Ordering::Relaxed),
            attr_cache_misses: self.attr_cache_misses.load(Ordering::Relaxed),
            dentry_cache_hits: self.dentry_cache_hits.load(Ordering::Relaxed),
            dentry_cache_misses: self.dentry_cache_misses.load(Ordering::Relaxed),
            chunk_read_queries: self.chunk_read_queries.load(Ordering::Relaxed),
            chunk_read_chunks: self.chunk_read_chunks.load(Ordering::Relaxed),
            chunk_write_chunks: self.chunk_write_chunks.load(Ordering::Relaxed),
            agentfs_batcher_enqueues: self.agentfs_batcher_enqueues.load(Ordering::Relaxed),
            agentfs_batcher_drains_timer: self.agentfs_batcher_drains_timer.load(Ordering::Relaxed),
            agentfs_batcher_drains_bytes: self.agentfs_batcher_drains_bytes.load(Ordering::Relaxed),
            agentfs_batcher_drains_explicit: self
                .agentfs_batcher_drains_explicit
                .load(Ordering::Relaxed),
            agentfs_batcher_pending_max_bytes: self
                .agentfs_batcher_pending_max_bytes
                .load(Ordering::Relaxed),
            agentfs_batcher_coalesced_ranges: self
                .agentfs_batcher_coalesced_ranges
                .load(Ordering::Relaxed),
            agentfs_batcher_commit_latency_ns_total: self
                .agentfs_batcher_commit_latency_ns_total
                .load(Ordering::Relaxed),
            agentfs_batcher_commit_txns: self.agentfs_batcher_commit_txns.load(Ordering::Relaxed),
            agentfs_batcher_txn_inodes_total: self
                .agentfs_batcher_txn_inodes_total
                .load(Ordering::Relaxed),
            agentfs_batcher_txn_inodes_max: self
                .agentfs_batcher_txn_inodes_max
                .load(Ordering::Relaxed),
            wal_checkpoint_count: self.wal_checkpoint_count.load(Ordering::Relaxed),
            wal_checkpoint_nanos: self.wal_checkpoint_nanos.load(Ordering::Relaxed),
            fuse_callback_count: self.fuse_callback_count.load(Ordering::Relaxed),
            fuse_lookup_count: self.fuse_lookup_count.load(Ordering::Relaxed),
            fuse_getattr_count: self.fuse_getattr_count.load(Ordering::Relaxed),
            fuse_readdir_count: self.fuse_readdir_count.load(Ordering::Relaxed),
            fuse_readdir_plus_count: self.fuse_readdir_plus_count.load(Ordering::Relaxed),
            fuse_open_count: self.fuse_open_count.load(Ordering::Relaxed),
            fuse_uring_requests: self.fuse_uring_requests.load(Ordering::Relaxed),
            fuse_read_count: self.fuse_read_count.load(Ordering::Relaxed),
            fuse_release_count: self.fuse_release_count.load(Ordering::Relaxed),
            fuse_write_count: self.fuse_write_count.load(Ordering::Relaxed),
            fuse_write_bytes: self.fuse_write_bytes.load(Ordering::Relaxed),
            fuse_flush_count: self.fuse_flush_count.load(Ordering::Relaxed),
            fuse_flush_ranges: self.fuse_flush_ranges.load(Ordering::Relaxed),
            fuse_flush_bytes: self.fuse_flush_bytes.load(Ordering::Relaxed),
            fuse_sync_inval_inode_ok: self.fuse_sync_inval_inode_ok.load(Ordering::Relaxed),
            fuse_sync_inval_inode_err: self.fuse_sync_inval_inode_err.load(Ordering::Relaxed),
            fuse_sync_inval_entry_ok: self.fuse_sync_inval_entry_ok.load(Ordering::Relaxed),
            fuse_sync_inval_entry_err: self.fuse_sync_inval_entry_err.load(Ordering::Relaxed),
            fuse_sync_inval_latency_ns_total: self
                .fuse_sync_inval_latency_ns_total
                .load(Ordering::Relaxed),
            fuse_dispatch_wait_count: self.fuse_dispatch_wait_count.load(Ordering::Relaxed),
            fuse_dispatch_wait_nanos: self.fuse_dispatch_wait_nanos.load(Ordering::Relaxed),
            fuse_adapter_lock_wait_count: self.fuse_adapter_lock_wait_count.load(Ordering::Relaxed),
            fuse_adapter_lock_wait_nanos: self.fuse_adapter_lock_wait_nanos.load(Ordering::Relaxed),
            fuse_read_lane_wait_count: self.fuse_read_lane_wait_count.load(Ordering::Relaxed),
            fuse_read_lane_wait_nanos: self.fuse_read_lane_wait_nanos.load(Ordering::Relaxed),
            fuse_write_lane_wait_count: self.fuse_write_lane_wait_count.load(Ordering::Relaxed),
            fuse_write_lane_wait_nanos: self.fuse_write_lane_wait_nanos.load(Ordering::Relaxed),
            fuse_read_lane_max_concurrent: self
                .fuse_read_lane_max_concurrent
                .load(Ordering::Relaxed),
            fuse_exclusive_fallback_count: self
                .fuse_exclusive_fallback_count
                .load(Ordering::Relaxed),
            fuse_workers_configured: self.fuse_workers_configured.load(Ordering::Relaxed),
            fuse_worker_queue_depth_peak: self.fuse_worker_queue_depth_peak.load(Ordering::Relaxed),
            fuse_dispatch_inline_fallback: self
                .fuse_dispatch_inline_fallback
                .load(Ordering::Relaxed),
            fuse_dispatch_parallel_tasks: self.fuse_dispatch_parallel_tasks.load(Ordering::Relaxed),
            fuse_dispatch_max_concurrent: self.fuse_dispatch_max_concurrent.load(Ordering::Relaxed),
            fuse_readdirplus_auto_requested: self
                .fuse_readdirplus_auto_requested
                .load(Ordering::Relaxed),
            fuse_readdirplus_auto_enabled: self
                .fuse_readdirplus_auto_enabled
                .load(Ordering::Relaxed),
            fuse_readdirplus_do_requested: self
                .fuse_readdirplus_do_requested
                .load(Ordering::Relaxed),
            fuse_readdirplus_do_enabled: self.fuse_readdirplus_do_enabled.load(Ordering::Relaxed),
            fuse_readdirplus_unsupported: self.fuse_readdirplus_unsupported.load(Ordering::Relaxed),
            fuse_readdirplus_mode: self.fuse_readdirplus_mode.load(Ordering::Relaxed),
            fuse_ttl_entry_ms: self.fuse_ttl_entry_ms.load(Ordering::Relaxed),
            fuse_ttl_attr_ms: self.fuse_ttl_attr_ms.load(Ordering::Relaxed),
            fuse_ttl_neg_ms: self.fuse_ttl_neg_ms.load(Ordering::Relaxed),
            fuse_writeback_cache_enabled: self.fuse_writeback_cache_enabled.load(Ordering::Relaxed),
            fuse_keepcache_enabled: self.fuse_keepcache_enabled.load(Ordering::Relaxed),
            fuse_keepcache_eligibility_drops: self
                .fuse_keepcache_eligibility_drops
                .load(Ordering::Relaxed),
            fuse_adapter_entry_hits: self.fuse_adapter_entry_hits.load(Ordering::Relaxed),
            fuse_adapter_entry_misses: self.fuse_adapter_entry_misses.load(Ordering::Relaxed),
            fuse_adapter_attr_hits: self.fuse_adapter_attr_hits.load(Ordering::Relaxed),
            fuse_adapter_attr_misses: self.fuse_adapter_attr_misses.load(Ordering::Relaxed),
            fuse_adapter_negative_hits: self.fuse_adapter_negative_hits.load(Ordering::Relaxed),
            fuse_adapter_negative_misses: self.fuse_adapter_negative_misses.load(Ordering::Relaxed),
            fuse_adapter_inval_inode_notifications: self
                .fuse_adapter_inval_inode_notifications
                .load(Ordering::Relaxed),
            fuse_adapter_inval_entry_notifications: self
                .fuse_adapter_inval_entry_notifications
                .load(Ordering::Relaxed),
            base_fast_open_eligible: self.base_fast_open_eligible.load(Ordering::Relaxed),
            base_fast_open_keep_cache: self.base_fast_open_keep_cache.load(Ordering::Relaxed),
            base_fast_open_passthrough_attempted: self
                .base_fast_open_passthrough_attempted
                .load(Ordering::Relaxed),
            base_fast_open_passthrough_succeeded: self
                .base_fast_open_passthrough_succeeded
                .load(Ordering::Relaxed),
            base_fast_open_passthrough_fallback: self
                .base_fast_open_passthrough_fallback
                .load(Ordering::Relaxed),
            base_fast_open_rejected: self.base_fast_open_rejected.load(Ordering::Relaxed),
            base_fast_inode_invalidations: self
                .base_fast_inode_invalidations
                .load(Ordering::Relaxed),
            base_fast_stale_rejections: self.base_fast_stale_rejections.load(Ordering::Relaxed),
            fuse_op_latency: {
                let mut map = std::collections::BTreeMap::new();
                for (idx, name) in FUSE_OP_SLOT_NAMES.iter().enumerate() {
                    let count = self.fuse_op_counts[idx].load(Ordering::Relaxed);
                    if count > 0 {
                        map.insert(format!("fuse_op_{name}_count"), count);
                        map.insert(
                            format!("fuse_op_{name}_nanos"),
                            self.fuse_op_nanos[idx].load(Ordering::Relaxed),
                        );
                    }
                }
                map
            },
        }
    }
}

impl Default for ProfileCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns true when profiling is enabled with `AGENTFS_PROFILE=1`.
/// Always-on under `#[cfg(test)]` so unit tests can assert on counters
/// without racing the global `OnceCell` init.
pub fn is_enabled() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        *ENABLED.get_or_init(|| {
            std::env::var("AGENTFS_PROFILE")
                .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
                .unwrap_or(false)
        })
    }
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

pub fn record_negative_cache_hit() {
    if is_enabled() {
        COUNTERS.add_negative_cache_hit();
    }
}

pub fn record_negative_cache_miss() {
    if is_enabled() {
        COUNTERS.add_negative_cache_miss();
    }
}

pub fn record_negative_cache_invalidation() {
    if is_enabled() {
        COUNTERS.add_negative_cache_invalidation();
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

pub fn record_agentfs_batcher_enqueue() {
    if is_enabled() {
        COUNTERS.add_agentfs_batcher_enqueue();
    }
}

pub fn record_agentfs_batcher_drain_timer() {
    if is_enabled() {
        COUNTERS.add_agentfs_batcher_drain_timer();
    }
}

pub fn record_agentfs_batcher_drain_bytes() {
    if is_enabled() {
        COUNTERS.add_agentfs_batcher_drain_bytes();
    }
}

pub fn record_agentfs_batcher_drain_explicit() {
    if is_enabled() {
        COUNTERS.add_agentfs_batcher_drain_explicit();
    }
}

pub fn record_agentfs_batcher_pending_bytes(pending_bytes: u64) {
    if is_enabled() {
        COUNTERS.update_agentfs_batcher_pending_max_bytes(pending_bytes);
    }
}

pub fn record_agentfs_batcher_coalesced_ranges(ranges: u64) {
    if is_enabled() && ranges > 0 {
        COUNTERS.add_agentfs_batcher_coalesced_ranges(ranges);
    }
}

pub fn record_agentfs_batcher_commit_latency(duration: Duration) {
    if is_enabled() {
        COUNTERS.add_agentfs_batcher_commit_latency(duration);
    }
}

/// Record one batcher SQLite commit transaction that covered `inodes` inodes.
pub fn record_agentfs_batcher_commit_txn(inodes: u64) {
    if is_enabled() {
        COUNTERS.add_agentfs_batcher_commit_txn(inodes);
    }
}

/// Record one FUSE request's full dispatch latency (parse → handler → reply).
pub fn record_fuse_op(slot: FuseOpSlot, duration: Duration) {
    if is_enabled() {
        COUNTERS.add_fuse_op(slot, duration);
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

/// Count a FUSE request delivered via the fuse-over-io_uring transport.
pub fn record_fuse_uring_request() {
    if is_enabled() {
        COUNTERS.add_fuse_uring_request();
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

pub fn record_fuse_sync_inval_inode_ok() {
    if is_enabled() {
        COUNTERS.add_fuse_sync_inval_inode_ok();
    }
}

pub fn record_fuse_sync_inval_inode_err() {
    if is_enabled() {
        COUNTERS.add_fuse_sync_inval_inode_err();
    }
}

pub fn record_fuse_sync_inval_entry_ok() {
    if is_enabled() {
        COUNTERS.add_fuse_sync_inval_entry_ok();
    }
}

pub fn record_fuse_sync_inval_entry_err() {
    if is_enabled() {
        COUNTERS.add_fuse_sync_inval_entry_err();
    }
}

pub fn record_fuse_sync_inval_latency(duration: Duration) {
    if is_enabled() {
        COUNTERS.add_fuse_sync_inval_latency(duration);
    }
}

pub fn record_fuse_dispatch_wait(duration: Duration) {
    if is_enabled() {
        COUNTERS.add_fuse_dispatch_wait(duration);
    }
}

pub fn record_fuse_adapter_lock_wait(duration: Duration) {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_lock_wait(duration);
    }
}

pub fn record_fuse_read_lane_wait(duration: Duration) {
    if is_enabled() {
        COUNTERS.add_fuse_read_lane_wait(duration);
    }
}

pub fn record_fuse_write_lane_wait(duration: Duration) {
    if is_enabled() {
        COUNTERS.add_fuse_write_lane_wait(duration);
    }
}

pub fn record_fuse_read_lane_concurrency(concurrent: u64) {
    if is_enabled() {
        COUNTERS.update_fuse_read_lane_max_concurrent(concurrent);
    }
}

pub fn record_fuse_exclusive_fallback() {
    if is_enabled() {
        COUNTERS.add_fuse_exclusive_fallback();
    }
}

pub fn set_fuse_workers_configured(workers: u64) {
    if is_enabled() {
        COUNTERS.set_fuse_workers_configured(workers);
    }
}

pub fn record_fuse_worker_queue_depth(depth: u64) {
    if is_enabled() {
        COUNTERS.update_fuse_worker_queue_depth_peak(depth);
    }
}

pub fn record_fuse_dispatch_inline_fallback() {
    if is_enabled() {
        COUNTERS.add_fuse_dispatch_inline_fallback();
    }
}

pub fn record_fuse_dispatch_parallel_task() {
    if is_enabled() {
        COUNTERS.add_fuse_dispatch_parallel_task();
    }
}

pub fn record_fuse_dispatch_concurrency(concurrent: u64) {
    if is_enabled() {
        COUNTERS.update_fuse_dispatch_max_concurrent(concurrent);
    }
}

pub fn record_fuse_readdirplus_auto_requested() {
    if is_enabled() {
        COUNTERS.add_fuse_readdirplus_auto_requested();
    }
}

pub fn record_fuse_readdirplus_auto_enabled() {
    if is_enabled() {
        COUNTERS.add_fuse_readdirplus_auto_enabled();
    }
}

pub fn record_fuse_readdirplus_do_requested() {
    if is_enabled() {
        COUNTERS.add_fuse_readdirplus_do_requested();
    }
}

pub fn record_fuse_readdirplus_do_enabled() {
    if is_enabled() {
        COUNTERS.add_fuse_readdirplus_do_enabled();
    }
}

pub fn record_fuse_readdirplus_unsupported() {
    if is_enabled() {
        COUNTERS.add_fuse_readdirplus_unsupported();
    }
}

pub fn set_fuse_readdirplus_mode(mode: u64) {
    if is_enabled() {
        COUNTERS.set_fuse_readdirplus_mode(mode);
    }
}

pub fn set_fuse_ttl_ms(entry_ms: u64, attr_ms: u64, neg_ms: u64) {
    if is_enabled() {
        COUNTERS.set_fuse_ttl_ms(entry_ms, attr_ms, neg_ms);
    }
}

pub fn set_fuse_writeback_cache_enabled(enabled: bool) {
    if is_enabled() {
        COUNTERS.set_fuse_writeback_cache_enabled(enabled);
    }
}

pub fn set_fuse_keepcache_enabled(enabled: bool) {
    if is_enabled() {
        COUNTERS.set_fuse_keepcache_enabled(enabled);
    }
}

pub fn record_fuse_keepcache_eligibility_drop() {
    if is_enabled() {
        COUNTERS.add_fuse_keepcache_eligibility_drop();
    }
}

pub fn record_fuse_adapter_entry_hit() {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_entry_hit();
    }
}

pub fn record_fuse_adapter_entry_miss() {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_entry_miss();
    }
}

pub fn record_fuse_adapter_attr_hit() {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_attr_hit();
    }
}

pub fn record_fuse_adapter_attr_miss() {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_attr_miss();
    }
}

pub fn record_fuse_adapter_negative_hit() {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_negative_hit();
    }
}

pub fn record_fuse_adapter_negative_miss() {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_negative_miss();
    }
}

pub fn record_fuse_adapter_inval_inode_notification() {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_inval_inode_notification();
    }
}

pub fn record_fuse_adapter_inval_entry_notification() {
    if is_enabled() {
        COUNTERS.add_fuse_adapter_inval_entry_notification();
    }
}

pub fn record_base_fast_open_eligible() {
    if is_enabled() {
        COUNTERS.add_base_fast_open_eligible();
    }
}

pub fn record_base_fast_open_keep_cache() {
    if is_enabled() {
        COUNTERS.add_base_fast_open_keep_cache();
    }
}

pub fn record_base_fast_open_passthrough_attempted() {
    if is_enabled() {
        COUNTERS.add_base_fast_open_passthrough_attempted();
    }
}

pub fn record_base_fast_open_passthrough_succeeded() {
    if is_enabled() {
        COUNTERS.add_base_fast_open_passthrough_succeeded();
    }
}

pub fn record_base_fast_open_passthrough_fallback() {
    if is_enabled() {
        COUNTERS.add_base_fast_open_passthrough_fallback();
    }
}

pub fn record_base_fast_open_rejected() {
    if is_enabled() {
        COUNTERS.add_base_fast_open_rejected();
    }
}

pub fn record_base_fast_inode_invalidation() {
    if is_enabled() {
        COUNTERS.add_base_fast_inode_invalidation();
    }
}

pub fn record_base_fast_stale_rejection() {
    if is_enabled() {
        COUNTERS.add_base_fast_stale_rejection();
    }
}

pub fn snapshot() -> ProfileSnapshot {
    COUNTERS.snapshot()
}

pub const fn passthrough_supported() -> bool {
    false
}

pub const fn passthrough_fallback_read_path() -> &'static str {
    "hostfs"
}

fn summary_json(source: &str, snapshot: &ProfileSnapshot) -> String {
    serde_json::json!({
        "event": "agentfs_profile_summary",
        "source": source,
        "counters": snapshot,
        "passthrough_supported": passthrough_supported(),
        "fallback_read_path": passthrough_fallback_read_path(),
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

/// Monotonic sequence for phase-boundary profile checkpoints.
static CHECKPOINT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Emit a cumulative profile summary tagged with a monotonic sequence number.
///
/// Used to attribute counters to workload phases: a consumer subtracts
/// consecutive checkpoint snapshots to obtain per-phase deltas. The sequence
/// number makes ordering unambiguous even if stderr lines interleave.
pub fn report_checkpoint() {
    if !is_enabled() {
        return;
    }

    let seq = CHECKPOINT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!(
        "{}",
        summary_json(&format!("phase-checkpoint-{seq}"), &snapshot())
    );
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
        counters.add_negative_cache_hit();
        counters.add_negative_cache_miss();
        counters.add_negative_cache_invalidation();
        counters.add_attr_cache_hit();
        counters.add_attr_cache_miss();
        counters.add_dentry_cache_hit();
        counters.add_dentry_cache_miss();
        counters.add_chunk_read_query();
        counters.add_chunk_read_chunks(3);
        counters.add_chunk_write_chunks(5);
        counters.add_agentfs_batcher_enqueue();
        counters.add_agentfs_batcher_drain_timer();
        counters.add_agentfs_batcher_drain_bytes();
        counters.add_agentfs_batcher_drain_explicit();
        counters.update_agentfs_batcher_pending_max_bytes(64);
        counters.update_agentfs_batcher_pending_max_bytes(32);
        counters.add_agentfs_batcher_coalesced_ranges(2);
        counters.add_agentfs_batcher_commit_latency(Duration::from_nanos(17));
        counters.add_agentfs_batcher_commit_txn(3);
        counters.add_agentfs_batcher_commit_txn(9);
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
        counters.add_fuse_sync_inval_inode_ok();
        counters.add_fuse_sync_inval_inode_err();
        counters.add_fuse_sync_inval_entry_ok();
        counters.add_fuse_sync_inval_entry_err();
        counters.add_fuse_sync_inval_latency(Duration::from_nanos(29));
        counters.add_fuse_dispatch_wait(Duration::from_nanos(31));
        counters.add_fuse_adapter_lock_wait(Duration::from_nanos(37));
        counters.add_fuse_read_lane_wait(Duration::from_nanos(41));
        counters.add_fuse_write_lane_wait(Duration::from_nanos(43));
        counters.update_fuse_read_lane_max_concurrent(2);
        counters.update_fuse_read_lane_max_concurrent(5);
        counters.update_fuse_read_lane_max_concurrent(3);
        counters.add_fuse_exclusive_fallback();
        counters.set_fuse_workers_configured(4);
        counters.update_fuse_worker_queue_depth_peak(7);
        counters.update_fuse_worker_queue_depth_peak(5);
        counters.add_fuse_dispatch_inline_fallback();
        counters.add_fuse_dispatch_parallel_task();
        counters.add_fuse_dispatch_parallel_task();
        counters.update_fuse_dispatch_max_concurrent(3);
        counters.update_fuse_dispatch_max_concurrent(6);
        counters.update_fuse_dispatch_max_concurrent(2);
        counters.set_fuse_readdirplus_mode(1);
        counters.set_fuse_ttl_ms(1000, 750, 250);
        counters.set_fuse_writeback_cache_enabled(true);
        counters.set_fuse_keepcache_enabled(true);
        counters.add_fuse_keepcache_eligibility_drop();
        counters.add_fuse_adapter_entry_hit();
        counters.add_fuse_adapter_entry_miss();
        counters.add_fuse_adapter_attr_hit();
        counters.add_fuse_adapter_attr_miss();
        counters.add_fuse_adapter_negative_hit();
        counters.add_fuse_adapter_negative_miss();
        counters.add_fuse_adapter_inval_inode_notification();
        counters.add_fuse_adapter_inval_entry_notification();
        counters.add_base_fast_open_eligible();
        counters.add_base_fast_open_keep_cache();
        counters.add_base_fast_open_passthrough_attempted();
        counters.add_base_fast_open_passthrough_succeeded();
        counters.add_base_fast_open_passthrough_fallback();
        counters.add_base_fast_open_rejected();
        counters.add_base_fast_inode_invalidation();
        counters.add_base_fast_stale_rejection();

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
        assert_eq!(snapshot.negative_cache_hits, 1);
        assert_eq!(snapshot.negative_cache_misses, 1);
        assert_eq!(snapshot.negative_cache_invalidations, 1);
        assert_eq!(snapshot.attr_cache_hits, 1);
        assert_eq!(snapshot.attr_cache_misses, 1);
        assert_eq!(snapshot.dentry_cache_hits, 1);
        assert_eq!(snapshot.dentry_cache_misses, 1);
        assert_eq!(snapshot.chunk_read_queries, 1);
        assert_eq!(snapshot.chunk_read_chunks, 3);
        assert_eq!(snapshot.chunk_write_chunks, 5);
        assert_eq!(snapshot.agentfs_batcher_enqueues, 1);
        assert_eq!(snapshot.agentfs_batcher_drains_timer, 1);
        assert_eq!(snapshot.agentfs_batcher_drains_bytes, 1);
        assert_eq!(snapshot.agentfs_batcher_drains_explicit, 1);
        assert_eq!(snapshot.agentfs_batcher_pending_max_bytes, 64);
        assert_eq!(snapshot.agentfs_batcher_coalesced_ranges, 2);
        assert_eq!(snapshot.agentfs_batcher_commit_latency_ns_total, 17);
        assert_eq!(snapshot.agentfs_batcher_commit_txns, 2);
        assert_eq!(snapshot.agentfs_batcher_txn_inodes_total, 12);
        assert_eq!(snapshot.agentfs_batcher_txn_inodes_max, 9);
        assert_eq!(snapshot.wal_checkpoint_count, 1);
        assert_eq!(snapshot.wal_checkpoint_nanos, 11);
        assert_eq!(snapshot.fuse_callback_count, 8);
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
        assert_eq!(snapshot.fuse_sync_inval_inode_ok, 1);
        assert_eq!(snapshot.fuse_sync_inval_inode_err, 1);
        assert_eq!(snapshot.fuse_sync_inval_entry_ok, 1);
        assert_eq!(snapshot.fuse_sync_inval_entry_err, 1);
        assert_eq!(snapshot.fuse_sync_inval_latency_ns_total, 29);
        assert_eq!(snapshot.fuse_dispatch_wait_count, 1);
        assert_eq!(snapshot.fuse_dispatch_wait_nanos, 31);
        assert_eq!(snapshot.fuse_adapter_lock_wait_count, 1);
        assert_eq!(snapshot.fuse_adapter_lock_wait_nanos, 37);
        assert_eq!(snapshot.fuse_read_lane_wait_count, 1);
        assert_eq!(snapshot.fuse_read_lane_wait_nanos, 41);
        assert_eq!(snapshot.fuse_write_lane_wait_count, 1);
        assert_eq!(snapshot.fuse_write_lane_wait_nanos, 43);
        assert_eq!(snapshot.fuse_read_lane_max_concurrent, 5);
        assert_eq!(snapshot.fuse_exclusive_fallback_count, 1);
        assert_eq!(snapshot.fuse_workers_configured, 4);
        assert_eq!(snapshot.fuse_worker_queue_depth_peak, 7);
        assert_eq!(snapshot.fuse_dispatch_inline_fallback, 1);
        assert_eq!(snapshot.fuse_dispatch_parallel_tasks, 2);
        assert_eq!(snapshot.fuse_dispatch_max_concurrent, 6);
        assert_eq!(snapshot.fuse_readdirplus_mode, 1);
        assert_eq!(snapshot.fuse_ttl_entry_ms, 1000);
        assert_eq!(snapshot.fuse_ttl_attr_ms, 750);
        assert_eq!(snapshot.fuse_ttl_neg_ms, 250);
        assert_eq!(snapshot.fuse_writeback_cache_enabled, 1);
        assert_eq!(snapshot.fuse_keepcache_enabled, 1);
        assert_eq!(snapshot.fuse_keepcache_eligibility_drops, 1);
        assert_eq!(snapshot.fuse_adapter_entry_hits, 1);
        assert_eq!(snapshot.fuse_adapter_entry_misses, 1);
        assert_eq!(snapshot.fuse_adapter_attr_hits, 1);
        assert_eq!(snapshot.fuse_adapter_attr_misses, 1);
        assert_eq!(snapshot.fuse_adapter_negative_hits, 1);
        assert_eq!(snapshot.fuse_adapter_negative_misses, 1);
        assert_eq!(snapshot.fuse_adapter_inval_inode_notifications, 1);
        assert_eq!(snapshot.fuse_adapter_inval_entry_notifications, 1);
        assert_eq!(snapshot.base_fast_open_eligible, 1);
        assert_eq!(snapshot.base_fast_open_keep_cache, 1);
        assert_eq!(snapshot.base_fast_open_passthrough_attempted, 1);
        assert_eq!(snapshot.base_fast_open_passthrough_succeeded, 1);
        assert_eq!(snapshot.base_fast_open_passthrough_fallback, 1);
        assert_eq!(snapshot.base_fast_open_rejected, 1);
        assert_eq!(snapshot.base_fast_inode_invalidations, 1);
        assert_eq!(snapshot.base_fast_stale_rejections, 1);
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

    #[test]
    fn summary_json_includes_phase65_fast_path_counters() {
        let counters = ProfileCounters::new();
        counters.add_fuse_dispatch_wait(Duration::from_nanos(5));
        counters.add_fuse_adapter_lock_wait(Duration::from_nanos(6));
        counters.add_fuse_read_lane_wait(Duration::from_nanos(7));
        counters.add_fuse_write_lane_wait(Duration::from_nanos(8));
        counters.update_fuse_read_lane_max_concurrent(3);
        counters.add_fuse_exclusive_fallback();
        counters.set_fuse_workers_configured(4);
        counters.update_fuse_worker_queue_depth_peak(9);
        counters.add_fuse_dispatch_inline_fallback();
        counters.add_fuse_dispatch_parallel_task();
        counters.update_fuse_dispatch_max_concurrent(5);
        counters.set_fuse_readdirplus_mode(2);
        counters.set_fuse_ttl_ms(1000, 1000, 500);
        counters.set_fuse_writeback_cache_enabled(true);
        counters.set_fuse_keepcache_enabled(true);
        counters.add_fuse_keepcache_eligibility_drop();
        counters.add_base_fast_open_eligible();
        counters.add_base_fast_open_keep_cache();
        counters.add_base_fast_open_passthrough_fallback();
        counters.add_base_fast_open_rejected();
        counters.add_base_fast_inode_invalidation();
        counters.add_base_fast_stale_rejection();

        let value: Value = serde_json::from_str(&summary_json("unit-test", &counters.snapshot()))
            .expect("summary JSON should parse");
        let counters = &value["counters"];

        assert_eq!(counters["fuse_dispatch_wait_count"], 1);
        assert_eq!(counters["fuse_dispatch_wait_nanos"], 5);
        assert_eq!(counters["fuse_adapter_lock_wait_count"], 1);
        assert_eq!(counters["fuse_adapter_lock_wait_nanos"], 6);
        assert_eq!(counters["fuse_read_lane_wait_count"], 1);
        assert_eq!(counters["fuse_read_lane_wait_nanos"], 7);
        assert_eq!(counters["fuse_write_lane_wait_count"], 1);
        assert_eq!(counters["fuse_write_lane_wait_nanos"], 8);
        assert_eq!(counters["fuse_read_lane_max_concurrent"], 3);
        assert_eq!(counters["fuse_exclusive_fallback_count"], 1);
        assert_eq!(counters["fuse_workers_configured"], 4);
        assert_eq!(counters["fuse_worker_queue_depth_peak"], 9);
        assert_eq!(counters["fuse_dispatch_inline_fallback"], 1);
        assert_eq!(counters["fuse_dispatch_parallel_tasks"], 1);
        assert_eq!(counters["fuse_dispatch_max_concurrent"], 5);
        assert_eq!(counters["fuse_readdirplus_mode"], 2);
        assert_eq!(counters["fuse_ttl_entry_ms"], 1000);
        assert_eq!(counters["fuse_ttl_attr_ms"], 1000);
        assert_eq!(counters["fuse_ttl_neg_ms"], 500);
        assert_eq!(counters["fuse_writeback_cache_enabled"], 1);
        assert_eq!(counters["fuse_keepcache_enabled"], 1);
        assert_eq!(counters["fuse_keepcache_eligibility_drops"], 1);
        assert_eq!(counters["base_fast_open_eligible"], 1);
        assert_eq!(counters["base_fast_open_keep_cache"], 1);
        assert_eq!(counters["base_fast_open_passthrough_attempted"], 0);
        assert_eq!(counters["base_fast_open_passthrough_succeeded"], 0);
        assert_eq!(counters["base_fast_open_passthrough_fallback"], 1);
        assert_eq!(counters["base_fast_open_rejected"], 1);
        assert_eq!(counters["base_fast_inode_invalidations"], 1);
        assert_eq!(counters["base_fast_stale_rejections"], 1);
    }
}
