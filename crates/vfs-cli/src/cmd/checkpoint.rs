//! Publish a consistent run-session checkpoint to the configured remote tier.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use futures_util::{stream, StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use vfs_core::{schema, CoreConfig, Vfs, VfsOptions};

use super::branch::SnapshotCleanup;
use super::remote::{
    chunk_key, manifest_key, metadata_key, RemoteConfig, RemoteManifest, RemoteMetadata,
    RemoteStore,
};

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckpointReport {
    session_id: String,
    seq: i64,
    history_epoch: i64,
    history_valid: bool,
    generation: u64,
    manifest_key: String,
    metadata_sha256: String,
    uploaded_chunks: u64,
    reused_chunks: u64,
    uploaded_bytes: u64,
    chunk_count: u64,
    chunk_bytes: u64,
    vfs_version: String,
}

#[derive(Debug, Clone)]
struct Chunk {
    digest: [u8; 32],
    bytes: u64,
}

/// Publish one consistent session point and report the committed manifest.
pub async fn handle_checkpoint_command(
    stdout: &mut impl Write,
    session_id: String,
    json: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    checkpoint_session(
        stdout,
        &home,
        session_id,
        json,
        crate::config::remote_config(),
        crate::config::core_config_from_env(),
    )
    .await
}

async fn checkpoint_session(
    stdout: &mut impl Write,
    home: &Path,
    session_id: String,
    json: bool,
    remote_config: Option<RemoteConfig>,
    core_config: CoreConfig,
) -> Result<()> {
    let remote_config =
        remote_config.context("vfs checkpoint requires VFS_REMOTE_URL to be configured")?;
    let remote = RemoteStore::new(&remote_config.url)?;
    let paths = super::run::session_paths(home, &session_id)?;
    if !paths.run_dir.is_dir() {
        bail!("session not found: {session_id}");
    }
    let base_path = super::run::read_session_base_path(&paths)?;
    refuse_encrypted_session(&paths.db_path)?;
    refuse_hollow_session(&paths.db_path).await?;

    let staging = paths
        .run_dir
        .join(format!(".checkpoint-{}.tmp", Uuid::new_v4()));
    let _cleanup = SnapshotCleanup::armed(staging.clone());
    super::branch::snapshot_parent_with_config(&paths, &staging, core_config.clone())
        .await
        .context("Failed to snapshot the session for checkpointing")?;

    let snapshot_history_valid = snapshot_history_valid(&staging).await?;
    let mut staging_config = core_config;
    staging_config.journal_enabled = snapshot_history_valid;
    let staging_path = staging
        .to_str()
        .with_context(|| format!("Checkpoint path is not UTF-8: {}", staging.display()))?;
    let vfs = Vfs::open(VfsOptions::with_path(staging_path).with_core_config(staging_config))
        .await
        .map_err(|error| super::migrate::open_error_with_guidance(error, staging_path))
        .context("Failed to open the staged checkpoint")?;

    super::pack::materialize_branch_staging(home, &base_path, &vfs)
        .await
        .context("Failed to materialize the checkpoint parent chain")?;

    let head_seq = newest_journal_seq(&vfs).await?;
    let history = vfs
        .history_status()
        .await
        .context("Failed to read checkpoint history status")?;
    let generation = vfs
        .session_status_metadata()
        .await
        .context("Failed to read checkpoint generation")?
        .generation;
    let seed_pin = vfs
        .seed_pin()
        .await
        .context("Failed to read checkpoint seed pin")?;
    let chunks = enumerate_chunks(&vfs).await?;
    let chunk_count = u64::try_from(chunks.len()).context("Chunk count exceeds u64")?;
    let chunk_bytes = chunks.iter().try_fold(0_u64, |total, chunk| {
        total
            .checked_add(chunk.bytes)
            .context("Checkpoint chunk byte count overflow")
    })?;

    let existing = remote
        .list("chunks")
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
    let missing = chunks
        .iter()
        .filter(|chunk| !existing.contains(&chunk_key(&chunk.digest)))
        .cloned()
        .collect::<Vec<_>>();
    let uploaded_chunks = u64::try_from(missing.len()).context("Chunk count exceeds u64")?;
    let reused_chunks = chunk_count - uploaded_chunks;
    let uploaded_bytes =
        upload_missing_chunks(&remote, &vfs, missing, remote_config.concurrency).await?;

    let conn = vfs
        .get_connection()
        .await
        .context("Failed to connect to staged checkpoint")?;
    let hollow = schema::hollow_chunks(&conn)
        .await
        .context("Failed to hollow checkpoint metadata")?;
    drop(conn);
    if hollow.chunks != chunk_count || hollow.bytes != chunk_bytes {
        bail!(
            "checkpoint chunk totals changed during publication: expected {chunk_count} chunks/{chunk_bytes} bytes, found {}/{}",
            hollow.chunks,
            hollow.bytes
        );
    }
    vfs.fs
        .finalize()
        .await
        .context("Failed to checkpoint staged metadata")?;
    drop(vfs);
    super::safety::remove_sqlite_sidecars_after_checkpoint(&staging)?;
    super::pack::sync_file_and_parent(&staging)?;

    let metadata_bytes = fs::read(&staging)
        .with_context(|| format!("Failed to read staged metadata {}", staging.display()))?;
    let metadata_sha256 = hex::encode(Sha256::digest(&metadata_bytes));
    let metadata_bytes_len =
        u64::try_from(metadata_bytes.len()).context("Metadata size exceeds u64")?;
    let metadata_object_key = metadata_key(&session_id, &metadata_sha256);
    remote
        .put(&metadata_object_key, Bytes::from(metadata_bytes))
        .await?;

    let manifest_object_key = manifest_key(&session_id);
    let manifest = RemoteManifest {
        session_id: session_id.clone(),
        head_seq,
        history_epoch: history.epoch,
        history_valid: history.valid,
        generation,
        artifact_version: schema::CURRENT.as_str().to_string(),
        seed_pin,
        metadata: RemoteMetadata {
            key: metadata_object_key,
            sha256: metadata_sha256.clone(),
            bytes: metadata_bytes_len,
        },
        chunk_count,
        chunk_bytes,
        created_at_ms: now_ms()?,
        vfs_version: super::version::VERSION.to_string(),
    };
    let manifest_bytes = manifest.to_json()?.into_bytes();
    remote
        .put(&manifest_object_key, Bytes::from(manifest_bytes.clone()))
        .await?;
    let committed = remote.get(&manifest_object_key).await?;
    if committed.as_ref() != manifest_bytes.as_slice() {
        bail!("remote manifest read-back did not match the committed checkpoint");
    }

    let report = CheckpointReport {
        session_id,
        seq: head_seq,
        history_epoch: history.epoch,
        history_valid: history.valid,
        generation,
        manifest_key: manifest_object_key,
        metadata_sha256,
        uploaded_chunks,
        reused_chunks,
        uploaded_bytes,
        chunk_count,
        chunk_bytes,
        vfs_version: super::version::VERSION.to_string(),
    };
    write_report(stdout, &report, json)
}

