//! Execute a command with an AgentFS filesystem mounted.
//!
//! This module provides the `agentfs exec` command which mounts an AgentFS
//! filesystem to a temporary directory, runs a command with that as the
//! working directory, and automatically unmounts when done.

use agentfs_core::{AgentFSOptions, EncryptionConfig, FileSystem, HostFS, OverlayFS};
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use turso::value::Value;

use crate::cmd::init::open_agentfs;
use crate::opts::MountBackend;
use agentfs_mount::supervise::{exit_code_for_status, run_supervised};
use agentfs_mount::{mount_fs, MountOpts};

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
    let agentfs = open_agentfs(opts)
        .await
        .map_err(|err| super::migrate::open_error_with_guidance(err, &id_or_path))?;

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
        backend: backend.into(),
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

    let mut child = tokio::process::Command::new(&command);
    child.args(&args).current_dir(&mountpoint);
    // The CLI's private spill dir is process-internal; children keep the
    // user's TMPDIR.
    crate::config::restore_original_tmpdir(&mut child);
    let status = run_supervised(mount_handle, child)
        .await
        .with_context(|| format!("Failed to execute: {}", command.display()));

    let _ = std::fs::remove_dir_all(&mountpoint);

    let status = status?;
    if !status.success() {
        crate::profiling::emit_cli_report();
        std::process::exit(exit_code_for_status(status));
    }
    Ok(())
}
