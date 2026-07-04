//! List active agentfs run sessions.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Information about a process in a session.
#[derive(Debug, Serialize, Deserialize)]
pub struct ProcInfo {
    /// Process ID.
    pub pid: u32,
    /// Whether this process is the session owner (created the FUSE mount).
    pub owner: bool,
    /// Command being run.
    pub command: String,
    /// When the process started.
    pub started_at: DateTime<Utc>,
    /// Working directory when the session was started.
    pub cwd: PathBuf,
}

/// Get the path to the procs directory for a session.
pub fn procs_dir(session_id: &str) -> PathBuf {
    let home = dirs::home_dir().expect("home directory");
    home.join(".agentfs")
        .join("run")
        .join(session_id)
        .join("procs")
}

/// Get the path to a proc file.
pub fn proc_file(session_id: &str, pid: u32) -> PathBuf {
    procs_dir(session_id).join(format!("{}.json", pid))
}

/// Write a proc file for the current process.
pub fn write_proc_file(session_id: &str, owner: bool, command: &str, cwd: &Path) -> Result<()> {
    let pid = std::process::id();
    let procs_dir = procs_dir(session_id);
    std::fs::create_dir_all(&procs_dir)?;

    let info = ProcInfo {
        pid,
        owner,
        command: command.to_string(),
        started_at: Utc::now(),
        cwd: cwd.to_path_buf(),
    };

    let path = proc_file(session_id, pid);
    let json = serde_json::to_string_pretty(&info)?;
    std::fs::write(path, json)?;

    Ok(())
}

/// Remove the proc file for the current process.
pub fn remove_proc_file(session_id: &str) {
    let pid = std::process::id();
    let path = proc_file(session_id, pid);
    let _ = std::fs::remove_file(path);
}

/// Check if a process is still running.
fn is_process_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{}", pid)).exists()
}

/// Information about a session with its processes.
struct SessionInfo {
    session_id: String,
    procs: Vec<ProcInfo>,
}

/// Get the set of active session IDs.
pub fn active_session_ids() -> std::collections::HashSet<String> {
    list_sessions().into_iter().map(|s| s.session_id).collect()
}

/// Read and validate a proc file, cleaning up stale entries.
///
/// Returns `Some(ProcInfo)` if the file is valid and the process is still alive,
/// or `None` if the file should be skipped (invalid, stale, or wrong extension).
fn read_proc_file_if_alive(path: &Path) -> Option<ProcInfo> {
    if path.extension() != Some(std::ffi::OsStr::new("json")) {
        return None;
    }

    let content = std::fs::read_to_string(path).ok()?;
    let info: ProcInfo = serde_json::from_str(&content).ok()?;

    if is_process_alive(info.pid) {
        Some(info)
    } else {
        // Clean up stale proc file
        let _ = std::fs::remove_file(path);
        None
    }
}

/// Collect active processes from a session's procs directory.
fn collect_session_procs(procs_dir: &Path) -> Vec<ProcInfo> {
    let proc_entries = match std::fs::read_dir(procs_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut procs: Vec<ProcInfo> = proc_entries
        .flatten()
        .filter_map(|entry| read_proc_file_if_alive(&entry.path()))
        .collect();

    // Sort by owner (true first), then by started_at
    procs.sort_by(|a, b| {
        b.owner
            .cmp(&a.owner)
            .then_with(|| a.started_at.cmp(&b.started_at))
    });

    procs
}

/// List all active sessions and their processes.
fn list_sessions() -> Vec<SessionInfo> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return vec![],
    };

    let run_dir = home.join(".agentfs").join("run");
    let entries = match std::fs::read_dir(&run_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut sessions: Vec<SessionInfo> = entries
        .flatten()
        .filter_map(|entry| {
            let session_id = entry.file_name().to_string_lossy().to_string();
            let procs_dir = entry.path().join("procs");

            if !procs_dir.exists() {
                return None;
            }

            let procs = collect_session_procs(&procs_dir);
            if procs.is_empty() {
                return None;
            }

            Some(SessionInfo { session_id, procs })
        })
        .collect();

    // Sort sessions by earliest start time
    sessions.sort_by_key(|s| s.procs.first().map(|p| p.started_at));
    sessions
}

/// Format a duration as a human-readable string.
fn format_duration(duration: chrono::Duration) -> String {
    let secs = duration.num_seconds();
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Truncate a string to a maximum length, adding ellipsis if needed.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

// Column widths for table output
const COL_SESSION: usize = 36;
const COL_PID: usize = 8;
const COL_OWNER: usize = 5;
const COL_COMMAND: usize = 15;
const COL_STARTED: usize = 10;

/// List active agentfs run sessions.
pub fn list_ps<W: Write>(out: &mut W) -> Result<()> {
    let sessions = list_sessions();

    if sessions.is_empty() {
        writeln!(out, "No active agentfs run sessions.")?;
        return Ok(());
    }

    // Print header
    writeln!(
        out,
        "{:<COL_SESSION$} {:>COL_PID$} {:^COL_OWNER$} {:<COL_COMMAND$} {:>COL_STARTED$}",
        "SESSION", "PID", "OWNER", "COMMAND", "STARTED",
    )?;

    let now = Utc::now();

    for session in &sessions {
        for proc in &session.procs {
            let owner_marker = if proc.owner { "*" } else { "" };
            let duration = now.signed_duration_since(proc.started_at);

            writeln!(
                out,
                "{:<COL_SESSION$} {:>COL_PID$} {:^COL_OWNER$} {:<COL_COMMAND$} {:>COL_STARTED$}",
                session.session_id,
                proc.pid,
                owner_marker,
                truncate(&proc.command, COL_COMMAND),
                format_duration(duration),
            )?;
        }
    }

    Ok(())
}
