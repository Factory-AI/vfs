//! Persistent metadata and maintenance operations for transferable sessions.

use serde::{Deserialize, Serialize};
use std::path::Path;

use turso::{Builder, Connection, Value};

use crate::error::{Error, Result};
use crate::fs::vfs::{JournalDelta, MutationTxn};
use crate::Vfs;

const GENERATION_KEY: &str = "generation";
const SEEDED_PATHS_KEY: &str = "seeded_paths";
const SEED_PIN_KEY: &str = "seed_pin";

/// Key in `fs_overlay_config` recording the sha256 of the frozen parent
/// artifact a branch session reads through. Presence makes the database a
/// branch delta: its mount shape is overlay(branch, overlay(parent, base)),
/// and the mount MUST refuse to serve if the artifact's bytes no longer hash
/// to this digest.
const PARENT_ARTIFACT_KEY: &str = "parent_artifact";

/// Persistent handoff metadata stored inside a session database.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    /// Monotonic counter incremented by every successful pack.
    pub generation: u64,
    /// Paths materialized by the future seed command.
    pub seeded_paths: Vec<String>,
}

/// Status fields that remain meaningful while a run session is live.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatusMetadata {
    /// Monotonic counter incremented by every successful pack.
    pub generation: u64,
    /// Whether a seed manifest exists, including an empty clean-checkout manifest.
    pub seeded: bool,
}

impl Vfs {
    /// Enumerate the content-addressed chunks currently stored in this database.
    pub async fn chunk_digests(&self) -> Result<Vec<[u8; 32]>> {
        let conn = self.pool.get_connection().await?;
        let mut rows = conn.query("SELECT digest FROM fs_chunk", ()).await?;
        let mut digests = Vec::new();
        while let Some(row) = rows.next().await? {
            let digest = row.get::<Vec<u8>>(0)?;
            let length = digest.len();
            digests.push(
                <[u8; 32]>::try_from(digest).map_err(|_| Error::InvalidChunkDigest { length })?,
            );
        }
        Ok(digests)
    }

    /// Read one content-addressed chunk, if it is still present.
    pub async fn chunk_data(&self, digest: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        let conn = self.pool.get_connection().await?;
        let mut rows = conn
            .query(
                "SELECT data FROM fs_chunk WHERE digest = ?",
                (Value::Blob(digest.to_vec()),),
            )
            .await?;
        match rows.next().await? {
            Some(row) => match row.get_value(0)? {
                Value::Blob(data) => Ok(Some(data)),
                value => Err(Error::Internal(format!(
                    "invalid fs_chunk data for {}: {value:?}",
                    blake3::Hash::from_bytes(*digest).to_hex()
                ))),
            },
            None => Ok(None),
        }
    }

    /// Read persistent session handoff metadata.
    pub async fn session_metadata(&self) -> Result<SessionMetadata> {
        let conn = self.pool.get_connection().await?;
        read_session_metadata(&conn).await
    }

    /// Read the compact metadata needed by machine-readable session status.
    pub async fn session_status_metadata(&self) -> Result<SessionStatusMetadata> {
        let conn = self.pool.get_connection().await?;
        read_session_status_metadata(&conn).await
    }

    /// Atomically increment and return the persistent session generation.
    pub async fn increment_session_generation(&self) -> Result<SessionMetadata> {
        let conn = self.pool.get_connection().await?;
        if let Some(value) = read_metadata_value(&conn, GENERATION_KEY).await? {
            value.parse::<u64>().map_err(|error| {
                Error::Internal(format!("invalid generation value {value:?}: {error}"))
            })?;
        }
        conn.execute(
            "INSERT INTO fs_session_metadata (key, value) VALUES (?, '1')
             ON CONFLICT(key) DO UPDATE
             SET value = CAST(fs_session_metadata.value AS INTEGER) + 1",
            (GENERATION_KEY,),
        )
        .await?;
        let generation = read_metadata_value(&conn, GENERATION_KEY)
            .await?
            .ok_or_else(|| Error::Internal("generation row is missing".to_string()))?
            .parse::<u64>()
            .map_err(|error| Error::Internal(format!("invalid generation value: {error}")))?;
        let seeded_paths = match read_metadata_value(&conn, SEEDED_PATHS_KEY).await? {
            Some(value) => serde_json::from_str(&value)?,
            None => Vec::new(),
        };
        Ok(SessionMetadata {
            generation,
            seeded_paths,
        })
    }

