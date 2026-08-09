//! Replayable filesystem history.
//!
//! Root snapshots and row-delta journal groups are the only inputs to this
//! module. Reconstruction writes only a caller-owned staging database. The
//! live filesystem, application KV rows, tool calls, and generation metadata
//! are outside the replay scope.

use crate::error::{Error, Result};
use crate::schema::{
    self, CONFIG_HISTORY_EPOCH_KEY, CONFIG_HISTORY_FLOOR_SEQ_KEY, CONFIG_HISTORY_VALID_KEY,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

const PARENT_ARTIFACT_KEY: &str = "parent_artifact";
const REPLAY_LIVE_TABLES: &[&str] = &[
    "fs_dentry",
    "fs_data",
    "fs_symlink",
    "fs_whiteout",
    "fs_origin",
    "fs_partial_origin",
    "fs_chunk_override",
    "fs_inode",
];
const SNAPSHOT_CHILD_TABLES: &[&str] = &[
    "fs_snapshot_meta",
    "fs_snapshot_chunk",
    "fs_snapshot_chunk_override",
    "fs_snapshot_partial_origin",
    "fs_snapshot_origin",
    "fs_snapshot_whiteout",
    "fs_snapshot_symlink",
    "fs_snapshot_data",
    "fs_snapshot_dentry",
    "fs_snapshot_inode",
];

/// Header for an immutable relational filesystem root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotHeader {
    pub snapshot_id: i64,
    pub through_seq: i64,
    pub created_at_ms: i64,
    pub reason: String,
    pub history_epoch: i64,
}

/// One complete committed transaction target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryTarget {
    pub seq: i64,
    pub txn_id: Option<i64>,
    pub label: Option<String>,
    pub wallclock_ms: Option<i64>,
    pub row_count: usize,
}

/// Retained replay range and complete transaction boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryStatus {
    pub floor_seq: i64,
    pub head_seq: i64,
    pub epoch: i64,
    pub valid: bool,
    pub targets: Vec<HistoryTarget>,
}

/// Result of validating a replay target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatedHistoryTarget {
    pub target_seq: i64,
    pub root: SnapshotHeader,
    pub floor_seq: i64,
    pub head_seq: i64,
    pub epoch: i64,
}

/// Result of replacing a staging database's filesystem state with a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconstructionInfo {
    pub target_seq: i64,
    pub source_head_seq: i64,
    pub root_snapshot_seq: i64,
    pub history_epoch: i64,
}

#[derive(Debug, Clone, Copy)]
struct Markers {
    epoch: i64,
    valid: bool,
    floor: i64,
    head: i64,
}

#[derive(Debug)]
struct JournalRow {
    seq: i64,
    txn_id: i64,
    tbl: String,
    verb: String,
    row: String,
}

#[derive(Debug, Deserialize)]
struct InodeDelta {
    ino: i64,
    mode: i64,
    nlink: i64,
    uid: i64,
    gid: i64,
    size: i64,
    atime: i64,
    mtime: i64,
    ctime: i64,
    rdev: i64,
    atime_nsec: i64,
    mtime_nsec: i64,
    ctime_nsec: i64,
    data_inline_digest: Option<String>,
    storage_kind: i64,
}

#[derive(Debug, Deserialize)]
struct InodeKey {
    ino: i64,
}

#[derive(Debug, Deserialize)]
struct DentryDelta {
    #[serde(default)]
    id: Option<i64>,
    parent_ino: i64,
    name: String,
    ino: i64,
}

#[derive(Debug, Deserialize)]
struct DentryKey {
    parent_ino: i64,
    name: String,
}

#[derive(Debug, Deserialize)]
struct DataDelta {
    ino: i64,
    chunk_index: i64,
    digest: String,
}

#[derive(Debug, Deserialize)]
struct DataKey {
    ino: i64,
    chunk_index: i64,
}

#[derive(Debug, Deserialize)]
struct SymlinkDelta {
    ino: i64,
    target: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WhiteoutDelta {
    path: String,
    parent_path: String,
    created_at: i64,
}

#[derive(Debug, Deserialize)]
struct WhiteoutKey {
    path: String,
}

#[derive(Debug, Deserialize)]
struct OriginDelta {
    delta_ino: i64,
    base_ino: i64,
}

#[derive(Debug, Deserialize)]
struct DeltaInoKey {
    delta_ino: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct PartialOriginDelta {
    delta_ino: i64,
    base_ino: i64,
    base_path: String,
    base_size: i64,
    base_fingerprint_size: i64,
    base_mtime: i64,
    base_mtime_nsec: i64,
    base_ctime: i64,
    base_ctime_nsec: i64,
    created_at: i64,
}

#[derive(Debug, Deserialize)]
struct ChunkOverrideDelta {
    delta_ino: i64,
    chunk_index: i64,
}

#[derive(Debug, Deserialize)]
struct OverlayConfigDelta {
    key: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Debug, Clone)]
struct ReplayInode {
    ino: i64,
    mode: i64,
    nlink: i64,
    uid: i64,
    gid: i64,
    size: i64,
    atime: i64,
    mtime: i64,
    ctime: i64,
    rdev: i64,
    atime_nsec: i64,
    mtime_nsec: i64,
    ctime_nsec: i64,
    data_inline_digest: Option<Vec<u8>>,
    storage_kind: i64,
}

#[derive(Debug, Clone)]
struct ReplayDentry {
    id: i64,
    parent_ino: i64,
    name: String,
    ino: i64,
}

#[derive(Default)]
struct ReplayState {
    inodes: BTreeMap<i64, ReplayInode>,
    dentries: BTreeMap<(i64, String), ReplayDentry>,
    data: BTreeMap<(i64, i64), Vec<u8>>,
    symlinks: BTreeMap<i64, String>,
    whiteouts: BTreeMap<String, WhiteoutDelta>,
    origins: BTreeMap<i64, i64>,
    partial_origins: BTreeMap<i64, PartialOriginDelta>,
    chunk_overrides: BTreeSet<(i64, i64)>,
    overlay_config: BTreeMap<String, String>,
    meta: BTreeMap<String, String>,
}

/// Reconcile the durable epoch markers for one writable open.
///
/// A disabled journal invalidates the current epoch once. Re-enabling after a
/// gap drops the stale replay plane, bumps the epoch, captures the current
/// root, and publishes that root as the new floor.
pub(crate) async fn reconcile_epoch(conn: &Connection, journaling_enabled: bool) -> Result<()> {
    let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
    let result = async {
        let markers = read_markers(conn).await?;
        if !journaling_enabled {
            if markers.valid {
                set_config_i64(conn, CONFIG_HISTORY_VALID_KEY, 0).await?;
            }
            return Ok(());
        }
        if markers.valid {
            return Ok(());
        }

        let next_epoch = markers
            .epoch
            .checked_add(1)
            .ok_or_else(|| Error::Internal("history epoch overflow".to_string()))?;
        delete_all_snapshots(conn).await?;
        conn.execute("DELETE FROM fs_journal_chunk", ()).await?;
        conn.execute("DELETE FROM fs_op_journal", ()).await?;
        let snapshot_id = schema::capture_root_raw(conn, "epoch", next_epoch, markers.head).await?;
        let root = snapshot_header(conn, snapshot_id).await?;
        set_config_i64(conn, CONFIG_HISTORY_EPOCH_KEY, next_epoch).await?;
        set_config_i64(conn, CONFIG_HISTORY_FLOOR_SEQ_KEY, root.through_seq).await?;
        set_config_i64(conn, CONFIG_HISTORY_VALID_KEY, 1).await?;
        collect_unpinned_chunks(conn).await
    }
    .await;
    match result {
        Ok(()) => txn.commit().await?,
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    }
    Ok(())
}

/// Capture the current filesystem root at the acknowledged journal head.
pub(crate) async fn capture_root(conn: &Connection, reason: &str) -> Result<SnapshotHeader> {
    let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
    let result = async {
        let markers = read_markers(conn).await?;
        if !markers.valid {
            return Err(Error::HistoryInvalid {
                epoch: markers.epoch,
                floor_seq: markers.floor,
                head_seq: markers.head,
            });
        }
        if let Some(root) = snapshot_at(conn, markers.epoch, markers.head).await? {
            return Ok(root);
        }
        let snapshot_id =
            schema::capture_root_raw(conn, reason, markers.epoch, markers.head).await?;
        snapshot_header(conn, snapshot_id).await
    }
    .await;
    match result {
        Ok(header) => {
            txn.commit().await?;
            Ok(header)
        }
        Err(error) => {
            let _ = txn.rollback().await;
            Err(error)
        }
    }
}

/// Return the retained range and every complete target in ascending order.
pub(crate) async fn status(conn: &Connection) -> Result<HistoryStatus> {
    let markers = read_markers(conn).await?;
    let mut targets = vec![HistoryTarget {
        seq: markers.floor,
        txn_id: None,
        label: None,
        wallclock_ms: None,
        row_count: 0,
    }];
    let mut rows = conn
        .query(
            "SELECT txn_id, MIN(label), MIN(wallclock_ms), COUNT(*), MAX(seq)
             FROM fs_op_journal
             WHERE seq > ?
             GROUP BY txn_id
             ORDER BY txn_id",
            (markers.floor,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        targets.push(HistoryTarget {
            txn_id: Some(row.get(0)?),
            label: Some(row.get(1)?),
            wallclock_ms: Some(row.get(2)?),
            row_count: usize::try_from(row.get::<i64>(3)?)
                .map_err(|_| Error::Internal("negative history row count".to_string()))?,
            seq: row.get(4)?,
        });
    }
    targets.dedup_by_key(|target| target.seq);
    Ok(HistoryStatus {
        floor_seq: markers.floor,
        head_seq: markers.head,
        epoch: markers.epoch,
        valid: markers.valid,
        targets,
    })
}

/// Validate range, epoch, transaction-boundary, snapshot, and contiguity rules.
pub(crate) async fn validate_target(
    conn: &Connection,
    target_seq: i64,
) -> Result<ValidatedHistoryTarget> {
    let markers = read_markers(conn).await?;
    validate_target_with_markers(conn, target_seq, markers).await
}

/// Replace live filesystem/overlay rows with the exact target state.
pub(crate) async fn reconstruct(
    conn: &Connection,
    database_path: &Path,
    target_seq: i64,
) -> Result<ReconstructionInfo> {
    let validated = validate_target(conn, target_seq).await?;
    let source_head_seq = validated.head_seq;
    let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
    let result = async {
        let max_inode_ever = max_known_inode(conn).await?;
        let (state, chunks) = materialize_state(conn, &validated.root, target_seq).await?;
        install_state_as_live(conn, &state, &chunks).await?;
        verify_reconstructed_digests(conn).await?;
        trim_future(conn, target_seq, validated.epoch).await?;
        schema::rebuild_journal_allocator(conn, target_seq).await?;
        recompute_chunk_refcounts(conn).await?;
        verify_inode_allocator(conn, max_inode_ever).await?;
        set_config_i64(conn, CONFIG_HISTORY_FLOOR_SEQ_KEY, validated.floor_seq).await?;
        collect_unpinned_chunks(conn).await?;
        visible_tree_sanity(conn).await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => txn.commit().await?,
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    }

    let report =
        schema::integrity::check(conn, &schema::integrity::CheckOpts::new(database_path)).await?;
    if !report.ok {
        let failed = report
            .checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| format!("{}: {}", check.name, check.detail))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Error::HistoryIntegrity(failed));
    }

