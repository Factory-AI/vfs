//! FUSE-over-io_uring transport (kernel 6.14+, `CONFIG_FUSE_IO_URING`).
//!
//! Replaces the read(2)/writev(2) round trip on /dev/fuse with per-CPU
//! io_uring queues: the daemon parks `FUSE_IO_URING_CMD_REGISTER` /
//! `COMMIT_AND_FETCH` uring_cmd SQEs in the kernel; a fuse request completes
//! a CQE with the request copied into pre-registered userspace buffers, and
//! the reply is committed (and the next request fetched) with a single SQE,
//! removing the syscall ping-pong and wakeup latency of the legacy channel.
//!
//! Scope (mirrors the kernel contract in fs/fuse/dev_uring.c):
//! - FORGET / INTERRUPT / notifications stay on the legacy /dev/fuse channel
//!   (`fuse_io_uring_ops.send_forget = fuse_dev_queue_forget`), so the
//!   classic session read loop keeps running alongside the rings.
//! - One queue per possible CPU is mandatory: the kernel routes each request
//!   to `task_cpu(current)` and WARNs if that queue is missing.
//! - REGISTER returns -EAGAIN until the kernel has processed our INIT reply;
//!   registration is retried. On persistent failure the kernel clears
//!   `fc->io_uring` and falls back to the legacy channel by itself.
//!
//! Requests are dispatched inline on the owning queue thread and handlers
//! reply synchronously, so each ring's submission queue is effectively
//! single-threaded (guarded by a mutex that is never contended in practice).
//!
//! Opt-in via `AGENTFS_FUSE_URING=1`; depth per queue via
//! `AGENTFS_FUSE_URING_DEPTH` (default 4).

#![cfg(target_os = "linux")]

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, error, warn};

use super::channel::ChannelSender;
use super::deferred_notify::DeferredNotifier;
use super::request::{AlignedRequestBuf, Request};
use super::session::SessionShared;
use super::Filesystem;

// ─── io_uring ABI ────────────────────────────────────────────────────────────

const SYS_IO_URING_SETUP: libc::c_long = 425;
const SYS_IO_URING_ENTER: libc::c_long = 426;

const IORING_SETUP_CQSIZE: u32 = 1 << 3;
const IORING_SETUP_SQE128: u32 = 1 << 10;

const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;

const IORING_ENTER_GETEVENTS: u32 = 1;

const IORING_OP_URING_CMD: u8 = 46;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoSqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoCqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: IoSqringOffsets,
    cq_off: IoCqringOffsets,
}

// ─── fuse-over-io_uring ABI (include/uapi/linux/fuse.h) ─────────────────────

const FUSE_IO_URING_CMD_REGISTER: u32 = 1;
const FUSE_IO_URING_CMD_COMMIT_AND_FETCH: u32 = 2;

/// `fuse_uring_req_header` layout: 128B in_out (fuse_in/out_header), 128B
/// op_in (first request argument), 32B `fuse_uring_ent_in_out`.
const HDR_IN_OUT_OFFSET: usize = 0;
const HDR_OP_IN_OFFSET: usize = 128;
const HDR_ENT_OFFSET: usize = 256;
const HDR_STRUCT_SIZE: usize = 288;
/// The kernel copies the first request argument into the 128-byte op_in area
/// without bounds-checking against it (names up to 255 bytes overflow into
/// and past `ent_in_out`). Oversize the header buffer so the overflow stays
/// inside our allocation, and detect/reject such requests on parse.
const HDR_BUF_SIZE: usize = 1024;

const ENT_COMMIT_ID_OFFSET: usize = HDR_ENT_OFFSET + 8;
const ENT_PAYLOAD_SZ_OFFSET: usize = HDR_ENT_OFFSET + 16;

const FUSE_IN_HEADER_SIZE: usize = 40;
const FUSE_OUT_HEADER_SIZE: usize = 16;
const MAX_OP_IN_SIZE: usize = 128;

