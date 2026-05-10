use crate::fuser::{
    consts::{
        FUSE_ASYNC_READ, FUSE_CACHE_SYMLINKS, FUSE_NO_OPENDIR_SUPPORT, FUSE_PARALLEL_DIROPS,
        FUSE_WRITEBACK_CACHE,
    },
    fuse_forget_one, FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request,
};
use agentfs_sdk::error::Error as SdkError;
use agentfs_sdk::filesystem::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFSOCK};
use agentfs_sdk::{BoxedFile, FileSystem, Stats, TimeChange};
use parking_lot::Mutex;
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
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

/// Cache entries never expire — we use deferred kernel cache invalidation
/// (via Notifier::inval_entry) after mutations to keep the dcache consistent.
/// This is safe because we are the only writer to the filesystem.
const TTL: Duration = Duration::MAX;

/// Maximum pending write data buffered per open FUSE file handle.
const MAX_PENDING_WRITE_BYTES: usize = 4 * 1024 * 1024;

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

    fn buffer_write(&mut self, offset: u64, data: &[u8]) -> Result<(), i32> {
        self.pending.write(offset, data)?;
        Ok(())
    }

    fn pending_bytes(&self) -> usize {
        self.pending.bytes()
    }

    fn flush_pending(&mut self, runtime: &Runtime) -> Result<(), SdkError> {
        if self.pending.is_empty() {
            return Ok(());
        }

        let file = self.file.clone();
        let ranges = self.pending.ranges_for_flush();
        let range_count = ranges.len() as u64;
        let byte_count = ranges
            .iter()
            .map(|(_, data)| data.len() as u64)
            .sum::<u64>();

        for (offset, data) in ranges {
            let file = file.clone();
            runtime.block_on(async move { file.pwrite(offset, &data).await })?;
        }

        self.pending.clear();
        agentfs_sdk::profiling::record_fuse_flush(range_count, byte_count);
        Ok(())
    }
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

    fn bytes(&self) -> usize {
        self.bytes
    }

    fn clear(&mut self) {
        self.ranges.clear();
        self.bytes = 0;
    }

    fn ranges_for_flush(&self) -> Vec<(u64, Vec<u8>)> {
        self.ranges
            .iter()
            .map(|(&offset, data)| (offset, data.clone()))
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

struct AgentFSFuse {
    fs: Arc<dyn FileSystem>,
    runtime: Runtime,
    /// Maps file handle -> open file state
    open_files: Arc<Mutex<HashMap<u64, OpenFile>>>,
    /// Next file handle to allocate
    next_fh: AtomicU64,
    /// Emits a profiling summary when the FUSE session object is dropped.
    _profile_report: Arc<agentfs_sdk::profiling::ProfileReportGuard>,
}

impl Filesystem for AgentFSFuse {
    /// Initialize the filesystem and enable performance optimizations.
    ///
    /// - Async read: allows the kernel to issue multiple read requests in parallel,
    ///   improving throughput for concurrent file access.
    /// - Writeback caching: allows the kernel to buffer writes and flush them
    ///   later, significantly improving write performance for small writes.
    /// - Parallel dirops: allows concurrent lookup() and readdir() on the same
    ///   directory, improving performance for parallel file access patterns.
    /// - Cache symlinks: caches readlink responses, avoiding repeated round-trips
    ///   for symlink resolution.
    /// - No opendir support: skips opendir/releasedir calls since we don't track
    ///   directory handles, reducing round-trips for directory operations.
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        tracing::debug!("FUSE::init");
        let _ = config.add_capabilities(
            FUSE_ASYNC_READ
                | FUSE_WRITEBACK_CACHE
                | FUSE_PARALLEL_DIROPS
                | FUSE_CACHE_SYMLINKS
                | FUSE_NO_OPENDIR_SUPPORT,
        );
        Ok(())
    }

    fn destroy(&mut self) {
        tracing::debug!("FUSE::destroy");
        if let Err(e) = self.flush_all_pending() {
            tracing::warn!("FUSE::destroy failed to flush pending writes: {}", e);
        }
    }

    // ─────────────────────────────────────────────────────────────
    // Name Resolution & Attributes
    // ─────────────────────────────────────────────────────────────

    /// Looks up a directory entry by name within a parent directory.
    ///
    /// Resolves `name` under the directory identified by `parent` inode.
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        agentfs_sdk::profiling::record_fuse_lookup();
        tracing::debug!("FUSE::lookup: parent={}, name={:?}", parent, name);

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.lookup(parent as i64, &name_owned).await });

        match result {
            Ok(Some(stats)) => {
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Retrieves file attributes for a given inode.
    ///
    /// Returns metadata (size, permissions, timestamps, etc.) for the file or
    /// directory identified by `ino`. Root inode (1) is handled specially.
    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        agentfs_sdk::profiling::record_fuse_getattr();
        tracing::debug!("FUSE::getattr: ino={}", ino);

        if let Err(e) = self.flush_pending_inode(ino) {
            reply.error(error_to_errno(&e));
            return;
        }

        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.getattr(ino as i64).await });

        match result {
            Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats)),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Reads the target of a symbolic link.
    ///
    /// Returns the path that the symlink points to. This is called by operations
    /// like `ls -l` to display symlink targets.
    fn readlink(&mut self, _req: &Request, ino: u64, reply: ReplyData) {
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
        &mut self,
        _req: &Request,
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
        }

        // Return updated attributes
        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.getattr(ino as i64).await });

        match result {
            Ok(Some(stats)) => reply.attr(&TTL, &fillattr(&stats)),
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
    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        agentfs_sdk::profiling::record_fuse_readdir();
        tracing::debug!("FUSE::readdir: ino={}, offset={}", ino, offset);

        let fs = self.fs.clone();
        let entries_result = self
            .runtime
            .block_on(async move { fs.readdir_plus(ino as i64).await });

        let entries = match entries_result {
            Ok(Some(entries)) => entries,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
                return;
            }
        };

        // Determine parent inode for ".." entry
        // In the inode-based API we don't track parent relationships directly.
        // The kernel tracks this information and will resolve ".." correctly.
        // We use 1 (root) as a fallback which is safe since the kernel
        // won't actually use this value for path resolution.
        let parent_ino = 1u64;

        let mut all_entries = vec![
            (ino, FileType::Directory, "."),
            (parent_ino, FileType::Directory, ".."),
        ];

        // Process entries with stats already available (no N+1 queries!)
        for entry in &entries {
            let kind = if entry.stats.is_directory() {
                FileType::Directory
            } else if entry.stats.is_symlink() {
                FileType::Symlink
            } else {
                FileType::RegularFile
            };

            all_entries.push((entry.stats.ino as u64, kind, entry.name.as_str()));
        }

        for (i, entry) in all_entries.iter().enumerate().skip(offset as usize) {
            if reply.add(entry.0, (i + 1) as i64, entry.1, entry.2) {
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
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectoryPlus,
    ) {
        agentfs_sdk::profiling::record_fuse_readdir_plus();
        tracing::debug!("FUSE::readdirplus: ino={}, offset={}", ino, offset);

        let fs = self.fs.clone();
        let entries_result = self
            .runtime
            .block_on(async move { fs.readdir_plus(ino as i64).await });

        let entries = match entries_result {
            Ok(Some(entries)) => entries,
            Ok(None) => {
                reply.error(libc::ENOENT);
                return;
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
                return;
            }
        };

        // Get current directory stats for "."
        let fs = self.fs.clone();
        let dir_stats = self
            .runtime
            .block_on(async move { fs.getattr(ino as i64).await })
            .ok()
            .flatten();

        // Determine parent inode and stats for ".." entry
        // In the inode-based API we don't track parent relationships directly.
        // Use root's stats for ".." as a fallback - the kernel handles proper ".." resolution.
        let (parent_ino, parent_stats) = if ino == 1 {
            (1u64, dir_stats.clone()) // Root's parent is itself
        } else {
            // Use root inode as fallback for parent
            let fs = self.fs.clone();
            let parent_stats = self
                .runtime
                .block_on(async move { fs.getattr(1).await })
                .ok()
                .flatten();
            (1u64, parent_stats)
        };

        // Build the entries list with full attributes
        let mut offset_counter = 0i64;

        // Add "." entry
        if offset <= offset_counter {
            if let Some(ref stats) = dir_stats {
                let attr = fillattr(stats);
                if reply.add(ino, offset_counter + 1, ".", &TTL, &attr, 0) {
                    reply.ok();
                    return;
                }
            }
        }
        offset_counter += 1;

        // Add ".." entry
        if offset <= offset_counter {
            if let Some(ref stats) = parent_stats {
                let attr = fillattr(stats);
                if reply.add(parent_ino, offset_counter + 1, "..", &TTL, &attr, 0) {
                    reply.ok();
                    return;
                }
            }
        }
        offset_counter += 1;

        // Add directory entries with their attributes
        for entry in &entries {
            if offset <= offset_counter {
                let attr = fillattr(&entry.stats);

                if reply.add(
                    entry.stats.ino as u64,
                    offset_counter + 1,
                    &entry.name,
                    &TTL,
                    &attr,
                    0,
                ) {
                    reply.ok();
                    return;
                }
            }
            offset_counter += 1;
        }

        reply.ok();
    }

    /// Creates a special file node (FIFO, device, socket, or regular file).
    ///
    /// Creates a file node at `name` under `parent` with the specified mode
    /// and device number, then stats it to return proper attributes.
    fn mknod(
        &mut self,
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
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
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
        &mut self,
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
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
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
    fn rmdir(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        tracing::debug!("FUSE::rmdir: parent={}, name={:?}", parent, name);

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.rmdir(parent as i64, &name_owned).await });

        match result {
            Ok(()) => {
                reply.ok();
                req.deferred_notifier().inval_entry(parent, name);
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
        &mut self,
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
                let attr = fillattr(&stats);

                let fh = self.alloc_fh();
                self.open_files
                    .lock()
                    .insert(fh, OpenFile::new(stats.ino as u64, file));

                reply.created(&TTL, &attr, 0, fh, 0);
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
        &mut self,
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
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
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
    fn link(
        &mut self,
        _req: &Request,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        tracing::debug!(
            "FUSE::link: ino={}, newparent={}, newname={:?}",
            ino,
            newparent,
            newname
        );

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
                let attr = fillattr(&stats);
                reply.entry(&TTL, &attr, 0);
            }
            Err(e) => {
                reply.error(error_to_errno(&e));
            }
        }
    }

    /// Removes a file (unlinks it from the directory).
    ///
    /// Gets the file's inode before removal to clean up the path cache.
    fn unlink(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        tracing::debug!("FUSE::unlink: parent={}, name={:?}", parent, name);

        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let fs = self.fs.clone();
        let name_owned = name_str.to_string();
        let result = self
            .runtime
            .block_on(async move { fs.unlink(parent as i64, &name_owned).await });

        match result {
            Ok(()) => {
                reply.ok();
                req.deferred_notifier().inval_entry(parent, name);
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Renames a file or directory.
    ///
    /// Moves `name` from `parent` to `newname` under `newparent`.
    fn rename(
        &mut self,
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

        let Some(old_name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        let Some(new_name_str) = newname.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

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
                reply.ok();
                let dn = req.deferred_notifier();
                dn.inval_entry(parent, name);
                dn.inval_entry(newparent, newname);
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
    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        agentfs_sdk::profiling::record_fuse_open();
        tracing::debug!("FUSE::open: ino={}, flags={}", ino, flags);

        let fs = self.fs.clone();
        let result = self
            .runtime
            .block_on(async move { fs.open(ino as i64, flags).await });

        match result {
            Ok(file) => {
                let fh = self.alloc_fh();
                self.open_files.lock().insert(fh, OpenFile::new(ino, file));
                reply.opened(fh, 0);
            }
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Reads data using the file handle.
    fn read(
        &mut self,
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
        &mut self,
        _req: &Request,
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

        let result = {
            let mut open_files = self.open_files.lock();
            let Some(open_file) = open_files.get_mut(&fh) else {
                reply.error(libc::EBADF);
                return;
            };

            if data_len > MAX_PENDING_WRITE_BYTES {
                let file = open_file.file.clone();
                if let Err(e) = open_file.flush_pending(&self.runtime) {
                    reply.error(error_to_errno(&e));
                    return;
                }
                return match self
                    .runtime
                    .block_on(async move { file.pwrite(offset as u64, data).await })
                {
                    Ok(()) => {
                        reply.written(data_len as u32);
                    }
                    Err(e) => {
                        reply.error(error_to_errno(&e));
                    }
                };
            }

            if open_file.pending_bytes().saturating_add(data_len) > MAX_PENDING_WRITE_BYTES {
                if let Err(e) = open_file.flush_pending(&self.runtime) {
                    reply.error(error_to_errno(&e));
                    return;
                }
            }

            if let Err(errno) = open_file.buffer_write(offset as u64, data) {
                reply.error(errno);
                return;
            }

            if open_file.pending_bytes() > MAX_PENDING_WRITE_BYTES {
                open_file.flush_pending(&self.runtime)
            } else {
                Ok(())
            }
        };

        match result {
            Ok(()) => reply.written(data_len as u32),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Flushes buffered data to the backend storage.
    fn flush(&mut self, _req: &Request, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        tracing::debug!("FUSE::flush: fh={}", fh);
        let result = {
            let mut open_files = self.open_files.lock();
            let Some(open_file) = open_files.get_mut(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            open_file.flush_pending(&self.runtime)
        };

        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(error_to_errno(&e)),
        }
    }

    /// Synchronizes file data to persistent storage using the file handle.
    ///
    /// This now uses the file handle's fsync which knows which layer(s) the
    /// file exists in, avoiding errors when a file only exists in one layer.
    fn fsync(&mut self, _req: &Request, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
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
        &mut self,
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
        let result = {
            let mut open_files = self.open_files.lock();
            let Some(open_file) = open_files.get_mut(&fh) else {
                reply.error(libc::EBADF);
                return;
            };
            open_file.flush_pending(&self.runtime)
        };

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
    fn statfs(&mut self, _req: &Request, _ino: u64, reply: ReplyStatfs) {
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
    fn forget(&mut self, _req: &Request, ino: u64, nlookup: u64) {
        tracing::debug!("FUSE::forget: ino={}, nlookup={}", ino, nlookup);
        let fs = self.fs.clone();
        self.runtime.block_on(async move {
            fs.forget(ino as i64, nlookup).await;
        });
    }

    /// Batch forget multiple inodes at once.
    ///
    /// This is an optimization over calling forget() individually for each inode.
    fn batch_forget(&mut self, _req: &Request, nodes: &[fuse_forget_one]) {
        tracing::debug!("FUSE::batch_forget: {} nodes", nodes.len());
        let fs = self.fs.clone();
        let nodes_vec: Vec<(i64, u64)> =
            nodes.iter().map(|n| (n.nodeid as i64, n.nlookup)).collect();
        self.runtime.block_on(async move {
            for (ino, nlookup) in nodes_vec {
                fs.forget(ino, nlookup).await;
            }
        });
    }
}

impl AgentFSFuse {
    fn flush_pending_inode(&self, ino: u64) -> Result<(), SdkError> {
        self.flush_pending_inode_except(ino, 0)
    }

    fn flush_pending_inode_except(&self, ino: u64, except_fh: u64) -> Result<(), SdkError> {
        let mut open_files = self.open_files.lock();
        for (fh, open_file) in open_files.iter_mut() {
            if *fh == except_fh || open_file.ino != ino {
                continue;
            }
            open_file.flush_pending(&self.runtime)?;
        }
        Ok(())
    }

    fn flush_all_pending(&self) -> Result<(), SdkError> {
        let mut open_files = self.open_files.lock();
        for open_file in open_files.values_mut() {
            open_file.flush_pending(&self.runtime)?;
        }
        Ok(())
    }

    /// Create a new FUSE filesystem adapter wrapping a FileSystem instance.
    ///
    /// The provided Tokio runtime is used to execute async FileSystem operations
    /// from within synchronous FUSE callbacks via `block_on`.
    fn new(fs: Arc<dyn FileSystem>, runtime: Runtime) -> Self {
        Self {
            fs,
            runtime,
            open_files: Arc::new(Mutex::new(HashMap::new())),
            next_fh: AtomicU64::new(1),
            _profile_report: Arc::new(agentfs_sdk::profiling::ProfileReportGuard::new(
                "fuse_session",
            )),
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
    use super::WriteBuffer;

    fn ranges(buffer: &WriteBuffer) -> Vec<(u64, Vec<u8>)> {
        buffer.ranges_for_flush()
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
}
