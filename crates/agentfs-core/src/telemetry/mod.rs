//! Lightweight env-gated telemetry counters for AgentFS hot paths.
//!
//! Recording remains a single cached `AGENTFS_PROFILE` branch followed by
//! relaxed atomic updates. Counter sections are declared from compact tables so
//! core and adapter-specific vocabularies can live in their owning crates while
//! still being reported by one process-wide registry.

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[cfg(not(test))]
static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

static SUMMARY_EMITTED: AtomicBool = AtomicBool::new(false);
static CHECKPOINT_SEQ: AtomicU64 = AtomicU64::new(0);
static REGISTRY: std::sync::OnceLock<Mutex<Vec<&'static dyn TelemetrySection>>> =
    std::sync::OnceLock::new();

pub const DEFAULT_PROFILE_ENABLED: bool = false;

/// A named domain of telemetry counters.
pub trait TelemetrySection: Sync {
    fn name(&self) -> &'static str;
    fn snapshot(&self) -> BTreeMap<String, u64>;
}

/// Process-wide registry for telemetry sections owned outside the SDK.
pub struct Registry;

impl Registry {
    pub fn register(section: &'static dyn TelemetrySection) {
        let mut sections = registry_sections().lock();
        if !sections
            .iter()
            .any(|existing| existing.name() == section.name())
        {
            sections.push(section);
        }
    }

    pub fn snapshot() -> ProfileSnapshot {
        snapshot()
    }
}

fn registry_sections() -> &'static Mutex<Vec<&'static dyn TelemetrySection>> {
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// A point-in-time telemetry snapshot grouped by domain.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ProfileSnapshot {
    sections: BTreeMap<String, BTreeMap<String, u64>>,
}

impl ProfileSnapshot {
    fn flat(&self) -> BTreeMap<String, u64> {
        let mut counters = BTreeMap::new();
        for section in self.sections.values() {
            counters.extend(section.iter().map(|(key, value)| (key.clone(), *value)));
        }
        counters
    }

    #[cfg(test)]
    pub(crate) fn counter(&self, name: &str) -> u64 {
        self.flat().get(name).copied().unwrap_or(0)
    }
}

/// Monotonic counter: increment by one.
#[derive(Debug)]
pub struct Counter {
    name: &'static str,
    value: AtomicU64,
}

impl Counter {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            value: AtomicU64::new(0),
        }
    }

    pub fn increment(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    #[doc(hidden)]
    pub fn snapshot_into(&self, out: &mut BTreeMap<String, u64>) {
        out.insert(self.name.to_string(), self.value.load(Ordering::Relaxed));
    }
}

/// Monotonic summing counter.
#[derive(Debug)]
pub struct Sum {
    name: &'static str,
    value: AtomicU64,
}

impl Sum {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            value: AtomicU64::new(0),
        }
    }

    pub fn add(&self, amount: u64) {
        self.value.fetch_add(amount, Ordering::Relaxed);
    }

    #[doc(hidden)]
    pub fn snapshot_into(&self, out: &mut BTreeMap<String, u64>) {
        out.insert(self.name.to_string(), self.value.load(Ordering::Relaxed));
    }
}

/// Duration counter, emitted as `<name>_count` plus `<name>_nanos`.
#[derive(Debug)]
pub struct Timer {
    name: &'static str,
    count: AtomicU64,
    nanos: AtomicU64,
}

impl Timer {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            count: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        }
    }

    pub fn record(&self, duration: Duration) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.nanos
            .fetch_add(duration.as_nanos() as u64, Ordering::Relaxed);
    }

    #[doc(hidden)]
    pub fn snapshot_into(&self, out: &mut BTreeMap<String, u64>) {
        out.insert(
            format!("{}_count", self.name),
            self.count.load(Ordering::Relaxed),
        );
        out.insert(
            format!("{}_nanos", self.name),
            self.nanos.load(Ordering::Relaxed),
        );
    }
}

/// Max-observed counter using relaxed compare-and-swap.
#[derive(Debug)]
pub struct Max {
    name: &'static str,
    value: AtomicU64,
}

