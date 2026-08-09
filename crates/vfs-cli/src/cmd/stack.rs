//! Overlay stack construction shared by every mount surface.
//!
//! One session database plus the artifact store fully determines the
//! filesystem a mount serves: a plain database mounts directly, an overlay
//! database stacks over its recorded base, and a branch delta (one carrying
//! a `parent_artifact` digest) stacks over its frozen parent chain:
//!
//! ```text
//! overlay(delta, overlay(parent, ... overlay(root parent, host base)))
//! ```
//!
//! Every surface (`vfs run`, `vfs mount`, `vfs exec`, both backends) goes
//! through [`build_overlay`], so a branch session cannot be half-mounted by
//! a surface that forgot the parent layer. Parent artifacts are opened
//! strictly read-only and are verified byte-for-byte against the digest the
//! branch recorded: a missing or drifted parent refuses the mount rather
//! than serving a view that is not the branched state (exact state or
//! refusal).

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use vfs_core::{FileSystem, OverlayFS, PartialOriginPolicy, Vfs};

use super::artifacts;

/// Longest supported branch-of-branch chain. Lookup cost through the stack
/// grows quadratically with depth, and every ancestor database stays open
/// for the mount's lifetime, so an unbounded chain is refused rather than
/// served arbitrarily slowly.
const MAX_BRANCH_DEPTH: usize = 8;

/// A branch mount refusal: the recorded parent chain cannot be reproduced
/// exactly on this machine. `vfs run` maps this onto the invalid-session
/// exit contract; other surfaces report it as a plain error.
#[derive(Debug)]
pub(crate) struct BranchRefusal(String);

impl BranchRefusal {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for BranchRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for BranchRefusal {}

/// Build the overlay for `delta` on top of `base`, inserting the verified
/// read-only parent chain when the delta is a branch.
///
/// `base` is the host-side filesystem the caller already prepared (each
/// surface builds it differently: `vfs run` pins the directory through a
/// `/proc/self/fd` path and marks the FUSE mountpoint inode). The returned
/// overlay is not yet initialized; the caller runs its own `init`/`load`.
///
/// On branch mounts the configured partial-origin policy is forced Off:
/// partial-origin rows fingerprint real base files, and the branch's base is
/// a virtual stack, not a host directory.
pub(crate) async fn build_overlay(
    home: &Path,
    base: Arc<dyn FileSystem>,
    delta: &Vfs,
    partial_origin_policy: Option<PartialOriginPolicy>,
) -> Result<OverlayFS> {
    let mut chain = Vec::new();
    let mut next_digest = delta.overlay_parent_artifact().await?;
    while let Some(digest) = next_digest {
        if chain.len() >= MAX_BRANCH_DEPTH {
            return Err(BranchRefusal::new(format!(
                "branch parent chain exceeds the supported depth of {MAX_BRANCH_DEPTH}"
            ))
            .into());
        }
        let artifact = artifacts::artifact_path(home, &digest);
        if !artifact.is_file() {
            return Err(BranchRefusal::new(format!(
                "parent artifact {digest} is not installed at {}; \
                 branches resume only on the machine that created them",
                artifact.display()
            ))
            .into());
        }
        let (actual, _, _) = super::pack::hash_file(&artifact, u64::MAX)
            .with_context(|| format!("Failed to hash parent artifact {}", artifact.display()))?;
        if actual != digest {
            return Err(BranchRefusal::new(format!(
                "parent artifact {} no longer hashes to its recorded digest {digest} (got {actual}); \
                 refusing to serve a drifted parent state",
                artifact.display()
            ))
            .into());
        }
        let parent = Vfs::open_read_only(&artifact)
            .await
            .with_context(|| format!("Failed to open parent artifact {}", artifact.display()))?;
        next_digest = parent.overlay_parent_artifact().await?;
        chain.push(parent);
    }

    let is_branch = !chain.is_empty();
    // Compose root-most first so each ancestor reads through everything
    // beneath it, exactly as it did when its own session was live.
    let mut base = base;
    for parent in chain.into_iter().rev() {
        let overlay = OverlayFS::new_with_partial_origin_policy(
            base,
            parent.fs.clone(),
            PartialOriginPolicy::default(),
        );
        overlay
            .load()
            .await
            .context("Failed to load a parent artifact's overlay state")?;
        base = Arc::new(overlay);
    }

    let overlay = if is_branch {
        if partial_origin_policy.is_some() {
            tracing::warn!(
                "partial-origin policy is forced Off on branch mounts; ignoring the configured policy"
            );
        }
        OverlayFS::new_with_partial_origin_policy(
            base,
            delta.fs.clone(),
            PartialOriginPolicy::default(),
        )
    } else if let Some(policy) = partial_origin_policy {
        OverlayFS::new_with_partial_origin_policy(base, delta.fs.clone(), policy)
    } else {
        OverlayFS::new(base, delta.fs.clone())
    };
    Ok(overlay)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_core::{HostFS, VfsOptions};

    const ROOT_INO: i64 = 1;

    async fn make_session(home: &Path, base: &Path, session_id: &str) {
        let paths = super::super::run::SessionPaths::new(home, session_id);
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        std::fs::write(&paths.base_path_file, base.to_string_lossy().as_bytes()).unwrap();
        let vfs = Vfs::open(VfsOptions::with_path(paths.db_path.to_string_lossy()).with_base(base))
            .await
            .unwrap();
        let (_, file) = vfs
            .fs
            .create_file(&format!("/from-{session_id}.txt"), 0o100644, 0, 0)
            .await
            .unwrap();
        file.pwrite(0, session_id.as_bytes()).await.unwrap();
        drop(file);
        vfs.fs.finalize().await.unwrap();
    }

    async fn branch(home: &Path, parent_id: &str, branch_id: &str) {
        let mut out = Vec::new();
        super::super::branch::branch_session(
            &mut out,
            home,
            parent_id.to_string(),
            Some(branch_id.to_string()),
        )
        .await
        .unwrap();
    }

    async fn open_branch_delta(home: &Path, branch_id: &str) -> Vfs {
        let paths = super::super::run::SessionPaths::new(home, branch_id);
        Vfs::open(VfsOptions::with_path(paths.db_path.to_string_lossy()))
            .await
            .unwrap()
    }

    async fn read_path(overlay: &OverlayFS, name: &str) -> Option<Vec<u8>> {
        let stats = overlay.lookup(ROOT_INO, name).await.unwrap()?;
        let file = overlay.open(stats.ino, libc::O_RDONLY).await.unwrap();
        Some(file.pread(0, stats.size as u64).await.unwrap())
    }

    #[tokio::test]
    async fn branch_stack_serves_parent_state_under_new_writes() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        std::fs::write(base.path().join("host.txt"), b"host bytes").unwrap();
        make_session(home.path(), base.path(), "p1").await;
        branch(home.path(), "p1", "b1").await;

        let delta = open_branch_delta(home.path(), "b1").await;
        let host: Arc<dyn FileSystem> = Arc::new(HostFS::new(base.path()).unwrap());
        let overlay = build_overlay(home.path(), host, &delta, None)
            .await
            .unwrap();
        overlay.init(base.path().to_str().unwrap()).await.unwrap();

        // Parent-written and host state are both visible through the stack.
        assert_eq!(
            read_path(&overlay, "from-p1.txt").await.as_deref(),
            Some(b"p1".as_slice())
        );
        assert_eq!(
            read_path(&overlay, "host.txt").await.as_deref(),
            Some(b"host bytes".as_slice())
        );

        // Branch writes land in the branch delta only.
        let (_, file) = overlay
            .create_file(ROOT_INO, "branch.txt", 0o100644, 0, 0)
            .await
            .unwrap();
        file.pwrite(0, b"branch write").await.unwrap();
        file.fsync().await.unwrap();
        drop(file);
        assert_eq!(
            read_path(&overlay, "branch.txt").await.as_deref(),
            Some(b"branch write".as_slice())
        );
        assert!(
            delta.fs.read_file("/from-p1.txt").await.unwrap().is_none(),
            "parent content must not be copied into the branch delta by reads"
        );
    }

