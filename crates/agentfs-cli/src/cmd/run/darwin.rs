//! Darwin (macOS) run command implementation.
//!
//! This module provides a sandboxed execution environment using NFS for
//! filesystem mounting. The current working directory becomes a
//! copy-on-write overlay backed by AgentFS, mounted via a localhost NFS server.
//!
//! Sandboxing is enforced using macOS sandbox-exec with dynamically generated
//! profiles. Writes are restricted to the NFS mountpoint and allowed paths;
//! reads are default-deny and limited to the session paths, the allowed
//! paths, and a curated set of platform roots the runtime needs (see
//! `PLATFORM_READ_ROOTS`).

use crate::opts::RunOptions;
use agentfs_core::{AgentFS, AgentFSOptions, FileSystem, HostFS, OverlayFS};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentfs_mount::supervise::{
    exit_code_for_spawn_error, exit_code_for_status, run_supervised, supervise_command,
    ChildOutcome,
};
use agentfs_mount::{mount_fs, Backend, MountOpts};

/// Configuration for the macOS sandbox profile.
#[derive(Debug, Clone)]
pub(super) struct SandboxConfig {
    /// The NFS mountpoint (primary read/write location).
    pub(super) mountpoint: PathBuf,
    /// Additional paths to allow read/write access.
    pub(super) allow_paths: Vec<PathBuf>,
    /// Whether to allow network access.
    pub(super) allow_network: bool,
    /// Session ID for log filtering.
    pub(super) session_id: String,
}

/// System roots that stay readable under the default-deny read posture.
///
/// Curated from the Codex restricted platform defaults and the
/// Chromium/Mozilla Seatbelt profiles: the dyld shared cache cryptex,
/// system libraries, executable directories, package managers, system
/// config (`/private/etc`), terminfo/locale data (`/usr/share`), temp
/// directories (already writable below), and `/dev` essentials. `/System`
/// is handled separately: `/System/Volumes` firmlinks back into the data
/// volume, so a plain `(subpath "/System")` would reopen every read the
/// posture is meant to deny.
pub(super) const PLATFORM_READ_ROOTS: &[&str] = &[
    "/Applications",
    "/Library/Apple",
    "/Library/Preferences",
    "/System/Volumes/Preboot/Cryptexes",
    "/bin",
    "/dev",
    "/etc",
    "/opt",
    "/private/etc",
    "/private/tmp",
    "/private/var/db",
    "/private/var/folders",
    "/private/var/select",
    "/private/var/tmp",
    "/sbin",
    "/tmp",
    "/usr/bin",
    "/usr/lib",
    "/usr/libexec",
    "/usr/local",
    "/usr/sbin",
    "/usr/share",
    "/var/tmp",
];

/// Standard symlinks and firmlink components that need metadata access for
/// path resolution (`/var` -> `/private/var`, the `/System/Volumes/Data`
/// firmlink chain, the `/System/Volumes/Preboot` ancestor of the dyld
/// cryptex read root), mirroring the Codex restricted defaults.
const PLATFORM_METADATA_PATHS: &[&str] = &[
    "/System/Volumes",
    "/System/Volumes/Data",
    "/System/Volumes/Data/Users",
    "/System/Volumes/Data/private",
    "/System/Volumes/Preboot",
    "/etc",
    "/private/etc/localtime",
    "/tmp",
    "/var",
];

/// Proper ancestors of every read root (excluding `/`), deduplicated and
/// sorted, so path resolution can stat each component on the way down
/// without granting data reads outside the roots themselves.
fn read_root_ancestors(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut ancestors = std::collections::BTreeSet::new();
    for root in roots {
        for ancestor in root.ancestors().skip(1) {
            if ancestor != Path::new("/") && !ancestor.as_os_str().is_empty() {
                ancestors.insert(ancestor.to_path_buf());
            }
        }
    }
    ancestors.into_iter().collect()
}

/// A generated Seatbelt policy plus the dynamic path parameters it
/// references.
///
/// Dynamic paths (session, allow-listed, home) never appear in the SBPL
/// text: rules reference them as `(param "NAME")` and the values are passed
/// to `/usr/bin/sandbox-exec` as `-D NAME=value` definitions, so a path
/// containing SBPL metacharacters (a double quote) cannot corrupt the
/// profile. This is the Chromium/Codex parameter model
/// (research/darwin-seatbelt-read-scoping.md).
#[derive(Debug, Clone)]
pub(super) struct SandboxProfile {
    pub(super) policy: String,
    pub(super) params: Vec<(String, PathBuf)>,
}

