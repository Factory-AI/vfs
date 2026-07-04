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
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_mount::supervise::{
    supervise_command, supervise_mounted_command, ChildOutcome, MountedCommandBackend,
    ShutdownFuture,
};
use agentfs_mount::{mount_fs, Backend, MountHandle, MountOpts};

/// Configuration for the macOS sandbox profile.
#[derive(Debug, Clone)]
struct SandboxConfig {
    /// The NFS mountpoint (primary read/write location).
    mountpoint: PathBuf,
    /// Additional paths to allow read/write access.
    allow_paths: Vec<PathBuf>,
    /// Whether to allow network access.
    allow_network: bool,
    /// Session ID for log filtering.
    session_id: String,
}

/// Generate a sandbox-exec profile for AgentFS.
///
/// The profile allows most operations but restricts file writes to the NFS
/// mountpoint, temp directories, and explicitly allowed paths.
fn generate_sandbox_profile(config: &SandboxConfig) -> String {
    let mut profile = Vec::new();
    let log_tag = format!("agentfs-{}", config.session_id);

    profile.push("(version 1)".to_string());
    profile.push(format!(
        r#"(deny default (with message "agentfs-{}: write denied"))"#,
        config.session_id
    ));
    profile.push(format!("; Log tag: {}", log_tag));

    profile.push("; Allow most operations".to_string());
    profile.push("(allow process*)".to_string());
    profile.push("(allow signal)".to_string());
    profile.push("(allow mach*)".to_string());
    profile.push("(allow sysctl*)".to_string());
    profile.push("(allow system*)".to_string());
    profile.push("(allow ipc*)".to_string());
    profile.push("(allow pseudo-tty)".to_string());

    profile.push("; Allow all file reads".to_string());
    profile.push("(allow file-read*)".to_string());

    profile.push("; Writable paths".to_string());
    let mountpoint_str = config.mountpoint.to_string_lossy();
    profile.push(format!(
        r#"(allow file-write* (subpath "{}"))"#,
        mountpoint_str
    ));

    if let Some(parent) = config.mountpoint.parent() {
        let run_dir_str = parent.to_string_lossy();
        profile.push(format!(
            r#"(allow file-write* (subpath "{}"))"#,
            run_dir_str
        ));
    }

    profile.push(r#"(allow file-write* (subpath "/private/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/var/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/private/var/folders"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/dev"))"#.to_string());
    profile.push(r#"(allow file-ioctl (subpath "/dev"))"#.to_string());

    for path in &config.allow_paths {
        let path_str = path.to_string_lossy();
        profile.push(format!(r#"(allow file-write* (subpath "{}"))"#, path_str));
    }

    profile.push("; Network".to_string());
    if config.allow_network {
        profile.push("(allow network*)".to_string());
    } else {
        profile.push(r#"(allow network* (remote ip "localhost:*"))"#.to_string());
        profile.push(r#"(allow network* (local ip "localhost:*"))"#.to_string());
    }

    profile.push("; Security and Keychain".to_string());
    profile.push(r#"(allow file-write* (subpath "/private/var/db/mds"))"#.to_string());
    profile.push(
        r#"(allow file-write* (regex #"^/private/var/folders/[^/]+/[^/]+/C/mds/"))"#.to_string(),
    );
    profile
        .push(r#"(allow file-write* (regex #"^/private/var/folders/[^/]+/[^/]+/T/"))"#.to_string());
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        profile.push(format!(
            r#"(allow file-write* (subpath "{}/Library"))"#,
            home_str
        ));
    }
    profile.push(r#"(allow file-write* (subpath "/Library/Preferences"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/Library/Keychains"))"#.to_string());
    profile.push("(allow authorization-right-obtain)".to_string());
    profile.push("(allow user-preference-write)".to_string());
    profile.push("(allow user-preference-read)".to_string());

    profile.join("\n")
}

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
            if let Err(e) = agentfs_mount::unmount(&session.mountpoint, Backend::Nfs, true) {
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

    let mut mount_opts = MountOpts::new(session.mountpoint.clone(), Backend::Nfs);
    mount_opts.fsname = format!("agentfs:{}", session.session_id);
    mount_opts.lazy_unmount = true;
    mount_opts.timeout = std::time::Duration::from_secs(10);
    let mount_handle = mount_fs(fs, mount_opts).await?;

    print_welcome_banner(&session, encrypted);

    let mount_backend = DarwinNfsRunMount {
        mountpoint: session.mountpoint.clone(),
        mount_handle: Some(mount_handle),
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
    mount_handle: Option<MountHandle>,
}

impl MountedCommandBackend for DarwinNfsRunMount {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    fn unmount(&mut self) -> Result<()> {
        Ok(())
    }

    fn shutdown_server(&mut self) -> ShutdownFuture<'_> {
        let mount_handle = self.mount_handle.take();
        Box::pin(async move {
            if let Some(handle) = mount_handle {
                handle
                    .unmount()
                    .await
                    .context("NFS mount shutdown failed")?;
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
    use super::group_paths_by_parent;

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
