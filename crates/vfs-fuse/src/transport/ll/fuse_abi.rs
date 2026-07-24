//! FUSE kernel interface.
//!
//! Types and definitions used for communication between the kernel driver and the userspace
//! part of a FUSE filesystem. Since the kernel driver may be installed independently, the ABI
//! interface is versioned and capabilities are exchanged during the initialization (mounting)
//! of a filesystem.
//!
//! libfuse (Linux): <https://github.com/libfuse/libfuse/blob/master/include/fuse_kernel.h>
//! - supports ABI 7.8 since FUSE 2.6.0
//! - supports ABI 7.12 since FUSE 2.8.0
//! - supports ABI 7.18 since FUSE 2.9.0
//! - supports ABI 7.19 since FUSE 2.9.1
//! - supports ABI 7.26 since FUSE 3.0.0
//!
//! Items without a version annotation are valid with ABI 7.8 and later

#![warn(missing_debug_implementations)]
#![allow(missing_docs)]

use consts::{FATTR_ATIME_NOW, FATTR_MTIME_NOW};
use std::convert::TryFrom;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub(crate) const FUSE_KERNEL_VERSION: u32 = 7;
pub(crate) const FUSE_KERNEL_MINOR_VERSION: u32 = 42;

#[allow(
    dead_code,
    reason = "kept as part of the adopted Linux FUSE ABI surface"
)]
pub(crate) const FUSE_ROOT_ID: u64 = 1;

#[repr(C)]
#[derive(Debug, IntoBytes, Clone, Copy, KnownLayout, Immutable)]
pub(crate) struct fuse_attr {
    pub(crate) ino: u64,
    pub(crate) size: u64,
    pub(crate) blocks: u64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    // to match stat.st_atime
    pub(crate) atime: i64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    // to match stat.st_mtime
    pub(crate) mtime: i64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    // to match stat.st_ctime
    pub(crate) ctime: i64,
    pub(crate) atimensec: u32,
    pub(crate) mtimensec: u32,
    pub(crate) ctimensec: u32,
    pub(crate) mode: u32,
    pub(crate) nlink: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) rdev: u32,
    pub(crate) blksize: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_kstatfs {
    pub(crate) blocks: u64,  // Total blocks (in units of frsize)
    pub(crate) bfree: u64,   // Free blocks
    pub(crate) bavail: u64,  // Free blocks for unprivileged users
    pub(crate) files: u64,   // Total inodes
    pub(crate) ffree: u64,   // Free inodes
    pub(crate) bsize: u32,   // Filesystem block size
    pub(crate) namelen: u32, // Maximum filename length
    pub(crate) frsize: u32,  // Fundamental file system block size
    pub(crate) padding: u32,
    pub(crate) spare: [u32; 6],
}

#[repr(C)]
#[derive(Debug, IntoBytes, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_file_lock {
    pub(crate) start: u64,
    pub(crate) end: u64,
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is treated as signed
    pub(crate) typ: i32,
    pub(crate) pid: u32,
}

#[allow(dead_code, reason = "kept as a complete Linux FUSE ABI constant table")]
pub(crate) mod consts {
    // Bitmasks for fuse_setattr_in.valid
    pub(crate) const FATTR_MODE: u32 = 1 << 0;
    pub(crate) const FATTR_UID: u32 = 1 << 1;
    pub(crate) const FATTR_GID: u32 = 1 << 2;
    pub(crate) const FATTR_SIZE: u32 = 1 << 3;
    pub(crate) const FATTR_ATIME: u32 = 1 << 4;
    pub(crate) const FATTR_MTIME: u32 = 1 << 5;
    pub(crate) const FATTR_FH: u32 = 1 << 6;
    pub(crate) const FATTR_ATIME_NOW: u32 = 1 << 7;
    pub(crate) const FATTR_MTIME_NOW: u32 = 1 << 8;
    pub(crate) const FATTR_LOCKOWNER: u32 = 1 << 9;
    pub(crate) const FATTR_CTIME: u32 = 1 << 10;