fn refuse_encrypted_session(db_path: &Path) -> Result<()> {
    let mut file = fs::File::open(db_path)
        .with_context(|| format!("Failed to open session database {}", db_path.display()))?;
    let mut header = [0_u8; SQLITE_HEADER.len()];
    file.read_exact(&mut header).with_context(|| {
        format!(
            "session database {} is too short to identify",
            db_path.display()
        )
    })?;
    if &header != SQLITE_HEADER {
        bail!(
            "vfs checkpoint does not support encrypted sessions; uploading plaintext chunks would weaken at-rest confidentiality"
        );
    }
    Ok(())
}

async fn refuse_hollow_session(db_path: &Path) -> Result<()> {
    let vfs = Vfs::open_read_only(db_path)
        .await
        .context("Failed to inspect session chunk storage before checkpointing")?;
    let conn = vfs
        .get_connection()
        .await
        .context("Failed to connect to session chunk storage")?;
    let hollow = schema::chunks_hollow(&conn)
        .await
        .context("Failed to inspect session chunk storage")?;
    if hollow {
        bail!("vfs checkpoint refuses hollow sessions; materialize the session first");
    }
    Ok(())
}

async fn newest_journal_seq(vfs: &Vfs) -> Result<i64> {
    let conn = vfs.get_connection().await?;
    let mut rows = conn
        .query(
            "SELECT seq FROM fs_op_journal ORDER BY seq DESC LIMIT 1",
            (),
        )
        .await?;
    Ok(rows
        .next()
        .await?
        .map(|row| row.get(0))
        .transpose()?
        .unwrap_or(0))
}

