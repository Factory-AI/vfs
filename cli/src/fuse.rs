use crate::fuser::{
    consts::{
        FOPEN_KEEP_CACHE, FUSE_ASYNC_READ, FUSE_CACHE_SYMLINKS, FUSE_DO_READDIRPLUS,
        FUSE_NO_OPENDIR_SUPPORT, FUSE_PARALLEL_DIROPS, FUSE_READDIRPLUS_AUTO, FUSE_WRITEBACK_CACHE,
    },
    fuse_forget_one, FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request,
};
use agentfs_sdk::error::Error as SdkError;
use agentfs_sdk::filesystem::{
    WriteRange, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFSOCK,
};
use agentfs_sdk::{BoxedFile, DirEntry, FileSystem, FsError, Stats, TimeChange};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    ffi::OsStr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;
use tracing;

/// Convert an SDK error to an errno code for FUSE replies.
///
/// If the error is a filesystem-specific FsError, returns the appropriate
/// errno code (ENOENT, EEXIST, ENOTDIR, etc.). Database busy errors and
/// connection pool timeouts return EAGAIN to signal the caller should retry.
/// Otherwise falls back to EIO.
fn error_to_errno(e: &SdkError) -> i32 {
    match e {
        SdkError::Fs(fs_err) => fs_err.to_errno(),
        SdkError::Io(io_err) => io_err.raw_os_error().unwrap_or(libc::EIO),
        SdkError::Database(turso::Error::Busy(_)) => libc::EAGAIN,
        SdkError::ConnectionPoolTimeout => libc::EAGAIN,
        _ => libc::EIO,
    }
}

/// Maximize the file descriptor limit by raising the soft limit to the hard limit.
///
/// This helps avoid "too many open files" errors when passthrough filesystems
/// cache O_PATH file descriptors for inode handles. Unlike raising the hard limit,
/// this does not require root privileges.
fn maximize_fd_limit() {
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) };
    if result == 0 {
        let old_soft = lim.rlim_cur;
        lim.rlim_cur = lim.rlim_max;
        let result = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) };
        if result == 0 {
            tracing::debug!("Raised fd limit from {} to {}", old_soft, lim.rlim_max);
        } else {
            tracing::warn!(
                "Failed to raise fd limit: {}",
                std::io::Error::last_os_error()
            );
        }
    } else {
        tracing::warn!(
            "Failed to get fd limit: {}",
            std::io::Error::last_os_error()
        );
    }
}

const DEFAULT_FUSE_TTL_MS: u64 = 1000;
const READDIRPLUS_MODE_OFF: u64 = 0;
const READDIRPLUS_MODE_AUTO: u64 = 1;
const READDIRPLUS_MODE_ALWAYS: u64 = 2;

/// FUSE kernel cache policy derived once per mount from environment knobs.
#[derive(Debug, Clone)]
struct FuseKernelCacheConfig {
    entry_ttl: Duration,
    attr_ttl: Duration,
    neg_ttl: Duration,
    entry_ttl_ms: u64,
    attr_ttl_ms: u64,
    neg_ttl_ms: u64,
    writeback_cache_enabled: bool,
    keepcache_enabled: bool,
    readdirplus_mode: ReaddirPlusMode,
}

impl FuseKernelCacheConfig {
    fn from_env() -> Self {
        let entry_ttl_ms = env_duration_ms("AGENTFS_FUSE_ENTRY_TTL_MS", DEFAULT_FUSE_TTL_MS);
        let attr_ttl_ms = env_duration_ms("AGENTFS_FUSE_ATTR_TTL_MS", DEFAULT_FUSE_TTL_MS);
        let neg_ttl_ms = env_duration_ms("AGENTFS_FUSE_NEG_TTL_MS", DEFAULT_FUSE_TTL_MS);

        // Kernel cache safety requires non-serial workers: we need a worker thread
        // distinct from the session loop to send FUSE_NOTIFY_INVAL_* without
        // blocking the request reader. Serial mode keeps reply+notify on the same
        // thread which deadlocks per cli/src/fuser/deferred_notify.rs.
        //
        // Whether AGENTFS_FUSE_SYNC_INVAL is on does NOT affect safety here:
        // - On (sync): worker writev's notify directly. Risk: kernel may block
        //   the worker's writev waiting for an inline FUSE_FORGET that the
        //   session thread cannot deliver if its lane queue is full. This
        //   reproduces under git clone on Linux 6+ kernels.
        // - Off (deferred, the default): notify is enqueued to the dedicated
        //   notify thread that owns its own writev fd. The notify thread is
        //   never blocked by the dispatch path, so the kernel-side FORGET
        //   round-trip drains independently. Cache coherency is bounded by
        //   the few-microsecond latency between mutation reply and notify
        //   delivery, which is well within the entry/attr TTL window.
        //
        // So: safe_kernel_cache only requires non-serial workers, and the
        // sync_invalidation env var is treated as an unsafe opt-in.
        let workers_not_serial = fuse_workers_not_serial_from_env();
        let safe_kernel_cache = workers_not_serial;
        let (entry_ttl_ms, attr_ttl_ms, neg_ttl_ms) = if safe_kernel_cache {
            (entry_ttl_ms, attr_ttl_ms, neg_ttl_ms)
        } else {
            if entry_ttl_ms != 0 || attr_ttl_ms != 0 || neg_ttl_ms != 0 {
                tracing::warn!(
                    "Refusing nonzero FUSE TTLs: kernel entry/attr/negative TTLs require non-serial AGENTFS_FUSE_WORKERS"
                );
            }
            (0, 0, 0)
        };

        let writeback_requested = env_flag_default("AGENTFS_FUSE_WRITEBACK", true);
        let writeback_cache_enabled = writeback_requested && safe_kernel_cache;
        if writeback_requested && !writeback_cache_enabled {
            tracing::warn!(
                "Refusing FUSE writeback cache: AGENTFS_FUSE_WRITEBACK requires non-serial AGENTFS_FUSE_WORKERS"
            );
        }

        let keepcache_requested = env_flag_default("AGENTFS_FUSE_KEEPCACHE", true);
        let keepcache_enabled = keepcache_requested && safe_kernel_cache;
        if keepcache_requested && !keepcache_enabled {
            tracing::warn!(
                "Refusing FOPEN_KEEP_CACHE: AGENTFS_FUSE_KEEPCACHE requires non-serial AGENTFS_FUSE_WORKERS"
            );
        }
        let readdirplus_mode = if safe_kernel_cache {
            readdirplus_mode_from_env()
        } else {
            tracing::warn!(
                "Refusing FUSE readdirplus: readdirplus requires non-serial AGENTFS_FUSE_WORKERS"
            );
            ReaddirPlusMode::Off
        };

        Self {
            entry_ttl: Duration::from_millis(entry_ttl_ms),
            attr_ttl: Duration::from_millis(attr_ttl_ms),
            neg_ttl: Duration::from_millis(neg_ttl_ms),
            entry_ttl_ms,
            attr_ttl_ms,
            neg_ttl_ms,
            writeback_cache_enabled,
            keepcache_enabled,
            readdirplus_mode,
        }
    }

    fn record_profile(&self) {
        agentfs_sdk::profiling::set_fuse_ttl_ms(
            self.entry_ttl_ms,
            self.attr_ttl_ms,
            self.neg_ttl_ms,
        );
        agentfs_sdk::profiling::set_fuse_keepcache_enabled(self.keepcache_enabled);
        agentfs_sdk::profiling::set_fuse_readdirplus_mode(self.readdirplus_mode.profile_value());
    }
}

#[derive(Debug, Default)]
struct KeepCacheDriftGuard {
    eligible: HashSet<u64>,
    dropped: HashSet<u64>,
    fingerprints: HashMap<u64, KeepCacheFingerprint>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct KeepCacheFingerprint {
    mode: u32,
    size: i64,
    mtime: i64,
    mtime_nsec: u32,
    ctime: i64,
    ctime_nsec: u32,
    rdev: u64,
}

impl KeepCacheFingerprint {
    fn from_stats(stats: &Stats) -> Self {
        Self {
            mode: stats.mode,
            size: stats.size,
            mtime: stats.mtime,
            mtime_nsec: stats.mtime_nsec,
            ctime: stats.ctime,
            ctime_nsec: stats.ctime_nsec,
            rdev: stats.rdev,
        }
    }
}

impl KeepCacheDriftGuard {
    fn allows(&self, ino: u64, fingerprint: &KeepCacheFingerprint) -> bool {
        !self.dropped.contains(&ino)
            && self
                .fingerprints
                .get(&ino)
                .map(|existing| existing == fingerprint)
                .unwrap_or(true)
    }

    fn mark_eligible(&mut self, ino: u64, fingerprint: KeepCacheFingerprint) {
        if !self.dropped.contains(&ino) {
            self.eligible.insert(ino);
            self.fingerprints.insert(ino, fingerprint);
        }
    }

    fn drop_eligibility(&mut self, ino: u64) -> bool {
        let was_eligible = self.eligible.remove(&ino);
        self.fingerprints.remove(&ino);
        let newly_dropped = self.dropped.insert(ino);
        was_eligible || newly_dropped
    }
}

/// Kernel readdirplus policy.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ReaddirPlusMode {
    Off,
    Auto,
    Always,
}

/// Options for mounting an agent filesystem via FUSE.
#[derive(Debug, Clone)]
pub struct FuseMountOptions {
    /// The mountpoint path.
    pub mountpoint: PathBuf,
    /// Automatically unmount when the process exits.
    pub auto_unmount: bool,
    /// Allow root to access the mount.
    pub allow_root: bool,
    /// Allow other system users to access the mount.
    /// Requires 'user_allow_other' in /etc/fuse.conf for non-root users.
    pub allow_other: bool,
    /// Filesystem name shown in mount output.
    pub fsname: String,
    /// User ID to report for all files (defaults to current user).
    pub uid: Option<u32>,
    /// Group ID to report for all files (defaults to current group).
    pub gid: Option<u32>,
}

/// Threshold at which the FUSE-layer per-fh write coalescer flushes its
/// accumulated ranges down to the SDK. Picked at 4x the chunk size so a single
/// flushed call covers a few SQLite chunks and the AsyncMutex acquisition in
/// the SDK write batcher is amortised across many FUSE_WRITE requests for the
/// same handle. Smaller writes (the common git-clone case) accumulate in this
/// buffer until `flush` / `release` arrives and only then hit the SDK.
const FUSE_COALESCE_FLUSH_BYTES: usize = 256 * 1024;

