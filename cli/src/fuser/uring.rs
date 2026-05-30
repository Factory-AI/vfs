//! FUSE-over-io-uring transport (Linux, ABI 7.42+).
//!
//! After the classic `/dev/fuse` INIT handshake negotiates `FUSE_OVER_IO_URING`,
//! this module brings up one io_uring per CPU core (the kernel routes each
//! request to the queue of the CPU it originates on). Each queue registers a
//! fixed set of entries; every entry owns a page-aligned `fuse_uring_req_header`
//! buffer (the kernel writes the request header / we write the reply header
//! into it) and a payload buffer (variable request/reply data). The protocol is
//! a 2-phase ring command:
//!
//!   * `FUSE_IO_URING_CMD_REGISTER` — hands an entry's buffers to the kernel and
//!     fetches the first request into it.
//!   * `FUSE_IO_URING_CMD_COMMIT_AND_FETCH` — commits the reply we wrote into the
//!     entry buffers and fetches the next request into the same entry.
//!
//! Each queue runs on its own CPU-pinned thread and dispatches requests inline
//! (matching libfuse's per-core model); parallelism comes from having a queue
//! per core. Reply-less operations (FORGET, interrupts, notifications) are not
//! delivered over io_uring — they stay on `/dev/fuse`, so the classic read loop
//! must keep running alongside this transport.
//!
//! This is an opt-in spike, gated behind `AGENTFS_FUSE_TRANSPORT=uring`.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use io_uring::{cqueue, opcode, squeue, types, IoUring};
use zerocopy::IntoBytes;

use super::channel::{ChannelSender, UringReplySender};
use super::deferred_notify::DeferredNotifier;
use super::ll::fuse_abi as abi;
use super::request::{AlignedRequestBuf, Request};
use super::session::SessionShared;
use super::Filesystem;

/// Page-aligned size of each entry's `fuse_uring_req_header` buffer. The uapi
/// header is 288 bytes; libfuse rounds the per-entry header allocation up to a
/// page, and so do we (page pinning requires page alignment).
const HEADER_BUF_SZ: usize = 4096;

/// Default entries per queue. Small by design: memory is `nr_queues * depth *
/// payload_cap`, and parallelism already comes from one queue per core.
const DEFAULT_QUEUE_DEPTH: usize = 2;

/// Per-entry payload buffer cap when io_uring is active. The INIT handshake caps
/// `max_write` to this so the kernel never sends a write larger than the buffer.
pub(crate) const URING_MAX_WRITE: u32 = 1 << 20;

const FUSE_IN_HEADER_SZ: usize = 40;
const OP_IN_OFFSET: usize = abi::FUSE_URING_IN_OUT_HEADER_SZ; // 128
const RING_ENT_OFFSET: usize = abi::FUSE_URING_IN_OUT_HEADER_SZ + abi::FUSE_URING_OP_IN_OUT_SZ; // 256
const COMMIT_ID_OFFSET: usize = RING_ENT_OFFSET + 8; // flags(8) -> commit_id
const PAYLOAD_SZ_OFFSET: usize = RING_ENT_OFFSET + 16; // flags(8)+commit_id(8) -> payload_sz

/// Whether the io_uring transport was requested via the environment.
pub(crate) fn uring_requested() -> bool {
    std::env::var("AGENTFS_FUSE_TRANSPORT")
        .map(|v| v.eq_ignore_ascii_case("uring"))
        .unwrap_or(false)
}

fn queue_depth() -> usize {
    std::env::var("AGENTFS_FUSE_URING_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(DEFAULT_QUEUE_DEPTH)
}

fn num_queues() -> usize {
    // The kernel allocates one queue per possible CPU and routes by CPU, so a
    // queue must exist for every core that can issue a request.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
    if n > 0 {
        n as usize
    } else {
        thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1)
    }
}

/// Owns the per-entry buffers handed to the kernel for one ring entry.
struct EntryBuf {
    header: *mut u8,
    header_layout: Layout,
    payload: *mut u8,
    payload_layout: Layout,
    payload_cap: usize,
    iov: Box<[libc::iovec; 2]>,
}

impl EntryBuf {
    fn new(payload_cap: usize) -> io::Result<Self> {
        let page = page_size::get();
        let header_layout = Layout::from_size_align(HEADER_BUF_SZ, page)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let payload_layout = Layout::from_size_align(payload_cap.max(page), page)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        // SAFETY: non-zero layouts; null is checked below.
        let header = unsafe { alloc_zeroed(header_layout) };
        let payload = unsafe { alloc_zeroed(payload_layout) };
        if header.is_null() || payload.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "failed to allocate io_uring entry buffers",
            ));
        }
        let iov = Box::new([
            libc::iovec {
                iov_base: header as *mut libc::c_void,
                iov_len: HEADER_BUF_SZ,
            },
            libc::iovec {
                iov_base: payload as *mut libc::c_void,
                iov_len: payload_cap,
            },
        ]);
        Ok(Self {
            header,
            header_layout,
            payload,
            payload_layout,
            payload_cap,
            iov,
        })
    }

    fn reply_sender(&self, device: Arc<File>) -> UringReplySender {
        UringReplySender {
            device,
            header: self.header,
            payload: self.payload,
            payload_cap: self.payload_cap,
        }
    }
}

