//! Session control channel served by the mount-owning `vfs run` process.
//!
//! A UNIX socket in the session directory lets same-user tooling ask the live
//! mount for work only the mount owner can do safely; today that is a
//! consistent database snapshot for `vfs branch`. Liveness stays derived from
//! the advisory session lock: a stale socket file left behind by SIGKILL
//! refuses connections, so the socket never becomes a second liveness signal.

// The server half is compiled only where a mount owner actually serves the
// socket (`run/linux.rs`); macOS builds carry just the client so `branch`
// and `prune artifacts` can still classify a live Linux-style session store
// without dead server code failing -D warnings.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[cfg(target_os = "linux")]
use {
    std::sync::Arc,
    std::time::Duration,
    tokio::net::UnixListener,
    tokio::sync::Mutex,
    tracing::{debug, warn},
    vfs_core::Vfs,
};

/// Reported by `ping`; bump when request or response semantics change.
#[cfg(target_os = "linux")]
pub(crate) const CTL_PROTOCOL_VERSION: u32 = 1;

/// Bound on how long the server waits for a connected client's request line.
#[cfg(target_os = "linux")]
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// One-line JSON request accepted on the control socket.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum CtlRequest {
    /// Liveness and capability probe.
    Ping,
    /// Copy a consistent point-in-time snapshot of the session database to
    /// `dest`, which must be a non-existing absolute path directly inside the
    /// session directory.
    Snapshot { dest: PathBuf },
    /// Report the frozen parent artifact digest this session's delta reads
    /// through, if it is a branch. Lets artifact GC classify a live session
    /// without opening its database from a second process.
    ParentArtifact,
}

/// One-line JSON response for every request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CtlResponse {
    pub(crate) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) protocol: Option<u32>,
    /// Set by `ParentArtifact` when the session is a branch; absent for a
    /// plain session (there is no digest to report).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_artifact: Option<String>,
}

#[cfg(target_os = "linux")]
impl CtlResponse {
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            protocol: None,
            parent_artifact: None,
        }
    }

    fn pong() -> Self {
        Self {
            ok: true,
            error: None,
            protocol: Some(CTL_PROTOCOL_VERSION),
            parent_artifact: None,
        }
    }

    fn parent_artifact(digest: Option<String>) -> Self {
        Self {
            ok: true,
            error: None,
            protocol: None,
            parent_artifact: digest,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            protocol: None,
            parent_artifact: None,
        }
    }
}

/// AF_UNIX `sun_path` is capped near 100 bytes and nested session stores
/// (gate temp roots, deep homes) routinely exceed it. On Linux both ends
/// address an over-long socket through `/proc/self/fd/<dirfd>/<name>`,
/// which stays short regardless of session-directory depth. The returned
/// directory handle must stay open until the bind/connect completes.
fn addressable_socket_path(socket_path: &Path) -> Result<(Option<std::fs::File>, PathBuf)> {
    // Conservative bound below both Linux (108) and macOS (104) sun_path
    // sizes, leaving room for the trailing NUL.
    const SUN_PATH_MAX: usize = 100;
    if socket_path.as_os_str().len() <= SUN_PATH_MAX {
        return Ok((None, socket_path.to_path_buf()));
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        let parent = socket_path
            .parent()
            .context("Control socket path has no parent directory")?;
        let name = socket_path
            .file_name()
            .context("Control socket path has no file name")?;
        let dir = std::fs::File::open(parent)
            .with_context(|| format!("Failed to open session directory {}", parent.display()))?;
        let short = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name);
        Ok((Some(dir), short))
    }
    #[cfg(not(target_os = "linux"))]
    {
        anyhow::bail!(
            "session control socket path {} exceeds the AF_UNIX limit",
            socket_path.display()
        )
    }
}

/// Running control server; owns the socket file and the session's SDK handle.
#[cfg(target_os = "linux")]
pub(crate) struct CtlServer {
    socket_path: PathBuf,
    task: tokio::task::JoinHandle<()>,
    remote_streamer: Option<crate::cmd::remote::streamer::RemoteStreamer>,
}

#[cfg(target_os = "linux")]
impl CtlServer {
    /// Bind the session control socket and serve requests until shutdown.
    ///
    /// Any pre-existing socket file is a leftover from a dead owner (the
    /// caller already holds the session lock) and is replaced.
    #[cfg(test)]
    pub(crate) fn spawn(socket_path: PathBuf, session_dir: PathBuf, vfs: Arc<Vfs>) -> Result<Self> {
        Self::spawn_with_remote(socket_path, session_dir, vfs, None)
    }

