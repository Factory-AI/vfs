//! Run command - common entry point.
//!
//! Dispatches to platform-specific implementations:
//! - Linux: FUSE + namespace sandbox
//! - Darwin: NFS + sandbox-exec

use crate::opts::RunOptions;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use vfs_core::VfsOptions;

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod not_supported;

#[cfg(test)]
mod tests;

#[cfg(target_os = "macos")]
use darwin as sys;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use not_supported as sys;

/// Default directories in HOME granted read/write access in the sandbox.
///
/// Common agent/tool config and cache directories that programs need at
/// runtime. One list for every platform: the per-platform copies diverged
/// silently (Linux lacked `.config`/`.bun`, macOS lacked `.codex`), so this
/// is deliberately the superset.
const DEFAULT_ALLOWED_DIRS: &[&str] = &[
    ".amp",         // Amp config
    ".bun",         // Used by opencode to install packages at runtime
    ".cache",       // XDG cache directory (corepack, pip, etc.)
    ".claude",      // Claude Code config
    ".claude.json", // Claude Code config file
    ".codex",       // OpenAI Codex config
    ".config",      // XDG config directory
    ".gemini",      // Gemini CLI config
    ".local",       // Local data directory
    ".npm",         // npm local registry
];

/// Exit code for failures while creating or recovering the run mount.
pub const MOUNT_FAILURE_EXIT_CODE: i32 = 4;
/// Exit code for an invalid, missing, or malformed run session.
pub const INVALID_SESSION_EXIT_CODE: i32 = 5;

/// Typed mount lifecycle failure used by the CLI edge.
#[derive(Debug)]
pub struct RunMountFailure(String);

impl RunMountFailure {
    pub fn new(error: impl std::fmt::Display) -> Self {
        Self(error.to_string())
    }
}

impl std::fmt::Display for RunMountFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RunMountFailure {}

/// Typed invalid-session failure used by the CLI edge.
#[derive(Debug)]
pub struct InvalidRunSession(String);

