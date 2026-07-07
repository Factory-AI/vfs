use std::path::PathBuf;

/// Information about a mounted agentfs filesystem
#[derive(Debug, Clone)]
pub struct Mount {
    /// The ID (from the mount source, e.g., "agentfs:my-agent" -> "my-agent")
    pub id: String,
    /// The mountpoint path
    pub mountpoint: PathBuf,
}

/// Get all currently mounted agentfs filesystems by parsing /proc/mounts
///
/// This is the authoritative source for mount information - if it's in /proc/mounts,
/// it's mounted. If not, it's not. No stale state possible.
#[cfg(target_os = "linux")]
pub fn get_mounts() -> Vec<Mount> {
    let Ok(contents) = std::fs::read_to_string("/proc/mounts") else {
        return vec![];
    };
    contents
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[0].starts_with("agentfs:") {
                let agent_id = parts[0].strip_prefix("agentfs:")?.to_string();
                // Skip the internal "fuse" mount used by the daemon
                if agent_id == "fuse" {
                    return None;
                }
                Some(Mount {
                    id: agent_id,
                    mountpoint: PathBuf::from(parts[1]),
                })
            } else {
                None
            }
        })
        .collect()
}

/// Get all currently mounted agentfs filesystems (non-Linux stub)
#[cfg(not(target_os = "linux"))]
pub fn get_mounts() -> Vec<Mount> {
    // On macOS, we could parse the output of `mount` command
    // For now, return empty - can be implemented later
    vec![]
}
