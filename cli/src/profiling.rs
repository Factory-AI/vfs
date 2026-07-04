//! CLI-owned profile sink helpers.
//!
//! Adapter-specific counters register from their owning crates when those
//! adapters are constructed; the CLI owns only the process report lifecycle.

pub use agentfs_core::telemetry::{report_checkpoint, ProfileReportGuard, ProfileSnapshot};

pub fn install_cli_sink() -> ProfileReportGuard {
    ProfileReportGuard::new("cli")
}

pub fn emit_cli_report() {
    agentfs_core::telemetry::report_summary("cli");
}

pub fn snapshot() -> ProfileSnapshot {
    agentfs_core::telemetry::snapshot()
}
