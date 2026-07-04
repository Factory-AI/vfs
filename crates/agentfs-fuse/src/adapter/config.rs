//! Typed FUSE runtime configuration.
//!
//! This is the single environment-reading edge for Linux FUSE adapter and
//! transport knobs. The adapter and session receive values from [`FuseConfig`]
//! instead of reading process environment directly.

use std::env::VarError;
use std::time::Duration;

use agentfs_core::EnvReader;

/// The max size of write requests plus header slack used by the FUSE session
/// request buffer. Dispatch auto-sizing must account for one buffer per
/// worker plus queued request.
pub(crate) const FUSE_REQUEST_BUFFER_SIZE: usize = (16 * 1024 * 1024) + 4096;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum FuseWorkersDefault {
    Auto,
}

impl FuseWorkersDefault {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
        }
    }

    fn matches(self, value: &str) -> bool {
        value.trim().eq_ignore_ascii_case(self.as_str())
    }

    fn resolve(self) -> usize {
        match self {
            Self::Auto => workers_from_resource_percent(
                env_percent("AGENTFS_FUSE_CPU_PERCENT", DEFAULT_AUTO_PERCENT),
                env_percent("AGENTFS_FUSE_MEMORY_PERCENT", DEFAULT_AUTO_PERCENT),
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum FuseQueueDefault {
    Derived,
}

impl FuseQueueDefault {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Derived => "derived",
        }
    }

    fn matches(self, value: &str) -> bool {
        value.trim().eq_ignore_ascii_case(self.as_str())
    }

    fn resolve(self, workers: usize) -> usize {
        match self {
            Self::Derived => derived_queue_capacity(workers),
        }
    }
}

/// Default kernel TTLs for positive dentries and attributes. 10s lets a whole
/// git-style workload (clone ≈3s + status/diff/fsck) reuse the dentries and
/// attrs established by mutation replies instead of re-LOOKUP/GETATTR storms
/// (warm steady-state reads measured 12.7x native at the old 1s default).
/// Within one mount the kernel is coherent for its own operations regardless
/// of TTL; the TTL only bounds staleness ACROSS concurrent mounts of the same
/// session DB (`agentfs run --session` from another terminal), which now see
/// attribute/namespace changes within 10s. Override with
/// `AGENTFS_FUSE_ENTRY_TTL_MS` / `AGENTFS_FUSE_ATTR_TTL_MS`.
pub(crate) const DEFAULT_FUSE_POSITIVE_TTL_MS: u64 = 10_000;
/// Default kernel TTL for negative dentries. Kept at 1s: a file created by a
/// second mount stays invisible to this mount for the negative TTL, and
/// lookup-miss caching is the most surprising staleness to debug. Override
/// with `AGENTFS_FUSE_NEG_TTL_MS`.
pub(crate) const DEFAULT_FUSE_NEG_TTL_MS: u64 = 1000;
pub(crate) const DEFAULT_AUTO_PERCENT: u8 = 50;
pub(crate) const DEFAULT_QUEUE_MEMORY_PERCENT: u8 = 25;
pub(crate) const DEFAULT_INO_FILES_CAP: usize = 65_536;
pub(crate) const DEFAULT_URING_DEPTH: usize = 4;
pub(crate) const DEFAULT_FUSE_WORKERS: FuseWorkersDefault = FuseWorkersDefault::Auto;
pub(crate) const DEFAULT_FUSE_QUEUE: FuseQueueDefault = FuseQueueDefault::Derived;
pub(crate) const DEFAULT_FUSE_WRITEBACK: bool = true;
pub(crate) const DEFAULT_FUSE_KEEPCACHE: bool = true;
pub(crate) const DEFAULT_FUSE_SYNC_INVAL: bool = false;
pub(crate) const DEFAULT_FUSE_SELF_INVAL: bool = false;
pub(crate) const DEFAULT_DRAIN_ON_RELEASE: bool = false;
pub(crate) const DEFAULT_DRAIN_ON_FORGET: bool = false;
pub(crate) const DEFAULT_FUSE_FLUSH_INVAL: bool = false;
pub(crate) const DEFAULT_FUSE_NOFLUSH: bool = true;
pub(crate) const DEFAULT_FUSE_NOOPEN: bool = true;
pub(crate) const DEFAULT_FUSE_CACHE_DIR: bool = true;
pub(crate) const DEFAULT_FUSE_STICKY_KEEPCACHE_DROP: bool = false;
const MAX_URING_DEPTH: usize = 64;
const MAX_URING_SPIN_US: u64 = 1000;

const READDIRPLUS_MODE_OFF: u64 = 0;
const READDIRPLUS_MODE_AUTO: u64 = 1;
const READDIRPLUS_MODE_ALWAYS: u64 = 2;

/// Runtime request dispatch mode for the FUSE session.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DispatchMode {
    Serial,
    Parallel {
        workers: usize,
        queue_capacity: usize,
    },
}