impl Max {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            value: AtomicU64::new(0),
        }
    }

    pub fn update(&self, candidate: u64) {
        let mut current = self.value.load(Ordering::Relaxed);
        while candidate > current {
            match self.value.compare_exchange_weak(
                current,
                candidate,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    #[doc(hidden)]
    pub fn snapshot_into(&self, out: &mut BTreeMap<String, u64>) {
        out.insert(self.name.to_string(), self.value.load(Ordering::Relaxed));
    }
}

/// Last-observed configuration or state value.
#[derive(Debug)]
pub struct Gauge {
    name: &'static str,
    value: AtomicU64,
}

impl Gauge {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            value: AtomicU64::new(0),
        }
    }

    pub fn set(&self, value: u64) {
        self.value.store(value, Ordering::Relaxed);
    }

    #[doc(hidden)]
    pub fn snapshot_into(&self, out: &mut BTreeMap<String, u64>) {
        out.insert(self.name.to_string(), self.value.load(Ordering::Relaxed));
    }
}

/// RAII timer guard. Disabled profiling constructs a zero-sized no-op guard.
#[derive(Debug, Default)]
pub struct TimerGuard {
    slot: Option<&'static Timer>,
    started: Option<Instant>,
}

impl TimerGuard {
    fn disarmed() -> Self {
        Self::default()
    }
}

impl Drop for TimerGuard {
    fn drop(&mut self) {
        if let (Some(slot), Some(started)) = (self.slot, self.started.take()) {
            slot.record(started.elapsed());
        }
    }
}

/// Start an env-gated RAII timer for a telemetry slot.
pub fn timer(slot: &'static Timer) -> TimerGuard {
    if is_enabled() {
        TimerGuard {
            slot: Some(slot),
            started: Some(Instant::now()),
        }
    } else {
        TimerGuard::disarmed()
    }
}

#[macro_export]
macro_rules! define_counters {
    (
        $(#[$meta:meta])*
        $vis:vis static $static_name:ident: $type_name:ident = $section:literal {
            $($counter:ident: $shape:ident),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug)]
        $vis struct $type_name {
            $(pub $counter: $crate::telemetry::$shape,)*
        }

        impl $type_name {
            pub const fn new() -> Self {
                Self {
                    $($counter: $crate::telemetry::$shape::new(stringify!($counter)),)*
                }
            }
        }

        impl Default for $type_name {
            fn default() -> Self {
                Self::new()
            }
        }

        $vis static $static_name: $type_name = $type_name::new();

        impl $crate::telemetry::TelemetrySection for $type_name {
            fn name(&self) -> &'static str {
                $section
            }

            fn snapshot(&self) -> std::collections::BTreeMap<String, u64> {
                let mut out = std::collections::BTreeMap::new();
                $(self.$counter.snapshot_into(&mut out);)*
                out
            }
        }
    };
}

pub use crate::define_counters;

crate::telemetry::define_counters! {
    /// SDK/core-owned counters. Adapter vocabulary is registered by adapter crates.
    pub static CORE_COUNTERS: CoreCounters = "core" {
        connection_wait: Timer,
        connection_create: Counter,
        connection_reuse: Counter,
        connection_drop_discards: Counter,
        connection_health_evictions: Counter,
        lookup_count: Counter,
        lookup_delta_count: Counter,
        lookup_base_count: Counter,
        lookup_whiteout_count: Counter,
        getattr_count: Counter,
        readdir_count: Counter,
        readdir_plus_count: Counter,
        path_resolution_count: Counter,
        path_component_count: Sum,
        path_cache_hits: Counter,
        path_cache_misses: Counter,
        negative_lookup_count: Counter,
        negative_cache_hits: Counter,
        negative_cache_misses: Counter,
        negative_cache_invalidations: Counter,
        attr_cache_hits: Counter,
        attr_cache_misses: Counter,
        dentry_cache_hits: Counter,
        dentry_cache_misses: Counter,
        chunk_read_queries: Counter,
        chunk_read_chunks: Sum,
        chunk_write_chunks: Sum,
        agentfs_batcher_enqueues: Counter,
        agentfs_batcher_drains_timer: Counter,
        agentfs_batcher_drains_bytes: Counter,
        agentfs_batcher_drains_explicit: Counter,
        agentfs_batcher_pending_max_bytes: Max,
        agentfs_batcher_coalesced_ranges: Sum,
        agentfs_batcher_commit_latency_ns_total: Sum,
        agentfs_batcher_commit_txns: Counter,
        agentfs_batcher_txn_inodes_total: Sum,
        agentfs_batcher_txn_inodes_max: Max,
        wal_checkpoint: Timer,
    }
}

