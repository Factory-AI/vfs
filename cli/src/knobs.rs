//! Canonical runtime knob declarations and KNOBS.md generation.
//!
//! This is the pre-crate-split home for the single knob ledger. It covers the
//! typed SDK/core config, the typed FUSE adapter config, CLI env-backed
//! options, and the first-class partial-origin CLI policy flags.

use agentfs_sdk::{
    DEFAULT_PARTIAL_ORIGIN_THRESHOLD_BYTES, DEFAULT_WRITE_BATCH_BYTES,
    DEFAULT_WRITE_BATCH_GLOBAL_BYTES, DEFAULT_WRITE_BATCH_MS, DEFAULT_WRITE_BATCH_TXN_BYTES,
    DEFAULT_WRITE_BATCH_TXN_INODES,
};

#[cfg(target_os = "linux")]
use crate::fuse_config::{
    DEFAULT_AUTO_PERCENT, DEFAULT_FUSE_NEG_TTL_MS, DEFAULT_FUSE_POSITIVE_TTL_MS,
    DEFAULT_INO_FILES_CAP, DEFAULT_QUEUE_MEMORY_PERCENT, DEFAULT_URING_DEPTH,
};

/// Knob class required by the architecture.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum KnobClass {
    ProductConfig,
    KillSwitch,
    Sunset,
}

impl KnobClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProductConfig => "product-config",
            Self::KillSwitch => "kill-switch",
            Self::Sunset => "sunset",
        }
    }
}

/// Code-backed default renderer for generated docs.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DefaultValue {
    Literal(&'static str),
    WriteBatchMs,
    WriteBatchBytes,
    WriteBatchGlobalBytes,
    WriteBatchTxnInodes,
    WriteBatchTxnBytes,
    PartialOriginThresholdBytes,
    #[cfg(target_os = "linux")]
    FusePositiveTtlMs,
    #[cfg(target_os = "linux")]
    FuseNegativeTtlMs,
    #[cfg(target_os = "linux")]
    FuseAutoPercent,
    #[cfg(target_os = "linux")]
    FuseQueueMemoryPercent,
    #[cfg(target_os = "linux")]
    FuseInoFilesCap,
    #[cfg(target_os = "linux")]
    FuseUringDepth,
}

impl DefaultValue {
    pub fn render(self) -> String {
        match self {
            Self::Literal(value) => value.to_string(),
            Self::WriteBatchMs => DEFAULT_WRITE_BATCH_MS.to_string(),
            Self::WriteBatchBytes => DEFAULT_WRITE_BATCH_BYTES.to_string(),
            Self::WriteBatchGlobalBytes => DEFAULT_WRITE_BATCH_GLOBAL_BYTES.to_string(),
            Self::WriteBatchTxnInodes => DEFAULT_WRITE_BATCH_TXN_INODES.to_string(),
            Self::WriteBatchTxnBytes => DEFAULT_WRITE_BATCH_TXN_BYTES.to_string(),
            Self::PartialOriginThresholdBytes => DEFAULT_PARTIAL_ORIGIN_THRESHOLD_BYTES.to_string(),
            #[cfg(target_os = "linux")]
            Self::FusePositiveTtlMs => DEFAULT_FUSE_POSITIVE_TTL_MS.to_string(),
            #[cfg(target_os = "linux")]
            Self::FuseNegativeTtlMs => DEFAULT_FUSE_NEG_TTL_MS.to_string(),
            #[cfg(target_os = "linux")]
            Self::FuseAutoPercent => DEFAULT_AUTO_PERCENT.to_string(),
            #[cfg(target_os = "linux")]
            Self::FuseQueueMemoryPercent => DEFAULT_QUEUE_MEMORY_PERCENT.to_string(),
            #[cfg(target_os = "linux")]
            Self::FuseInoFilesCap => DEFAULT_INO_FILES_CAP.to_string(),
            #[cfg(target_os = "linux")]
            Self::FuseUringDepth => DEFAULT_URING_DEPTH.to_string(),
        }
    }
}