impl DispatchMode {
    fn from_env() -> Self {
        let workers = match std::env::var("AGENTFS_FUSE_WORKERS") {
            Ok(value) if value.eq_ignore_ascii_case("serial") => return Self::Serial,
            Ok(value) if DEFAULT_FUSE_WORKERS.matches(&value) => DEFAULT_FUSE_WORKERS.resolve(),
            Ok(value) => parse_workers(&value).unwrap_or_else(|| {
                tracing::warn!(
                    value,
                    "invalid AGENTFS_FUSE_WORKERS; using serial FUSE dispatch"
                );
                0
            }),
            Err(VarError::NotPresent) => DEFAULT_FUSE_WORKERS.resolve(),
            Err(VarError::NotUnicode(value)) => {
                tracing::warn!(
                    ?value,
                    "invalid AGENTFS_FUSE_WORKERS; using serial FUSE dispatch"
                );
                0
            }
        };

        if workers == 0 {
            return Self::Serial;
        }

        let default_queue_capacity = default_queue_capacity(workers);
        let queue_capacity = match std::env::var("AGENTFS_FUSE_QUEUE") {
            Ok(value) if DEFAULT_FUSE_QUEUE.matches(&value) => default_queue_capacity,
            Ok(value) => parse_queue_capacity(&value, workers).unwrap_or_else(|| {
                tracing::warn!(
                    value,
                    default_queue_capacity,
                    "invalid AGENTFS_FUSE_QUEUE; using default queue capacity"
                );
                default_queue_capacity
            }),
            Err(VarError::NotPresent) => default_queue_capacity,
            Err(VarError::NotUnicode(value)) => {
                tracing::warn!(
                    ?value,
                    default_queue_capacity,
                    "invalid AGENTFS_FUSE_QUEUE; using default queue capacity"
                );
                default_queue_capacity
            }
        };

        Self::Parallel {
            workers,
            queue_capacity,
        }
    }

    pub(crate) const fn is_serial(self) -> bool {
        matches!(self, Self::Serial)
    }
}

/// Kernel readdirplus policy.
#[derive(Debug, Default, Clone, Copy, Eq, PartialEq)]
pub enum ReaddirPlusMode {
    Off,
    Auto,
    #[default]
    Always,
}

impl ReaddirPlusMode {
    pub(crate) fn profile_value(self) -> u64 {
        match self {
            ReaddirPlusMode::Off => READDIRPLUS_MODE_OFF,
            ReaddirPlusMode::Auto => READDIRPLUS_MODE_AUTO,
            ReaddirPlusMode::Always => READDIRPLUS_MODE_ALWAYS,
        }
    }
}

/// Effective kernel-cache policy after applying the dispatch safety interlock.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FuseKernelCacheConfig {
    pub(crate) entry_ttl: Duration,
    pub(crate) attr_ttl: Duration,
    pub(crate) neg_ttl: Duration,
    pub(crate) entry_ttl_ms: u64,
    pub(crate) attr_ttl_ms: u64,
    pub(crate) neg_ttl_ms: u64,
    pub(crate) writeback_cache_enabled: bool,
    pub(crate) keepcache_enabled: bool,
    pub(crate) readdirplus_mode: ReaddirPlusMode,
}