    Ok(ReconstructionInfo {
        target_seq,
        source_head_seq,
        root_snapshot_seq: validated.root.through_seq,
        history_epoch: validated.epoch,
    })
}

/// Advance the retained floor to a complete boundary while preserving a root
/// at the new floor and a contiguous journal suffix.
pub(crate) async fn journal_gc(conn: &Connection, retention_ops: usize) -> Result<()> {
    if retention_ops == 0 {
        return Err(Error::Internal(
            "journal retention must be positive".to_string(),
        ));
    }
    let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
    let result = async {
        let markers = read_markers(conn).await?;
        if !markers.valid {
            return collect_unpinned_chunks(conn).await;
        }
        let retention = i64::try_from(retention_ops)
            .map_err(|_| Error::Internal("journal retention is too large".to_string()))?;
        let horizon = markers.head.saturating_sub(retention);
        if horizon <= markers.floor {
            return collect_unpinned_chunks(conn).await;
        }

        let mut rows = conn
            .query(
                "SELECT MAX(seq)
                 FROM fs_op_journal
                 GROUP BY txn_id
                 HAVING MAX(seq) <= ?
                 ORDER BY MAX(seq) DESC
                 LIMIT 1",
                (horizon,),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return collect_unpinned_chunks(conn).await;
        };
        let boundary: i64 = row.get(0)?;
        drop(rows);
        if boundary <= markers.floor {
            return collect_unpinned_chunks(conn).await;
        }

        let validated = validate_target_with_markers(conn, boundary, markers).await?;
        let (state, _) = materialize_state(conn, &validated.root, boundary).await?;
        delete_snapshot_at(conn, markers.epoch, boundary).await?;
        let snapshot_id = capture_state_root(conn, &state, "gc", markers.epoch, boundary).await?;
        let root = snapshot_header(conn, snapshot_id).await?;
        if root.through_seq != boundary {
            return Err(Error::HistoryIntegrity(format!(
                "GC root landed at {}, expected {boundary}",
                root.through_seq
            )));
        }
        set_config_i64(conn, CONFIG_HISTORY_FLOOR_SEQ_KEY, boundary).await?;
        delete_journal_through(conn, boundary).await?;
        delete_snapshots_before(conn, markers.epoch, boundary).await?;
        collect_unpinned_chunks(conn).await
    }
    .await;
    match result {
        Ok(()) => txn.commit().await?,
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    }
    Ok(())
}

/// Establish a generation boundary at the current state.
///
/// Pack calls this after materializing any parent chain. It captures the live
/// state at the current head, removes every pre-pack replay target, and keeps
/// that root as the sole floor of the current epoch.
pub(crate) async fn establish_fresh_floor(
    conn: &Connection,
    reason: &str,
) -> Result<SnapshotHeader> {
    let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
    let result = async {
        let markers = read_markers(conn).await?;
        if !markers.valid {
            return Err(Error::HistoryInvalid {
                epoch: markers.epoch,
                floor_seq: markers.floor,
                head_seq: markers.head,
            });
        }
        delete_snapshot_at(conn, markers.epoch, markers.head).await?;
        let snapshot_id =
            schema::capture_root_raw(conn, reason, markers.epoch, markers.head).await?;
        let root = snapshot_header(conn, snapshot_id).await?;
        set_config_i64(conn, CONFIG_HISTORY_FLOOR_SEQ_KEY, markers.head).await?;
        delete_journal_through(conn, markers.head).await?;
        delete_snapshots_except(conn, markers.epoch, root.snapshot_id).await?;
        collect_unpinned_chunks(conn).await?;
        Ok(root)
    }
    .await;
    match result {
        Ok(root) => {
            txn.commit().await?;
            Ok(root)
        }
        Err(error) => {
            let _ = txn.rollback().await;
            Err(error)
        }
    }
}

async fn validate_target_with_markers(
    conn: &Connection,
    target_seq: i64,
    markers: Markers,
) -> Result<ValidatedHistoryTarget> {
    if !markers.valid {
        return Err(Error::HistoryInvalid {
            epoch: markers.epoch,
            floor_seq: markers.floor,
            head_seq: markers.head,
        });
    }
    if target_seq < markers.floor || target_seq > markers.head {
        return Err(Error::HistoryTargetOutOfRange {
            target_seq,
            floor_seq: markers.floor,
            head_seq: markers.head,
            epoch: markers.epoch,
        });
    }

    if target_seq != markers.floor {
        let mut rows = conn
            .query(
                "SELECT txn_id FROM fs_op_journal WHERE seq = ?",
                (target_seq,),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Err(Error::HistoryGap {
                target_seq,
                snapshot_seq: markers.floor,
                expected_seq: target_seq,
                found_seq: None,
                floor_seq: markers.floor,
                head_seq: markers.head,
                epoch: markers.epoch,
            });
        };
        let txn_id: i64 = row.get(0)?;
        drop(rows);
        let mut rows = conn
            .query(
                "SELECT seq
                 FROM fs_op_journal
                 WHERE txn_id = ?
                 ORDER BY seq DESC
                 LIMIT 1",
                (txn_id,),
            )
            .await?;
        let transaction_end_seq: i64 = rows
            .next()
            .await?
            .ok_or_else(|| Error::Internal("history transaction has no rows".to_string()))?
            .get(0)?;
        if transaction_end_seq != target_seq {
            return Err(Error::HistoryTargetMidTransaction {
                target_seq,
                txn_id,
                transaction_end_seq,
                floor_seq: markers.floor,
                head_seq: markers.head,
                epoch: markers.epoch,
            });
        }
    }

    let root = nearest_snapshot(conn, markers.epoch, target_seq)
        .await?
        .ok_or(Error::HistorySnapshotMissing {
            target_seq,
            floor_seq: markers.floor,
            head_seq: markers.head,
            epoch: markers.epoch,
        })?;
    validate_contiguous_rows(conn, &root, target_seq, markers).await?;
    Ok(ValidatedHistoryTarget {
        target_seq,
        root,
        floor_seq: markers.floor,
        head_seq: markers.head,
        epoch: markers.epoch,
    })
}