/// Our INIT reply clamps max_write/max_readahead to 1 MiB when uring is on,
/// and the kernel clamps max_pages to 256 (1 MiB), so its required payload
/// size is exactly max(8K, 1M, 1M). Allocate with one page of slack.
pub(crate) const URING_MAX_WRITE: u32 = 1 << 20;
const PAYLOAD_BUF_SIZE: usize = (URING_MAX_WRITE as usize) + 4096;

// ─── configuration ──────────────────────────────────────────────────────────

pub(crate) fn uring_enabled() -> bool {
    matches!(
        std::env::var("AGENTFS_FUSE_URING").as_deref(),
        Ok("1") | Ok("true") | Ok("on")
    )
}

fn uring_queue_depth() -> usize {
    std::env::var("AGENTFS_FUSE_URING_DEPTH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|d| (1..=64).contains(d))
        .unwrap_or(4)
}

/// Busy-poll the completion queue for this long before blocking in
/// io_uring_enter, trading idle CPU for wakeup latency on request bursts.
/// Default 0 (no spin).
fn uring_spin_us() -> u64 {
    std::env::var("AGENTFS_FUSE_URING_SPIN_US")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|us| *us <= 1000)
        .unwrap_or(0)
}

/// One queue per possible CPU: the kernel sizes its queue array with
/// `num_possible_cpus()` and routes requests by `task_cpu(current)`.
fn possible_cpus() -> usize {
    let fallback = || {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
    };
    let Ok(s) = std::fs::read_to_string("/sys/devices/system/cpu/possible") else {
        return fallback();
    };
    // Format: "0-13" or "0".
    s.trim()
        .rsplit(['-', ','])
        .next()
        .and_then(|last| last.parse::<usize>().ok())
        .map(|last| last + 1)
        .unwrap_or_else(fallback)
}

/// Cheap capability probe so INIT never advertises FUSE_OVER_IO_URING on a
/// host where ring creation would fail afterwards (e.g. io_uring_disabled
/// sysctl): advertising and then failing to register would stall the mount
/// until the kernel-side EAGAIN/abort path recovers it.
pub(crate) fn probe_ring_setup() -> bool {
    let mut params = IoUringParams {
        flags: IORING_SETUP_SQE128,
        ..Default::default()
    };
    let fd = unsafe { libc::syscall(SYS_IO_URING_SETUP, 4u32, &mut params as *mut _) };
    if fd < 0 {
        return false;
    }
    let ok = params.features & IORING_FEAT_SINGLE_MMAP != 0;
    unsafe { libc::close(fd as RawFd) };
    ok
}

// ─── raw ring ────────────────────────────────────────────────────────────────

struct RawRing {
    fd: OwnedFd,
    sq_ring_ptr: *mut u8,
    sq_ring_size: usize,
    sqes_ptr: *mut u8,
    sqes_size: usize,
    sq_khead: *const AtomicU32,
    sq_ktail: *const AtomicU32,
    sq_mask: u32,
    sq_array: *mut u32,
    cq_khead: *const AtomicU32,
    cq_ktail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const u8,
    local_sq_tail: u32,
}

// All pointers reference the kernel-shared ring mappings, which live until
// drop; cross-thread access is serialized by the owning mutex.
unsafe impl Send for RawRing {}

#[derive(Debug, Clone, Copy)]
struct Cqe {
    user_data: u64,
    res: i32,
}

impl RawRing {
    fn new(entries: u32) -> io::Result<RawRing> {
        let mut params = IoUringParams {
            flags: IORING_SETUP_SQE128 | IORING_SETUP_CQSIZE,
            cq_entries: entries * 2,
            ..Default::default()
        };
        let raw = unsafe { libc::syscall(SYS_IO_URING_SETUP, entries, &mut params as *mut _) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw as RawFd) };
        if params.features & IORING_FEAT_SINGLE_MMAP == 0 {
            return Err(io::Error::other("io_uring lacks IORING_FEAT_SINGLE_MMAP"));
        }