    /// Bind the control socket and optionally couple a remote streamer to it.
    pub(crate) fn spawn_with_remote(
        socket_path: PathBuf,
        session_dir: PathBuf,
        vfs: Arc<Vfs>,
        remote_config: Option<crate::cmd::remote::RemoteConfig>,
    ) -> Result<Self> {
        match std::fs::remove_file(&socket_path) {
            Ok(()) => debug!(socket = %socket_path.display(), "removed stale control socket"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Failed to remove stale control socket {}",
                        socket_path.display()
                    )
                })
            }
        }
        let (dir_handle, bind_path) = addressable_socket_path(&socket_path)?;
        let listener = UnixListener::bind(&bind_path).with_context(|| {
            format!(
                "Failed to bind session control socket {}",
                socket_path.display()
            )
        })?;
        drop(dir_handle);
        let remote_streamer = match remote_config
            .map(|config| crate::cmd::remote::streamer::RemoteStreamer::spawn(vfs.clone(), config))
            .transpose()
        {
            Ok(streamer) => streamer,
            Err(error) => {
                let _ = std::fs::remove_file(&socket_path);
                return Err(error).context("Failed to start the remote chunk streamer");
            }
        };
        let task = tokio::spawn(accept_loop(listener, Arc::new(session_dir), vfs));
        Ok(Self {
            socket_path,
            task,
            remote_streamer,
        })
    }

    /// Stop serving and remove the socket file.
    pub(crate) async fn shutdown(self) {
        self.task.abort();
        if let Some(streamer) = self.remote_streamer {
            streamer.shutdown();
        }
        let _ = self.task.await;
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(target_os = "linux")]
async fn accept_loop(listener: UnixListener, session_dir: Arc<PathBuf>, vfs: Arc<Vfs>) {
    // Serializes snapshots: concurrent VACUUM INTO copies of one database
    // multiply IO for no benefit and complicate failure cleanup.
    let snapshot_gate = Arc::new(Mutex::new(()));
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn(handle_connection(
                    stream,
                    session_dir.clone(),
                    vfs.clone(),
                    snapshot_gate.clone(),
                ));
            }
            Err(error) => {
                warn!(error = %error, "control socket accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn handle_connection(
    stream: UnixStream,
    session_dir: Arc<PathBuf>,
    vfs: Arc<Vfs>,
    snapshot_gate: Arc<Mutex<()>>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read_half);
    let response =
        match tokio::time::timeout(REQUEST_READ_TIMEOUT, reader.read_line(&mut line)).await {
            Err(_elapsed) => CtlResponse::error("timed out waiting for the request line"),
            Ok(Err(error)) => CtlResponse::error(format!("failed to read the request: {error}")),
            Ok(Ok(0)) => return,
            Ok(Ok(_)) => match serde_json::from_str::<CtlRequest>(line.trim()) {
                Err(error) => CtlResponse::error(format!("invalid request: {error}")),
                Ok(CtlRequest::Ping) => CtlResponse::pong(),
                Ok(CtlRequest::Snapshot { dest }) => {
                    handle_snapshot(&session_dir, &vfs, &snapshot_gate, &dest).await
                }
                Ok(CtlRequest::ParentArtifact) => match vfs.overlay_parent_artifact().await {
                    Ok(digest) => CtlResponse::parent_artifact(digest),
                    Err(error) => {
                        CtlResponse::error(format!("failed to read the parent digest: {error}"))
                    }
                },
            },
        };

    let mut payload = match serde_json::to_string(&response) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(error = %error, "failed to serialize control response");
            return;
        }
    };
    payload.push('\n');
    if let Err(error) = write_half.write_all(payload.as_bytes()).await {
        debug!(error = %error, "failed to write control response");
    }
}

#[cfg(target_os = "linux")]
async fn handle_snapshot(
    session_dir: &Path,
    vfs: &Vfs,
    snapshot_gate: &Mutex<()>,
    dest: &Path,
) -> CtlResponse {
    if !dest.is_absolute() {
        return CtlResponse::error("snapshot dest must be an absolute path");
    }
    if dest.parent() != Some(session_dir) {
        return CtlResponse::error(format!(
            "snapshot dest must live directly inside the session directory {}",
            session_dir.display()
        ));
    }
    if dest.exists() {
        return CtlResponse::error(format!("snapshot dest {} already exists", dest.display()));
    }

    let _gate = snapshot_gate.lock().await;
    match vfs.snapshot_into(dest).await {
        Ok(()) => CtlResponse::ok(),
        Err(error) => {
            debug!(error = %error, dest = %dest.display(), "snapshot request failed");
            CtlResponse::error(format!("snapshot failed: {error}"))
        }
    }
}

