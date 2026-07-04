//! FUSE/CLI-owned telemetry counters.
//!
//! The SDK owns the registry and core counter shapes. This module owns FUSE
//! adapter vocabulary and registers those sections with the SDK registry when
//! the CLI starts.

use std::sync::OnceLock;
use std::time::Duration;

pub use agentfs_core::telemetry::{
    is_enabled, record_base_fast_inode_invalidation, record_base_fast_open_eligible,
    record_base_fast_open_keep_cache, record_base_fast_open_passthrough_attempted,
    record_base_fast_open_passthrough_fallback, record_base_fast_open_passthrough_succeeded,
    record_base_fast_open_rejected, record_base_fast_stale_rejection, record_negative_cache_hit,
    record_negative_cache_invalidation, record_negative_cache_miss, report_checkpoint, timer,
    ProfileReportGuard, ProfileSnapshot, TimerGuard,
};

agentfs_core::telemetry::define_counters! {
    pub(crate) static FUSE_COUNTERS: FuseCounters = "fuse" {
        fuse_callback_count: Counter,
        fuse_lookup_count: Counter,
        fuse_getattr_count: Counter,
        fuse_readdir_count: Counter,
        fuse_readdir_plus_count: Counter,
        fuse_open_count: Counter,
        fuse_uring_requests: Counter,
        fuse_read_count: Counter,
        fuse_release_count: Counter,
        fuse_write_count: Counter,
        fuse_write_bytes: Sum,
        fuse_flush_count: Counter,
        fuse_flush_ranges: Sum,
        fuse_flush_bytes: Sum,
        fuse_noflush_enosys_replies: Counter,
        fuse_pending_tail_drains: Counter,
        fuse_noopen_enosys_replies: Counter,
        fuse_ino_file_resolutions: Counter,
        fuse_ino_file_upgrades: Counter,
        fuse_sync_inval_inode_ok: Counter,
        fuse_sync_inval_inode_err: Counter,
        fuse_sync_inval_entry_ok: Counter,
        fuse_sync_inval_entry_err: Counter,
        fuse_sync_inval_latency_ns_total: Sum,
        fuse_dispatch_wait: Timer,
        fuse_adapter_lock_wait: Timer,
        fuse_read_lane_wait: Timer,
        fuse_write_lane_wait: Timer,
        fuse_read_lane_max_concurrent: Max,
        fuse_exclusive_fallback_count: Counter,
        fuse_worker_queue_depth_peak: Max,
        fuse_dispatch_inline_fallback: Counter,
        fuse_dispatch_parallel_tasks: Counter,
        fuse_dispatch_max_concurrent: Max,
        fuse_readdirplus_auto_requested: Counter,
        fuse_readdirplus_auto_enabled: Counter,
        fuse_readdirplus_do_requested: Counter,
        fuse_readdirplus_do_enabled: Counter,
        fuse_readdirplus_unsupported: Counter,
        fuse_keepcache_eligibility_drops: Counter,
        fuse_adapter_entry_hits: Counter,
        fuse_adapter_entry_misses: Counter,
        fuse_adapter_attr_hits: Counter,
        fuse_adapter_attr_misses: Counter,
        fuse_adapter_negative_hits: Counter,
        fuse_adapter_negative_misses: Counter,
        fuse_adapter_inval_inode_notifications: Counter,
        fuse_adapter_inval_entry_notifications: Counter,
        fuse_op_lookup: Timer,
        fuse_op_getattr: Timer,
        fuse_op_setattr: Timer,
        fuse_op_open: Timer,
        fuse_op_create: Timer,
        fuse_op_read: Timer,
        fuse_op_write: Timer,
        fuse_op_flush: Timer,
        fuse_op_release: Timer,
        fuse_op_readdirplus: Timer,
        fuse_op_forget: Timer,
        fuse_op_other: Timer,
    }
}

