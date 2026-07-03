//! Filesystem session
//!
//! A session runs a filesystem implementation while it is being mounted to a specific mount
//! point. A session begins by mounting the filesystem and ends by unmounting it. While the
//! filesystem is mounted, the session loop receives, dispatches and replies to kernel requests
//! for filesystem operations under its mount point.

use libc::{EAGAIN, EINTR, ENODEV, ENOENT};
use log::{debug, warn};
use nix::unistd::geteuid;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Instant;

use crate::fuse_config::{DispatchMode, UringConfig, FUSE_REQUEST_BUFFER_SIZE};

use std::sync::mpsc;

use super::deferred_notify::{DeferredNotifier, NotifyOp};
use super::ll::fuse_abi as abi;
use super::notify::Notifier;
use super::request::{AlignedRequestBuf, Request, ScheduleClass};
use super::Filesystem;
use super::MountOption;
use super::{channel::Channel, mnt::Mount};

/// The max size of write requests from the kernel. The absolute minimum is 4k,
/// FUSE recommends at least 128k, max 16M. Linux defaults to 128k.
pub const MAX_WRITE_SIZE: usize = 16 * 1024 * 1024;

/// Size of the buffer for reading a request from the kernel. Since the kernel may send
/// up to `MAX_WRITE_SIZE` bytes in a write request, we use that value plus some extra space.
const BUFFER_SIZE: usize = FUSE_REQUEST_BUFFER_SIZE;

#[derive(Default, Debug, Eq, PartialEq)]
/// How requests should be filtered based on the calling UID.
pub enum SessionACL {
    /// Allow requests from any user. Corresponds to the `allow_other` mount option.
    All,
    /// Allow requests from root. Corresponds to the `allow_root` mount option.
    RootAndOwner,
    /// Allow requests from the owning UID. This is FUSE's default mode of operation.
    #[default]
    Owner,
}

/// The session data structure
#[derive(Debug)]
pub struct Session<FS: Filesystem> {
    /// Shared session state and filesystem operation implementations.
    pub(crate) shared: Arc<SessionShared<FS>>,
    /// Communication channel to the kernel driver
    pub(crate) ch: Channel,
    /// Handle to the mount.  Dropping this unmounts.
    mount: Arc<Mutex<Option<(PathBuf, Mount)>>>,
    /// Sender half of the deferred notification queue
    notify_tx: Option<mpsc::Sender<NotifyOp>>,
    /// Receiver half — moved to the notify thread in run()
    notify_rx: Option<mpsc::Receiver<NotifyOp>>,
    /// Request dispatch mode parsed by the FUSE adapter config.
    dispatch_mode: DispatchMode,
    /// FUSE-over-io_uring settings parsed by the FUSE adapter config.
    #[cfg(target_os = "linux")]
    uring_config: UringConfig,
    /// Shutdown/join control for optional FUSE-over-io_uring queue threads.
    #[cfg(target_os = "linux")]
    uring_control: Arc<super::uring::UringQueueControl>,
}

#[derive(Debug)]
pub(crate) struct SessionShared<FS: Filesystem> {
    /// Filesystem operation implementations.
    pub(crate) filesystem: FS,
    /// Whether to restrict access to owner, root + owner, or unrestricted.
    /// Used to implement `allow_root` and `auto_unmount`.
    pub(crate) allowed: SessionACL,
    /// User that launched the fuser process.
    pub(crate) session_owner: u32,
    /// FUSE protocol major version.
    proto_major: AtomicU32,
    /// FUSE protocol minor version.
    proto_minor: AtomicU32,
    /// True if the filesystem is initialized (init operation done).
    initialized: AtomicBool,
    /// True if the filesystem was destroyed (destroy operation done).
    destroyed: AtomicBool,
}

impl<FS: Filesystem> SessionShared<FS> {
    fn new(filesystem: FS, allowed: SessionACL, session_owner: u32) -> Self {
        Self {
            filesystem,
            allowed,
            session_owner,
            proto_major: AtomicU32::new(0),
            proto_minor: AtomicU32::new(0),
            initialized: AtomicBool::new(false),
            destroyed: AtomicBool::new(false),
        }
    }

    pub(crate) fn set_proto_version(&self, major: u32, minor: u32) {
        self.proto_major.store(major, Ordering::Relaxed);
        self.proto_minor.store(minor, Ordering::Relaxed);
    }

    pub(crate) fn set_initialized(&self, initialized: bool) {
        self.initialized.store(initialized, Ordering::Release);
    }

    pub(crate) fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub(crate) fn set_destroyed(&self, destroyed: bool) {
        self.destroyed.store(destroyed, Ordering::Release);
    }

