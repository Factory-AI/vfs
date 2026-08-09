use serde_json::{json, Map, Value as JsonValue};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

use crate::error::{Error, Result};

use super::{store::NormalizedWriteRange, STORAGE_INLINE};

/// Rows per multi-row journal insert; two shapes (full batch and remainder)
/// keep prepared-statement variety low while bounding SQL length.
const JOURNAL_INSERT_ROWS: usize = 64;
const JOURNAL_PIN_INSERT_ROWS: usize = 128;

#[derive(Debug)]
pub(in crate::fs) struct JournalOp {
    name: &'static str,
    payload: JsonValue,
    digests: Vec<Vec<u8>>,
}

impl JournalOp {
    pub(in crate::fs) fn new(name: &'static str, payload: JsonValue) -> Self {
        Self {
            name,
            payload,
            digests: Vec::new(),
        }
    }

    pub(in crate::fs) fn with_digests(
        name: &'static str,
        payload: JsonValue,
        digests: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            name,
            payload,
            digests,
        }
    }

    pub(in crate::fs) fn setattr(ino: i64, fields: JsonValue) -> Self {
        Self::new("setattr", json!({ "ino": ino, "fields": fields }))
    }

    pub(in crate::fs) async fn write(
        conn: &Connection,
        ino: i64,
        chunk_size: usize,
        ranges: &[NormalizedWriteRange],
    ) -> Result<Self> {
        let storage_kind = inode_storage_kind(conn, ino).await?;
        if storage_kind == STORAGE_INLINE {
            let ranges = ranges
                .iter()
                .map(|range| {
                    json!({
                        "offset": range.offset,
                        "len": range.data.len(),
                        "digests": [],
                    })
                })
                .collect::<Vec<_>>();
            return Ok(Self::new(
                "write",
                json!({ "ino": ino, "ranges": ranges, "inline": true }),
            ));
        }

        let mut payload_ranges = Vec::with_capacity(ranges.len());
        let mut pins = BTreeSet::new();
        for range in ranges {
            let len = range.data.len();
            if len == 0 {
                continue;
            }
            let start_chunk = range.offset / chunk_size as u64;
            let end_chunk = (range.offset + len as u64 - 1) / chunk_size as u64;
            let digests = mapping_digests(conn, ino, start_chunk as i64, end_chunk as i64).await?;
            let digest_hex = digests
                .iter()
                .map(|digest| hex_digest(digest))
                .collect::<Vec<_>>();
            pins.extend(digests);
            payload_ranges.push(json!({
                "offset": range.offset,
                "len": len,
                "digests": digest_hex,
            }));
        }

        Ok(Self::with_digests(
            "write",
            json!({ "ino": ino, "ranges": payload_ranges }),
            pins.into_iter().collect(),
        ))
    }

    pub(in crate::fs) async fn truncate(
        conn: &Connection,
        ino: i64,
        chunk_size: usize,
        size: u64,
    ) -> Result<Self> {
        let storage_kind = inode_storage_kind(conn, ino).await?;
        if storage_kind == STORAGE_INLINE || size == 0 {
            return Ok(Self::new(
                "truncate",
                json!({ "ino": ino, "size": size, "inline": storage_kind == STORAGE_INLINE }),
            ));
        }

        let tail_index = ((size - 1) / chunk_size as u64) as i64;
        let digests = mapping_digests(conn, ino, tail_index, tail_index).await?;
        let digest_hex = digests
            .iter()
            .map(|digest| hex_digest(digest))
            .collect::<Vec<_>>();
        Ok(Self::with_digests(
            "truncate",
            json!({ "ino": ino, "size": size, "digests": digest_hex }),
            digests,
        ))
    }

    /// `digests` are the chunk digests the importer just wrote, in chunk
    /// order; `None` means the content was stored inline. Taking them from
    /// the writer instead of reading mappings back keeps bulk import free of
    /// per-file SELECT round trips.
    pub(in crate::fs) fn import(ino: i64, size: u64, digests: Option<Vec<Vec<u8>>>) -> Self {
        let Some(digests) = digests else {
            return Self::new(
                "import",
                json!({ "ino": ino, "size": size, "inline": true }),
            );
        };
        let digest_hex = digests
            .iter()
            .map(|digest| hex_digest(digest))
            .collect::<Vec<_>>();
        Self::with_digests(
            "import",
            json!({ "ino": ino, "size": size, "digests": digest_hex }),
            digests,
        )
    }
}

