//! Linux run command implementation.
//!
//! Runs commands through the FUSE+namespace sandbox.

use agentfs_core::PartialOriginPolicy;
use anyhow::Result;
use std::path::PathBuf;

/// Run the command in a Linux sandbox.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    session: Option<String>,
    system: bool,
    encryption: Option<(String, String)>,
    partial_origin_policy: Option<PartialOriginPolicy>,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    crate::sandbox::linux::run_cmd(
        allow,
        no_default_allows,
        session,
        system,
        encryption,
        partial_origin_policy,
        command,
        args,
    )
    .await?;
    Ok(())
}
