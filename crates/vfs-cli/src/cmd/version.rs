//! Human and machine-readable version reporting.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize)]
pub struct VersionInfo {
    version: &'static str,
    commit: Option<&'static str>,
    features: VersionFeatures,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionFeatures {
    uid_squash_run: bool,
    pack: bool,
    seed: bool,
}

/// Print the build version and supported handoff capabilities.
pub fn handle_version_command(stdout: &mut impl Write, json: bool) -> Result<()> {
    let commit = option_env!("BUILD_COMMIT").filter(|commit| !commit.is_empty());
    let info = VersionInfo {
        version: VERSION,
        commit,
        features: VersionFeatures {
            uid_squash_run: cfg!(target_os = "linux"),
            pack: true,
            seed: false,
        },
    };

    if json {
        serde_json::to_writer(&mut *stdout, &info)?;
        writeln!(stdout)?;
    } else if let Some(commit) = commit {
        writeln!(stdout, "vfs {} ({commit})", info.version)?;
    } else {
        writeln!(stdout, "vfs {}", info.version)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_output_has_capability_map() {
        let mut output = Vec::new();
        handle_version_command(&mut output, true).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(value["features"]["uidSquashRun"], cfg!(target_os = "linux"));
        assert_eq!(value["features"]["pack"], true);
        assert_eq!(value["features"]["seed"], false);
    }
}