/// Client side: send one request over the session control socket.
///
/// A connect failure means no live mount owner is serving the socket; callers
/// classify liveness from the session lock, not from this.
pub(crate) async fn request(socket_path: &Path, request: &CtlRequest) -> Result<CtlResponse> {
    let (dir_handle, connect_path) = addressable_socket_path(socket_path)?;
    let stream = UnixStream::connect(&connect_path).await.with_context(|| {
        format!(
            "Failed to connect to session control socket {}",
            socket_path.display()
        )
    })?;
    drop(dir_handle);
    let (read_half, mut write_half) = stream.into_split();
    let mut payload = serde_json::to_string(request)?;
    payload.push('\n');
    write_half
        .write_all(payload.as_bytes())
        .await
        .context("Failed to send the control request")?;

    let mut line = String::new();
    let mut reader = BufReader::new(read_half);
    reader
        .read_line(&mut line)
        .await
        .context("Failed to read the control response")?;
    if line.is_empty() {
        anyhow::bail!("control server closed the connection without a response");
    }
    serde_json::from_str(line.trim()).context("Invalid control response")
}

// Every test spawns the server, which exists only on Linux.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use vfs_core::VfsOptions;

    async fn open_session_vfs(dir: &Path) -> Arc<Vfs> {
        let db_path = dir.join("delta.db");
        Arc::new(
            Vfs::open(VfsOptions::with_path(db_path.to_string_lossy()))
                .await
                .expect("open test vfs"),
        )
    }

    #[tokio::test]
    async fn ping_reports_protocol_version() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        let vfs = open_session_vfs(dir.path()).await;
        let server = CtlServer::spawn(socket.clone(), dir.path().to_path_buf(), vfs).unwrap();

        let response = request(&socket, &CtlRequest::Ping).await.unwrap();
        assert!(response.ok);
        assert_eq!(response.protocol, Some(CTL_PROTOCOL_VERSION));

        server.shutdown().await;
        assert!(!socket.exists(), "shutdown must remove the socket file");
    }

    #[tokio::test]
    async fn snapshot_writes_a_reopenable_copy() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        let vfs = open_session_vfs(dir.path()).await;
        let (_, file) = vfs
            .fs
            .create_file("/hello.txt", 0o100644, 0, 0)
            .await
            .unwrap();
        file.pwrite(0, b"snapshot me").await.unwrap();
        drop(file);

        let server =
            CtlServer::spawn(socket.clone(), dir.path().to_path_buf(), vfs.clone()).unwrap();
        let dest = dir.path().join("snapshot.db");
        let response = request(&socket, &CtlRequest::Snapshot { dest: dest.clone() })
            .await
            .unwrap();
        assert!(response.ok, "snapshot failed: {:?}", response.error);

        let copy = Vfs::open(VfsOptions::with_path(dest.to_string_lossy()))
            .await
            .unwrap();
        assert_eq!(
            copy.fs.read_file("/hello.txt").await.unwrap().as_deref(),
            Some(b"snapshot me".as_slice())
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn snapshot_rejects_destinations_outside_the_session_dir() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        let vfs = open_session_vfs(dir.path()).await;
        let server = CtlServer::spawn(socket.clone(), dir.path().to_path_buf(), vfs).unwrap();

        for dest in [
            outside.path().join("escape.db"),
            dir.path().join("nested").join("escape.db"),
            PathBuf::from("relative.db"),
        ] {
            let response = request(&socket, &CtlRequest::Snapshot { dest })
                .await
                .unwrap();
            assert!(!response.ok, "escaping snapshot dest must be rejected");
        }
        server.shutdown().await;
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn socket_deeper_than_sun_path_still_serves() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for _ in 0..8 {
            deep = deep.join("deeply-nested-session-store-segment");
        }
        std::fs::create_dir_all(&deep).unwrap();
        let socket = deep.join("ctl.sock");
        assert!(
            socket.as_os_str().len() > 108,
            "test path must exceed sun_path"
        );

        let vfs = open_session_vfs(&deep).await;
        let server = CtlServer::spawn(socket.clone(), deep.clone(), vfs).unwrap();
        let response = request(&socket, &CtlRequest::Ping).await.unwrap();
        assert!(response.ok);
        let dest = deep.join("snapshot.db");
        let response = request(&socket, &CtlRequest::Snapshot { dest: dest.clone() })
            .await
            .unwrap();
        assert!(response.ok, "snapshot failed: {:?}", response.error);
        assert!(dest.is_file());
        server.shutdown().await;
        assert!(!socket.exists(), "shutdown must remove the socket file");
    }

    #[tokio::test]
    async fn stale_socket_file_is_replaced_on_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("ctl.sock");
        std::fs::write(&socket, b"stale").unwrap();

        let vfs = open_session_vfs(dir.path()).await;
        let server = CtlServer::spawn(socket.clone(), dir.path().to_path_buf(), vfs).unwrap();
        let response = request(&socket, &CtlRequest::Ping).await.unwrap();
        assert!(response.ok);
        server.shutdown().await;
    }
}