crate::telemetry::define_counters! {
    /// Base-file fast-path counters emitted by core overlay/host integration.
    pub static BASE_COUNTERS: BaseCounters = "base" {
        base_fast_open_eligible: Counter,
        base_fast_open_keep_cache: Counter,
        base_fast_open_passthrough_attempted: Counter,
        base_fast_open_passthrough_succeeded: Counter,
        base_fast_open_passthrough_fallback: Counter,
        base_fast_open_rejected: Counter,
        base_fast_inode_invalidations: Counter,
        base_fast_stale_rejections: Counter,
    }
}

/// Returns true when profiling is enabled with `AGENTFS_PROFILE=1`.
/// Always-on under `#[cfg(test)]` so unit tests can assert on counters without
/// racing the global `OnceLock` init.
pub fn is_enabled() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        *ENABLED.get_or_init(|| {
            crate::config::EnvReader::new().bool("AGENTFS_PROFILE", DEFAULT_PROFILE_ENABLED)
        })
    }
}

pub(crate) fn record_connection_create() {
    if is_enabled() {
        CORE_COUNTERS.connection_create.increment();
    }
}

pub(crate) fn record_connection_reuse() {
    if is_enabled() {
        CORE_COUNTERS.connection_reuse.increment();
    }
}

pub(crate) fn record_connection_drop_discard() {
    if is_enabled() {
        CORE_COUNTERS.connection_drop_discards.increment();
    }
}

pub(crate) fn record_connection_health_eviction() {
    if is_enabled() {
        CORE_COUNTERS.connection_health_evictions.increment();
    }
}

pub(crate) fn record_lookup() {
    if is_enabled() {
        CORE_COUNTERS.lookup_count.increment();
    }
}

pub(crate) fn record_lookup_delta() {
    if is_enabled() {
        CORE_COUNTERS.lookup_delta_count.increment();
    }
}

pub(crate) fn record_lookup_base() {
    if is_enabled() {
        CORE_COUNTERS.lookup_base_count.increment();
    }
}

pub(crate) fn record_lookup_whiteout() {
    if is_enabled() {
        CORE_COUNTERS.lookup_whiteout_count.increment();
    }
}

pub(crate) fn record_getattr() {
    if is_enabled() {
        CORE_COUNTERS.getattr_count.increment();
    }
}

pub(crate) fn record_readdir() {
    if is_enabled() {
        CORE_COUNTERS.readdir_count.increment();
    }
}

pub(crate) fn record_readdir_plus() {
    if is_enabled() {
        CORE_COUNTERS.readdir_plus_count.increment();
    }
}

pub(crate) fn record_path_resolution(components: u64) {
    if is_enabled() {
        CORE_COUNTERS.path_resolution_count.increment();
        CORE_COUNTERS.path_component_count.add(components);
    }
}

pub(crate) fn record_path_cache_hit() {
    if is_enabled() {
        CORE_COUNTERS.path_cache_hits.increment();
    }
}

pub(crate) fn record_path_cache_miss() {
    if is_enabled() {
        CORE_COUNTERS.path_cache_misses.increment();
    }
}

pub(crate) fn record_negative_lookup() {
    if is_enabled() {
        CORE_COUNTERS.negative_lookup_count.increment();
    }
}

pub fn record_negative_cache_hit() {
    if is_enabled() {
        CORE_COUNTERS.negative_cache_hits.increment();
    }
}

pub fn record_negative_cache_miss() {
    if is_enabled() {
        CORE_COUNTERS.negative_cache_misses.increment();
    }
}

pub fn record_negative_cache_invalidation() {
    if is_enabled() {
        CORE_COUNTERS.negative_cache_invalidations.increment();
    }
}

pub(crate) fn record_attr_cache_hit() {
    if is_enabled() {
        CORE_COUNTERS.attr_cache_hits.increment();
    }
}

pub(crate) fn record_attr_cache_miss() {
    if is_enabled() {
        CORE_COUNTERS.attr_cache_misses.increment();
    }
}