/// One row in the canonical knob ledger.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Knob {
    pub name: &'static str,
    pub surface: &'static str,
    pub class: KnobClass,
    pub default: DefaultValue,
    pub owner: &'static str,
    pub description: &'static str,
    pub removal_criteria: &'static str,
    pub gate: &'static str,
}

impl Knob {
    const fn product(
        name: &'static str,
        surface: &'static str,
        default: DefaultValue,
        owner: &'static str,
        description: &'static str,
        gate: &'static str,
    ) -> Self {
        Self {
            name,
            surface,
            class: KnobClass::ProductConfig,
            default,
            owner,
            description,
            removal_criteria: "n/a",
            gate,
        }
    }

    const fn kill_switch(
        name: &'static str,
        default: DefaultValue,
        owner: &'static str,
        description: &'static str,
        gate: &'static str,
    ) -> Self {
        Self {
            name,
            surface: "env",
            class: KnobClass::KillSwitch,
            default,
            owner,
            description,
            removal_criteria:
                "Retire only after N=2 consecutive milestones with both FUSE transport legs green and the named off-leg gate still passing.",
            gate,
        }
    }

    const fn sunset(
        name: &'static str,
        surface: &'static str,
        default: DefaultValue,
        owner: &'static str,
        description: &'static str,
        removal_criteria: &'static str,
        gate: &'static str,
    ) -> Self {
        Self {
            name,
            surface,
            class: KnobClass::Sunset,
            default,
            owner,
            description,
            removal_criteria,
            gate,
        }
    }
}

const CORE_OWNER: &str = "agentfs-sdk config";
const FUSE_OWNER: &str = "agentfs FUSE config";
const CLI_OWNER: &str = "agentfs CLI edge";

