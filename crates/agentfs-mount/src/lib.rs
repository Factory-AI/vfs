//! Mount lifecycle infrastructure for AgentFS.
//!
//! This module provides a unified mount API that abstracts over FUSE and NFS backends.
//! The `mount_fs()` function returns a `MountHandle` whose explicit `unmount`
//! path drains and joins backend-owned work before returning. The exported
//! surface is the mount lifecycle only: `mount_fs`/`MountOpts`/`MountHandle`,
//! the `supervise` module (the one supervision path for every mount-owning
//! command), the `daemon` module, and mountpoint helpers.
//!
//! Owned invariants:
//!
//! - One mount lifecycle: every backend mount is created through `mount_fs`
//!   and torn down through `MountHandle::unmount`, which joins all
//!   backend-owned threads/tasks within bounded timeouts — no leaked mounts
//!   or orphan sessions on the success path.
//! - One supervision path: `supervise::run_supervised*` owns
//!   signal-forwarding, child-exit propagation, and
//!   unmount-before-exit ordering for commands that hold a mount while a
//!   child process runs.
//! - Readiness and teardown are time-bounded (`wait_for_mount`, unmount
//!   timeouts), so a wedged backend surfaces as an error instead of a hang.
//!
//! # Example
//!
//! ```ignore
//! use agentfs_mount::{mount_fs, Backend, MountOpts};
//!
//! let opts = MountOpts::new(PathBuf::from("/mnt/agent"), Backend::Fuse);
//! let handle = mount_fs(Arc::new(my_fs), opts).await?;
//! // ... use the mounted filesystem ...
//! handle.unmount().await?;
//! ```

pub mod daemon;
#[cfg(target_os = "linux")]
mod fuse;
mod nfs;
pub mod supervise;

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Default timeout for mount to become ready.
const DEFAULT_MOUNT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_UNMOUNT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "linux")]
fn get_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("internal error: failed to initialize runtime")
}

/// Mount backend type.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Backend {
    /// FUSE filesystem (Linux only).
    Fuse,
    /// NFS over localhost.
    Nfs,
}

// Platform-specific default: FUSE on Linux, NFS elsewhere.
#[allow(clippy::derivable_impls)]
impl Default for Backend {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        {
            Backend::Fuse
        }
        #[cfg(not(target_os = "linux"))]
        {
            Backend::Nfs
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Fuse => write!(f, "fuse"),
            Backend::Nfs => write!(f, "nfs"),
        }
    }
}

/// Options for mounting a filesystem.
///
/// This struct provides a unified configuration for both FUSE and NFS backends.
/// Use `MountOpts::new()` to create default options, then customize as needed.
#[derive(Debug, Clone)]
pub struct MountOpts {
    /// The mountpoint path.
    pub mountpoint: PathBuf,
    /// Mount backend to use.
    pub backend: Backend,
    /// Filesystem name shown in mount output.
    pub fsname: String,
    /// User ID to report for all files.
    pub uid: Option<u32>,
    /// Group ID to report for all files.
    pub gid: Option<u32>,
    /// Allow other system users to access the mount.
    pub allow_other: bool,
    /// Allow root to access the mount (FUSE only).
    pub allow_root: bool,
    /// Auto unmount when process exits (FUSE only).
    pub auto_unmount: bool,
    /// Use lazy unmount on cleanup.
    pub lazy_unmount: bool,
    /// Timeout for mount to become ready.
    pub timeout: Duration,
}

impl MountOpts {
    /// Create default options for the given mountpoint and backend.
    fn new(mountpoint: PathBuf, backend: Backend) -> Self {
        Self {
            mountpoint,
            backend,
            fsname: "agentfs".to_string(),
            uid: None,
            gid: None,
            allow_other: false,
            allow_root: false,
            auto_unmount: false,
            lazy_unmount: false,
            timeout: DEFAULT_MOUNT_TIMEOUT,
        }
    }
}

impl Default for MountOpts {
    fn default() -> Self {
        Self::new(PathBuf::new(), Backend::default())
    }
}

