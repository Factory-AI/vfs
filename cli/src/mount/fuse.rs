//! FUSE backend implementation for the mount infrastructure.

use anyhow::Result;
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Instant;
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::{wait_for_mount, MountBackend, MountHandle, MountHandleInner, MountOpts};

/// FUSE unmount implementation using fusermount.
pub(super) fn unmount_fuse(mountpoint: &Path, lazy: bool) -> Result<()> {
    const FUSERMOUNT_COMMANDS: &[&str] = &["fusermount3", "fusermount"];
    let args: &[&str] = if lazy { &["-uz"] } else { &["-u"] };

    for cmd in FUSERMOUNT_COMMANDS {
        let result = Command::new(cmd)
            .args(args)
            .arg(mountpoint.as_os_str())
            .status();

        match result {
            Ok(status) if status.success() => return Ok(()),
            Ok(_) => continue,
            Err(_) => continue,
        }
    }

    anyhow::bail!(
        "Failed to unmount {}. You may need to unmount manually with: fusermount -u {}",
        mountpoint.display(),
        mountpoint.display()
    )
}

/// Internal FUSE mount implementation.
pub(super) fn mount_fuse(
    fs: Arc<dyn agentfs_sdk::FileSystem>,
    opts: MountOpts,
) -> Result<MountHandle> {
    use crate::fuse::FuseMountOptions;

    let fuse_opts = FuseMountOptions {
        mountpoint: opts.mountpoint.clone(),
        auto_unmount: opts.auto_unmount,
        allow_root: opts.allow_root,
        allow_other: opts.allow_other,
        fsname: opts.fsname.clone(),
        uid: opts.uid,
        gid: opts.gid,
    };

    let mountpoint = opts.mountpoint.clone();
    let timeout = opts.timeout;
    let lazy_unmount = opts.lazy_unmount;

    let fs_arc: Arc<dyn agentfs_sdk::FileSystem> = Arc::new(ReadWriteLaneFsAdapter::new(fs));

    let fuse_handle = std::thread::spawn(move || {
        let rt = crate::get_runtime();
        crate::fuse::mount(fs_arc, fuse_opts, rt)
    });

    if !wait_for_mount(&mountpoint, timeout) {
        anyhow::bail!("FUSE mount did not become ready within {:?}", timeout);
    }

    Ok(MountHandle {
        mountpoint,
        backend: MountBackend::Fuse,
        lazy_unmount,
        inner: MountHandleInner::Fuse {
            thread: Some(fuse_handle),
        },
    })
}

/// Adapter that admits read operations concurrently while serializing
/// mutations before they reach the backend filesystem.
struct ReadWriteLaneFsAdapter {
    inner: Arc<dyn agentfs_sdk::FileSystem>,
    lanes: RwLock<()>,
    active_reads: AtomicU64,
}

struct ReadLaneGuard<'a> {
    active_reads: &'a AtomicU64,
    _guard: RwLockReadGuard<'a, ()>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FuseFsOperationClass {
    PureRead,
    Mutation,
}

impl Drop for ReadLaneGuard<'_> {
    fn drop(&mut self) {
        self.active_reads.fetch_sub(1, Ordering::Relaxed);
    }
}

impl ReadWriteLaneFsAdapter {
    fn new(inner: Arc<dyn agentfs_sdk::FileSystem>) -> Self {
        Self {
            inner,
            lanes: RwLock::new(()),
            active_reads: AtomicU64::new(0),
        }
    }

    async fn enter_read_lane(&self) -> ReadLaneGuard<'_> {
        let started = agentfs_sdk::profiling::is_enabled().then(Instant::now);
        let guard = self.lanes.read().await;
        if let Some(started) = started {
            agentfs_sdk::profiling::record_fuse_read_lane_wait(started.elapsed());
        }

        let active_reads = self.active_reads.fetch_add(1, Ordering::Relaxed) + 1;
        agentfs_sdk::profiling::record_fuse_read_lane_concurrency(active_reads);

        ReadLaneGuard {
            active_reads: &self.active_reads,
            _guard: guard,
        }
    }

    async fn enter_write_lane(&self) -> RwLockWriteGuard<'_, ()> {
        let started = agentfs_sdk::profiling::is_enabled().then(Instant::now);
        let guard = self.lanes.write().await;
        if let Some(started) = started {
            agentfs_sdk::profiling::record_fuse_write_lane_wait(started.elapsed());
        }
        guard
    }

    async fn lock_read_fs(&self) -> ReadLaneGuard<'_> {
        self.enter_read_lane().await
    }

    async fn lock_write_fs(&self) -> RwLockWriteGuard<'_, ()> {
        self.enter_write_lane().await
    }
}