/// Tracks an open file handle
struct OpenFile {
    /// Inode associated with this FUSE file handle.
    ino: u64,
    /// The file handle from the filesystem layer.
    file: BoxedFile,
    /// Pending writes buffered for coalescing before reaching the filesystem layer.
    pending: WriteBuffer,
}

impl OpenFile {
    fn new(ino: u64, file: BoxedFile) -> Self {
        Self {
            ino,
            file,
            pending: WriteBuffer::default(),
        }
    }

    #[cfg(test)]
    fn buffer_write(&mut self, offset: u64, data: &[u8]) -> Result<(), i32> {
        self.pending.write(offset, data)?;
        Ok(())
    }

    /// Coalesce a single FUSE write into the per-fh pending buffer. Returns
    /// `true` if the cumulative buffer size has reached the flush threshold
    /// and the caller should drain it before replying to the kernel.
    fn buffer_fuse_write(&mut self, offset: u64, data: &[u8]) -> Result<bool, i32> {
        self.pending.write(offset, data)?;
        Ok(self.pending.bytes >= FUSE_COALESCE_FLUSH_BYTES)
    }

    /// Drain the per-fh pending buffer into a `(file, ranges, range_count,
    /// byte_count)` tuple so the caller can release the surrounding
    /// `open_files` lock before issuing the async `pwrite_ranges*` call. The
    /// hot write path MUST NOT hold the parking_lot `open_files` mutex across
    /// `runtime.block_on(...)`: doing so serializes every other FUSE handler
    /// behind one fh's SQLite commit and was the source of a 2x checkout
    /// regression observed in the first Tier Two benchmark pass.
    fn take_pending(&mut self) -> Option<(BoxedFile, Vec<WriteRange>, u64, u64)> {
        if self.pending.is_empty() {
            return None;
        }
        let file = self.file.clone();
        let ranges = self.pending.ranges_for_flush();
        let range_count = ranges.len() as u64;
        let byte_count = ranges
            .iter()
            .map(|range| range.data.len() as u64)
            .sum::<u64>();
        self.pending.clear();
        Some((file, ranges, range_count, byte_count))
    }

    /// Synchronous flush via the non-batched pwrite API. Production code uses
    /// `take_pending` + `flush_pending_batched_out_of_lock` instead; this
    /// remains as a test-only convenience so the OpenFile unit tests stay
    /// readable.
    #[cfg(test)]
    fn flush_pending(&mut self, runtime: &Runtime) -> Result<(), SdkError> {
        let Some((file, ranges, range_count, byte_count)) = self.take_pending() else {
            return Ok(());
        };

        runtime.block_on(async move { file.pwrite_ranges(ranges).await })?;
        agentfs_sdk::profiling::record_fuse_flush(range_count, byte_count);
        Ok(())
    }
}

/// Flush a `(file, ranges, range_count, byte_count)` tuple produced by
/// `OpenFile::take_pending()` via the SDK write batcher (so the coalesced
/// ranges enter the cross-inode batched-commit path). Called by the FUSE
/// write / flush / release handlers AFTER they have released the
/// `open_files` parking_lot mutex.
fn flush_pending_batched_out_of_lock(
    runtime: &Runtime,
    drain: (BoxedFile, Vec<WriteRange>, u64, u64),
) -> Result<(), SdkError> {
    let (file, ranges, range_count, byte_count) = drain;
    runtime.block_on(async move { file.pwrite_ranges_batched(ranges).await })?;
    agentfs_sdk::profiling::record_fuse_flush(range_count, byte_count);
    Ok(())
}

/// Pending write ranges for one open FUSE file handle.
///
/// Ranges are keyed by start offset and kept non-overlapping. Adjacent and
/// overlapping writes are merged eagerly so common sequential writes become one
/// filesystem-layer `pwrite` when the handle is flushed.
#[derive(Default)]
struct WriteBuffer {
    ranges: BTreeMap<u64, Vec<u8>>,
    bytes: usize,
}

impl WriteBuffer {
    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }

    fn clear(&mut self) {
        self.ranges.clear();
        self.bytes = 0;
    }

    fn ranges_for_flush(&self) -> Vec<WriteRange> {
        self.ranges
            .iter()
            .map(|(&offset, data)| WriteRange {
                offset,
                data: data.clone(),
            })
            .collect()
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), i32> {
        if data.is_empty() {
            return Ok(());
        }

        let data_len = u64::try_from(data.len()).map_err(|_| libc::EINVAL)?;
        let write_start = offset;
        let write_end = offset.checked_add(data_len).ok_or(libc::EINVAL)?;
        let mut start = write_start;
        let mut end = write_end;
        let mut existing_ranges = Vec::new();

        if let Some((&prev_start, prev_data)) = self.ranges.range(..=write_start).next_back() {
            let prev_end = prev_start
                .checked_add(prev_data.len() as u64)
                .ok_or(libc::EINVAL)?;

            if prev_end >= write_start {
                let prev_data = prev_data.clone();
                self.ranges.remove(&prev_start);
                self.bytes -= prev_data.len();

                start = prev_start;
                end = end.max(prev_end);
                existing_ranges.push((prev_start, prev_data));
            }
        }

        loop {
            let next = self
                .ranges
                .range(start..)
                .next()
                .map(|(&next_start, next_data)| (next_start, next_data.clone()));

            let Some((next_start, next_data)) = next else {
                break;
            };

            if next_start > end {
                break;
            }

            let next_end = next_start
                .checked_add(next_data.len() as u64)
                .ok_or(libc::EINVAL)?;
            self.ranges.remove(&next_start);
            self.bytes -= next_data.len();

            end = end.max(next_end);
            existing_ranges.push((next_start, next_data));
        }

        let mut merged = vec![0; (end - start) as usize];
        for (range_start, range_data) in existing_ranges {
            let range_offset = (range_start - start) as usize;
            merged[range_offset..range_offset + range_data.len()].copy_from_slice(&range_data);
        }

        let write_offset = (write_start - start) as usize;
        merged[write_offset..write_offset + data.len()].copy_from_slice(data);

        self.bytes += merged.len();
        self.ranges.insert(start, merged);
        Ok(())
    }
}

struct CachedDirEntry {
    name: String,
    attr: FileAttr,
}