agentfs_core::telemetry::define_counters! {
    pub(crate) static CONFIG_COUNTERS: ConfigCounters = "config" {
        fuse_workers_configured: Gauge,
        fuse_readdirplus_mode: Gauge,
        fuse_ttl_entry_ms: Gauge,
        fuse_ttl_attr_ms: Gauge,
        fuse_ttl_neg_ms: Gauge,
        fuse_writeback_cache_enabled: Gauge,
        fuse_keepcache_enabled: Gauge,
    }
}

static REGISTERED: OnceLock<()> = OnceLock::new();

pub fn register_sections() {
    REGISTERED.get_or_init(|| {
        agentfs_core::telemetry::Registry::register(&FUSE_COUNTERS);
        agentfs_core::telemetry::Registry::register(&CONFIG_COUNTERS);
    });
}

pub fn install_cli_sink() -> ProfileReportGuard {
    register_sections();
    ProfileReportGuard::new("cli")
}

pub fn emit_cli_report() {
    register_sections();
    agentfs_core::telemetry::report_summary("cli");
}

pub fn snapshot() -> ProfileSnapshot {
    register_sections();
    agentfs_core::telemetry::snapshot()
}

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

pub fn fuse_op_timer(slot: FuseOpSlot) -> TimerGuard {
    let timer_slot = match slot {
        FuseOpSlot::Lookup => &FUSE_COUNTERS.fuse_op_lookup,
        FuseOpSlot::GetAttr => &FUSE_COUNTERS.fuse_op_getattr,
        FuseOpSlot::SetAttr => &FUSE_COUNTERS.fuse_op_setattr,
        FuseOpSlot::Open => &FUSE_COUNTERS.fuse_op_open,
        FuseOpSlot::Create => &FUSE_COUNTERS.fuse_op_create,
        FuseOpSlot::Read => &FUSE_COUNTERS.fuse_op_read,
        FuseOpSlot::Write => &FUSE_COUNTERS.fuse_op_write,
        FuseOpSlot::Flush => &FUSE_COUNTERS.fuse_op_flush,
        FuseOpSlot::Release => &FUSE_COUNTERS.fuse_op_release,
        FuseOpSlot::ReadDirPlus => &FUSE_COUNTERS.fuse_op_readdirplus,
        FuseOpSlot::Forget => &FUSE_COUNTERS.fuse_op_forget,
        FuseOpSlot::Other => &FUSE_COUNTERS.fuse_op_other,
    };
    timer(timer_slot)
}

pub fn record_fuse_op(slot: FuseOpSlot, duration: Duration) {
    if !is_enabled() {
        return;
    }

    match slot {
        FuseOpSlot::Lookup => FUSE_COUNTERS.fuse_op_lookup.record(duration),
        FuseOpSlot::GetAttr => FUSE_COUNTERS.fuse_op_getattr.record(duration),
        FuseOpSlot::SetAttr => FUSE_COUNTERS.fuse_op_setattr.record(duration),
        FuseOpSlot::Open => FUSE_COUNTERS.fuse_op_open.record(duration),
        FuseOpSlot::Create => FUSE_COUNTERS.fuse_op_create.record(duration),
        FuseOpSlot::Read => FUSE_COUNTERS.fuse_op_read.record(duration),
        FuseOpSlot::Write => FUSE_COUNTERS.fuse_op_write.record(duration),
        FuseOpSlot::Flush => FUSE_COUNTERS.fuse_op_flush.record(duration),
        FuseOpSlot::Release => FUSE_COUNTERS.fuse_op_release.record(duration),
        FuseOpSlot::ReadDirPlus => FUSE_COUNTERS.fuse_op_readdirplus.record(duration),
        FuseOpSlot::Forget => FUSE_COUNTERS.fuse_op_forget.record(duration),
        FuseOpSlot::Other => FUSE_COUNTERS.fuse_op_other.record(duration),
    }
}

fn record_fuse_callback() {
    FUSE_COUNTERS.fuse_callback_count.increment();
}

