//! Run command - common entry point.
//!
//! Dispatches to platform-specific implementations:
//! - Linux: FUSE + namespace sandbox
//! - Darwin: NFS + sandbox-exec

use crate::opts::RunOptions;
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
pub async fn handle_run_command(options: RunOptions) -> Result<()> {
    sys::run(options).await
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
