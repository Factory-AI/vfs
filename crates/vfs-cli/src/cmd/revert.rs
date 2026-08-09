//! `vfs revert`: crash-safe offline publication of a reconstructed session.

use anyhow::{Context, Result};
use serde::Serialize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;
use vfs_core::{schema, Vfs, VfsOptions};

use super::pack::SessionStillRunning;
use super::run::InvalidRunSession;
use super::safety::remove_sqlite_sidecars_after_checkpoint;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RevertManifest {
    manifest_version: u32,
    session_id: String,
    target_seq: i64,
    source_head_seq: i64,
    root_snapshot_seq: i64,
    history_epoch: i64,
    generation: u64,
    db_path: PathBuf,
}

struct CandidateDatabase(PathBuf);

impl CandidateDatabase {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for CandidateDatabase {
    fn drop(&mut self) {
        super::pack::remove_database_family(&self.0);
    }
}

pub async fn handle_revert_command(
    stdout: &mut impl Write,
    session_id: String,
    target_seq: i64,
    json: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    revert_session(stdout, &home, session_id, target_seq, json).await
}

async fn revert_session(
    stdout: &mut impl Write,
    home: &Path,
    session_id: String,
    target_seq: i64,
    json: bool,
) -> Result<()> {
    if !VfsOptions::validate_agent_id(&session_id) {
        return Err(InvalidRunSession::new(format!("invalid session ID: {session_id}")).into());
    }
    let paths = super::run::session_paths(home, &session_id)?;
    if !paths.run_dir.is_dir() {
        return Err(InvalidRunSession::new(format!("session not found: {session_id}")).into());
    }
    let _ = super::run::read_session_base_path(&paths)?;

    let _lock =
        super::session_lock::SessionLock::try_exclusive(&paths.run_dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::Error::new(SessionStillRunning)
            } else {
                anyhow::Error::new(error).context("Failed to lock session for revert")
            }
        })?;
    super::pack::recover_interrupted_publication(&paths.db_path)?;
    recover_interrupted_publication(&paths.db_path)?;
    if !paths.db_path.is_file() {
        return Err(InvalidRunSession::new(format!(
            "invalid session {}: session database not found: {}",
            session_id,
            paths.db_path.display()
        ))
        .into());
    }
    super::pack::ensure_session_inactive(&paths)?;

    let candidate = CandidateDatabase::new(
        paths
            .run_dir
            .join(format!(".delta.db.revert-{}.tmp", Uuid::new_v4())),
    );
    super::pack::copy_database_family(&paths.db_path, candidate.path())
        .context("Failed to stage the session database for revert")?;

    let reconstruction = Vfs::reconstruct_to(candidate.path(), target_seq)
        .await
        .context("Failed to reconstruct the requested history target")?;
    let staged = Vfs::open(
        VfsOptions::with_path(candidate.path().to_string_lossy())
            .with_core_config(crate::config::core_config_from_env()),
    )
    .await
    .context("Failed to open the reconstructed session candidate")?;
    let publication_root = staged
        .establish_history_floor("revert")
        .await
        .context("Failed to establish the reverted session history floor")?;
    let metadata = staged
        .increment_session_generation()
        .await
        .context("Failed to increment the reverted session generation")?;
    staged
        .fs
        .finalize()
        .await
        .context("Failed to finalize the reverted session candidate")?;
    drop(staged);
    remove_sqlite_sidecars_after_checkpoint(candidate.path())?;

    super::pack::ensure_session_inactive(&paths)?;
    let backup = publish_candidate(candidate.path(), &paths.db_path)?;
    if let Err(error) = verify_published(
        &paths.db_path,
        publication_root.through_seq,
        metadata.generation,
    )
    .await
    {
        rollback_publication(&paths.db_path, &backup)?;
        return Err(error);
    }
    cleanup_backup_family(&backup);

    let manifest = RevertManifest {
        manifest_version: 1,
        session_id,
        target_seq: reconstruction.target_seq,
        source_head_seq: reconstruction.source_head_seq,
        root_snapshot_seq: reconstruction.root_snapshot_seq,
        history_epoch: reconstruction.history_epoch,
        generation: metadata.generation,
        db_path: paths.db_path,
    };
    if json {
        serde_json::to_writer(&mut *stdout, &manifest)?;
        writeln!(stdout)?;
    } else {
        writeln!(
            stdout,
            "Reverted session {} to history sequence {}.",
            manifest.session_id, manifest.target_seq
        )?;
        writeln!(stdout, "Generation: {}", manifest.generation)?;
        writeln!(stdout, "Database: {}", manifest.db_path.display())?;
    }
    Ok(())
}

