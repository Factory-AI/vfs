//! Darwin (macOS) run command implementation.
//!
//! This module provides a sandboxed execution environment using NFS for
//! filesystem mounting. The current working directory becomes a
//! copy-on-write overlay backed by AgentFS, mounted via a localhost NFS server.
//!
//! Sandboxing is enforced using macOS sandbox-exec with dynamically generated
//! profiles that restrict file writes to the NFS mountpoint and allowed paths.

use agentfs_core::{
    AgentFS, AgentFSOptions, EncryptionConfig, FileSystem, HostFS, OverlayFS, PartialOriginPolicy,
};
use agentfs_nfs::{serve, NfsServeOptions, ServerHandle};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::cmd::supervise::{
    supervise_command, supervise_mounted_command, ChildOutcome, MountedCommandBackend,
    ShutdownFuture,
};

#[cfg(target_os = "macos")]
use crate::sandbox::darwin::{generate_sandbox_profile, SandboxConfig};

/// Default NFS port to try (use a high port to avoid needing root)
const DEFAULT_NFS_PORT: u32 = 11111;

/// Run the command in a Darwin sandbox.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    session_id: Option<String>,
    _system: bool,
    encryption: Option<(String, String)>,
    partial_origin_policy: Option<PartialOriginPolicy>,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let home = dirs::home_dir().context("Failed to get home directory")?;

    let session = setup_run_directory(session_id, allow, no_default_allows, &cwd, &home)?;

    // Check if we're joining an existing session
    if is_mountpoint(&session.mountpoint) {
        if is_mount_healthy(&session.mountpoint) {
            eprintln!("Joining existing session: {}", session.session_id);
            eprintln!();
            let outcome = run_command_in_mount(&session, command, args).await?;
            crate::profiling::emit_cli_report();
            std::process::exit(exit_code_for_outcome(outcome));
        } else {
            eprintln!("Cleaning up stale NFS mount...");
            if let Err(e) = unmount(&session.mountpoint) {
                eprintln!("Warning: Failed to unmount stale mount: {}", e);
            }
        }
    }

    // Initialize the AgentFS database
    let db_path_str = session
        .db_path
        .to_str()
        .context("Database path contains non-UTF8 characters")?;

    let encrypted = encryption.is_some();
    let mut options = AgentFSOptions::with_path(db_path_str)
        .with_core_config(crate::config::core_config_from_env());
    if let Some((key, cipher)) = encryption {
        options = options.with_encryption(EncryptionConfig {
            hex_key: key,
            cipher,
        });
    }
    let agentfs = AgentFS::open(options)
        .await
        .context("Failed to create AgentFS")?;

    // Create overlay filesystem with CWD as base
    let base_str = cwd.to_string_lossy().to_string();
    let hostfs = HostFS::new(&base_str).context("Failed to create HostFS")?;
    let overlay = if let Some(policy) = partial_origin_policy {
        OverlayFS::new_with_partial_origin_policy(Arc::new(hostfs), agentfs.fs, policy)
    } else {
        OverlayFS::new(Arc::new(hostfs), agentfs.fs)
    };

    // Initialize the overlay (copies directory structure)
    overlay
        .init(&base_str)
        .await
        .context("Failed to initialize overlay")?;

    let fs: Arc<dyn FileSystem> = Arc::new(overlay);

    // Find an available port
    let port = find_available_port(DEFAULT_NFS_PORT)?;

    // Start NFS server in background
    let shutdown = CancellationToken::new();
    let server_handle = serve(
        fs,
        NfsServeOptions::new("127.0.0.1", port),
        shutdown.clone(),
    )
    .await
    .context("Failed to bind NFS server")?;

    // Give the server a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Mount the NFS filesystem
    mount_nfs(port, &session.mountpoint)?;

    print_welcome_banner(&session, encrypted);

    let mount_backend = DarwinNfsRunMount {
        mountpoint: session.mountpoint.clone(),
        server_handle: Some(server_handle),
    };
    let command_display = command.display().to_string();
    let child_command = command_in_mount(&session, command, args);
    let outcome = supervise_mounted_command(child_command, mount_backend)
        .await
        .with_context(|| format!("Darwin/NFS run supervision failed for {command_display}"));

    // Print session info for the user
    eprintln!();
    eprintln!("Session: {}", session.session_id);
    eprintln!();
    eprintln!("To resume this session:");
    eprintln!("  agentfs run --session {}", session.session_id);
    eprintln!();
    eprintln!("To see what changed:");
    eprintln!("  agentfs diff {}", session.session_id);

    crate::profiling::emit_cli_report();
    std::process::exit(exit_code_for_outcome(outcome?));
}