        let sq_size = params.sq_off.array as usize + params.sq_entries as usize * 4;
        let cq_size = params.cq_off.cqes as usize + params.cq_entries as usize * 16;
        let ring_size = sq_size.max(cq_size);
        let sq_ring_ptr = mmap_ring(&fd, ring_size, IORING_OFF_SQ_RING)?;
        let sqes_size = params.sq_entries as usize * 128;
        let sqes_ptr = match mmap_ring(&fd, sqes_size, IORING_OFF_SQES) {
            Ok(ptr) => ptr,
            Err(e) => {
                unsafe { libc::munmap(sq_ring_ptr.cast(), ring_size) };
                return Err(e);
            }
        };

        let at = |off: u32| unsafe { sq_ring_ptr.add(off as usize) };
        let ring = RawRing {
            sq_khead: at(params.sq_off.head).cast::<AtomicU32>(),
            sq_ktail: at(params.sq_off.tail).cast::<AtomicU32>(),
            sq_mask: unsafe { *at(params.sq_off.ring_mask).cast::<u32>() },
            sq_array: at(params.sq_off.array).cast::<u32>(),
            cq_khead: at(params.cq_off.head).cast::<AtomicU32>(),
            cq_ktail: at(params.cq_off.tail).cast::<AtomicU32>(),
            cq_mask: unsafe { *at(params.cq_off.ring_mask).cast::<u32>() },
            cqes: at(params.cq_off.cqes),
            local_sq_tail: 0,
            fd,
            sq_ring_ptr,
            sq_ring_size: ring_size,
            sqes_ptr,
            sqes_size,
        };
        Ok(ring)
    }

    fn push_sqe(&mut self, sqe: &[u8; 128]) {
        let slot = self.local_sq_tail & self.sq_mask;
        unsafe {
            std::ptr::copy_nonoverlapping(
                sqe.as_ptr(),
                self.sqes_ptr.add(slot as usize * 128),
                128,
            );
            *self.sq_array.add(slot as usize) = slot;
        }
        self.local_sq_tail = self.local_sq_tail.wrapping_add(1);
        unsafe { (*self.sq_ktail).store(self.local_sq_tail, Ordering::Release) };
    }

    fn cq_ready(&self) -> bool {
        let head = unsafe { (*self.cq_khead).load(Ordering::Relaxed) };
        let tail = unsafe { (*self.cq_ktail).load(Ordering::Acquire) };
        head != tail
    }

    fn pop_cqe(&mut self) -> Option<Cqe> {
        let head = unsafe { (*self.cq_khead).load(Ordering::Relaxed) };
        let tail = unsafe { (*self.cq_ktail).load(Ordering::Acquire) };
        if head == tail {
            return None;
        }
        let idx = (head & self.cq_mask) as usize;
        let cqe = unsafe {
            let base = self.cqes.add(idx * 16);
            Cqe {
                user_data: std::ptr::read(base.cast::<u64>()),
                res: std::ptr::read(base.add(8).cast::<i32>()),
            }
        };
        unsafe { (*self.cq_khead).store(head.wrapping_add(1), Ordering::Release) };
        Some(cqe)
    }
}

impl Drop for RawRing {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.sqes_ptr.cast(), self.sqes_size);
            libc::munmap(self.sq_ring_ptr.cast(), self.sq_ring_size);
        }
    }
}

fn mmap_ring(fd: &OwnedFd, size: usize, offset: i64) -> io::Result<*mut u8> {
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            fd.as_raw_fd(),
            offset,
        )
    };
    if ptr == libc::MAP_FAILED {
        Err(io::Error::last_os_error())
    } else {
        Ok(ptr.cast())
    }
}

fn enter(ring_fd: RawFd, to_submit: u32, min_complete: u32) -> io::Result<()> {
    loop {
        let rc = unsafe {
            libc::syscall(
                SYS_IO_URING_ENTER,
                ring_fd,
                to_submit,
                min_complete,
                IORING_ENTER_GETEVENTS,
                std::ptr::null::<libc::c_void>(),
                0usize,
            )
        };
        if rc >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(err);
    }
}