    // Flags returned by the open request
    pub(crate) const FOPEN_DIRECT_IO: u32 = 1 << 0; // bypass page cache for this open file
    pub(crate) const FOPEN_KEEP_CACHE: u32 = 1 << 1; // don't invalidate the data cache on open
    pub(crate) const FOPEN_NONSEEKABLE: u32 = 1 << 2; // the file is not seekable
    pub(crate) const FOPEN_CACHE_DIR: u32 = 1 << 3; // allow caching this directory
    pub(crate) const FOPEN_STREAM: u32 = 1 << 4; // the file is stream-like (no file position at all)
                                                 // Init request/reply flags
    pub(crate) const FUSE_ASYNC_READ: u64 = 1 << 0; // asynchronous read requests
    pub(crate) const FUSE_POSIX_LOCKS: u64 = 1 << 1; // remote locking for POSIX file locks
    pub(crate) const FUSE_FILE_OPS: u64 = 1 << 2; // kernel sends file handle for fstat, etc...
    pub(crate) const FUSE_ATOMIC_O_TRUNC: u64 = 1 << 3; // handles the O_TRUNC open flag in the filesystem
    pub(crate) const FUSE_EXPORT_SUPPORT: u64 = 1 << 4; // filesystem handles lookups of "." and ".."
    pub(crate) const FUSE_BIG_WRITES: u64 = 1 << 5; // filesystem can handle write size larger than 4kB
    pub(crate) const FUSE_DONT_MASK: u64 = 1 << 6; // don't apply umask to file mode on create operations
    pub(crate) const FUSE_SPLICE_WRITE: u64 = 1 << 7; // kernel supports splice write on the device
    pub(crate) const FUSE_SPLICE_MOVE: u64 = 1 << 8; // kernel supports splice move on the device
    pub(crate) const FUSE_SPLICE_READ: u64 = 1 << 9; // kernel supports splice read on the device
    pub(crate) const FUSE_FLOCK_LOCKS: u64 = 1 << 10; // remote locking for flock-style file locks
    pub(crate) const FUSE_HAS_IOCTL_DIR: u64 = 1 << 11; // kernel supports ioctl on directories
    pub(crate) const FUSE_AUTO_INVAL_DATA: u64 = 1 << 12; // automatically invalidate cached pages
    pub(crate) const FUSE_DO_READDIRPLUS: u64 = 1 << 13; // do READDIRPLUS (READDIR+LOOKUP in one)
    pub(crate) const FUSE_READDIRPLUS_AUTO: u64 = 1 << 14; // adaptive readdirplus
    pub(crate) const FUSE_ASYNC_DIO: u64 = 1 << 15; // asynchronous direct I/O submission
    pub(crate) const FUSE_WRITEBACK_CACHE: u64 = 1 << 16; // use writeback cache for buffered writes
    pub(crate) const FUSE_NO_OPEN_SUPPORT: u64 = 1 << 17; // kernel supports zero-message opens
    pub(crate) const FUSE_PARALLEL_DIROPS: u64 = 1 << 18; // allow parallel lookups and readdir
    pub(crate) const FUSE_HANDLE_KILLPRIV: u64 = 1 << 19; // fs handles killing suid/sgid/cap on write/chown/trunc
    pub(crate) const FUSE_POSIX_ACL: u64 = 1 << 20; // filesystem supports posix acls
    pub(crate) const FUSE_ABORT_ERROR: u64 = 1 << 21; // reading the device after abort returns ECONNABORTED
    pub(crate) const FUSE_MAX_PAGES: u64 = 1 << 22; // init_out.max_pages contains the max number of req pages
    pub(crate) const FUSE_CACHE_SYMLINKS: u64 = 1 << 23; // cache READLINK responses
    pub(crate) const FUSE_NO_OPENDIR_SUPPORT: u64 = 1 << 24; // kernel supports zero-message opendir
    pub(crate) const FUSE_EXPLICIT_INVAL_DATA: u64 = 1 << 25; // only invalidate cached pages on explicit request
    pub(crate) const FUSE_INIT_EXT: u64 = 1 << 30; // extended fuse_init_in request
    pub(crate) const FUSE_INIT_RESERVED: u64 = 1 << 31; // reserved, do not use
    pub(crate) const FUSE_OVER_IO_URING: u64 = 1 << 41; // client supports fuse-over-io-uring

    // CUSE init request/reply flags
    pub(crate) const CUSE_UNRESTRICTED_IOCTL: u32 = 1 << 0; // use unrestricted ioctl

    // Release flags
    pub(crate) const FUSE_RELEASE_FLUSH: u32 = 1 << 0;
    pub(crate) const FUSE_RELEASE_FLOCK_UNLOCK: u32 = 1 << 1;

