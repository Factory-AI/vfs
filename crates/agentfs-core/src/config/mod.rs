//! Typed runtime configuration for the SDK/core crate.
//!
//! Environment variables are read only in this module. Everything downstream
//! receives typed values through [`CoreConfig`] or narrower config structs.

mod core;
pub mod env;

pub use core::{
    BatcherConfig, CoreConfig, Geometry, DEFAULT_CHUNK_SIZE, DEFAULT_INLINE_THRESHOLD,
    DEFAULT_WRITE_BATCH_BYTES, DEFAULT_WRITE_BATCH_GLOBAL_BYTES, DEFAULT_WRITE_BATCH_MS,
    DEFAULT_WRITE_BATCH_TXN_BYTES, DEFAULT_WRITE_BATCH_TXN_INODES,
};
pub use env::EnvReader;

#[cfg(test)]
mod tests {
    use super::{CoreConfig, EnvReader};
    use crate::fs::PartialOriginMode;
    use std::path::{Path, PathBuf};

    #[test]
    fn runtime_env_bool_grammar_is_shared() {
        let covered_runtime_bool_knobs = [
            "AGENTFS_FUSE_WRITEBACK",
            "AGENTFS_FUSE_NOOPEN",
            "AGENTFS_FUSE_NOFLUSH",
            "AGENTFS_FUSE_URING",
            "AGENTFS_PROFILE",
            "AGENTFS_DRAIN_ON_SETATTR",
        ];
        eprintln!(
            "covered runtime bool knobs: {}",
            covered_runtime_bool_knobs.join(", ")
        );

        for value in ["1", "true", "TRUE", "True", "yes", "YES", "on", "ON"] {
            assert_eq!(
                EnvReader::parse_bool(value),
                Some(true),
                "{value} should parse as true"
            );
        }

        for value in ["0", "false", "FALSE", "False", "no", "NO", "off", "OFF"] {
            assert_eq!(
                EnvReader::parse_bool(value),
                Some(false),
                "{value} should parse as false"
            );
        }

        for value in ["", "maybe", "truthy", "2", "enable"] {
            assert_eq!(
                EnvReader::parse_bool(value),
                None,
                "{value:?} should fall back to the caller default"
            );
        }

        let key = format!("AGENTFS_TEST_BOOL_GRAMMAR_{}", std::process::id());
        std::env::set_var(&key, "garbage");
        let reader = EnvReader::new();
        assert!(
            reader.bool(&key, true),
            "invalid true-default case should keep the default"
        );
        assert!(
            !reader.bool(&key, false),
            "invalid false-default case should keep the default"
        );
        std::env::remove_var(&key);
    }

