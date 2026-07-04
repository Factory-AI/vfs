//! Sealed NFS serve surface for AgentFS.
//!
//! The adopted NFS protocol server and AgentFS adapter are private
//! implementation details. Callers receive only the serve entry point, serve
//! options, and server handle needed for lifecycle management.

mod adapter;
mod server;

use std::net::SocketAddr;
use std::sync::Arc;

use agentfs_core::FileSystem;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use adapter::AgentNFS;
use server::tcp::{NFSTcp, NFSTcpListener};

/// Options for serving an agent filesystem over NFSv3.
#[derive(Debug, Clone)]
pub struct NfsServeOptions {
    /// IP address or hostname to bind.
    pub bind: String,
    /// TCP port to bind. Use `0` to request an ephemeral port.
    pub port: u32,
    /// Optional export name. `None` exports the root path `/`.
    pub export_name: Option<String>,
}

impl NfsServeOptions {
    /// Create NFS serve options for the given bind host and port.
    pub fn new(bind: impl Into<String>, port: u32) -> Self {
        Self {
            bind: bind.into(),
            port,
            export_name: None,
        }
    }

    fn bind_addr(&self) -> anyhow::Result<String> {
        if self.port > u16::MAX as u32 {
            anyhow::bail!("NFS port {} is outside the valid TCP port range", self.port);
        }
        Ok(format!("{}:{}", self.bind, self.port))
    }
}

impl Default for NfsServeOptions {
    fn default() -> Self {
        Self::new("127.0.0.1", 0)
    }
}

/// Handle for a running NFS server.
pub struct ServerHandle {
    shutdown: CancellationToken,
    task: Option<JoinHandle<std::io::Result<()>>>,
    local_addr: SocketAddr,
}

impl ServerHandle {
    /// Listening address chosen by the OS.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Listening TCP port chosen by the OS.
    pub fn local_port(&self) -> u16 {
        self.local_addr.port()
    }

    /// Request cooperative server shutdown.
    pub fn cancel(&self) {
        self.shutdown.cancel();
    }

    /// Whether the background server task has finished.
    pub fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .map(JoinHandle::is_finished)
            .unwrap_or(true)
    }

    /// Abort the background server task if it has not stopped cooperatively.
    pub fn abort(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    /// Wait for the server task to stop and surface shutdown errors.
    pub async fn join(mut self) -> anyhow::Result<()> {
        let Some(task) = self.task.take() else {
            return Ok(());
        };
        task.await
            .map_err(|error| anyhow::anyhow!("NFS server task failed to join: {error}"))?
            .map_err(anyhow::Error::from)
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = &self.task {
            if !task.is_finished() {
                task.abort();
            }
        }
    }
}

/// Serve an agent filesystem over NFSv3 until `shutdown` is cancelled.
pub async fn serve(
    fs: Arc<dyn FileSystem>,
    opts: NfsServeOptions,
    shutdown: CancellationToken,
) -> anyhow::Result<ServerHandle> {
    let nfs = AgentNFS::new(fs);
    let mut listener = NFSTcpListener::bind(&opts.bind_addr()?, nfs).await?;
    if let Some(export_name) = opts.export_name {
        listener.with_export_name(export_name);
    }
    let local_addr = SocketAddr::new(listener.get_listen_ip(), listener.get_listen_port());
    let server_shutdown = shutdown.clone();
    let task = tokio::spawn(async move { listener.handle_until_cancelled(server_shutdown).await });

    Ok(ServerHandle {
        shutdown,
        task: Some(task),
        local_addr,
    })
}
