//! NFS-facing shim over the shared AgentFS access semantics.
//!
//! POSIX permission logic lives in `agentfs_core::semantics::access`. This
//! module only converts NFS wire types to core credentials/stats and maps the
//! shared answers back to RFC 1813 bitmasks/status codes.

use super::nfs::{
    fattr3, ftype3, nfsstat3, sattr3, set_atime, set_gid3, set_mode3, set_mtime, set_size3,
    set_uid3,
};
use super::rpc::auth_unix;
use agentfs_core::fs::FsError;
use agentfs_core::semantics::access::{self, AttrChange, Credentials, TimeUpdate};
use agentfs_core::{Stats, S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, S_IFSOCK};
use smallvec::SmallVec;

/// NFS ACCESS procedure permission bits (from RFC 1813)
pub const ACCESS3_READ: u32 = 0x0001;
pub const ACCESS3_LOOKUP: u32 = 0x0002;
pub const ACCESS3_MODIFY: u32 = 0x0004;
pub const ACCESS3_EXTEND: u32 = 0x0008;
pub const ACCESS3_DELETE: u32 = 0x0010;
pub const ACCESS3_EXECUTE: u32 = 0x0020;

/// Convert AUTH_UNIX credentials to core access credentials.
pub fn credentials(auth: &auth_unix) -> Credentials {
    Credentials {
        uid: auth.uid,
        gid: auth.gid,
        groups: SmallVec::from_slice(&auth.gids),
    }
}

/// Convert NFS attributes to core `Stats` for access checks.
pub fn stats(attr: &fattr3) -> Stats {
    Stats {
        ino: attr.fileid as i64,
        mode: file_type_mode(attr.ftype) | (attr.mode & 0o7777),
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        size: attr.size as i64,
        atime: attr.atime.seconds as i64,
        mtime: attr.mtime.seconds as i64,
        ctime: attr.ctime.seconds as i64,
        atime_nsec: attr.atime.nseconds,
        mtime_nsec: attr.mtime.nseconds,
        ctime_nsec: attr.ctime.nseconds,
        rdev: libc::makedev(attr.rdev.specdata1 as _, attr.rdev.specdata2 as _) as u64,
    }
}

/// Convert an NFS setattr payload to the shared access authorization shape.
pub fn attr_change(attr: &sattr3) -> AttrChange {
    AttrChange {
        size: matches!(attr.size, set_size3::size(_)),
        mode: match attr.mode {
            set_mode3::mode(mode) => Some(mode),
            set_mode3::Void => None,
        },
        uid: match attr.uid {
            set_uid3::uid(uid) => Some(uid),
            set_uid3::Void => None,
        },
        gid: match attr.gid {
            set_gid3::gid(gid) => Some(gid),
            set_gid3::Void => None,
        },
        atime: match attr.atime {
            set_atime::SET_TO_CLIENT_TIME(_) => TimeUpdate::Explicit,
            set_atime::SET_TO_SERVER_TIME => TimeUpdate::Now,
            set_atime::DONT_CHANGE => TimeUpdate::Omit,
        },
        mtime: match attr.mtime {
            set_mtime::SET_TO_CLIENT_TIME(_) => TimeUpdate::Explicit,
            set_mtime::SET_TO_SERVER_TIME => TimeUpdate::Now,
            set_mtime::DONT_CHANGE => TimeUpdate::Omit,
        },
    }
}

/// Map shared access-denial errors to NFS status codes.
pub fn denial_status(error: &FsError) -> nfsstat3 {
    match error {
        FsError::OperationNotPermitted => nfsstat3::NFS3ERR_PERM,
        FsError::PermissionDenied => nfsstat3::NFS3ERR_ACCES,
        _ => nfsstat3::NFS3ERR_ACCES,
    }
}

/// Apply shared chmod normalization to an NFS setattr payload.
pub fn normalize_setattr_mode(
    stats: &Stats,
    creds: &Credentials,
    change: &AttrChange,
) -> Option<u32> {
    access::normalize_setattr_mode(stats, creds, change)
}

/// Clear write/chown privilege bits according to shared access semantics.
pub fn without_killpriv(stats: &Stats, creds: &Credentials, mode: u32) -> u32 {
    access::without_killpriv(stats, creds, mode)
}

/// Compute the ACCESS3 result bitmask for the given auth and file attributes.
///
/// This implements RFC 1813 ACCESS procedure semantics:
/// - ACCESS3_READ: read file data or directory contents
/// - ACCESS3_LOOKUP: search directory entries (execute permission on directories)
/// - ACCESS3_MODIFY: alter existing file/directory data
/// - ACCESS3_EXTEND: add new data or directory entries
/// - ACCESS3_DELETE: remove directory entries (checked against parent directory)
/// - ACCESS3_EXECUTE: execute files (execute permission on files)
pub fn compute_access(auth: &auth_unix, attr: &fattr3, requested: u32) -> u32 {
    let mut result = 0u32;
    let creds = credentials(auth);
    let stats = stats(attr);

    // ACCESS3_READ - read file data or directory contents
    if (requested & ACCESS3_READ) != 0 && access::may_read(&stats, &creds) {
        result |= ACCESS3_READ;
    }

    // ACCESS3_LOOKUP - search directory (execute permission on directories)
    if (requested & ACCESS3_LOOKUP) != 0
        && stats.is_directory()
        && access::may_search(&stats, &creds)
    {
        result |= ACCESS3_LOOKUP;
    }

    // ACCESS3_MODIFY - alter existing data (write permission)
    if (requested & ACCESS3_MODIFY) != 0 && access::may_write(&stats, &creds) {
        result |= ACCESS3_MODIFY;
    }

    // ACCESS3_EXTEND - add new data (write permission)
    if (requested & ACCESS3_EXTEND) != 0 && access::may_write(&stats, &creds) {
        result |= ACCESS3_EXTEND;
    }

    // ACCESS3_DELETE - for non-directory files, always 0 (per RFC 1813)
    // For directories, this would need to check parent directory permissions
    // which is handled at the operation level, not here
    if (requested & ACCESS3_DELETE) != 0 {
        // DELETE permission is checked at operation time against the parent directory
        // For the ACCESS procedure, we return 0 for files (per UNIX semantics)
        // and the directory's write permission for directories
        if stats.is_directory() && access::may_write(&stats, &creds) {
            result |= ACCESS3_DELETE;
        }
    }

    // ACCESS3_EXECUTE - execute files (not directories)
    if (requested & ACCESS3_EXECUTE) != 0
        && !stats.is_directory()
        && access::may_search(&stats, &creds)
    {
        result |= ACCESS3_EXECUTE;
    }

    result
}

fn file_type_mode(ftype: ftype3) -> u32 {
    match ftype {
        ftype3::NF3REG => S_IFREG,
        ftype3::NF3DIR => S_IFDIR,
        ftype3::NF3BLK => S_IFBLK,
        ftype3::NF3CHR => S_IFCHR,
        ftype3::NF3LNK => S_IFLNK,
        ftype3::NF3SOCK => S_IFSOCK,
        ftype3::NF3FIFO => S_IFIFO,
    }
}
