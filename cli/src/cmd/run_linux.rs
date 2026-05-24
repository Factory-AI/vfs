//! Linux run command implementation.
//!
//! Dispatches to either the FUSE+namespace sandbox (default) or the experimental
//! ptrace-based sandbox based on command-line flags.

use agentfs_sdk::PartialOriginPolicy;
use anyhow::Result;
use std::path::PathBuf;

/// Run the command in a Linux sandbox.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    allow: Vec<PathBuf>,
    no_default_allows: bool,
    experimental_sandbox: bool,
    strace: bool,
    session: Option<String>,
    system: bool,
    encryption: Option<(String, String)>,
    partial_origin_policy: Option<PartialOriginPolicy>,
    command: PathBuf,
    args: Vec<String>,
) -> Result<()> {
    if experimental_sandbox {
        if !allow.is_empty() || no_default_allows {
            eprintln!("Warning: --allow and --no-default-allows are not supported with --experimental-sandbox, ignoring");
        }
        if session.is_some() {
            eprintln!("Warning: --session is not supported with --experimental-sandbox, ignoring");
        }
        if encryption.is_some() {
            eprintln!("Warning: --key is not supported with --experimental-sandbox, ignoring");
        }
        if partial_origin_policy.is_some() {
            eprintln!(
                "Warning: --partial-origin is not supported with --experimental-sandbox, ignoring"
            );
        }
        crate::sandbox::linux_ptrace::run_cmd(strace, command, args).await;
    } else {
        if strace {
            eprintln!("Warning: --strace is only supported with --experimental-sandbox, ignoring");
        }
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
    }
    Ok(())
}
