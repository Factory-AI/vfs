//! FUSE backend implementation for the mount infrastructure.

use anyhow::Result;
use std::path::Path;
use std::process::Command;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::{wait_for_mount, Backend, MountHandle, MountHandleInner, MountOpts};

/// Extra margin past `MountOpts::timeout` for the readiness probe channel:
/// the probe thread itself bounds at `timeout` unless its stat call wedges.
const READY_PROBE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// How long the failure path waits for best-effort session teardown before
/// surfacing the readiness error anyway.
const READY_TEARDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

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
    fs: Arc<dyn agentfs_core::FileSystem>,
    opts: MountOpts,
) -> Result<MountHandle> {
    use agentfs_fuse::FuseMountOptions;

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

    let fs_arc: Arc<dyn agentfs_core::FileSystem> = Arc::new(ReadWriteLaneFsAdapter::new(fs));

    let rt = crate::get_runtime();
    let mut fuse_session = agentfs_fuse::mount(fs_arc, fuse_opts, rt)?;

    // The readiness probe stats the mountpoint, which can block indefinitely
    // when a fresh fuse-over-io_uring mount races the kernel-side drain of a
    // just-closed connection and post-INIT requests stall (docs/MANUAL.md,
    // "FUSE-over-io_uring"). Probing from a helper thread keeps this wait
    // bounded either way, so a wedged mount surfaces as an error.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let probe_mountpoint = mountpoint.clone();
    let _ = std::thread::Builder::new()
        .name("agentfs-mount-ready".into())
        .spawn(move || {
            let _ = ready_tx.send(wait_for_mount(&probe_mountpoint, timeout));
        });
    let ready = ready_rx
        .recv_timeout(timeout + READY_PROBE_GRACE)
        .unwrap_or(false);

    if !ready {
        // Teardown from a helper thread too: joining a wedged session could
        // block, and the caller is owed a bounded, clear failure.
        let (teardown_tx, teardown_rx) = std::sync::mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("agentfs-mount-teardown".into())
            .spawn(move || {
                if let Err(error) = fuse_session.unmount() {
                    tracing::warn!(%error, "failed to unmount FUSE session after readiness timeout");
                }
                if let Err(error) = fuse_session.join() {
                    tracing::warn!(%error, "FUSE session exited with error after readiness timeout");
                }
                let _ = teardown_tx.send(());
            });
        let _ = teardown_rx.recv_timeout(READY_TEARDOWN_GRACE);
        anyhow::bail!(
            "FUSE mount at {} did not become ready within {:?}. On kernels with \
             fuse-over-io_uring enabled, a mount racing the drain of a just-closed \
             FUSE connection can stall; retry shortly, avoid rapid \
             unmount-then-mount cycles, or set AGENTFS_FUSE_URING=0.",
            mountpoint.display(),
            timeout
        );
    }

    Ok(MountHandle {
        mountpoint,
        backend: Backend::Fuse,
        lazy_unmount,
        inner: MountHandleInner::Fuse {
            session: Some(fuse_session),
        },
    })
}

/// Adapter that admits read operations concurrently while serializing
/// mutations before they reach the backend filesystem.
struct ReadWriteLaneFsAdapter {
    inner: Arc<dyn agentfs_core::FileSystem>,
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
    fn new(inner: Arc<dyn agentfs_core::FileSystem>) -> Self {
        Self {
            inner,
            lanes: RwLock::new(()),
            active_reads: AtomicU64::new(0),
        }
    }

    async fn enter_read_lane(&self) -> ReadLaneGuard<'_> {
        let guard = self.lanes.read().await;

        self.active_reads.fetch_add(1, Ordering::Relaxed);

        ReadLaneGuard {
            active_reads: &self.active_reads,
            _guard: guard,
        }
    }

    async fn enter_write_lane(&self) -> RwLockWriteGuard<'_, ()> {
        self.lanes.write().await
    }

    async fn lock_read_fs(&self) -> ReadLaneGuard<'_> {
        self.enter_read_lane().await
    }

    async fn lock_write_fs(&self) -> RwLockWriteGuard<'_, ()> {
        self.enter_write_lane().await
    }
}