    pub(crate) fn is_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
struct QueuedRequest {
    request: Request,
    enqueued_at: Instant,
    class: ScheduleClass,
}

impl QueuedRequest {
    fn new(request: Request) -> Self {
        let class = request.schedule_class();
        Self {
            request,
            enqueued_at: Instant::now(),
            class,
        }
    }
}

struct ActiveDispatchGuard<'a> {
    active_dispatches: &'a AtomicU64,
}

impl Drop for ActiveDispatchGuard<'_> {
    fn drop(&mut self) {
        self.active_dispatches.fetch_sub(1, Ordering::AcqRel);
    }
}

fn dispatch_request<FS: Filesystem>(
    shared: &SessionShared<FS>,
    active_dispatches: &AtomicU64,
    request: Request,
) {
    let concurrent = active_dispatches.fetch_add(1, Ordering::AcqRel) + 1;
    agentfs_sdk::profiling::record_fuse_dispatch_concurrency(concurrent);
    let _guard = ActiveDispatchGuard { active_dispatches };
    request.dispatch(shared);
}

fn dispatch_queued_request<FS: Filesystem>(
    shared: &SessionShared<FS>,
    active_dispatches: &AtomicU64,
    queued: QueuedRequest,
) {
    agentfs_sdk::profiling::record_fuse_dispatch_parallel_task();
    agentfs_sdk::profiling::record_fuse_dispatch_wait(queued.enqueued_at.elapsed());
    dispatch_request(shared, active_dispatches, queued.request);
}

fn lane_for_class(class: ScheduleClass, lanes: usize) -> usize {
    if lanes == 1 {
        return 0;
    }
    match class {
        ScheduleClass::GlobalWrite => 0,
        ScheduleClass::Keyed(key) => {
            let mut hasher = DefaultHasher::new();
            key.hash(&mut hasher);
            (hasher.finish() as usize) % lanes
        }
    }
}

impl<FS: Filesystem> AsFd for Session<FS> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.ch.as_fd()
    }
}

impl<FS: Filesystem> Session<FS> {
    /// Create a new session by mounting the given filesystem to the given mountpoint
    /// # Errors
    /// Returns an error if the options are incorrect, or if the fuse device can't be mounted.
    pub fn new<P: AsRef<Path>>(
        filesystem: FS,
        mountpoint: P,
        options: &[MountOption],
        dispatch_mode: DispatchMode,
        uring_config: UringConfig,
    ) -> io::Result<Session<FS>> {
        let mountpoint = mountpoint.as_ref();
        debug!("Mounting {}", mountpoint.display());
        // If AutoUnmount is requested, but not AllowRoot or AllowOther we enforce the ACL
        // ourself and implicitly set AllowOther because fusermount needs allow_root or allow_other
        // to handle the auto_unmount option
        let (file, mount) = if options.contains(&MountOption::AutoUnmount)
            && !(options.contains(&MountOption::AllowRoot)
                || options.contains(&MountOption::AllowOther))
        {
            warn!(
                "Given auto_unmount without allow_root or allow_other; adding allow_other, with userspace permission handling"
            );
            let mut modified_options = options.to_vec();
            modified_options.push(MountOption::AllowOther);
            Mount::new(mountpoint, &modified_options)?
        } else {
            Mount::new(mountpoint, options)?
        };

        let ch = Channel::new(file);
        let allowed = if options.contains(&MountOption::AllowRoot) {
            SessionACL::RootAndOwner
        } else if options.contains(&MountOption::AllowOther) {
            SessionACL::All
        } else {
            SessionACL::Owner
        };

        let (notify_tx, notify_rx) = mpsc::channel();

        Ok(Session {
            shared: Arc::new(SessionShared::new(filesystem, allowed, geteuid().as_raw())),
            ch,
            mount: Arc::new(Mutex::new(Some((mountpoint.to_owned(), mount)))),
            notify_tx: Some(notify_tx),
            notify_rx: Some(notify_rx),
            dispatch_mode,
            #[cfg(target_os = "linux")]
            uring_config,
            #[cfg(target_os = "linux")]
            uring_control: super::uring::UringQueueControl::new(),
        })
    }