impl FuseKernelCacheConfig {
    pub(crate) fn record_profile(&self) {
        crate::telemetry::set_fuse_ttl_ms(self.entry_ttl_ms, self.attr_ttl_ms, self.neg_ttl_ms);
        crate::telemetry::set_fuse_keepcache_enabled(self.keepcache_enabled);
        crate::telemetry::set_fuse_readdirplus_mode(self.readdirplus_mode.profile_value());
    }
}

/// FUSE-over-io_uring runtime settings.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct UringConfig {
    pub(crate) enabled: bool,
    pub(crate) depth: usize,
    pub(crate) spin_us: u64,
}

impl Default for UringConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            depth: DEFAULT_URING_DEPTH,
            spin_us: 0,
        }
    }
}

impl UringConfig {
    fn from_env(reader: EnvReader) -> Self {
        let default = Self::default();
        Self {
            enabled: reader.bool("AGENTFS_FUSE_URING", default.enabled),
            depth: env_usize_in_range(
                "AGENTFS_FUSE_URING_DEPTH",
                default.depth,
                1,
                MAX_URING_DEPTH,
            ),
            spin_us: env_u64_in_range(
                "AGENTFS_FUSE_URING_SPIN_US",
                default.spin_us,
                0,
                MAX_URING_SPIN_US,
            ),
        }
    }
}

/// Parsed FUSE adapter/session configuration. Fields that need safety
/// interlocks store the requested env values; use [`FuseConfig::kernel_cache`]
/// for effective cache values.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FuseConfig {
    pub(crate) dispatch_mode: DispatchMode,
    pub(crate) entry_ttl_ms: u64,
    pub(crate) attr_ttl_ms: u64,
    pub(crate) neg_ttl_ms: u64,
    pub(crate) writeback_cache_requested: bool,
    pub(crate) keepcache_requested: bool,
    pub(crate) readdirplus_requested: ReaddirPlusMode,
    pub(crate) sync_inval: bool,
    pub(crate) self_inval: bool,
    pub(crate) drain_on_release: bool,
    pub(crate) drain_on_forget: bool,
    pub(crate) flush_inval_always: bool,
    pub(crate) noflush: bool,
    pub(crate) noopen: bool,
    pub(crate) ino_files_cap: usize,
    pub(crate) cache_dir_requested: bool,
    pub(crate) keepcache_sticky_drop: bool,
    pub(crate) uring: UringConfig,
}

