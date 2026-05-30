use std::{
    fs::File,
    io,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::prelude::AsRawFd,
    },
    sync::Arc,
};

use libc::{c_int, c_void, size_t};

use super::reply::ReplySender;

/// A raw communication channel to the FUSE kernel driver
#[derive(Debug)]
pub struct Channel {
    device: Arc<File>,
}

impl AsFd for Channel {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.device.as_fd()
    }
}

impl Channel {
    /// Create a new communication channel to the kernel driver by mounting the
    /// given path. The kernel driver will delegate filesystem operations of
    /// the given path to the channel.
    pub(crate) fn new(device: Arc<File>) -> Self {
        Self { device }
    }

    /// Clone of the underlying `/dev/fuse` file handle, used by the io_uring
    /// transport to issue ring commands against the same connection.
    #[cfg(feature = "abi-7-42")]
    pub(crate) fn device_arc(&self) -> Arc<File> {
        self.device.clone()
    }

    /// Receives data up to the capacity of the given buffer (can block).
    pub fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let rc = unsafe {
            libc::read(
                self.device.as_raw_fd(),
                buffer.as_ptr() as *mut c_void,
                buffer.len() as size_t,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(rc as usize)
        }
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub fn sender(&self) -> ChannelSender {
        ChannelSender::Classic {
            device: self.device.clone(),
        }
    }
}

/// Reply transport for a single request.
///
/// `Classic` writes the reply (or a notification) back over the `/dev/fuse`
/// file descriptor with `writev`. `Uring` writes the reply into the per-entry
/// io_uring buffers; the owning ring thread then submits COMMIT_AND_FETCH (see
/// `cli/src/fuser/uring.rs`). Both variants carry the device fd so notifications
/// (which must never go through the ring) always have a classic path via
/// [`ChannelSender::notify_sender`].
#[derive(Clone, Debug)]
pub enum ChannelSender {
    Classic {
        device: Arc<File>,
    },
    #[cfg(feature = "abi-7-42")]
    Uring(UringReplySender),
}

impl ChannelSender {
    /// A classic (`/dev/fuse` writev) sender suitable for kernel notifications,
    /// regardless of which transport delivered the originating request.
    pub fn notify_sender(&self) -> ChannelSender {
        match self {
            ChannelSender::Classic { device } => ChannelSender::Classic {
                device: device.clone(),
            },
            #[cfg(feature = "abi-7-42")]
            ChannelSender::Uring(u) => ChannelSender::Classic {
                device: u.device.clone(),
            },
        }
    }
}

fn classic_send(device: &File, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
    let rc = unsafe {
        libc::writev(
            device.as_raw_fd(),
            bufs.as_ptr() as *const libc::iovec,
            bufs.len() as c_int,
        )
    };
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        debug_assert_eq!(bufs.iter().map(|b| b.len()).sum::<usize>(), rc as usize);
        Ok(())
    }
}

impl ReplySender for ChannelSender {
    fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        match self {
            ChannelSender::Classic { device } => classic_send(device, bufs),
            #[cfg(feature = "abi-7-42")]
            ChannelSender::Uring(u) => u.commit_reply(bufs),
        }
    }
}

/// Reply target backed by a single io_uring ring entry. Holds raw pointers into
/// the entry's page-aligned header buffer and payload buffer. These are only
/// ever written from the owning ring thread during synchronous dispatch (the
/// reply is produced before the ring thread advances), so the pointer access is
/// race-free despite the `Send + Sync` bounds the `ReplySender` trait requires.
#[cfg(feature = "abi-7-42")]
#[derive(Clone, Debug)]
pub struct UringReplySender {
    pub(crate) device: Arc<File>,
    /// Page-aligned `fuse_uring_req_header` buffer (in_out[128] + op_in[128] +
    /// ring_ent_in_out{..}). Reply out-header goes at offset 0; the reply
    /// payload size is written into ring_ent_in_out.payload_sz at offset 272.
    pub(crate) header: *mut u8,
    pub(crate) payload: *mut u8,
    pub(crate) payload_cap: usize,
}

// SAFETY: see the type-level doc — the raw buffers are single-threaded per ring
// queue and only touched during synchronous dispatch on the owning thread.
#[cfg(feature = "abi-7-42")]
unsafe impl Send for UringReplySender {}
#[cfg(feature = "abi-7-42")]
unsafe impl Sync for UringReplySender {}

#[cfg(feature = "abi-7-42")]
impl UringReplySender {
    /// Offset of `ring_ent_in_out.payload_sz` within `fuse_uring_req_header`:
    /// in_out(128) + op_in(128) + flags(8) + commit_id(8) = 272.
    const PAYLOAD_SZ_OFF: usize = 272;
    /// `fuse_out_header` is 16 bytes (len + error + unique).
    const OUT_HEADER_LEN: usize = 16;

    fn commit_reply(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        if bufs.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty FUSE reply",
            ));
        }
        // bufs[0] is the fuse_out_header; bufs[1..] is the reply payload.
        let out_header = &bufs[0];
        let header_len = out_header.len().min(Self::OUT_HEADER_LEN);
        let payload_len: usize = bufs[1..].iter().map(|b| b.len()).sum();
        if payload_len > self.payload_cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "FUSE reply payload exceeds io_uring entry buffer",
            ));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(out_header.as_ptr(), self.header, header_len);
            let mut off = 0usize;
            for b in &bufs[1..] {
                std::ptr::copy_nonoverlapping(b.as_ptr(), self.payload.add(off), b.len());
                off += b.len();
            }
            let sz_ptr = self.header.add(Self::PAYLOAD_SZ_OFF) as *mut u32;
            sz_ptr.write_unaligned(payload_len as u32);
        }
        Ok(())
    }
}