    /// Persist the paths materialized by the future seed command.
    pub async fn set_seeded_paths(&self, paths: &[String]) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let value = serde_json::to_string(paths)?;
        write_metadata_value(&conn, SEEDED_PATHS_KEY, value).await
    }

    /// Read the git commit recorded as the session's seed pin, if any.
    pub async fn seed_pin(&self) -> Result<Option<String>> {
        let conn = self.pool.get_connection().await?;
        read_metadata_value(&conn, SEED_PIN_KEY).await
    }

    /// Atomically persist seed whiteouts, the path manifest, and the pin.
    ///
    /// Content import commits before this call; publishing the whiteouts,
    /// manifest, and pin together prevents a completed seed from exposing only
    /// half of its deletion state.
    pub async fn record_seed_state(
        &self,
        seeded_paths: &[String],
        whiteout_paths: &[String],
        pin: &str,
    ) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let mut txn = MutationTxn::begin(&conn, self.fs.journal_ctx()).await?;
        let result = async {
            let created_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            for path in whiteout_paths {
                let parent_path = crate::fs::overlay::parent_path_for_whiteout(path);
                txn.conn()
                    .execute(
                        "INSERT OR REPLACE INTO fs_whiteout (path, parent_path, created_at)
                     VALUES (?, ?, ?)",
                        (path.as_str(), parent_path.as_str(), created_at),
                    )
                    .await?;
                txn.record(JournalDelta::whiteout_upsert(
                    "seed_whiteout",
                    path,
                    &parent_path,
                    created_at,
                ));
            }
            write_metadata_value(txn.conn(), SEED_PIN_KEY, pin.to_string()).await?;
            let value = serde_json::to_string(seeded_paths)?;
            write_metadata_value(txn.conn(), SEEDED_PATHS_KEY, value).await
        }
        .await;

        match result {
            Ok(()) => {
                txn.commit().await?;
                Ok(())
            }
            Err(error) => {
                let _ = txn.rollback().await;
                Err(error)
            }
        }
    }

    /// Checkpoint and compact a local database into a new single-file artifact.
    pub async fn compact_local_database_into(&self, output: &Path) -> Result<()> {
        self.fs.finalize().await?;
        let conn = self.pool.get_connection().await?;
        conn.execute("PRAGMA synchronous = FULL", ()).await?;

        let compact_result = async {
            checkpoint_truncate(&conn).await?;
            vacuum_into(&conn, output).await
        }
        .await;

        conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
        compact_result?;
        drop(conn);
        self.fs.finalize().await?;
        publish_single_file_artifact(output).await
    }

    /// Record the frozen parent artifact digest this branch delta reads
    /// through. Requires the overlay schema to be initialized.
    pub async fn set_overlay_parent_artifact(&self, digest: &str) -> Result<()> {
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Internal(format!(
                "invalid parent artifact digest {digest:?}: expected 64 hex characters"
            )));
        }
        let conn = self.pool.get_connection().await?;
        let mut txn = MutationTxn::begin(&conn, self.fs.journal_ctx()).await?;
        txn.conn()
            .execute(
                "INSERT INTO fs_overlay_config (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                (PARENT_ARTIFACT_KEY, digest.to_ascii_lowercase()),
            )
            .await?;
        txn.record(JournalDelta::overlay_config_upsert(
            "parent_artifact",
            PARENT_ARTIFACT_KEY,
            &digest.to_ascii_lowercase(),
        ));
        txn.commit().await?;
        Ok(())
    }

    /// Read the frozen parent artifact digest, if this is a branch delta.
    pub async fn overlay_parent_artifact(&self) -> Result<Option<String>> {
        self.overlay_config_value(PARENT_ARTIFACT_KEY).await
    }

    /// Remove the parent artifact reference after its state has been folded
    /// into this database.
    pub async fn clear_overlay_parent_artifact(&self) -> Result<()> {
        let conn = self.pool.get_connection().await?;
        let mut txn = MutationTxn::begin(&conn, self.fs.journal_ctx()).await?;
        let changed = txn
            .conn()
            .execute(
                "DELETE FROM fs_overlay_config WHERE key = ?",
                (PARENT_ARTIFACT_KEY,),
            )
            .await?;
        if changed > 0 {
            txn.record(JournalDelta::overlay_config_delete(
                "parent_artifact_clear",
                PARENT_ARTIFACT_KEY,
            ));
        }
        txn.commit().await?;
        Ok(())
    }

    /// Read the overlay base directory recorded at initialization, if any.
    pub async fn overlay_base_path(&self) -> Result<Option<String>> {
        self.overlay_config_value("base_path").await
    }

    async fn overlay_config_value(&self, key: &str) -> Result<Option<String>> {
        let conn = self.pool.get_connection().await?;
        // A database without the overlay schema is a plain Vfs.
        let mut tables = conn
            .query(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'fs_overlay_config'",
                (),
            )
            .await?;
        if tables.next().await?.is_none() {
            return Ok(None);
        }
        let mut rows = conn
            .query("SELECT value FROM fs_overlay_config WHERE key = ?", (key,))
            .await?;
        match rows.next().await? {
            Some(row) => match row.get_value(0)? {
                Value::Text(value) => Ok(Some(value)),
                value => Err(Error::Internal(format!(
                    "invalid fs_overlay_config value for {key}: {value:?}"
                ))),
            },
            None => Ok(None),
        }
    }

    /// Copy a consistent point-in-time image of a live database into a new
    /// single-file artifact.
    ///
    /// Unlike [`Vfs::compact_local_database_into`], this is safe to call while
    /// the filesystem is serving a mount: it drains pending batched writes so
    /// every write acknowledged before the call is included, then copies a
    /// read-consistent image with `VACUUM INTO` while concurrent writers
    /// proceed. `output` must not already exist.
    pub async fn snapshot_into(&self, output: &Path) -> Result<()> {
        self.fs.drain_all().await?;
        let conn = self.pool.get_connection().await?;
        vacuum_into(&conn, output).await?;
        drop(conn);
        publish_single_file_artifact(output).await
    }

    /// Truncate the op journal to the configured retention horizon and
    /// collect zero-refcount chunks no surviving journal entry pins.
    ///
    /// `pack` runs this on its private staging copy before compaction so a
    /// shipped artifact carries a bounded journal and no unreachable chunk
    /// bytes. Pending batched writes are drained first so every acknowledged
    /// write is journaled before the horizon is computed.
    pub async fn collect_journal(&self) -> Result<()> {
        self.fs.drain_all().await?;
        let conn = self.pool.get_connection().await?;
        crate::fs::journal_gc(&conn, self.fs.journal_retention_ops()).await?;
        // GC may have deleted zero-ref chunks the known-digest cache vouches
        // for; a stale entry would let a later commit pin a missing digest.
        self.fs.journal_ctx().forget_chunks();
        Ok(())
    }
}