async fn validate_contiguous_rows(
    conn: &Connection,
    root: &SnapshotHeader,
    target_seq: i64,
    markers: Markers,
) -> Result<()> {
    let mut expected = root.through_seq.saturating_add(1);
    let mut current_txn = None;
    let mut current_txn_first = 0;
    let mut rows = conn
        .query(
            "SELECT seq, txn_id
             FROM fs_op_journal
             WHERE seq > ? AND seq <= ?
             ORDER BY seq",
            (root.through_seq, target_seq),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let seq: i64 = row.get(0)?;
        let txn_id: i64 = row.get(1)?;
        if seq != expected {
            return Err(history_gap(
                target_seq,
                root.through_seq,
                expected,
                Some(seq),
                markers,
            ));
        }
        match current_txn {
            None => {
                if txn_id != seq {
                    return Err(history_gap(
                        target_seq,
                        root.through_seq,
                        seq,
                        Some(txn_id),
                        markers,
                    ));
                }
                current_txn = Some(txn_id);
                current_txn_first = seq;
            }
            Some(active) if active == txn_id => {}
            Some(_) => {
                if txn_id != seq || current_txn_first <= root.through_seq {
                    return Err(history_gap(
                        target_seq,
                        root.through_seq,
                        seq,
                        Some(txn_id),
                        markers,
                    ));
                }
                current_txn = Some(txn_id);
                current_txn_first = seq;
            }
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(|| Error::Internal("history seq overflow".to_string()))?;
    }
    if expected != target_seq.saturating_add(1) {
        return Err(history_gap(
            target_seq,
            root.through_seq,
            expected,
            None,
            markers,
        ));
    }
    Ok(())
}

fn history_gap(
    target_seq: i64,
    snapshot_seq: i64,
    expected_seq: i64,
    found_seq: Option<i64>,
    markers: Markers,
) -> Error {
    Error::HistoryGap {
        target_seq,
        snapshot_seq,
        expected_seq,
        found_seq,
        floor_seq: markers.floor,
        head_seq: markers.head,
        epoch: markers.epoch,
    }
}

async fn materialize_state(
    conn: &Connection,
    root: &SnapshotHeader,
    target_seq: i64,
) -> Result<(ReplayState, BTreeMap<Vec<u8>, Vec<u8>>)> {
    let mut state = load_snapshot_state(conn, root.snapshot_id).await?;
    let mut rows = conn
        .query(
            "SELECT seq, txn_id, tbl, verb, row
             FROM fs_op_journal
             WHERE seq > ? AND seq <= ?
             ORDER BY seq",
            (root.through_seq, target_seq),
        )
        .await?;
    let mut active_txn = None;
    while let Some(row) = rows.next().await? {
        let delta = JournalRow {
            seq: row.get(0)?,
            txn_id: row.get(1)?,
            tbl: row.get(2)?,
            verb: row.get(3)?,
            row: row.get(4)?,
        };
        if active_txn != Some(delta.txn_id) {
            if delta.txn_id != delta.seq {
                return Err(Error::HistoryIntegrity(format!(
                    "journal transaction {} begins at seq {}",
                    delta.txn_id, delta.seq
                )));
            }
            active_txn = Some(delta.txn_id);
        }
        apply_delta_to_state(&mut state, &delta)?;
    }
    drop(rows);
    let chunks = load_referenced_chunks(conn, &state).await?;
    Ok((state, chunks))
}

async fn load_snapshot_state(conn: &Connection, snapshot_id: i64) -> Result<ReplayState> {
    let mut state = ReplayState::default();
    let mut rows = conn
        .query(
            "SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
                    atime_nsec, mtime_nsec, ctime_nsec, data_inline_digest, storage_kind
             FROM fs_snapshot_inode WHERE snapshot_id = ? ORDER BY ino",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let ino = row.get(0)?;
        state.inodes.insert(
            ino,
            ReplayInode {
                ino,
                mode: row.get(1)?,
                nlink: row.get(2)?,
                uid: row.get(3)?,
                gid: row.get(4)?,
                size: row.get(5)?,
                atime: row.get(6)?,
                mtime: row.get(7)?,
                ctime: row.get(8)?,
                rdev: row.get(9)?,
                atime_nsec: row.get(10)?,
                mtime_nsec: row.get(11)?,
                ctime_nsec: row.get(12)?,
                data_inline_digest: optional_blob(&row, 13)?,
                storage_kind: row.get(14)?,
            },
        );
    }
    drop(rows);

    let mut rows = conn
        .query(
            "SELECT id, name, parent_ino, ino
             FROM fs_snapshot_dentry WHERE snapshot_id = ? ORDER BY id",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let dentry = ReplayDentry {
            id: row.get(0)?,
            name: row.get(1)?,
            parent_ino: row.get(2)?,
            ino: row.get(3)?,
        };
        state
            .dentries
            .insert((dentry.parent_ino, dentry.name.clone()), dentry);
    }
    drop(rows);

    let mut rows = conn
        .query(
            "SELECT ino, chunk_index, digest
             FROM fs_snapshot_data WHERE snapshot_id = ?",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        state.data.insert((row.get(0)?, row.get(1)?), row.get(2)?);
    }
    drop(rows);

    let mut rows = conn
        .query(
            "SELECT ino, target FROM fs_snapshot_symlink WHERE snapshot_id = ?",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        state.symlinks.insert(row.get(0)?, row.get(1)?);
    }
    drop(rows);

    let mut rows = conn
        .query(
            "SELECT path, parent_path, created_at
             FROM fs_snapshot_whiteout WHERE snapshot_id = ?",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let whiteout = WhiteoutDelta {
            path: row.get(0)?,
            parent_path: row.get(1)?,
            created_at: row.get(2)?,
        };
        state.whiteouts.insert(whiteout.path.clone(), whiteout);
    }
    drop(rows);

    let mut rows = conn
        .query(
            "SELECT delta_ino, base_ino
             FROM fs_snapshot_origin WHERE snapshot_id = ?",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        state.origins.insert(row.get(0)?, row.get(1)?);
    }
    drop(rows);

    let mut rows = conn
        .query(
            "SELECT delta_ino, base_ino, base_path, base_size, base_fingerprint_size,
                    base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec, created_at
             FROM fs_snapshot_partial_origin WHERE snapshot_id = ?",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let partial = PartialOriginDelta {
            delta_ino: row.get(0)?,
            base_ino: row.get(1)?,
            base_path: row.get(2)?,
            base_size: row.get(3)?,
            base_fingerprint_size: row.get(4)?,
            base_mtime: row.get(5)?,
            base_mtime_nsec: row.get(6)?,
            base_ctime: row.get(7)?,
            base_ctime_nsec: row.get(8)?,
            created_at: row.get(9)?,
        };
        state.partial_origins.insert(partial.delta_ino, partial);
    }
    drop(rows);

    let mut rows = conn
        .query(
            "SELECT delta_ino, chunk_index
             FROM fs_snapshot_chunk_override WHERE snapshot_id = ?",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        state.chunk_overrides.insert((row.get(0)?, row.get(1)?));
    }
    drop(rows);

    let mut rows = conn
        .query(
            "SELECT key, value FROM fs_snapshot_meta WHERE snapshot_id = ?",
            (snapshot_id,),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        state.meta.insert(row.get(0)?, row.get(1)?);
    }
    if let Some(parent) = state.meta.get(PARENT_ARTIFACT_KEY) {
        state
            .overlay_config
            .insert(PARENT_ARTIFACT_KEY.to_string(), parent.clone());
    }
    Ok(state)
}