fn publish_candidate(candidate: &Path, live: &Path) -> Result<PathBuf> {
    let backup = revert_backup_path(live);
    super::pack::rename_database_family(live, &backup)
        .context("Failed to stage the live database for revert publication")?;
    if let Err(error) = fs::rename(candidate, live)
        .with_context(|| format!("Failed to publish reverted database {}", live.display()))
        .and_then(|()| super::pack::sync_file_and_parent(live))
    {
        super::pack::remove_database_family(live);
        super::pack::rename_database_family(&backup, live)
            .context("Failed to restore the live database after revert publication failed")?;
        return Err(error);
    }
    Ok(backup)
}

async fn verify_published(live: &Path, history_floor_seq: i64, generation: u64) -> Result<()> {
    let vfs = Vfs::open_read_only(live)
        .await
        .context("Failed to reopen the published reverted database")?;
    let status = vfs
        .history_status()
        .await
        .context("Failed to verify reverted history status")?;
    if status.floor_seq != history_floor_seq
        || status.head_seq != history_floor_seq
        || !status.valid
    {
        anyhow::bail!(
            "published reverted history is inconsistent: expected valid range {history_floor_seq}..={history_floor_seq}, found {}..={} (valid={})",
            status.floor_seq,
            status.head_seq,
            status.valid
        );
    }
    let metadata = vfs.session_metadata().await?;
    if metadata.generation != generation {
        anyhow::bail!(
            "published reverted generation changed: expected {generation}, found {}",
            metadata.generation
        );
    }
    let conn = vfs.get_connection().await?;
    let report = schema::integrity::check(
        &conn,
        &schema::integrity::CheckOpts::new(live.to_path_buf()),
    )
    .await?;
    if !report.ok {
        let failures = report
            .checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| format!("{}: {}", check.name, check.detail))
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("published reverted database failed integrity checks: {failures}");
    }
    Ok(())
}

fn rollback_publication(live: &Path, backup: &Path) -> Result<()> {
    super::pack::remove_database_family(live);
    super::pack::rename_database_family(backup, live)
        .context("Failed to restore the pre-revert session database")?;
    super::pack::sync_file_and_parent(live)
}

pub(crate) fn recover_interrupted_publication(live: &Path) -> Result<()> {
    let backup = revert_backup_path(live);
    if database_family_exists(&backup) {
        if live.exists() {
            cleanup_backup_family(&backup);
        } else {
            super::pack::rename_database_family(&backup, live)
                .context("Failed to recover an interrupted revert publication")?;
            super::pack::sync_file_and_parent(live)?;
        }
    }
    cleanup_orphaned_candidates(live)
}

fn cleanup_orphaned_candidates(live: &Path) -> Result<()> {
    let Some(parent) = live.parent() else {
        return Ok(());
    };
    for entry in fs::read_dir(parent)
        .with_context(|| format!("Failed to inspect session directory {}", parent.display()))?
    {
        let path = entry?.path();
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default();
        if name.starts_with(".delta.db.revert-") && name.ends_with(".tmp") {
            super::pack::remove_database_family(&path);
        }
    }
    Ok(())
}

fn database_family_exists(path: &Path) -> bool {
    path.exists()
        || super::safety::sidecar_path(path, "-wal").exists()
        || super::safety::sidecar_path(path, "-shm").exists()
}

fn revert_backup_path(live: &Path) -> PathBuf {
    live.with_file_name("delta.db.revert-backup")
}

fn cleanup_backup_family(backup: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let path = if suffix.is_empty() {
            backup.to_path_buf()
        } else {
            super::safety::sidecar_path(backup, suffix)
        };
        if let Err(error) = fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "Warning: reverted session committed but failed to remove backup {}: {error}",
                    path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_restores_backup_and_removes_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("delta.db");
        let backup = revert_backup_path(&live);
        let candidate = dir.path().join(".delta.db.revert-orphan.tmp");
        fs::write(&backup, b"before").unwrap();
        fs::write(&candidate, b"candidate").unwrap();

        recover_interrupted_publication(&live).unwrap();

        assert_eq!(fs::read(&live).unwrap(), b"before");
        assert!(!backup.exists());
        assert!(!candidate.exists());
    }

    #[test]
    fn recovery_keeps_published_live_and_drops_backup() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("delta.db");
        let backup = revert_backup_path(&live);
        fs::write(&live, b"after").unwrap();
        fs::write(&backup, b"before").unwrap();

        recover_interrupted_publication(&live).unwrap();

        assert_eq!(fs::read(&live).unwrap(), b"after");
        assert!(!backup.exists());
    }

    #[test]
    fn revert_reuses_the_pack_live_exit_code() {
        assert_eq!(super::super::pack::SESSION_STILL_RUNNING_EXIT_CODE, 3);
    }
}