#[async_trait::async_trait]
impl agentfs_sdk::FileSystem for ReadWriteLaneFsAdapter {
    async fn lookup(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<Option<agentfs_sdk::Stats>, agentfs_sdk::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.lookup(parent_ino, name).await
    }

    async fn getattr(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<agentfs_sdk::Stats>, agentfs_sdk::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.getattr(ino).await
    }

    async fn readlink(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<String>, agentfs_sdk::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.readlink(ino).await
    }

    async fn readdir(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<Vec<String>>, agentfs_sdk::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.readdir(ino).await
    }

    async fn readdir_plus(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<Vec<agentfs_sdk::DirEntry>>, agentfs_sdk::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.readdir_plus(ino).await
    }

    async fn chmod(
        &self,
        ino: i64,
        mode: u32,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.chmod(ino, mode).await
    }

    async fn chown(
        &self,
        ino: i64,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.chown(ino, uid, gid).await
    }

    async fn utimens(
        &self,
        ino: i64,
        atime: agentfs_sdk::TimeChange,
        mtime: agentfs_sdk::TimeChange,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.utimens(ino, atime, mtime).await
    }

    async fn open(
        &self,
        ino: i64,
        flags: i32,
    ) -> std::result::Result<agentfs_sdk::BoxedFile, agentfs_sdk::error::Error> {
        match classify_open(flags) {
            FuseFsOperationClass::PureRead => {
                let _lane = self.lock_read_fs().await;
                self.inner.open(ino, flags).await
            }
            FuseFsOperationClass::Mutation => {
                let _lane = self.lock_write_fs().await;
                self.inner.open(ino, flags).await
            }
        }
    }

    async fn keep_cache_for_read_open(
        &self,
        ino: i64,
        flags: i32,
    ) -> std::result::Result<Option<agentfs_sdk::Stats>, agentfs_sdk::error::Error> {
        match classify_open(flags) {
            FuseFsOperationClass::PureRead => {
                let _lane = self.lock_read_fs().await;
                self.inner.keep_cache_for_read_open(ino, flags).await
            }
            FuseFsOperationClass::Mutation => {
                let _lane = self.lock_write_fs().await;
                self.inner.keep_cache_for_read_open(ino, flags).await
            }
        }
    }

    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<agentfs_sdk::Stats, agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.mkdir(parent_ino, name, mode, uid, gid).await
    }

    async fn create_file(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<(agentfs_sdk::Stats, agentfs_sdk::BoxedFile), agentfs_sdk::error::Error>
    {
        let _lane = self.lock_write_fs().await;
        self.inner
            .create_file(parent_ino, name, mode, uid, gid)
            .await
    }

    async fn mknod(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        rdev: u64,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<agentfs_sdk::Stats, agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner
            .mknod(parent_ino, name, mode, rdev, uid, gid)
            .await
    }

    async fn symlink(
        &self,
        parent_ino: i64,
        name: &str,
        target: &str,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<agentfs_sdk::Stats, agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.symlink(parent_ino, name, target, uid, gid).await
    }

    async fn unlink(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.unlink(parent_ino, name).await
    }

    async fn rmdir(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.rmdir(parent_ino, name).await
    }

    async fn link(
        &self,
        ino: i64,
        newparent_ino: i64,
        newname: &str,
    ) -> std::result::Result<agentfs_sdk::Stats, agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.link(ino, newparent_ino, newname).await
    }

    async fn rename(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner
            .rename(oldparent_ino, oldname, newparent_ino, newname)
            .await
    }

    async fn statfs(
        &self,
    ) -> std::result::Result<agentfs_sdk::FilesystemStats, agentfs_sdk::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.statfs().await
    }

    async fn drain_inode_writes(
        &self,
        ino: i64,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.drain_inode_writes(ino).await
    }

    async fn drain_all(&self) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.drain_all().await
    }

    async fn finalize(&self) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.finalize().await
    }

    async fn retain_lookup(
        &self,
        ino: i64,
        nlookup: u64,
    ) -> std::result::Result<(), agentfs_sdk::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.retain_lookup(ino, nlookup).await
    }

    async fn forget(&self, ino: i64, nlookup: u64) {
        let _lane = self.lock_write_fs().await;
        self.inner.forget(ino, nlookup).await;
    }
}

fn classify_open(flags: i32) -> FuseFsOperationClass {
    if (flags & libc::O_ACCMODE) == libc::O_RDONLY && (flags & libc::O_TRUNC) == 0 {
        FuseFsOperationClass::PureRead
    } else {
        FuseFsOperationClass::Mutation
    }
}