fn apply_delta_to_state(state: &mut ReplayState, delta: &JournalRow) -> Result<()> {
    match (delta.tbl.as_str(), delta.verb.as_str()) {
        ("fs_inode", "upsert") => {
            let row: InodeDelta = serde_json::from_str(&delta.row)?;
            let data_inline_digest = row
                .data_inline_digest
                .as_deref()
                .map(decode_digest)
                .transpose()?;
            state.inodes.insert(
                row.ino,
                ReplayInode {
                    ino: row.ino,
                    mode: row.mode,
                    nlink: row.nlink,
                    uid: row.uid,
                    gid: row.gid,
                    size: row.size,
                    atime: row.atime,
                    mtime: row.mtime,
                    ctime: row.ctime,
                    rdev: row.rdev,
                    atime_nsec: row.atime_nsec,
                    mtime_nsec: row.mtime_nsec,
                    ctime_nsec: row.ctime_nsec,
                    data_inline_digest,
                    storage_kind: row.storage_kind,
                },
            );
        }
        ("fs_inode", "delete") => {
            let row: InodeKey = serde_json::from_str(&delta.row)?;
            state.inodes.remove(&row.ino);
        }
        ("fs_dentry", "upsert") => {
            let row: DentryDelta = serde_json::from_str(&delta.row)?;
            let id = row.id.ok_or_else(|| {
                Error::HistoryIntegrity(format!(
                    "fs_dentry upsert at seq {} is missing id",
                    delta.seq
                ))
            })?;
            state.dentries.retain(|_, dentry| dentry.id != id);
            state.dentries.insert(
                (row.parent_ino, row.name.clone()),
                ReplayDentry {
                    id,
                    parent_ino: row.parent_ino,
                    name: row.name,
                    ino: row.ino,
                },
            );
        }
        ("fs_dentry", "delete") => {
            let row: DentryKey = serde_json::from_str(&delta.row)?;
            state.dentries.remove(&(row.parent_ino, row.name));
        }
        ("fs_data", "upsert") => {
            let row: DataDelta = serde_json::from_str(&delta.row)?;
            state
                .data
                .insert((row.ino, row.chunk_index), decode_digest(&row.digest)?);
        }
        ("fs_data", "delete") => {
            let row: DataKey = serde_json::from_str(&delta.row)?;
            state.data.remove(&(row.ino, row.chunk_index));
        }
        ("fs_symlink", "upsert") => {
            let row: SymlinkDelta = serde_json::from_str(&delta.row)?;
            state.symlinks.insert(row.ino, row.target);
        }
        ("fs_symlink", "delete") => {
            let row: InodeKey = serde_json::from_str(&delta.row)?;
            state.symlinks.remove(&row.ino);
        }
        ("fs_whiteout", "upsert") => {
            let row: WhiteoutDelta = serde_json::from_str(&delta.row)?;
            state.whiteouts.insert(row.path.clone(), row);
        }
        ("fs_whiteout", "delete") => {
            let row: WhiteoutKey = serde_json::from_str(&delta.row)?;
            state.whiteouts.remove(&row.path);
        }
        ("fs_origin", "upsert") => {
            let row: OriginDelta = serde_json::from_str(&delta.row)?;
            state.origins.insert(row.delta_ino, row.base_ino);
        }
        ("fs_origin", "delete") => {
            let row: DeltaInoKey = serde_json::from_str(&delta.row)?;
            state.origins.remove(&row.delta_ino);
        }
        ("fs_partial_origin", "upsert") => {
            let row: PartialOriginDelta = serde_json::from_str(&delta.row)?;
            state.partial_origins.insert(row.delta_ino, row);
        }
        ("fs_partial_origin", "delete") => {
            let row: DeltaInoKey = serde_json::from_str(&delta.row)?;
            state.partial_origins.remove(&row.delta_ino);
        }
        ("fs_chunk_override", "upsert") => {
            let row: ChunkOverrideDelta = serde_json::from_str(&delta.row)?;
            state
                .chunk_overrides
                .insert((row.delta_ino, row.chunk_index));
        }
        ("fs_chunk_override", "delete") => {
            let row: ChunkOverrideDelta = serde_json::from_str(&delta.row)?;
            state
                .chunk_overrides
                .remove(&(row.delta_ino, row.chunk_index));
        }
        ("fs_overlay_config", "upsert") => {
            let row: OverlayConfigDelta = serde_json::from_str(&delta.row)?;
            if row.key != PARENT_ARTIFACT_KEY {
                return Err(Error::HistoryIntegrity(format!(
                    "journal row {} contains non-replayable overlay key {:?}",
                    delta.seq, row.key
                )));
            }
            let value = row.value.ok_or_else(|| {
                Error::HistoryIntegrity(format!(
                    "overlay upsert at seq {} is missing value",
                    delta.seq
                ))
            })?;
            state.overlay_config.insert(row.key.clone(), value.clone());
            state.meta.insert(row.key, value);
        }
        ("fs_overlay_config", "delete") => {
            let row: OverlayConfigDelta = serde_json::from_str(&delta.row)?;
            if row.key != PARENT_ARTIFACT_KEY {
                return Err(Error::HistoryIntegrity(format!(
                    "journal row {} contains non-replayable overlay key {:?}",
                    delta.seq, row.key
                )));
            }
            state.overlay_config.remove(&row.key);
            state.meta.remove(&row.key);
        }
        _ => {
            return Err(Error::HistoryIntegrity(format!(
                "unsupported journal delta {} {} at seq {}",
                delta.tbl, delta.verb, delta.seq
            )));
        }
    }
    Ok(())
}

async fn install_state_as_live(
    conn: &Connection,
    state: &ReplayState,
    chunks: &BTreeMap<Vec<u8>, Vec<u8>>,
) -> Result<()> {
    for table in REPLAY_LIVE_TABLES {
        conn.execute(&format!("DELETE FROM {table}"), ()).await?;
    }
    conn.execute(
        "DELETE FROM fs_overlay_config WHERE key = ?",
        (PARENT_ARTIFACT_KEY,),
    )
    .await?;
    for key in ["seed_pin", "seeded_paths"] {
        conn.execute("DELETE FROM fs_session_metadata WHERE key = ?", (key,))
            .await?;
    }

    let mut inode_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_inode
             (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
              atime_nsec, mtime_nsec, ctime_nsec, data_inline, storage_kind)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .await?;
    for inode in state.inodes.values() {
        let data_inline = match &inode.data_inline_digest {
            Some(digest) if inode.storage_kind == 1 => Value::Blob(
                chunks
                    .get(digest)
                    .expect("materialization verified every inline digest")
                    .clone(),
            ),
            _ => Value::Null,
        };
        inode_stmt
            .execute((
                inode.ino,
                inode.mode,
                inode.nlink,
                inode.uid,
                inode.gid,
                inode.size,
                inode.atime,
                inode.mtime,
                inode.ctime,
                inode.rdev,
                inode.atime_nsec,
                inode.mtime_nsec,
                inode.ctime_nsec,
                data_inline,
                inode.storage_kind,
            ))
            .await?;
    }

    let mut dentry_stmt = conn
        .prepare_cached("INSERT INTO fs_dentry (id, name, parent_ino, ino) VALUES (?, ?, ?, ?)")
        .await?;
    let mut dentries = state.dentries.values().collect::<Vec<_>>();
    dentries.sort_by_key(|dentry| dentry.id);
    for dentry in dentries {
        dentry_stmt
            .execute((
                dentry.id,
                dentry.name.as_str(),
                dentry.parent_ino,
                dentry.ino,
            ))
            .await?;
    }

    let mut data_stmt = conn
        .prepare_cached("INSERT INTO fs_data (ino, chunk_index, digest) VALUES (?, ?, ?)")
        .await?;
    for ((ino, chunk_index), digest) in &state.data {
        data_stmt
            .execute((*ino, *chunk_index, Value::Blob(digest.clone())))
            .await?;
    }
    let mut symlink_stmt = conn
        .prepare_cached("INSERT INTO fs_symlink (ino, target) VALUES (?, ?)")
        .await?;
    for (ino, target) in &state.symlinks {
        symlink_stmt.execute((*ino, target.as_str())).await?;
    }
    let mut whiteout_stmt = conn
        .prepare_cached("INSERT INTO fs_whiteout (path, parent_path, created_at) VALUES (?, ?, ?)")
        .await?;
    for whiteout in state.whiteouts.values() {
        whiteout_stmt
            .execute((
                whiteout.path.as_str(),
                whiteout.parent_path.as_str(),
                whiteout.created_at,
            ))
            .await?;
    }
    let mut origin_stmt = conn
        .prepare_cached("INSERT INTO fs_origin (delta_ino, base_ino) VALUES (?, ?)")
        .await?;
    for (delta_ino, base_ino) in &state.origins {
        origin_stmt.execute((*delta_ino, *base_ino)).await?;
    }
    let mut partial_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_partial_origin
             (delta_ino, base_ino, base_path, base_size, base_fingerprint_size,
              base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .await?;
    for partial in state.partial_origins.values() {
        partial_stmt
            .execute((
                partial.delta_ino,
                partial.base_ino,
                partial.base_path.as_str(),
                partial.base_size,
                partial.base_fingerprint_size,
                partial.base_mtime,
                partial.base_mtime_nsec,
                partial.base_ctime,
                partial.base_ctime_nsec,
                partial.created_at,
            ))
            .await?;
    }
    let mut override_stmt = conn
        .prepare_cached("INSERT INTO fs_chunk_override (delta_ino, chunk_index) VALUES (?, ?)")
        .await?;
    for (delta_ino, chunk_index) in &state.chunk_overrides {
        override_stmt.execute((*delta_ino, *chunk_index)).await?;
    }
    if let Some(parent) = state.overlay_config.get(PARENT_ARTIFACT_KEY) {
        conn.execute(
            "INSERT INTO fs_overlay_config (key, value) VALUES (?, ?)",
            (PARENT_ARTIFACT_KEY, parent.as_str()),
        )
        .await?;
    }
    for key in ["seed_pin", "seeded_paths"] {
        if let Some(value) = state.meta.get(key) {
            conn.execute(
                "INSERT INTO fs_session_metadata (key, value) VALUES (?, ?)",
                (key, value.as_str()),
            )
            .await?;
        }
    }
    Ok(())
}

async fn load_referenced_chunks(
    conn: &Connection,
    state: &ReplayState,
) -> Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    let mut references = BTreeMap::<Vec<u8>, &'static str>::new();
    for digest in state.data.values() {
        references.insert(digest.clone(), "fs_data");
    }
    for inode in state.inodes.values() {
        if let Some(digest) = &inode.data_inline_digest {
            references.insert(digest.clone(), "fs_inode.data_inline_digest");
        }
    }
    let mut chunks = BTreeMap::new();
    let mut rows = conn.query("SELECT digest, data FROM fs_chunk", ()).await?;
    while let Some(row) = rows.next().await? {
        let digest: Vec<u8> = row.get(0)?;
        if references.remove(&digest).is_some() {
            chunks.insert(digest, row.get(1)?);
        }
    }
    if let Some((digest, referenced_by)) = references.into_iter().next() {
        return Err(Error::HistoryMissingChunk {
            digest: hex_digest(&digest),
            referenced_by: referenced_by.to_string(),
        });
    }
    Ok(chunks)
}

