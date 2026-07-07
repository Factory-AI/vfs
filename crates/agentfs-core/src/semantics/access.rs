//! POSIX access checks shared by every transport adapter.
//!
//! FUSE delegates enforcement to the kernel with `default_permissions`; NFS
//! cannot, so this module is the single server-side implementation of the same
//! owner/group/other, sticky-bit, setattr, and privilege-bit rules.

use crate::fs::{FsError, Stats};
use smallvec::SmallVec;

const S_IRUSR: u32 = 0o400;
const S_IWUSR: u32 = 0o200;
const S_IXUSR: u32 = 0o100;
const S_IRGRP: u32 = 0o040;
const S_IWGRP: u32 = 0o020;
const S_IXGRP: u32 = 0o010;
const S_IROTH: u32 = 0o004;
const S_IWOTH: u32 = 0o002;
const S_IXOTH: u32 = 0o001;
const S_ISUID: u32 = 0o4000;
const S_ISGID: u32 = 0o2000;
const S_ISVTX: u32 = 0o1000;
const S_IXUGO: u32 = S_IXUSR | S_IXGRP | S_IXOTH;
const KILLPRIV_BITS: u32 = S_ISUID | S_ISGID;
const PERMISSION_BITS: u32 = 0o7777;

/// Caller credentials used for POSIX permission checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Credentials {
    pub uid: u32,
    pub gid: u32,
    pub groups: SmallVec<[u32; 16]>,
}

/// Requested timestamp update in an attribute change.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TimeUpdate {
    #[default]
    Omit,
    Now,
    Explicit,
}

/// Attribute changes whose authorization depends on POSIX permissions.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AttrChange {
    pub size: bool,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub atime: TimeUpdate,
    pub mtime: TimeUpdate,
}

/// Return whether `creds` may read `stats`.
pub fn may_read(stats: &Stats, creds: &Credentials) -> bool {
    is_root(creds) || selected_bits(stats, creds).read
}

/// Return whether `creds` may write `stats`.
pub fn may_write(stats: &Stats, creds: &Credentials) -> bool {
    is_root(creds) || selected_bits(stats, creds).write
}

/// Return whether `creds` may search a directory or execute a file.
pub fn may_search(stats: &Stats, creds: &Credentials) -> bool {
    if is_root(creds) {
        return stats.is_directory() || (stats.mode & S_IXUGO) != 0;
    }
    selected_bits(stats, creds).execute
}

/// Return whether `creds` may delete or rename `victim` from `parent`.
pub fn sticky_delete_ok(parent: &Stats, victim: &Stats, creds: &Credentials) -> bool {
    if !may_modify_directory(parent, creds) {
        return false;
    }
    if (parent.mode & S_ISVTX) == 0 {
        return true;
    }
    is_root(creds) || creds.uid == parent.uid || creds.uid == victim.uid
}

/// Return whether `creds` may create, remove, or rename entries in `parent`.
pub fn may_modify_directory(parent: &Stats, creds: &Credentials) -> bool {
    may_write(parent, creds) && may_search(parent, creds)
}

/// Return whether `creds` may rename `victim` out of `parent`.
///
/// This is `sticky_delete_ok` plus the NFS server's POSIX guard for moving a
/// directory across parents: non-root callers must own the directory whose
/// `..` link is being changed.
pub fn sticky_rename_from_ok(
    parent: &Stats,
    victim: &Stats,
    creds: &Credentials,
    crosses_parent: bool,
) -> bool {
    if !sticky_delete_ok(parent, victim, creds) {
        return false;
    }
    if crosses_parent && victim.is_directory() && !is_root(creds) && !is_owner(victim, creds) {
        return false;
    }
    true
}

/// Validate whether `creds` may apply `change` to `stats`.
pub fn setattr_allowed(
    stats: &Stats,
    creds: &Credentials,
    change: &AttrChange,
) -> Result<(), FsError> {
    if change.size && !may_write(stats, creds) {
        return Err(FsError::PermissionDenied);
    }

    if change.mode.is_some() && !is_owner_or_root(stats, creds) {
        return Err(FsError::OperationNotPermitted);
    }

    if let Some(uid) = change.uid {
        if !is_root(creds) && uid != stats.uid {
            return Err(FsError::OperationNotPermitted);
        }
    }

    if let Some(gid) = change.gid {
        if !is_root(creds) && (!is_owner(stats, creds) || !is_in_group(creds, gid)) {
            return Err(FsError::OperationNotPermitted);
        }
    }

    if (change.atime == TimeUpdate::Explicit || change.mtime == TimeUpdate::Explicit)
        && !is_owner_or_root(stats, creds)
    {
        return Err(FsError::OperationNotPermitted);
    }

    if (change.atime == TimeUpdate::Now || change.mtime == TimeUpdate::Now)
        && !is_owner_or_root(stats, creds)
        && !may_write(stats, creds)
    {
        return Err(FsError::PermissionDenied);
    }

    Ok(())
}

