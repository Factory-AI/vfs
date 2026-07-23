//! Sealed NFS serve surface for AgentFS.
//!
//! The adopted NFS protocol server and AgentFS adapter are private
//! implementation details. Callers receive only the serve entry point, serve
//! options, and server handle needed for lifecycle management (`serve`,
//! `NfsServeOptions`, `ServerHandle`).
//!
//! Owned invariants:
//!
//! - Server-side enforcement: permission checks go through the shared
//!   `agentfs_core::semantics` access implementation, and write authority is
//!   granted only via handle-table tokens (LRU-bounded, invalidated on
//!   unlink/rename) — local processes reaching the TCP port are inside the
//!   threat model.
//! - Honest durability: a WRITE is acknowledged `FILE_SYNC` only after the
//!   bytes are committed through the core durability contract.
//! - Duplicate request handling: retransmitted non-idempotent RPCs replay
//!   their recorded reply instead of re-executing.
//! - Graceful shutdown: the accept loop stops on cancellation, in-flight
//!   tasks are joined, and the filesystem is finalized before the handle
//!   resolves.

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
    bind: String,
    /// TCP port to bind. Use `0` to request an ephemeral port.
    port: u32,
}

impl NfsServeOptions {
    /// Create NFS serve options for the given bind host and port.
    pub fn new(bind: impl Into<String>, port: u32) -> Self {
        Self {
            bind: bind.into(),
            port,
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
    pub async fn join(&mut self) -> anyhow::Result<()> {
        let Some(task) = self.task.as_mut() else {
            return Ok(());
        };
        // The task must leave the slot as soon as it completes, even on a
        // JoinError: a retained handle would re-poll a completed JoinHandle on
        // the next join (panic) and trip the unjoined-task warn in Drop.
        let result = task.await;
        self.task.take();
        result
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
            } else {
                tracing::warn!(
                    "dropping a finished NFS server task without joining; call ServerHandle::join to observe shutdown errors"
                );
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
    let listener = NFSTcpListener::bind(&opts.bind_addr()?, nfs).await?;
    let local_addr = SocketAddr::new(listener.get_listen_ip(), listener.get_listen_port());
    let server_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        let result = listener.handle_until_cancelled(server_shutdown).await;
        if let Err(error) = &result {
            tracing::error!(%error, "NFS server task exited with error");
        }
        result
    });

    Ok(ServerHandle {
        shutdown,
        task: Some(task),
        local_addr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::time::Duration;

    fn test_handle(task: JoinHandle<io::Result<()>>) -> ServerHandle {
        ServerHandle {
            shutdown: CancellationToken::new(),
            task: Some(task),
            local_addr: "127.0.0.1:0".parse().expect("valid socket addr"),
        }
    }

    #[tokio::test]
    async fn join_surfaces_server_task_error() {
        let task = tokio::spawn(async { Err(io::Error::other("synthetic NFS serve failure")) });
        let mut handle = test_handle(task);

        let error = handle.join().await.expect_err("join should surface error");

        assert!(
            error.to_string().contains("synthetic NFS serve failure"),
            "unexpected join error: {error}"
        );
    }

    #[tokio::test]
    async fn join_timeout_leaves_server_task_abortable() {
        let task = tokio::spawn(async { std::future::pending::<io::Result<()>>().await });
        let mut handle = test_handle(task);

        let timed_out = tokio::time::timeout(Duration::from_millis(10), handle.join()).await;
        assert!(timed_out.is_err(), "join should remain pending");
        assert!(
            !handle.is_finished(),
            "timed out join must not detach or finish the task"
        );

        handle.abort();
        let aborted = tokio::time::timeout(Duration::from_secs(1), handle.join())
            .await
            .expect("aborted task should join promptly");
        assert!(
            aborted.is_err(),
            "aborted task should report a JoinError on the join path"
        );
    }

    #[tokio::test]
    async fn join_after_join_error_empties_task_slot() {
        let task = tokio::spawn(async { std::future::pending::<io::Result<()>>().await });
        let mut handle = test_handle(task);

        handle.abort();
        let first = tokio::time::timeout(Duration::from_secs(1), handle.join())
            .await
            .expect("aborted task should join promptly");
        assert!(first.is_err(), "abort should surface a JoinError");

        assert!(
            handle.task.is_none(),
            "JoinError must still take the task out of the slot (Drop would \
             otherwise warn about an unjoined finished task)"
        );
        assert!(
            handle.is_finished(),
            "handle with an emptied slot reports finished"
        );

        let second = tokio::time::timeout(Duration::from_secs(1), handle.join())
            .await
            .expect("second join must not re-poll a completed JoinHandle");
        assert!(
            second.is_ok(),
            "second join after a JoinError should be a no-op: {second:?}"
        );
    }
}