/// Options for serving AgentFS over NFS without mounting it locally.
#[derive(Debug, Clone)]
pub struct NfsServerOptions {
    /// IP address or hostname to bind.
    bind: String,
    /// TCP port to bind. Use `0` to request an ephemeral port.
    port: u32,
}

impl NfsServerOptions {
    /// Create NFS server options for the given bind host and port.
    pub fn new(bind: impl Into<String>, port: u32) -> Self {
        Self {
            bind: bind.into(),
            port,
        }
    }
}

impl Default for NfsServerOptions {
    fn default() -> Self {
        Self::new("127.0.0.1", 0)
    }
}

/// Handle for a standalone NFS server owned by the mount lifecycle crate.
pub struct NfsServerHandle {
    inner: agentfs_nfs::ServerHandle,
}

impl NfsServerHandle {
    /// Listening address chosen by the OS.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.inner.local_addr()
    }

    /// Listening TCP port chosen by the OS.
    pub fn local_port(&self) -> u16 {
        self.inner.local_port()
    }

    /// Request cooperative server shutdown.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    /// Wait for the server task to stop and surface shutdown errors.
    pub async fn join(mut self) -> Result<()> {
        self.inner.join().await
    }
}

/// Serve a filesystem over NFS through the mount lifecycle crate's sealed edge.
pub async fn serve_nfs(
    fs: Arc<dyn agentfs_core::FileSystem>,
    opts: NfsServerOptions,
) -> Result<NfsServerHandle> {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let inner = agentfs_nfs::serve(
        fs,
        agentfs_nfs::NfsServeOptions::new(opts.bind, opts.port),
        shutdown,
    )
    .await?;
    Ok(NfsServerHandle { inner })
}

/// A mounted filesystem handle.
///
/// This handle represents an active mount. Prefer calling [`MountHandle::unmount`]
/// so the backend can join all worker tasks and surface teardown errors. Drop is
/// retained as best-effort cleanup for early-return paths.
pub struct MountHandle {
    mountpoint: PathBuf,
    backend: Backend,
    lazy_unmount: bool,
    inner: MountHandleInner,
}

pub(crate) enum MountHandleInner {
    #[cfg(target_os = "linux")]
    Fuse {
        session: Option<agentfs_fuse::SessionHandle>,
    },
    Nfs {
        server_handle: Option<agentfs_nfs::ServerHandle>,
    },
}

impl MountHandle {
    /// Get the mountpoint path.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    /// Whether the backend-owned serving task has stopped.
    fn is_finished(&self) -> bool {
        match &self.inner {
            #[cfg(target_os = "linux")]
            MountHandleInner::Fuse { session } => session
                .as_ref()
                .map(agentfs_fuse::SessionHandle::is_finished)
                .unwrap_or(true),
            MountHandleInner::Nfs { server_handle } => server_handle
                .as_ref()
                .map(agentfs_nfs::ServerHandle::is_finished)
                .unwrap_or(true),
        }
    }

    /// Unmount and join all backend-owned work.
    ///
    /// FUSE teardown requests the session unmount, joins the session thread
    /// (which drains FUSE dispatch workers and uring queue threads), then
    /// verifies the mountpoint is no longer mounted. NFS teardown cancels the
    /// server token, unmounts the client mount, and awaits the server task so
    /// acknowledged writes drain and `finalize()` runs.
    pub async fn unmount(mut self) -> Result<()> {
        self.unmount_inner_async().await
    }