pub fn record_fuse_lookup() {
    if is_enabled() {
        record_fuse_callback();
        FUSE_COUNTERS.fuse_lookup_count.increment();
    }
}

pub fn record_fuse_getattr() {
    if is_enabled() {
        record_fuse_callback();
        FUSE_COUNTERS.fuse_getattr_count.increment();
    }
}

pub fn record_fuse_readdir() {
    if is_enabled() {
        record_fuse_callback();
        FUSE_COUNTERS.fuse_readdir_count.increment();
    }
}

pub fn record_fuse_readdir_plus() {
    if is_enabled() {
        record_fuse_callback();
        FUSE_COUNTERS.fuse_readdir_plus_count.increment();
    }
}

pub fn record_fuse_open() {
    if is_enabled() {
        record_fuse_callback();
        FUSE_COUNTERS.fuse_open_count.increment();
    }
}

pub fn record_fuse_uring_request() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_uring_requests.increment();
    }
}

pub fn record_fuse_read() {
    if is_enabled() {
        record_fuse_callback();
        FUSE_COUNTERS.fuse_read_count.increment();
    }
}

pub fn record_fuse_release() {
    if is_enabled() {
        record_fuse_callback();
        FUSE_COUNTERS.fuse_release_count.increment();
    }
}

pub fn record_fuse_write(bytes: u64) {
    if is_enabled() {
        record_fuse_callback();
        FUSE_COUNTERS.fuse_write_count.increment();
        FUSE_COUNTERS.fuse_write_bytes.add(bytes);
    }
}

pub fn record_fuse_flush(ranges: u64, bytes: u64) {
    if is_enabled() {
        FUSE_COUNTERS.fuse_flush_count.increment();
        FUSE_COUNTERS.fuse_flush_ranges.add(ranges);
        FUSE_COUNTERS.fuse_flush_bytes.add(bytes);
    }
}

pub fn record_fuse_noflush_enosys_reply() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_noflush_enosys_replies.increment();
    }
}

pub fn record_fuse_pending_tail_drain() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_pending_tail_drains.increment();
    }
}

pub fn record_fuse_noopen_enosys_reply() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_noopen_enosys_replies.increment();
    }
}

pub fn record_fuse_ino_file_resolution() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_ino_file_resolutions.increment();
    }
}

pub fn record_fuse_ino_file_upgrade() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_ino_file_upgrades.increment();
    }
}

pub fn record_fuse_sync_inval_inode_ok() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_sync_inval_inode_ok.increment();
    }
}

pub fn record_fuse_sync_inval_inode_err() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_sync_inval_inode_err.increment();
    }
}

pub fn record_fuse_sync_inval_entry_ok() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_sync_inval_entry_ok.increment();
    }
}

pub fn record_fuse_sync_inval_entry_err() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_sync_inval_entry_err.increment();
    }
}

pub fn record_fuse_sync_inval_latency(duration: Duration) {
    if is_enabled() {
        FUSE_COUNTERS
            .fuse_sync_inval_latency_ns_total
            .add(duration.as_nanos() as u64);
    }
}

pub fn record_fuse_dispatch_wait(duration: Duration) {
    if is_enabled() {
        FUSE_COUNTERS.fuse_dispatch_wait.record(duration);
    }
}

pub fn record_fuse_adapter_lock_wait(duration: Duration) {
    if is_enabled() {
        FUSE_COUNTERS.fuse_adapter_lock_wait.record(duration);
    }
}

pub fn record_fuse_read_lane_wait(duration: Duration) {
    if is_enabled() {
        FUSE_COUNTERS.fuse_read_lane_wait.record(duration);
    }
}

pub fn record_fuse_write_lane_wait(duration: Duration) {
    if is_enabled() {
        FUSE_COUNTERS.fuse_write_lane_wait.record(duration);
    }
}

