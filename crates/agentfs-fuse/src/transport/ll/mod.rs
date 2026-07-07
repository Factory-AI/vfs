//! Low-level kernel communication.

mod argument;
pub(crate) mod fuse_abi;
pub(crate) mod notify;
pub(crate) mod reply;
pub(crate) mod request;

use std::{convert::TryInto, num::NonZeroI32, time::SystemTime};

pub(crate) use reply::Response;
pub(crate) use request::{
    AnyRequest, FileHandle, INodeNo, Lock, Operation, Request, RequestId, Version,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
/// Possible input arguments for atime & mtime, which can either be set to a specified time,
/// or to the current time
pub(crate) enum TimeOrNow {
    /// Specific time provided
    SpecificTime(SystemTime),
    /// Current time
    Now,
}

macro_rules! errno {
    ($x: expr_2021) => {
        Errno(unsafe {
            // This is a static assertion that the constant $x is > 0
            const _X: [(); 0 - !{
                const ASSERT: bool = ($x > 0);
                ASSERT
            } as usize] = [];
            // Which makes this safe
            NonZeroI32::new_unchecked($x)
        })
    };
}

macro_rules! no_xattr_doc {
    () => {"Use this as an error return from getxattr/removexattr to indicate that the xattr doesn't exist.  This resolves to the appropriate platform-specific error code."}
}

/// Represents an error code to be returned to the caller
#[derive(Debug)]
pub(crate) struct Errno(pub(crate) NonZeroI32);
#[allow(
    dead_code,
    reason = "complete errno table mirrors kernel FUSE reply vocabulary"
)]
impl Errno {
    /// Operation not permitted
    pub(crate) const EPERM: Errno = errno!(libc::EPERM);
    /// No such file or directory
    pub(crate) const ENOENT: Errno = errno!(libc::ENOENT);
    /// No such process
    pub(crate) const ESRCH: Errno = errno!(libc::ESRCH);
    /// Interrupted system call
    pub(crate) const EINTR: Errno = errno!(libc::EINTR);
    /// Input/output error
    pub(crate) const EIO: Errno = errno!(libc::EIO);
    /// No such device or address
    pub(crate) const ENXIO: Errno = errno!(libc::ENXIO);
    /// Argument list too long
    pub(crate) const E2BIG: Errno = errno!(libc::E2BIG);
    /// Exec format error
    pub(crate) const ENOEXEC: Errno = errno!(libc::ENOEXEC);
    /// Bad file descriptor
    pub(crate) const EBADF: Errno = errno!(libc::EBADF);
    /// No child processes
    pub(crate) const ECHILD: Errno = errno!(libc::ECHILD);
    /// Resource temporarily unavailable
    pub(crate) const EAGAIN: Errno = errno!(libc::EAGAIN);
    /// Cannot allocate memory
    pub(crate) const ENOMEM: Errno = errno!(libc::ENOMEM);
    /// Permission denied
    pub(crate) const EACCES: Errno = errno!(libc::EACCES);
    /// Bad address
    pub(crate) const EFAULT: Errno = errno!(libc::EFAULT);
    /// Block device required
    pub(crate) const ENOTBLK: Errno = errno!(libc::ENOTBLK);
    /// Device or resource busy
    pub(crate) const EBUSY: Errno = errno!(libc::EBUSY);
    /// File exists
    pub(crate) const EEXIST: Errno = errno!(libc::EEXIST);
    /// Invalid cross-device link
    pub(crate) const EXDEV: Errno = errno!(libc::EXDEV);
    /// No such device
    pub(crate) const ENODEV: Errno = errno!(libc::ENODEV);
    /// Not a directory
    pub(crate) const ENOTDIR: Errno = errno!(libc::ENOTDIR);
    /// Is a directory
    pub(crate) const EISDIR: Errno = errno!(libc::EISDIR);
    /// Invalid argument
    pub(crate) const EINVAL: Errno = errno!(libc::EINVAL);
    /// Too many open files in system
    pub(crate) const ENFILE: Errno = errno!(libc::ENFILE);
    /// Too many open files
    pub(crate) const EMFILE: Errno = errno!(libc::EMFILE);
    /// Inappropriate ioctl for device
    pub(crate) const ENOTTY: Errno = errno!(libc::ENOTTY);
    /// Text file busy
    pub(crate) const ETXTBSY: Errno = errno!(libc::ETXTBSY);
    /// File too large
    pub(crate) const EFBIG: Errno = errno!(libc::EFBIG);
    /// No space left on device
    pub(crate) const ENOSPC: Errno = errno!(libc::ENOSPC);
    /// Illegal seek
    pub(crate) const ESPIPE: Errno = errno!(libc::ESPIPE);
    /// Read-only file system
    pub(crate) const EROFS: Errno = errno!(libc::EROFS);
    /// Too many links
    pub(crate) const EMLINK: Errno = errno!(libc::EMLINK);
    /// Broken pipe
    pub(crate) const EPIPE: Errno = errno!(libc::EPIPE);
    /// Numerical argument out of domain
    pub(crate) const EDOM: Errno = errno!(libc::EDOM);
    /// Numerical result out of range
    pub(crate) const ERANGE: Errno = errno!(libc::ERANGE);
    /// Resource deadlock avoided
    pub(crate) const EDEADLK: Errno = errno!(libc::EDEADLK);
    /// File name too long
    pub(crate) const ENAMETOOLONG: Errno = errno!(libc::ENAMETOOLONG);
    /// No locks available
    pub(crate) const ENOLCK: Errno = errno!(libc::ENOLCK);
    /// Function not implemented
    pub(crate) const ENOSYS: Errno = errno!(libc::ENOSYS);
    /// Directory not empty
    pub(crate) const ENOTEMPTY: Errno = errno!(libc::ENOTEMPTY);
    /// Too many levels of symbolic links
    pub(crate) const ELOOP: Errno = errno!(libc::ELOOP);
    /// Resource temporarily unavailable
    pub(crate) const EWOULDBLOCK: Errno = errno!(libc::EWOULDBLOCK);
    /// No message of desired type
    pub(crate) const ENOMSG: Errno = errno!(libc::ENOMSG);
    /// Identifier removed
    pub(crate) const EIDRM: Errno = errno!(libc::EIDRM);
    /// Object is remote
    pub(crate) const EREMOTE: Errno = errno!(libc::EREMOTE);
    /// Link has been severed
    pub(crate) const ENOLINK: Errno = errno!(libc::ENOLINK);
    /// Protocol error
    pub(crate) const EPROTO: Errno = errno!(libc::EPROTO);
    /// Multihop attempted
    pub(crate) const EMULTIHOP: Errno = errno!(libc::EMULTIHOP);
    /// Bad message
    pub(crate) const EBADMSG: Errno = errno!(libc::EBADMSG);
    /// Value too large for defined data type
    pub(crate) const EOVERFLOW: Errno = errno!(libc::EOVERFLOW);
    /// Invalid or incomplete multibyte or wide character
    pub(crate) const EILSEQ: Errno = errno!(libc::EILSEQ);
    /// Too many users
    pub(crate) const EUSERS: Errno = errno!(libc::EUSERS);
    /// Socket operation on non-socket
    pub(crate) const ENOTSOCK: Errno = errno!(libc::ENOTSOCK);
    /// Destination address required
    pub(crate) const EDESTADDRREQ: Errno = errno!(libc::EDESTADDRREQ);
    /// Message too long
    pub(crate) const EMSGSIZE: Errno = errno!(libc::EMSGSIZE);
    /// Protocol wrong type for socket
    pub(crate) const EPROTOTYPE: Errno = errno!(libc::EPROTOTYPE);
    /// Protocol not available
    pub(crate) const ENOPROTOOPT: Errno = errno!(libc::ENOPROTOOPT);
    /// Protocol not supported
    pub(crate) const EPROTONOSUPPORT: Errno = errno!(libc::EPROTONOSUPPORT);
    /// Socket type not supported
    pub(crate) const ESOCKTNOSUPPORT: Errno = errno!(libc::ESOCKTNOSUPPORT);
    /// Operation not supported
    pub(crate) const EOPNOTSUPP: Errno = errno!(libc::EOPNOTSUPP);
    /// Protocol family not supported
    pub(crate) const EPFNOSUPPORT: Errno = errno!(libc::EPFNOSUPPORT);
    /// Address family not supported by protocol
    pub(crate) const EAFNOSUPPORT: Errno = errno!(libc::EAFNOSUPPORT);
    /// Address already in use
    pub(crate) const EADDRINUSE: Errno = errno!(libc::EADDRINUSE);
    /// Cannot assign requested address
    pub(crate) const EADDRNOTAVAIL: Errno = errno!(libc::EADDRNOTAVAIL);
    /// Network is down
    pub(crate) const ENETDOWN: Errno = errno!(libc::ENETDOWN);
    /// Network is unreachable
    pub(crate) const ENETUNREACH: Errno = errno!(libc::ENETUNREACH);
    /// Network dropped connection on reset
    pub(crate) const ENETRESET: Errno = errno!(libc::ENETRESET);
    /// Software caused connection abort
    pub(crate) const ECONNABORTED: Errno = errno!(libc::ECONNABORTED);
    /// Connection reset by peer
    pub(crate) const ECONNRESET: Errno = errno!(libc::ECONNRESET);
    /// No buffer space available
    pub(crate) const ENOBUFS: Errno = errno!(libc::ENOBUFS);
    /// Transport endpoint is already connected
    pub(crate) const EISCONN: Errno = errno!(libc::EISCONN);
    /// Transport endpoint is not connected
    pub(crate) const ENOTCONN: Errno = errno!(libc::ENOTCONN);
    /// Cannot send after transport endpoint shutdown
    pub(crate) const ESHUTDOWN: Errno = errno!(libc::ESHUTDOWN);
    /// Too many references: cannot splice
    pub(crate) const ETOOMANYREFS: Errno = errno!(libc::ETOOMANYREFS);
    /// Connection timed out
    pub(crate) const ETIMEDOUT: Errno = errno!(libc::ETIMEDOUT);
    /// Connection refused
    pub(crate) const ECONNREFUSED: Errno = errno!(libc::ECONNREFUSED);
    /// Host is down
    pub(crate) const EHOSTDOWN: Errno = errno!(libc::EHOSTDOWN);
    /// No route to host
    pub(crate) const EHOSTUNREACH: Errno = errno!(libc::EHOSTUNREACH);
    /// Operation already in progress
    pub(crate) const EALREADY: Errno = errno!(libc::EALREADY);
    /// Operation now in progress
    pub(crate) const EINPROGRESS: Errno = errno!(libc::EINPROGRESS);
    /// Stale file handle
    pub(crate) const ESTALE: Errno = errno!(libc::ESTALE);
    /// Disk quota exceeded
    pub(crate) const EDQUOT: Errno = errno!(libc::EDQUOT);
    /// Operation cancelled
    pub(crate) const ECANCELED: Errno = errno!(libc::ECANCELED);
    /// Owner died
    pub(crate) const EOWNERDEAD: Errno = errno!(libc::EOWNERDEAD);
    /// State not recoverable
    pub(crate) const ENOTRECOVERABLE: Errno = errno!(libc::ENOTRECOVERABLE);
    /// Operation not supported
    pub(crate) const ENOTSUP: Errno = errno!(libc::ENOTSUP);