#[async_trait::async_trait]
impl agentfs_core::FileSystem for ReadWriteLaneFsAdapter {
    async fn lookup(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<Option<agentfs_core::Stats>, agentfs_core::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.lookup(parent_ino, name).await
    }

    async fn getattr(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<agentfs_core::Stats>, agentfs_core::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.getattr(ino).await
    }

    async fn readlink(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<String>, agentfs_core::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.readlink(ino).await
    }

    async fn readdir(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<Vec<String>>, agentfs_core::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.readdir(ino).await
    }

    async fn readdir_plus(
        &self,
        ino: i64,
    ) -> std::result::Result<Option<Vec<agentfs_core::DirEntry>>, agentfs_core::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.readdir_plus(ino).await
    }

    async fn chmod(
        &self,
        ino: i64,
        mode: u32,
    ) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.chmod(ino, mode).await
    }

    async fn chown(
        &self,
        ino: i64,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.chown(ino, uid, gid).await
    }

    async fn utimens(
        &self,
        ino: i64,
        atime: agentfs_core::TimeChange,
        mtime: agentfs_core::TimeChange,
    ) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.utimens(ino, atime, mtime).await
    }

    async fn open(
        &self,
        ino: i64,
        flags: i32,
    ) -> std::result::Result<agentfs_core::BoxedFile, agentfs_core::error::Error> {
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
    ) -> std::result::Result<Option<agentfs_core::Stats>, agentfs_core::error::Error> {
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

    fn delta_keep_cache_fast_path(&self) -> bool {
        self.inner.delta_keep_cache_fast_path()
    }

    fn kernel_cache_policy(&self, ino: i64) -> agentfs_core::fs::KernelCachePolicy {
        self.inner.kernel_cache_policy(ino)
    }

    async fn mkdir(
        &self,
        parent_ino: i64,
        name: &str,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> std::result::Result<agentfs_core::Stats, agentfs_core::error::Error> {
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
    ) -> std::result::Result<
        (agentfs_core::Stats, agentfs_core::BoxedFile),
        agentfs_core::error::Error,
    > {
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
    ) -> std::result::Result<agentfs_core::Stats, agentfs_core::error::Error> {
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
    ) -> std::result::Result<agentfs_core::Stats, agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.symlink(parent_ino, name, target, uid, gid).await
    }

    async fn unlink(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.unlink(parent_ino, name).await
    }

    async fn rmdir(
        &self,
        parent_ino: i64,
        name: &str,
    ) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.rmdir(parent_ino, name).await
    }

    async fn link(
        &self,
        ino: i64,
        newparent_ino: i64,
        newname: &str,
    ) -> std::result::Result<agentfs_core::Stats, agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.link(ino, newparent_ino, newname).await
    }

    async fn rename(
        &self,
        oldparent_ino: i64,
        oldname: &str,
        newparent_ino: i64,
        newname: &str,
    ) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner
            .rename(oldparent_ino, oldname, newparent_ino, newname)
            .await
    }

    async fn statfs(
        &self,
    ) -> std::result::Result<agentfs_core::FilesystemStats, agentfs_core::error::Error> {
        let _lane = self.lock_read_fs().await;
        self.inner.statfs().await
    }

    async fn drain_inode_writes(
        &self,
        ino: i64,
    ) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.drain_inode_writes(ino).await
    }

    async fn drain_all(&self) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.drain_all().await
    }

    async fn finalize(&self) -> std::result::Result<(), agentfs_core::error::Error> {
        let _lane = self.lock_write_fs().await;
        self.inner.finalize().await
    }

    async fn retain_lookup(
        &self,
        ino: i64,
        nlookup: u64,
    ) -> std::result::Result<(), agentfs_core::error::Error> {
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
