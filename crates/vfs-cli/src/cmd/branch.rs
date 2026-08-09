//! `vfs branch`: fork a run session into a new session at the parent's
//! current state.
//!
//! The parent database is snapshotted (through the live mount's control
//! socket when the session is running, or directly under the exclusive
//! session lock when it is not), published as an immutable content-addressed
//! artifact under `~/.vfs/artifacts/`, and a new session is installed whose
//! overlay reads fall through to that artifact. The artifact digest is
//! recorded inside the branch database, so a branch mount can refuse to
//! serve a drifted or missing parent (exact state or refusal).
//!
//! Branching a live session costs no stall beyond a batcher drain: the
//! snapshot is a `VACUUM INTO` read-transaction copy taken by the mount
//! owner, so writes acknowledged before the branch call are included and
//! later writes simply miss the snapshot.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;
use vfs_core::{Vfs, VfsOptions};

use super::artifacts;
use super::run::ctl;
use super::run::SessionPaths;
use super::session_lock::SessionLock;

/// One-line JSON emitted on success; fields are additive (transfer contract
/// discipline, same as the pack manifest).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchManifest {
    manifest_version: u32,
    session_id: String,
    parent_session_id: String,
    parent_artifact_sha256: String,
    artifact_path: PathBuf,
    base_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed_pin: Option<String>,
    parent_live: bool,
    vfs_version: String,
}

/// Fork `parent_session_id` into a new session and emit the branch manifest.
pub async fn handle_branch_command(
    stdout: &mut impl Write,
    parent_session_id: String,
    branch_session_id: Option<String>,
    _json: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    branch_session(stdout, &home, parent_session_id, branch_session_id).await
}

pub(crate) async fn branch_session(
    stdout: &mut impl Write,
    home: &Path,
    parent_session_id: String,
    branch_session_id: Option<String>,
) -> Result<()> {
    if !VfsOptions::validate_agent_id(&parent_session_id) {
        bail!("invalid session ID: {parent_session_id}");
    }
    let branch_session_id = branch_session_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    if !VfsOptions::validate_agent_id(&branch_session_id) {
        bail!("invalid session ID: {branch_session_id}");
    }
    if branch_session_id == parent_session_id {
        bail!("a session cannot branch onto itself");
    }

    let parent = SessionPaths::new(home, &parent_session_id);
    if !parent.run_dir.is_dir() {
        bail!("session not found: {}", parent.run_dir.display());
    }
    let branch = SessionPaths::new(home, &branch_session_id);
    if branch.run_dir.exists() {
        bail!(
            "branch session already exists: {}",
            branch.run_dir.display()
        );
    }
    let base_path = std::fs::read_to_string(&parent.base_path_file)
        .with_context(|| {
            format!(
                "Failed to read session base path {}",
                parent.base_path_file.display()
            )
        })
        .map(|raw| PathBuf::from(raw.trim()))?;
    if !base_path.is_dir() {
        bail!(
            "session base path {} is not a directory on this machine",
            base_path.display()
        );
    }

    let staging = parent
        .run_dir
        .join(format!(".branch-{}.tmp", Uuid::new_v4()));
    let staging_cleanup = RemoveOnDrop::armed(staging.clone());
    let parent_live = snapshot_parent(&parent, &staging).await?;

    // Handoff metadata is read from the snapshot (the parent may be live and
    // exclusively owned); any sidecar churn this causes lands before hashing.
    let (seed_pin, seeded_paths) = read_snapshot_metadata(&staging).await?;

    // Shared store lock spans install through session publication: an
    // artifact is only safe from `vfs prune artifacts` once a session
    // references it, so GC must not run between those two steps.
    let store_lock = artifacts::lock_store_shared(home)?;
    let (digest, artifact_path) = artifacts::install_artifact(home, &staging)?;
    staging_cleanup.disarm();

    install_branch_session(&branch, &base_path, &digest, &seed_pin, &seeded_paths)
        .await
        .with_context(|| {
            format!(
                "Failed to install branch session {}",
                branch.run_dir.display()
            )
        })?;
    drop(store_lock);

    let manifest = BranchManifest {
        manifest_version: 1,
        session_id: branch_session_id,
        parent_session_id,
        parent_artifact_sha256: digest,
        artifact_path,
        base_path,
        seed_pin,
        parent_live,
        vfs_version: super::version::VERSION.to_string(),
    };
    serde_json::to_writer(&mut *stdout, &manifest)?;
    writeln!(stdout)?;
    Ok(())
}