    /// Run the session loop that receives kernel requests and dispatches them to method
    /// calls into the filesystem. This read-dispatch-loop is non-concurrent to prevent
    /// having multiple buffers (which take up much memory), but the filesystem methods
    /// may run concurrent by spawning threads.
    /// # Errors
    /// Returns any final error when the session comes to an end.
    pub fn run(&mut self) -> io::Result<()> {
        let notify_rx = self.notify_rx.take().expect("run() called more than once");
        let notifier = self.notifier();
        let notify_handle = thread::spawn(move || {
            for op in notify_rx {
                let res = match op {
                    NotifyOp::InvalEntry { parent, ref name } => {
                        notifier.inval_entry(parent, name.as_os_str())
                    }
                    NotifyOp::InvalInode { ino, offset, len } => {
                        notifier.inval_inode(ino, offset, len)
                    }
                };
                if let Err(e) = res {
                    debug!("FUSE notify failed: {e}");
                }
            }
        });

        // A single DeferredNotifier shared by all requests in this session,
        // avoiding a Sender clone on every FUSE request dispatch.
        let deferred = Arc::new(DeferredNotifier::new(
            self.notify_tx.as_ref().expect("notify_tx missing").clone(),
        ));

        // Optional fuse-over-io_uring transport: per-CPU ring queues serve
        // regular requests; this legacy loop keeps running for INIT, FORGET,
        // INTERRUPT and as fallback when the kernel rejects ring setup.
        #[cfg(target_os = "linux")]
        if self.uring_config.enabled {
            super::uring::start_uring_queues(
                self.shared.clone(),
                deferred.clone(),
                self.ch.device(),
                self.uring_control.clone(),
                self.uring_config,
            );
        }

        let result = match self.dispatch_mode {
            DispatchMode::Serial => {
                tracing::info!("resolved FUSE dispatch mode: serial");
                agentfs_sdk::profiling::set_fuse_workers_configured(0);
                self.run_serial(deferred.clone())
            }
            DispatchMode::Parallel {
                workers,
                queue_capacity,
            } => {
                tracing::info!(
                    workers,
                    queue_capacity,
                    "resolved FUSE dispatch mode: parallel"
                );
                agentfs_sdk::profiling::set_fuse_workers_configured(workers as u64);
                self.run_parallel(deferred.clone(), workers, queue_capacity)
            }
        };

        #[cfg(target_os = "linux")]
        self.uring_control.shutdown_and_join();

        // Drop all senders to close the channel, then join the notify thread
        // to ensure in-flight invalidations are flushed before returning.
        drop(deferred);
        self.notify_tx.take();
        if let Err(e) = notify_handle.join() {
            warn!("notify thread panicked: {e:?}");
        }

        result
    }

    fn run_serial(&self, deferred: Arc<DeferredNotifier>) -> io::Result<()> {
        let shared = self.shared.clone();
        let active_dispatches = AtomicU64::new(0);

        self.read_requests(
            move |request| {
                dispatch_request(shared.as_ref(), &active_dispatches, request);
                Ok(())
            },
            deferred,
        )
    }