async fn snapshot_history_valid(staging: &Path) -> Result<bool> {
    let db = super::safety::build_local_database(staging, None)
        .await
        .context("Failed to inspect checkpoint history markers")?;
    let conn = db
        .connect()
        .context("Failed to connect to checkpoint history markers")?;
    let mut rows = conn
        .query(
            "SELECT value FROM fs_config WHERE key = ?",
            (schema::CONFIG_HISTORY_VALID_KEY,),
        )
        .await?;
    let valid = rows
        .next()
        .await?
        .map(|row| row.get::<String>(0))
        .transpose()?
        .as_deref()
        != Some("0");
    drop(rows);
    drop(conn);
    drop(db);
    Ok(valid)
}

async fn enumerate_chunks(vfs: &Vfs) -> Result<Vec<Chunk>> {
    let conn = vfs.get_connection().await?;
    let mut rows = conn
        .query(
            "SELECT digest, LENGTH(data) FROM fs_chunk ORDER BY digest",
            (),
        )
        .await?;
    let mut chunks = Vec::new();
    while let Some(row) = rows.next().await? {
        let digest = row.get::<Vec<u8>>(0)?;
        let length = digest.len();
        let digest = <[u8; 32]>::try_from(digest)
            .map_err(|_| anyhow::anyhow!("stored chunk digest has length {length}, expected 32"))?;
        let bytes =
            u64::try_from(row.get::<i64>(1)?).context("stored chunk has a negative byte length")?;
        chunks.push(Chunk { digest, bytes });
    }
    Ok(chunks)
}

async fn upload_missing_chunks(
    remote: &RemoteStore,
    vfs: &Vfs,
    chunks: Vec<Chunk>,
    concurrency: usize,
) -> Result<u64> {
    stream::iter(chunks.into_iter().map(|chunk| async move {
        let conn = vfs.get_connection().await?;
        let mut rows = conn
            .query(
                "SELECT data FROM fs_chunk WHERE digest = ?",
                (chunk.digest.as_slice(),),
            )
            .await?;
        let bytes = rows
            .next()
            .await?
            .context("chunk disappeared while checkpointing")?
            .get::<Vec<u8>>(0)?;
        if blake3::hash(&bytes).as_bytes() != &chunk.digest {
            bail!(
                "stored chunk {} does not match its BLAKE3 digest",
                hex::encode(chunk.digest)
            );
        }
        let bytes_len = u64::try_from(bytes.len()).context("Chunk size exceeds u64")?;
        if bytes_len != chunk.bytes {
            bail!(
                "chunk {} changed size while checkpointing",
                hex::encode(chunk.digest)
            );
        }
        drop(rows);
        drop(conn);
        remote
            .put(&chunk_key(&chunk.digest), Bytes::from(bytes))
            .await?;
        Ok::<u64, anyhow::Error>(bytes_len)
    }))
    .buffer_unordered(concurrency)
    .try_fold(0_u64, |total, bytes| async move {
        total
            .checked_add(bytes)
            .context("Uploaded byte count overflow")
    })
    .await
}

fn now_ms() -> Result<i64> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    i64::try_from(millis).context("Current time exceeds i64 milliseconds")
}

