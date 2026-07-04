//! Run command - common entry point.
//!
//! Dispatches to platform-specific implementations:
//! - Linux: FUSE + namespace sandbox
//! - Darwin: NFS + sandbox-exec

use agentfs_core::PartialOriginPolicy;
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[cfg(target_os = "macos")]
mod darwin;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod not_supported;

#[cfg(target_os = "macos")]
use darwin as sys;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use not_supported as sys;

/// Handle the `run` command, dispatching to the platform-specific implementation.
#[allow(clippy::too_many_arguments)]
pub async fn handle_run_command(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    session: Option<String>,
    system: bool,
    encryption: Option<(String, String)>,
    partial_origin_policy: Option<PartialOriginPolicy>,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    sys::run(
        allow,
        no_default_allows,
        session,
        system,
        encryption,
        partial_origin_policy,
        command,
        args,
    )
    .await
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
