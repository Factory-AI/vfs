//! Execute a command with an AgentFS filesystem mounted.
//!
//! This module provides the `agentfs exec` command which mounts an AgentFS
//! filesystem to a temporary directory, runs a command with that as the
//! working directory, and automatically unmounts when done.

use agentfs_sdk::{AgentFSOptions, EncryptionConfig, FileSystem, HostFS, OverlayFS};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use turso::value::Value;

use crate::cmd::init::open_agentfs;
use crate::mount::{mount_fs, MountBackend, MountOpts};

/// Handle the exec command.
///
/// Mounts the specified AgentFS, runs the command, and unmounts on completion.
pub async fn handle_exec_command(
    id_or_path: String,
    command: PathBuf,
    args: Vec<String>,
    backend: MountBackend,
    encryption: Option<(String, String)>,
) -> Result<()> {
    // Resolve AgentFS options
    let mut opts = AgentFSOptions::resolve(&id_or_path)?;
    if let Some((key, cipher)) = encryption {
        opts = opts.with_encryption(EncryptionConfig {
            hex_key: key,
            cipher,
        });
    }

    // Open AgentFS
    let agentfs = open_agentfs(opts).await?;

    // Check for overlay configuration
    let fs: Arc<dyn FileSystem> = {
        let conn = agentfs.get_connection().await?;

        // Check if fs_overlay_config table exists and has base_path
        let query = "SELECT value FROM fs_overlay_config WHERE key = 'base_path'";
        let base_path: Option<String> = match conn.query(query, ()).await {
            Ok(mut rows) => {
                if let Ok(Some(row)) = rows.next().await {
                    row.get_value(0).ok().and_then(|v| {
                        if let Value::Text(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            }
            Err(_) => None,
        };

        if let Some(base_path) = base_path {
            eprintln!("Using overlay filesystem with base: {}", base_path);
            let hostfs = HostFS::new(&base_path)?;
            let overlay = OverlayFS::new(Arc::new(hostfs), agentfs.fs);
            overlay.load().await?; // Load persisted whiteouts and origin mappings
            Arc::new(overlay) as Arc<dyn FileSystem>
        } else {
            Arc::new(agentfs.fs) as Arc<dyn FileSystem>
        }
    };

    // Create a temporary directory for the mount
    let exec_id = uuid::Uuid::new_v4().to_string();
    let mountpoint = std::env::temp_dir().join(format!("agentfs-exec-{}", exec_id));
    std::fs::create_dir_all(&mountpoint).context("Failed to create mount directory")?;

    let fsname = format!(
        "agentfs:{}",
        std::fs::canonicalize(&id_or_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| id_or_path.clone())
    );

    let mount_opts = MountOpts {
        mountpoint: mountpoint.clone(),
        backend,
        fsname,
        uid: None,
        gid: None,
        allow_other: false,
        allow_root: false,
        // Not auto_unmount: the vendored fuser forces allow_other with it,
        // which requires user_allow_other in /etc/fuse.conf and widens access.
        auto_unmount: false,
        lazy_unmount: true,
        timeout: std::time::Duration::from_secs(10),
    };

    // Mount the filesystem
    let mount_handle = mount_fs(fs, mount_opts).await?;

    let outcome = supervise_child(&command, &args, &mountpoint).await;

    // Unmount and remove the mountpoint even when the workload was
    // interrupted, so no dead mount table entry or temp directory survives.
    drop(mount_handle);
    let _ = std::fs::remove_dir_all(&mountpoint);

    match outcome? {
        ChildOutcome::Exited(status) => {
            if !status.success() {
                std::process::exit(status.code().unwrap_or(1));
            }
            Ok(())
        }
        ChildOutcome::Interrupted(signo) => std::process::exit(128 + signo),
    }
}

enum ChildOutcome {
    Exited(std::process::ExitStatus),
    Interrupted(i32),
}

/// Run the workload while listening for termination signals.
///
/// The default signal disposition would kill this process without running
/// `MountHandle`'s unmount, leaving a dead mount table entry and the child
/// orphaned but alive inside it. PR_SET_PDEATHSIG additionally guarantees the
/// child cannot outlive us even under SIGKILL, which no userspace handler can
/// intercept.
async fn supervise_child(
    command: &std::path::Path,
    args: &[String],
    mountpoint: &std::path::Path,
) -> Result<ChildOutcome> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args).current_dir(mountpoint);
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent may have died between fork and prctl.
            if libc::getppid() == 1 {
                libc::raise(libc::SIGKILL);
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("Failed to execute: {}", command.display()))?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sighup = signal(SignalKind::hangup())?;
    let signo = tokio::select! {
        status = child.wait() => return Ok(ChildOutcome::Exited(status?)),
        _ = sigterm.recv() => libc::SIGTERM,
        _ = sigint.recv() => libc::SIGINT,
        _ = sighup.recv() => libc::SIGHUP,
    };

    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    Ok(ChildOutcome::Interrupted(signo))
}