fn write_report(stdout: &mut impl Write, report: &CheckpointReport, json: bool) -> Result<()> {
    if json {
        serde_json::to_writer(&mut *stdout, report)?;
        writeln!(stdout)?;
        return Ok(());
    }
    writeln!(stdout, "Session: {}", report.session_id)?;
    writeln!(stdout, "History epoch: {}", report.history_epoch)?;
    writeln!(stdout, "History valid: {}", report.history_valid)?;
    writeln!(stdout, "Generation: {}", report.generation)?;
    writeln!(stdout, "Manifest: {}", report.manifest_key)?;
    writeln!(stdout, "Metadata SHA-256: {}", report.metadata_sha256)?;
    writeln!(stdout, "Uploaded chunks: {}", report.uploaded_chunks)?;
    writeln!(stdout, "Reused chunks: {}", report.reused_chunks)?;
    writeln!(stdout, "Uploaded bytes: {}", report.uploaded_bytes)?;
    writeln!(stdout, "Chunk count: {}", report.chunk_count)?;
    writeln!(stdout, "Chunk bytes: {}", report.chunk_bytes)?;
    writeln!(stdout, "Vfs version: {}", report.vfs_version)?;
    writeln!(stdout, "Seq: {}", report.seq)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use tempfile::TempDir;
    use turso::Builder;
    use url::Url;
    use vfs_core::{chunks_hollow, hydrate_chunks, ChunkSource, DEFAULT_FILE_MODE};

    use super::*;

    struct Fixture {
        home: TempDir,
        remote: TempDir,
        _base: TempDir,
        session_id: String,
        pre_hollow_chunks: BTreeMap<[u8; 32], Vec<u8>>,
        expected_seq: i64,
    }

    struct FileRemoteSource {
        store: RemoteStore,
    }

    #[async_trait]
    impl ChunkSource for FileRemoteSource {
        async fn fetch(&self, digest: &[u8; 32]) -> vfs_core::error::Result<Vec<u8>> {
            self.store
                .get(&chunk_key(digest))
                .await
                .map(|bytes| bytes.to_vec())
                .map_err(|error| vfs_core::error::Error::Internal(error.to_string()))
        }
    }

    fn test_core_config(journal_enabled: bool) -> CoreConfig {
        let mut config = CoreConfig::default();
        config.batcher.enabled = false;
        config.journal_enabled = journal_enabled;
        config
    }

    fn remote_config(remote: &TempDir) -> RemoteConfig {
        RemoteConfig {
            url: Url::from_directory_path(remote.path()).unwrap().to_string(),
            concurrency: 2,
            stream_interval_ms: 0,
        }
    }

    async fn fixture(journal_enabled: bool) -> Result<Fixture> {
        let home = tempfile::tempdir()?;
        let remote = tempfile::tempdir()?;
        let base = tempfile::tempdir()?;
        let session_id = "checkpoint-test".to_string();
        let paths = super::super::run::SessionPaths::new(home.path(), &session_id);
        fs::create_dir_all(&paths.run_dir)?;
        fs::write(
            &paths.base_path_file,
            base.path().to_string_lossy().as_bytes(),
        )?;
        let vfs = Vfs::open(
            VfsOptions::with_path(paths.db_path.to_string_lossy())
                .with_core_config(test_core_config(journal_enabled)),
        )
        .await?;
        let content = vec![b'c'; 256 * 1024];
        let (_, file) = vfs
            .fs
            .create_file("/checkpoint.bin", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        file.pwrite(0, &content).await?;
        file.fsync().await?;
        drop(file);
        vfs.record_seed_state(&["/checkpoint.bin".to_string()], &[], "seed-pin-123")
            .await?;
        assert_eq!(vfs.increment_session_generation().await?.generation, 1);
        let expected_seq = newest_journal_seq(&vfs).await?;
        let pre_hollow_chunks = read_chunks(&vfs).await?;
        vfs.fs.finalize().await?;
        drop(vfs);
        Ok(Fixture {
            home,
            remote,
            _base: base,
            session_id,
            pre_hollow_chunks,
            expected_seq,
        })
    }

    async fn read_chunks(vfs: &Vfs) -> Result<BTreeMap<[u8; 32], Vec<u8>>> {
        let conn = vfs.get_connection().await?;
        let mut rows = conn
            .query("SELECT digest, data FROM fs_chunk ORDER BY digest", ())
            .await?;
        let mut chunks = BTreeMap::new();
        while let Some(row) = rows.next().await? {
            chunks.insert(
                row.get::<Vec<u8>>(0)?.try_into().unwrap(),
                row.get::<Vec<u8>>(1)?,
            );
        }
        Ok(chunks)
    }

    async fn run_checkpoint(fixture: &Fixture, journal_enabled: bool) -> Result<CheckpointReport> {
        let mut output = Vec::new();
        checkpoint_session(
            &mut output,
            fixture.home.path(),
            fixture.session_id.clone(),
            true,
            Some(remote_config(&fixture.remote)),
            test_core_config(journal_enabled),
        )
        .await?;
        Ok(serde_json::from_slice(&output)?)
    }

    #[tokio::test]
    async fn offline_checkpoint_publishes_verified_hollow_round_trip() -> Result<()> {
        let fixture = fixture(true).await?;
        let report = run_checkpoint(&fixture, true).await?;
        assert_eq!(report.seq, fixture.expected_seq);
        assert_eq!(report.generation, 1);
        assert!(report.history_valid);
        assert_eq!(report.uploaded_chunks, report.chunk_count);
        assert_eq!(report.reused_chunks, 0);
        assert_eq!(report.uploaded_bytes, report.chunk_bytes);

        let store = RemoteStore::new(&remote_config(&fixture.remote).url)?;
        let manifest_bytes = store.get(&report.manifest_key).await?;
        let manifest = RemoteManifest::from_json(std::str::from_utf8(&manifest_bytes)?)?;
        assert_eq!(manifest.session_id, fixture.session_id);
        assert_eq!(manifest.head_seq, fixture.expected_seq);
        assert_eq!(manifest.generation, 1);
        assert_eq!(manifest.seed_pin.as_deref(), Some("seed-pin-123"));
        assert!(manifest.history_valid);
        assert_eq!(manifest.chunk_count, report.chunk_count);
        assert_eq!(manifest.chunk_bytes, report.chunk_bytes);

        for (digest, expected) in &fixture.pre_hollow_chunks {
            let bytes = store.get(&chunk_key(digest)).await?;
            assert_eq!(blake3::hash(&bytes).as_bytes(), digest);
            assert_eq!(bytes.as_ref(), expected);
        }

        let metadata = store.get(&manifest.metadata.key).await?;
        assert_eq!(
            hex::encode(Sha256::digest(&metadata)),
            manifest.metadata.sha256
        );
        assert_eq!(metadata.len() as u64, manifest.metadata.bytes);
        let inspect_path = fixture.home.path().join("inspect-hollow.db");
        fs::write(&inspect_path, &metadata)?;
        let inspect = Vfs::open_read_only(&inspect_path).await?;
        let inspect_conn = inspect.get_connection().await?;
        assert!(chunks_hollow(&inspect_conn).await?);
        drop(inspect_conn);
        drop(inspect);

        let refuse_path = fixture.home.path().join("refuse-hollow.db");
        fs::write(&refuse_path, &metadata)?;
        let error = match Vfs::open(VfsOptions::with_path(refuse_path.to_string_lossy())).await {
            Ok(_) => panic!("hollow metadata must refuse writable open"),
            Err(error) => error,
        };
        assert!(matches!(error, vfs_core::error::Error::ChunksHollow));

        let hydrate_path = fixture.home.path().join("hydrate.db");
        fs::write(&hydrate_path, &metadata)?;
        let db = Builder::new_local(&hydrate_path.to_string_lossy())
            .build()
            .await?;
        let conn = db.connect()?;
        let source = FileRemoteSource { store };
        assert_eq!(
            hydrate_chunks(&conn, &source, 4).await?,
            fixture.pre_hollow_chunks.len() as u64
        );
        drop(conn);
        drop(db);
        let hydrated = Vfs::open(VfsOptions::with_path(hydrate_path.to_string_lossy())).await?;
        assert_eq!(read_chunks(&hydrated).await?, fixture.pre_hollow_chunks);
        Ok(())
    }

    #[tokio::test]
    async fn recheckpoint_reuses_every_chunk() -> Result<()> {
        let fixture = fixture(true).await?;
        let first = run_checkpoint(&fixture, true).await?;
        let second = run_checkpoint(&fixture, true).await?;
        assert_eq!(second.uploaded_chunks, 0);
        assert_eq!(second.uploaded_bytes, 0);
        assert_eq!(second.reused_chunks, first.chunk_count);
        assert_eq!(second.chunk_count, first.chunk_count);
        assert_eq!(second.seq, first.seq);
        let store = RemoteStore::new(&remote_config(&fixture.remote).url)?;
        let committed = RemoteManifest::from_json(std::str::from_utf8(
            &store.get(&second.manifest_key).await?,
        )?)?;
        assert_eq!(committed.head_seq, second.seq);
        Ok(())
    }

    #[tokio::test]
    async fn unconfigured_remote_is_a_clear_error() -> Result<()> {
        let fixture = fixture(true).await?;
        let error = checkpoint_session(
            &mut Vec::new(),
            fixture.home.path(),
            fixture.session_id,
            true,
            None,
            test_core_config(true),
        )
        .await
        .expect_err("missing remote must fail");
        assert!(error.to_string().contains("VFS_REMOTE_URL"));
        Ok(())
    }

    #[tokio::test]
    async fn journal_disabled_checkpoint_preserves_invalid_history() -> Result<()> {
        let fixture = fixture(false).await?;
        let report = run_checkpoint(&fixture, false).await?;
        assert!(!report.history_valid);
        let store = RemoteStore::new(&remote_config(&fixture.remote).url)?;
        let manifest = RemoteManifest::from_json(std::str::from_utf8(
            &store.get(&report.manifest_key).await?,
        )?)?;
        assert!(!manifest.history_valid);
        assert_eq!(manifest.head_seq, report.seq);
        Ok(())
    }

    #[tokio::test]
    async fn encrypted_session_refuses_before_uploading_objects() -> Result<()> {
        let home = tempfile::tempdir()?;
        let remote = tempfile::tempdir()?;
        let base = tempfile::tempdir()?;
        let session_id = "encrypted-checkpoint".to_string();
        let paths = super::super::run::SessionPaths::new(home.path(), &session_id);
        fs::create_dir_all(&paths.run_dir)?;
        fs::write(
            &paths.base_path_file,
            base.path().to_string_lossy().as_bytes(),
        )?;
        let key = "11".repeat(32);
        let vfs = Vfs::open(
            VfsOptions::with_path(paths.db_path.to_string_lossy())
                .with_encryption_key(&key, "aegis256")
                .with_core_config(test_core_config(true)),
        )
        .await?;
        vfs.fs.finalize().await?;
        drop(vfs);

        let config = remote_config(&remote);
        let error = checkpoint_session(
            &mut Vec::new(),
            home.path(),
            session_id,
            true,
            Some(config.clone()),
            test_core_config(true),
        )
        .await
        .expect_err("encrypted checkpoint must fail");
        assert!(error.to_string().contains("encrypted sessions"));
        assert!(RemoteStore::new(&config.url)?.list("").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn hollow_session_refuses_before_uploading_objects() -> Result<()> {
        let fixture = fixture(true).await?;
        let db_path =
            super::super::run::SessionPaths::new(fixture.home.path(), &fixture.session_id).db_path;
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        schema::hollow_chunks(&conn).await?;
        drop(conn);
        drop(db);

        let config = remote_config(&fixture.remote);
        let error = checkpoint_session(
            &mut Vec::new(),
            fixture.home.path(),
            fixture.session_id,
            true,
            Some(config.clone()),
            test_core_config(true),
        )
        .await
        .expect_err("hollow checkpoint must fail");
        assert!(error.to_string().contains("materialize the session first"));
        assert!(RemoteStore::new(&config.url)?.list("").await?.is_empty());
        Ok(())
    }
}
