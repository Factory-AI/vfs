use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

use crate::error::{Error, Result};
use crate::fs::FsError;

use super::store::{DataDelta, StorageChanges};

/// Rows per multi-row journal insert; two shapes (full batch and remainder)
/// keep prepared-statement variety low while bounding SQL length.
const JOURNAL_INSERT_ROWS: usize = 64;
const JOURNAL_PIN_INSERT_ROWS: usize = 128;

/// Shared journal context: the kill-switch state plus an optimistic hint for
/// the next journal seq.
///
/// The hint spares each commit the tip lookup that dominated the journal's
/// per-commit fixed cost. `0` means unknown (fresh open, or invalidated by a
/// failed commit); a commit that finds its hint disagreeing with the seq
/// AUTOINCREMENT actually assigned patches its own rows, so the txn_id ==
/// first-seq contract holds regardless of hint staleness.
#[derive(Clone)]
pub(crate) struct JournalCtx {
    enabled: bool,
    next_seq: Arc<AtomicI64>,
}

impl JournalCtx {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            next_seq: Arc::new(AtomicI64::new(0)),
        }
    }
}

#[derive(Debug)]
pub(crate) struct JournalDelta {
    label: &'static str,
    tbl: &'static str,
    verb: &'static str,
    row: JsonValue,
    digests: Vec<Vec<u8>>,
}

impl JournalDelta {
    fn new(
        label: &'static str,
        tbl: &'static str,
        verb: &'static str,
        row: impl Serialize,
    ) -> Self {
        Self {
            label,
            tbl,
            verb,
            row: serde_json::to_value(row).expect("journal row serialization cannot fail"),
            digests: Vec::new(),
        }
    }

    fn with_digests(mut self, digests: Vec<Vec<u8>>) -> Self {
        self.digests = digests;
        self
    }

    pub(crate) fn inode_delete(label: &'static str, ino: i64) -> Self {
        Self::new(label, "fs_inode", "delete", json!({ "ino": ino }))
    }

    pub(crate) fn dentry_upsert(
        label: &'static str,
        parent_ino: i64,
        name: &str,
        ino: i64,
    ) -> Self {
        Self::new(
            label,
            "fs_dentry",
            "upsert",
            json!({ "parent_ino": parent_ino, "name": name, "ino": ino }),
        )
    }

    pub(crate) fn dentry_delete(label: &'static str, parent_ino: i64, name: &str) -> Self {
        Self::new(
            label,
            "fs_dentry",
            "delete",
            json!({ "parent_ino": parent_ino, "name": name }),
        )
    }

    pub(crate) fn data_upsert(
        label: &'static str,
        ino: i64,
        chunk_index: i64,
        digest: Vec<u8>,
    ) -> Self {
        let digest_hex = hex_digest(&digest);
        Self::new(
            label,
            "fs_data",
            "upsert",
            json!({ "ino": ino, "chunk_index": chunk_index, "digest": digest_hex }),
        )
        .with_digests(vec![digest])
    }

    pub(crate) fn data_delete(label: &'static str, ino: i64, chunk_index: i64) -> Self {
        Self::new(
            label,
            "fs_data",
            "delete",
            json!({ "ino": ino, "chunk_index": chunk_index }),
        )
    }

    pub(crate) fn symlink_upsert(label: &'static str, ino: i64, target: &str) -> Self {
        Self::new(
            label,
            "fs_symlink",
            "upsert",
            json!({ "ino": ino, "target": target }),
        )
    }

    pub(crate) fn symlink_delete(label: &'static str, ino: i64) -> Self {
        Self::new(label, "fs_symlink", "delete", json!({ "ino": ino }))
    }

    pub(crate) fn whiteout_upsert(
        label: &'static str,
        path: &str,
        parent_path: &str,
        created_at: i64,
    ) -> Self {
        Self::new(
            label,
            "fs_whiteout",
            "upsert",
            json!({
                "path": path,
                "parent_path": parent_path,
                "created_at": created_at,
            }),
        )
    }

    pub(crate) fn whiteout_delete(label: &'static str, path: &str) -> Self {
        Self::new(label, "fs_whiteout", "delete", json!({ "path": path }))
    }

    pub(crate) fn origin_upsert(label: &'static str, delta_ino: i64, base_ino: i64) -> Self {
        Self::new(
            label,
            "fs_origin",
            "upsert",
            json!({ "delta_ino": delta_ino, "base_ino": base_ino }),
        )
    }

    pub(crate) fn origin_delete(label: &'static str, delta_ino: i64) -> Self {
        Self::new(
            label,
            "fs_origin",
            "delete",
            json!({ "delta_ino": delta_ino }),
        )
    }

    pub(crate) fn partial_origin_upsert(label: &'static str, row: &PartialOriginRow) -> Self {
        Self::new(label, "fs_partial_origin", "upsert", row)
    }