impl Drop for EntryBuf {
    fn drop(&mut self) {
        // SAFETY: allocated by `alloc_zeroed` with these exact layouts.
        unsafe {
            dealloc(self.header, self.header_layout);
            dealloc(self.payload, self.payload_layout);
        }
    }
}

fn cmd_bytes(qid: u16, commit_id: u64) -> [u8; 80] {
    let req = abi::fuse_uring_cmd_req {
        flags: 0,
        commit_id,
        qid,
        padding: [0; 6],
    };
    let mut buf = [0u8; 80];
    let src = req.as_bytes();
    buf[..src.len()].copy_from_slice(src);
    buf
}

/// `UringCmd80` leaves `sqe.addr`/`sqe.len` zero, but REGISTER needs them to
/// point at the entry's `[header, payload]` iovec array. Both `Entry`/`Entry128`
/// are `#[repr(C)]` over the stable-ABI kernel SQE, so patch the raw bytes:
/// `addr` is at offset 16 (u64), `len` at offset 24 (u32). These do not overlap
/// the URING_CMD inline data (cmd_op@8, cmd bytes@48+).
fn patch_addr_len(entry: &mut squeue::Entry128, addr: u64, len: u32) {
    // SAFETY: Entry128 is repr(C) and exactly 128 bytes (64B SQE + 64B ext).
    let raw = unsafe { &mut *(entry as *mut squeue::Entry128 as *mut [u8; 128]) };
    raw[16..24].copy_from_slice(&addr.to_le_bytes());
    raw[24..28].copy_from_slice(&len.to_le_bytes());
}

fn register_sqe(qid: u16, fd: i32, ent: &EntryBuf, idx: u64) -> squeue::Entry128 {
    let cmd = cmd_bytes(qid, 0);
    let mut entry = opcode::UringCmd80::new(types::Fd(fd), abi::FUSE_IO_URING_CMD_REGISTER)
        .cmd(cmd)
        .build()
        .user_data(idx);
    let iov_ptr = ent.iov.as_ptr() as u64;
    patch_addr_len(&mut entry, iov_ptr, 2);
    entry
}

fn commit_sqe(qid: u16, fd: i32, idx: u64, commit_id: u64) -> squeue::Entry128 {
    let cmd = cmd_bytes(qid, commit_id);
    opcode::UringCmd80::new(types::Fd(fd), abi::FUSE_IO_URING_CMD_COMMIT_AND_FETCH)
        .cmd(cmd)
        .build()
        .user_data(idx)
}

/// Reconstruct a classic contiguous request buffer from an entry's split layout:
/// `fuse_in_header (40) ++ op_in[0..fixed] ++ payload[0..payload_sz]`, where
/// `fixed = fuse_in_header.len - 40 - payload_sz`. Returns the bytes and the
/// request's `commit_id`.
///
/// # Safety
/// `ent.header`/`ent.payload` must point at valid buffers the kernel has just
/// filled with a request.
unsafe fn build_request(ent: &EntryBuf) -> Option<(Vec<u8>, u64)> {
    let header = ent.header;
    let total = (header as *const u32).read_unaligned() as usize; // fuse_in_header.len @ 0
    let payload_sz = (header.add(PAYLOAD_SZ_OFFSET) as *const u32).read_unaligned() as usize;
    let commit_id = (header.add(COMMIT_ID_OFFSET) as *const u64).read_unaligned();
    if total < FUSE_IN_HEADER_SZ {
        return None;
    }
    let variable = total - FUSE_IN_HEADER_SZ;
    if payload_sz > variable {
        return None;
    }
    let fixed = variable - payload_sz;
    if fixed > abi::FUSE_URING_OP_IN_OUT_SZ || payload_sz > ent.payload_cap {
        return None;
    }
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(std::slice::from_raw_parts(header, FUSE_IN_HEADER_SZ));
    buf.extend_from_slice(std::slice::from_raw_parts(header.add(OP_IN_OFFSET), fixed));
    buf.extend_from_slice(std::slice::from_raw_parts(ent.payload, payload_sz));
    Some((buf, commit_id))
}

fn pin_to_core(qid: usize) {
    // SAFETY: zero-initialised cpu_set, single CPU set, current thread.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(qid, &mut set);
        let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

/// Handle to the running io_uring transport. Dropping it signals all ring
/// threads to stop and joins them.
pub(crate) struct UringRuntime {
    shutdown: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
}

impl Drop for UringRuntime {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        for h in self.handles.drain(..) {
            let _ = h.join();
        }
    }
}