/// Produce a consistent snapshot of the parent database at `staging`.
///
/// Returns whether the parent was live. Classification is lock-derived:
/// exclusive acquisition proves no owner, and the control socket is only a
/// capability channel to whoever holds the shared lock. The single retry
/// covers the owner exiting between classification and connect.
async fn snapshot_parent(parent: &SessionPaths, staging: &Path) -> Result<bool> {
    for attempt in 0..2 {
        match SessionLock::try_exclusive(&parent.run_dir) {
            Ok(_lock) => {
                super::pack::recover_interrupted_publication(&parent.db_path)?;
                if !parent.db_path.is_file() {
                    bail!("session database not found: {}", parent.db_path.display());
                }
                let db_path = parent.db_path.to_str().with_context(|| {
                    format!(
                        "Database path contains non-UTF8 characters: {}",
                        parent.db_path.display()
                    )
                })?;
                let vfs = Vfs::open(
                    VfsOptions::with_path(db_path)
                        .with_core_config(crate::config::core_config_from_env()),
                )
                .await
                .map_err(|error| super::migrate::open_error_with_guidance(error, db_path))?;
                let result = vfs.snapshot_into(staging).await;
                vfs.fs
                    .finalize()
                    .await
                    .context("Failed to finalize the parent database after snapshotting")?;
                result.context("Failed to snapshot the parent database")?;
                return Ok(false);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let _shared = SessionLock::try_shared(&parent.run_dir).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::WouldBlock {
                        anyhow::anyhow!(
                            "session {} is being packed or seeded; retry once it settles",
                            parent.session_id
                        )
                    } else {
                        anyhow::Error::new(error).context("Failed to lock session for branching")
                    }
                })?;
                let request = ctl::CtlRequest::Snapshot {
                    dest: staging.to_path_buf(),
                };
                match ctl::request(&parent.ctl_socket, &request).await {
                    Ok(response) if response.ok => return Ok(true),
                    Ok(response) => bail!(
                        "live snapshot failed: {}",
                        response
                            .error
                            .unwrap_or_else(|| "unknown error".to_string())
                    ),
                    // The owner may have exited between lock classification
                    // and connect; one reclassification pass settles it.
                    Err(error) if attempt == 0 => {
                        tracing::debug!(error = %error, "control socket unreachable; reclassifying");
                        continue;
                    }
                    Err(error) => {
                        return Err(error.context(format!(
                            "session {} is live but its control socket is unreachable; \
                             its mount may predate branch support — stop the session and retry",
                            parent.session_id
                        )))
                    }
                }
            }
            Err(error) => {
                return Err(
                    anyhow::Error::new(error).context("Failed to lock session for branching")
                )
            }
        }
    }
    unreachable!("snapshot_parent loops at most twice and every arm returns")
}

/// Read the handoff metadata a branch inherits from its parent snapshot.
async fn read_snapshot_metadata(staging: &Path) -> Result<(Option<String>, Vec<String>)> {
    let staging_str = staging
        .to_str()
        .context("Staging path contains non-UTF8 characters")?;
    let vfs = Vfs::open(VfsOptions::with_path(staging_str))
        .await
        .context("Failed to open the staged parent snapshot")?;
    let result = async {
        let seed_pin = vfs.seed_pin().await?;
        let metadata = vfs.session_metadata().await?;
        Ok::<_, vfs_core::error::Error>((seed_pin, metadata.seeded_paths))
    }
    .await;
    vfs.fs
        .finalize()
        .await
        .context("Failed to finalize the staged parent snapshot")?;
    Ok(result?)
}