    fn run_parallel(
        &self,
        deferred: Arc<DeferredNotifier>,
        workers: usize,
        queue_capacity: usize,
    ) -> io::Result<()> {
        let mut lane_senders = Vec::with_capacity(workers);
        let mut lane_depths = Vec::with_capacity(workers);
        let queue_depth = Arc::new(AtomicU64::new(0));
        let active_dispatches = Arc::new(AtomicU64::new(0));
        let mut worker_handles = Vec::with_capacity(workers);

        for worker_id in 0..workers {
            let (tx, rx) = mpsc::sync_channel::<QueuedRequest>(queue_capacity);
            let lane_depth = Arc::new(AtomicU64::new(0));
            lane_senders.push(tx);
            lane_depths.push(lane_depth.clone());
            let shared = self.shared.clone();
            let queue_depth = queue_depth.clone();
            let active_dispatches = active_dispatches.clone();
            worker_handles.push(
                thread::Builder::new()
                    .name(format!("agentfs-fuse-worker-{worker_id}"))
                    .spawn(move || {
                        while let Ok(queued) = rx.recv() {
                            queue_depth.fetch_sub(1, Ordering::AcqRel);
                            lane_depth.fetch_sub(1, Ordering::AcqRel);
                            dispatch_queued_request(
                                shared.as_ref(),
                                active_dispatches.as_ref(),
                                queued,
                            );
                        }
                    })?,
            );
        }

        let read_result = self.read_requests(
            move |request| {
                let queued = QueuedRequest::new(request);
                let lane = lane_for_class(queued.class, lane_senders.len());
                let depth = queue_depth.fetch_add(1, Ordering::AcqRel) + 1;
                let lane_depth = lane_depths[lane].fetch_add(1, Ordering::AcqRel) + 1;
                match lane_senders[lane].try_send(queued) {
                    Ok(()) => {
                        agentfs_sdk::profiling::record_fuse_worker_queue_depth(depth);
                        Ok(())
                    }
                    Err(mpsc::TrySendError::Full(queued)) => {
                        agentfs_sdk::profiling::record_fuse_dispatch_inline_fallback();
                        lane_senders[lane].send(queued).map_err(|_| {
                            queue_depth.fetch_sub(1, Ordering::AcqRel);
                            lane_depths[lane].fetch_sub(1, Ordering::AcqRel);
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "FUSE dispatch worker queue disconnected",
                            )
                        })?;
                        agentfs_sdk::profiling::record_fuse_worker_queue_depth(depth);
                        agentfs_sdk::profiling::record_fuse_worker_queue_depth(lane_depth);
                        Ok(())
                    }
                    Err(mpsc::TrySendError::Disconnected(queued)) => {
                        queue_depth.fetch_sub(1, Ordering::AcqRel);
                        lane_depths[lane].fetch_sub(1, Ordering::AcqRel);
                        drop(queued);
                        agentfs_sdk::profiling::record_fuse_dispatch_inline_fallback();
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "FUSE dispatch worker queue disconnected",
                        ))
                    }
                }
            },
            deferred,
        );

        for handle in worker_handles {
            if let Err(e) = handle.join() {
                warn!("FUSE worker thread panicked: {e:?}");
            }
        }

        read_result
    }

    fn read_requests<F>(&self, mut dispatch: F, deferred: Arc<DeferredNotifier>) -> io::Result<()>
    where
        F: FnMut(Request) -> io::Result<()>,
    {
        // Buffer for receiving requests from the kernel. Only one is allocated and
        // it is reused immediately after dispatching to conserve memory and allocations.
        let mut buffer = vec![0; BUFFER_SIZE];
        let buf = aligned_sub_buf(&mut buffer, std::mem::align_of::<abi::fuse_in_header>());

        loop {
            // Read the next request from the given channel to kernel driver.
            // The kernel driver makes sure that we get exactly one request per read.
            match self.ch.receive(buf) {
                Ok(size) => {
                    let data = AlignedRequestBuf::copy_from(&buf[..size]);
                    match Request::new(self.ch.sender(), deferred.clone(), data) {
                        // Dispatch request.
                        Some(req) => dispatch(req)?,
                        // Quit loop on illegal request.
                        None => break,
                    }
                }
                Err(err) => match err.raw_os_error() {
                    Some(
                          ENOENT // Operation interrupted. Accordingly to FUSE, this is safe to retry
                        | EINTR // Interrupted system call, retry
                        | EAGAIN // Explicitly instructed to try again
                    ) => continue,
                    Some(ENODEV) => break,
                    // Unhandled error.
                    _ => return Err(err),
                },
            }
        }

        Ok(())
    }

    /// Returns a thread-safe object that can be used to unmount the Filesystem
    pub fn unmount_callable(&mut self) -> SessionUnmounter {
        SessionUnmounter {
            mount: self.mount.clone(),
            device: self.ch.device(),
            #[cfg(target_os = "linux")]
            uring_control: self.uring_control.clone(),
        }
    }

    /// Returns an object that can be used to send notifications to the kernel
    pub fn notifier(&self) -> Notifier {
        Notifier::new(self.ch.sender())
    }
}

#[derive(Debug)]
/// A thread-safe object that can be used to unmount a Filesystem
pub struct SessionUnmounter {
    mount: Arc<Mutex<Option<(PathBuf, Mount)>>>,
    device: Arc<std::fs::File>,
    #[cfg(target_os = "linux")]
    uring_control: Arc<super::uring::UringQueueControl>,
}

impl SessionUnmounter {
    /// Unmount the filesystem
    pub fn unmount(&mut self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        self.uring_control.shutdown_and_join();
        #[cfg(target_os = "linux")]
        if let Err(err) = abort_fuse_connection(&self.device) {
            debug!("failed to abort FUSE connection during unmount: {err}");
        }
        drop(std::mem::take(&mut *self.mount.lock().unwrap()));
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn abort_fuse_connection(device: &std::fs::File) -> io::Result<()> {
    let fdinfo_path = format!("/proc/self/fdinfo/{}", device.as_raw_fd());
    let fdinfo = std::fs::read_to_string(fdinfo_path)?;
    let Some(connection_id) = fdinfo.lines().find_map(|line| {
        line.strip_prefix("fuse_connection:")
            .and_then(|value| value.split_whitespace().next())
    }) else {
        return Ok(());
    };
    let abort_path = format!("/sys/fs/fuse/connections/{connection_id}/abort");
    match std::fs::write(&abort_path, b"1\n") {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn aligned_sub_buf(buf: &mut [u8], alignment: usize) -> &mut [u8] {
    let off = alignment - (buf.as_ptr() as usize) % alignment;
    if off == alignment {
        buf
    } else {
        &mut buf[off..]
    }
}

impl<FS: Filesystem> Drop for Session<FS> {
    fn drop(&mut self) {
        if !self.shared.is_destroyed() {
            self.shared.filesystem.destroy();
            self.shared.set_destroyed(true);
        }

        if let Some((mountpoint, _mount)) = std::mem::take(&mut *self.mount.lock().unwrap()) {
            debug!("unmounting session at {}", mountpoint.display());
        }
    }
}
