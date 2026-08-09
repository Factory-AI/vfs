//! Frozen parent artifact store (`~/.vfs/artifacts/<sha256>.db`).
//!
//! Artifacts are immutable single-file session databases addressed by their
//! sha256. `vfs branch` publishes parent snapshots here; branch mounts open
//! them strictly read-only and refuse to serve on a digest mismatch. Because
//! the store is content-addressed, N branches of the same state share one
//! artifact, and an install that finds its digest already present simply
//! discards the staged copy.
//!
//! Staged files must live under `~/.vfs` (the session store): publication is
//! a same-filesystem rename so a crash never leaves a half-written artifact
//! at a digest path.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use vfs_core::Vfs;

use super::session_lock::SessionLock;

/// Root of the artifact store under `home`.
pub(crate) fn artifacts_root(home: &Path) -> PathBuf {
    home.join(".vfs").join("artifacts")
}

/// Hold the store lock shared for the duration of a fork's publication —
/// from artifact install until the referencing branch session is installed —
/// so `vfs prune artifacts` can never collect an artifact whose reference is
/// still being written.
pub(crate) fn lock_store_shared(home: &Path) -> Result<SessionLock> {
    let root = artifacts_root(home);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("Failed to create artifact store {}", root.display()))?;
    SessionLock::try_shared(&root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::anyhow!("the artifact store is being pruned; retry once it settles")
        } else {
            anyhow::Error::new(error).context("Failed to lock the artifact store")
        }
    })
}

fn lock_store_exclusive(home: &Path) -> Result<SessionLock> {
    let root = artifacts_root(home);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("Failed to create artifact store {}", root.display()))?;
    SessionLock::try_exclusive(&root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::anyhow!("a branch fork is publishing an artifact; retry once it settles")
        } else {
            anyhow::Error::new(error).context("Failed to lock the artifact store")
        }
    })
}

/// Path of the artifact with the given sha256 digest.
pub(crate) fn artifact_path(home: &Path, digest: &str) -> PathBuf {
    artifacts_root(home).join(format!("{digest}.db"))
}

/// Publish a staged single-file database into the store.
///
/// Consumes `staged` (renamed into place, or removed when its digest is
/// already installed) and returns the digest with the installed path. The
/// installed file is write-protected: immutability is what lets every later
/// mount trust the digest recorded by the branch that referenced it.
pub(crate) fn install_artifact(home: &Path, staged: &Path) -> Result<(String, PathBuf)> {
    let (digest, _size, _chunks) = super::pack::hash_file(staged, u64::MAX)
        .with_context(|| format!("Failed to hash staged artifact {}", staged.display()))?;
    let root = artifacts_root(home);
    std::fs::create_dir_all(&root)
        .with_context(|| format!("Failed to create artifact store {}", root.display()))?;
    let dest = artifact_path(home, &digest);

    if dest.is_file() {
        std::fs::remove_file(staged).with_context(|| {
            format!(
                "Failed to remove staged duplicate of installed artifact {}",
                staged.display()
            )
        })?;
        return Ok((digest, dest));
    }

    // Bytes become durable before the write-protect and rename; after the
    // rename only the directory entry is new, so a parent-dir sync completes
    // publication without ever write-opening the immutable file.
    std::fs::File::open(staged)
        .and_then(|file| file.sync_all())
        .with_context(|| format!("Failed to sync staged artifact {}", staged.display()))?;
    let mut permissions = std::fs::metadata(staged)
        .with_context(|| format!("Failed to stat staged artifact {}", staged.display()))?
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o444);
    }
    std::fs::set_permissions(staged, permissions)
        .with_context(|| format!("Failed to write-protect {}", staged.display()))?;
    std::fs::rename(staged, &dest).with_context(|| {
        format!(
            "Failed to publish artifact {} (staging must live under ~/.vfs)",
            dest.display()
        )
    })?;
    super::pack::sync_parent_directory(&dest)?;
    Ok((digest, dest))
}

/// One-line JSON report emitted by `vfs prune artifacts`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PruneArtifactsReport {
    pub(crate) dry_run: bool,
    pub(crate) removed: Vec<String>,
    pub(crate) kept: Vec<String>,
    pub(crate) reclaimed_bytes: u64,
}

