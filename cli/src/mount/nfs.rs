//! NFS backend implementation for the mount infrastructure.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use agentfs_nfs::{serve, NfsServeOptions};

use super::{MountBackend, MountHandle, MountHandleInner, MountOpts};

/// Default NFS port to try (use a high port to avoid needing root).
const DEFAULT_NFS_PORT: u32 = 11111;

/// NFS unmount implementation (Linux).
#[cfg(target_os = "linux")]
pub(super) fn unmount_nfs(mountpoint: &Path, lazy: bool) -> Result<()> {
    let output = if lazy {
        Command::new("umount")
            .arg("-l")
            .arg(mountpoint)
            .output()
            .context("Failed to execute umount")?
    } else {
        Command::new("umount")
            .arg(mountpoint)
            .output()
            .context("Failed to execute umount")?
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !lazy {
            let output2 = Command::new("umount").arg("-l").arg(mountpoint).output()?;
            if output2.status.success() {
                return Ok(());
            }
        }
        anyhow::bail!(
            "Failed to unmount: {}. You may need to manually unmount with: umount -l {}",
            stderr.trim(),
            mountpoint.display()
        );
    }

    Ok(())
}

/// NFS unmount implementation (macOS).
#[cfg(target_os = "macos")]
pub(super) fn unmount_nfs(mountpoint: &Path, lazy: bool) -> Result<()> {
    let _ = lazy;
    let output = Command::new("/sbin/umount")
        .arg(mountpoint)
        .output()
        .context("Failed to execute umount")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let output2 = Command::new("/sbin/umount")
            .arg("-f")
            .arg(mountpoint)
            .output()?;

        if !output2.status.success() {
            anyhow::bail!(
                "Failed to unmount: {}. You may need to manually unmount with: umount -f {}",
                stderr.trim(),
                mountpoint.display()
            );
        }
    }

    Ok(())
}

/// Internal NFS mount implementation.
pub(super) async fn mount_nfs(
    fs: Arc<dyn agentfs_core::FileSystem>,
    opts: MountOpts,
) -> Result<MountHandle> {
    use tokio_util::sync::CancellationToken;

    let port = find_available_port(DEFAULT_NFS_PORT)?;

    let shutdown = CancellationToken::new();
    let server_handle = serve(
        fs,
        NfsServeOptions::new("127.0.0.1", port),
        shutdown.clone(),
    )
    .await
    .context("Failed to bind NFS server")?;

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    nfs_mount(port, &opts.mountpoint)?;

    Ok(MountHandle {
        mountpoint: opts.mountpoint,
        backend: MountBackend::Nfs,
        lazy_unmount: opts.lazy_unmount,
        inner: MountHandleInner::Nfs {
            server_handle: Some(server_handle),
        },
    })
}

/// Find an available TCP port starting from the given port.
fn find_available_port(start_port: u32) -> Result<u32> {
    for port in start_port..start_port + 100 {
        if std::net::TcpListener::bind(format!("127.0.0.1:{}", port)).is_ok() {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "Could not find an available port in range {}-{}",
        start_port,
        start_port + 100
    );
}

/// Mount the NFS filesystem (Linux version).
#[cfg(target_os = "linux")]
fn nfs_mount(port: u32, mountpoint: &Path) -> Result<()> {
    let output = Command::new("mount")
        .args([
            "-t",
            "nfs",
            "-o",
            &format!(
                "vers=3,tcp,port={},mountport={},nolock,soft,timeo=10,retrans=2",
                port, port
            ),
            "127.0.0.1:/",
            mountpoint.to_str().unwrap(),
        ])
        .output()
        .context("Failed to execute mount command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Failed to mount NFS: {}. Make sure NFS client tools are installed.",
            stderr.trim()
        );
    }

    Ok(())
}

/// Mount the NFS filesystem (macOS version).
#[cfg(target_os = "macos")]
fn nfs_mount(port: u32, mountpoint: &Path) -> Result<()> {
    let output = Command::new("/sbin/mount_nfs")
        .args([
            "-o",
            &format!(
                "locallocks,vers=3,tcp,port={},mountport={},wsize=1048576,rsize=1048576,soft,timeo=10,retrans=2",
                port, port
            ),
            "127.0.0.1:/",
            mountpoint.to_str().unwrap(),
        ])
        .output()
        .context("Failed to execute mount_nfs")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to mount NFS: {}", stderr.trim());
    }

    Ok(())
}