struct DarwinNfsRunMount {
    mountpoint: PathBuf,
    server_handle: Option<ServerHandle>,
}

impl MountedCommandBackend for DarwinNfsRunMount {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    fn unmount(&mut self) -> Result<()> {
        unmount(&self.mountpoint)
    }

    fn shutdown_server(&mut self) -> ShutdownFuture<'_> {
        let server_handle = self.server_handle.take();
        Box::pin(async move {
            if let Some(handle) = server_handle {
                handle.cancel();
                handle.join().await.context("NFS server shutdown failed")?;
            }
            Ok(())
        })
    }

    fn remove_mountpoint(&mut self) -> Result<()> {
        if let Err(e) = std::fs::remove_dir(&self.mountpoint) {
            eprintln!(
                "Warning: Failed to clean up mountpoint {}: {}",
                self.mountpoint.display(),
                e
            );
        }
        Ok(())
    }
}

fn exit_code_for_outcome(outcome: ChildOutcome) -> i32 {
    match outcome {
        ChildOutcome::Exited(status) => status.code().unwrap_or(1),
        ChildOutcome::Interrupted(signo) => 128 + signo,
    }
}

/// Print the welcome banner showing sandbox configuration (macOS).
#[cfg(target_os = "macos")]
fn print_welcome_banner(session: &RunSession, encrypted: bool) {
    use crate::sandbox::group_paths_by_parent;

    eprintln!("Welcome to AgentFS!");
    eprintln!();
    eprintln!("The following directories are writable:");
    eprintln!();
    eprintln!("  - {} (copy-on-write)", session.cwd.display());
    eprintln!("  - /tmp");
    for grouped_path in group_paths_by_parent(&session.allow_paths) {
        eprintln!("  - {}", grouped_path);
    }
    eprintln!();
    eprintln!("🔒 Everything else is read-only.");
    if encrypted {
        eprintln!("🔐 Delta layer is encrypted.");
    }
    eprintln!();
    eprintln!("To join this session from another terminal:");
    eprintln!();
    eprintln!("  agentfs run --session {} <command>", session.session_id);
    eprintln!();
}

/// Configuration for a sandbox run session.
struct RunSession {
    /// Directory containing session artifacts.
    run_dir: PathBuf,
    /// Path to the delta database.
    db_path: PathBuf,
    /// Path where NFS filesystem will be mounted.
    mountpoint: PathBuf,
    /// Session ID for the sandbox profile.
    session_id: String,
    /// Additional paths to allow write access.
    allow_paths: Vec<PathBuf>,
    /// Original working directory.
    cwd: PathBuf,
}

/// Default directories in HOME that are allowed to be writable.
/// These are common application config/cache directories that many programs need.
const DEFAULT_ALLOWED_DIRS: &[&str] = &[
    ".amp",         // Amp config
    ".claude",      // Claude Code config
    ".claude.json", // Claude Code config file
    ".gemini",      // Gemini CLI config
    ".local",       // Local data directory
    ".npm",         // npm local registry
    ".config",      // XDG config directory
    ".cache",       // XDG cache directory
    ".bun",         // Used by opencode to install packages at runtime
];