/// Start the io_uring transport: one CPU-pinned ring thread per core.
pub(crate) fn start<FS>(
    device: Arc<File>,
    shared: Arc<SessionShared<FS>>,
    deferred: Arc<DeferredNotifier>,
    payload_cap: usize,
) -> io::Result<UringRuntime>
where
    FS: Filesystem + Send + Sync + 'static,
{
    let depth = queue_depth();
    let nr_queues = num_queues();
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(nr_queues);

    tracing::info!(
        nr_queues,
        depth,
        payload_cap,
        "starting FUSE-over-io_uring transport"
    );

    for qid in 0..nr_queues {
        let device = device.clone();
        let shared = shared.clone();
        let deferred = deferred.clone();
        let shutdown = shutdown.clone();
        let fd = device.as_raw_fd();
        let handle = thread::Builder::new()
            .name(format!("agentfs-uring-{qid}"))
            .spawn(move || {
                pin_to_core(qid);
                if let Err(e) = run_queue(
                    qid as u16,
                    fd,
                    device,
                    shared,
                    deferred,
                    depth,
                    payload_cap,
                    shutdown,
                ) {
                    tracing::warn!(qid, error = %e, "FUSE io_uring queue exited with error");
                }
            })?;
        handles.push(handle);
    }

    Ok(UringRuntime { shutdown, handles })
}

#[allow(clippy::too_many_arguments)]
fn run_queue<FS>(
    qid: u16,
    fd: i32,
    device: Arc<File>,
    shared: Arc<SessionShared<FS>>,
    deferred: Arc<DeferredNotifier>,
    depth: usize,
    payload_cap: usize,
    shutdown: Arc<AtomicBool>,
) -> io::Result<()>
where
    FS: Filesystem + Send + Sync + 'static,
{
    let mut ring: IoUring<squeue::Entry128, cqueue::Entry> = IoUring::builder()
        .setup_cqsize((depth * 2) as u32)
        .build(depth as u32)?;

    let entries: Vec<EntryBuf> = (0..depth)
        .map(|_| EntryBuf::new(payload_cap))
        .collect::<io::Result<_>>()?;

    // Register all entries (each REGISTER also fetches the first request).
    {
        let mut sq = ring.submission();
        for (idx, ent) in entries.iter().enumerate() {
            let sqe = register_sqe(qid, fd, ent, idx as u64);
            // SAFETY: the iovec the SQE references lives in `ent` for the whole
            // queue lifetime; the entry buffers outlive all in-flight commands.
            unsafe {
                sq.push(&sqe).map_err(|_| {
                    io::Error::new(io::ErrorKind::Other, "io_uring SQ full during register")
                })?;
            }
        }
    }
    ring.submit()?;

    let ts = types::Timespec::new().sec(0).nsec(200_000_000);
    let args = types::SubmitArgs::new().timespec(&ts);

    let mut commits: Vec<(usize, u64)> = Vec::with_capacity(depth);

    while !shutdown.load(Ordering::SeqCst) {
        match ring.submitter().submit_with_args(1, &args) {
            Ok(_) => {}
            Err(ref e) if e.raw_os_error() == Some(libc::ETIME) => continue,
            Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(ref e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            Err(e) => return Err(e),
        }

        commits.clear();
        {
            let mut cq = ring.completion();
            cq.sync();
            for cqe in &mut cq {
                let idx = cqe.user_data() as usize;
                let res = cqe.result();
                if res < 0 {
                    // -ENOTCONN on teardown; other errors: drop this entry.
                    continue;
                }
                if idx >= entries.len() {
                    continue;
                }
                let ent = &entries[idx];
                // SAFETY: kernel just filled this entry's buffers.
                let Some((bytes, commit_id)) = (unsafe { build_request(ent) }) else {
                    continue;
                };
                if commit_id == 0 {
                    // The kernel cannot match a reply with commit_id 0; skip.
                    continue;
                }
                let sender = ChannelSender::Uring(ent.reply_sender(device.clone()));
                let data = AlignedRequestBuf::copy_from(&bytes);
                if let Some(req) = Request::new(sender, deferred.clone(), data) {
                    req.dispatch(shared.as_ref());
                }
                commits.push((idx, commit_id));
            }
        }

        if !commits.is_empty() {
            let mut sq = ring.submission();
            for &(idx, commit_id) in &commits {
                let sqe = commit_sqe(qid, fd, idx as u64, commit_id);
                // SAFETY: same-thread; the entry buffers referenced by the
                // commit outlive the in-flight command.
                unsafe {
                    if sq.push(&sqe).is_err() {
                        // SQ is sized to the queue depth and we never have more
                        // than `depth` outstanding, so this should not happen.
                        tracing::error!(qid, "io_uring SQ full during commit");
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