pub fn record_fuse_read_lane_concurrency(concurrent: u64) {
    if is_enabled() {
        FUSE_COUNTERS
            .fuse_read_lane_max_concurrent
            .update(concurrent);
    }
}

pub fn record_fuse_exclusive_fallback() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_exclusive_fallback_count.increment();
    }
}

pub fn set_fuse_workers_configured(workers: u64) {
    if is_enabled() {
        CONFIG_COUNTERS.fuse_workers_configured.set(workers);
    }
}

pub fn record_fuse_worker_queue_depth(depth: u64) {
    if is_enabled() {
        FUSE_COUNTERS.fuse_worker_queue_depth_peak.update(depth);
    }
}

pub fn record_fuse_dispatch_inline_fallback() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_dispatch_inline_fallback.increment();
    }
}

pub fn record_fuse_dispatch_parallel_task() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_dispatch_parallel_tasks.increment();
    }
}

pub fn record_fuse_dispatch_concurrency(concurrent: u64) {
    if is_enabled() {
        FUSE_COUNTERS
            .fuse_dispatch_max_concurrent
            .update(concurrent);
    }
}

pub fn record_fuse_readdirplus_auto_requested() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_readdirplus_auto_requested.increment();
    }
}

pub fn record_fuse_readdirplus_auto_enabled() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_readdirplus_auto_enabled.increment();
    }
}

pub fn record_fuse_readdirplus_do_requested() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_readdirplus_do_requested.increment();
    }
}

pub fn record_fuse_readdirplus_do_enabled() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_readdirplus_do_enabled.increment();
    }
}

pub fn record_fuse_readdirplus_unsupported() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_readdirplus_unsupported.increment();
    }
}

pub fn set_fuse_readdirplus_mode(mode: u64) {
    if is_enabled() {
        CONFIG_COUNTERS.fuse_readdirplus_mode.set(mode);
    }
}

pub fn set_fuse_ttl_ms(entry_ms: u64, attr_ms: u64, neg_ms: u64) {
    if is_enabled() {
        CONFIG_COUNTERS.fuse_ttl_entry_ms.set(entry_ms);
        CONFIG_COUNTERS.fuse_ttl_attr_ms.set(attr_ms);
        CONFIG_COUNTERS.fuse_ttl_neg_ms.set(neg_ms);
    }
}

pub fn set_fuse_writeback_cache_enabled(enabled: bool) {
    if is_enabled() {
        CONFIG_COUNTERS
            .fuse_writeback_cache_enabled
            .set(u64::from(enabled));
    }
}

pub fn set_fuse_keepcache_enabled(enabled: bool) {
    if is_enabled() {
        CONFIG_COUNTERS
            .fuse_keepcache_enabled
            .set(u64::from(enabled));
    }
}

pub fn record_fuse_keepcache_eligibility_drop() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_keepcache_eligibility_drops.increment();
    }
}

pub fn record_fuse_adapter_entry_hit() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_adapter_entry_hits.increment();
    }
}

pub fn record_fuse_adapter_entry_miss() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_adapter_entry_misses.increment();
    }
}

pub fn record_fuse_adapter_attr_hit() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_adapter_attr_hits.increment();
    }
}

pub fn record_fuse_adapter_attr_miss() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_adapter_attr_misses.increment();
    }
}

pub fn record_fuse_adapter_negative_hit() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_adapter_negative_hits.increment();
    }
}

pub fn record_fuse_adapter_negative_miss() {
    if is_enabled() {
        FUSE_COUNTERS.fuse_adapter_negative_misses.increment();
    }
}

pub fn record_fuse_adapter_inval_inode_notification() {
    if is_enabled() {
        FUSE_COUNTERS
            .fuse_adapter_inval_inode_notifications
            .increment();
    }
}

pub fn record_fuse_adapter_inval_entry_notification() {
    if is_enabled() {
        FUSE_COUNTERS
            .fuse_adapter_inval_entry_notifications
            .increment();
    }
}