/// Mode bits to clear when file contents, size, or ownership change.
pub fn killpriv_mask(stats: &Stats, _creds: &Credentials) -> u32 {
    if !stats.is_file() {
        return 0;
    }
    stats.mode & KILLPRIV_BITS
}

/// Apply POSIX chmod normalization, including SGID clearing for non-members.
pub fn normalize_setattr_mode(
    stats: &Stats,
    creds: &Credentials,
    change: &AttrChange,
) -> Option<u32> {
    let mut mode = change.mode? & PERMISSION_BITS;
    if !is_root(creds) && (mode & S_ISGID) != 0 {
        let effective_gid = change.gid.unwrap_or(stats.gid);
        if !is_in_group(creds, effective_gid) {
            mode &= !S_ISGID;
        }
    }
    Some(mode)
}

/// Clear privilege bits from `mode` according to [`killpriv_mask`].
pub fn without_killpriv(stats: &Stats, creds: &Credentials, mode: u32) -> u32 {
    mode & !killpriv_mask(stats, creds)
}

/// Return whether `creds` is root.
pub fn is_root(creds: &Credentials) -> bool {
    creds.uid == 0
}

/// Return whether `creds` owns `stats`.
pub fn is_owner(stats: &Stats, creds: &Credentials) -> bool {
    creds.uid == stats.uid
}

/// Return whether `creds` owns `stats` or is root.
pub fn is_owner_or_root(stats: &Stats, creds: &Credentials) -> bool {
    is_root(creds) || is_owner(stats, creds)
}

/// Return whether `creds` belongs to `gid`.
pub fn is_in_group(creds: &Credentials, gid: u32) -> bool {
    creds.gid == gid || creds.groups.contains(&gid)
}

#[derive(Clone, Copy)]
struct Rwx {
    read: bool,
    write: bool,
    execute: bool,
}