#[cfg(target_os = "linux")]
const LINUX_FUSE_KNOBS: &[Knob] = &[
    Knob::product(
        "AGENTFS_FUSE_WORKERS",
        "env",
        DefaultValue::Literal("auto"),
        FUSE_OWNER,
        "FUSE request dispatch mode. auto sizes from CPU and memory budgets; serial disables kernel-cache acceleration for safety.",
        "fuse_config::tests::workers_config_feeds_dispatch_and_cache_policy",
    ),
    Knob::product(
        "AGENTFS_FUSE_CPU_PERCENT",
        "env",
        DefaultValue::FuseAutoPercent,
        FUSE_OWNER,
        "CPU budget percentage used when AGENTFS_FUSE_WORKERS=auto.",
        "knobs::tests::knob_defaults_in_docs_match_runtime_defaults",
    ),
    Knob::product(
        "AGENTFS_FUSE_MEMORY_PERCENT",
        "env",
        DefaultValue::FuseAutoPercent,
        FUSE_OWNER,
        "Memory budget percentage used when AGENTFS_FUSE_WORKERS=auto.",
        "knobs::tests::knob_defaults_in_docs_match_runtime_defaults",
    ),
    Knob::product(
        "AGENTFS_FUSE_QUEUE",
        "env",
        DefaultValue::Literal("derived"),
        FUSE_OWNER,
        "FUSE worker request queue capacity; unset derives from worker count and queue memory budget.",
        "fuse_config::tests::workers_config_feeds_dispatch_and_cache_policy",
    ),
    Knob::product(
        "AGENTFS_FUSE_QUEUE_MEMORY_PERCENT",
        "env",
        DefaultValue::FuseQueueMemoryPercent,
        FUSE_OWNER,
        "Memory budget percentage for derived FUSE queue capacity.",
        "knobs::tests::knob_defaults_in_docs_match_runtime_defaults",
    ),
    Knob::product(
        "AGENTFS_FUSE_ENTRY_TTL_MS",
        "env",
        DefaultValue::FusePositiveTtlMs,
        FUSE_OWNER,
        "Positive dentry TTL in milliseconds.",
        "knobs::tests::knob_defaults_in_docs_match_runtime_defaults",
    ),
    Knob::product(
        "AGENTFS_FUSE_ATTR_TTL_MS",
        "env",
        DefaultValue::FusePositiveTtlMs,
        FUSE_OWNER,
        "Attribute TTL in milliseconds.",
        "knobs::tests::knob_defaults_in_docs_match_runtime_defaults",
    ),
    Knob::product(
        "AGENTFS_FUSE_NEG_TTL_MS",
        "env",
        DefaultValue::FuseNegativeTtlMs,
        FUSE_OWNER,
        "Negative dentry TTL in milliseconds.",
        "knobs::tests::knob_defaults_in_docs_match_runtime_defaults",
    ),
    Knob::product(
        "AGENTFS_FUSE_WRITEBACK",
        "env",
        DefaultValue::Literal("true"),
        FUSE_OWNER,
        "Enables FUSE writeback cache and the SDK write batcher when the CLI opens core config.",
        "fuse_config::tests::serial_dispatch_disables_kernel_cache_policy",
    ),
    Knob::product(
        "AGENTFS_FUSE_KEEPCACHE",
        "env",
        DefaultValue::Literal("true"),
        FUSE_OWNER,
        "Allows eligible read-only base files to use FOPEN_KEEP_CACHE.",
        "fuse_config::tests::serial_dispatch_disables_kernel_cache_policy",
    ),
    Knob::product(
        "AGENTFS_FUSE_READDIRPLUS",
        "env",
        DefaultValue::Literal("always"),
        FUSE_OWNER,
        "Kernel READDIRPLUS policy.",
        "fuse_config::tests::invalid_readdirplus_warns_and_defaults_on",
    ),
    Knob::product(
        "AGENTFS_FUSE_SYNC_INVAL",
        "env",
        DefaultValue::Literal("false"),
        FUSE_OWNER,
        "Requests synchronous kernel invalidation; serial dispatch keeps deferred invalidation for deadlock safety.",
        "fuse_config::tests::serial_dispatch_disables_kernel_cache_policy",
    ),
    Knob::sunset(
        "AGENTFS_FUSE_SELF_INVAL",
        "env",
        DefaultValue::Literal("false"),
        FUSE_OWNER,
        "Compatibility path that restores notify-on-self-mutation behavior.",
        "Remove after adapter cache extraction proves self-invalidation suppression through VAL-FUSE cache and coherence gates.",
        "VAL-FUSE-011 and VAL-FUSE-018",
    ),
    Knob::sunset(
        "AGENTFS_DRAIN_ON_RELEASE",
        "env",
        DefaultValue::Literal("false"),
        FUSE_OWNER,
        "Compatibility path that restores commit-on-close and disables noopen/noflush.",
        "Remove after N=2 consecutive milestones with VAL-FUSE-015, noopen, and noflush off-leg gates green.",
        "VAL-FUSE-015",
    ),
    Knob::sunset(
        "AGENTFS_DRAIN_ON_FORGET",
        "env",
        DefaultValue::Literal("false"),
        FUSE_OWNER,
        "Compatibility path that restores drain-on-forget.",
        "Remove after lifecycle and FORGET-driven cleanup gates are green for N=2 consecutive milestones.",
        "VAL-FUSE-003 and VAL-FUSE-004",
    ),
    Knob::sunset(
        "AGENTFS_FUSE_FLUSH_INVAL",
        "env",
        DefaultValue::Literal("false"),
        FUSE_OWNER,
        "Compatibility path that restores invalidate-on-every-FLUSH.",
        "Remove after noflush coherence gates are green for N=2 consecutive milestones.",
        "VAL-FUSE-007 and VAL-FUSE-008",
    ),
    Knob::kill_switch(
        "AGENTFS_FUSE_NOFLUSH",
        DefaultValue::Literal("true"),
        FUSE_OWNER,
        "Disables close-time FLUSH by returning ENOSYS after the kernel has written dirty pages.",
        "VAL-FUSE-007 default leg and VAL-FUSE-008 off leg",
    ),
    Knob::kill_switch(
        "AGENTFS_FUSE_NOOPEN",
        DefaultValue::Literal("true"),
        FUSE_OWNER,
        "Disables per-file OPEN/RELEASE by returning ENOSYS when the kernel supports no-open.",
        "VAL-FUSE-003 default leg and VAL-FUSE-004 off leg",
    ),
    Knob::product(
        "AGENTFS_FUSE_INO_FILES_CAP",
        "env",
        DefaultValue::FuseInoFilesCap,
        FUSE_OWNER,
        "Soft cap for inode-file cache backing no-open file resolution.",
        "scripts/validation/noopen-coherence.py",
    ),
    Knob::product(
        "AGENTFS_FUSE_CACHE_DIR",
        "env",
        DefaultValue::Literal("true"),
        FUSE_OWNER,
        "Directory-entry cache fast path, effective only when keepcache remains enabled.",
        "VAL-FUSE-011 and VAL-FUSE-018",
    ),
    Knob::sunset(
        "AGENTFS_FUSE_STICKY_KEEPCACHE_DROP",
        "env",
        DefaultValue::Literal("false"),
        FUSE_OWNER,
        "Compatibility path that restores old sticky keepcache-drop behavior after mutation.",
        "Remove after keep-cache fingerprint gates are green for N=2 consecutive milestones.",
        "VAL-FUSE-012 and VAL-FUSE-014",
    ),
    Knob::kill_switch(
        "AGENTFS_FUSE_URING",
        DefaultValue::Literal("true"),
        FUSE_OWNER,
        "FUSE-over-io_uring transport attempt. Set false to force the legacy /dev/fuse path.",
        "VAL-FUSE-009 uring leg and VAL-FUSE-010 off leg",
    ),
    Knob::product(
        "AGENTFS_FUSE_URING_DEPTH",
        "env",
        DefaultValue::FuseUringDepth,
        FUSE_OWNER,
        "io_uring queue depth, bounded by the adapter parser.",
        "VAL-FUSE-009",
    ),
    Knob::sunset(
        "AGENTFS_FUSE_URING_SPIN_US",
        "env",
        DefaultValue::Literal("0"),
        FUSE_OWNER,
        "Compatibility tuning for io_uring busy-poll spin before blocking.",
        "Remove after uring teardown and performance gates are green for N=2 consecutive milestones.",
        "VAL-FUSE-009 and VAL-GATE-004",
    ),
];