fn optional_blob(row: &turso::Row, column: usize) -> Result<Option<Vec<u8>>> {
    match row.get_value(column)? {
        Value::Blob(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        value => Err(Error::HistoryIntegrity(format!(
            "expected optional blob, found {value:?}"
        ))),
    }
}

async fn verify_reconstructed_digests(conn: &Connection) -> Result<()> {
    let mut rows = conn
        .query(
            "SELECT hex(d.digest)
             FROM fs_data d
             LEFT JOIN fs_chunk c ON c.digest = d.digest
             WHERE c.digest IS NULL LIMIT 1",
            (),
        )
        .await?;
    if let Some(row) = rows.next().await? {
        return Err(Error::HistoryMissingChunk {
            digest: row.get::<String>(0)?.to_ascii_lowercase(),
            referenced_by: "fs_data".to_string(),
        });
    }
    Ok(())
}

async fn trim_future(conn: &Connection, target_seq: i64, epoch: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM fs_journal_chunk
         WHERE seq IN (SELECT seq FROM fs_op_journal WHERE seq > ?)",
        (target_seq,),
    )
    .await?;
    conn.execute("DELETE FROM fs_op_journal WHERE seq > ?", (target_seq,))
        .await?;
    delete_snapshots_after(conn, epoch, target_seq).await?;
    Ok(())
}

async fn recompute_chunk_refcounts(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE fs_chunk
         SET refcount = (SELECT COUNT(*) FROM fs_data d WHERE d.digest = fs_chunk.digest)",
        (),
    )
    .await?;
    Ok(())
}

async fn max_known_inode(conn: &Connection) -> Result<i64> {
    let max_live_inode =
        query_scalar_i64(conn, "SELECT COALESCE(MAX(ino), 0) FROM fs_inode", ()).await?;
    let max_snapshot_inode = query_scalar_i64(
        conn,
        "SELECT COALESCE(MAX(ino), 0) FROM fs_snapshot_inode",
        (),
    )
    .await?;
    let max_journal_inode = max_journal_integer_field(conn, "fs_inode", "ino").await?;
    Ok(max_live_inode
        .max(max_snapshot_inode)
        .max(max_journal_inode))
}

async fn verify_inode_allocator(conn: &Connection, max_inode_ever: i64) -> Result<()> {
    let mut rows = conn
        .query(
            "SELECT seq FROM sqlite_sequence WHERE name = 'fs_inode'",
            (),
        )
        .await?;
    let allocated_through = rows
        .next()
        .await?
        .map(|row| row.get(0))
        .transpose()?
        .unwrap_or(0);
    if allocated_through < max_inode_ever {
        return Err(Error::HistoryIntegrity(format!(
            "inode allocator is at {allocated_through}, below retained-history inode {max_inode_ever}"
        )));
    }
    Ok(())
}

async fn max_journal_integer_field(conn: &Connection, table: &str, field: &str) -> Result<i64> {
    let mut rows = conn
        .query(
            "SELECT row FROM fs_op_journal WHERE tbl = ? ORDER BY seq",
            (table,),
        )
        .await?;
    let mut maximum = 0;
    while let Some(row) = rows.next().await? {
        let payload: String = row.get(0)?;
        let value: serde_json::Value = serde_json::from_str(&payload)?;
        if let Some(number) = value.get(field).and_then(serde_json::Value::as_i64) {
            maximum = maximum.max(number);
        }
    }
    Ok(maximum)
}

async fn visible_tree_sanity(conn: &Connection) -> Result<()> {
    let mut inode_rows = conn
        .query("SELECT ino, mode, nlink FROM fs_inode", ())
        .await?;
    let mut modes = BTreeMap::new();
    let mut linked = BTreeSet::new();
    while let Some(row) = inode_rows.next().await? {
        let ino: i64 = row.get(0)?;
        modes.insert(ino, row.get::<i64>(1)?);
        if row.get::<i64>(2)? > 0 {
            linked.insert(ino);
        }
    }
    if !modes.contains_key(&1) {
        return Err(Error::HistoryIntegrity(
            "visible tree is missing root inode 1".to_string(),
        ));
    }

    let mut rows = conn
        .query(
            "SELECT parent_ino, name, ino FROM fs_dentry ORDER BY parent_ino, name",
            (),
        )
        .await?;
    let mut children: BTreeMap<i64, Vec<(String, i64)>> = BTreeMap::new();
    while let Some(row) = rows.next().await? {
        children
            .entry(row.get(0)?)
            .or_default()
            .push((row.get(1)?, row.get(2)?));
    }

    let mut queue = VecDeque::from([1_i64]);
    let mut visited_dirs = BTreeSet::new();
    let mut reachable = BTreeSet::from([1_i64]);
    while let Some(parent) = queue.pop_front() {
        if !visited_dirs.insert(parent) {
            return Err(Error::HistoryIntegrity(format!(
                "visible tree contains a directory cycle at inode {parent}"
            )));
        }
        for (_, child) in children.get(&parent).into_iter().flatten() {
            reachable.insert(*child);
            let mode = modes.get(child).ok_or_else(|| {
                Error::HistoryIntegrity(format!(
                    "visible tree dentry references missing inode {child}"
                ))
            })?;
            if mode & 0o170000 == 0o040000 {
                queue.push_back(*child);
            }
        }
    }
    if let Some(ino) = linked.difference(&reachable).next() {
        return Err(Error::HistoryIntegrity(format!(
            "linked inode {ino} is unreachable from root"
        )));
    }
    Ok(())
}

