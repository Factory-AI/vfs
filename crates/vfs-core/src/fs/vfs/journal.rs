use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

use crate::error::{Error, Result};
use crate::fs::FsError;

use super::store::{DataDelta, StorageChanges};

/// Rows per multi-row journal insert; two shapes (full batch and remainder)
/// keep prepared-statement variety low while bounding SQL length.
const JOURNAL_INSERT_ROWS: usize = 64;

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
    /// Whether this opener has already durably marked history invalid.
    ///
    /// With the kill switch on, the first mutating commit writes
    /// `history_valid=0` in the same transaction as the mutation it fails to
    /// journal, so the gap is detectable even after a crash. Maintenance
    /// opens that mutate nothing (snapshot reads, staged reverts) leave a
    /// valid history valid. Set only after a successful commit so a failed
    /// one retries the marker.
    invalidated: Arc<AtomicBool>,
    /// Digests this opener has already committed to `fs_chunk`.
    ///
    /// Inline post-images content-address their bytes on every inode row, and
    /// a workload like a git clone rewrites the same inline content two or
    /// three times per file (create, write, setattr). Without this set each
    /// occurrence ships the full blob in an `ON CONFLICT DO NOTHING` upsert;
    /// with it only the first insert pays. Digests enter the set only after
    /// their transaction commits, so a rollback cannot leave the set claiming
    /// a chunk the database never got. Journal chunk GC runs only offline
    /// (pack's collect path), never beside a live opener, so a cached digest
    /// cannot be collected out from under a later pin.
    known_chunks: Arc<Mutex<HashSet<[u8; 32]>>>,
}

/// Bound on the known-digest set; a clone of the codex fixture produces ~5k
/// distinct inline digests, so this is generous while capping memory at
/// ~8 MiB. Overflow clears: correctness never depends on membership.
const KNOWN_CHUNKS_CAP: usize = 1 << 18;

impl JournalCtx {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            next_seq: Arc::new(AtomicI64::new(0)),
            invalidated: Arc::new(AtomicBool::new(false)),
            known_chunks: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn chunk_known(&self, digest: &[u8]) -> bool {
        let Ok(digest) = <[u8; 32]>::try_from(digest) else {
            return false;
        };
        self.known_chunks
            .lock()
            .expect("known-chunk set is never poisoned")
            .contains(&digest)
    }

    /// Drop every cached digest. Must be called after any operation that can
    /// delete `fs_chunk` rows on a live opener (journal GC, floor
    /// establishment, root capture), since a collected chunk the cache still
    /// vouches for would let a later commit pin a digest the database no
    /// longer holds.
    pub(crate) fn forget_chunks(&self) {
        self.known_chunks
            .lock()
            .expect("known-chunk set is never poisoned")
            .clear();
    }

    fn remember_chunks(&self, digests: &[[u8; 32]]) {
        if digests.is_empty() {
            return;
        }
        let mut known = self
            .known_chunks
            .lock()
            .expect("known-chunk set is never poisoned");
        if known.len() + digests.len() > KNOWN_CHUNKS_CAP {
            known.clear();
        }
        known.extend(digests.iter().copied());
    }
}

#[derive(Debug)]
pub(crate) struct JournalDelta {
    label: &'static str,
    tbl: &'static str,
    verb: &'static str,
    row: JsonValue,
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
        }
    }

    pub(crate) fn inode_delete(label: &'static str, ino: i64) -> Self {
        Self::new(label, "fs_inode", "delete", json!({ "ino": ino }))
    }

    pub(crate) fn dentry_upsert(
        label: &'static str,
        id: i64,
        parent_ino: i64,
        name: &str,
        ino: i64,
    ) -> Self {
        Self::new(
            label,
            "fs_dentry",
            "upsert",
            json!({ "id": id, "parent_ino": parent_ino, "name": name, "ino": ino }),
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
    /// Digests whose `fs_chunk` upsert this transaction issued; promoted to
    /// the shared known-digest set only after the commit succeeds.
    inserted_chunks: Vec<[u8; 32]>,
}

impl<'conn> MutationTxn<'conn> {
    pub(crate) async fn begin(conn: &'conn Connection, journal: JournalCtx) -> Result<Self> {
        Ok(Self {
            txn: Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?,
            journal,
            records: Vec::new(),
            inserted_chunks: Vec::new(),
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
            let digest = *blake3::hash(data).as_bytes();
            if !self.journal.chunk_known(&digest) {
                self.txn
                    .execute(
                        "INSERT INTO fs_chunk (digest, data, refcount)
                         VALUES (?, ?, 0)
                         ON CONFLICT(digest) DO NOTHING",
                        (Value::Blob(digest.to_vec()), Value::Blob(data.clone())),
                    )
                    .await?;
                self.inserted_chunks.push(digest);
            }
            Some(digest.to_vec())
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
            inserted_chunks,
        } = self;
        let marks_history_invalid =
            !journal.enabled && !journal.invalidated.load(Ordering::Acquire);
        if marks_history_invalid {
            // Every MutationTxn mutates replayable scope, so this commit is
            // exactly the unjournaled gap the durable epoch contract must
            // record. Committed atomically with the mutation itself.
            txn.execute(
                "INSERT INTO fs_config (key, value) VALUES ('history_valid', '0')
                 ON CONFLICT(key) DO UPDATE SET value = '0'",
                (),
            )
            .await?;
        }
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
            }
            // Stored before the SQLite commit: a failed commit leaves the
            // hint one group too high, which the next commit detects and
            // patches. Waiters serialize on the IMMEDIATE lock, so no one
            // reads the hint before this store.
            journal.next_seq.store(last + 1, Ordering::Release);
        }
        txn.commit().await?;
        if marks_history_invalid {
            journal.invalidated.store(true, Ordering::Release);
        }
        journal.remember_chunks(&inserted_chunks);
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
    // Bare `MAX(seq)` only: turso's min/max optimization plans it as a
    // reverse seek, but wrapping it (e.g. `COALESCE(MAX(seq), 0)`) falls
    // back to a full table scan, so the NULL of an empty journal is handled
    // here instead of in SQL.
    let mut stmt = conn
        .prepare_cached("SELECT MAX(seq) FROM fs_op_journal")
        .await?;
    let mut rows = stmt.query(()).await?;
    let head = match rows.next().await? {
        Some(row) => match row.get_value(0)? {
            Value::Null => None,
            Value::Integer(seq) => Some(seq),
            other => {
                return Err(Error::Internal(format!(
                    "journal head aggregate returned non-integer {other:?}"
                )))
            }
        },
        None => None,
    };
    head.unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| Error::Internal("journal seq overflow".to_string()))
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
    super::super::history::journal_gc(conn, retention_ops).await
}
