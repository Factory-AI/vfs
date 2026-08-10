//! Human and machine-readable version reporting.

use std::io::Write;

use anyhow::Result;
use serde::Serialize;
use vfs_core::schema;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    version: &'static str,
    commit: Option<&'static str>,
    artifact_version: &'static str,
    min_supported_artifact_version: &'static str,
    features: VersionFeatures,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VersionFeatures {
    uid_squash_run: bool,
    pack: bool,
    checkpoint: bool,
    seed: bool,
    adopt: bool,
    adopt_remote: bool,
    branch: bool,
    history: bool,
}

/// Print the build version and supported handoff capabilities.
pub fn handle_version_command(stdout: &mut impl Write, json: bool) -> Result<()> {
    let commit = option_env!("BUILD_COMMIT").filter(|commit| !commit.is_empty());
    let info = VersionInfo {
        version: VERSION,
        commit,
        artifact_version: schema::CURRENT.as_str(),
        min_supported_artifact_version: schema::MIN_SUPPORTED.as_str(),
        features: VersionFeatures {
            uid_squash_run: cfg!(target_os = "linux"),
            pack: true,
            checkpoint: true,
            seed: true,
            adopt: true,
            adopt_remote: true,
            branch: cfg!(unix),
            history: cfg!(unix),
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
        assert_eq!(value["artifactVersion"], schema::CURRENT.as_str());
        assert_eq!(
            value["minSupportedArtifactVersion"],
            schema::MIN_SUPPORTED.as_str()
        );
        assert_eq!(value["features"]["uidSquashRun"], cfg!(target_os = "linux"));
        assert_eq!(value["features"]["pack"], true);
        assert_eq!(value["features"]["checkpoint"], true);
        assert_eq!(value["features"]["seed"], true);
        assert_eq!(value["features"]["adopt"], true);
        assert_eq!(value["features"]["adoptRemote"], true);
        assert_eq!(value["features"]["branch"], cfg!(unix));
        assert_eq!(value["features"]["history"], cfg!(unix));
    }
}