/// Create the branch session directory and its delta database.
async fn install_branch_session(
    branch: &SessionPaths,
    base_path: &Path,
    digest: &str,
    seed_pin: &Option<String>,
    seeded_paths: &[String],
) -> Result<()> {
    std::fs::create_dir_all(&branch.run_dir).with_context(|| {
        format!(
            "Failed to create branch session directory {}",
            branch.run_dir.display()
        )
    })?;
    let cleanup = RemoveDirOnDrop::armed(branch.run_dir.clone());

    let db_path = branch
        .db_path
        .to_str()
        .context("Branch database path contains non-UTF8 characters")?;
    let vfs = Vfs::open(
        VfsOptions::with_path(db_path)
            .with_base(base_path)
            .with_core_config(crate::config::core_config_from_env()),
    )
    .await
    .context("Failed to create the branch delta database")?;
    vfs.set_overlay_parent_artifact(digest).await?;
    if let Some(pin) = seed_pin {
        // The branch shares the parent's git base provenance; whiteouts and
        // content live in the parent artifact, so none are recorded here.
        vfs.record_seed_state(seeded_paths, &[], pin).await?;
    }
    vfs.fs
        .finalize()
        .await
        .context("Failed to finalize the branch delta database")?;
    drop(vfs);

    std::fs::write(
        &branch.base_path_file,
        base_path.to_string_lossy().as_bytes(),
    )
    .context("Failed to write the branch session base path")?;
    super::pack::sync_file_and_parent(&branch.base_path_file)?;
    cleanup.disarm();
    Ok(())
}

/// Removes a staged database family unless disarmed.
struct RemoveOnDrop(Option<PathBuf>);

impl RemoveOnDrop {
    fn armed(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            super::pack::remove_database_family(&path);
        }
    }
}

/// Removes a directory tree unless disarmed.
struct RemoveDirOnDrop(Option<PathBuf>);

impl RemoveDirOnDrop {
    fn armed(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn seed_parent_session(home: &Path, session_id: &str, base: &Path) -> SessionPaths {
        let paths = SessionPaths::new(home, session_id);
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        std::fs::write(&paths.base_path_file, base.to_string_lossy().as_bytes()).unwrap();
        let vfs = Vfs::open(VfsOptions::with_path(paths.db_path.to_string_lossy()).with_base(base))
            .await
            .unwrap();
        let (_, file) = vfs
            .fs
            .create_file("/parent-state.txt", 0o100644, 0, 0)
            .await
            .unwrap();
        file.pwrite(0, b"branch me").await.unwrap();
        drop(file);
        vfs.record_seed_state(&["a.txt".to_string()], &[], &"c".repeat(40))
            .await
            .unwrap();
        vfs.fs.finalize().await.unwrap();
        paths
    }

    #[tokio::test]
    async fn branching_an_inactive_session_installs_artifact_and_branch() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        seed_parent_session(home.path(), "parent-1", base.path()).await;

        let mut out = Vec::new();
        branch_session(
            &mut out,
            home.path(),
            "parent-1".to_string(),
            Some("branch-1".to_string()),
        )
        .await
        .unwrap();
        let manifest: BranchManifest = serde_json::from_slice(&out).unwrap();
        assert_eq!(manifest.session_id, "branch-1");
        assert_eq!(manifest.parent_session_id, "parent-1");
        assert!(!manifest.parent_live);
        assert_eq!(manifest.seed_pin.as_deref(), Some("c".repeat(40).as_str()));
        assert!(manifest.artifact_path.is_file());

        // The artifact carries the parent state and hashes to its own name.
        let (digest, _, _) =
            super::super::pack::hash_file(&manifest.artifact_path, u64::MAX).unwrap();
        assert_eq!(digest, manifest.parent_artifact_sha256);
        // Inspect through a writable copy: the installed artifact is 0444 and
        // must never be opened writable (that is the production contract too).
        let inspect_copy = home.path().join("inspect.db");
        std::fs::copy(&manifest.artifact_path, &inspect_copy).unwrap();
        let perms = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::Permissions::from_mode(0o600)
        };
        std::fs::set_permissions(&inspect_copy, perms).unwrap();
        let artifact = Vfs::open(VfsOptions::with_path(inspect_copy.to_string_lossy()))
            .await
            .unwrap();
        assert_eq!(
            artifact
                .fs
                .read_file("/parent-state.txt")
                .await
                .unwrap()
                .as_deref(),
            Some(b"branch me".as_slice())
        );
        drop(artifact);