    async fn unmount_inner_async(&mut self) -> Result<()> {
        let _ = std::env::set_current_dir("/");
        let mut first_error = None;

        match &mut self.inner {
            #[cfg(target_os = "linux")]
            MountHandleInner::Fuse { session } => {
                if let Some(session) = session.as_mut() {
                    if let Err(error) = session.unmount() {
                        remember_error(
                            &mut first_error,
                            anyhow::anyhow!(
                                "failed to request FUSE session unmount at {}: {}",
                                self.mountpoint.display(),
                                error
                            ),
                        );
                    }
                }
                if is_mountpoint(&self.mountpoint) {
                    if let Err(error) = unmount(&self.mountpoint, self.backend, self.lazy_unmount) {
                        remember_error(&mut first_error, error);
                    }
                }
                if let Some(session) = session.take() {
                    if let Err(error) = session.join() {
                        remember_error(&mut first_error, error);
                    }
                }
                if is_mountpoint(&self.mountpoint) {
                    if let Err(error) = unmount(&self.mountpoint, self.backend, self.lazy_unmount) {
                        remember_error(&mut first_error, error);
                    }
                }
                if is_mountpoint(&self.mountpoint) {
                    remember_error(
                        &mut first_error,
                        anyhow::anyhow!(
                            "FUSE mountpoint {} is still mounted after teardown",
                            self.mountpoint.display()
                        ),
                    );
                }
            }
            MountHandleInner::Nfs { server_handle } => {
                if let Some(handle) = server_handle.as_ref() {
                    handle.cancel();
                }
                if is_mountpoint(&self.mountpoint) {
                    if let Err(error) = unmount(&self.mountpoint, self.backend, self.lazy_unmount) {
                        remember_error(&mut first_error, error);
                    }
                }
                if let Some(mut handle) = server_handle.take() {
                    match tokio::time::timeout(DEFAULT_UNMOUNT_TIMEOUT, handle.join()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => remember_error(&mut first_error, error),
                        Err(_) => {
                            let timeout_error = anyhow::anyhow!(
                                "NFS server did not stop gracefully for {} within {:?}",
                                self.mountpoint.display(),
                                DEFAULT_UNMOUNT_TIMEOUT
                            );
                            tracing::warn!(
                                mountpoint = %self.mountpoint.display(),
                                timeout = ?DEFAULT_UNMOUNT_TIMEOUT,
                                "NFS server did not stop gracefully; aborting task"
                            );
                            handle.abort();
                            match tokio::time::timeout(Duration::from_secs(1), handle.join()).await
                            {
                                Ok(Ok(())) => {}
                                Ok(Err(error)) => tracing::warn!(
                                    %error,
                                    "NFS server task reported an error after abort"
                                ),
                                Err(_) => tracing::warn!(
                                    mountpoint = %self.mountpoint.display(),
                                    "NFS server task did not join after abort"
                                ),
                            }
                            remember_error(&mut first_error, timeout_error);
                        }
                    }
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn unmount_inner_sync(&mut self) {
        let _ = std::env::set_current_dir("/");

        match &mut self.inner {
            #[cfg(target_os = "linux")]
            MountHandleInner::Fuse { session } => {
                if let Some(session) = session.as_mut() {
                    if let Err(error) = session.unmount() {
                        tracing::warn!(
                            mountpoint = %self.mountpoint.display(),
                            %error,
                            "failed to request FUSE session unmount"
                        );
                    }
                }
                if is_mountpoint(&self.mountpoint) {
                    if let Err(error) = unmount(&self.mountpoint, self.backend, self.lazy_unmount) {
                        tracing::warn!(
                            mountpoint = %self.mountpoint.display(),
                            %error,
                            "failed to unmount FUSE filesystem"
                        );
                    }
                }
                if let Some(session) = session.take() {
                    if let Err(error) = session.join() {
                        tracing::warn!(%error, "FUSE session exited with error");
                    }
                }
                if is_mountpoint(&self.mountpoint) {
                    if let Err(error) = unmount(&self.mountpoint, self.backend, self.lazy_unmount) {
                        tracing::warn!(
                            mountpoint = %self.mountpoint.display(),
                            %error,
                            "failed final FUSE unmount"
                        );
                    }
                }
                if is_mountpoint(&self.mountpoint) {
                    tracing::warn!(
                        mountpoint = %self.mountpoint.display(),
                        "FUSE mountpoint is still mounted after teardown"
                    );
                }
            }
            MountHandleInner::Nfs { server_handle } => {
                if let Some(handle) = server_handle.as_ref() {
                    handle.cancel();
                }

                if is_mountpoint(&self.mountpoint) {
                    if let Err(error) = unmount(&self.mountpoint, self.backend, self.lazy_unmount) {
                        tracing::warn!(
                            mountpoint = %self.mountpoint.display(),
                            %error,
                            "failed to unmount NFS filesystem"
                        );
                    }
                }

                if let Some(mut handle) = server_handle.take() {
                    let deadline = std::time::Instant::now() + DEFAULT_UNMOUNT_TIMEOUT;
                    while !handle.is_finished() && std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if !handle.is_finished() {
                        tracing::warn!(
                            mountpoint = %self.mountpoint.display(),
                            timeout = ?DEFAULT_UNMOUNT_TIMEOUT,
                            "NFS server did not stop gracefully; aborting task"
                        );
                        handle.abort();
                    }
                }
            }
        }
    }
}

impl Drop for MountHandle {
    fn drop(&mut self) {
        self.unmount_inner_sync();
    }
}

/// Unmount a filesystem at the given mountpoint.
///
/// This function handles unmounting for both FUSE and NFS backends.
/// If `lazy` is true, uses lazy unmount which detaches immediately even if busy.
fn unmount(mountpoint: &Path, backend: Backend, lazy: bool) -> Result<()> {
    match backend {
        #[cfg(target_os = "linux")]
        Backend::Fuse => fuse::unmount_fuse(mountpoint, lazy),
        #[cfg(not(target_os = "linux"))]
        Backend::Fuse => anyhow::bail!("FUSE is not supported on this platform"),
        Backend::Nfs => nfs::unmount_nfs(mountpoint, lazy),
    }
}

/// Mount a filesystem with the given options.
///
/// Returns a handle that automatically unmounts when dropped.
/// The filesystem must be wrapped in `Arc<dyn FileSystem>`.
#[cfg(target_os = "linux")]
pub async fn mount_fs(
    fs: Arc<dyn agentfs_core::FileSystem>,
    opts: MountOpts,
) -> Result<MountHandle> {
    match opts.backend {
        Backend::Fuse => fuse::mount_fuse(fs, opts),
        Backend::Nfs => nfs::mount_nfs(fs, opts).await,
    }
}

/// Mount a filesystem with the given options (macOS version).
#[cfg(target_os = "macos")]
pub async fn mount_fs(
    fs: Arc<dyn agentfs_core::FileSystem>,
    opts: MountOpts,
) -> Result<MountHandle> {
    match opts.backend {
        Backend::Fuse => {
            anyhow::bail!(
                "FUSE mounting is not supported on macOS.\n\
                 Use --backend nfs (default) instead."
            );
        }
        Backend::Nfs => nfs::mount_nfs(fs, opts).await,
    }
}

fn remember_error(slot: &mut Option<anyhow::Error>, error: anyhow::Error) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

/// Resolve when SIGTERM, SIGINT, or SIGHUP is delivered.
///
/// Mount-owning commands must tear down through this rather than the default
/// signal disposition: dying without unmounting leaves a dead mount table
/// entry (ENOTCONN for every later visitor) and skips `MountHandle`'s Drop.
#[cfg(unix)]
async fn termination_signal() -> std::io::Result<i32> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let signo = tokio::select! {
        _ = term.recv() => libc::SIGTERM,
        _ = int.recv() => libc::SIGINT,
        _ = hup.recv() => libc::SIGHUP,
    };
    Ok(signo)
}

/// Resolve when SIGTERM, SIGINT, or SIGHUP is delivered.
#[cfg(unix)]
async fn shutdown_signal() -> std::io::Result<()> {
    termination_signal().await.map(|_| ())
}

/// Wait for a path to become a mountpoint.
pub(crate) fn wait_for_mount(path: &Path, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    let interval = Duration::from_millis(50);

    while start.elapsed() < timeout {
        if is_mountpoint(path) {
            return true;
        }
        std::thread::sleep(interval);
    }
    false
}

/// Check if a path is a mountpoint by comparing device IDs with parent.
pub fn is_mountpoint(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let path_meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };

        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("/"),
        };

        let parent_meta = match std::fs::metadata(parent) {
            Ok(m) => m,
            Err(_) => return false,
        };

        path_meta.dev() != parent_meta.dev()
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}