/// `vfs prune artifacts`: collect unreferenced branch parent artifacts and
/// emit the one-line JSON report.
pub async fn handle_prune_artifacts(stdout: &mut impl std::io::Write, dry_run: bool) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    let report = prune_artifacts(&home, dry_run).await?;
    serde_json::to_writer(&mut *stdout, &report)?;
    writeln!(stdout)?;
    Ok(())
}

/// Remove artifacts no session chain references.
///
/// Classification is conservative: every session must be provably inactive
/// (exclusive lock, digest read from its finalized database) or provably
/// live and answerable (digest served over its control socket). Any session
/// that cannot be classified aborts the prune — deleting an artifact whose
/// reference merely could not be read would break a resumable branch.
pub(crate) async fn prune_artifacts(home: &Path, dry_run: bool) -> Result<PruneArtifactsReport> {
    let _store_lock = lock_store_exclusive(home)?;
    let root = artifacts_root(home);

    let mut candidates = BTreeSet::new();
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("Failed to list artifact store {}", root.display()))?
    {
        let name = entry?.file_name();
        let Some(name) = name.to_str() else { continue };
        if let Some(digest) = name.strip_suffix(".db") {
            if digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                candidates.insert(digest.to_string());
            }
        }
    }

    // Direct references: one digest (or none) per installed session.
    let mut worklist = Vec::new();
    let sessions_root = super::run::sessions_root(home);
    if sessions_root.is_dir() {
        for entry in std::fs::read_dir(&sessions_root)
            .with_context(|| format!("Failed to list sessions {}", sessions_root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let session_id = entry.file_name().to_string_lossy().to_string();
            let paths = super::run::SessionPaths::new(home, &session_id);
            if let Some(digest) = session_parent_digest(&paths).await? {
                worklist.push(digest);
            }
        }
    }

    // Transitive closure through the frozen artifacts themselves. A missing
    // ancestor is terminal here: its own mount already refuses, and it must
    // not shield unrelated artifacts from collection.
    let mut referenced = BTreeSet::new();
    while let Some(digest) = worklist.pop() {
        if !referenced.insert(digest.clone()) {
            continue;
        }
        let path = artifact_path(home, &digest);
        if !path.is_file() {
            continue;
        }
        let artifact = Vfs::open_read_only(&path)
            .await
            .with_context(|| format!("Failed to open artifact {}", path.display()))?;
        if let Some(parent) = artifact.overlay_parent_artifact().await? {
            worklist.push(parent);
        }
    }

    let mut removed = Vec::new();
    let mut kept = Vec::new();
    let mut reclaimed_bytes = 0u64;
    for digest in candidates {
        if referenced.contains(&digest) {
            kept.push(digest);
            continue;
        }
        let path = artifact_path(home, &digest);
        reclaimed_bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if !dry_run {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to remove artifact {}", path.display()))?;
        }
        removed.push(digest);
    }
    if !dry_run && !removed.is_empty() {
        std::fs::File::open(&root)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("Failed to sync artifact store {}", root.display()))?;
    }
    Ok(PruneArtifactsReport {
        dry_run,
        removed,
        kept,
        reclaimed_bytes,
    })
}

