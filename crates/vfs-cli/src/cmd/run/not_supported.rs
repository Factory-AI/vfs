//! Unsupported-platform run command implementation.
//!
//! The `run` command is supported on Linux and macOS.

use crate::opts::RunOptions;
use anyhow::{bail, Result};

/// Report that the run command is unavailable on this platform.
pub async fn run(_options: RunOptions) -> Result<()> {
    bail!("The `run` command is supported only on Linux and macOS")
}
