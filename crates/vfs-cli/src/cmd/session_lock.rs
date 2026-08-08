//! Cross-process session locking shared by `vfs run`, `vfs seed`, and `vfs pack`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Advisory lock held for the lifetime of a run, seed, or pack operation.
pub(crate) struct SessionLock {
    _file: File,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let _ = unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

impl SessionLock {
    /// Path of the advisory lock file guarding `session_dir`.
    ///
    /// This module owns the lock file name; callers that need to test for its
    /// presence ask here rather than joining the literal themselves.
    pub(crate) fn lock_path(session_dir: &Path) -> PathBuf {
        session_dir.join(".session.lock")
    }

    /// Acquire a shared lock for a run owner or joiner.
    pub(crate) fn try_shared(session_dir: &Path) -> io::Result<Self> {
        Self::try_acquire(session_dir, libc::LOCK_SH)
    }

    /// Acquire an exclusive lock for seed or pack.
    pub(crate) fn try_exclusive(session_dir: &Path) -> io::Result<Self> {
        Self::try_acquire(session_dir, libc::LOCK_EX)
    }

    /// Atomically downgrade an exclusive seed lock to the run lifetime lock.
    pub(crate) fn downgrade_to_shared(self) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            if unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_SH) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(self)
    }

    fn try_acquire(session_dir: &Path, mode: libc::c_int) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_CLOEXEC);
        }
        let file = options.open(Self::lock_path(session_dir))?;
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

    #[test]
    fn seed_lock_downgrades_without_releasing_session_ownership() {
        let dir = tempdir().unwrap();
        let seed = SessionLock::try_exclusive(dir.path()).unwrap();
        let run = seed.downgrade_to_shared().unwrap();
        let joiner = SessionLock::try_shared(dir.path()).unwrap();
        assert_eq!(
            SessionLock::try_exclusive(dir.path()).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(run);
        drop(joiner);
        SessionLock::try_exclusive(dir.path()).unwrap();
    }
}
