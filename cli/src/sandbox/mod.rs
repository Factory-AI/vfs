//! Sandbox implementations for running commands in isolated environments.
//!
//! This module provides platform-specific sandbox approaches:
//! - `linux`: FUSE + namespace-based sandbox with copy-on-write filesystem
//! - `darwin`: Kernel-enforced sandbox using sandbox-exec

use std::collections::BTreeMap;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod darwin;

/// Group paths by parent directory and format using brace expansion.
///
/// For example, given paths:
/// - /home/user/.claude
/// - /home/user/.claude.json
/// - /home/user/.codex
/// - /home/user/.npm
///
/// Returns: `["/home/user/{.claude, .claude.json, .codex, .npm}"]`
pub fn group_paths_by_parent(paths: &[PathBuf]) -> Vec<String> {
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