    /// No data available
    pub(crate) const ENODATA: Errno = errno!(libc::ENODATA);
    #[doc = no_xattr_doc!()]
    pub(crate) const NO_XATTR: Errno = Self::ENODATA;

    pub(crate) fn from_i32(err: i32) -> Errno {
        err.try_into().ok().map_or(Errno::EIO, Errno)
    }
}
impl From<std::io::Error> for Errno {
    fn from(err: std::io::Error) -> Self {
        let errno = err.raw_os_error().unwrap_or(0);
        match errno.try_into() {
            Ok(i) => Errno(i),
            Err(_) => Errno::EIO,
        }
    }
}
impl From<nix::errno::Errno> for Errno {
    fn from(x: nix::errno::Errno) -> Self {
        let err: std::io::Error = x.into();
        err.into()
    }
}
impl From<std::io::ErrorKind> for Errno {
    fn from(x: std::io::ErrorKind) -> Self {
        let err: std::io::Error = x.into();
        err.into()
    }
}
impl From<Errno> for i32 {
    fn from(x: Errno) -> Self {
        x.0.into()
    }
}

/// A newtype for generation numbers
///
/// If the file system will be exported over NFS, the (ino, generation) pairs
/// need to be unique over the file system's lifetime (rather than just the
/// mount time). So if the file system reuses an inode after it has been
/// deleted, it must assign a new, previously unused generation number to the
/// inode at the same time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Generation(pub(crate) u64);
impl From<Generation> for u64 {
    fn from(fh: Generation) -> Self {
        fh.0
    }
}

#[cfg(test)]
mod test {
    use std::io::IoSlice;
    use std::ops::{Deref, DerefMut};
    /// If we want to be able to cast bytes to our fuse C struct types we need it
    /// to be aligned.  This struct helps getting &[u8]s which are 8 byte aligned.
    #[cfg(test)]
    #[repr(align(8))]
    pub(crate) struct AlignedData<T>(pub(crate) T);
    impl<T> Deref for AlignedData<T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }
    impl<T> DerefMut for AlignedData<T> {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    pub(crate) fn ioslice_to_vec(s: &[IoSlice<'_>]) -> Vec<u8> {
        let mut v = Vec::with_capacity(s.iter().map(|x| x.len()).sum());
        for x in s {
            v.extend_from_slice(x);
        }
        v
    }
}
