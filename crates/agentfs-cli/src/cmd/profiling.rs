//! CLI-owned profile sink helpers.
//!
//! Adapter-specific counters register from their owning crates when those
//! adapters are constructed; the CLI owns only the process report lifecycle.

pub use agentfs_core::telemetry::ProfileSnapshot;

const PROFILE_SUMMARY_EVENT: &str = "agentfs_profile_summary";

/// Drop guard installed by the CLI binary for a single process summary.
#[derive(Debug)]
pub struct ProfileReportGuard {
    source: &'static str,
}

impl ProfileReportGuard {
    fn new(source: &'static str) -> Self {
        Self { source }
    }

    pub fn emit_now(&self) {
        emit_profile_summary(self.source);
    }
}

impl Drop for ProfileReportGuard {
    fn drop(&mut self) {
        self.emit_now();
    }
}

pub fn install_cli_sink() -> ProfileReportGuard {
    ProfileReportGuard::new("cli")
}

pub fn emit_cli_report() {
    emit_profile_summary("cli");
}

pub fn report_checkpoint() {
    if let Some(payload) = agentfs_core::telemetry::checkpoint_payload(PROFILE_SUMMARY_EVENT) {
        eprintln!("{payload}");
    }
}

fn emit_profile_summary(source: &str) {
    if let Some(payload) =
        agentfs_core::telemetry::take_summary_payload(PROFILE_SUMMARY_EVENT, source)
    {
        eprintln!("{payload}");
    }
}

pub fn snapshot() -> ProfileSnapshot {
    agentfs_core::telemetry::snapshot()
}