/// Build the 128-byte uring_cmd SQE carrying `struct fuse_uring_cmd_req`.
fn build_cmd_sqe(
    dev_fd: RawFd,
    cmd_op: u32,
    user_data: u64,
    addr: u64,
    len: u32,
    qid: u16,
    commit_id: u64,
) -> [u8; 128] {
    let mut sqe = [0u8; 128];
    sqe[0] = IORING_OP_URING_CMD;
    sqe[4..8].copy_from_slice(&dev_fd.to_le_bytes());
    sqe[8..12].copy_from_slice(&cmd_op.to_le_bytes());
    sqe[16..24].copy_from_slice(&addr.to_le_bytes());
    sqe[24..28].copy_from_slice(&len.to_le_bytes());
    sqe[32..40].copy_from_slice(&user_data.to_le_bytes());
    // struct fuse_uring_cmd_req in the 80-byte SQE128 command area.
    sqe[48..56].copy_from_slice(&0u64.to_le_bytes()); // flags
    sqe[56..64].copy_from_slice(&commit_id.to_le_bytes());
    sqe[64..66].copy_from_slice(&qid.to_le_bytes());
    sqe
}

// ─── queue state ─────────────────────────────────────────────────────────────

struct EntryBufs {
    header: Box<[u8]>,
    payload: Box<[u8]>,
    /// REGISTER passes `[header, payload]` via sqe->addr as a
    /// `struct iovec[2]`; stored as raw words (`{base, len} x 2`, identical
    /// layout on LP64) so the type stays `Send`. The kernel snapshots the
    /// addresses at REGISTER, but keep the array alive for the queue's
    /// lifetime anyway.
    iov_words: Box<[u64; 4]>,
}

pub(crate) struct QueueShared {
    qid: u16,
    dev_fd: RawFd,
    ring: Mutex<RawRing>,
    entries: Vec<Mutex<EntryBufs>>,
    pending_submit: AtomicU32,
    /// Keeps the /dev/fuse fd (and thus `dev_fd`) alive for the queue's
    /// lifetime; also the target for notification sends.
    device: Arc<File>,
}

impl std::fmt::Debug for QueueShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueShared")
            .field("qid", &self.qid)
            .finish()
    }
}

/// Per-request reply target: commits the reply into the ring entry's buffers
/// and queues a COMMIT_AND_FETCH SQE. Replies happen inline on the queue
/// thread (handlers are synchronous), so the submission mutex is uncontended;
/// the next loop iteration submits it.
#[derive(Debug, Clone)]
pub struct UringSender {
    queue: Arc<QueueShared>,
    slot: usize,
    commit_id: u64,
    sent: Arc<AtomicBool>,
}

impl UringSender {
    pub(crate) fn device(&self) -> Arc<File> {
        self.queue.device.clone()
    }