    #[test]
    fn env_reader_is_the_only_runtime_env_entry() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo_root = manifest_dir
            .parent()
            .and_then(Path::parent)
            .expect("crates/agentfs-core should live two levels below the repo root");
        let source_roots = [
            SourceRoot::new(
                "crates/agentfs-core/src",
                manifest_dir.join("src"),
                SourceKind::Sdk,
            ),
            SourceRoot::new(
                "cli/src",
                repo_root.join("cli").join("src"),
                SourceKind::Cli,
            ),
            SourceRoot::new(
                "crates/agentfs-fuse/src",
                repo_root.join("crates").join("agentfs-fuse").join("src"),
                SourceKind::Fuse,
            ),
        ];
        let mut offenders = Vec::new();
        for source_root in &source_roots {
            scan_rs_files(source_root, &source_root.path, &mut offenders);
        }
        assert!(
            offenders.is_empty(),
            "runtime env reads outside config modules:\n{}",
            offenders.join("\n")
        );
    }

    struct SourceRoot {
        label: &'static str,
        path: PathBuf,
        kind: SourceKind,
    }

    impl SourceRoot {
        fn new(label: &'static str, path: PathBuf, kind: SourceKind) -> Self {
            Self { label, path, kind }
        }
    }

    #[derive(Clone, Copy)]
    enum SourceKind {
        Sdk,
        Cli,
        Fuse,
    }

    fn scan_rs_files(source_root: &SourceRoot, path: &Path, offenders: &mut Vec<String>) {
        if path
            .components()
            .any(|component| component.as_os_str() == "target")
            || is_test_path(source_root, path)
            || is_config_module(source_root, path)
        {
            return;
        }

        if path.is_dir() {
            for entry in std::fs::read_dir(path).expect("source dir should be readable") {
                let entry = entry.expect("source entry should be readable");
                scan_rs_files(source_root, &entry.path(), offenders);
            }
            return;
        }

        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return;
        }

        let contents = std::fs::read_to_string(path).expect("source file should be readable");
        let mut cfg_test_pending = false;
        let mut test_depth = 0usize;
        for (line_idx, line) in contents.lines().enumerate() {
            if is_test_only_line(line, &mut cfg_test_pending, &mut test_depth) {
                continue;
            }
            if line.contains("std::env::var(")
                || line.contains("std::env::var_os(")
                || line.contains("env::var(")
                || line.contains("env::var_os(")
            {
                let rel = path.strip_prefix(&source_root.path).unwrap_or(path);
                offenders.push(format!(
                    "{}/{}:{}",
                    source_root.label,
                    rel.display(),
                    line_idx + 1
                ));
            }
        }
    }

    fn is_test_path(source_root: &SourceRoot, path: &Path) -> bool {
        let rel = path.strip_prefix(&source_root.path).unwrap_or(path);
        rel.components()
            .any(|component| component.as_os_str() == "tests")
    }

    fn is_config_module(source_root: &SourceRoot, path: &Path) -> bool {
        let rel = path.strip_prefix(&source_root.path).unwrap_or(path);
        match source_root.kind {
            SourceKind::Sdk => rel
                .components()
                .any(|component| component.as_os_str() == "config"),
            SourceKind::Cli => {
                rel == Path::new("config.rs")
                    || rel == Path::new("fuse_config.rs")
                    || rel
                        .components()
                        .next()
                        .is_some_and(|component| component.as_os_str() == "config")
            }
            SourceKind::Fuse => rel == Path::new("adapter/config.rs"),
        }
    }

    fn is_test_only_line(line: &str, cfg_test_pending: &mut bool, test_depth: &mut usize) -> bool {
        if *test_depth > 0 {
            update_brace_depth(test_depth, line);
            return true;
        }

        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            *cfg_test_pending = true;
            if line.contains('{') {
                *test_depth = brace_delta(line).max(0) as usize;
                *cfg_test_pending = false;
            }
            return true;
        }

        if *cfg_test_pending {
            if line.contains('{') {
                *test_depth = brace_delta(line).max(0) as usize;
                *cfg_test_pending = false;
            }
            return true;
        }

        false
    }

    fn update_brace_depth(depth: &mut usize, line: &str) {
        let delta = brace_delta(line);
        if delta.is_negative() {
            *depth = depth.saturating_sub(delta.unsigned_abs());
        } else {
            *depth += delta as usize;
        }
    }

    fn brace_delta(line: &str) -> isize {
        line.chars().fold(0, |depth, ch| match ch {
            '{' => depth + 1,
            '}' => depth - 1,
            _ => depth,
        })
    }

    #[test]
    fn core_config_ignores_legacy_partial_origin_env() {
        let key = concat!("AGENTFS_OVERLAY_", "PARTIAL_ORIGIN");
        let previous = std::env::var(key).ok();
        std::env::set_var(key, "1");

        let config = CoreConfig::from_env();

        match previous {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }

        eprintln!(
            "legacy partial-origin env ignored; resolved policy is {:?}",
            config.partial_origin.mode
        );
        assert_eq!(config.partial_origin.mode, PartialOriginMode::Off);
    }
}