/// Dynamic-path parameter table, deduplicated by value so a path referenced
/// by several rules (e.g. the mountpoint's read and write allows) is defined
/// exactly once on the sandbox-exec command line.
#[derive(Default)]
struct SandboxParams(Vec<(String, PathBuf)>);

impl SandboxParams {
    fn define(&mut self, name: impl Into<String>, path: &Path) -> String {
        if let Some((existing, _)) = self.0.iter().find(|(_, value)| value == path) {
            return existing.clone();
        }
        let name = name.into();
        debug_assert!(self.0.iter().all(|(existing, _)| existing != &name));
        self.0.push((name.clone(), path.to_path_buf()));
        name
    }
}

/// The session id reaches the profile text itself (deny message and log-tag
/// comment) where Seatbelt parameters are not available, so restrict it to a
/// conservative charset instead of trusting `--session` input. A session id
/// with no surviving characters falls back to a fixed placeholder so the
/// deny message never degrades to a bare `agentfs-:` tag.
fn sandbox_log_tag(session_id: &str) -> String {
    let tag: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if tag.is_empty() {
        "session".to_string()
    } else {
        tag
    }
}

/// Generate a sandbox-exec profile for AgentFS.
///
/// The profile allows most operations but restricts file writes to the NFS
/// mountpoint, temp directories, and explicitly allowed paths, and file
/// reads to the session paths, allowed paths, and `PLATFORM_READ_ROOTS`.
/// Every dynamic path is emitted as a `(param "NAME")` reference; the values
/// travel out-of-band in [`SandboxProfile::params`].
pub(super) fn generate_sandbox_profile(config: &SandboxConfig) -> SandboxProfile {
    let mut profile = Vec::new();
    let mut params = SandboxParams::default();
    let log_tag = format!("agentfs-{}", sandbox_log_tag(&config.session_id));

    profile.push("(version 1)".to_string());
    profile.push(format!(
        r#"(deny default (with message "{log_tag}: access denied"))"#
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

    profile.push(
        "; Readable paths: session and allow-listed roots (reads are default-deny)".to_string(),
    );
    let mut read_roots = vec![config.mountpoint.clone()];
    let mountpoint_param = params.define("MOUNTPOINT", &config.mountpoint);
    let mut read_root_params = vec![mountpoint_param.clone()];
    let mut run_dir_param = None;
    if let Some(parent) = config.mountpoint.parent() {
        read_roots.push(parent.to_path_buf());
        let name = params.define("RUN_DIR", parent);
        read_root_params.push(name.clone());
        run_dir_param = Some(name);
    }
    let mut allow_path_params = Vec::new();
    for (index, path) in config.allow_paths.iter().enumerate() {
        read_roots.push(path.clone());
        let name = params.define(format!("ALLOW_PATH_{index}"), path);
        read_root_params.push(name.clone());
        allow_path_params.push(name);
    }
    for name in &read_root_params {
        profile.push(format!(
            r#"(allow file-read* file-map-executable file-test-existence (subpath (param "{name}")))"#
        ));
    }

    profile
        .push("; Platform read roots (loader, frameworks, executables, config, temp)".to_string());
    profile.push(r#"(allow file-read* file-test-existence (literal "/"))"#.to_string());
    profile.push(
        r#"(allow file-read* file-map-executable file-test-existence (require-all (subpath "/System") (require-not (subpath "/System/Volumes"))))"#
            .to_string(),
    );
    for root in PLATFORM_READ_ROOTS {
        profile.push(format!(
            r#"(allow file-read* file-map-executable file-test-existence (subpath "{root}"))"#
        ));
    }

    profile
        .push("; Metadata for path resolution (symlinks, firmlinks, root ancestors)".to_string());
    for path in PLATFORM_METADATA_PATHS {
        profile.push(format!(
            r#"(allow file-read-metadata file-test-existence (literal "{path}"))"#
        ));
    }
    for (index, ancestor) in read_root_ancestors(&read_roots).iter().enumerate() {
        let name = params.define(format!("ANCESTOR_{index}"), ancestor);
        profile.push(format!(
            r#"(allow file-read-metadata file-test-existence (literal (param "{name}")))"#
        ));
    }

    profile.push("; Writable paths".to_string());
    profile.push(format!(
        r#"(allow file-write* (subpath (param "{mountpoint_param}")))"#
    ));

    if let Some(name) = &run_dir_param {
        profile.push(format!(r#"(allow file-write* (subpath (param "{name}")))"#));
    }

    profile.push(r#"(allow file-write* (subpath "/private/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/var/tmp"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/private/var/folders"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/dev"))"#.to_string());
    profile.push(r#"(allow file-ioctl (subpath "/dev"))"#.to_string());

    for name in &allow_path_params {
        profile.push(format!(r#"(allow file-write* (subpath (param "{name}")))"#));
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
        let name = params.define("HOME_LIBRARY", &home.join("Library"));
        profile.push(format!(r#"(allow file-write* (subpath (param "{name}")))"#));
    }
    profile.push(r#"(allow file-write* (subpath "/Library/Preferences"))"#.to_string());
    profile.push(r#"(allow file-write* (subpath "/Library/Keychains"))"#.to_string());
    profile.push("(allow authorization-right-obtain)".to_string());
    profile.push("(allow user-preference-write)".to_string());
    profile.push("(allow user-preference-read)".to_string());

    SandboxProfile {
        policy: profile.join("\n"),
        params: params.0,
    }
}

/// Missing / non-executable commands pass through as 127/126 like the
/// shared exec path (VAL-CLI-019 exception); every other supervision error
/// goes to the unified reporter.
pub(super) fn spawn_error_exit_code(error: &anyhow::Error) -> Option<i32> {
    error
        .downcast_ref::<std::io::Error>()
        .and_then(exit_code_for_spawn_error)
}

/// Run the command in a Darwin sandbox.
pub async fn run(options: RunOptions) -> Result<()> {
    let RunOptions {
        allow,
        no_default_allows,
        session,
        system: _,
        encryption,
        partial_origin_policy,
        command,
        args,
    } = options;
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let home = dirs::home_dir().context("Failed to get home directory")?;

    let session = setup_run_directory(session, allow, no_default_allows, &cwd, &home)?;

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
    if let Some(encryption) = encryption {
        options = options.with_encryption(encryption);
    }
    let agentfs = AgentFS::open(options)
        .await
        .map_err(|err| crate::cmd::migrate::open_error_with_guidance(err, db_path_str))
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

    let command_display = command.display().to_string();
    let child_command = command_in_mount(&session, command, args);
    let status = run_supervised(mount_handle, child_command).await;
    if let Err(e) = std::fs::remove_dir(&session.mountpoint) {
        eprintln!(
            "Warning: Failed to clean up mountpoint {}: {}",
            session.mountpoint.display(),
            e
        );
    }

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
    match status {
        Ok(status) => std::process::exit(exit_code_for_status(status)),
        Err(error) => {
            if let Some(code) = spawn_error_exit_code(&error) {
                eprintln!("Error: Failed to execute: {command_display}: {error}");
                std::process::exit(code);
            }
            Err(error.context(format!(
                "Darwin/NFS run supervision failed for {command_display}"
            )))
        }
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
    eprintln!("🔒 System paths are read-only.");
    eprintln!("🙈 Everything else (including the rest of your home directory) is unreadable.");
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
        allow_paths.extend(super::default_allowed_paths(home));
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

    // Wrap the command with sandbox-exec, pinned to the system binary so a
    // PATH-injected replacement cannot supplant the sandbox. Dynamic paths
    // travel as -D parameter definitions, never inside the profile text.
    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p").arg(&profile.policy);
    for (name, value) in &profile.params {
        let mut definition = std::ffi::OsString::from(format!("{name}="));
        definition.push(value.as_os_str());
        cmd.arg("-D").arg(definition);
    }
    cmd.arg(&command)
        .args(&args)
        .current_dir(&session.mountpoint)
        .env("AGENTFS", "1")
        .env("AGENTFS_SANDBOX", "macos-sandbox")
        // Bash prompt - show full path since we're not changing HOME
        .env("PS1", "🤖 \\w\\$ ")
        // Zsh: use custom ZDOTDIR to override prompt
        .env("ZDOTDIR", session.run_dir.join("zsh"));
    // The CLI's private spill dir is process-internal; children keep the
    // user's TMPDIR.
    crate::config::restore_original_tmpdir(&mut cmd);

    cmd
}

async fn run_command_in_mount(
    session: &RunSession,
    command: PathBuf,
    args: Vec<String>,
) -> Result<ChildOutcome> {
    let command_display = command.display().to_string();
    let child_command = command_in_mount(session, command, args);
    match supervise_command(child_command).await {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            if let Some(code) = spawn_error_exit_code(&error) {
                eprintln!("Error: Failed to execute: {command_display}: {error}");
                crate::profiling::emit_cli_report();
                std::process::exit(code);
            }
            Err(error).with_context(|| format!("Failed to execute command: {command_display}"))
        }
    }
}
