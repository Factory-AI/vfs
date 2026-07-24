//! Persistent metadata and maintenance operations for transferable sessions.

use serde::{Deserialize, Serialize};
use std::path::Path;

use turso::{Builder, Connection, Value};

use crate::error::{Error, Result};
use crate::Vfs;

const GENERATION_KEY: &str = "generation";
const SEEDED_PATHS_KEY: &str = "seeded_paths";

/// Persistent handoff metadata stored inside a session database.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetadata {
    /// Monotonic counter incremented by every successful pack.
    pub generation: u64,
    /// Paths materialized by the future seed command.
    pub seeded_paths: Vec<String>,
}

impl Vfs {
    /// Read persistent session handoff metadata.
    pub async fn session_metadata(&self) -> Result<SessionMetadata> {
        let conn = self.pool.get_connection().await?;
        read_session_metadata(&conn).await
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

    /// Checkpoint and compact a local database into a new single-file artifact.
    pub async fn compact_local_database_into(&self, output: &Path) -> Result<()> {
        self.fs.finalize().await?;
        let conn = self.pool.get_connection().await?;
        conn.execute("PRAGMA synchronous = FULL", ()).await?;

        let compact_result = async {
            checkpoint_truncate(&conn).await?;
            let escaped_output = output.to_string_lossy().replace('\'', "''");
            conn.execute(&format!("VACUUM INTO '{escaped_output}'"), ())
                .await?;
            Ok::<(), Error>(())
        }
        .await;

        conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
        compact_result?;
        drop(conn);
        self.fs.finalize().await?;
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
    async fn metadata_defaults_and_generation_is_monotonic() -> Result<()> {
        let dir = tempdir()?;
        let db_path = dir.path().join("session.db");
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;

        assert_eq!(vfs.session_metadata().await?, SessionMetadata::default());
        assert_eq!(vfs.increment_session_generation().await?.generation, 1);
        assert_eq!(vfs.increment_session_generation().await?.generation, 2);

        let seeded_paths = vec!["/src/lib.rs".to_string(), "/Cargo.toml".to_string()];
        vfs.set_seeded_paths(&seeded_paths).await?;
        assert_eq!(
            vfs.session_metadata().await?,
            SessionMetadata {
                generation: 2,
                seeded_paths: seeded_paths.clone(),
            }
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
}