pub(crate) fn record_dentry_cache_hit() {
    if is_enabled() {
        CORE_COUNTERS.dentry_cache_hits.increment();
    }
}

pub(crate) fn record_dentry_cache_miss() {
    if is_enabled() {
        CORE_COUNTERS.dentry_cache_misses.increment();
    }
}

pub(crate) fn record_chunk_read_query() {
    if is_enabled() {
        CORE_COUNTERS.chunk_read_queries.increment();
    }
}

pub(crate) fn record_chunk_read_chunks(chunks: u64) {
    if is_enabled() {
        CORE_COUNTERS.chunk_read_chunks.add(chunks);
    }
}

pub(crate) fn record_chunk_write_chunks(chunks: u64) {
    if is_enabled() {
        CORE_COUNTERS.chunk_write_chunks.add(chunks);
    }
}

pub(crate) fn record_agentfs_batcher_enqueue() {
    if is_enabled() {
        CORE_COUNTERS.agentfs_batcher_enqueues.increment();
    }
}

pub(crate) fn record_agentfs_batcher_drain_timer() {
    if is_enabled() {
        CORE_COUNTERS.agentfs_batcher_drains_timer.increment();
    }
}

pub(crate) fn record_agentfs_batcher_drain_bytes() {
    if is_enabled() {
        CORE_COUNTERS.agentfs_batcher_drains_bytes.increment();
    }
}

pub(crate) fn record_agentfs_batcher_drain_explicit() {
    if is_enabled() {
        CORE_COUNTERS.agentfs_batcher_drains_explicit.increment();
    }
}

pub(crate) fn record_agentfs_batcher_pending_bytes(pending_bytes: u64) {
    if is_enabled() {
        CORE_COUNTERS
            .agentfs_batcher_pending_max_bytes
            .update(pending_bytes);
    }
}

pub(crate) fn record_agentfs_batcher_coalesced_ranges(ranges: u64) {
    if is_enabled() && ranges > 0 {
        CORE_COUNTERS.agentfs_batcher_coalesced_ranges.add(ranges);
    }
}

pub(crate) fn record_agentfs_batcher_commit_latency(duration: Duration) {
    if is_enabled() {
        CORE_COUNTERS
            .agentfs_batcher_commit_latency_ns_total
            .add(duration.as_nanos() as u64);
    }
}

/// Record one batcher SQLite commit transaction that covered `inodes` inodes.
pub(crate) fn record_agentfs_batcher_commit_txn(inodes: u64) {
    if is_enabled() {
        CORE_COUNTERS.agentfs_batcher_commit_txns.increment();
        CORE_COUNTERS.agentfs_batcher_txn_inodes_total.add(inodes);
        CORE_COUNTERS.agentfs_batcher_txn_inodes_max.update(inodes);
    }
}

pub fn record_base_fast_open_eligible() {
    if is_enabled() {
        BASE_COUNTERS.base_fast_open_eligible.increment();
    }
}

pub fn record_base_fast_open_keep_cache() {
    if is_enabled() {
        BASE_COUNTERS.base_fast_open_keep_cache.increment();
    }
}

pub(crate) fn record_base_fast_open_passthrough_attempted() {
    if is_enabled() {
        BASE_COUNTERS
            .base_fast_open_passthrough_attempted
            .increment();
    }
}

pub(crate) fn record_base_fast_open_passthrough_succeeded() {
    if is_enabled() {
        BASE_COUNTERS
            .base_fast_open_passthrough_succeeded
            .increment();
    }
}

pub(crate) fn record_base_fast_open_passthrough_fallback() {
    if is_enabled() {
        BASE_COUNTERS
            .base_fast_open_passthrough_fallback
            .increment();
    }
}

pub fn record_base_fast_open_rejected() {
    if is_enabled() {
        BASE_COUNTERS.base_fast_open_rejected.increment();
    }
}

pub fn record_base_fast_inode_invalidation() {
    if is_enabled() {
        BASE_COUNTERS.base_fast_inode_invalidations.increment();
    }
}

pub fn record_base_fast_stale_rejection() {
    if is_enabled() {
        BASE_COUNTERS.base_fast_stale_rejections.increment();
    }
}

