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
    use super::EnvReader;
    use std::path::Path;

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
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        scan_rs_files(&root, &root, &mut offenders);
        assert!(
            offenders.is_empty(),
            "runtime env reads outside sdk/rust/src/config:\n{}",
            offenders.join("\n")
        );
    }

    fn scan_rs_files(root: &Path, path: &Path, offenders: &mut Vec<String>) {
        if path
            .components()
            .any(|component| component.as_os_str() == "target" || component.as_os_str() == "config")
        {
            return;
        }

        if path.is_dir() {
            for entry in std::fs::read_dir(path).expect("source dir should be readable") {
                let entry = entry.expect("source entry should be readable");
                scan_rs_files(root, &entry.path(), offenders);
            }
            return;
        }

        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return;
        }

        let contents = std::fs::read_to_string(path).expect("source file should be readable");
        for (line_idx, line) in contents.lines().enumerate() {
            if line.contains("std::env::var(")
                || line.contains("std::env::var_os(")
                || line.contains("env::var(")
                || line.contains("env::var_os(")
            {
                let rel = path.strip_prefix(root).unwrap_or(path);
                offenders.push(format!("{}:{}", rel.display(), line_idx + 1));
            }
        }
    }
}