    // Getattr flags
    pub(crate) const FUSE_GETATTR_FH: u32 = 1 << 0;

    // Lock flags
    pub(crate) const FUSE_LK_FLOCK: u32 = 1 << 0;

    // Write flags
    pub(crate) const FUSE_WRITE_CACHE: u32 = 1 << 0; // delayed write from page cache, file handle is guessed
    pub(crate) const FUSE_WRITE_LOCKOWNER: u32 = 1 << 1; // lock_owner field is valid
    pub(crate) const FUSE_WRITE_KILL_PRIV: u32 = 1 << 2; // kill suid and sgid bits

    // Read flags
    pub(crate) const FUSE_READ_LOCKOWNER: u32 = 1 << 1;

    // IOCTL flags
    pub(crate) const FUSE_IOCTL_COMPAT: u32 = 1 << 0; // 32bit compat ioctl on 64bit machine
    pub(crate) const FUSE_IOCTL_UNRESTRICTED: u32 = 1 << 1; // not restricted to well-formed ioctls, retry allowed
    pub(crate) const FUSE_IOCTL_RETRY: u32 = 1 << 2; // retry with new iovecs
    pub(crate) const FUSE_IOCTL_32BIT: u32 = 1 << 3; // 32bit ioctl
    pub(crate) const FUSE_IOCTL_DIR: u32 = 1 << 4; // is a directory
    pub(crate) const FUSE_IOCTL_COMPAT_X32: u32 = 1 << 5; // x32 compat ioctl on 64bit machine (64bit time_t)
    pub(crate) const FUSE_IOCTL_MAX_IOV: u32 = 256; // maximum of in_iovecs + out_iovecs

    // Poll flags
    pub(crate) const FUSE_POLL_SCHEDULE_NOTIFY: u32 = 1 << 0; // request poll notify

    // fsync flags
    pub(crate) const FUSE_FSYNC_FDATASYNC: u32 = 1 << 0; // Sync data only, not metadata

    // The read buffer is required to be at least 8k, but may be much larger
    pub(crate) const FUSE_MIN_READ_BUFFER: usize = 8192;
}

/// Invalid opcode error.
#[derive(Debug)]
pub(crate) struct InvalidOpcodeError;

#[repr(C)]
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub(crate) enum fuse_opcode {
    FUSE_LOOKUP = 1,
    FUSE_FORGET = 2, // no reply
    FUSE_GETATTR = 3,
    FUSE_SETATTR = 4,
    FUSE_READLINK = 5,
    FUSE_SYMLINK = 6,
    FUSE_MKNOD = 8,
    FUSE_MKDIR = 9,
    FUSE_UNLINK = 10,
    FUSE_RMDIR = 11,
    FUSE_RENAME = 12,
    FUSE_LINK = 13,
    FUSE_OPEN = 14,
    FUSE_READ = 15,
    FUSE_WRITE = 16,
    FUSE_STATFS = 17,
    FUSE_RELEASE = 18,
    FUSE_FSYNC = 20,
    FUSE_SETXATTR = 21,
    FUSE_GETXATTR = 22,
    FUSE_LISTXATTR = 23,
    FUSE_REMOVEXATTR = 24,
    FUSE_FLUSH = 25,
    FUSE_INIT = 26,
    FUSE_OPENDIR = 27,
    FUSE_READDIR = 28,
    FUSE_RELEASEDIR = 29,
    FUSE_FSYNCDIR = 30,
    FUSE_GETLK = 31,
    FUSE_SETLK = 32,
    FUSE_SETLKW = 33,
    FUSE_ACCESS = 34,
    FUSE_CREATE = 35,
    FUSE_INTERRUPT = 36,
    FUSE_BMAP = 37,
    FUSE_DESTROY = 38,
    FUSE_IOCTL = 39,
    FUSE_POLL = 40,
    FUSE_BATCH_FORGET = 42,
    FUSE_FALLOCATE = 43,
    FUSE_READDIRPLUS = 44,
    FUSE_RENAME2 = 45,
    FUSE_LSEEK = 46,
    FUSE_COPY_FILE_RANGE = 47,
}