    #[tokio::test]
    async fn drifted_parent_artifact_refuses_the_mount() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        make_session(home.path(), base.path(), "p2").await;
        branch(home.path(), "p2", "b2").await;

        // Corrupt the installed artifact in place. The store also holds its
        // advisory lock file and read_dir order is filesystem-dependent, so
        // select the artifact by extension rather than enumeration order.
        let root = artifacts::artifacts_root(home.path());
        let artifact = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "db"))
            .expect("artifact store must hold one artifact");
        let perms = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::Permissions::from_mode(0o600)
        };
        std::fs::set_permissions(&artifact, perms).unwrap();
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&artifact)
            .unwrap();
        file.write_all(b"drift").unwrap();
        drop(file);

        let delta = open_branch_delta(home.path(), "b2").await;
        let host: Arc<dyn FileSystem> = Arc::new(HostFS::new(base.path()).unwrap());
        let error = match build_overlay(home.path(), host, &delta, None).await {
            Ok(_) => panic!("drifted artifact must refuse the mount"),
            Err(error) => error,
        };
        assert!(error.downcast_ref::<BranchRefusal>().is_some());
        assert!(format!("{error:#}").contains("drifted parent state"));
    }

    #[tokio::test]
    async fn missing_parent_artifact_refuses_the_mount() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        make_session(home.path(), base.path(), "p3").await;
        branch(home.path(), "p3", "b3").await;
        std::fs::remove_dir_all(artifacts::artifacts_root(home.path())).unwrap();

        let delta = open_branch_delta(home.path(), "b3").await;
        let host: Arc<dyn FileSystem> = Arc::new(HostFS::new(base.path()).unwrap());
        let error = match build_overlay(home.path(), host, &delta, None).await {
            Ok(_) => panic!("missing artifact must refuse the mount"),
            Err(error) => error,
        };
        assert!(error.downcast_ref::<BranchRefusal>().is_some());
        assert!(format!("{error:#}").contains("is not installed"));
    }

    #[tokio::test]
    async fn branch_of_branch_stacks_the_whole_chain() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        make_session(home.path(), base.path(), "p4").await;
        branch(home.path(), "p4", "b4").await;

        // Write into the first branch, then branch it again.
        {
            let delta = open_branch_delta(home.path(), "b4").await;
            let (_, file) = delta
                .fs
                .create_file("/from-b4.txt", 0o100644, 0, 0)
                .await
                .unwrap();
            file.pwrite(0, b"b4").await.unwrap();
            drop(file);
            delta.fs.finalize().await.unwrap();
        }
        branch(home.path(), "b4", "b5").await;

        let delta = open_branch_delta(home.path(), "b5").await;
        let host: Arc<dyn FileSystem> = Arc::new(HostFS::new(base.path()).unwrap());
        let overlay = build_overlay(home.path(), host, &delta, None)
            .await
            .unwrap();
        overlay.init(base.path().to_str().unwrap()).await.unwrap();
        assert_eq!(
            read_path(&overlay, "from-p4.txt").await.as_deref(),
            Some(b"p4".as_slice()),
            "grandparent state must be visible through the chain"
        );
        assert_eq!(
            read_path(&overlay, "from-b4.txt").await.as_deref(),
            Some(b"b4".as_slice()),
            "parent branch state must be visible through the chain"
        );
    }
}