fn selected_bits(stats: &Stats, creds: &Credentials) -> Rwx {
    let (read, write, execute) = if creds.uid == stats.uid {
        (S_IRUSR, S_IWUSR, S_IXUSR)
    } else if is_in_group(creds, stats.gid) {
        (S_IRGRP, S_IWGRP, S_IXGRP)
    } else {
        (S_IROTH, S_IWOTH, S_IXOTH)
    };

    Rwx {
        read: (stats.mode & read) != 0,
        write: (stats.mode & write) != 0,
        execute: (stats.mode & execute) != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{S_IFDIR, S_IFREG};
    fn creds(uid: u32, gid: u32, groups: &[u32]) -> Credentials {
        Credentials {
            uid,
            gid,
            groups: SmallVec::from_slice(groups),
        }
    }

    fn stats(mode: u32, uid: u32, gid: u32) -> Stats {
        Stats {
            ino: 2,
            mode,
            nlink: 1,
            uid,
            gid,
            size: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            atime_nsec: 0,
            mtime_nsec: 0,
            ctime_nsec: 0,
            rdev: 0,
        }
    }

    #[test]
    fn conformance_table_matches_kernel_default_permissions() {
        struct Case {
            name: &'static str,
            stats: Stats,
            creds: Credentials,
            read: bool,
            write: bool,
            search: bool,
        }

        let cases = [
            Case {
                name: "owner read write execute",
                stats: stats(S_IFREG | 0o700, 1000, 2000),
                creds: creds(1000, 9999, &[]),
                read: true,
                write: true,
                search: true,
            },
            Case {
                name: "matching primary group",
                stats: stats(S_IFREG | 0o070, 3000, 2000),
                creds: creds(1000, 2000, &[]),
                read: true,
                write: true,
                search: true,
            },
            Case {
                name: "matching auxiliary group",
                stats: stats(S_IFREG | 0o040, 3000, 2000),
                creds: creds(1000, 1000, &[2000]),
                read: true,
                write: false,
                search: false,
            },
            Case {
                name: "auxiliary group execute only directory",
                stats: stats(S_IFDIR | 0o010, 3000, 2000),
                creds: creds(1000, 1000, &[2000]),
                read: false,
                write: false,
                search: true,
            },
            Case {
                name: "primary group write without execute directory",
                stats: stats(S_IFDIR | 0o020, 3000, 2000),
                creds: creds(1000, 2000, &[]),
                read: false,
                write: true,
                search: false,
            },
            Case {
                name: "other write only",
                stats: stats(S_IFREG | 0o002, 2000, 2000),
                creds: creds(1000, 1000, &[]),
                read: false,
                write: true,
                search: false,
            },
            Case {
                name: "other execute only file",
                stats: stats(S_IFREG | 0o001, 2000, 2000),
                creds: creds(1000, 1000, &[]),
                read: false,
                write: false,
                search: true,
            },
            Case {
                name: "root reads and writes mode zero file",
                stats: stats(S_IFREG, 1000, 1000),
                creds: creds(0, 0, &[]),
                read: true,
                write: true,
                search: false,
            },
            Case {
                name: "root searches mode zero directory",
                stats: stats(S_IFDIR, 1000, 1000),
                creds: creds(0, 0, &[]),
                read: true,
                write: true,
                search: true,
            },
        ];

        println!("shared semantics::access conformance cases");
        let mut mismatches = 0usize;
        for case in cases {
            let got = (
                may_read(&case.stats, &case.creds),
                may_write(&case.stats, &case.creds),
                may_search(&case.stats, &case.creds),
            );
            let expected = (case.read, case.write, case.search);
            println!(
                "{}: expected=({},{},{}) access=({},{},{})",
                case.name, expected.0, expected.1, expected.2, got.0, got.1, got.2
            );
            if got != expected {
                mismatches += 1;
            }
        }
        println!("semantics::access conformance mismatches={mismatches}");
        assert_eq!(mismatches, 0);
    }

    #[test]
    fn sticky_delete_and_killpriv_cases_match_posix() {
        let parent = stats(S_IFDIR | 0o1777, 10, 10);
        let victim = stats(S_IFREG | 0o644, 20, 20);

        assert!(sticky_delete_ok(&parent, &victim, &creds(10, 10, &[])));
        assert!(sticky_delete_ok(&parent, &victim, &creds(20, 20, &[])));
        assert!(!sticky_delete_ok(&parent, &victim, &creds(30, 30, &[])));

        let privileged = stats(S_IFREG | 0o6755, 20, 20);
        assert_eq!(killpriv_mask(&privileged, &creds(1000, 1000, &[])), 0o6000);
        assert_eq!(
            killpriv_mask(&privileged, &creds(0, 0, &[])),
            0o6000,
            "root chown/truncate follows Linux killpriv behavior for NFS SETATTR"
        );
        assert_eq!(
            killpriv_mask(&stats(S_IFDIR | 0o6755, 20, 20), &creds(0, 0, &[])),
            0,
            "killpriv only applies to regular files"
        );
    }

    #[test]
    fn setattr_authorization_cases_match_kernel_rules() {
        let owned = stats(S_IFREG | 0o644, 1000, 2000);
        let other = creds(3000, 3000, &[]);
        let owner = creds(1000, 1000, &[2000]);

        assert!(setattr_allowed(
            &owned,
            &owner,
            &AttrChange {
                mode: Some(0o600),
                ..Default::default()
            }
        )
        .is_ok());
        assert!(setattr_allowed(
            &owned,
            &other,
            &AttrChange {
                mode: Some(0o600),
                ..Default::default()
            }
        )
        .is_err());
        assert!(setattr_allowed(
            &owned,
            &owner,
            &AttrChange {
                gid: Some(2000),
                ..Default::default()
            }
        )
        .is_ok());
        assert!(setattr_allowed(
            &owned,
            &owner,
            &AttrChange {
                gid: Some(3000),
                ..Default::default()
            }
        )
        .is_err());
        assert!(setattr_allowed(
            &owned,
            &owner,
            &AttrChange {
                atime: TimeUpdate::Explicit,
                ..Default::default()
            }
        )
        .is_ok());
    }
}
