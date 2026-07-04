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
pub(crate) struct Channel {
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

    /// Receives data up to the capacity of the given buffer (can block).
    pub(crate) fn receive(&self, buffer: &mut [u8]) -> io::Result<usize> {
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

    /// Returns the shared /dev/fuse device handle.
    pub(crate) fn device(&self) -> Arc<File> {
        self.device.clone()
    }

    /// Returns a sender object for this channel. The sender object can be
    /// used to send to the channel. Multiple sender objects can be used
    /// and they can safely be sent to other threads.
    pub(crate) fn sender(&self) -> ChannelSender {
        ChannelSender::Fd {
            device: self.device.clone(),
        }
    }
}

/// Reply target for a FUSE request: either the classic /dev/fuse writev path
/// or a fuse-over-io_uring ring entry commit.
#[derive(Clone, Debug)]
pub(crate) enum ChannelSender {
    Fd {
        device: Arc<File>,
    },
    #[cfg(target_os = "linux")]
    Uring(super::uring::UringSender),
}

impl ChannelSender {
    /// Notifications (and poll wakeups) are not supported over
    /// fuse-io-uring; they must always travel via the /dev/fuse fd.
    pub(crate) fn for_notify(&self) -> ChannelSender {
        match self {
            ChannelSender::Fd { device } => ChannelSender::Fd {
                device: device.clone(),
            },
            #[cfg(target_os = "linux")]
            ChannelSender::Uring(sender) => ChannelSender::Fd {
                device: sender.device(),
            },
        }
    }
}

impl ReplySender for ChannelSender {
    fn send(&self, bufs: &[io::IoSlice<'_>]) -> io::Result<()> {
        match self {
            ChannelSender::Fd { device } => {
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
            #[cfg(target_os = "linux")]
            ChannelSender::Uring(sender) => sender.send_reply(bufs),
        }
    }
}
