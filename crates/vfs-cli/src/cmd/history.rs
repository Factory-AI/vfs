//! `vfs history`: inspect retained complete-transaction replay targets.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use uuid::Uuid;
use vfs_core::{Vfs, VfsOptions};

#[cfg(test)]
use super::run::SessionPaths;

const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryManifest {
    manifest_version: u32,
    session_id: String,
    history_epoch: i64,
    history_valid: bool,
    history_floor_seq: i64,
    history_head_seq: i64,
    targets: Vec<HistoryEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryEntry {
    seq: i64,
    txn_id: i64,
    label: String,
    wallclock_ms: i64,
    tables: Vec<String>,
    rows: usize,
}

struct PendingEntry {
    seq: i64,
    txn_id: i64,
    label: String,
    wallclock_ms: i64,
    tables: BTreeSet<String>,
    rows: usize,
}

impl PendingEntry {
    fn finish(self) -> HistoryEntry {
        HistoryEntry {
            seq: self.seq,
            txn_id: self.txn_id,
            label: self.label,
            wallclock_ms: self.wallclock_ms,
            tables: self.tables.into_iter().collect(),
            rows: self.rows,
        }
    }
}

pub async fn handle_history_command(
    stdout: &mut impl Write,
    session_id: String,
    limit: Option<usize>,
    all: bool,
    json: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    history_session(stdout, &home, session_id, limit, all, json).await
}

async fn history_session(
    stdout: &mut impl Write,
    home: &Path,
    session_id: String,
    limit: Option<usize>,
    all: bool,
    json: bool,
) -> Result<()> {
    if !VfsOptions::validate_agent_id(&session_id) {
        return Err(super::run::InvalidRunSession::new(format!(
            "invalid session ID: {session_id}"
        ))
        .into());
    }
    let limit = if all {
        None
    } else {
        let limit = limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            bail!("--limit must be at least 1");
        }
        Some(limit)
    };

    let paths = super::run::session_paths(home, &session_id)?;
    if !paths.run_dir.is_dir() {
        return Err(
            super::run::InvalidRunSession::new(format!("session not found: {session_id}")).into(),
        );
    }
    let _ = super::run::read_session_base_path(&paths)?;

    let staging = paths
        .run_dir
        .join(format!(".history-{}.tmp", Uuid::new_v4()));
    let _cleanup = super::branch::SnapshotCleanup::armed(staging.clone());
    super::branch::snapshot_parent(&paths, &staging)
        .await
        .context("Failed to snapshot the session for history inspection")?;

    let vfs = Vfs::open_read_only(&staging)
        .await
        .context("Failed to open the history snapshot")?;
    let status = vfs
        .history_status()
        .await
        .context("Failed to read filesystem history status")?;
    let entries = read_entries(&vfs, status.floor_seq, limit).await?;
    drop(vfs);

    if json {
        let manifest = HistoryManifest {
            manifest_version: 1,
            session_id,
            history_epoch: status.epoch,
            history_valid: status.valid,
            history_floor_seq: status.floor_seq,
            history_head_seq: status.head_seq,
            targets: entries,
        };
        serde_json::to_writer(&mut *stdout, &manifest)?;
        writeln!(stdout)?;
        return Ok(());
    }

    writeln!(stdout, "Session: {session_id}")?;
    writeln!(
        stdout,
        "History: epoch {}, {}, available {}..={}",
        status.epoch,
        if status.valid { "valid" } else { "invalid" },
        status.floor_seq,
        status.head_seq
    )?;
    for entry in entries {
        writeln!(
            stdout,
            "seq {}  {}  wallclockMs={}  tables={}  rows={}",
            entry.seq,
            entry.label,
            entry.wallclock_ms,
            entry.tables.join(","),
            entry.rows
        )?;
    }
    Ok(())
}

async fn read_entries(
    vfs: &Vfs,
    floor_seq: i64,
    limit: Option<usize>,
) -> Result<Vec<HistoryEntry>> {
    let conn = vfs.get_connection().await?;
    let mut rows = conn
        .query(
            "SELECT seq, txn_id, label, wallclock_ms, tbl
             FROM fs_op_journal
             WHERE seq > ?
             ORDER BY seq DESC",
            (floor_seq,),
        )
        .await?;
    let mut entries = Vec::new();
    let mut pending: Option<PendingEntry> = None;

    while let Some(row) = rows.next().await? {
        let seq: i64 = row.get(0)?;
        let txn_id: i64 = row.get(1)?;
        let label: String = row.get(2)?;
        let wallclock_ms: i64 = row.get(3)?;
        let table: String = row.get(4)?;

        if pending.as_ref().is_some_and(|entry| entry.txn_id != txn_id) {
            if let Some(entry) = pending.take() {
                entries.push(entry.finish());
            }
            if limit.is_some_and(|limit| entries.len() >= limit) {
                break;
            }
        }
        let entry = pending.get_or_insert_with(|| PendingEntry {
            seq,
            txn_id,
            label: label.clone(),
            wallclock_ms,
            tables: BTreeSet::new(),
            rows: 0,
        });
        // Rows are streamed newest-first, so the final value assigned within
        // a group is the first row's diagnostic label and wall clock.
        entry.label = label;
        entry.wallclock_ms = wallclock_ms;
        entry.tables.insert(table);
        entry.rows += 1;
    }
    if let Some(entry) = pending {
        if limit.is_none_or(|limit| entries.len() < limit) {
            entries.push(entry.finish());
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn history_json_is_newest_first_and_limited() {
        let home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let paths = SessionPaths::new(home.path(), "history-session");
        std::fs::create_dir_all(&paths.run_dir).unwrap();
        std::fs::write(
            &paths.base_path_file,
            base.path().to_string_lossy().as_bytes(),
        )
        .unwrap();
        let vfs = Vfs::open(VfsOptions::with_path(paths.db_path.to_string_lossy()))
            .await
            .unwrap();
        for name in ["one", "two", "three"] {
            vfs.fs.mkdir(&format!("/{name}"), 0, 0).await.unwrap();
        }
        vfs.fs.finalize().await.unwrap();

        let mut output = Vec::new();
        history_session(
            &mut output,
            home.path(),
            "history-session".to_string(),
            Some(2),
            false,
            true,
        )
        .await
        .unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&output).unwrap();
        let targets = manifest["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);
        assert!(targets[0]["seq"].as_i64().unwrap() > targets[1]["seq"].as_i64().unwrap());
        assert_eq!(manifest["historyValid"], true);
    }
}
