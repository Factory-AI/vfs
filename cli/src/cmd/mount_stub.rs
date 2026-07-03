use agentfs_sdk::PartialOriginPolicy;
use anyhow::Result;
use std::{io::Write, path::PathBuf};

pub use crate::opts::MountBackend;

/// Arguments for the mount command.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MountArgs {
    /// The agent filesystem ID or path.
    pub id_or_path: String,
    /// The mountpoint path.
    pub mountpoint: PathBuf,
    /// Automatically unmount when the process exits.
    pub auto_unmount: bool,
    /// Allow root to access the mount.
    pub allow_root: bool,
    /// Allow other system users to access the mount.
    pub allow_other: bool,
    /// Run in foreground (don't daemonize).
    pub foreground: bool,
    /// User ID to report for all files (defaults to current user).
    pub uid: Option<u32>,
    /// Group ID to report for all files (defaults to current group).
    pub gid: Option<u32>,
    /// The mount backend to use (fuse or nfs).
    pub backend: MountBackend,
    /// Partial-origin policy for overlay copy-up.
    pub partial_origin_policy: Option<PartialOriginPolicy>,
}

/// List all currently mounted agentfs filesystems
pub fn list_mounts<W: Write>(out: &mut W) {
    let _ = writeln!(out, "Mount listing is only available on Unix.");
}

/// Mount the agent filesystem.
pub fn mount(_args: MountArgs) -> Result<()> {
    anyhow::bail!("Mounting is only available on Unix (Linux or macOS)")
}

/// Prune unused agentfs mount points.
pub fn prune_mounts(_force: bool) -> Result<()> {
    anyhow::bail!("Mount pruning is only available on Unix")
}