/// Create a run directory with database and mountpoint paths.
///
/// If `session_id` is provided, uses that as the run ID (allowing multiple
/// runs to share the same delta layer). Otherwise generates a unique UUID.
fn setup_run_directory(
    session_id: Option<String>,
    user_allow_paths: Vec<PathBuf>,
    no_default_allows: bool,
    cwd: &Path,
    home: &Path,
) -> Result<RunSession> {
    let run_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let run_dir = home.join(".agentfs").join("run").join(&run_id);
    std::fs::create_dir_all(&run_dir).context("Failed to create run directory")?;

    let db_path = run_dir.join("delta.db");
    let mountpoint = run_dir.join("mnt");
    std::fs::create_dir_all(&mountpoint).context("Failed to create mountpoint")?;

    // Build allowed paths list
    let mut allow_paths = user_allow_paths;
    if !no_default_allows {
        for dir in DEFAULT_ALLOWED_DIRS {
            let path = home.join(dir);
            if path.exists() {
                allow_paths.push(path);
            }
        }
    }

    // Create zsh config directory with custom prompt
    let zsh_dir = run_dir.join("zsh");
    std::fs::create_dir_all(&zsh_dir).context("Failed to create zsh config directory")?;
    std::fs::write(zsh_dir.join(".zshrc"), "PROMPT='🤖 %~%# '\n")
        .context("Failed to write zsh config")?;

    Ok(RunSession {
        run_dir,
        db_path,
        mountpoint,
        session_id: run_id,
        allow_paths,
        cwd: cwd.to_path_buf(),
    })
}

/// Check if a path is a mountpoint by comparing device IDs with parent.
fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(path_meta) = std::fs::metadata(path) else {
        return false;
    };

    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("/"));

    let Ok(parent_meta) = std::fs::metadata(parent) else {
        return false;
    };

    path_meta.dev() != parent_meta.dev()
}

/// Check if a mount is healthy (not stale).
///
/// Stale NFS mounts will fail when trying to access them.
fn is_mount_healthy(mountpoint: &Path) -> bool {
    std::fs::read_dir(mountpoint).is_ok()
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

/// Mount the NFS filesystem (macOS version).
#[cfg(target_os = "macos")]
fn mount_nfs(port: u32, mountpoint: &Path) -> Result<()> {
    let output = Command::new("/sbin/mount_nfs")
        .args([
            "-o",
            &format!(
                "locallocks,vers=3,tcp,port={},mountport={},wsize=1048576,rsize=1048576,soft,timeo=100,retrans=5",
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

/// Run a command with the working directory set to the mounted filesystem (macOS).
///
/// On macOS, the command is wrapped with sandbox-exec using a Sandbox profile
/// that restricts file writes to the NFS mountpoint and allowed paths.
/// The mountpoint overlays CWD, and additional paths in HOME are made writable
/// through the allow_paths configuration.
#[cfg(target_os = "macos")]
fn command_in_mount(
    session: &RunSession,
    command: PathBuf,
    args: Vec<String>,
) -> tokio::process::Command {
    // Generate the Sandbox profile
    let config = SandboxConfig {
        mountpoint: session.mountpoint.clone(),
        allow_paths: session.allow_paths.clone(),
        allow_network: true,
        session_id: session.session_id.clone(),
    };
    let profile = generate_sandbox_profile(&config);

    // Wrap the command with sandbox-exec
    let mut cmd = tokio::process::Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(&profile)
        .arg(&command)
        .args(&args)
        .current_dir(&session.mountpoint)
        .env("AGENTFS", "1")
        .env("AGENTFS_SANDBOX", "macos-sandbox")
        // Bash prompt - show full path since we're not changing HOME
        .env("PS1", "🤖 \\w\\$ ")
        // Zsh: use custom ZDOTDIR to override prompt
        .env("ZDOTDIR", session.run_dir.join("zsh"));

    cmd
}

async fn run_command_in_mount(
    session: &RunSession,
    command: PathBuf,
    args: Vec<String>,
) -> Result<ChildOutcome> {
    let command_display = command.display().to_string();
    let child_command = command_in_mount(session, command, args);
    supervise_command(child_command)
        .await
        .with_context(|| format!("Failed to execute command: {command_display}"))
}

/// Unmount the NFS filesystem (macOS version).
#[cfg(target_os = "macos")]
fn unmount(mountpoint: &Path) -> Result<()> {
    let output = Command::new("/sbin/umount")
        .arg(mountpoint)
        .output()
        .context("Failed to execute umount")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Try force unmount
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