    pub(crate) fn partial_origin_delete(label: &'static str, delta_ino: i64) -> Self {
        Self::new(
            label,
            "fs_partial_origin",
            "delete",
            json!({ "delta_ino": delta_ino }),
        )
    }

    pub(crate) fn chunk_override_upsert(
        label: &'static str,
        delta_ino: i64,
        chunk_index: i64,
    ) -> Self {
        Self::new(
            label,
            "fs_chunk_override",
            "upsert",
            json!({ "delta_ino": delta_ino, "chunk_index": chunk_index }),
        )
    }

    pub(crate) fn chunk_override_delete(
        label: &'static str,
        delta_ino: i64,
        chunk_index: i64,
    ) -> Self {
        Self::new(
            label,
            "fs_chunk_override",
            "delete",
            json!({ "delta_ino": delta_ino, "chunk_index": chunk_index }),
        )
    }

    pub(crate) fn overlay_config_upsert(label: &'static str, key: &str, value: &str) -> Self {
        Self::new(
            label,
            "fs_overlay_config",
            "upsert",
            json!({ "key": key, "value": value }),
        )
    }

    pub(crate) fn overlay_config_delete(label: &'static str, key: &str) -> Self {
        Self::new(label, "fs_overlay_config", "delete", json!({ "key": key }))
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct InodeRow {
    pub(crate) ino: i64,
    pub(crate) mode: i64,
    pub(crate) nlink: i64,
    pub(crate) uid: i64,
    pub(crate) gid: i64,
    pub(crate) size: i64,
    pub(crate) atime: i64,
    pub(crate) mtime: i64,
    pub(crate) ctime: i64,
    pub(crate) rdev: i64,
    pub(crate) atime_nsec: i64,
    pub(crate) mtime_nsec: i64,
    pub(crate) ctime_nsec: i64,
    #[serde(skip)]
    pub(crate) data_inline: Option<Vec<u8>>,
    pub(crate) storage_kind: i64,
}

impl InodeRow {
    pub(crate) fn from_row(row: &turso::Row, start: usize) -> Result<Self> {
        let integer = |index: usize, name: &str| -> Result<i64> {
            row.get_value(start + index)
                .ok()
                .and_then(|value| value.as_integer().copied())
                .ok_or_else(|| FsError::Corrupt(format!("invalid fs_inode.{name}")).into())
        };
        let data_inline = match row.get_value(start + 13) {
            Ok(Value::Blob(data)) => Some(data),
            Ok(Value::Null) => None,
            Ok(_) | Err(_) => {
                return Err(FsError::Corrupt("invalid fs_inode.data_inline".to_string()).into())
            }
        };
        Ok(Self {
            ino: integer(0, "ino")?,
            mode: integer(1, "mode")?,
            nlink: integer(2, "nlink")?,
            uid: integer(3, "uid")?,
            gid: integer(4, "gid")?,
            size: integer(5, "size")?,
            atime: integer(6, "atime")?,
            mtime: integer(7, "mtime")?,
            ctime: integer(8, "ctime")?,
            rdev: integer(9, "rdev")?,
            atime_nsec: integer(10, "atime_nsec")?,
            mtime_nsec: integer(11, "mtime_nsec")?,
            ctime_nsec: integer(12, "ctime_nsec")?,
            data_inline,
            storage_kind: integer(14, "storage_kind")?,
        })
    }

    pub(crate) fn from_stats(
        stats: &crate::fs::Stats,
        data_inline: Option<Vec<u8>>,
        storage_kind: i64,
    ) -> Self {
        Self {
            ino: stats.ino,
            mode: stats.mode as i64,
            nlink: stats.nlink as i64,
            uid: stats.uid as i64,
            gid: stats.gid as i64,
            size: stats.size,
            atime: stats.atime,
            mtime: stats.mtime,
            ctime: stats.ctime,
            rdev: stats.rdev as i64,
            atime_nsec: stats.atime_nsec as i64,
            mtime_nsec: stats.mtime_nsec as i64,
            ctime_nsec: stats.ctime_nsec as i64,
            data_inline,
            storage_kind,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PartialOriginRow {
    pub(crate) delta_ino: i64,
    pub(crate) base_ino: i64,
    pub(crate) base_path: String,
    pub(crate) base_size: i64,
    pub(crate) base_fingerprint_size: i64,
    pub(crate) base_mtime: i64,
    pub(crate) base_mtime_nsec: i64,
    pub(crate) base_ctime: i64,
    pub(crate) base_ctime_nsec: i64,
    pub(crate) created_at: i64,
}

pub(crate) struct MutationTxn<'conn> {
    txn: Transaction<'conn>,
    journal: JournalCtx,
    records: Vec<JournalDelta>,
}

impl<'conn> MutationTxn<'conn> {
    pub(crate) async fn begin(conn: &'conn Connection, journal: JournalCtx) -> Result<Self> {
        Ok(Self {
            txn: Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?,
            journal,
            records: Vec::new(),
        })
    }

    pub(crate) fn conn(&self) -> &Connection {
        &self.txn
    }

    /// Whether this transaction will journal row deltas. Callers use this to
    /// skip serialization and inline-content pinning when history is disabled.
    pub(crate) fn journaling(&self) -> bool {
        self.journal.enabled
    }

    pub(crate) fn record(&mut self, delta: JournalDelta) {
        if self.journal.enabled {
            self.records.push(delta);
        }
    }

    pub(crate) async fn record_inode(&mut self, label: &'static str, row: InodeRow) -> Result<()> {
        if !self.journal.enabled {
            return Ok(());
        }
        let inline_digest = if let Some(data) = &row.data_inline {
            let digest = blake3::hash(data).as_bytes().to_vec();
            self.txn
                .execute(
                    "INSERT INTO fs_chunk (digest, data, refcount)
                     VALUES (?, ?, 0)
                     ON CONFLICT(digest) DO NOTHING",
                    (Value::Blob(digest.clone()), Value::Blob(data.clone())),
                )
                .await?;
            Some(digest)
        } else {
            None
        };
        let mut value = serde_json::to_value(&row)?;
        let object = value
            .as_object_mut()
            .expect("serialized inode journal row must be an object");
        object.insert(
            "data_inline_digest".to_string(),
            inline_digest
                .as_ref()
                .map(|digest| JsonValue::String(hex_digest(digest)))
                .unwrap_or(JsonValue::Null),
        );
        self.record(JournalDelta {
            label,
            tbl: "fs_inode",
            verb: "upsert",
            row: value,
            digests: inline_digest.into_iter().collect(),
        });
        Ok(())
    }

    pub(crate) async fn record_storage_changes(
        &mut self,
        label: &'static str,
        changes: StorageChanges,
    ) -> Result<()> {
        self.record_inode(label, changes.inode).await?;
        for change in changes.data {
            match change {
                DataDelta::Upsert {
                    ino,
                    chunk_index,
                    digest,
                } => self.record(JournalDelta::data_upsert(label, ino, chunk_index, digest)),
                DataDelta::Delete { ino, chunk_index } => {
                    self.record(JournalDelta::data_delete(label, ino, chunk_index))
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn commit(self) -> Result<()> {
        let Self {
            txn,
            journal,
            records,
        } = self;
        if journal.enabled && !records.is_empty() {
            let hint = journal.next_seq.load(Ordering::Acquire);
            let mut txn_id = if hint > 0 {
                hint
            } else {
                next_txn_id(&txn).await?
            };
            let wallclock_ms = wallclock_ms()?;
            // Bulk imports journal thousands of rows in one commit, and
            // per-row statements made clone-import ~60% slower; multi-row
            // inserts keep the statement count bounded. Inside this IMMEDIATE
            // transaction no other writer can interleave, so the AUTOINCREMENT
            // seqs of one insert are contiguous and recoverable from
            // last_insert_rowid.
            let mut pins: Vec<(i64, Vec<u8>)> = Vec::new();
            let mut seen_pins = BTreeSet::new();
            let mut first_batch = true;
            let mut last = 0;
            for batch in records.chunks(JOURNAL_INSERT_ROWS) {
                let placeholders = vec!["(?, ?, ?, ?, ?, ?)"; batch.len()].join(", ");
                let sql = format!(
                    "INSERT INTO fs_op_journal (txn_id, label, tbl, verb, row, wallclock_ms)
                     VALUES {placeholders}"
                );
                let mut params = Vec::with_capacity(batch.len() * 6);
                for record in batch {
                    params.push(Value::Integer(txn_id));
                    params.push(Value::Text(record.label.to_string()));
                    params.push(Value::Text(record.tbl.to_string()));
                    params.push(Value::Text(record.verb.to_string()));
                    params.push(Value::Text(serde_json::to_string(&record.row)?));
                    params.push(Value::Integer(wallclock_ms));
                }
                let mut stmt = txn.prepare_cached(&sql).await?;
                stmt.execute(params).await?;
                last = txn.last_insert_rowid();
                let first = last - batch.len() as i64 + 1;
                if first_batch {
                    first_batch = false;
                    if first != txn_id {
                        // Stale hint (a rolled-back commit, or another opener
                        // wrote in between). Only this commit's rows can have
                        // seq >= first inside the exclusive transaction, so
                        // one patch restores txn_id == first-seq.
                        let mut stmt = txn
                            .prepare_cached("UPDATE fs_op_journal SET txn_id = ? WHERE seq >= ?")
                            .await?;
                        stmt.execute((first, first)).await?;
                        txn_id = first;
                    }
                }
                for (index, record) in batch.iter().enumerate() {
                    let seq = first + index as i64;
                    for digest in &record.digests {
                        if seen_pins.insert(digest.clone()) {
                            pins.push((seq, digest.clone()));
                        }
                    }
                }
            }
            for batch in pins.chunks(JOURNAL_PIN_INSERT_ROWS) {
                let placeholders = vec!["(?, ?)"; batch.len()].join(", ");
                let sql =
                    format!("INSERT INTO fs_journal_chunk (seq, digest) VALUES {placeholders}");
                let mut params = Vec::with_capacity(batch.len() * 2);
                for (seq, digest) in batch {
                    params.push(Value::Integer(*seq));
                    params.push(Value::Blob(digest.clone()));
                }
                let mut stmt = txn.prepare_cached(&sql).await?;
                stmt.execute(params).await?;
            }
            // Stored before the SQLite commit: a failed commit leaves the
            // hint one group too high, which the next commit detects and
            // patches. Waiters serialize on the IMMEDIATE lock, so no one
            // reads the hint before this store.
            journal.next_seq.store(last + 1, Ordering::Release);
        }
        txn.commit().await?;
        Ok(())
    }

    pub(crate) async fn rollback(self) -> Result<()> {
        self.txn.rollback().await?;
        Ok(())
    }
}

/// A transaction group's ID is the seq of its first journal row.
///
/// AUTOINCREMENT never reuses a seq and GC always retains the newest whole
/// group, so `MAX(seq) + 1` is exactly the seq the next insert will receive
/// inside this exclusive transaction. Deriving the ID this way keeps the
/// commit path free of allocator-table writes, which dirtied an extra B-tree
/// page on every mutating commit and dominated clone-workload overhead.
async fn next_txn_id(conn: &Connection) -> Result<i64> {
    // Expressed as a reverse PK walk because turso 0.5.3 evaluates
    // `MAX(seq)` with a full table scan, which made commit cost grow with
    // journal length.
    let mut stmt = conn
        .prepare_cached("SELECT seq FROM fs_op_journal ORDER BY seq DESC LIMIT 1")
        .await?;
    let mut rows = stmt.query(()).await?;
    Ok(match rows.next().await? {
        Some(row) => row
            .get_value(0)
            .ok()
            .and_then(|value| value.as_integer().copied())
            .ok_or_else(|| {
                Error::Internal("journal txn_id derivation returned no value".to_string())
            })?
            .checked_add(1)
            .ok_or_else(|| Error::Internal("journal seq overflow".to_string()))?,
        None => 1,
    })
}

fn wallclock_ms() -> Result<i64> {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH)?;
    i64::try_from(duration.as_millis())
        .map_err(|_| Error::Internal("journal wallclock overflow".to_string()))
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

pub async fn journal_gc(conn: &Connection, retention_ops: usize) -> Result<()> {
    if retention_ops == 0 {
        return Err(Error::Internal(
            "journal retention must be positive".to_string(),
        ));
    }

    let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
    let mut max_rows = txn
        .query("SELECT COALESCE(MAX(seq), 0) FROM fs_op_journal", ())
        .await?;
    let max_seq = max_rows
        .next()
        .await?
        .ok_or_else(|| Error::Internal("journal max sequence query returned no row".to_string()))?
        .get_value(0)
        .ok()
        .and_then(|value| value.as_integer().copied())
        .unwrap_or(0);
    drop(max_rows);
    let retention_ops = i64::try_from(retention_ops)
        .map_err(|_| Error::Internal("journal retention is too large".to_string()))?;
    let horizon = max_seq.saturating_sub(retention_ops);

    if horizon > 0 {
        txn.execute(
            "DELETE FROM fs_journal_chunk
             WHERE seq IN (
                 SELECT seq FROM fs_op_journal
                 WHERE txn_id IN (
                     SELECT txn_id FROM fs_op_journal
                     GROUP BY txn_id
                     HAVING MAX(seq) <= ?
                 )
             )",
            (horizon,),
        )
        .await?;
        txn.execute(
            "DELETE FROM fs_op_journal
             WHERE txn_id IN (
                 SELECT txn_id FROM fs_op_journal
                 GROUP BY txn_id
                 HAVING MAX(seq) <= ?
             )",
            (horizon,),
        )
        .await?;
    }

    txn.execute(
        "DELETE FROM fs_chunk
         WHERE refcount = 0
           AND digest NOT IN (SELECT digest FROM fs_journal_chunk)
           AND digest NOT IN (SELECT digest FROM fs_snapshot_chunk)",
        (),
    )
    .await?;
    txn.commit().await?;
    Ok(())
}