impl TryFrom<u32> for fuse_opcode {
    type Error = InvalidOpcodeError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        match n {
            1 => Ok(fuse_opcode::FUSE_LOOKUP),
            2 => Ok(fuse_opcode::FUSE_FORGET),
            3 => Ok(fuse_opcode::FUSE_GETATTR),
            4 => Ok(fuse_opcode::FUSE_SETATTR),
            5 => Ok(fuse_opcode::FUSE_READLINK),
            6 => Ok(fuse_opcode::FUSE_SYMLINK),
            8 => Ok(fuse_opcode::FUSE_MKNOD),
            9 => Ok(fuse_opcode::FUSE_MKDIR),
            10 => Ok(fuse_opcode::FUSE_UNLINK),
            11 => Ok(fuse_opcode::FUSE_RMDIR),
            12 => Ok(fuse_opcode::FUSE_RENAME),
            13 => Ok(fuse_opcode::FUSE_LINK),
            14 => Ok(fuse_opcode::FUSE_OPEN),
            15 => Ok(fuse_opcode::FUSE_READ),
            16 => Ok(fuse_opcode::FUSE_WRITE),
            17 => Ok(fuse_opcode::FUSE_STATFS),
            18 => Ok(fuse_opcode::FUSE_RELEASE),
            20 => Ok(fuse_opcode::FUSE_FSYNC),
            21 => Ok(fuse_opcode::FUSE_SETXATTR),
            22 => Ok(fuse_opcode::FUSE_GETXATTR),
            23 => Ok(fuse_opcode::FUSE_LISTXATTR),
            24 => Ok(fuse_opcode::FUSE_REMOVEXATTR),
            25 => Ok(fuse_opcode::FUSE_FLUSH),
            26 => Ok(fuse_opcode::FUSE_INIT),
            27 => Ok(fuse_opcode::FUSE_OPENDIR),
            28 => Ok(fuse_opcode::FUSE_READDIR),
            29 => Ok(fuse_opcode::FUSE_RELEASEDIR),
            30 => Ok(fuse_opcode::FUSE_FSYNCDIR),
            31 => Ok(fuse_opcode::FUSE_GETLK),
            32 => Ok(fuse_opcode::FUSE_SETLK),
            33 => Ok(fuse_opcode::FUSE_SETLKW),
            34 => Ok(fuse_opcode::FUSE_ACCESS),
            35 => Ok(fuse_opcode::FUSE_CREATE),
            36 => Ok(fuse_opcode::FUSE_INTERRUPT),
            37 => Ok(fuse_opcode::FUSE_BMAP),
            38 => Ok(fuse_opcode::FUSE_DESTROY),
            39 => Ok(fuse_opcode::FUSE_IOCTL),
            40 => Ok(fuse_opcode::FUSE_POLL),
            42 => Ok(fuse_opcode::FUSE_BATCH_FORGET),
            43 => Ok(fuse_opcode::FUSE_FALLOCATE),
            44 => Ok(fuse_opcode::FUSE_READDIRPLUS),
            45 => Ok(fuse_opcode::FUSE_RENAME2),
            46 => Ok(fuse_opcode::FUSE_LSEEK),
            47 => Ok(fuse_opcode::FUSE_COPY_FILE_RANGE),
            _ => Err(InvalidOpcodeError),
        }
    }
}

/// Invalid notify code error.
#[derive(Debug)]
pub(crate) struct InvalidNotifyCodeError;

#[repr(C)]
#[derive(Debug)]
#[allow(non_camel_case_types)]
pub(crate) enum fuse_notify_code {
    FUSE_POLL = 1,
    FUSE_NOTIFY_INVAL_INODE = 2,
    FUSE_NOTIFY_INVAL_ENTRY = 3,
    FUSE_NOTIFY_STORE = 4,
    FUSE_NOTIFY_RETRIEVE = 5,
    FUSE_NOTIFY_DELETE = 6,
}

