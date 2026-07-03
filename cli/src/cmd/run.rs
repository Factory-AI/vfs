//! Run command - common entry point.
//!
//! Dispatches to platform-specific implementations:
//! - Linux: FUSE + namespace sandbox
//! - Darwin: NFS + sandbox-exec

use agentfs_sdk::PartialOriginPolicy;
use anyhow::Result;
use std::path::PathBuf;

#[cfg_attr(target_os = "linux", path = "run_linux.rs")]
#[cfg_attr(target_os = "macos", path = "run_darwin.rs")]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    path = "run_not_supported.rs"
)]
mod sys;

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
