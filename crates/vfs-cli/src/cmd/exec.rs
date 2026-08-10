//! Execute a command with a Vfs filesystem mounted.
//!
//! This module provides the `vfs exec` command which mounts a Vfs
//! filesystem to a temporary directory, runs a command with that as the
//! working directory, and automatically unmounts when done.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use vfs_core::{EncryptionConfig, FileSystem, HostFS, VfsOptions};

use crate::cmd::init::open_vfs;
use crate::opts::MountBackend;
use vfs_mount::supervise::{exit_code_for_spawn_error, exit_code_for_status, run_supervised};
use vfs_mount::{mount_fs, MountOpts};

/// Handle the exec command.
///
/// Mounts the specified Vfs, runs the command, and unmounts on completion.
pub async fn handle_exec_command(
    id_or_path: String,
    command: PathBuf,
    args: Vec<String>,
    backend: MountBackend,
    encryption: Option<(String, String)>,
) -> Result<()> {
    // Resolve Vfs options
    let mut opts = VfsOptions::resolve(&id_or_path)?;
    if let Some((key, cipher)) = encryption {
        opts = opts.with_encryption(EncryptionConfig {
            hex_key: key,
            cipher,
        });
    }

    // Open Vfs
    let vfs = open_vfs(opts)
        .await
        .map_err(|err| super::migrate::open_error_with_guidance(err, &id_or_path))?;

    let fs: Arc<dyn FileSystem> = if let Some(base_path) = vfs.overlay_base_path().await? {
        // Overlay database: stack over the recorded base (and the branch
        // parent chain, when the delta records one).
        eprintln!("Using overlay filesystem with base: {}", base_path);
        let hostfs = HostFS::new(&base_path)?;
        let home = dirs::home_dir().context("Failed to get home directory")?;
        let overlay = crate::cmd::stack::build_overlay(&home, Arc::new(hostfs), &vfs, None).await?;
        overlay.load().await?; // Load persisted whiteouts and origin mappings
        Arc::new(overlay) as Arc<dyn FileSystem>
    } else {
        Arc::new(vfs.fs) as Arc<dyn FileSystem>
    };

    // Create a temporary directory for the mount
    let exec_id = uuid::Uuid::new_v4().to_string();
    let mountpoint = std::env::temp_dir().join(format!("vfs-exec-{}", exec_id));
    std::fs::create_dir_all(&mountpoint).context("Failed to create mount directory")?;

    let fsname = format!(
        "vfs:{}",
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
    let status = run_supervised(mount_handle, child).await;

    let _ = std::fs::remove_dir_all(&mountpoint);

    let status = match status {
        Ok(status) => status,
        Err(error) => {
            // Missing / non-executable commands pass through as 127/126 like
            // run's child exec path (VAL-CLI-019 exception); every other
            // failure goes to the unified reporter.
            if let Some(code) = error
                .downcast_ref::<std::io::Error>()
                .and_then(exit_code_for_spawn_error)
            {
                eprintln!("Error: Failed to execute: {}: {}", command.display(), error);
                crate::profiling::emit_cli_report();
                std::process::exit(code);
            }
            return Err(error).with_context(|| format!("Failed to execute: {}", command.display()));
        }
    };
    if !status.success() {
        crate::profiling::emit_cli_report();
        std::process::exit(exit_code_for_status(status));
    }
    Ok(())
}