#[cfg(not(target_os = "linux"))]
const LINUX_FUSE_KNOBS: &[Knob] = &[];

const ACTIVE_COMMON_KNOBS: &[Knob] = &[
    Knob::product(
        "AGENTFS_KEY",
        "env or --key",
        DefaultValue::Literal("unset"),
        CLI_OWNER,
        "Hex-encoded local encryption key for CLI commands that open a database.",
        "opts clap env binding",
    ),
    Knob::product(
        "AGENTFS_CIPHER",
        "env or --cipher",
        DefaultValue::Literal("unset"),
        CLI_OWNER,
        "Encryption cipher paired with AGENTFS_KEY or --key.",
        "opts clap env binding",
    ),
    Knob::sunset(
        "AGENTFS_CLONE_TIMINGS",
        "env",
        DefaultValue::Literal("false"),
        CLI_OWNER,
        "Ad hoc clone timing printout for local performance investigations.",
        "Remove after telemetry registry exposes clone timing through the single report sink.",
        "config::clone_timings_enabled",
    ),
    Knob::product(
        "AGENTFS_PROFILE",
        "env",
        DefaultValue::Literal("false"),
        CORE_OWNER,
        "Enables profiling counters and summaries.",
        "VAL-CONF-011 and VAL-CONF-014",
    ),
    Knob::sunset(
        "AGENTFS_OVERLAY_READS",
        "env",
        DefaultValue::Literal("true"),
        CORE_OWNER,
        "Tier-4 pending-write read overlay rollback path.",
        "Remove after PendingView/stat coherence and overlay read gates are green for N=2 consecutive milestones.",
        "VAL-CORE-006 and phase8 smoke",
    ),
    Knob::product(
        "AGENTFS_KEEPCACHE_DELTA",
        "env",
        DefaultValue::Literal("true"),
        CORE_OWNER,
        "Allows DB-backed delta files to participate in keep-cache eligibility.",
        "VAL-FUSE-014",
    ),
    Knob::product(
        "AGENTFS_BATCH_MS",
        "env",
        DefaultValue::WriteBatchMs,
        CORE_OWNER,
        "Write batcher timer window in milliseconds.",
        "sdk write-batcher tests",
    ),
    Knob::product(
        "AGENTFS_BATCH_BYTES",
        "env",
        DefaultValue::WriteBatchBytes,
        CORE_OWNER,
        "Per-inode pending-byte drain trigger.",
        "sdk write-batcher tests",
    ),
    Knob::product(
        "AGENTFS_BATCH_GLOBAL_BYTES",
        "env",
        DefaultValue::WriteBatchGlobalBytes,
        CORE_OWNER,
        "Global pending-byte cap across inodes.",
        "sdk write-batcher tests",
    ),
    Knob::product(
        "AGENTFS_BATCH_TXN_INODES",
        "env",
        DefaultValue::WriteBatchTxnInodes,
        CORE_OWNER,
        "Maximum inodes drained or imported per transaction.",
        "sdk write-batcher and import tests",
    ),
    Knob::product(
        "AGENTFS_BATCH_TXN_BYTES",
        "env",
        DefaultValue::WriteBatchTxnBytes,
        CORE_OWNER,
        "Maximum bytes drained or imported per transaction.",
        "sdk write-batcher and import tests",
    ),
    Knob::sunset(
        "AGENTFS_DRAIN_ON_SETATTR",
        "env",
        DefaultValue::Literal("true"),
        CORE_OWNER,
        "Compatibility path that drains pending writes before setattr operations.",
        "Remove after PendingView/stat coherence and setattr tests are green for N=2 consecutive milestones.",
        "VAL-CORE-006 and VAL-NFS-016",
    ),
    Knob::product(
        "--partial-origin",
        "cli flag",
        DefaultValue::Literal("off"),
        CLI_OWNER,
        "First-class partial-origin copy-up policy: off, on, or auto.",
        "partial_origin::legacy_env_does_not_override_cli_off",
    ),
    Knob::product(
        "--partial-origin-threshold-bytes",
        "cli flag",
        DefaultValue::PartialOriginThresholdBytes,
        CLI_OWNER,
        "Size threshold used by --partial-origin auto.",
        "opts partial-origin parse tests",
    ),
];