impl FuseConfig {
    pub(crate) fn from_env() -> Self {
        let reader = EnvReader::new();
        let dispatch_mode = DispatchMode::from_env();
        let drain_on_release = reader.bool("AGENTFS_DRAIN_ON_RELEASE", DEFAULT_DRAIN_ON_RELEASE);
        let noflush_requested = reader.bool("AGENTFS_FUSE_NOFLUSH", DEFAULT_FUSE_NOFLUSH);
        let noflush = noflush_requested && !drain_on_release;
        if noflush_requested && !noflush {
            tracing::warn!(
                "AGENTFS_FUSE_NOFLUSH disabled: AGENTFS_DRAIN_ON_RELEASE needs the close-time FLUSH"
            );
        }
        let noopen_requested = reader.bool("AGENTFS_FUSE_NOOPEN", DEFAULT_FUSE_NOOPEN);
        let noopen = noopen_requested && !drain_on_release;
        if noopen_requested && !noopen {
            tracing::warn!(
                "AGENTFS_FUSE_NOOPEN disabled: AGENTFS_DRAIN_ON_RELEASE needs per-handle releases"
            );
        }

        let config = Self {
            dispatch_mode,
            entry_ttl_ms: env_duration_ms(
                "AGENTFS_FUSE_ENTRY_TTL_MS",
                DEFAULT_FUSE_POSITIVE_TTL_MS,
            ),
            attr_ttl_ms: env_duration_ms("AGENTFS_FUSE_ATTR_TTL_MS", DEFAULT_FUSE_POSITIVE_TTL_MS),
            neg_ttl_ms: env_duration_ms("AGENTFS_FUSE_NEG_TTL_MS", DEFAULT_FUSE_NEG_TTL_MS),
            writeback_cache_requested: reader
                .bool("AGENTFS_FUSE_WRITEBACK", DEFAULT_FUSE_WRITEBACK),
            keepcache_requested: reader.bool("AGENTFS_FUSE_KEEPCACHE", DEFAULT_FUSE_KEEPCACHE),
            readdirplus_requested: readdirplus_mode_from_env(),
            sync_inval: sync_inval_from_env(reader, dispatch_mode),
            self_inval: reader.bool("AGENTFS_FUSE_SELF_INVAL", DEFAULT_FUSE_SELF_INVAL),
            drain_on_release,
            drain_on_forget: reader.bool("AGENTFS_DRAIN_ON_FORGET", DEFAULT_DRAIN_ON_FORGET),
            flush_inval_always: reader.bool("AGENTFS_FUSE_FLUSH_INVAL", DEFAULT_FUSE_FLUSH_INVAL),
            noflush,
            noopen,
            ino_files_cap: env_usize_min("AGENTFS_FUSE_INO_FILES_CAP", DEFAULT_INO_FILES_CAP, 16),
            cache_dir_requested: reader.bool("AGENTFS_FUSE_CACHE_DIR", DEFAULT_FUSE_CACHE_DIR),
            keepcache_sticky_drop: reader.bool(
                "AGENTFS_FUSE_STICKY_KEEPCACHE_DROP",
                DEFAULT_FUSE_STICKY_KEEPCACHE_DROP,
            ),
            uring: UringConfig::from_env(reader),
        };
        emit_kernel_cache_interlock_warnings(&config);
        config
    }

    pub(crate) fn kernel_cache(&self) -> FuseKernelCacheConfig {
        cache_safety_interlock(self)
    }

    pub(crate) fn cache_dir_enabled(&self) -> bool {
        self.cache_dir_requested && self.kernel_cache().keepcache_enabled
    }
}

/// Pure cache-safety interlock: serial dispatch cannot safely use kernel
/// TTLs, writeback, keepcache, or readdirplus because invalidation progress
/// depends on a worker distinct from the request reader.
pub(crate) fn cache_safety_interlock(config: &FuseConfig) -> FuseKernelCacheConfig {
    if config.dispatch_mode.is_serial() {
        return FuseKernelCacheConfig {
            entry_ttl: Duration::ZERO,
            attr_ttl: Duration::ZERO,
            neg_ttl: Duration::ZERO,
            entry_ttl_ms: 0,
            attr_ttl_ms: 0,
            neg_ttl_ms: 0,
            writeback_cache_enabled: false,
            keepcache_enabled: false,
            readdirplus_mode: ReaddirPlusMode::Off,
        };
    }

    FuseKernelCacheConfig {
        entry_ttl: Duration::from_millis(config.entry_ttl_ms),
        attr_ttl: Duration::from_millis(config.attr_ttl_ms),
        neg_ttl: Duration::from_millis(config.neg_ttl_ms),
        entry_ttl_ms: config.entry_ttl_ms,
        attr_ttl_ms: config.attr_ttl_ms,
        neg_ttl_ms: config.neg_ttl_ms,
        writeback_cache_enabled: config.writeback_cache_requested,
        keepcache_enabled: config.keepcache_requested,
        readdirplus_mode: config.readdirplus_requested,
    }
}