impl TryFrom<u32> for fuse_notify_code {
    type Error = InvalidNotifyCodeError;

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        match n {
            1 => Ok(fuse_notify_code::FUSE_POLL),
            2 => Ok(fuse_notify_code::FUSE_NOTIFY_INVAL_INODE),
            3 => Ok(fuse_notify_code::FUSE_NOTIFY_INVAL_ENTRY),
            4 => Ok(fuse_notify_code::FUSE_NOTIFY_STORE),
            5 => Ok(fuse_notify_code::FUSE_NOTIFY_RETRIEVE),
            6 => Ok(fuse_notify_code::FUSE_NOTIFY_DELETE),

            _ => Err(InvalidNotifyCodeError),
        }
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_entry_out {
    pub(crate) nodeid: u64,
    pub(crate) generation: u64,
    pub(crate) entry_valid: u64,
    pub(crate) attr_valid: u64,
    pub(crate) entry_valid_nsec: u32,
    pub(crate) attr_valid_nsec: u32,
    pub(crate) attr: fuse_attr,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_forget_in {
    pub(crate) nlookup: u64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_forget_one {
    pub(crate) nodeid: u64,
    pub(crate) nlookup: u64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_batch_forget_in {
    pub(crate) count: u32,
    pub(crate) dummy: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_getattr_in {
    pub(crate) getattr_flags: u32,
    pub(crate) dummy: u32,
    pub(crate) fh: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_attr_out {
    pub(crate) attr_valid: u64,
    pub(crate) attr_valid_nsec: u32,
    pub(crate) dummy: u32,
    pub(crate) attr: fuse_attr,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_mknod_in {
    pub(crate) mode: u32,
    pub(crate) rdev: u32,
    pub(crate) umask: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_mkdir_in {
    pub(crate) mode: u32,
    pub(crate) umask: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_rename_in {
    pub(crate) newdir: u64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_rename2_in {
    pub(crate) newdir: u64,
    pub(crate) flags: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_link_in {
    pub(crate) oldnodeid: u64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_setattr_in {
    pub(crate) valid: u32,
    pub(crate) padding: u32,
    pub(crate) fh: u64,
    pub(crate) size: u64,
    pub(crate) lock_owner: u64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    // to match stat.st_atime
    pub(crate) atime: i64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    // to match stat.st_mtime
    pub(crate) mtime: i64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    // to match stat.st_ctime
    pub(crate) ctime: i64,
    pub(crate) atimensec: u32,
    pub(crate) mtimensec: u32,
    pub(crate) ctimensec: u32,
    pub(crate) mode: u32,
    pub(crate) unused4: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) unused5: u32,
}

impl fuse_setattr_in {
    pub(crate) fn atime_now(&self) -> bool {
        self.valid & FATTR_ATIME_NOW != 0
    }

    pub(crate) fn mtime_now(&self) -> bool {
        self.valid & FATTR_MTIME_NOW != 0
    }
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_open_in {
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i32 when invoking the filesystem's open method and this matches the open() syscall
    pub(crate) flags: i32,
    pub(crate) unused: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_create_in {
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i32 when invoking the filesystem's create method and this matches the open() syscall
    pub(crate) flags: i32,
    pub(crate) mode: u32,
    pub(crate) umask: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_create_out(pub(crate) fuse_entry_out, pub(crate) fuse_open_out);

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_open_out {
    pub(crate) fh: u64,
    pub(crate) open_flags: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_release_in {
    pub(crate) fh: u64,
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i32 when invoking the filesystem's read method
    pub(crate) flags: i32,
    pub(crate) release_flags: u32,
    pub(crate) lock_owner: u64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_flush_in {
    pub(crate) fh: u64,
    pub(crate) unused: u32,
    pub(crate) padding: u32,
    pub(crate) lock_owner: u64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_read_in {
    pub(crate) fh: u64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i64 when invoking the filesystem's read method
    pub(crate) offset: i64,
    pub(crate) size: u32,
    pub(crate) read_flags: u32,
    pub(crate) lock_owner: u64,
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i32 when invoking the filesystem's read method
    pub(crate) flags: i32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_write_in {
    pub(crate) fh: u64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i64 when invoking the filesystem's write method
    pub(crate) offset: i64,
    pub(crate) size: u32,
    pub(crate) write_flags: u32,
    pub(crate) lock_owner: u64,
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i32 when invoking the filesystem's read method
    pub(crate) flags: i32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_write_out {
    pub(crate) size: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_statfs_out {
    pub(crate) st: fuse_kstatfs,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_fsync_in {
    pub(crate) fh: u64,
    pub(crate) fsync_flags: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_setxattr_in {
    pub(crate) size: u32,
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i32 when invoking the filesystem's setxattr method
    pub(crate) flags: i32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_getxattr_in {
    pub(crate) size: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
#[allow(
    dead_code,
    reason = "reply shape retained for default-ENOSYS xattr dispatch"
)]
pub(crate) struct fuse_getxattr_out {
    pub(crate) size: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_lk_in {
    pub(crate) fh: u64,
    pub(crate) owner: u64,
    pub(crate) lk: fuse_file_lock,
    pub(crate) lk_flags: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
#[allow(
    dead_code,
    reason = "reply shape retained for default-ENOSYS lock dispatch"
)]
pub(crate) struct fuse_lk_out {
    pub(crate) lk: fuse_file_lock,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_access_in {
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is then cast
    // to an i32 when invoking the filesystem's access method
    pub(crate) mask: i32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_init_in {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) max_readahead: u32,
    pub(crate) flags: u32,
    pub(crate) flags2: u32,
    pub(crate) unused: [u32; 11],
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_init_out {
    pub(crate) major: u32,
    pub(crate) minor: u32,
    pub(crate) max_readahead: u32,
    pub(crate) flags: u32,
    pub(crate) max_background: u16,
    pub(crate) congestion_threshold: u16,
    pub(crate) max_write: u32,
    pub(crate) time_gran: u32,
    pub(crate) max_pages: u16,
    pub(crate) unused2: u16,
    pub(crate) flags2: u32,
    pub(crate) reserved: [u32; 7],
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_interrupt_in {
    pub(crate) unique: u64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_bmap_in {
    pub(crate) block: u64,
    pub(crate) blocksize: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
#[allow(
    dead_code,
    reason = "reply shape retained for default-ENOSYS bmap dispatch"
)]
pub(crate) struct fuse_bmap_out {
    pub(crate) block: u64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_ioctl_in {
    pub(crate) fh: u64,
    pub(crate) flags: u32,
    pub(crate) cmd: u32,
    pub(crate) arg: u64, // TODO: this is currently unused, but is defined as a void* in libfuse
    pub(crate) in_size: u32,
    pub(crate) out_size: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
#[allow(
    dead_code,
    reason = "reply shape retained for default-ENOSYS ioctl dispatch"
)]
pub(crate) struct fuse_ioctl_out {
    pub(crate) result: i32,
    pub(crate) flags: u32,
    pub(crate) in_iovs: u32,
    pub(crate) out_iovs: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_poll_in {
    pub(crate) fh: u64,
    pub(crate) kh: u64,
    pub(crate) flags: u32,
    pub(crate) events: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
#[allow(
    dead_code,
    reason = "reply shape retained for default-ENOSYS poll dispatch"
)]
pub(crate) struct fuse_poll_out {
    pub(crate) revents: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_fallocate_in {
    pub(crate) fh: u64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    pub(crate) offset: i64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    pub(crate) length: i64,
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is treated as signed
    pub(crate) mode: i32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_in_header {
    pub(crate) len: u32,
    pub(crate) opcode: u32,
    pub(crate) unique: u64,
    pub(crate) nodeid: u64,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) pid: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_out_header {
    pub(crate) len: u32,
    pub(crate) error: i32,
    pub(crate) unique: u64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_dirent {
    pub(crate) ino: u64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    pub(crate) off: i64,
    pub(crate) namelen: u32,
    pub(crate) typ: u32,
    // followed by name of namelen bytes
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_direntplus {
    pub(crate) entry_out: fuse_entry_out,
    pub(crate) dirent: fuse_dirent,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_notify_inval_inode_out {
    pub(crate) ino: u64,
    pub(crate) off: i64,
    pub(crate) len: i64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_notify_inval_entry_out {
    pub(crate) parent: u64,
    pub(crate) namelen: u32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_lseek_in {
    pub(crate) fh: u64,
    pub(crate) offset: i64,
    // NOTE: this field is defined as u32 in fuse_kernel.h in libfuse. However, it is treated as signed
    pub(crate) whence: i32,
    pub(crate) padding: u32,
}

#[repr(C)]
#[derive(Debug, IntoBytes, KnownLayout, Immutable)]
#[allow(
    dead_code,
    reason = "reply shape retained for default-ENOSYS lseek dispatch"
)]
pub(crate) struct fuse_lseek_out {
    pub(crate) offset: i64,
}

#[repr(C)]
#[derive(Debug, FromBytes, KnownLayout, Immutable)]
pub(crate) struct fuse_copy_file_range_in {
    pub(crate) fh_in: u64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    pub(crate) off_in: i64,
    pub(crate) nodeid_out: u64,
    pub(crate) fh_out: u64,
    // NOTE: this field is defined as u64 in fuse_kernel.h in libfuse. However, it is treated as signed
    pub(crate) off_out: i64,
    pub(crate) len: u64,
    pub(crate) flags: u64,
}