#[cfg(debug_assertions)]
thread_local! {
    static MUTATION_INVALIDATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Records that an invalidation flowed through this thread for the active
/// mutation. Zero overhead in release builds.
#[inline(always)]
fn record_mutation_invalidation() {
    #[cfg(debug_assertions)]
    MUTATION_INVALIDATIONS.with(|c| c.set(c.get().saturating_add(1)));
}

/// RAII audit guard: captures the per-thread invalidation count at the start of
/// a mutation. Calling [`MutationAudit::assert_invalidated`] on the success path
/// asserts (debug builds only) that at least one kernel-cache invalidation was
/// recorded between construction and the assertion. Compiles to a ZST with
/// no instructions in release builds.
struct MutationAudit {
    #[cfg(debug_assertions)]
    start: u64,
}

impl MutationAudit {
    #[inline(always)]
    fn new() -> Self {
        Self {
            #[cfg(debug_assertions)]
            start: MUTATION_INVALIDATIONS.with(|c| c.get()),
        }
    }

    /// Asserts that the success branch of a mutation called
    /// `invalidate_inode_cache` or `invalidate_entry_cache` at least once.
    /// No-op in release; intentionally takes `self` so the audit can only be
    /// asserted once per mutation.
    #[inline(always)]
    fn assert_invalidated(self, _op: &'static str) {
        #[cfg(debug_assertions)]
        {
            let end = MUTATION_INVALIDATIONS.with(|c| c.get());
            debug_assert!(
                end > self.start,
                "FUSE mutation `{}` must call invalidate_inode_cache or invalidate_entry_cache before replying with success",
                _op
            );
        }
    }
}

struct AgentFSFuse {
    fs: Arc<dyn FileSystem>,
    runtime: Runtime,
    /// Env-backed kernel cache safety configuration for this mount.
    cache_config: FuseKernelCacheConfig,
    /// Maps file handle -> open file state
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    /// Caches fully materialized directory entries across FUSE readdir offset calls.
    dir_entries_cache: Arc<Mutex<HashMap<u64, Arc<Vec<CachedDirEntry>>>>>,
    /// Caches attributes discovered by lookup/readdir_plus for read-heavy traversals.
    attr_cache: Arc<Mutex<HashMap<u64, Stats>>>,
    /// Caches positive parent/name lookups discovered by lookup/readdir_plus.
    entry_cache: Arc<Mutex<HashMap<(u64, String), Stats>>>,
    /// Caches negative parent/name lookups; exact namespace mutations remove or update keys.
    negative_entry_cache: Arc<Mutex<HashMap<(u64, String), ()>>>,
    /// Drops FOPEN_KEEP_CACHE eligibility after mutations that can stale kernel pages.
    keepcache_drift_guard: Arc<Mutex<KeepCacheDriftGuard>>,
    /// Serializes cacheable FUSE replies against mutation invalidations.
    cache_reply_lock: Arc<Mutex<()>>,
    /// Monotonic epoch bumped whenever a mutation invalidates cached namespace or attrs.
    cache_epoch: AtomicU64,
    /// Next file handle to allocate
    next_fh: AtomicU64,
    /// Whether kernel cache invalidations are sent synchronously before replies.
    sync_inval: bool,
    /// Emits a profiling summary when the FUSE session object is dropped.
    _profile_report: Arc<agentfs_sdk::profiling::ProfileReportGuard>,
    /// Whether FUSE writeback mode is enabled for this mount.
    writeback_enabled: bool,
}

impl Filesystem for AgentFSFuse {
    /// Initialize the filesystem and enable performance optimizations.
    ///
    /// - Async read: allows the kernel to issue multiple read requests in parallel,
    ///   improving throughput for concurrent file access.
    /// - Writeback caching is enabled only when the Phase 8 safety interlocks
    ///   indicate non-serial workers and synchronous invalidation; batched writes
    ///   drain on flush/fsync/release before durability replies.
    /// - Parallel dirops: allows concurrent lookup() and readdir() on the same
    ///   directory, improving performance for parallel file access patterns.
    /// - Cache symlinks: caches readlink responses, avoiding repeated round-trips
    ///   for symlink resolution.
    /// - No opendir support: skips opendir/releasedir calls since we don't track
    ///   directory handles, reducing round-trips for directory operations.
    fn init(&self, _req: &Request, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        tracing::debug!("FUSE::init");
        self.cache_config.record_profile();
        let _ = config.add_capabilities(
            FUSE_ASYNC_READ | FUSE_PARALLEL_DIROPS | FUSE_CACHE_SYMLINKS | FUSE_NO_OPENDIR_SUPPORT,
        );
        configure_writeback_cache(config, self.cache_config.writeback_cache_enabled);
        configure_readdirplus(config, self.cache_config.readdirplus_mode);
        Ok(())
    }

    fn destroy(&self) {
        tracing::debug!("FUSE::destroy");
        if let Err(e) = self.flush_all_pending() {
            tracing::warn!("FUSE::destroy failed to flush pending writes: {}", e);
        }
        if let Err(e) = self.finalize_filesystem() {
            tracing::warn!("FUSE::destroy failed to finalize filesystem: {}", e);
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Name Resolution & Attributes
    // ─────────────────────────────────────────────────────────────

    /// Looks up a directory entry by name within a parent directory.
    ///
    /// Resolves `name` under the directory identified by `parent` inode.
    fn lookup(&self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        agentfs_sdk::profiling::record_fuse_lookup();
        tracing::debug!("FUSE::lookup: parent={}, name={:?}", parent, name);

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let cache_epoch = self.cache_epoch();
        if let Some(stats) = self
            .entry_cache
            .lock()
            .get(&(parent, name_str.to_string()))
            .cloned()
        {
            let fs = self.fs.clone();
            let retained = self
                .runtime
                .block_on(async move { fs.retain_lookup(stats.ino, 1).await })
                .is_ok();
            let cache_reply = self.cache_reply_lock.try_lock();
            if retained && cache_reply.is_some() && !self.cache_epoch_changed(cache_epoch) {
                let attr = fillattr(&stats);
                reply.entry_with_ttls(
                    &self.cache_config.entry_ttl,
                    &self.cache_config.attr_ttl,
                    &attr,
                    0,
                );
                return;
            }
            if retained {
                let fs = self.fs.clone();
                let ino = stats.ino;
                self.runtime
                    .block_on(async move { fs.forget(ino, 1).await });
            }
        }

        let cache_epoch = self.cache_epoch();
        if self
            .negative_entry_cache
            .lock()
            .contains_key(&(parent, name_str.to_string()))
        {
            let cache_reply = self.cache_reply_lock.try_lock();
            if cache_reply.is_some() && !self.cache_epoch_changed(cache_epoch) {
                agentfs_sdk::profiling::record_negative_cache_hit();
                self.reply_negative_entry(reply);
                return;
            }
        }
        agentfs_sdk::profiling::record_negative_cache_miss();

        let mut stable = false;
        let mut stable_epoch = 0;
        let mut result = None;
        for _ in 0..2 {
            let epoch = self.cache_epoch();
            let fs = self.fs.clone();
            let name_owned = name_str.to_string();
            let lookup_result = self
                .runtime
                .block_on(async move { fs.lookup(parent as i64, &name_owned).await });
            stable = !self.cache_epoch_changed(epoch);
            stable_epoch = epoch;
            result = Some(lookup_result);
            if stable {
                break;
            }
        }
        let result = result.expect("lookup loop always runs");
        let cache_reply = self.cache_reply_lock.try_lock();
        stable = stable && cache_reply.is_some() && !self.cache_epoch_changed(stable_epoch);

        match result {
            Ok(Some(stats)) => {
                if stable {
                    self.cache_entry(parent, name_str, &stats);
                }
                let attr = fillattr(&stats);
                reply.entry_with_ttls(
                    if stable {
                        &self.cache_config.entry_ttl
                    } else {
                        &Duration::ZERO
                    },
                    if stable {
                        &self.cache_config.attr_ttl
                    } else {
                        &Duration::ZERO
                    },
                    &attr,
                    0,
                );
            }
            Ok(None) => {
                if stable {
                    self.cache_negative_entry(parent, name_str);
                }
                self.reply_negative_entry_with_ttl(reply, stable);
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Retrieves file attributes for a given inode.
    ///
    /// Returns metadata (size, permissions, timestamps, etc.) for the file or
    /// directory identified by `ino`. Root inode (1) is handled specially.
    fn getattr(&self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        agentfs_sdk::profiling::record_fuse_getattr();
        tracing::debug!("FUSE::getattr: ino={}", ino);

        if let Err(e) = self.flush_pending_inode(ino) {
            reply.error(error_to_errno(&e));
            return;
        }

        let cache_epoch = self.cache_epoch();
        if let Some(stats) = self.attr_cache.lock().get(&ino).cloned() {
            let cache_reply = self.cache_reply_lock.try_lock();
            if cache_reply.is_some() && !self.cache_epoch_changed(cache_epoch) {
                reply.attr(&self.cache_config.attr_ttl, &fillattr(&stats));
                return;
            }
        }

        let mut stable = false;
        let mut stable_epoch = 0;
        let mut result = None;
        for _ in 0..2 {
            let epoch = self.cache_epoch();
            let fs = self.fs.clone();
            let getattr_result = self
                .runtime
                .block_on(async move { fs.getattr(ino as i64).await });
            stable = !self.cache_epoch_changed(epoch);
            stable_epoch = epoch;
            result = Some(getattr_result);
            if stable {
                break;
            }
        }
        let result = result.expect("getattr loop always runs");
        let cache_reply = self.cache_reply_lock.try_lock();
        stable = stable && cache_reply.is_some() && !self.cache_epoch_changed(stable_epoch);

        match result {
            Ok(Some(stats)) => {
                if stable {
                    self.cache_attr(&stats);
                }
                reply.attr(
                    if stable {
                        &self.cache_config.attr_ttl
                    } else {
                        &Duration::ZERO
                    },
                    &fillattr(&stats),
                );
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Reads the target of a symbolic link.
    ///
    /// Returns the path that the symlink points to. This is called by operations
    /// like `ls -l` to display symlink targets.
    fn readlink(&self, _req: &Request, ino: u64, reply: ReplyData) {
        tracing::debug!("FUSE::readlink: ino={}", ino);

        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.readlink(ino as i64).await });

        match result {
            Ok(Some(target)) => reply.data(target.as_bytes()),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Sets file attributes, handling truncate and chmod operations.
    ///
    /// Currently `size` changes (truncate) and `mode` changes (chmod) are supported.
    /// Other attribute changes (uid, gid, timestamps) are accepted but ignored.
    fn setattr(
        &self,
        req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<crate::fuser::TimeOrNow>,
        mtime: Option<crate::fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        tracing::debug!(
            "FUSE::setattr: ino={}, mode={:?}, uid={:?}, gid={:?}, size={:?}",
            ino,
            mode,
            uid,
            gid,
            size
        );
        let audit = MutationAudit::new();
        let mutated = mode.is_some()
            || uid.is_some()
            || gid.is_some()
            || size.is_some()
            || atime.is_some()
            || mtime.is_some();

        // Handle chmod
        if let Some(new_mode) = mode {
            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.chmod(ino as i64, new_mode).await });

            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
            self.invalidate_inode_cache(req, ino);
        }

        // Handle chown
        if uid.is_some() || gid.is_some() {
            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.chown(ino as i64, uid, gid).await });

            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
            self.invalidate_inode_cache(req, ino);
        }

        // Handle truncate
        if let Some(new_size) = size {
            if let Err(e) = self.flush_pending_inode(ino) {
                reply.error(error_to_errno(&e));
                return;
            }

            let result = if let Some(fh) = fh {
                // Use file handle if available (ftruncate).
                let file = {
                    let open_files = self.open_files.lock();
                    let Some(open_file) = open_files.get(&fh) else {
                        reply.error(libc::EBADF);
                        return;
                    };

                    open_file.file.clone()
                };

                self.runtime
                    .block_on(async move { file.truncate(new_size).await })
            } else {
                if let Err(e) = self.flush_pending_inode(ino) {
                    reply.error(error_to_errno(&e));
                    return;
                }

                // Open file and truncate via file handle
                let fs = self.fs.clone();
                self.runtime.block_on(async move {
                    let file = fs.open(ino as i64, libc::O_RDWR).await?;
                    file.truncate(new_size).await
                })
            };

            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
            self.invalidate_inode_cache(req, ino);
        }

        // Handle atime/mtime changes (utimensat)
        if atime.is_some() || mtime.is_some() {
            let new_atime = match atime {
                Some(crate::fuser::TimeOrNow::SpecificTime(t)) => {
                    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                    TimeChange::Set(dur.as_secs() as i64, dur.subsec_nanos())
                }
                Some(crate::fuser::TimeOrNow::Now) => TimeChange::Now,
                None => TimeChange::Omit,
            };
            let new_mtime = match mtime {
                Some(crate::fuser::TimeOrNow::SpecificTime(t)) => {
                    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
                    TimeChange::Set(dur.as_secs() as i64, dur.subsec_nanos())
                }
                Some(crate::fuser::TimeOrNow::Now) => TimeChange::Now,
                None => TimeChange::Omit,
            };
            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.utimens(ino as i64, new_atime, new_mtime).await });
            if let Err(e) = result {
                reply.error(error_to_errno(&e));
                return;
            }
            self.invalidate_inode_cache(req, ino);
        }

        // Return updated attributes
        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.getattr(ino as i64).await });

        match result {
            Ok(Some(stats)) => {
                self.cache_attr(&stats);
                if mutated {
                    audit.assert_invalidated("setattr");
                } else {
                    let _ = audit;
                }
                reply.attr(&self.cache_config.attr_ttl, &fillattr(&stats));
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Directory Operations
    // ─────────────────────────────────────────────────────────────

    /// Reads directory entries for the given inode.
    ///
    /// Returns "." and ".." entries followed by the directory contents.
    /// Each entry's inode is cached for subsequent lookups.
    ///
    /// Uses readdir_plus to fetch entries with stats in a single query,
    /// avoiding N+1 database queries.
    fn readdir(&self, _req: &Request, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        agentfs_sdk::profiling::record_fuse_readdir();
        tracing::debug!("FUSE::readdir: ino={}, offset={}", ino, offset);

        let all_entries = match self.cached_readdir_entries(ino) {
            Ok((entries, _stable, _epoch)) => entries,
            Err(e) => {
                reply.error(error_to_errno(&e));
                return;
            }
        };

        for (i, entry) in all_entries.iter().enumerate().skip(readdir_start(offset)) {
            if reply.add(entry.attr.ino, (i + 1) as i64, entry.attr.kind, &entry.name) {
                break;
            }
        }
        reply.ok();
    }

    /// Reads directory entries with full attributes for the given inode.
    ///
    /// This is an optimized version that returns both directory entries and
    /// their attributes in a single call, reducing kernel/userspace round trips.
    /// Uses readdir_plus to fetch entries with stats in a single database query.
    fn readdirplus(
        &self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        agentfs_sdk::profiling::record_fuse_readdir_plus();
        tracing::debug!("FUSE::readdirplus: ino={}, offset={}", ino, offset);

        let (all_entries, stable, stable_epoch) = match self.cached_readdir_entries(ino) {
            Ok(entries) => entries,
            Err(e) => {
                reply.error(error_to_errno(&e));
                return;
            }
        };

        let cache_reply = self.cache_reply_lock.try_lock();
        let stable = stable && cache_reply.is_some() && !self.cache_epoch_changed(stable_epoch);
        for (i, entry) in all_entries.iter().enumerate().skip(readdir_start(offset)) {
            if reply.add_with_ttls(
                entry.attr.ino,
                (i + 1) as i64,
                &entry.name,
                if stable {
                    &self.cache_config.entry_ttl
                } else {
                    &Duration::ZERO
                },
                if stable {
                    &self.cache_config.attr_ttl
                } else {
                    &Duration::ZERO
                },
                &entry.attr,
                0,
            ) {
                reply.ok();
                return;
            }
        }

        reply.ok();
    }

    /// Creates a special file node (FIFO, device, socket, or regular file).
    ///
    /// Creates a file node at `name` under `parent` with the specified mode
    /// and device number, then stats it to return proper attributes.
    fn mknod(
        &self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::mknod: parent={}, name={:?}, mode={:o}, rdev={}",
            parent,
            name,
            mode,
            rdev
        );
        let audit = MutationAudit::new();

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self.runtime.block_on(async move {
            fs.mknod(parent as i64, &name_owned, mode, rdev as u64, uid, gid)
                .await
        });

        match result {
            Ok(stats) => {
                self.invalidate_inode_cache(req, parent);
                self.invalidate_entry_cache(req, parent, name);
                let attr = fillattr(&stats);
                audit.assert_invalidated("mknod");
                reply.entry_with_ttls(&Duration::ZERO, &Duration::ZERO, &attr, 0);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Creates a new directory.
    ///
    /// Creates a directory at `name` under `parent`, then stats it to return
    /// proper attributes and cache the inode mapping.
    fn mkdir(
        &self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::mkdir: parent={}, name={:?}, mode={:o}",
            parent,
            name,
            mode
        );
        let audit = MutationAudit::new();

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.mkdir(parent as i64, &name_owned, mode, uid, gid).await });

        match result {
            Ok(stats) => {
                self.invalidate_inode_cache(req, parent);
                self.invalidate_entry_cache(req, parent, name);
                let attr = fillattr(&stats);
                audit.assert_invalidated("mkdir");
                reply.entry_with_ttls(&Duration::ZERO, &Duration::ZERO, &attr, 0);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Removes an empty directory.
    ///
    /// Verifies the target is a directory and is empty before removal.
    /// Returns `ENOTDIR` if not a directory, `ENOTEMPTY` if not empty.
    fn rmdir(&self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        tracing::debug!("FUSE::rmdir: parent={}, name={:?}", parent, name);
        let audit = MutationAudit::new();

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let removed_stats = self.lookup_child_for_invalidation(parent, name_str);
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.rmdir(parent as i64, &name_owned).await });

        match result {
            Ok(()) => {
                self.invalidate_inode_cache(req, parent);
                if let Some(stats) = removed_stats {
                    self.invalidate_inode_cache(req, stats.ino as u64);
                }
                self.invalidate_entry_cache(req, parent, name);
                audit.assert_invalidated("rmdir");
                reply.ok();
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    // ─────────────────────────────────────────────────────────────
    // File Creation & Removal
    // ─────────────────────────────────────────────────────────────

    /// Creates and opens a new file.
    ///
    /// Creates an empty file at `name` under `parent`, allocates a file handle,
    /// and returns both the file attributes and handle for immediate use.
    fn create(
        &self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        tracing::debug!(
            "FUSE::create: parent={}, name={:?}, mode={:o}",
            parent,
            name,
            mode
        );
        let audit = MutationAudit::new();

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        // Create file with mode, get stats and file handle in one operation
        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self.runtime.block_on(async move {
            fs.create_file(parent as i64, &name_owned, mode, uid, gid)
                .await
        });

        match result {
            Ok((stats, file)) => {
                self.invalidate_inode_cache(req, parent);
                self.invalidate_entry_cache(req, parent, name);
                let attr = fillattr(&stats);

                let fh = self.alloc_fh();
                self.open_files
                    .lock()
                    .insert(fh, OpenFile::new(stats.ino as u64, file));

                audit.assert_invalidated("create");
                reply.created_with_ttls(&Duration::ZERO, &Duration::ZERO, &attr, 0, fh, 0);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Creates a symbolic link.
    ///
    /// Creates a symlink at `name` under `parent` pointing to `link`.
    fn symlink(
        &self,
        req: &Request,
        parent: u64,
        link_name: &OsStr,
        target: &std::path::Path,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::symlink: parent={}, link_name={:?}, target={:?}",
            parent,
            link_name,
            target
        );
        let audit = MutationAudit::new();

        let Some(name_str) = link_name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let Some(target_str) = target.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let uid = req.uid();
        let gid = req.gid();
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let target_owned = target_str.to_string();
        let result = self.runtime.block_on(async move {
            fs.symlink(parent as i64, &name_owned, &target_owned, uid, gid)
                .await
        });

        match result {
            Ok(stats) => {
                self.invalidate_inode_cache(req, parent);
                self.invalidate_entry_cache(req, parent, link_name);
                let attr = fillattr(&stats);
                audit.assert_invalidated("symlink");
                reply.entry_with_ttls(&Duration::ZERO, &Duration::ZERO, &attr, 0);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Creates a hard link.
    ///
    /// Creates a new directory entry `newname` under `newparent` that refers to the
    /// same inode as `ino`. The link count of the inode is incremented.
    fn link(&self, req: &Request, ino: u64, newparent: u64, newname: &OsStr, reply: ReplyEntry) {
        tracing::debug!(
            "FUSE::link: ino={}, newparent={}, newname={:?}",
            ino,
            newparent,
            newname
        );
        let audit = MutationAudit::new();

        let Some(name_str) = newname.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.link(ino as i64, newparent as i64, &name_owned).await });

        match result {
            Ok(stats) => {
                self.invalidate_inode_cache(req, ino);
                self.invalidate_inode_cache(req, newparent);
                self.invalidate_entry_cache(req, newparent, newname);
                let attr = fillattr(&stats);
                audit.assert_invalidated("link");
                reply.entry_with_ttls(&Duration::ZERO, &Duration::ZERO, &attr, 0);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Removes a file (unlinks it from the directory).
    ///
    /// Gets the file's inode before removal to clean up the path cache.
    fn unlink(&self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        tracing::debug!("FUSE::unlink: parent={}, name={:?}", parent, name);
        let audit = MutationAudit::new();

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let removed_stats = self.lookup_child_for_invalidation(parent, name_str);
        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.unlink(parent as i64, &name_owned).await });

        match result {
            Ok(()) => {
                self.invalidate_inode_cache(req, parent);
                if let Some(stats) = removed_stats {
                    self.invalidate_inode_cache(req, stats.ino as u64);
                }
                self.invalidate_entry_cache(req, parent, name);
                audit.assert_invalidated("unlink");
                reply.ok();
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Renames a file or directory.
    ///
    /// Moves `name` from `parent` to `newname` under `newparent`.
    fn rename(
        &self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        tracing::debug!(
            "FUSE::rename: parent={}, name={:?}, newparent={}, newname={:?}",
            parent,
            name,
            newparent,
            newname
        );
        let audit = MutationAudit::new();

        let Some(old_name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let Some(new_name_str) = newname.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let source_stats = self.lookup_child_for_invalidation(parent, old_name_str);
        let replaced_stats = self.lookup_child_for_invalidation(newparent, new_name_str);
        let fs = self.fs.clone();
        let old_name_owned = old_name_str.to_string();
        let new_name_owned = new_name_str.to_string();
        let result = self.runtime.block_on(async move {
            fs.rename(
                parent as i64,
                &old_name_owned,
                newparent as i64,
                &new_name_owned,
            )
            .await
        });

        match result {
            Ok(()) => {
                self.invalidate_inode_cache(req, parent);
                if newparent != parent {
                    self.invalidate_inode_cache(req, newparent);
                }
                if let Some(stats) = source_stats {
                    self.invalidate_inode_cache(req, stats.ino as u64);
                }
                if let Some(stats) = replaced_stats {
                    self.invalidate_inode_cache(req, stats.ino as u64);
                }
                self.invalidate_entry_cache(req, parent, name);
                self.invalidate_entry_cache(req, newparent, newname);
                audit.assert_invalidated("rename");
                reply.ok();
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    // ─────────────────────────────────────────────────────────────
    // File I/O Lifecycle
    // ─────────────────────────────────────────────────────────────

    /// Opens a file for reading or writing.
    ///
    /// Allocates a file handle and opens the file in the filesystem layer.
    fn open(&self, req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        agentfs_sdk::profiling::record_fuse_open();
        tracing::debug!("FUSE::open: ino={}, flags={}", ino, flags);

        let mut keep_cache = false;
        let mut keep_cache_fingerprint = None;
        if fuse_write_open(flags) {
            self.drop_keepcache_eligibility(ino);
        } else if self.cache_config.keepcache_enabled && !self.has_pending_write_for_inode(ino) {
            let fs = self.fs.clone();
            let keep_cache_result = self.runtime.block_on(async move {
                if !fs.keep_cache_for_read_open(ino as i64, flags).await? {
                    return Ok(None);
                }
                let Some(stats) = fs.getattr(ino as i64).await? else {
                    return Ok(None);
                };
                Ok::<_, SdkError>(Some(KeepCacheFingerprint::from_stats(&stats)))
            });
            match keep_cache_result {
                Ok(Some(fingerprint)) if self.keepcache_allows(ino, &fingerprint) => {
                    keep_cache = true;
                    keep_cache_fingerprint = Some(fingerprint);
                }
                Ok(Some(_)) => {
                    agentfs_sdk::profiling::record_base_fast_stale_rejection();
                }
                Ok(None) => {}
                Err(e) => {
                    reply.error(error_to_errno(&e));
                    return;
                }
            };
        }

        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.open(ino as i64, flags).await });

        match result {
            Ok(file) => {
                let mut open_flags = 0;
                keep_cache = keep_cache
                    && keep_cache_fingerprint
                        .as_ref()
                        .map(|fingerprint| self.keepcache_allows(ino, fingerprint))
                        .unwrap_or(false)
                    && !self.has_pending_write_for_inode(ino);
                if keep_cache {
                    open_flags |= FOPEN_KEEP_CACHE;
                    self.mark_keepcache_eligible(
                        ino,
                        keep_cache_fingerprint.expect("checked before enabling keep-cache"),
                    );
                    agentfs_sdk::profiling::record_base_fast_open_eligible();
                    agentfs_sdk::profiling::record_base_fast_open_keep_cache();
                } else {
                    agentfs_sdk::profiling::record_base_fast_open_rejected();
                }
                if fuse_write_open(flags) {
                    self.invalidate_inode_cache(req, ino);
                }
                let fh = self.alloc_fh();
                self.open_files.lock().insert(fh, OpenFile::new(ino, file));
                reply.opened(fh, open_flags);
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Reads data using the file handle.
    fn read(
        &self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        agentfs_sdk::profiling::record_fuse_read();
        tracing::debug!("FUSE::read: fh={}, offset={}, size={}", fh, offset, size);
        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }

        let file = {
            let open_files = self.open_files.lock();
            let Some(open_file) = open_files.get(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            open_file.file.clone()
        };

        if let Err(e) = self.flush_pending_inode(ino) {
            reply.error(error_to_errno(&e));
            return;
        }

        let result = self
            .runtime
            .block_on(async move { file.pread(offset as u64, size as u64).await });

        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Writes data using the file handle.
    fn write(
        &self,
        req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        tracing::debug!(
            "FUSE::write: fh={}, offset={}, data_len={}",
            fh,
            offset,
            data.len()
        );
        let audit = MutationAudit::new();

        if offset < 0 {
            reply.error(libc::EINVAL);
            return;
        }

        let data_len = data.len();
        agentfs_sdk::profiling::record_fuse_write(data_len as u64);
        if let Err(e) = self.flush_pending_inode_except(ino, fh) {
            reply.error(error_to_errno(&e));
            return;
        }

        let writeback_enabled = self.writeback_enabled;

        let flush_result = if writeback_enabled {
            // Coalesce into the per-fh WriteBuffer. Sequential / adjacent
            // FUSE_WRITEs for the same handle merge into one entry instead of
            // taking the SDK batcher's AsyncMutex once per request. Flushing
            // is deferred until either the buffer crosses
            // FUSE_COALESCE_FLUSH_BYTES, or the kernel issues
            // FLUSH / RELEASE / FSYNC for the handle.
            //
            // The take-then-block-on pattern is deliberate: we MUST NOT hold
            // the parking_lot `open_files` lock across `runtime.block_on(...)`
            // or every other FUSE handler serializes behind this fh's SQLite
            // commit. An earlier draft of Axis A2 held the lock through the
            // flush and regressed checkout by 2x.
            let drain = {
                let mut open_files = self.open_files.lock();
                let Some(open_file) = open_files.get_mut(&fh) else {
                    reply.error(libc::EBADF);
                    return;
                };
                match open_file.buffer_fuse_write(offset as u64, data) {
                    Ok(true) => open_file.take_pending(),
                    Ok(false) => None,
                    Err(errno) => {
                        reply.error(errno);
                        return;
                    }
                }
            };
            match drain {
                Some(drain) => flush_pending_batched_out_of_lock(&self.runtime, drain),
                None => Ok(()),
            }
        } else {
            // Writeback disabled: keep the direct, immediate-commit path so
            // each FUSE_WRITE lands in SQLite before we reply (preserves the
            // pre-Tier-One synchronous-write semantics for users that opt out
            // of writeback).
            let file = {
                let open_files = self.open_files.lock();
                let Some(open_file) = open_files.get(&fh) else {
                    reply.error(libc::EBADF);
                    return;
                };
                open_file.file.clone()
            };
            let data = data.to_vec();
            self.runtime.block_on(async move {
                file.pwrite_ranges(vec![WriteRange {
                    offset: offset as u64,
                    data,
                }])
                .await
            })
        };

        match flush_result {
            Ok(()) => {
                self.invalidate_inode_cache(req, ino);
                audit.assert_invalidated("write");
                reply.written(data_len as u32);
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Flushes buffered data to the backend storage.
    fn flush(&self, req: &Request, ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        tracing::debug!("FUSE::flush: fh={}", fh);
        let audit = MutationAudit::new();
        // Tier Three Axis E attempt was reverted: deferring the SDK
        // `drain_writes` here caused subsequent SDK-internal `pread`/`pwrite`
        // entry points (which each prelude with `self.drain_writes()` for
        // read-after-write consistency) to take the drain hit synchronously
        // and serialised reads behind a much larger commit. Keep the
        // restoration of synchronous drain on flush/release; FUSE
        // close-time latency is bounded.
        let (drain, file) = {
            let mut open_files = self.open_files.lock();
            let Some(open_file) = open_files.get_mut(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            (open_file.take_pending(), open_file.file.clone())
        };
        let result = (|| -> Result<(), SdkError> {
            if let Some(drain) = drain {
                flush_pending_batched_out_of_lock(&self.runtime, drain)?;
            }
            self.runtime
                .block_on(async move { file.drain_writes().await })
        })();

        match result {
            Ok(()) => {
                self.invalidate_inode_cache(req, ino);
                audit.assert_invalidated("flush");
                reply.ok();
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Synchronizes file data to persistent storage using the file handle.
    ///
    /// This now uses the file handle's fsync which knows which layer(s) the
    /// file exists in, avoiding errors when a file only exists in one layer.
    fn fsync(&self, _req: &Request, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        tracing::debug!("FUSE::fsync: fh={}", fh);
        let file = {
            let open_files = self.open_files.lock();
            let Some(open_file) = open_files.get(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            open_file.file.clone()
        };

        if let Err(e) = self.flush_pending_inode(ino) {
            reply.error(error_to_errno(&e));
            return;
        }

        let result = self.runtime.block_on(async move { file.fsync().await });

        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Releases (closes) an open file handle.
    ///
    /// Flushes pending writes and removes the file handle from the open files table.
    fn release(
        &self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        agentfs_sdk::profiling::record_fuse_release();
        tracing::debug!("FUSE::release: fh={}", fh);
        // Tier Three Axis E attempt reverted (see `fn flush`): keep
        // synchronous drain on release.
        let (drain, file) = {
            let mut open_files = self.open_files.lock();
            let Some(open_file) = open_files.get_mut(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            (open_file.take_pending(), open_file.file.clone())
        };
        let result = (|| -> Result<(), SdkError> {
            if let Some(drain) = drain {
                flush_pending_batched_out_of_lock(&self.runtime, drain)?;
            }
            self.runtime
                .block_on(async move { file.drain_writes().await })
        })();

        match result {
            Ok(()) => {
                self.open_files.lock().remove(&fh);
                reply.ok();
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Returns filesystem statistics.
    ///
    /// Queries actual usage from the SDK and reports it to tools like `df`.
    fn statfs(&self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
        tracing::debug!("FUSE::statfs");
        const BLOCK_SIZE: u64 = 4096;
        const TOTAL_INODES: u64 = 1_000_000; // Virtual limit
        const MAX_NAMELEN: u32 = 255;

        let fs = self.fs.clone();
        let result = self.runtime.block_on(async move { fs.statfs().await });

        let (used_blocks, used_inodes) = match result {
            Ok(stats) => {
                let used_blocks = stats.bytes_used.div_ceil(BLOCK_SIZE);
                (used_blocks, stats.inodes)
            }
            Err(_) => (0, 1), // Fallback: just root inode
        };

        // Report a large virtual capacity so tools don't think we're out of space
        const TOTAL_BLOCKS: u64 = 1024 * 1024 * 1024; // ~4TB virtual size
        let free_blocks = TOTAL_BLOCKS.saturating_sub(used_blocks);
        let free_inodes = TOTAL_INODES.saturating_sub(used_inodes);

        reply.statfs(
            TOTAL_BLOCKS,
            free_blocks,
            free_blocks,
            TOTAL_INODES,
            free_inodes,
            BLOCK_SIZE as u32,
            MAX_NAMELEN,       // namelen: maximum filename length
            BLOCK_SIZE as u32, // frsize: fragment size
        );
    }

    // ─────────────────────────────────────────────────────────────
    // Inode Lifecycle
    // ─────────────────────────────────────────────────────────────

    /// Forget about an inode.
    ///
    /// Called when the kernel removes an inode from its cache. For passthrough
    /// filesystems (like HostFS), this allows releasing O_PATH file descriptors
    /// that were cached for the inode, preventing file descriptor exhaustion.
    fn forget(&self, _req: &Request, ino: u64, nlookup: u64) {
        tracing::debug!("FUSE::forget: ino={}, nlookup={}", ino, nlookup);
        let fs = self.fs.clone();
        self.runtime.block_on(async move {
            if let Err(error) = fs.drain_inode_writes(ino as i64).await {
                tracing::warn!(
                    "FUSE::forget failed to drain batched writes for inode {}: {}",
                    ino,
                    error
                );
            }
            fs.forget(ino as i64, nlookup).await;
        });
    }

    /// Batch forget multiple inodes at once.
    ///
    /// This is an optimization over calling forget() individually for each inode.
    fn batch_forget(&self, _req: &Request, nodes: &[fuse_forget_one]) {
        tracing::debug!("FUSE::batch_forget: {} nodes", nodes.len());
        let fs = self.fs.clone();
        let nodes_vec: Vec<(i64, u64)> =
            nodes.iter().map(|n| (n.nodeid as i64, n.nlookup)).collect();
        self.runtime.block_on(async move {
            for (ino, nlookup) in nodes_vec {
                if let Err(error) = fs.drain_inode_writes(ino).await {
                    tracing::warn!(
                        "FUSE::batch_forget failed to drain batched writes for inode {}: {}",
                        ino,
                        error
                    );
                }
                fs.forget(ino, nlookup).await;
            }
        });
    }
}

impl Drop for AgentFSFuse {
    fn drop(&mut self) {
        if let Err(e) = self.flush_all_pending() {
            tracing::warn!("FUSE drop failed to flush pending writes: {}", e);
        }
        if let Err(e) = self.finalize_filesystem() {
            tracing::warn!("FUSE drop failed to finalize filesystem: {}", e);
        }
    }
}

impl AgentFSFuse {
    fn flush_pending_inode(&self, ino: u64) -> Result<(), SdkError> {
        // Tier Four: only flush per-fh FUSE WriteBuffer state into the SDK
        // batcher. Do NOT call drain_inode_writes here — the SDK now serves
        // reads from the in-memory overlay (peek_pending merge), so a
        // synchronous SQLite commit on every read is wasted work. Durability
        // remains via fsync/destroy/timer.
        self.flush_open_file_pending_inode_except(ino, 0)
    }

    fn flush_pending_inode_except(&self, ino: u64, except_fh: u64) -> Result<(), SdkError> {
        self.flush_open_file_pending_inode_except(ino, except_fh)
    }

    fn flush_open_file_pending_inode_except(
        &self,
        ino: u64,
        except_fh: u64,
    ) -> Result<(), SdkError> {
        // Collect pending buffers under the lock, then release the lock
        // before issuing the async pwrites. See `OpenFile::take_pending` for
        // why holding the parking_lot lock across `runtime.block_on(...)` is
        // a hot-path foot-gun.
        let drains = {
            let mut open_files = self.open_files.lock();
            let mut drains = Vec::new();
            for (fh, open_file) in open_files.iter_mut() {
                if *fh == except_fh || open_file.ino != ino {
                    continue;
                }
                if let Some(drain) = open_file.take_pending() {
                    drains.push(drain);
                }
            }
            drains
        };
        for drain in drains {
            flush_pending_batched_out_of_lock(&self.runtime, drain)?;
        }
        Ok(())
    }

    fn flush_all_pending(&self) -> Result<(), SdkError> {
        // Same lock-release pattern as `flush_open_file_pending_inode_except`.
        let drains = {
            let mut open_files = self.open_files.lock();
            let mut drains = Vec::new();
            for open_file in open_files.values_mut() {
                if let Some(drain) = open_file.take_pending() {
                    drains.push(drain);
                }
            }
            drains
        };
        for drain in drains {
            flush_pending_batched_out_of_lock(&self.runtime, drain)?;
        }
        Ok(())
    }

    fn cache_epoch(&self) -> u64 {
        self.cache_epoch.load(Ordering::Acquire)
    }

    fn cache_epoch_changed(&self, epoch: u64) -> bool {
        self.cache_epoch.load(Ordering::Acquire) != epoch
    }

    fn bump_cache_epoch(&self) {
        self.cache_epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn reply_negative_entry(&self, reply: ReplyEntry) {
        if self.cache_config.neg_ttl.is_zero() {
            reply.error(libc::ENOENT);
        } else {
            reply.negative(&self.cache_config.neg_ttl);
        }
    }

    fn reply_negative_entry_with_ttl(&self, reply: ReplyEntry, stable: bool) {
        if stable {
            self.reply_negative_entry(reply);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn has_pending_write_for_inode(&self, ino: u64) -> bool {
        self.open_files
            .lock()
            .values()
            .any(|open_file| open_file.ino == ino && !open_file.pending.is_empty())
    }

    fn keepcache_allows(&self, ino: u64, fingerprint: &KeepCacheFingerprint) -> bool {
        self.keepcache_drift_guard.lock().allows(ino, fingerprint)
    }

    fn mark_keepcache_eligible(&self, ino: u64, fingerprint: KeepCacheFingerprint) {
        self.keepcache_drift_guard
            .lock()
            .mark_eligible(ino, fingerprint);
    }

    fn drop_keepcache_eligibility(&self, ino: u64) {
        if self.keepcache_drift_guard.lock().drop_eligibility(ino) {
            agentfs_sdk::profiling::record_fuse_keepcache_eligibility_drop();
        }
    }

    #[allow(dead_code)]
    fn drain_inode_writes(&self, ino: u64) -> Result<(), SdkError> {
        // Kept for emergency parity with pre-Tier-4 paths; not called on the
        // hot read path because the SDK overlay handles read-after-write
        // consistency without forcing a SQLite commit.
        let fs = self.fs.clone();
        self.runtime
            .block_on(async move { fs.drain_inode_writes(ino as i64).await })
    }

    fn finalize_filesystem(&self) -> Result<(), SdkError> {
        let fs = self.fs.clone();
        self.runtime.block_on(async move { fs.finalize().await })
    }

    fn invalidate_inode_cache(&self, req: &Request, ino: u64) {
        let _cache_reply = self.cache_reply_lock.lock();
        self.bump_cache_epoch();
        self.drop_keepcache_eligibility(ino);
        self.invalidate_cached_inode(ino);
        self.notify_inval_inode(req, ino, 0, i64::MAX);
        agentfs_sdk::profiling::record_base_fast_inode_invalidation();
        record_mutation_invalidation();
    }

    fn invalidate_entry_cache(&self, req: &Request, parent: u64, name: &OsStr) {
        let _cache_reply = self.cache_reply_lock.lock();
        self.bump_cache_epoch();
        if let Some(name) = name.to_str() {
            self.invalidate_cached_entry(parent, name);
        }
        self.notify_inval_entry(req, parent, name);
        record_mutation_invalidation();
    }

    fn notify_inval_inode(&self, req: &Request, ino: u64, offset: i64, len: i64) {
        if !self.sync_inval {
            req.deferred_notifier().inval_inode(ino, offset, len);
            return;
        }

        let start = Instant::now();
        let result = req.notifier().inval_inode(ino, offset, len);
        agentfs_sdk::profiling::record_fuse_sync_inval_latency(start.elapsed());

        match result {
            Ok(()) => agentfs_sdk::profiling::record_fuse_sync_inval_inode_ok(),
            Err(e) => {
                tracing::warn!(
                    "synchronous FUSE inval_inode failed ino={}, offset={}, len={}: {}",
                    ino,
                    offset,
                    len,
                    e
                );
                agentfs_sdk::profiling::record_fuse_sync_inval_inode_err();
            }
        }
    }

    fn notify_inval_entry(&self, req: &Request, parent: u64, name: &OsStr) {
        if !self.sync_inval {
            req.deferred_notifier().inval_entry(parent, name);
            return;
        }

        let start = Instant::now();
        let result = req.notifier().inval_entry(parent, name);
        agentfs_sdk::profiling::record_fuse_sync_inval_latency(start.elapsed());

        match result {
            Ok(()) => agentfs_sdk::profiling::record_fuse_sync_inval_entry_ok(),
            Err(e) => {
                tracing::warn!(
                    "synchronous FUSE inval_entry failed parent={}, name={:?}: {}",
                    parent,
                    name,
                    e
                );
                agentfs_sdk::profiling::record_fuse_sync_inval_entry_err();
            }
        }
    }

    fn invalidate_cached_inode(&self, ino: u64) {
        self.attr_cache.lock().remove(&ino);
        self.entry_cache
            .lock()
            .retain(|_, stats| stats.ino as u64 != ino);
        self.dir_entries_cache.lock().retain(|dir_ino, entries| {
            *dir_ino != ino && !entries.iter().any(|entry| entry.attr.ino == ino)
        });
    }

    fn invalidate_cached_entry(&self, parent: u64, name: &str) {
        let key = (parent, name.to_string());
        self.entry_cache.lock().remove(&key);
        if self.negative_entry_cache.lock().remove(&key).is_some() {
            agentfs_sdk::profiling::record_negative_cache_invalidation();
        }
    }

    fn cache_negative_entry(&self, parent: u64, name: &str) {
        let key = (parent, name.to_string());
        self.entry_cache.lock().remove(&key);
        self.negative_entry_cache.lock().insert(key, ());
    }

    fn lookup_child_for_invalidation(&self, parent: u64, name: &str) -> Option<Stats> {
        if let Some(stats) = self
            .entry_cache
            .lock()
            .get(&(parent, name.to_string()))
            .cloned()
        {
            return Some(stats);
        }

        let fs = self.fs.clone();
        self.runtime
            .block_on(async move { fs.lookup(parent as i64, name).await })
            .ok()
            .flatten()
    }

    fn cache_attr(&self, stats: &Stats) {
        self.attr_cache
            .lock()
            .insert(stats.ino as u64, stats.clone());
    }

    fn cache_entry(&self, parent: u64, name: &str, stats: &Stats) {
        self.cache_attr(stats);
        self.invalidate_cached_entry(parent, name);
        self.entry_cache
            .lock()
            .insert((parent, name.to_string()), stats.clone());
    }

    fn cached_attr(&self, ino: u64) -> Result<Option<Stats>, SdkError> {
        let cache_epoch = self.cache_epoch();
        if let Some(stats) = self.attr_cache.lock().get(&ino).cloned() {
            let cache_reply = self.cache_reply_lock.try_lock();
            if cache_reply.is_some() && !self.cache_epoch_changed(cache_epoch) {
                return Ok(Some(stats));
            }
        }

        let cache_epoch = self.cache_epoch();
        let fs = self.fs.clone();
        let stats = self
            .runtime
            .block_on(async move { fs.getattr(ino as i64).await })?;

        let cache_reply = self.cache_reply_lock.try_lock();
        if let Some(ref stats) = stats {
            if cache_reply.is_some() && !self.cache_epoch_changed(cache_epoch) {
                self.cache_attr(stats);
            }
        }

        Ok(stats)
    }

    fn cached_readdir_entries(
        &self,
        ino: u64,
    ) -> Result<(Arc<Vec<CachedDirEntry>>, bool, u64), SdkError> {
        let cache_epoch = self.cache_epoch();
        if let Some(entries) = self.dir_entries_cache.lock().get(&ino).cloned() {
            let cache_reply = self.cache_reply_lock.try_lock();
            if cache_reply.is_some() && !self.cache_epoch_changed(cache_epoch) {
                return Ok((entries, true, cache_epoch));
            }
        }

        let mut stable = false;
        let mut stable_epoch = 0;
        let mut entries_result = None;
        for _ in 0..2 {
            let epoch = self.cache_epoch();
            let fs = self.fs.clone();
            let result = self
                .runtime
                .block_on(async move { fs.readdir_plus(ino as i64).await });
            stable = !self.cache_epoch_changed(epoch);
            stable_epoch = epoch;
            entries_result = Some(result);
            if stable {
                break;
            }
        }
        let entries_result = entries_result.expect("readdir loop always runs");

        let entries = match entries_result {
            Ok(Some(entries)) => entries,
            Ok(None) => return Err(FsError::NotFound.into()),
            Err(e) => return Err(e),
        };

        let dir_stats = self
            .cached_attr(ino)?
            .ok_or_else(|| SdkError::from(FsError::NotFound))?;

        // In the inode-based API we do not track parent relationships directly.
        // Use root's stats for non-root ".." entries as the existing fallback;
        // the kernel handles proper path resolution for parent traversal.
        let parent_stats = if ino == 1 {
            dir_stats.clone()
        } else {
            self.cached_attr(1)?
                .ok_or_else(|| SdkError::from(FsError::NotFound))?
        };
        let cache_reply = self.cache_reply_lock.try_lock();
        stable = stable && cache_reply.is_some() && !self.cache_epoch_changed(stable_epoch);

        if stable {
            for entry in &entries {
                self.cache_entry(ino, &entry.name, &entry.stats);
            }
        }

        let all_entries = build_cached_readdir_entries(&dir_stats, &parent_stats, entries);
        let entries = Arc::new(all_entries);
        if stable {
            self.dir_entries_cache.lock().insert(ino, entries.clone());
        }
        Ok((entries, stable, stable_epoch))
    }

    /// Create a new FUSE filesystem adapter wrapping a FileSystem instance.
    ///
    /// The provided Tokio runtime is used to execute async FileSystem operations
    /// from within synchronous FUSE callbacks via `block_on`.
    fn new(fs: Arc<dyn FileSystem>, runtime: Runtime) -> Self {
        let sync_inval = fuse_sync_inval_enabled_from_env();
        let cache_config = FuseKernelCacheConfig::from_env();
        cache_config.record_profile();
        let writeback_enabled = cache_config.writeback_cache_enabled;
        Self {
            fs,
            runtime,
            cache_config,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            dir_entries_cache: Arc::new(Mutex::new(HashMap::new())),
            attr_cache: Arc::new(Mutex::new(HashMap::new())),
            entry_cache: Arc::new(Mutex::new(HashMap::new())),
            negative_entry_cache: Arc::new(Mutex::new(HashMap::new())),
            keepcache_drift_guard: Arc::new(Mutex::new(KeepCacheDriftGuard::default())),
            cache_reply_lock: Arc::new(Mutex::new(())),
            cache_epoch: AtomicU64::new(0),
            next_fh: AtomicU64::new(1),
            sync_inval,
            _profile_report: Arc::new(agentfs_sdk::profiling::ProfileReportGuard::new(
                "fuse_session",
            )),
            writeback_enabled,
        }
    }

    /// Allocate a new file handle for tracking open files.
    ///
    /// Similar to the Linux kernel's `get_unused_fd()`, this returns a unique
    /// handle that identifies an open file throughout its lifetime.
    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::SeqCst)
    }
}

fn readdir_start(offset: i64) -> usize {
    usize::try_from(offset).unwrap_or(0)
}

fn fuse_write_open(flags: i32) -> bool {
    (flags & libc::O_ACCMODE) != libc::O_RDONLY || (flags & libc::O_TRUNC) != 0
}

fn env_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "1" => Some(true),
        "0" => Some(false),
        value
            if value.eq_ignore_ascii_case("true")
                || value.eq_ignore_ascii_case("yes")
                || value.eq_ignore_ascii_case("on") =>
        {
            Some(true)
        }
        value
            if value.eq_ignore_ascii_case("false")
                || value.eq_ignore_ascii_case("no")
                || value.eq_ignore_ascii_case("off") =>
        {
            Some(false)
        }
        _ => None,
    }
}

fn env_duration_ms(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => match value.parse::<u64>() {
            Ok(ms) => ms,
            Err(_) => {
                tracing::warn!(
                    "Ignoring invalid {}={} for FUSE TTL; using {}ms",
                    name,
                    value,
                    default
                );
                default
            }
        },
        Err(_) => default,
    }
}

fn env_flag_default(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(value) => env_bool(&value).unwrap_or_else(|| {
            tracing::warn!(
                "Ignoring invalid {}={} for FUSE kernel cache flag; using default {}",
                name,
                value,
                default
            );
            default
        }),
        Err(_) => default,
    }
}

fn fuse_workers_serial_from_env() -> bool {
    std::env::var("AGENTFS_FUSE_WORKERS")
        .map(|value| {
            let value = value.trim();
            value.eq_ignore_ascii_case("serial") || value == "0"
        })
        // Default (unset): parallel dispatch so kernel cache invariants hold.
        // Pair with the matching default in cli/src/fuser/session.rs::FuseDispatchMode::from_env.
        .unwrap_or(false)
}

fn fuse_workers_not_serial_from_env() -> bool {
    !fuse_workers_serial_from_env()
}

fn fuse_sync_inval_enabled_from_env() -> bool {
    let workers_serial = fuse_workers_serial_from_env();
    let sync_requested = match std::env::var("AGENTFS_FUSE_SYNC_INVAL") {
        Ok(value) => match env_bool(&value) {
            Some(enabled) => enabled,
            None => {
                tracing::warn!(
                    "Ignoring invalid AGENTFS_FUSE_SYNC_INVAL={:?}; expected 0/1/true/false",
                    value
                );
                // Fall back to deferred invalidation (the safe default).
                false
            }
        },
        // Default (unset): use deferred invalidation. Synchronous writev of
        // FUSE_NOTIFY_INVAL_* from a request handler can deadlock with the
        // kernel: the notify triggers d_invalidate -> iput -> FUSE_FORGET, and
        // the kernel may block the writev call until that FORGET is delivered.
        // In parallel mode this surfaces under git workloads (clone, checkout)
        // when the session thread is blocked on a full worker queue and cannot
        // read the pending FORGET. The DeferredNotifier thread is the only
        // path that's safe in both serial and parallel modes, so it is the
        // default. Users who explicitly opt into AGENTFS_FUSE_SYNC_INVAL=1
        // accept the deadlock risk in exchange for tighter cache coherency.
        Err(_) => false,
    };

    if workers_serial && sync_requested {
        tracing::info!(
            "AGENTFS_FUSE_SYNC_INVAL requested with AGENTFS_FUSE_WORKERS=serial; using deferred invalidation to avoid notify/reply deadlock"
        );
        false
    } else {
        sync_requested
    }
}

fn readdirplus_mode_from_env() -> ReaddirPlusMode {
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
                "Ignoring invalid AGENTFS_FUSE_READDIRPLUS={}; disabling readdirplus",
                value
            );
            ReaddirPlusMode::Off
        }
        Err(_) => ReaddirPlusMode::Auto,
    }
}

impl ReaddirPlusMode {
    fn profile_value(self) -> u64 {
        match self {
            ReaddirPlusMode::Off => READDIRPLUS_MODE_OFF,
            ReaddirPlusMode::Auto => READDIRPLUS_MODE_AUTO,
            ReaddirPlusMode::Always => READDIRPLUS_MODE_ALWAYS,
        }
    }
}

fn configure_writeback_cache(config: &mut KernelConfig, enabled: bool) {
    if !enabled {
        agentfs_sdk::profiling::set_fuse_writeback_cache_enabled(false);
        return;
    }

    match config.add_capabilities(FUSE_WRITEBACK_CACHE) {
        Ok(()) => agentfs_sdk::profiling::set_fuse_writeback_cache_enabled(true),
        Err(_) => {
            tracing::warn!("Kernel does not support FUSE_WRITEBACK_CACHE; leaving it disabled");
            agentfs_sdk::profiling::set_fuse_writeback_cache_enabled(false);
        }
    }
}

fn configure_readdirplus(config: &mut KernelConfig, mode: ReaddirPlusMode) {
    agentfs_sdk::profiling::set_fuse_readdirplus_mode(mode.profile_value());

    // FUSE_READDIRPLUS opcode 44 is decoded by the vendored fuser dispatcher
    // only when the `abi-7-21` feature is enabled. If we advertised the
    // capability without that feature, the kernel would send opcode 44 and the
    // dispatcher would return ENOSYS, breaking readdir on the mount. Gating the
    // capability negotiation here turns the mismatch into a compile-time
    // expectation rather than a runtime kernel error.
    #[cfg(not(feature = "abi-7-21"))]
    {
        if !matches!(mode, ReaddirPlusMode::Off) {
            tracing::warn!(
                ?mode,
                "AGENTFS_FUSE_READDIRPLUS requested but cli compiled without abi-7-21 feature; \
                 capability not advertised (kernel would send opcodes the dispatcher cannot decode)"
            );
        }
        let _ = config;
        return;
    }

    #[cfg(feature = "abi-7-21")]
    match mode {
        ReaddirPlusMode::Off => {}
        ReaddirPlusMode::Auto => {
            agentfs_sdk::profiling::record_fuse_readdirplus_auto_requested();
            match config.add_capabilities(FUSE_DO_READDIRPLUS) {
                Ok(()) => agentfs_sdk::profiling::record_fuse_readdirplus_do_enabled(),
                Err(_) => {
                    tracing::warn!("Kernel does not support FUSE_DO_READDIRPLUS");
                    agentfs_sdk::profiling::record_fuse_readdirplus_unsupported();
                }
            }
            match config.add_capabilities(FUSE_READDIRPLUS_AUTO) {
                Ok(()) => agentfs_sdk::profiling::record_fuse_readdirplus_auto_enabled(),
                Err(_) => {
                    tracing::warn!("Kernel does not support FUSE_READDIRPLUS_AUTO");
                    agentfs_sdk::profiling::record_fuse_readdirplus_unsupported();
                }
            }
        }
        ReaddirPlusMode::Always => {
            agentfs_sdk::profiling::record_fuse_readdirplus_do_requested();
            match config.add_capabilities(FUSE_DO_READDIRPLUS) {
                Ok(()) => agentfs_sdk::profiling::record_fuse_readdirplus_do_enabled(),
                Err(_) => agentfs_sdk::profiling::record_fuse_readdirplus_unsupported(),
            }
        }
    }
}

fn build_cached_readdir_entries(
    dir_stats: &Stats,
    parent_stats: &Stats,
    entries: Vec<DirEntry>,
) -> Vec<CachedDirEntry> {
    let mut all_entries = Vec::with_capacity(entries.len() + 2);

    all_entries.push(cached_dir_entry(".", dir_stats));
    all_entries.push(cached_dir_entry("..", parent_stats));

    for entry in entries {
        all_entries.push(cached_dir_entry(entry.name, &entry.stats));
    }

    all_entries
}

fn cached_dir_entry(name: impl Into<String>, stats: &Stats) -> CachedDirEntry {
    CachedDirEntry {
        name: name.into(),
        attr: fillattr(stats),
    }
}

// ─────────────────────────────────────────────────────────────
// Attribute Conversion
// ─────────────────────────────────────────────────────────────

/// Fill a `FileAttr` from AgentFS stats.
///
/// Similar to the Linux kernel's `generic_fillattr()`, this converts
/// filesystem-specific stat information into the VFS attribute structure.
///
/// The uid and gid parameters override the stored values to ensure proper
/// file ownership reporting (avoids "dubious ownership" errors from git).
fn fillattr(stats: &Stats) -> FileAttr {
    let file_type = stats.mode & S_IFMT;
    let kind = match file_type {
        S_IFDIR => FileType::Directory,
        S_IFLNK => FileType::Symlink,
        S_IFIFO => FileType::NamedPipe,
        S_IFCHR => FileType::CharDevice,
        S_IFBLK => FileType::BlockDevice,
        S_IFSOCK => FileType::Socket,
        _ => FileType::RegularFile,
    };

    let size = if file_type == S_IFDIR {
        4096_u64 // Standard directory size
    } else {
        stats.size as u64
    };

    FileAttr {
        ino: stats.ino as u64,
        size,
        blocks: size.div_ceil(512),
        atime: UNIX_EPOCH + Duration::new(stats.atime as u64, stats.atime_nsec),
        mtime: UNIX_EPOCH + Duration::new(stats.mtime as u64, stats.mtime_nsec),
        ctime: UNIX_EPOCH + Duration::new(stats.ctime as u64, stats.ctime_nsec),
        crtime: UNIX_EPOCH,
        kind,
        perm: (stats.mode & 0o7777) as u16,
        nlink: stats.nlink,
        uid: stats.uid,
        gid: stats.gid,
        rdev: stats.rdev as u32,
        flags: 0,
        blksize: 512,
    }
}

/// Check if allow_other is supported for FUSE mounts.
///
/// Returns true if the current user is root or if user_allow_other is enabled
/// in /etc/fuse.conf.
fn allow_other_supported() -> bool {
    // Root can always use allow_other
    if unsafe { libc::getuid() } == 0 {
        return true;
    }

    // Check if user_allow_other is enabled in /etc/fuse.conf
    if let Ok(contents) = std::fs::read_to_string("/etc/fuse.conf") {
        for line in contents.lines() {
            let line = line.trim();
            // Skip comments and empty lines
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            if line == "user_allow_other" {
                return true;
            }
        }
    }

    false
}

pub fn mount(
    fs: Arc<dyn FileSystem>,
    opts: FuseMountOptions,
    runtime: Runtime,
) -> anyhow::Result<()> {
    // Raise fd limit to hard limit to prevent "too many open files" errors
    // when passthrough filesystems cache O_PATH file descriptors
    maximize_fd_limit();

    let fs = AgentFSFuse::new(fs, runtime);

    let mut mount_opts = vec![
        MountOption::FSName(opts.fsname),
        // Enable kernel-level permission checking based on file mode/uid/gid
        MountOption::DefaultPermissions,
    ];

    // Allow users other than the one who mounted the filesystem to access it.
    // This requires either running as root or having user_allow_other enabled
    // in /etc/fuse.conf.
    if opts.allow_other {
        if allow_other_supported() {
            mount_opts.push(MountOption::AllowOther);
        } else {
            anyhow::bail!(
                "FUSE allow_other not supported. Add 'user_allow_other' to /etc/fuse.conf or run as root."
            );
        }
    }

    if opts.auto_unmount {
        mount_opts.push(MountOption::AutoUnmount);
    }
    if opts.allow_root {
        mount_opts.push(MountOption::AllowRoot);
    }

    crate::fuser::mount2(fs, &opts.mountpoint, &mount_opts)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_cached_readdir_entries, fuse_write_open, readdir_start, OpenFile, WriteBuffer,
    };
    use agentfs_sdk::filesystem::{DirEntry, Stats, WriteRange, S_IFDIR, S_IFLNK, S_IFREG};
    use agentfs_sdk::{BoxedFile, File};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tokio::runtime::Runtime;

    fn ranges(buffer: &WriteBuffer) -> Vec<(u64, Vec<u8>)> {
        buffer
            .ranges_for_flush()
            .into_iter()
            .map(|range| (range.offset, range.data))
            .collect()
    }

    #[derive(Default)]
    struct RecordingFile {
        pwrite_calls: AtomicUsize,
        pwrite_ranges_calls: AtomicUsize,
        ranges: Mutex<Vec<WriteRange>>,
    }

    #[async_trait::async_trait]
    impl File for RecordingFile {
        async fn pread(&self, _offset: u64, _size: u64) -> agentfs_sdk::error::Result<Vec<u8>> {
            Ok(Vec::new())
        }

        async fn pwrite(&self, _offset: u64, _data: &[u8]) -> agentfs_sdk::error::Result<()> {
            self.pwrite_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn pwrite_ranges(&self, ranges: Vec<WriteRange>) -> agentfs_sdk::error::Result<()> {
            self.pwrite_ranges_calls.fetch_add(1, Ordering::SeqCst);
            *self.ranges.lock().unwrap() = ranges;
            Ok(())
        }

        async fn truncate(&self, _size: u64) -> agentfs_sdk::error::Result<()> {
            Ok(())
        }

        async fn fsync(&self) -> agentfs_sdk::error::Result<()> {
            Ok(())
        }

        async fn fstat(&self) -> agentfs_sdk::error::Result<Stats> {
            Ok(stats(1, S_IFREG | 0o644))
        }
    }

    fn stats(ino: i64, mode: u32) -> Stats {
        Stats {
            ino,
            mode,
            nlink: 1,
            uid: 1000,
            gid: 1000,
            size: 123,
            atime: 1,
            mtime: 2,
            ctime: 3,
            atime_nsec: 4,
            mtime_nsec: 5,
            ctime_nsec: 6,
            rdev: 0,
        }
    }

    #[test]
    fn readdir_start_clamps_negative_offsets_to_beginning() {
        assert_eq!(readdir_start(-1), 0);
        assert_eq!(readdir_start(0), 0);
        assert_eq!(readdir_start(2), 2);
    }

    #[test]
    fn fuse_write_open_detects_mutating_flags() {
        assert!(!fuse_write_open(libc::O_RDONLY));
        assert!(fuse_write_open(libc::O_WRONLY));
        assert!(fuse_write_open(libc::O_RDWR));
        assert!(fuse_write_open(libc::O_RDONLY | libc::O_TRUNC));
    }

    #[test]
    fn cached_readdir_entries_include_attrs_for_dot_dotdot_and_children() {
        let dir = stats(10, S_IFDIR | 0o755);
        let parent = stats(1, S_IFDIR | 0o755);
        let child = stats(11, S_IFREG | 0o644);
        let symlink = stats(12, S_IFLNK | 0o777);

        let entries = build_cached_readdir_entries(
            &dir,
            &parent,
            vec![
                DirEntry {
                    name: "file.txt".to_string(),
                    stats: child,
                },
                DirEntry {
                    name: "link".to_string(),
                    stats: symlink,
                },
            ],
        );

        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name, ".");
        assert_eq!(entries[0].attr.ino, 10);
        assert_eq!(entries[0].attr.kind, crate::fuser::FileType::Directory);
        assert_eq!(entries[1].name, "..");
        assert_eq!(entries[1].attr.ino, 1);
        assert_eq!(entries[1].attr.kind, crate::fuser::FileType::Directory);
        assert_eq!(entries[2].name, "file.txt");
        assert_eq!(entries[2].attr.ino, 11);
        assert_eq!(entries[2].attr.kind, crate::fuser::FileType::RegularFile);
        assert_eq!(entries[3].name, "link");
        assert_eq!(entries[3].attr.ino, 12);
        assert_eq!(entries[3].attr.kind, crate::fuser::FileType::Symlink);
    }

    #[test]
    fn write_buffer_merges_adjacent_ranges() {
        let mut buffer = WriteBuffer::default();

        buffer.write(0, b"hello").unwrap();
        buffer.write(5, b" world").unwrap();

        assert_eq!(buffer.bytes(), 11);
        assert_eq!(ranges(&buffer), vec![(0, b"hello world".to_vec())]);
    }

    #[test]
    fn write_buffer_overlays_overlapping_writes() {
        let mut buffer = WriteBuffer::default();

        buffer.write(0, b"abcdef").unwrap();
        buffer.write(2, b"ZZ").unwrap();

        assert_eq!(buffer.bytes(), 6);
        assert_eq!(ranges(&buffer), vec![(0, b"abZZef".to_vec())]);
    }

    #[test]
    fn write_buffer_overlays_following_range() {
        let mut buffer = WriteBuffer::default();

        buffer.write(10, b"abc").unwrap();
        buffer.write(8, b"ZZZZ").unwrap();

        assert_eq!(buffer.bytes(), 5);
        assert_eq!(ranges(&buffer), vec![(8, b"ZZZZc".to_vec())]);
    }

    #[test]
    fn write_buffer_bridges_two_existing_ranges() {
        let mut buffer = WriteBuffer::default();

        buffer.write(0, b"ab").unwrap();
        buffer.write(4, b"ef").unwrap();
        buffer.write(2, b"cd").unwrap();

        assert_eq!(buffer.bytes(), 6);
        assert_eq!(ranges(&buffer), vec![(0, b"abcdef".to_vec())]);
    }

    #[test]
    fn write_buffer_keeps_disjoint_ranges_ordered() {
        let mut buffer = WriteBuffer::default();

        buffer.write(10, b"tail").unwrap();
        buffer.write(0, b"head").unwrap();

        assert_eq!(buffer.bytes(), 8);
        assert_eq!(
            ranges(&buffer),
            vec![(0, b"head".to_vec()), (10, b"tail".to_vec())]
        );
    }

    #[test]
    fn write_buffer_rejects_offset_overflow() {
        let mut buffer = WriteBuffer::default();

        assert_eq!(buffer.write(u64::MAX, b"x"), Err(libc::EINVAL));
        assert!(buffer.is_empty());
    }

    #[test]
    fn open_file_flushes_pending_writes_with_batch_api() {
        let runtime = Runtime::new().unwrap();
        let recorder = Arc::new(RecordingFile::default());
        let file: BoxedFile = recorder.clone();
        let mut open_file = OpenFile::new(1, file);

        open_file.buffer_write(0, b"head").unwrap();
        open_file.buffer_write(10, b"tail").unwrap();
        open_file.flush_pending(&runtime).unwrap();

        assert_eq!(recorder.pwrite_calls.load(Ordering::SeqCst), 0);
        assert_eq!(recorder.pwrite_ranges_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *recorder.ranges.lock().unwrap(),
            vec![
                WriteRange {
                    offset: 0,
                    data: b"head".to_vec(),
                },
                WriteRange {
                    offset: 10,
                    data: b"tail".to_vec(),
                },
            ]
        );
        assert!(open_file.pending.is_empty());
    }
}
