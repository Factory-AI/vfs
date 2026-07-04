use crate::error::{Error, Result};
use crate::fs::{File, Stats};
use async_trait::async_trait;
use std::os::unix::io::{AsRawFd, OwnedFd};

/// An open file handle for HostFS (real fd for I/O).
pub(super) struct HostFSFile {
    /// Real file descriptor for read/write operations.
    pub(super) fd: OwnedFd,
}

#[async_trait]
impl File for HostFSFile {
    async fn pread(&self, offset: u64, size: u64) -> Result<Vec<u8>> {
        let fd = self.fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; size as usize];
            let n = unsafe {
                libc::pread(
                    fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    size as usize,
                    offset as libc::off_t,
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            buf.truncate(n as usize);
            Ok(buf)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn pwrite(&self, offset: u64, data: &[u8]) -> Result<()> {
        let fd = self.fd.as_raw_fd();
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            let n = unsafe {
                libc::pwrite(
                    fd,
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    offset as libc::off_t,
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn truncate(&self, size: u64) -> Result<()> {
        let fd = self.fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let result = unsafe { libc::ftruncate(fd, size as libc::off_t) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn fsync(&self) -> Result<()> {
        let fd = self.fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let result = unsafe { libc::fsync(fd) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    async fn fstat(&self) -> Result<Stats> {
        let fd = self.fd.as_raw_fd();
        tokio::task::spawn_blocking(move || {
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let result = unsafe { libc::fstat(fd, &mut stat) };
            if result < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(stat_to_stats(&stat))
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }
}

/// Convert libc::stat to our Stats struct.
#[cfg(target_os = "linux")]
pub(super) fn stat_to_stats(stat: &libc::stat) -> Stats {
    Stats {
        ino: stat.st_ino as i64,
        mode: stat.st_mode,
        nlink: stat.st_nlink as u32,
        uid: stat.st_uid,
        gid: stat.st_gid,
        size: stat.st_size,
        atime: stat.st_atime,
        mtime: stat.st_mtime,
        ctime: stat.st_ctime,
        atime_nsec: stat.st_atime_nsec as u32,
        mtime_nsec: stat.st_mtime_nsec as u32,
        ctime_nsec: stat.st_ctime_nsec as u32,
        rdev: stat.st_rdev,
    }
}

/// Convert libc::stat to our Stats struct.
#[cfg(target_os = "macos")]
pub(super) fn stat_to_stats(stat: &libc::stat) -> Stats {
    Stats {
        ino: stat.st_ino as i64,
        mode: stat.st_mode as u32,
        nlink: stat.st_nlink as u32,
        uid: stat.st_uid,
        gid: stat.st_gid,
        size: stat.st_size,
        atime: stat.st_atime,
        mtime: stat.st_mtime,
        ctime: stat.st_ctime,
        atime_nsec: stat.st_atime_nsec as u32,
        mtime_nsec: stat.st_mtime_nsec as u32,
        ctime_nsec: stat.st_ctime_nsec as u32,
        rdev: stat.st_rdev as u64,
    }
}