const DELETED_COMPAT_KNOBS: &[Knob] = &[Knob::sunset(
    concat!("AGENTFS_OVERLAY_", "PARTIAL_ORIGIN"),
    "deleted env compat",
    DefaultValue::Literal("removed"),
    CORE_OWNER,
    "Removed legacy env compatibility path superseded by --partial-origin.",
    "Already removed in M3. Do not reintroduce; use --partial-origin or --partial-origin-threshold-bytes.",
    "partial_origin::legacy_env_does_not_override_cli_off",
)];

/// Active runtime knobs declared in one table.
pub fn active_knobs() -> Vec<Knob> {
    let mut knobs = Vec::with_capacity(ACTIVE_COMMON_KNOBS.len() + LINUX_FUSE_KNOBS.len());
    knobs.extend_from_slice(ACTIVE_COMMON_KNOBS);
    knobs.extend_from_slice(LINUX_FUSE_KNOBS);
    knobs
}

/// Deleted compatibility knobs retained only as sunset documentation.
pub fn deleted_compat_knobs() -> &'static [Knob] {
    DELETED_COMPAT_KNOBS
}

/// Generate the checked-in docs/KNOBS.md contents.
pub fn generated_knobs_doc() -> String {
    let active = active_knobs();
    let mut out = String::new();
    out.push_str("# AgentFS Runtime Knobs\n\n");
    out.push_str("<!-- Generated by `cargo test -p agentfs knobs::tests::generated_knobs_doc_matches_declarations -- --exact`. Do not edit by hand. -->\n\n");
    out.push_str(
        "Every active runtime knob is declared here with an architecture class. Defaults are rendered from the typed config declarations used by the SDK/core and FUSE adapter config modules.\n\n",
    );
    out.push_str("## Active knobs\n\n");
    out.push_str(
        "| Name | Surface | Class | Default | Owner | Description | Removal criteria | Gate |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for knob in &active {
        push_knob_row(&mut out, knob);
    }

    out.push_str("\n## Deleted compatibility knobs\n\n");
    out.push_str(
        "| Name | Surface | Class | Default | Owner | Description | Removal criteria | Gate |\n",
    );
    out.push_str("|---|---|---|---|---|---|---|---|\n");
    for knob in deleted_compat_knobs() {
        push_knob_row(&mut out, knob);
    }
    out
}

