//! Unsupported-platform run command implementation.
//!
//! The `run` command is supported on Linux and macOS.

use agentfs_core::PartialOriginPolicy;
use anyhow::{bail, Result};
use std::path::PathBuf;

/// Report that the run command is unavailable on this platform.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    _allow: Vec<PathBuf>,
    _no_default_allows: bool,
    _session: Option<String>,
    _system: bool,
    _encryption: Option<(String, String)>,
    _partial_origin_policy: Option<PartialOriginPolicy>,
    _command: PathBuf,
    _args: Vec<String>,
) -> Result<()> {
    bail!("The `run` command is supported only on Linux and macOS")
}