async fn capture_state_root(
    conn: &Connection,
    state: &ReplayState,
    reason: &str,
    epoch: i64,
    through_seq: i64,
) -> Result<i64> {
    let created_at_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis(),
    )
    .map_err(|_| Error::Internal("snapshot timestamp overflow".to_string()))?;
    conn.execute(
        "INSERT INTO fs_snapshot
         (through_seq, created_at_ms, reason, history_epoch)
         VALUES (?, ?, ?, ?)",
        (through_seq, created_at_ms, reason, epoch),
    )
    .await?;
    let snapshot_id = conn.last_insert_rowid();
    let mut inode_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_snapshot_inode
             (snapshot_id, ino, mode, nlink, uid, gid, size, atime, mtime, ctime,
              rdev, atime_nsec, mtime_nsec, ctime_nsec, data_inline_digest, storage_kind)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .await?;
    for inode in state.inodes.values() {
        inode_stmt
            .execute((
                snapshot_id,
                inode.ino,
                inode.mode,
                inode.nlink,
                inode.uid,
                inode.gid,
                inode.size,
                inode.atime,
                inode.mtime,
                inode.ctime,
                inode.rdev,
                inode.atime_nsec,
                inode.mtime_nsec,
                inode.ctime_nsec,
                inode
                    .data_inline_digest
                    .clone()
                    .map(Value::Blob)
                    .unwrap_or(Value::Null),
                inode.storage_kind,
            ))
            .await?;
    }
    let mut dentry_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_snapshot_dentry
             (snapshot_id, id, name, parent_ino, ino) VALUES (?, ?, ?, ?, ?)",
        )
        .await?;
    for dentry in state.dentries.values() {
        dentry_stmt
            .execute((
                snapshot_id,
                dentry.id,
                dentry.name.as_str(),
                dentry.parent_ino,
                dentry.ino,
            ))
            .await?;
    }
    let mut data_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_snapshot_data
             (snapshot_id, ino, chunk_index, digest) VALUES (?, ?, ?, ?)",
        )
        .await?;
    for ((ino, chunk_index), digest) in &state.data {
        data_stmt
            .execute((snapshot_id, *ino, *chunk_index, Value::Blob(digest.clone())))
            .await?;
    }
    let mut symlink_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_snapshot_symlink (snapshot_id, ino, target) VALUES (?, ?, ?)",
        )
        .await?;
    for (ino, target) in &state.symlinks {
        symlink_stmt
            .execute((snapshot_id, *ino, target.as_str()))
            .await?;
    }
    let mut whiteout_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_snapshot_whiteout
             (snapshot_id, path, parent_path, created_at) VALUES (?, ?, ?, ?)",
        )
        .await?;
    for whiteout in state.whiteouts.values() {
        whiteout_stmt
            .execute((
                snapshot_id,
                whiteout.path.as_str(),
                whiteout.parent_path.as_str(),
                whiteout.created_at,
            ))
            .await?;
    }
    let mut origin_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_snapshot_origin
             (snapshot_id, delta_ino, base_ino) VALUES (?, ?, ?)",
        )
        .await?;
    for (delta_ino, base_ino) in &state.origins {
        origin_stmt
            .execute((snapshot_id, *delta_ino, *base_ino))
            .await?;
    }
    let mut partial_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_snapshot_partial_origin
             (snapshot_id, delta_ino, base_ino, base_path, base_size,
              base_fingerprint_size, base_mtime, base_mtime_nsec,
              base_ctime, base_ctime_nsec, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .await?;
    for partial in state.partial_origins.values() {
        partial_stmt
            .execute((
                snapshot_id,
                partial.delta_ino,
                partial.base_ino,
                partial.base_path.as_str(),
                partial.base_size,
                partial.base_fingerprint_size,
                partial.base_mtime,
                partial.base_mtime_nsec,
                partial.base_ctime,
                partial.base_ctime_nsec,
                partial.created_at,
            ))
            .await?;
    }
    let mut override_stmt = conn
        .prepare_cached(
            "INSERT INTO fs_snapshot_chunk_override
             (snapshot_id, delta_ino, chunk_index) VALUES (?, ?, ?)",
        )
        .await?;
    for (delta_ino, chunk_index) in &state.chunk_overrides {
        override_stmt
            .execute((snapshot_id, *delta_ino, *chunk_index))
            .await?;
    }
    let mut meta_stmt = conn
        .prepare_cached("INSERT INTO fs_snapshot_meta (snapshot_id, key, value) VALUES (?, ?, ?)")
        .await?;
    for (key, value) in &state.meta {
        meta_stmt
            .execute((snapshot_id, key.as_str(), value.as_str()))
            .await?;
    }
    let mut pins = state.data.values().cloned().collect::<BTreeSet<_>>();
    pins.extend(
        state
            .inodes
            .values()
            .filter_map(|inode| inode.data_inline_digest.clone()),
    );
    let mut pin_stmt = conn
        .prepare_cached("INSERT INTO fs_snapshot_chunk (snapshot_id, digest) VALUES (?, ?)")
        .await?;
    for digest in pins {
        pin_stmt.execute((snapshot_id, Value::Blob(digest))).await?;
    }
    Ok(snapshot_id)
}

async fn read_markers(conn: &Connection) -> Result<Markers> {
    let epoch = config_i64(conn, CONFIG_HISTORY_EPOCH_KEY).await?;
    let valid = config_i64(conn, CONFIG_HISTORY_VALID_KEY).await?;
    if !matches!(valid, 0 | 1) {
        return Err(Error::HistoryIntegrity(format!(
            "invalid history_valid marker {valid}"
        )));
    }
    let floor = config_i64(conn, CONFIG_HISTORY_FLOOR_SEQ_KEY).await?;
    let mut rows = conn
        .query(
            "SELECT seq FROM fs_op_journal ORDER BY seq DESC LIMIT 1",
            (),
        )
        .await?;
    let journal_head = rows.next().await?.map(|row| row.get(0)).transpose()?;
    drop(rows);
    let mut rows = conn
        .query(
            "SELECT through_seq
             FROM fs_snapshot
             WHERE history_epoch = ?
             ORDER BY through_seq DESC
             LIMIT 1",
            (epoch,),
        )
        .await?;
    let snapshot_head = rows.next().await?.map(|row| row.get(0)).transpose()?;
    let head = journal_head
        .unwrap_or(floor)
        .max(snapshot_head.unwrap_or(floor))
        .max(floor);
    Ok(Markers {
        epoch,
        valid: valid == 1,
        floor,
        head,
    })
}

async fn config_i64(conn: &Connection, key: &str) -> Result<i64> {
    let mut rows = conn
        .query("SELECT value FROM fs_config WHERE key = ?", (key,))
        .await?;
    let value: String = rows
        .next()
        .await?
        .ok_or_else(|| Error::HistoryIntegrity(format!("missing history marker {key}")))?
        .get(0)?;
    value
        .parse()
        .map_err(|error| Error::HistoryIntegrity(format!("invalid {key}={value:?}: {error}")))
}

async fn set_config_i64(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO fs_config (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value.to_string()),
    )
    .await?;
    Ok(())
}

async fn snapshot_header(conn: &Connection, snapshot_id: i64) -> Result<SnapshotHeader> {
    let mut rows = conn
        .query(
            "SELECT snapshot_id, through_seq, created_at_ms, reason, history_epoch
             FROM fs_snapshot WHERE snapshot_id = ?",
            (snapshot_id,),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| Error::HistoryIntegrity(format!("missing snapshot {snapshot_id}")))?;
    Ok(SnapshotHeader {
        snapshot_id: row.get(0)?,
        through_seq: row.get(1)?,
        created_at_ms: row.get(2)?,
        reason: row.get(3)?,
        history_epoch: row.get(4)?,
    })
}

async fn snapshot_at(
    conn: &Connection,
    epoch: i64,
    through_seq: i64,
) -> Result<Option<SnapshotHeader>> {
    let mut rows = conn
        .query(
            "SELECT snapshot_id FROM fs_snapshot
             WHERE history_epoch = ? AND through_seq = ?",
            (epoch, through_seq),
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(snapshot_header(conn, row.get(0)?).await?)),
        None => Ok(None),
    }
}

async fn nearest_snapshot(
    conn: &Connection,
    epoch: i64,
    target_seq: i64,
) -> Result<Option<SnapshotHeader>> {
    let mut rows = conn
        .query(
            "SELECT snapshot_id
             FROM fs_snapshot
             WHERE history_epoch = ? AND through_seq <= ?
             ORDER BY through_seq DESC
             LIMIT 1",
            (epoch, target_seq),
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(Some(snapshot_header(conn, row.get(0)?).await?)),
        None => Ok(None),
    }
}

async fn delete_journal_through(conn: &Connection, through_seq: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM fs_journal_chunk
         WHERE seq IN (SELECT seq FROM fs_op_journal WHERE seq <= ?)",
        (through_seq,),
    )
    .await?;
    conn.execute("DELETE FROM fs_op_journal WHERE seq <= ?", (through_seq,))
        .await?;
    Ok(())
}

async fn delete_all_snapshots(conn: &Connection) -> Result<()> {
    for table in SNAPSHOT_CHILD_TABLES {
        conn.execute(&format!("DELETE FROM {table}"), ()).await?;
    }
    conn.execute("DELETE FROM fs_snapshot", ()).await?;
    Ok(())
}

async fn delete_snapshot_at(conn: &Connection, epoch: i64, through_seq: i64) -> Result<()> {
    delete_snapshots_matching(
        conn,
        "history_epoch = ? AND through_seq = ?",
        vec![Value::Integer(epoch), Value::Integer(through_seq)],
    )
    .await
}

async fn delete_snapshots_before(conn: &Connection, epoch: i64, through_seq: i64) -> Result<()> {
    delete_snapshots_matching(
        conn,
        "history_epoch != ? OR through_seq < ?",
        vec![Value::Integer(epoch), Value::Integer(through_seq)],
    )
    .await
}

async fn delete_snapshots_after(conn: &Connection, epoch: i64, through_seq: i64) -> Result<()> {
    delete_snapshots_matching(
        conn,
        "history_epoch != ? OR through_seq > ?",
        vec![Value::Integer(epoch), Value::Integer(through_seq)],
    )
    .await
}

async fn delete_snapshots_except(conn: &Connection, epoch: i64, snapshot_id: i64) -> Result<()> {
    delete_snapshots_matching(
        conn,
        "history_epoch != ? OR snapshot_id != ?",
        vec![Value::Integer(epoch), Value::Integer(snapshot_id)],
    )
    .await
}

async fn delete_snapshots_matching(
    conn: &Connection,
    predicate: &str,
    params: Vec<Value>,
) -> Result<()> {
    for table in SNAPSHOT_CHILD_TABLES {
        conn.execute(
            &format!(
                "DELETE FROM {table}
                 WHERE snapshot_id IN (SELECT snapshot_id FROM fs_snapshot WHERE {predicate})"
            ),
            params.clone(),
        )
        .await?;
    }
    conn.execute(
        &format!("DELETE FROM fs_snapshot WHERE {predicate}"),
        params,
    )
    .await?;
    Ok(())
}

