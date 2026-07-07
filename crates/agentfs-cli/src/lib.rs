//! Thin CLI edge for AgentFS.
//!
//! The CLI crate owns argument parsing, user-facing command output, process
//! exit/reporting behavior, and runtime config assembly. Filesystem semantics,
//! transport adapters, and mount lifecycle code live in `agentfs-core`,
//! `agentfs-fuse`, `agentfs-nfs`, and `agentfs-mount` respectively.
//!
//! Owned invariants:
//!
//! - One error reporter: handler results flow back to `main`, which prints
//!   the single `Error:` form and exits 1; child-status passthrough is the
//!   only sanctioned direct `process::exit`, and every hard-exit site
//!   flushes the profile report sink first.
//! - Config at the edge: CLI flags and env are resolved here into typed
//!   options before core/transport code runs; every runtime knob is
//!   declared in the `knobs` ledger that generates docs/KNOBS.md.
//! - Generated docs parity: the docs/MANUAL.md command reference is
//!   rendered from the clap tree by `docs` and pinned by tests, so help
//!   output and the manual cannot drift.

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