fn emit_kernel_cache_interlock_warnings(config: &FuseConfig) {
    if !config.dispatch_mode.is_serial() {
        return;
    }

    if config.entry_ttl_ms != 0 || config.attr_ttl_ms != 0 || config.neg_ttl_ms != 0 {
        tracing::warn!(
            "Refusing nonzero FUSE TTLs: kernel entry/attr/negative TTLs require non-serial AGENTFS_FUSE_WORKERS"
        );
    }
    if config.writeback_cache_requested {
        tracing::warn!(
            "Refusing FUSE writeback cache: AGENTFS_FUSE_WRITEBACK requires non-serial AGENTFS_FUSE_WORKERS"
        );
    }
    if config.keepcache_requested {
        tracing::warn!(
            "Refusing FOPEN_KEEP_CACHE: AGENTFS_FUSE_KEEPCACHE requires non-serial AGENTFS_FUSE_WORKERS"
        );
    }
    if config.readdirplus_requested != ReaddirPlusMode::Off {
        tracing::warn!(
            "Refusing FUSE readdirplus: readdirplus requires non-serial AGENTFS_FUSE_WORKERS"
        );
    }
}

fn readdirplus_mode_from_env() -> ReaddirPlusMode {
    let default = ReaddirPlusMode::default();
    match std::env::var("AGENTFS_FUSE_READDIRPLUS") {
        Ok(value)
            if value.eq_ignore_ascii_case("off")
                || value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no")
                || value == "0" =>
        {
            ReaddirPlusMode::Off
        }
        Ok(value) if value.eq_ignore_ascii_case("auto") => ReaddirPlusMode::Auto,
        Ok(value)
            if value.eq_ignore_ascii_case("always")
                || value.eq_ignore_ascii_case("on")
                || value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value == "1" =>
        {
            ReaddirPlusMode::Always
        }
        Ok(value) => {
            tracing::warn!(
                "Ignoring invalid AGENTFS_FUSE_READDIRPLUS={:?}; using default {:?}",
                value,
                default
            );
            default
        }
        Err(VarError::NotPresent) => default,
        Err(VarError::NotUnicode(value)) => {
            tracing::warn!(
                ?value,
                "Ignoring non-Unicode AGENTFS_FUSE_READDIRPLUS; using default"
            );
            default
        }
    }
}

fn sync_inval_from_env(reader: EnvReader, dispatch_mode: DispatchMode) -> bool {
    let sync_requested = reader.bool("AGENTFS_FUSE_SYNC_INVAL", DEFAULT_FUSE_SYNC_INVAL);
    if dispatch_mode.is_serial() && sync_requested {
        tracing::info!(
            "AGENTFS_FUSE_SYNC_INVAL requested with AGENTFS_FUSE_WORKERS=serial; using deferred invalidation to avoid notify/reply deadlock"
        );
        false
    } else {
        sync_requested
    }
}

fn env_duration_ms(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => match value.trim().parse::<u64>() {
            Ok(ms) => ms,
            Err(_) => {
                tracing::warn!(
                    "Ignoring invalid {}={:?} for FUSE TTL; using {}ms",
                    name,
                    value,
                    default
                );
                default
            }
        },
        Err(VarError::NotPresent) => default,
        Err(VarError::NotUnicode(value)) => {
            tracing::warn!(
                "Ignoring non-Unicode {}={:?} for FUSE TTL; using {}ms",
                name,
                value,
                default
            );
            default
        }
    }
}

fn env_usize_min(name: &str, default: usize, min: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => match value.trim().parse::<usize>().ok().filter(|v| *v >= min) {
            Some(parsed) => parsed,
            None => {
                tracing::warn!(
                    name,
                    value,
                    default,
                    min,
                    "invalid FUSE usize config; using default"
                );
                default
            }
        },
        Err(VarError::NotPresent) => default,
        Err(VarError::NotUnicode(value)) => {
            tracing::warn!(
                name,
                ?value,
                default,
                min,
                "non-Unicode FUSE usize config; using default"
            );
            default
        }
    }
}