impl InvalidRunSession {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for InvalidRunSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for InvalidRunSession {}

#[derive(Debug, Clone)]
struct SessionPaths {
    session_id: String,
    run_dir: PathBuf,
    db_path: PathBuf,
    mountpoint: PathBuf,
    base_path_file: PathBuf,
    procs_dir: PathBuf,
    runtime_status_file: PathBuf,
}

impl SessionPaths {
    fn new(home: &Path, session_id: &str) -> Self {
        let run_dir = home.join(".vfs").join("run").join(session_id);
        Self {
            session_id: session_id.to_string(),
            db_path: run_dir.join("delta.db"),
            mountpoint: run_dir.join("mnt"),
            base_path_file: run_dir.join("base_path"),
            procs_dir: run_dir.join("procs"),
            runtime_status_file: run_dir.join("runtime-status.json"),
            run_dir,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartState {
    Stopped,
    StaleRecovered,
}

struct PreparedSession {
    paths: SessionPaths,
    base_path: PathBuf,
    session_lock: crate::cmd::session_lock::SessionLock,
    start_state: StartState,
}

fn session_paths(home: &Path, session_id: &str) -> Result<SessionPaths> {
    if !VfsOptions::validate_agent_id(session_id) {
        return Err(InvalidRunSession::new(format!("invalid session ID: {session_id}")).into());
    }
    Ok(SessionPaths::new(home, session_id))
}

fn read_session_base_path(paths: &SessionPaths) -> Result<PathBuf> {
    let raw = std::fs::read_to_string(&paths.base_path_file).map_err(|error| {
        InvalidRunSession::new(format!(
            "invalid session {}: failed to read {}: {error}",
            paths.session_id,
            paths.base_path_file.display()
        ))
    })?;
    let base_path = PathBuf::from(raw.trim());
    if !base_path.is_absolute() {
        return Err(InvalidRunSession::new(format!(
            "invalid session {}: base_path must contain an absolute path",
            paths.session_id
        ))
        .into());
    }
    if !base_path.is_dir() {
        return Err(InvalidRunSession::new(format!(
            "invalid session {}: base path does not exist or is not a directory: {}",
            paths.session_id,
            base_path.display()
        ))
        .into());
    }
    Ok(base_path)
}

fn prepare_session(
    home: &Path,
    session_id: String,
    requested_base: &Path,
    allow_missing_database: bool,
) -> Result<PreparedSession> {
    let paths = session_paths(home, &session_id)?;
    let existed = paths.run_dir.exists();
    if existed && !paths.run_dir.is_dir() {
        return Err(InvalidRunSession::new(format!(
            "invalid session {}: session path is not a directory: {}",
            session_id,
            paths.run_dir.display()
        ))
        .into());
    }
    std::fs::create_dir_all(&paths.run_dir).context("Failed to create run directory")?;
    let lock_file_existed = paths.run_dir.join(".session.lock").is_file();
    let exclusive =
        crate::cmd::session_lock::SessionLock::try_exclusive(&paths.run_dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::Error::new(crate::cmd::pack::SessionStillRunning)
            } else {
                anyhow::Error::new(error).context("Failed to lock run session")
            }
        })?;

    crate::cmd::pack::recover_interrupted_publication(&paths.db_path)?;
    if !existed {
        std::fs::write(
            &paths.base_path_file,
            requested_base.to_string_lossy().as_bytes(),
        )
        .context("Failed to write session base path")?;
    } else if !paths.db_path.is_file() && !lock_file_existed && !allow_missing_database {
        return Err(InvalidRunSession::new(format!(
            "invalid session {}: session database not found: {}",
            session_id,
            paths.db_path.display()
        ))
        .into());
    }
    let base_path = read_session_base_path(&paths)?;

    let start_state = recover_stale_runtime(&paths)?;
    let session_lock = exclusive
        .downgrade_to_shared()
        .context("Failed to establish run session lifetime lock")?;
    Ok(PreparedSession {
        paths,
        base_path,
        session_lock,
        start_state,
    })
}

fn recover_stale_runtime(paths: &SessionPaths) -> Result<StartState> {
    let mut recovered = false;
    if vfs_mount::recover_stale_mount(&paths.mountpoint, platform_backend())
        .map_err(RunMountFailure::new)?
    {
        recovered = true;
    }
    if paths.procs_dir.exists() {
        std::fs::remove_dir_all(&paths.procs_dir)
            .context("Failed to remove stale session process records")?;
        recovered = true;
    }
    if paths.runtime_status_file.exists() {
        std::fs::remove_file(&paths.runtime_status_file)
            .context("Failed to remove stale runtime status")?;
        recovered = true;
    }
    let runtime_status_staging = runtime_status_staging_path(&paths.runtime_status_file);
    if runtime_status_staging.exists() {
        std::fs::remove_file(runtime_status_staging)
            .context("Failed to remove staged runtime status")?;
        recovered = true;
    }
    if paths.mountpoint.exists() {
        std::fs::remove_dir_all(&paths.mountpoint).map_err(|error| {
            RunMountFailure::new(format!("failed to reset mountpoint: {error}"))
        })?;
    }
    std::fs::create_dir_all(&paths.mountpoint)
        .map_err(|error| RunMountFailure::new(format!("failed to create mountpoint: {error}")))?;
    Ok(if recovered {
        StartState::StaleRecovered
    } else {
        StartState::Stopped
    })
}

pub(crate) fn recover_stale_session_runtime(home: &Path, session_id: &str) -> Result<()> {
    let paths = session_paths(home, session_id)?;
    recover_stale_runtime(&paths)?;
    Ok(())
}

fn prepared_seeded_session(
    home: &Path,
    session_id: String,
    session_lock: crate::cmd::session_lock::SessionLock,
) -> Result<PreparedSession> {
    let paths = session_paths(home, &session_id)?;
    let base_path = read_session_base_path(&paths)?;
    if !paths.db_path.is_file() {
        return Err(InvalidRunSession::new(format!(
            "invalid session {}: session database not found: {}",
            session_id,
            paths.db_path.display()
        ))
        .into());
    }
    std::fs::create_dir_all(&paths.mountpoint)
        .map_err(|error| RunMountFailure::new(format!("failed to create mountpoint: {error}")))?;
    Ok(PreparedSession {
        paths,
        base_path,
        session_lock,
        start_state: StartState::Stopped,
    })
}

fn platform_backend() -> vfs_mount::Backend {
    #[cfg(target_os = "linux")]
    {
        vfs_mount::Backend::Fuse
    }
    #[cfg(not(target_os = "linux"))]
    {
        vfs_mount::Backend::Nfs
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionStatus {
    session_id: String,
    state: SessionState,
    mounted: bool,
    pid: Option<u32>,
    generation: u64,
    seeded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum SessionState {
    Stopped,
    Busy,
    Live,
    StaleRecovered,
}

/// Emit a machine-readable preflight status for a run session.
pub async fn handle_status_command(
    stdout: &mut impl Write,
    session_id: String,
    _json: bool,
    encryption: Option<vfs_core::EncryptionConfig>,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    let paths = session_paths(&home, &session_id)?;
    if !paths.run_dir.is_dir() {
        return Err(InvalidRunSession::new(format!("session not found: {session_id}")).into());
    }
    let _ = read_session_base_path(&paths)?;

    let mounted_before = vfs_mount::is_mountpoint(&paths.mountpoint);
    let (state, mounted, pid, metadata, _lock) =
        match crate::cmd::session_lock::SessionLock::try_exclusive(&paths.run_dir) {
            Ok(lock) => {
                crate::cmd::pack::recover_interrupted_publication(&paths.db_path)?;
                if !paths.db_path.is_file() {
                    return Err(
                        InvalidRunSession::new(format!("session not found: {session_id}")).into(),
                    );
                }
                let start_state = recover_stale_runtime(&paths)?;
                let metadata = read_session_metadata(&paths.db_path, encryption.as_ref()).await?;
                (
                    if start_state == StartState::StaleRecovered || mounted_before {
                        SessionState::StaleRecovered
                    } else {
                        SessionState::Stopped
                    },
                    false,
                    None,
                    metadata,
                    Some(lock),
                )
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                match read_runtime_status(&paths)? {
                    Some(runtime) => (
                        SessionState::Live,
                        mounted_before,
                        Some(runtime.pid),
                        vfs_core::SessionStatusMetadata {
                            generation: runtime.generation,
                            seeded: runtime.seeded,
                        },
                        None,
                    ),
                    None => (
                        SessionState::Busy,
                        mounted_before,
                        None,
                        vfs_core::SessionStatusMetadata::default(),
                        None,
                    ),
                }
            }
            Err(error) => return Err(error).context("Failed to inspect run session lock"),
        };

    let status = SessionStatus {
        session_id,
        state,
        mounted,
        pid,
        generation: metadata.generation,
        seeded: metadata.seeded,
    };
    serde_json::to_writer(&mut *stdout, &status)?;
    writeln!(stdout)?;
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    pid: u32,
    generation: u64,
    seeded: bool,
}

fn read_runtime_status(paths: &SessionPaths) -> Result<Option<RuntimeStatus>> {
    let contents = match std::fs::read(&paths.runtime_status_file) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RunMountFailure::new(format!(
                "failed to read live runtime status {}: {error}",
                paths.runtime_status_file.display()
            ))
            .into());
        }
    };
    serde_json::from_slice(&contents)
        .map(Some)
        .map_err(|error| {
            RunMountFailure::new(format!(
                "invalid live runtime status {}: {error}",
                paths.runtime_status_file.display()
            ))
            .into()
        })
}

pub(super) fn write_runtime_status(path: &Path, generation: u64, seeded: bool) -> Result<()> {
    let status = RuntimeStatus {
        pid: std::process::id(),
        generation,
        seeded,
    };
    let staging = runtime_status_staging_path(path);
    std::fs::write(&staging, serde_json::to_vec(&status)?)
        .context("Failed to stage live runtime status")?;
    std::fs::rename(&staging, path).context("Failed to publish live runtime status")?;
    Ok(())
}

fn runtime_status_staging_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

async fn read_session_metadata(
    db_path: &Path,
    encryption: Option<&vfs_core::EncryptionConfig>,
) -> Result<vfs_core::SessionStatusMetadata> {
    let sidecars = crate::cmd::safety::ReadOnlyOpenSidecars::capture(db_path);
    let result = async {
        let db_path_str = db_path
            .to_str()
            .context("Session database path contains non-UTF8 characters")?;
        let mut options = VfsOptions::with_path(db_path_str)
            .with_core_config(crate::config::core_config_from_env());
        if let Some(encryption) = encryption {
            options = options.with_encryption(encryption.clone());
        }
        let vfs = vfs_core::Vfs::open(options)
            .await
            .map_err(|error| crate::cmd::migrate::open_error_with_guidance(error, db_path_str))
            .context("Failed to open session database for status")?;
        let metadata = vfs.session_status_metadata().await?;
        crate::cmd::init::finalize_readonly(&vfs).await;
        Ok::<_, anyhow::Error>(metadata)
    }
    .await;
    sidecars.remove_created_frameless();
    result
}

/// Expand `DEFAULT_ALLOWED_DIRS` against a home directory, keeping only
/// entries that exist.
fn default_allowed_paths(home: &Path) -> Vec<PathBuf> {
    DEFAULT_ALLOWED_DIRS
        .iter()
        .map(|dir| home.join(dir))
        .filter(|path| path.exists())
        .collect()
}

/// Handle the `run` command, dispatching to the platform-specific implementation.
///
/// Deliberately synchronous: the Linux backend must fork before any tokio
/// runtime exists (forking a live multi-threaded runtime can deadlock the
/// child on the allocator lock), so each backend owns its runtime.
#[cfg(target_os = "linux")]
pub fn handle_run_command(options: RunOptions) -> Result<()> {
    sys::run(options)
}

/// Handle the `run` command, dispatching to the platform-specific implementation.
#[cfg(not(target_os = "linux"))]
pub fn handle_run_command(options: RunOptions) -> Result<()> {
    crate::get_runtime().block_on(sys::run(options))
}

/// Group paths by parent directory and format using brace expansion.
///
/// For example, given paths:
/// - /home/user/.claude
/// - /home/user/.claude.json
/// - /home/user/.codex
/// - /home/user/.npm
///
/// Returns: `["/home/user/{.claude, .claude.json, .codex, .npm}"]`
fn group_paths_by_parent(paths: &[PathBuf]) -> Vec<String> {
    let mut groups: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();

    for path in paths {
        let (parent, name) = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => {
                (parent.to_path_buf(), name.to_string_lossy().to_string())
            }
            _ => (PathBuf::new(), path.display().to_string()),
        };
        groups.entry(parent).or_default().push(name);
    }

    groups
        .into_iter()
        .map(|(parent, mut names)| {
            names.sort();
            let parent_str = parent.display().to_string();
            if names.len() == 1 {
                if parent_str.is_empty() {
                    names.remove(0)
                } else {
                    format!("{}/{}", parent_str, names[0])
                }
            } else {
                format!("{}/{{{}}}", parent_str, names.join(", "))
            }
        })
        .collect()
}