fn push_knob_row(out: &mut String, knob: &Knob) {
    out.push_str("| `");
    out.push_str(knob.name);
    out.push_str("` | ");
    out.push_str(knob.surface);
    out.push_str(" | ");
    out.push_str(knob.class.as_str());
    out.push_str(" | `");
    out.push_str(&markdown_escape(&knob.default.render()));
    out.push_str("` | ");
    out.push_str(&markdown_escape(knob.owner));
    out.push_str(" | ");
    out.push_str(&markdown_escape(knob.description));
    out.push_str(" | ");
    out.push_str(&markdown_escape(knob.removal_criteria));
    out.push_str(" | ");
    out.push_str(&markdown_escape(knob.gate));
    out.push_str(" |\n");
}

fn markdown_escape(value: &str) -> String {
    value.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    #[test]
    fn generated_knobs_doc_matches_declarations() {
        assert_runtime_env_mentions_are_declared();
        assert_unique_names(active_knobs().iter().chain(deleted_compat_knobs()));

        let docs_path = repo_root().join("docs").join("KNOBS.md");
        let expected = generated_knobs_doc();
        let actual = std::fs::read_to_string(&docs_path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", docs_path.display()));
        assert_eq!(
            actual,
            expected,
            "{} is stale; regenerate it from cli::knobs::generated_knobs_doc()",
            docs_path.display()
        );
    }

    #[test]
    fn knob_defaults_in_docs_match_runtime_defaults() {
        let active = active_knobs();
        let expected = [
            ("AGENTFS_FUSE_ENTRY_TTL_MS", "10000"),
            ("AGENTFS_FUSE_ATTR_TTL_MS", "10000"),
            ("AGENTFS_FUSE_CPU_PERCENT", "50"),
            ("AGENTFS_FUSE_MEMORY_PERCENT", "50"),
            ("AGENTFS_FUSE_QUEUE_MEMORY_PERCENT", "25"),
            ("AGENTFS_BATCH_MS", "5"),
            ("AGENTFS_BATCH_BYTES", "4194304"),
            ("AGENTFS_BATCH_GLOBAL_BYTES", "67108864"),
            ("AGENTFS_BATCH_TXN_INODES", "1024"),
            ("AGENTFS_BATCH_TXN_BYTES", "33554432"),
            ("AGENTFS_PROFILE", "false"),
        ];

        for (name, default) in expected {
            let knob = find_knob(&active, name);
            let rendered = knob.default.render();
            eprintln!("{name}={rendered}");
            assert_eq!(rendered, default, "{name} default drifted");
        }
    }

    #[test]
    fn all_knobs_have_class_and_sunset_criteria() {
        let mut sunset_names = Vec::new();
        for knob in active_knobs().iter().chain(deleted_compat_knobs()) {
            assert!(
                matches!(
                    knob.class,
                    KnobClass::ProductConfig | KnobClass::KillSwitch | KnobClass::Sunset
                ),
                "{} has an invalid class",
                knob.name
            );
            if knob.class == KnobClass::Sunset {
                assert_ne!(
                    knob.removal_criteria, "n/a",
                    "{} is sunset-class but has no removal criteria",
                    knob.name
                );
                sunset_names.push(knob.name);
            }
        }
        sunset_names.sort_unstable();
        eprintln!("sunset knobs: {}", sunset_names.join(", "));
        for required in [
            "AGENTFS_CLONE_TIMINGS",
            "AGENTFS_FUSE_SELF_INVAL",
            "AGENTFS_DRAIN_ON_RELEASE",
            "AGENTFS_DRAIN_ON_FORGET",
            "AGENTFS_FUSE_FLUSH_INVAL",
            "AGENTFS_FUSE_STICKY_KEEPCACHE_DROP",
            "AGENTFS_FUSE_URING_SPIN_US",
            "AGENTFS_DRAIN_ON_SETATTR",
            "AGENTFS_OVERLAY_READS",
        ] {
            assert!(
                sunset_names.contains(&required),
                "missing sunset row for {required}"
            );
        }
    }

    #[test]
    fn fuse_kill_switches_are_declared_and_gated() {
        let active = active_knobs();
        for name in [
            "AGENTFS_FUSE_NOOPEN",
            "AGENTFS_FUSE_NOFLUSH",
            "AGENTFS_FUSE_URING",
        ] {
            let knob = find_knob(&active, name);
            assert_eq!(knob.class, KnobClass::KillSwitch, "{name} class drifted");
            assert!(
                knob.gate.contains("VAL-FUSE"),
                "{name} must name its off-leg FUSE gate"
            );
            eprintln!("{} => {}", knob.name, knob.gate);
        }
    }

    fn find_knob<'a>(knobs: &'a [Knob], name: &str) -> &'a Knob {
        knobs
            .iter()
            .find(|knob| knob.name == name)
            .unwrap_or_else(|| panic!("missing knob declaration for {name}"))
    }

    fn assert_unique_names<'a>(knobs: impl Iterator<Item = &'a Knob>) {
        let mut seen = BTreeSet::new();
        let mut duplicates = Vec::new();
        for knob in knobs {
            if !seen.insert(knob.name) {
                duplicates.push(knob.name);
            }
        }
        assert!(duplicates.is_empty(), "duplicate knob rows: {duplicates:?}");
    }

    fn assert_runtime_env_mentions_are_declared() {
        let declared = active_knobs()
            .iter()
            .filter(|knob| {
                knob.name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_')
            })
            .map(|knob| knob.name)
            .collect::<BTreeSet<_>>();
        let mentioned = collect_runtime_env_mentions();
        let missing = mentioned
            .difference(&declared)
            .copied()
            .filter(|name| !ignored_env_token(name))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "runtime env mentions missing knob declarations: {missing:?}"
        );
    }

    fn collect_runtime_env_mentions() -> BTreeSet<&'static str> {
        let root = repo_root();
        let mut tokens = BTreeSet::new();
        for source_root in [root.join("cli/src"), root.join("sdk/rust/src")] {
            collect_runtime_env_mentions_from_path(&source_root, &mut tokens);
        }
        tokens
    }

    fn collect_runtime_env_mentions_from_path(path: &Path, tokens: &mut BTreeSet<&'static str>) {
        if path
            .components()
            .any(|component| component.as_os_str() == "target")
        {
            return;
        }
        if path.is_dir() {
            for entry in std::fs::read_dir(path).expect("source dir should be readable") {
                let entry = entry.expect("source entry should be readable");
                collect_runtime_env_mentions_from_path(&entry.path(), tokens);
            }
            return;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("knobs.rs") {
            return;
        }

        let contents = std::fs::read_to_string(path).expect("source file should be readable");
        for token in extract_env_tokens(&contents) {
            tokens.insert(Box::leak(token.into_boxed_str()));
        }
    }

    fn extract_env_tokens(contents: &str) -> BTreeSet<String> {
        let mut tokens = BTreeSet::new();
        for prefix in ["AGENTFS_", "TURSO_DB_AUTH_TOKEN"] {
            let mut offset = 0;
            while let Some(index) = contents[offset..].find(prefix) {
                let start = offset + index;
                let mut end = start + prefix.len();
                for ch in contents[end..].chars() {
                    if ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_' {
                        end += ch.len_utf8();
                    } else {
                        break;
                    }
                }
                tokens.insert(contents[start..end].to_string());
                offset = end;
            }
        }
        tokens
    }

    fn ignored_env_token(name: &str) -> bool {
        matches!(
            name,
            "AGENTFS_VERSION"
                | "AGENTFS_SCHEMA_VERSION"
                | "AGENTFS_SANDBOX"
                | "AGENTFS_SESSION"
                | "TURSO_DB_AUTH_TOKEN"
        ) || name.starts_with("AGENTFS_TEST_")
            || name.ends_with('_')
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("cli crate should have repo root parent")
            .to_path_buf()
    }
}