fn env_usize_in_range(name: &str, default: usize, min: usize, max: usize) -> usize {
    match std::env::var(name) {
        Ok(value) => match value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|v| (min..=max).contains(v))
        {
            Some(parsed) => parsed,
            None => {
                tracing::warn!(
                    name,
                    value,
                    default,
                    min,
                    max,
                    "invalid FUSE usize config; using default"
                );
                default
            }
        },
        Err(VarError::NotPresent) => default,
        Err(VarError::NotUnicode(value)) => {
            tracing::warn!(
                name,
                ?value,
                default,
                min,
                max,
                "non-Unicode FUSE usize config; using default"
            );
            default
        }
    }
}

fn env_u64_in_range(name: &str, default: u64, min: u64, max: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => match value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|v| (min..=max).contains(v))
        {
            Some(parsed) => parsed,
            None => {
                tracing::warn!(
                    name,
                    value,
                    default,
                    min,
                    max,
                    "invalid FUSE u64 config; using default"
                );
                default
            }
        },
        Err(VarError::NotPresent) => default,
        Err(VarError::NotUnicode(value)) => {
            tracing::warn!(
                name,
                ?value,
                default,
                min,
                max,
                "non-Unicode FUSE u64 config; using default"
            );
            default
        }
    }
}

fn parse_workers(value: &str) -> Option<usize> {
    let value = value.trim();
    if let Some(percent) = parse_percent_suffix(value) {
        return Some(workers_from_resource_percent(
            percent,
            env_percent("AGENTFS_FUSE_MEMORY_PERCENT", percent),
        ));
    }
    value.parse::<usize>().ok().filter(|workers| *workers > 0)
}

fn parse_queue_capacity(value: &str, workers: usize) -> Option<usize> {
    let value = value.trim();
    if let Some(percent) = parse_percent_suffix(value) {
        return Some(queue_capacity_for_memory_percent(workers, percent));
    }
    value.parse::<usize>().ok().filter(|queue| *queue > 0)
}

fn parse_percent_suffix(value: &str) -> Option<u8> {
    let percent = value.strip_suffix('%')?.trim().parse::<u8>().ok()?;
    (1..=100).contains(&percent).then_some(percent)
}

fn parse_percent(value: &str) -> Option<u8> {
    parse_percent_suffix(value.trim()).or_else(|| {
        value
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|v| (1..=100).contains(v))
    })
}

fn env_percent(name: &str, default: u8) -> u8 {
    match std::env::var(name) {
        Ok(value) => parse_percent(&value).unwrap_or_else(|| {
            tracing::warn!(
                name,
                value,
                default,
                "invalid percent environment variable; using default"
            );
            default
        }),
        Err(VarError::NotPresent) => default,
        Err(VarError::NotUnicode(value)) => {
            tracing::warn!(
                name,
                ?value,
                default,
                "non-Unicode percent environment variable; using default"
            );
            default
        }
    }
}

fn workers_from_resource_percent(cpu_percent: u8, memory_percent: u8) -> usize {
    let cpu_workers = std::thread::available_parallelism()
        .map(|parallelism| percent_of_count(parallelism.get(), cpu_percent))
        .unwrap_or(1);
    let memory_workers = available_memory_bytes()
        .map(|bytes| {
            let budget = percent_of_bytes(bytes, memory_percent);
            (budget / FUSE_REQUEST_BUFFER_SIZE as u64).max(1) as usize
        })
        .unwrap_or(cpu_workers);
    cpu_workers.min(memory_workers).max(1)
}

fn default_queue_capacity(workers: usize) -> usize {
    DEFAULT_FUSE_QUEUE.resolve(workers)
}

fn derived_queue_capacity(workers: usize) -> usize {
    let memory_percent = env_percent(
        "AGENTFS_FUSE_QUEUE_MEMORY_PERCENT",
        DEFAULT_QUEUE_MEMORY_PERCENT,
    );
    workers
        .saturating_mul(4)
        .max(1)
        .min(queue_capacity_for_memory_percent(workers, memory_percent))
}