    pub(crate) fn send_reply(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        if self.sent.swap(true, Ordering::AcqRel) {
            return Err(io::Error::other("duplicate uring reply"));
        }
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        if total < FUSE_OUT_HEADER_SIZE {
            return Err(io::Error::other("uring reply shorter than fuse_out_header"));
        }
        let payload_len = total - FUSE_OUT_HEADER_SIZE;

        {
            let mut ent = self.queue.entries[self.slot].lock().unwrap();
            if payload_len > ent.payload.len() {
                self.sent.store(false, Ordering::Release);
                return Err(io::Error::other("uring reply exceeds payload buffer"));
            }
            // Gather: first 16 bytes into in_out, the rest into the payload
            // buffer (slices may split anywhere).
            let mut copied = 0usize;
            for buf in bufs {
                let mut chunk: &[u8] = buf;
                while !chunk.is_empty() {
                    if copied < FUSE_OUT_HEADER_SIZE {
                        let n = chunk.len().min(FUSE_OUT_HEADER_SIZE - copied);
                        ent.header[HDR_IN_OUT_OFFSET + copied..HDR_IN_OUT_OFFSET + copied + n]
                            .copy_from_slice(&chunk[..n]);
                        chunk = &chunk[n..];
                        copied += n;
                    } else {
                        let off = copied - FUSE_OUT_HEADER_SIZE;
                        ent.payload[off..off + chunk.len()].copy_from_slice(chunk);
                        copied += chunk.len();
                        chunk = &[];
                    }
                }
            }
            ent.header[HDR_ENT_OFFSET..HDR_ENT_OFFSET + 8].fill(0); // flags
            let sz = (payload_len as u32).to_le_bytes();
            ent.header[ENT_PAYLOAD_SZ_OFFSET..ENT_PAYLOAD_SZ_OFFSET + 4].copy_from_slice(&sz);
        }

        let sqe = build_cmd_sqe(
            self.queue.dev_fd,
            FUSE_IO_URING_CMD_COMMIT_AND_FETCH,
            self.slot as u64,
            0,
            0,
            self.queue.qid,
            self.commit_id,
        );
        self.queue.ring.lock().unwrap().push_sqe(&sqe);
        self.queue.pending_submit.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

// ─── session integration ─────────────────────────────────────────────────────

/// Spawn the uring transport for an initialized session. Returns immediately;
/// a starter thread waits for FUSE_INIT to complete, then brings up one queue
/// thread per possible CPU. All failures degrade to the legacy channel (the
/// kernel clears `fc->io_uring` when a REGISTER fails).
pub(crate) fn start_uring_queues<FS: Filesystem + Send + Sync + 'static>(
    shared: Arc<SessionShared<FS>>,
    deferred: Arc<DeferredNotifier>,
    device: Arc<File>,
) {
    let depth = uring_queue_depth();
    let nr_queues = possible_cpus();
    let active_dispatches = Arc::new(AtomicU64::new(0));
    let starter = move || {
        // REGISTER needs the kernel-side fc->initialized; our INIT reply also
        // races the kernel's processing of it, so the per-queue registration
        // loop additionally retries on EAGAIN.
        let wait_start = std::time::Instant::now();
        while !shared.is_initialized() {
            if wait_start.elapsed() > Duration::from_secs(30) {
                warn!("fuse-uring: session not initialized after 30s; not starting rings");
                return;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        tracing::info!(nr_queues, depth, "starting fuse-over-io_uring queues");
        for qid in 0..nr_queues {
            let shared = shared.clone();
            let deferred = deferred.clone();
            let device = device.clone();
            let active_dispatches = active_dispatches.clone();
            if let Err(e) = std::thread::Builder::new()
                .name(format!("agentfs-fuse-uring-{qid}"))
                .spawn(move || {
                    queue_thread(
                        qid as u16,
                        depth,
                        shared,
                        deferred,
                        device,
                        active_dispatches,
                    )
                })
            {
                error!("fuse-uring: failed to spawn queue thread {qid}: {e}");
                return;
            }
        }
    };
    if let Err(e) = std::thread::Builder::new()
        .name("agentfs-fuse-uring-start".into())
        .spawn(starter)
    {
        error!("fuse-uring: failed to spawn starter thread: {e}");
    }
}

fn queue_thread<FS: Filesystem>(
    qid: u16,
    depth: usize,
    shared: Arc<SessionShared<FS>>,
    deferred: Arc<DeferredNotifier>,
    device: Arc<File>,
    active_dispatches: Arc<AtomicU64>,
) {
    let ring = match RawRing::new((depth + 1) as u32) {
        Ok(ring) => ring,
        Err(e) => {
            error!("fuse-uring: ring setup failed for qid={qid}: {e}");
            return;
        }
    };

    let mut entries = Vec::with_capacity(depth);
    for _ in 0..depth {
        let header = vec![0u8; HDR_BUF_SIZE].into_boxed_slice();
        let payload = vec![0u8; PAYLOAD_BUF_SIZE].into_boxed_slice();
        let iov_words = Box::new([
            header.as_ptr() as u64,
            HDR_BUF_SIZE as u64,
            payload.as_ptr() as u64,
            PAYLOAD_BUF_SIZE as u64,
        ]);
        entries.push(Mutex::new(EntryBufs {
            header,
            payload,
            iov_words,
        }));
    }

    let dev_fd = device.as_raw_fd();
    let queue = Arc::new(QueueShared {
        qid,
        dev_fd,
        ring: Mutex::new(ring),
        entries,
        pending_submit: AtomicU32::new(0),
        device,
    });

    let register_sqe = |slot: usize| {
        let ent = queue.entries[slot].lock().unwrap();
        build_cmd_sqe(
            dev_fd,
            FUSE_IO_URING_CMD_REGISTER,
            slot as u64,
            ent.iov_words.as_ptr() as u64,
            2,
            qid,
            0,
        )
    };

    {
        let mut ring = queue.ring.lock().unwrap();
        for slot in 0..depth {
            let sqe = register_sqe(slot);
            ring.push_sqe(&sqe);
        }
    }
    let mut to_submit = depth as u32;
    let mut dead = 0usize;
    let mut register_retries = 0u32;
    let ring_fd = queue.ring.lock().unwrap().fd.as_raw_fd();
    let spin = Duration::from_micros(uring_spin_us());

    loop {
        // Submit pending SQEs immediately, then optionally busy-poll the CQ
        // before blocking: the wakeup from a blocking enter costs more than
        // a typical request inter-arrival gap on hot paths.
        if !spin.is_zero() {
            if let Err(e) = enter(ring_fd, to_submit, 0) {
                error!("fuse-uring: io_uring_enter failed on qid={qid}: {e}");
                return;
            }
            let spin_start = std::time::Instant::now();
            let mut ready = false;
            while spin_start.elapsed() < spin {
                if queue.ring.lock().unwrap().cq_ready() {
                    ready = true;
                    break;
                }
                std::hint::spin_loop();
            }
            if !ready {
                if let Err(e) = enter(ring_fd, 0, 1) {
                    error!("fuse-uring: io_uring_enter failed on qid={qid}: {e}");
                    return;
                }
            }
        } else if let Err(e) = enter(ring_fd, to_submit, 1) {
            error!("fuse-uring: io_uring_enter failed on qid={qid}: {e}");
            return;
        }
        loop {
            let cqe = queue.ring.lock().unwrap().pop_cqe();
            let Some(cqe) = cqe else { break };
            let slot = cqe.user_data as usize;
            if cqe.res < 0 {
                match -cqe.res {
                    libc::EAGAIN if register_retries < 10_000 => {
                        // Kernel hasn't processed our INIT reply yet.
                        register_retries += 1;
                        std::thread::sleep(Duration::from_millis(1));
                        let sqe = register_sqe(slot);
                        queue.ring.lock().unwrap().push_sqe(&sqe);
                        queue.pending_submit.fetch_add(1, Ordering::AcqRel);
                    }
                    libc::EOPNOTSUPP => {
                        debug!("fuse-uring: not supported for this connection (qid={qid})");
                        return;
                    }
                    _ => {
                        // ENOTCONN/ECANCELED on teardown, or a fatal error.
                        dead += 1;
                        if dead == depth {
                            debug!("fuse-uring: queue {qid} drained; exiting");
                            return;
                        }
                    }
                }
                continue;
            }
            handle_request(&queue, slot, &shared, &deferred, &active_dispatches);
        }
        to_submit = queue.pending_submit.swap(0, Ordering::AcqRel);
    }
}

/// Reassemble the classic contiguous /dev/fuse request layout
/// (`[fuse_in_header][op header][remaining args]`) from the split uring
/// buffers and run it through the regular dispatch path.
fn handle_request<FS: Filesystem>(
    queue: &Arc<QueueShared>,
    slot: usize,
    shared: &Arc<SessionShared<FS>>,
    deferred: &Arc<DeferredNotifier>,
    active_dispatches: &AtomicU64,
) {
    let (data, commit_id, unique) = {
        let ent = queue.entries[slot].lock().unwrap();
        let header = &ent.header;
        let read_u32 = |off: usize| u32::from_le_bytes(header[off..off + 4].try_into().unwrap());
        let read_u64 = |off: usize| u64::from_le_bytes(header[off..off + 8].try_into().unwrap());

        let total_len = read_u32(HDR_IN_OUT_OFFSET) as usize;
        let unique = read_u64(HDR_IN_OUT_OFFSET + 8);
        let commit_id = read_u64(ENT_COMMIT_ID_OFFSET);
        let payload_sz = read_u32(ENT_PAYLOAD_SZ_OFFSET) as usize;

        let op_in_len = total_len
            .checked_sub(FUSE_IN_HEADER_SIZE + payload_sz)
            .filter(|len| *len <= MAX_OP_IN_SIZE)
            .filter(|_| payload_sz <= ent.payload.len());
        let Some(op_in_len) = op_in_len else {
            warn!(
                "fuse-uring: malformed request on qid={} slot={slot}: len={total_len} payload={payload_sz}",
                queue.qid
            );
            drop(ent);
            reply_error_raw(queue, slot, commit_id, unique, libc::EIO);
            return;
        };

        let mut buf = AlignedRequestBuf::with_capacity(total_len);
        {
            let dst = buf.as_mut_slice();
            dst[..FUSE_IN_HEADER_SIZE].copy_from_slice(&header[..FUSE_IN_HEADER_SIZE]);
            dst[FUSE_IN_HEADER_SIZE..FUSE_IN_HEADER_SIZE + op_in_len]
                .copy_from_slice(&header[HDR_OP_IN_OFFSET..HDR_OP_IN_OFFSET + op_in_len]);
            dst[FUSE_IN_HEADER_SIZE + op_in_len..total_len]
                .copy_from_slice(&ent.payload[..payload_sz]);
        }
        buf.set_len(total_len);
        (buf, commit_id, unique)
    };

    agentfs_sdk::profiling::record_fuse_uring_request();

    let sender = ChannelSender::Uring(UringSender {
        queue: queue.clone(),
        slot,
        commit_id,
        sent: Arc::new(AtomicBool::new(false)),
    });
    match Request::new(sender.clone(), deferred.clone(), data) {
        Some(request) => {
            // Mirror the legacy worker pool's concurrency accounting so the
            // serialization gates observe uring-side parallelism too.
            let concurrent = active_dispatches.fetch_add(1, Ordering::AcqRel) + 1;
            agentfs_sdk::profiling::record_fuse_dispatch_concurrency(concurrent);
            request.dispatch(shared);
            active_dispatches.fetch_sub(1, Ordering::AcqRel);
            // Every op the kernel routes through uring expects a reply
            // (FORGET/INTERRUPT stay on the legacy channel). If dispatch did
            // not reply (parse error path), commit an error so the slot
            // recycles instead of leaking.
            if let ChannelSender::Uring(uring) = &sender {
                if !uring.sent.load(Ordering::Acquire) {
                    reply_error_raw(queue, slot, commit_id, unique, libc::EIO);
                }
            }
        }
        None => reply_error_raw(queue, slot, commit_id, unique, libc::EIO),
    }
}

fn reply_error_raw(queue: &Arc<QueueShared>, slot: usize, commit_id: u64, unique: u64, errno: i32) {
    let mut out = [0u8; FUSE_OUT_HEADER_SIZE];
    out[..4].copy_from_slice(&(FUSE_OUT_HEADER_SIZE as u32).to_le_bytes());
    out[4..8].copy_from_slice(&(-errno).to_le_bytes());
    out[8..16].copy_from_slice(&unique.to_le_bytes());
    let sender = UringSender {
        queue: queue.clone(),
        slot,
        commit_id,
        sent: Arc::new(AtomicBool::new(false)),
    };
    if let Err(e) = sender.send_reply(&[io::IoSlice::new(&out)]) {
        error!("fuse-uring: failed to commit error reply: {e}");
    }
}
