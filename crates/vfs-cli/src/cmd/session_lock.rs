//! Cross-process session locking shared by `vfs run` and `vfs pack`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Advisory lock held for the lifetime of a run or pack operation.
pub(crate) struct SessionLock {
    _file: File,
}

impl SessionLock {
    /// Acquire a shared lock for a run owner or joiner.
    pub(crate) fn try_shared(session_dir: &Path) -> io::Result<Self> {
        Self::try_acquire(session_dir, libc::LOCK_SH)
    }

    /// Acquire an exclusive lock for pack.
    pub(crate) fn try_exclusive(session_dir: &Path) -> io::Result<Self> {
        Self::try_acquire(session_dir, libc::LOCK_EX)
    }

    fn try_acquire(session_dir: &Path, mode: libc::c_int) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(session_dir.join(".session.lock"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn exclusive_pack_lock_waits_for_all_run_locks() {
        let dir = tempdir().unwrap();
        let first_run = SessionLock::try_shared(dir.path()).unwrap();
        let second_run = SessionLock::try_shared(dir.path()).unwrap();
        assert_eq!(
            SessionLock::try_exclusive(dir.path()).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );

        drop(first_run);
        drop(second_run);
        SessionLock::try_exclusive(dir.path()).unwrap();
    }
}