        // The branch session is installed: empty delta, descriptor recorded,
        // seed provenance inherited, base path published.
        let branch = SessionPaths::new(home.path(), "branch-1");
        let branch_vfs = Vfs::open(VfsOptions::with_path(branch.db_path.to_string_lossy()))
            .await
            .unwrap();
        assert_eq!(
            branch_vfs
                .overlay_parent_artifact()
                .await
                .unwrap()
                .as_deref(),
            Some(manifest.parent_artifact_sha256.as_str())
        );
        assert_eq!(
            branch_vfs.seed_pin().await.unwrap().as_deref(),
            Some("c".repeat(40).as_str())
        );
        assert!(branch_vfs
            .fs
            .read_file("/parent-state.txt")
            .await
            .unwrap()
            .is_none());
        drop(branch_vfs);
        assert_eq!(
            std::fs::read_to_string(&branch.base_path_file).unwrap(),
            base.path().canonicalize().unwrap().to_string_lossy()
        );

        // No staging leftovers in the parent session dir.
        let leftovers = std::fs::read_dir(&SessionPaths::new(home.path(), "parent-1").run_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".branch-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[tokio::test]
    async fn branching_twice_from_the_same_state_shares_one_artifact() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        seed_parent_session(home.path(), "parent-2", base.path()).await;

        let mut out_a = Vec::new();
        branch_session(
            &mut out_a,
            home.path(),
            "parent-2".to_string(),
            Some("branch-a".to_string()),
        )
        .await
        .unwrap();
        let mut out_b = Vec::new();
        branch_session(
            &mut out_b,
            home.path(),
            "parent-2".to_string(),
            Some("branch-b".to_string()),
        )
        .await
        .unwrap();

        let a: BranchManifest = serde_json::from_slice(&out_a).unwrap();
        let b: BranchManifest = serde_json::from_slice(&out_b).unwrap();
        assert_eq!(a.parent_artifact_sha256, b.parent_artifact_sha256);
        // The store also holds its advisory lock file; count only artifacts.
        let count = std::fs::read_dir(artifacts::artifacts_root(home.path()))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".db")
            })
            .count();
        assert_eq!(count, 1, "identical parent state must share one artifact");
    }

    #[tokio::test]
    async fn branching_a_live_session_uses_the_control_socket() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let parent = seed_parent_session(home.path(), "parent-3", base.path()).await;

        // Simulate a live owner: shared session lock + control server.
        let owner_lock = SessionLock::try_shared(&parent.run_dir).unwrap();
        let owner_vfs = std::sync::Arc::new(
            Vfs::open(VfsOptions::with_path(parent.db_path.to_string_lossy()))
                .await
                .unwrap(),
        );
        let server = ctl::CtlServer::spawn(
            parent.ctl_socket.clone(),
            parent.run_dir.clone(),
            owner_vfs.clone(),
        )
        .unwrap();

        let mut out = Vec::new();
        branch_session(
            &mut out,
            home.path(),
            "parent-3".to_string(),
            Some("branch-live".to_string()),
        )
        .await
        .unwrap();
        let manifest: BranchManifest = serde_json::from_slice(&out).unwrap();
        assert!(manifest.parent_live);
        assert!(manifest.artifact_path.is_file());

        server.shutdown().await;
        drop(owner_lock);
        drop(owner_vfs);
    }

    #[tokio::test]
    async fn live_session_without_a_control_socket_is_an_error() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let parent = seed_parent_session(home.path(), "parent-4", base.path()).await;
        let _owner_lock = SessionLock::try_shared(&parent.run_dir).unwrap();

        let mut out = Vec::new();
        let error = branch_session(&mut out, home.path(), "parent-4".to_string(), None)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("control socket"),
            "unexpected error: {error:#}"
        );
    }
}