/// Read one session's direct parent digest, classifying it as provably
/// inactive or provably live first.
async fn session_parent_digest(paths: &super::run::SessionPaths) -> Result<Option<String>> {
    match SessionLock::try_exclusive(&paths.run_dir) {
        Ok(_lock) => {
            super::pack::recover_interrupted_publication(&paths.db_path)?;
            super::revert::recover_interrupted_publication(&paths.db_path)?;
            if !paths.db_path.is_file() {
                return Ok(None);
            }
            let vfs = Vfs::open_read_only(&paths.db_path).await.with_context(|| {
                format!(
                    "Cannot prune artifacts: failed to read session {}",
                    paths.session_id
                )
            })?;
            Ok(vfs.overlay_parent_artifact().await?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            let response = super::run::ctl::request(
                &paths.ctl_socket,
                &super::run::ctl::CtlRequest::ParentArtifact,
            )
            .await
            .with_context(|| {
                format!(
                    "Cannot prune artifacts: session {} is live but its control socket is \
                     unreachable; stop it and retry",
                    paths.session_id
                )
            })?;
            if !response.ok {
                bail!(
                    "Cannot prune artifacts: session {} rejected the digest request: {}",
                    paths.session_id,
                    response
                        .error
                        .unwrap_or_else(|| "unknown error".to_string())
                );
            }
            Ok(response.parent_artifact)
        }
        Err(error) => Err(anyhow::Error::new(error).context(format!(
            "Cannot prune artifacts: failed to classify session {}",
            paths.session_id
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_staged(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn install_publishes_write_protected_content_addressed_file() {
        let home = tempfile::tempdir().unwrap();
        let staging_dir = home.path().join(".vfs").join("run").join("s1");
        std::fs::create_dir_all(&staging_dir).unwrap();

        let staged = write_staged(&staging_dir, ".branch-a.tmp", b"artifact bytes");
        let (digest, installed) = install_artifact(home.path(), &staged).unwrap();

        assert_eq!(installed, artifact_path(home.path(), &digest));
        assert!(!staged.exists(), "staged file must be consumed");
        assert_eq!(std::fs::read(&installed).unwrap(), b"artifact bytes");
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&installed).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode, 0o444, "artifact must be write-protected");
    }

    #[test]
    fn reinstalling_the_same_bytes_dedupes_onto_one_artifact() {
        let home = tempfile::tempdir().unwrap();
        let staging_dir = home.path().join(".vfs").join("run").join("s1");
        std::fs::create_dir_all(&staging_dir).unwrap();

        let first = write_staged(&staging_dir, ".branch-a.tmp", b"same bytes");
        let (digest_a, path_a) = install_artifact(home.path(), &first).unwrap();
        let second = write_staged(&staging_dir, ".branch-b.tmp", b"same bytes");
        let (digest_b, path_b) = install_artifact(home.path(), &second).unwrap();

        assert_eq!(digest_a, digest_b);
        assert_eq!(path_a, path_b);
        assert!(!second.exists(), "duplicate staging must be discarded");
        let count = std::fs::read_dir(artifacts_root(home.path()))
            .unwrap()
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn different_bytes_land_at_different_digests() {
        let home = tempfile::tempdir().unwrap();
        let staging_dir = home.path().join(".vfs").join("run").join("s1");
        std::fs::create_dir_all(&staging_dir).unwrap();

        let first = write_staged(&staging_dir, ".branch-a.tmp", b"alpha");
        let second = write_staged(&staging_dir, ".branch-b.tmp", b"beta");
        let (digest_a, _) = install_artifact(home.path(), &first).unwrap();
        let (digest_b, _) = install_artifact(home.path(), &second).unwrap();
        assert_ne!(digest_a, digest_b);
    }

    use vfs_core::VfsOptions;

    async fn make_session(home: &Path, base: &Path, session_id: &str) {
        let paths = super::super::run::SessionPaths::new(home, session_id);
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        std::fs::write(&paths.base_path_file, base.to_string_lossy().as_bytes()).unwrap();
        let vfs = Vfs::open(VfsOptions::with_path(paths.db_path.to_string_lossy()).with_base(base))
            .await
            .unwrap();
        vfs.fs.finalize().await.unwrap();
    }

    async fn write_into_session(home: &Path, session_id: &str, name: &str) {
        let paths = super::super::run::SessionPaths::new(home, session_id);
        let vfs = Vfs::open(VfsOptions::with_path(paths.db_path.to_string_lossy()))
            .await
            .unwrap();
        let (_, file) = vfs
            .fs
            .create_file(&format!("/{name}"), 0o100644, 0, 0)
            .await
            .unwrap();
        file.pwrite(0, name.as_bytes()).await.unwrap();
        drop(file);
        vfs.fs.finalize().await.unwrap();
    }

    async fn branch(home: &Path, parent: &str, child: &str) -> String {
        let mut out = Vec::new();
        super::super::branch::branch_session(
            &mut out,
            home,
            parent.to_string(),
            Some(child.to_string()),
            None,
        )
        .await
        .unwrap();
        serde_json::from_slice::<serde_json::Value>(&out).unwrap()["parentArtifactSha256"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn remove_session(home: &Path, session_id: &str) {
        let paths = super::super::run::SessionPaths::new(home, session_id);
        std::fs::remove_dir_all(&paths.run_dir).unwrap();
    }

    #[tokio::test]
    async fn prune_keeps_referenced_and_removes_orphaned_artifacts() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        make_session(home.path(), base.path(), "p").await;
        let digest_a = branch(home.path(), "p", "b1").await;
        write_into_session(home.path(), "p", "changed.txt").await;
        let digest_b = branch(home.path(), "p", "b2").await;
        assert_ne!(digest_a, digest_b);

        // Foreign files in the store are not candidates and must survive.
        let root = artifacts_root(home.path());
        std::fs::write(root.join("README.txt"), b"foreign").unwrap();
        std::fs::write(root.join("deadbeef.db"), b"short name").unwrap();

        let report = prune_artifacts(home.path(), true).await.unwrap();
        assert!(report.dry_run);
        assert!(report.removed.is_empty(), "everything is referenced");
        assert_eq!(report.kept.len(), 2);

        remove_session(home.path(), "b2");
        let report = prune_artifacts(home.path(), false).await.unwrap();
        assert_eq!(report.removed, vec![digest_b.clone()]);
        assert_eq!(report.kept, vec![digest_a.clone()]);
        assert!(report.reclaimed_bytes > 0);
        assert!(!artifact_path(home.path(), &digest_b).exists());
        assert!(artifact_path(home.path(), &digest_a).is_file());
        assert!(root.join("README.txt").is_file());
        assert!(root.join("deadbeef.db").is_file());
    }

    #[tokio::test]
    async fn prune_follows_branch_chains_transitively() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        make_session(home.path(), base.path(), "p").await;
        let digest_a = branch(home.path(), "p", "b1").await;
        write_into_session(home.path(), "b1", "b1-owned.txt").await;
        let digest_b = branch(home.path(), "b1", "b2").await;

        // Only the leaf branch remains; its chain keeps the ancestor alive.
        remove_session(home.path(), "p");
        remove_session(home.path(), "b1");
        let report = prune_artifacts(home.path(), false).await.unwrap();
        assert!(report.removed.is_empty(), "chain must be kept: {report:?}");
        assert_eq!(report.kept.len(), 2);

        remove_session(home.path(), "b2");
        let mut report = prune_artifacts(home.path(), false).await.unwrap();
        report.removed.sort();
        let mut expected = vec![digest_a, digest_b];
        expected.sort();
        assert_eq!(report.removed, expected);
        assert!(report.kept.is_empty());
    }

    // The control server exists only where a mount owner serves it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn prune_asks_a_live_session_over_its_control_socket() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        make_session(home.path(), base.path(), "p").await;
        let digest = branch(home.path(), "p", "b1").await;
        remove_session(home.path(), "p");

        // Simulate a live owner of the branch: shared lock + control server.
        let paths = super::super::run::SessionPaths::new(home.path(), "b1");
        let owner_lock = SessionLock::try_shared(&paths.run_dir).unwrap();
        let owner_vfs = std::sync::Arc::new(
            Vfs::open(VfsOptions::with_path(paths.db_path.to_string_lossy()))
                .await
                .unwrap(),
        );
        let server = super::super::run::ctl::CtlServer::spawn(
            paths.ctl_socket.clone(),
            paths.run_dir.clone(),
            owner_vfs.clone(),
        )
        .unwrap();

        let report = prune_artifacts(home.path(), false).await.unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(report.kept, vec![digest]);

        server.shutdown().await;
        drop(owner_lock);
        drop(owner_vfs);
    }

    #[tokio::test]
    async fn prune_aborts_when_a_live_session_cannot_answer() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        make_session(home.path(), base.path(), "p").await;
        let digest = branch(home.path(), "p", "b1").await;

        let paths = super::super::run::SessionPaths::new(home.path(), "b1");
        let _owner_lock = SessionLock::try_shared(&paths.run_dir).unwrap();

        let error = prune_artifacts(home.path(), false).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("control socket"),
            "unexpected error: {error:#}"
        );
        assert!(
            artifact_path(home.path(), &digest).is_file(),
            "an aborted prune must not delete anything"
        );
    }

    #[tokio::test]
    async fn prune_fails_fast_while_a_fork_is_publishing() {
        let home = tempfile::tempdir().unwrap();
        let _fork = lock_store_shared(home.path()).unwrap();
        let error = prune_artifacts(home.path(), false).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("publishing"),
            "unexpected error: {error:#}"
        );
    }
}