pub(in crate::fs) struct MutationTxn<'conn> {
    txn: Transaction<'conn>,
    journal_enabled: bool,
    records: Vec<JournalOp>,
}

impl<'conn> MutationTxn<'conn> {
    pub(in crate::fs) async fn begin(
        conn: &'conn Connection,
        journal_enabled: bool,
    ) -> Result<Self> {
        Ok(Self {
            txn: Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?,
            journal_enabled,
            records: Vec::new(),
        })
    }

    pub(in crate::fs) fn conn(&self) -> &Connection {
        &self.txn
    }

    pub(in crate::fs) fn record(&mut self, op: JournalOp) {
        if self.journal_enabled {
            self.records.push(op);
        }
    }

    pub(in crate::fs) async fn commit(self) -> Result<()> {
        let Self {
            txn,
            journal_enabled,
            records,
        } = self;
        if journal_enabled && !records.is_empty() {
            let txn_id = next_txn_id(&txn).await?;
            let wallclock_ms = wallclock_ms()?;
            // Bulk imports journal thousands of rows in one commit, and
            // per-row statements made clone-import ~60% slower; multi-row
            // inserts keep the statement count bounded. Inside this IMMEDIATE
            // transaction no other writer can interleave, so the AUTOINCREMENT
            // seqs of one insert are contiguous and recoverable from
            // last_insert_rowid.
            let mut pins: Vec<(i64, Vec<u8>)> = Vec::new();
            for batch in records.chunks(JOURNAL_INSERT_ROWS) {
                let placeholders = vec!["(?, ?, ?, ?)"; batch.len()].join(", ");
                let sql = format!(
                    "INSERT INTO fs_op_journal (txn_id, op, payload, wallclock_ms)
                     VALUES {placeholders}"
                );
                let mut params = Vec::with_capacity(batch.len() * 4);
                for record in batch {
                    params.push(Value::Integer(txn_id));
                    params.push(Value::Text(record.name.to_string()));
                    params.push(Value::Text(serde_json::to_string(&record.payload)?));
                    params.push(Value::Integer(wallclock_ms));
                }
                let mut stmt = txn.prepare_cached(&sql).await?;
                stmt.execute(params).await?;
                let last = txn.last_insert_rowid();
                let first = last - batch.len() as i64 + 1;
                for (index, record) in batch.iter().enumerate() {
                    let seq = first + index as i64;
                    for digest in &record.digests {
                        pins.push((seq, digest.clone()));
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
        }
        txn.commit().await?;
        Ok(())
    }

    pub(in crate::fs) async fn rollback(self) -> Result<()> {
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

async fn inode_storage_kind(conn: &Connection, ino: i64) -> Result<i64> {
    let mut rows = conn
        .query("SELECT storage_kind FROM fs_inode WHERE ino = ?", (ino,))
        .await?;
    rows.next()
        .await?
        .ok_or_else(|| Error::Internal(format!("inode {ino} has no storage_kind")))?
        .get_value(0)
        .ok()
        .and_then(|value| value.as_integer().copied())
        .ok_or_else(|| Error::Internal(format!("inode {ino} has no storage_kind")))
}

async fn mapping_digests(
    conn: &Connection,
    ino: i64,
    start_chunk: i64,
    end_chunk: i64,
) -> Result<Vec<Vec<u8>>> {
    let mut rows = conn
        .query(
            "SELECT digest FROM fs_data
             WHERE ino = ? AND chunk_index BETWEEN ? AND ?
             ORDER BY chunk_index",
            (ino, start_chunk, end_chunk),
        )
        .await?;
    collect_digests(&mut rows).await
}

async fn collect_digests(rows: &mut turso::Rows) -> Result<Vec<Vec<u8>>> {
    let mut digests = Vec::new();
    while let Some(row) = rows.next().await? {
        match row.get_value(0) {
            Ok(Value::Blob(digest)) => digests.push(digest),
            Ok(_) | Err(_) => {
                return Err(Error::Internal(
                    "journal encountered a non-blob chunk digest".to_string(),
                ))
            }
        }
    }
    Ok(digests)
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
           AND digest NOT IN (SELECT digest FROM fs_journal_chunk)",
        (),
    )
    .await?;
    txn.commit().await?;
    Ok(())
}

pub(in crate::fs) fn fields(
    entries: impl IntoIterator<Item = (&'static str, JsonValue)>,
) -> JsonValue {
    let mut fields = Map::new();
    fields.extend(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value)),
    );
    JsonValue::Object(fields)
}