pub fn snapshot() -> ProfileSnapshot {
    let mut sections = BTreeMap::new();
    sections.insert(CORE_COUNTERS.name().to_string(), CORE_COUNTERS.snapshot());
    sections.insert(BASE_COUNTERS.name().to_string(), BASE_COUNTERS.snapshot());
    for section in registry_sections().lock().iter() {
        sections.insert(section.name().to_string(), section.snapshot());
    }
    ProfileSnapshot { sections }
}

const fn passthrough_supported() -> bool {
    false
}

const fn passthrough_fallback_read_path() -> &'static str {
    "hostfs"
}

fn summary_payload(event: &str, source: &str, snapshot: &ProfileSnapshot) -> String {
    serde_json::json!({
        "event": event,
        "source": source,
        "sections": &snapshot.sections,
        "counters": snapshot.flat(),
        "passthrough_supported": passthrough_supported(),
        "fallback_read_path": passthrough_fallback_read_path(),
    })
    .to_string()
}

/// Return the process summary payload at most once.
pub fn take_summary_payload(event: &str, source: &str) -> Option<String> {
    if !is_enabled() {
        return None;
    }

    if SUMMARY_EMITTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        Some(summary_payload(event, source, &snapshot()))
    } else {
        None
    }
}

/// Return a cumulative profile checkpoint payload tagged with a sequence number.
pub fn checkpoint_payload(event: &str) -> Option<String> {
    if !is_enabled() {
        return None;
    }

    let seq = CHECKPOINT_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    Some(summary_payload(
        event,
        &format!("phase-checkpoint-{seq}"),
        &snapshot(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn macro_counters_accumulate_expected_values() {
        crate::telemetry::define_counters! {
            static TEST_COUNTERS: TestCounters = "test" {
                hits: Counter,
                bytes: Sum,
                latency: Timer,
                peak: Max,
                current: Gauge,
            }
        }

        TEST_COUNTERS.hits.increment();
        TEST_COUNTERS.bytes.add(5);
        TEST_COUNTERS.bytes.add(7);
        TEST_COUNTERS.latency.record(Duration::from_nanos(11));
        TEST_COUNTERS.latency.record(Duration::from_nanos(13));
        TEST_COUNTERS.peak.update(4);
        TEST_COUNTERS.peak.update(3);
        TEST_COUNTERS.current.set(9);

        let section = TEST_COUNTERS.snapshot();
        assert_eq!(section["hits"], 1);
        assert_eq!(section["bytes"], 12);
        assert_eq!(section["latency_count"], 2);
        assert_eq!(section["latency_nanos"], 24);
        assert_eq!(section["peak"], 4);
        assert_eq!(section["current"], 9);
    }

    #[test]
    fn snapshot_has_core_and_base_sections() {
        CORE_COUNTERS
            .connection_wait
            .record(Duration::from_nanos(7));
        record_connection_create();
        record_base_fast_open_eligible();

        let snapshot = snapshot();
        assert!(snapshot.sections.contains_key("core"));
        assert!(snapshot.sections.contains_key("base"));
        assert!(snapshot.counter("connection_wait_count") >= 1);
        assert!(snapshot.counter("connection_wait_nanos") >= 7);
        assert!(snapshot.counter("connection_create") >= 1);
        assert!(snapshot.counter("base_fast_open_eligible") >= 1);
    }

    #[test]
    fn summary_json_is_structured_with_sections_and_flat_counters() {
        let mut sections = BTreeMap::new();
        sections.insert(
            "core".to_string(),
            BTreeMap::from([("connection_create".to_string(), 1)]),
        );
        let snapshot = ProfileSnapshot { sections };
        let value: Value = serde_json::from_str(&summary_payload(
            "unit_profile_summary",
            "unit-test",
            &snapshot,
        ))
        .expect("summary json should parse");

        assert_eq!(value["event"], "unit_profile_summary");
        assert_eq!(value["source"], "unit-test");
        assert_eq!(value["sections"]["core"]["connection_create"], 1);
        assert_eq!(value["counters"]["connection_create"], 1);
    }

    #[test]
    fn sdk_only_usage_does_not_emit_report() {
        record_connection_create();
        let snapshot = snapshot();
        assert!(snapshot.counter("connection_create") >= 1);
        // No report guard is constructed here. The validation harness runs this
        // test with --nocapture and asserts no CLI profile summary line is
        // printed by SDK-only usage.
    }
}
