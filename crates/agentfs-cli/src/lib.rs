//! Thin CLI edge for AgentFS.
//!
//! The CLI crate owns argument parsing, user-facing command output, process
//! exit/reporting behavior, and runtime config assembly. Filesystem semantics,
//! transport adapters, and mount lifecycle code live in `agentfs-core`,
//! `agentfs-fuse`, `agentfs-nfs`, and `agentfs-mount` respectively.

pub mod cmd;
pub mod config;
pub mod docs;
pub mod knobs;
pub mod logging;
pub mod opts;

pub use cmd::profiling;

pub fn get_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("Internal error: failed to initialize runtime")
}