async fn vacuum_into(conn: &Connection, output: &Path) -> Result<()> {
    let escaped_output = output.to_string_lossy().replace('\'', "''");
    conn.execute(&format!("VACUUM INTO '{escaped_output}'"), ())
        .await?;
    Ok(())
}

/// Checkpoint a freshly written copy into a durable single-file family.
async fn publish_single_file_artifact(output: &Path) -> Result<()> {
    let output_str = output
        .to_str()
        .ok_or_else(|| Error::InvalidUtf8Path(output.display().to_string()))?;
    let output_db = Builder::new_local(output_str).build().await?;
    let output_conn = output_db.connect()?;
    checkpoint_truncate(&output_conn).await?;
    drop(output_conn);
    drop(output_db);
    remove_sidecar_if_present(output, "-wal")?;
    remove_sidecar_if_present(output, "-shm")?;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(output)?
        .sync_all()?;
    Ok(())
}

fn remove_sidecar_if_present(path: &Path, suffix: &str) -> Result<()> {
    let sidecar = Path::new(&format!("{}{}", path.display(), suffix)).to_path_buf();
    match std::fs::remove_file(&sidecar) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn read_session_metadata(conn: &Connection) -> Result<SessionMetadata> {
    let generation = match read_metadata_value(conn, GENERATION_KEY).await? {
        Some(value) => value.parse::<u64>().map_err(|error| {
            Error::Internal(format!(
                "invalid session metadata generation {value:?}: {error}"
            ))
        })?,
        None => 0,
    };
    let seeded_paths = match read_metadata_value(conn, SEEDED_PATHS_KEY).await? {
        Some(value) => serde_json::from_str(&value)?,
        None => Vec::new(),
    };
    Ok(SessionMetadata {
        generation,
        seeded_paths,
    })
}

async fn read_session_status_metadata(conn: &Connection) -> Result<SessionStatusMetadata> {
    let generation = match read_metadata_value(conn, GENERATION_KEY).await? {
        Some(value) => value.parse::<u64>().map_err(|error| {
            Error::Internal(format!(
                "invalid session metadata generation {value:?}: {error}"
            ))
        })?,
        None => 0,
    };
    Ok(SessionStatusMetadata {
        generation,
        seeded: read_metadata_value(conn, SEEDED_PATHS_KEY).await?.is_some(),
    })
}

async fn read_metadata_value(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut rows = conn
        .query(
            "SELECT value FROM fs_session_metadata WHERE key = ?",
            (key,),
        )
        .await?;
    match rows.next().await? {
        Some(row) => match row.get_value(0)? {
            Value::Text(value) => Ok(Some(value)),
            value => Err(Error::Internal(format!(
                "invalid fs_session_metadata value for {key}: {value:?}"
            ))),
        },
        None => Ok(None),
    }
}

async fn write_metadata_value(conn: &Connection, key: &str, value: String) -> Result<()> {
    conn.execute(
        "INSERT INTO fs_session_metadata (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )
    .await?;
    Ok(())
}

async fn checkpoint_truncate(conn: &Connection) -> Result<()> {
    let mut rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
    if let Some(row) = rows.next().await? {
        let busy: i64 = row.get(0)?;
        if busy != 0 {
            return Err(Error::Internal(
                "WAL checkpoint could not complete because the database is busy".to_string(),
            ));
        }
    }
    while rows.next().await?.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::VfsOptions;

    #[tokio::test]
    async fn chunk_accessors_enumerate_and_read_content() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("session.db");
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;
        let content: Vec<u8> = (0..100_000).map(|index| (index % 251) as u8).collect();
        let (_, file) = vfs.fs.create_file("/chunked.bin", 0o100644, 0, 0).await?;
        file.pwrite(0, &content).await?;
        file.fsync().await?;
        drop(file);

        let digests = vfs.chunk_digests().await?;
        assert!(!digests.is_empty());

        let conn = vfs.get_connection().await?;
        let mut rows = conn.query("SELECT digest FROM fs_chunk", ()).await?;
        let mut expected = std::collections::HashSet::new();
        while let Some(row) = rows.next().await? {
            expected.insert(row.get::<Vec<u8>>(0)?);
        }
        drop(rows);
        drop(conn);
        assert_eq!(
            digests
                .iter()
                .map(|digest| digest.to_vec())
                .collect::<std::collections::HashSet<_>>(),
            expected
        );

        for digest in &digests {
            let data = vfs
                .chunk_data(digest)
                .await?
                .expect("enumerated chunk must still exist");
            assert_eq!(blake3::hash(&data).as_bytes(), digest);
        }
        assert_eq!(vfs.chunk_data(&[0xff; 32]).await?, None);
        Ok(())
    }

    #[tokio::test]
    async fn chunk_digests_rejects_malformed_rows() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("session.db");
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;
        let conn = vfs.get_connection().await?;
        conn.execute(
            "INSERT INTO fs_chunk (digest, data, refcount) VALUES (?, ?, 0)",
            (Value::Blob(vec![0xaa]), Value::Blob(vec![0xbb])),
        )
        .await?;
        drop(conn);

        assert!(matches!(
            vfs.chunk_digests().await,
            Err(Error::InvalidChunkDigest { length: 1 })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn metadata_defaults_and_generation_is_monotonic() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("session.db");
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;

        assert_eq!(vfs.session_metadata().await?, SessionMetadata::default());
        assert_eq!(
            vfs.session_status_metadata().await?,
            SessionStatusMetadata::default()
        );
        assert_eq!(vfs.increment_session_generation().await?.generation, 1);
        assert_eq!(vfs.increment_session_generation().await?.generation, 2);

        let seeded_paths = vec!["/src/lib.rs".to_string(), "/Cargo.toml".to_string()];
        vfs.set_seeded_paths(&seeded_paths).await?;
        assert_eq!(
            vfs.session_status_metadata().await?,
            SessionStatusMetadata {
                generation: 2,
                seeded: true,
            }
        );
        assert_eq!(
            vfs.session_metadata().await?,
            SessionMetadata {
                generation: 2,
                seeded_paths: seeded_paths.clone(),
            }
        );
        let seeded_paths = vec!["src/main.rs".to_string(), "deleted.txt".to_string()];
        vfs.record_seed_state(&seeded_paths, &["/deleted.txt".to_string()], "pin-sha")
            .await?;
        assert_eq!(
            vfs.session_metadata().await?.seeded_paths,
            seeded_paths.clone()
        );
        assert_eq!(vfs.seed_pin().await?.as_deref(), Some("pin-sha"));
        assert_eq!(
            vfs.get_whiteouts().await?,
            std::collections::HashSet::from(["/deleted.txt".to_string()])
        );
        drop(vfs);
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;
        assert_eq!(vfs.session_metadata().await?.generation, 2);
        let compacted_path = dir.path().join("compacted.db");
        vfs.compact_local_database_into(&compacted_path).await?;
        drop(vfs);
        let compacted = Vfs::open(VfsOptions::with_path(compacted_path.to_string_lossy())).await?;
        assert_eq!(
            compacted.session_metadata().await?,
            SessionMetadata {
                generation: 2,
                seeded_paths,
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_into_is_consistent_under_concurrent_writes() -> Result<()> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tempdir()?;
        let db_path = dir.path().join("session.db");
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;

        let (_, file) = vfs.fs.create_file("/pinned.txt", 0o100644, 0, 0).await?;
        file.pwrite(0, b"pinned before snapshot").await?;
        file.fsync().await?;
        drop(file);

        // Churn writer racing the snapshot copy: every file it creates is
        // either absent from the snapshot or fully intact — never torn.
        let writer_fs = vfs.fs.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let writer_stop = stop.clone();
        let writer = tokio::spawn(async move {
            let mut created = 0u32;
            while !writer_stop.load(Ordering::Relaxed) {
                let path = format!("/churn-{created}.txt");
                let (_, file) = writer_fs.create_file(&path, 0o100644, 0, 0).await?;
                file.pwrite(0, &[b'x'; 8192]).await?;
                file.fsync().await?;
                created += 1;
                // Keep the writer concurrent without starving turso's
                // VACUUM INTO I/O completion loop on the current-thread test
                // runtime.
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Ok::<u32, Error>(created)
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let snapshot_path = dir.path().join("snapshot.db");
        vfs.snapshot_into(&snapshot_path).await?;
        stop.store(true, Ordering::Relaxed);
        let created = writer.await.expect("writer task panicked")?;
        assert!(created > 0, "churn writer made no progress");
        drop(vfs);

        for suffix in ["-wal", "-shm"] {
            let sidecar = format!("{}{suffix}", snapshot_path.display());
            assert!(
                !Path::new(&sidecar).exists(),
                "snapshot left sidecar {sidecar}"
            );
        }

        let snapshot = Vfs::open(VfsOptions::with_path(snapshot_path.to_string_lossy())).await?;
        assert_eq!(
            snapshot.fs.read_file("/pinned.txt").await?.as_deref(),
            Some(b"pinned before snapshot".as_slice())
        );
        let entries = crate::fs::FileSystem::readdir(&snapshot.fs, 1)
            .await?
            .expect("snapshot root must list");
        for entry in entries {
            if let Some(rest) = entry.strip_prefix("churn-") {
                let content = snapshot
                    .fs
                    .read_file(&format!("/{entry}"))
                    .await?
                    .unwrap_or_else(|| panic!("churn file {rest} listed but unreadable"));
                assert_eq!(content, vec![b'x'; 8192], "torn churn file {entry}");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn empty_seed_manifest_still_marks_session_seeded() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("session.db");
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;

        vfs.set_seeded_paths(&[]).await?;

        assert!(vfs.session_status_metadata().await?.seeded);
        assert!(vfs.session_metadata().await?.seeded_paths.is_empty());
        Ok(())
    }
}