async fn collect_unpinned_chunks(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM fs_chunk
         WHERE refcount = 0
           AND digest NOT IN (SELECT digest FROM fs_journal_chunk)
           AND digest NOT IN (SELECT digest FROM fs_snapshot_chunk)",
        (),
    )
    .await?;
    Ok(())
}

async fn query_scalar_i64<P>(conn: &Connection, sql: &str, params: P) -> Result<i64>
where
    P: turso::params::IntoParams,
{
    let mut rows = conn.query(sql, params).await?;
    rows.next()
        .await?
        .ok_or_else(|| Error::Internal("scalar query returned no row".to_string()))?
        .get(0)
        .map_err(Error::from)
}

fn decode_digest(hex: &str) -> Result<Vec<u8>> {
    if hex.len() != 64 {
        return Err(Error::HistoryIntegrity(format!(
            "invalid digest length in history row: {hex:?}"
        )));
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(32);
    for [high, low] in bytes.as_chunks::<2>().0 {
        let high = decode_nibble(*high)?;
        let low = decode_nibble(*low)?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_digest(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(Error::HistoryIntegrity(format!(
            "invalid lowercase hex digit {:?} in history digest",
            byte as char
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::vfs::{JournalDelta, MutationTxn, PartialOriginRow};
    use crate::fs::{FileSystem, DEFAULT_FILE_MODE};
    use crate::{Vfs, VfsOptions};
    use std::fs;
    use turso::Builder;

    const COMPARED_TABLES: &[(&str, &str, usize)] = &[
        (
            "fs_inode",
            "SELECT ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
                    atime_nsec, mtime_nsec, ctime_nsec, data_inline, storage_kind
             FROM fs_inode ORDER BY ino",
            15,
        ),
        (
            "fs_dentry",
            "SELECT id, name, parent_ino, ino FROM fs_dentry ORDER BY id",
            4,
        ),
        (
            "fs_data",
            "SELECT ino, chunk_index, digest FROM fs_data ORDER BY ino, chunk_index",
            3,
        ),
        (
            "fs_symlink",
            "SELECT ino, target FROM fs_symlink ORDER BY ino",
            2,
        ),
        (
            "fs_whiteout",
            "SELECT path, parent_path, created_at FROM fs_whiteout ORDER BY path",
            3,
        ),
        (
            "fs_origin",
            "SELECT delta_ino, base_ino FROM fs_origin ORDER BY delta_ino",
            2,
        ),
        (
            "fs_partial_origin",
            "SELECT delta_ino, base_ino, base_path, base_size, base_fingerprint_size,
                    base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec, created_at
             FROM fs_partial_origin ORDER BY delta_ino",
            10,
        ),
        (
            "fs_chunk_override",
            "SELECT delta_ino, chunk_index FROM fs_chunk_override
             ORDER BY delta_ino, chunk_index",
            2,
        ),
        (
            "fs_overlay_config",
            "SELECT key, value FROM fs_overlay_config ORDER BY key",
            2,
        ),
        (
            "fs_chunk",
            "SELECT digest, data, refcount FROM fs_chunk ORDER BY digest",
            3,
        ),
    ];

    async fn table_dump(path: &Path, sql: &str, columns: usize) -> Result<Vec<Vec<String>>> {
        let db = Builder::new_local(path.to_str().unwrap()).build().await?;
        let conn = db.connect()?;
        let mut rows = conn.query(sql, ()).await?;
        let mut dump = Vec::new();
        while let Some(row) = rows.next().await? {
            dump.push(
                (0..columns)
                    .map(|column| format!("{:?}", row.get_value(column)))
                    .collect(),
            );
        }
        Ok(dump)
    }

    async fn assert_filesystem_tables_equal(expected: &Path, actual: &Path) -> Result<()> {
        for (table, sql, columns) in COMPARED_TABLES {
            assert_eq!(
                table_dump(expected, sql, *columns).await?,
                table_dump(actual, sql, *columns).await?,
                "table {table} differs"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn reconstruct_matches_intermediate_database_exactly() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source_path = temp.path().join("source.db");
        let expected_path = temp.path().join("expected.db");
        let replay_path = temp.path().join("replay.db");

        let vfs = Vfs::open(VfsOptions::with_path(source_path.to_string_lossy())).await?;
        let (stats, file) =
            FileSystem::create_file(&vfs.fs, 1, "data", DEFAULT_FILE_MODE, 1000, 1000).await?;
        file.pwrite(0, b"inline target").await?;
        FileSystem::link(&vfs.fs, stats.ino, 1, "hardlink").await?;

        let chunk_size = vfs.fs.chunk_size();
        file.pwrite((chunk_size * 2 + 17) as u64, &vec![0x5a; chunk_size + 31])
            .await?;
        let (dense_stats, dense) =
            FileSystem::create_file(&vfs.fs, 1, "dense", DEFAULT_FILE_MODE, 1000, 1000).await?;
        dense.pwrite(0, &vec![0x33; chunk_size]).await?;
        dense.truncate(4).await?;
        let (_, doomed) =
            FileSystem::create_file(&vfs.fs, 1, "doomed", DEFAULT_FILE_MODE, 1000, 1000).await?;
        doomed.pwrite(0, b"reaped").await?;
        drop(doomed);
        FileSystem::unlink(&vfs.fs, 1, "doomed").await?;
        let (_, rename_source) =
            FileSystem::create_file(&vfs.fs, 1, "rename-src", DEFAULT_FILE_MODE, 1000, 1000)
                .await?;
        rename_source.pwrite(0, b"source").await?;
        let (_, rename_destination) =
            FileSystem::create_file(&vfs.fs, 1, "rename-dst", DEFAULT_FILE_MODE, 1000, 1000)
                .await?;
        rename_destination.pwrite(0, b"destination").await?;
        drop(rename_source);
        drop(rename_destination);
        FileSystem::rename(&vfs.fs, 1, "rename-src", 1, "rename-dst").await?;
        vfs.fs.drain_all().await?;
        let conn = vfs.get_connection().await?;
        assert_eq!(
            query_scalar_i64(
                &conn,
                "SELECT storage_kind FROM fs_inode WHERE ino = ?",
                (dense_stats.ino,),
            )
            .await?,
            1,
            "dense chunked file must transition back to inline"
        );
        drop(conn);
        let target = vfs.history_status().await?.head_seq;
        vfs.snapshot_into(&expected_path).await?;

        FileSystem::unlink(&vfs.fs, 1, "hardlink").await?;
        file.truncate(4).await?;
        file.pwrite(0, b"tiny").await?;
        let (_, replacement) =
            FileSystem::create_file(&vfs.fs, 1, "replacement", DEFAULT_FILE_MODE, 1000, 1000)
                .await?;
        replacement.pwrite(0, b"future").await?;
        FileSystem::rename(&vfs.fs, 1, "replacement", 1, "data").await?;
        vfs.fs.finalize().await?;
        drop(vfs);

        fs::copy(&source_path, &replay_path)?;
        Vfs::reconstruct_to(&replay_path, target).await?;
        assert_filesystem_tables_equal(&expected_path, &replay_path).await
    }

    #[tokio::test]
    async fn reconstruct_restores_overlay_sidecars_and_parent_config() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source_path = temp.path().join("overlay-source.db");
        let expected_path = temp.path().join("overlay-expected.db");
        let replay_path = temp.path().join("overlay-replay.db");

        let vfs = Vfs::open(VfsOptions::with_path(source_path.to_string_lossy())).await?;
        let (stats, file) =
            FileSystem::create_file(&vfs.fs, 1, "partial", DEFAULT_FILE_MODE, 1000, 1000).await?;
        file.pwrite(vfs.fs.chunk_size() as u64, b"partial").await?;
        vfs.fs.drain_all().await?;

        let conn = vfs.get_connection().await?;
        let mut txn = MutationTxn::begin(&conn, vfs.fs.journal_ctx()).await?;
        txn.conn()
            .execute(
                "INSERT INTO fs_whiteout (path, parent_path, created_at)
                 VALUES ('/hidden', '/', 7)",
                (),
            )
            .await?;
        txn.record(JournalDelta::whiteout_upsert(
            "test_overlay",
            "/hidden",
            "/",
            7,
        ));
        txn.conn()
            .execute(
                "INSERT INTO fs_origin (delta_ino, base_ino) VALUES (?, 42)",
                (stats.ino,),
            )
            .await?;
        txn.record(JournalDelta::origin_upsert("test_overlay", stats.ino, 42));
        let partial = PartialOriginRow {
            delta_ino: stats.ino,
            base_ino: 42,
            base_path: "/partial".to_string(),
            base_size: stats.size.max(vfs.fs.chunk_size() as i64 + 7),
            base_fingerprint_size: 0,
            base_mtime: 1,
            base_mtime_nsec: 2,
            base_ctime: 3,
            base_ctime_nsec: 4,
            created_at: 5,
        };
        txn.conn()
            .execute(
                "INSERT INTO fs_partial_origin
                 (delta_ino, base_ino, base_path, base_size, base_fingerprint_size,
                  base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    partial.delta_ino,
                    partial.base_ino,
                    partial.base_path.as_str(),
                    partial.base_size,
                    partial.base_fingerprint_size,
                    partial.base_mtime,
                    partial.base_mtime_nsec,
                    partial.base_ctime,
                    partial.base_ctime_nsec,
                    partial.created_at,
                ),
            )
            .await?;
        txn.record(JournalDelta::partial_origin_upsert(
            "test_overlay",
            &partial,
        ));
        txn.conn()
            .execute(
                "INSERT INTO fs_chunk_override (delta_ino, chunk_index) VALUES (?, 0)",
                (stats.ino,),
            )
            .await?;
        txn.record(JournalDelta::chunk_override_upsert(
            "test_overlay",
            stats.ino,
            0,
        ));
        txn.commit().await?;
        drop(conn);
        vfs.set_overlay_parent_artifact(&"ab".repeat(32)).await?;

        let target = vfs.history_status().await?.head_seq;
        vfs.snapshot_into(&expected_path).await?;

        let conn = vfs.get_connection().await?;
        let mut txn = MutationTxn::begin(&conn, vfs.fs.journal_ctx()).await?;
        txn.conn()
            .execute("DELETE FROM fs_whiteout WHERE path = '/hidden'", ())
            .await?;
        txn.record(JournalDelta::whiteout_delete(
            "test_overlay_cleanup",
            "/hidden",
        ));
        txn.conn()
            .execute(
                "DELETE FROM fs_chunk_override WHERE delta_ino = ?",
                (stats.ino,),
            )
            .await?;
        txn.record(JournalDelta::chunk_override_delete(
            "test_overlay_cleanup",
            stats.ino,
            0,
        ));
        txn.conn()
            .execute(
                "DELETE FROM fs_partial_origin WHERE delta_ino = ?",
                (stats.ino,),
            )
            .await?;
        txn.record(JournalDelta::partial_origin_delete(
            "test_overlay_cleanup",
            stats.ino,
        ));
        txn.conn()
            .execute("DELETE FROM fs_origin WHERE delta_ino = ?", (stats.ino,))
            .await?;
        txn.record(JournalDelta::origin_delete(
            "test_overlay_cleanup",
            stats.ino,
        ));
        txn.commit().await?;
        drop(conn);
        vfs.clear_overlay_parent_artifact().await?;
        vfs.fs.finalize().await?;
        drop(vfs);

        fs::copy(&source_path, &replay_path)?;
        Vfs::reconstruct_to(&replay_path, target).await?;
        assert_filesystem_tables_equal(&expected_path, &replay_path).await
    }

    #[tokio::test]
    async fn rejects_mid_transaction_and_out_of_range_targets() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("targets.db");
        let vfs = Vfs::open(VfsOptions::with_path(path.to_string_lossy())).await?;
        FileSystem::create_file(&vfs.fs, 1, "target", DEFAULT_FILE_MODE, 1000, 1000).await?;
        let status = vfs.history_status().await?;
        let head = status.head_seq;
        let mut rows = vfs
            .get_connection()
            .await?
            .query("SELECT txn_id FROM fs_op_journal WHERE seq = ?", (head,))
            .await?;
        let txn_id: i64 = rows.next().await?.unwrap().get(0)?;
        if txn_id < head {
            assert!(matches!(
                vfs.validate_target(txn_id).await,
                Err(Error::HistoryTargetMidTransaction { .. })
            ));
        }
        assert!(matches!(
            vfs.validate_target(head + 1).await,
            Err(Error::HistoryTargetOutOfRange { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn capture_root_records_the_drained_head_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("capture.db");
        let vfs = Vfs::open(VfsOptions::with_path(path.to_string_lossy())).await?;
        let (_, file) =
            FileSystem::create_file(&vfs.fs, 1, "captured", DEFAULT_FILE_MODE, 1000, 1000).await?;
        file.pwrite(0, b"captured bytes").await?;
        let root = vfs.capture_root("test").await?;
        assert_eq!(root.through_seq, vfs.history_status().await?.head_seq);
        assert_eq!(
            vfs.capture_root("duplicate").await?.snapshot_id,
            root.snapshot_id
        );
        Ok(())
    }

    #[tokio::test]
    async fn kill_switch_invalidates_and_reenabling_starts_new_epoch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("epoch.db");
        let vfs = Vfs::open(VfsOptions::with_path(path.to_string_lossy())).await?;
        FileSystem::create_file(&vfs.fs, 1, "before-gap", DEFAULT_FILE_MODE, 1000, 1000).await?;
        let old_epoch = vfs.history_status().await?.epoch;
        vfs.fs.finalize().await?;
        drop(vfs);

        let disabled = crate::CoreConfig {
            journal_enabled: false,
            ..crate::CoreConfig::default()
        };
        let vfs =
            Vfs::open(VfsOptions::with_path(path.to_string_lossy()).with_core_config(disabled))
                .await?;
        FileSystem::create_file(&vfs.fs, 1, "gap", DEFAULT_FILE_MODE, 1000, 1000).await?;
        let invalid = vfs.history_status().await?;
        assert!(!invalid.valid);
        assert!(matches!(
            vfs.validate_target(invalid.floor_seq).await,
            Err(Error::HistoryInvalid { .. })
        ));
        vfs.fs.finalize().await?;
        drop(vfs);

        let readonly = Vfs::open_read_only(&path).await?;
        assert!(!readonly.history_status().await?.valid);
        drop(readonly);

        let vfs = Vfs::open(VfsOptions::with_path(path.to_string_lossy())).await?;
        let revalidated = vfs.history_status().await?;
        assert!(revalidated.valid);
        assert_eq!(revalidated.epoch, old_epoch + 1);
        assert_eq!(revalidated.floor_seq, revalidated.head_seq);
        assert_eq!(revalidated.targets.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn gc_keeps_every_advertised_target_reconstructible() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("gc.db");
        let post_gc = temp.path().join("post-gc.db");
        let config = crate::CoreConfig {
            journal_retention_ops: 8,
            ..crate::CoreConfig::default()
        };
        let vfs = Vfs::open(VfsOptions::with_path(path.to_string_lossy()).with_core_config(config))
            .await?;
        for index in 0..8 {
            let (_, file) = FileSystem::create_file(
                &vfs.fs,
                1,
                &format!("file-{index}"),
                DEFAULT_FILE_MODE,
                1000,
                1000,
            )
            .await?;
            file.pwrite(0, format!("payload-{index}").as_bytes())
                .await?;
        }
        vfs.fs.drain_all().await?;
        let before = vfs.history_status().await?;
        vfs.collect_journal().await?;
        let after = vfs.history_status().await?;
        assert!(after.floor_seq > before.floor_seq);
        assert!(after
            .targets
            .iter()
            .all(|target| target.seq >= after.floor_seq));
        vfs.snapshot_into(&post_gc).await?;
        drop(vfs);

        for target in after.targets {
            let candidate = temp.path().join(format!("gc-target-{}.db", target.seq));
            fs::copy(&post_gc, &candidate)?;
            Vfs::reconstruct_to(&candidate, target.seq).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn reconstruction_preserves_inode_high_water_and_exact_refcounts() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("allocator-source.db");
        let replay = temp.path().join("allocator-replay.db");
        let vfs = Vfs::open(VfsOptions::with_path(source.to_string_lossy())).await?;
        FileSystem::create_file(&vfs.fs, 1, "target", DEFAULT_FILE_MODE, 1000, 1000).await?;
        let target = vfs.history_status().await?.head_seq;
        for index in 0..5 {
            FileSystem::create_file(
                &vfs.fs,
                1,
                &format!("future-{index}"),
                DEFAULT_FILE_MODE,
                1000,
                1000,
            )
            .await?;
        }
        let conn = vfs.get_connection().await?;
        let max_ever =
            query_scalar_i64(&conn, "SELECT COALESCE(MAX(ino), 0) FROM fs_inode", ()).await?;
        drop(conn);
        vfs.fs.finalize().await?;
        drop(vfs);

        fs::copy(&source, &replay)?;
        Vfs::reconstruct_to(&replay, target).await?;
        let vfs = Vfs::open(VfsOptions::with_path(replay.to_string_lossy())).await?;
        let (created, _) =
            FileSystem::create_file(&vfs.fs, 1, "after-replay", DEFAULT_FILE_MODE, 1000, 1000)
                .await?;
        assert!(created.ino > max_ever);
        let conn = vfs.get_connection().await?;
        let mismatches = query_scalar_i64(
            &conn,
            "SELECT COUNT(*) FROM fs_chunk c
             WHERE c.refcount != (
                 SELECT COUNT(*) FROM fs_data d WHERE d.digest = c.digest
             )",
            (),
        )
        .await?;
        assert_eq!(mismatches, 0);
        Ok(())
    }
}