fn queue_capacity_for_memory_percent(workers: usize, percent: u8) -> usize {
    let Some(bytes) = available_memory_bytes() else {
        return workers.saturating_mul(4).max(1);
    };
    let budget = percent_of_bytes(bytes, percent);
    let worker_bytes = workers.saturating_mul(FUSE_REQUEST_BUFFER_SIZE) as u64;
    let queue_budget = budget.saturating_sub(worker_bytes);
    (queue_budget / FUSE_REQUEST_BUFFER_SIZE as u64).max(1) as usize
}

fn percent_of_count(count: usize, percent: u8) -> usize {
    ((count as u64 * percent as u64) / 100).max(1) as usize
}

fn percent_of_bytes(bytes: u64, percent: u8) -> u64 {
    bytes.saturating_mul(percent as u64) / 100
}

fn available_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in meminfo.lines() {
            let Some(rest) = line.strip_prefix("MemAvailable:") else {
                continue;
            };
            let kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return kib.checked_mul(1024);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const FUSE_ENV_KEYS: &[&str] = &[
        "AGENTFS_FUSE_WORKERS",
        "AGENTFS_FUSE_CPU_PERCENT",
        "AGENTFS_FUSE_MEMORY_PERCENT",
        "AGENTFS_FUSE_QUEUE",
        "AGENTFS_FUSE_QUEUE_MEMORY_PERCENT",
        "AGENTFS_FUSE_ENTRY_TTL_MS",
        "AGENTFS_FUSE_ATTR_TTL_MS",
        "AGENTFS_FUSE_NEG_TTL_MS",
        "AGENTFS_FUSE_WRITEBACK",
        "AGENTFS_FUSE_KEEPCACHE",
        "AGENTFS_FUSE_READDIRPLUS",
        "AGENTFS_FUSE_SYNC_INVAL",
        "AGENTFS_FUSE_SELF_INVAL",
        "AGENTFS_DRAIN_ON_RELEASE",
        "AGENTFS_DRAIN_ON_FORGET",
        "AGENTFS_FUSE_FLUSH_INVAL",
        "AGENTFS_FUSE_NOFLUSH",
        "AGENTFS_FUSE_NOOPEN",
        "AGENTFS_FUSE_INO_FILES_CAP",
        "AGENTFS_FUSE_CACHE_DIR",
        "AGENTFS_FUSE_STICKY_KEEPCACHE_DROP",
        "AGENTFS_FUSE_URING",
        "AGENTFS_FUSE_URING_DEPTH",
        "AGENTFS_FUSE_URING_SPIN_US",
    ];

    struct EnvSnapshot {
        values: Vec<(&'static str, Option<String>)>,
    }

    impl EnvSnapshot {
        fn capture(keys: &[&'static str]) -> Self {
            let values = keys
                .iter()
                .map(|key| (*key, std::env::var(key).ok()))
                .collect();
            for key in keys {
                std::env::remove_var(key);
            }
            Self { values }
        }
    }

    impl Drop for EnvSnapshot {
        fn drop(&mut self) {
            for (key, value) in &self.values {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn default_test_config(dispatch_mode: DispatchMode) -> FuseConfig {
        FuseConfig {
            dispatch_mode,
            entry_ttl_ms: DEFAULT_FUSE_POSITIVE_TTL_MS,
            attr_ttl_ms: DEFAULT_FUSE_POSITIVE_TTL_MS,
            neg_ttl_ms: DEFAULT_FUSE_NEG_TTL_MS,
            writeback_cache_requested: true,
            keepcache_requested: true,
            readdirplus_requested: ReaddirPlusMode::Always,
            sync_inval: false,
            self_inval: false,
            drain_on_release: false,
            drain_on_forget: false,
            flush_inval_always: false,
            noflush: true,
            noopen: true,
            ino_files_cap: DEFAULT_INO_FILES_CAP,
            cache_dir_requested: true,
            keepcache_sticky_drop: false,
            uring: UringConfig::default(),
        }
    }

    #[test]
    fn documented_default_tokens_feed_runtime_parsers() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(FUSE_ENV_KEYS);

        let unset_config = FuseConfig::from_env();
        let unset_workers = match unset_config.dispatch_mode {
            DispatchMode::Parallel { workers, .. } => workers,
            DispatchMode::Serial => 0,
        };

        std::env::set_var("AGENTFS_FUSE_WORKERS", DEFAULT_FUSE_WORKERS.as_str());
        let explicit_config = FuseConfig::from_env();
        let explicit_workers = match explicit_config.dispatch_mode {
            DispatchMode::Parallel { workers, .. } => workers,
            DispatchMode::Serial => 0,
        };
        assert_eq!(
            explicit_workers, unset_workers,
            "documented worker default must use the same parser path as an unset env var"
        );

        std::env::set_var("AGENTFS_FUSE_WORKERS", "2");
        std::env::remove_var("AGENTFS_FUSE_QUEUE");
        let unset_queue_config = FuseConfig::from_env();

        std::env::set_var("AGENTFS_FUSE_QUEUE", DEFAULT_FUSE_QUEUE.as_str());
        let explicit_queue_config = FuseConfig::from_env();

        assert_eq!(
            explicit_queue_config.dispatch_mode, unset_queue_config.dispatch_mode,
            "documented queue default must use the same parser path as an unset env var"
        );
    }

    #[test]
    fn invalid_readdirplus_warns_and_defaults_on() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(FUSE_ENV_KEYS);
        std::env::set_var("AGENTFS_FUSE_READDIRPLUS", "bogus");

        let config = FuseConfig::from_env();
        let cache = config.kernel_cache();

        println!(
            "invalid AGENTFS_FUSE_READDIRPLUS kept default {:?}",
            cache.readdirplus_mode
        );
        assert_eq!(cache.readdirplus_mode, ReaddirPlusMode::Always);
    }

    #[test]
    fn workers_config_feeds_dispatch_and_cache_policy() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(FUSE_ENV_KEYS);
        std::env::set_var("AGENTFS_FUSE_WORKERS", "25%");

        let config = FuseConfig::from_env();
        let cache = config.kernel_cache();

        let DispatchMode::Parallel { workers, .. } = config.dispatch_mode else {
            panic!("25% workers should resolve to parallel dispatch");
        };
        println!(
            "parsed DispatchMode::Parallel workers={workers}; cache writeback={}",
            cache.writeback_cache_enabled
        );
        assert!(workers > 0);
        assert!(cache.writeback_cache_enabled);
        assert!(cache.keepcache_enabled);
        assert_ne!(cache.readdirplus_mode, ReaddirPlusMode::Off);
    }

    #[test]
    fn serial_dispatch_disables_kernel_cache_policy() {
        let config = default_test_config(DispatchMode::Serial);
        let cache = cache_safety_interlock(&config);

        assert_eq!(cache.entry_ttl_ms, 0);
        assert_eq!(cache.attr_ttl_ms, 0);
        assert_eq!(cache.neg_ttl_ms, 0);
        assert!(!cache.writeback_cache_enabled);
        assert!(!cache.keepcache_enabled);
        assert_eq!(cache.readdirplus_mode, ReaddirPlusMode::Off);
    }

    #[test]
    fn drain_on_release_disables_noopen_and_noflush() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(FUSE_ENV_KEYS);
        std::env::set_var("AGENTFS_DRAIN_ON_RELEASE", "1");

        let config = FuseConfig::from_env();

        assert!(config.drain_on_release);
        assert!(!config.noopen);
        assert!(!config.noflush);
    }

    #[test]
    fn combined_kill_switches_are_honored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(FUSE_ENV_KEYS);
        std::env::set_var("AGENTFS_FUSE_NOOPEN", "0");
        std::env::set_var("AGENTFS_FUSE_NOFLUSH", "0");
        std::env::set_var("AGENTFS_FUSE_URING", "0");

        let config = FuseConfig::from_env();

        assert!(!config.noopen);
        assert!(!config.noflush);
        assert!(!config.uring.enabled);
    }
}
