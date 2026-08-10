//! First-class installation of externally transferred run sessions.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use vfs_core::{schema, Vfs, VfsOptions};

use super::pack::SessionStillRunning;
use super::remote::{manifest_key, RemoteChunkSource, RemoteManifest, RemoteStore};
use super::safety::{build_local_database, remove_sqlite_sidecars_after_checkpoint};

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdoptManifest {
    manifest_version: u32,
    session_id: String,
    base_path: PathBuf,
    base_pin: String,
    generation: u64,
    schema_version: String,
    seeded_paths: Vec<String>,
    vfs_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<bool>,
}

enum AdoptInput {
    Local(PathBuf),
    Remote(String),
}

/// Install a transferred session artifact and print its machine-readable manifest.
pub async fn handle_adopt_command(
    stdout: &mut impl Write,
    session_id: String,
    db: Option<PathBuf>,
    remote: bool,
    base: PathBuf,
    pin: Option<String>,
    _json: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    let input = match (db, remote) {
        (Some(db), false) => AdoptInput::Local(db),
        (None, true) => AdoptInput::Remote(
            crate::config::remote_config()
                .context("vfs adopt --remote requires VFS_REMOTE_URL to be configured")?
                .url,
        ),
        _ => bail!("exactly one of --db or --remote is required"),
    };
    adopt_session(stdout, &home, session_id, input, base, pin).await
}

async fn adopt_session(
    stdout: &mut impl Write,
    home: &Path,
    session_id: String,
    input: AdoptInput,
    base: PathBuf,
    pin: Option<String>,
) -> Result<()> {
    if !VfsOptions::validate_agent_id(&session_id) {
        bail!("invalid session ID: {session_id}");
    }
    let input = match input {
        AdoptInput::Local(db) => {
            let source_db = std::path::absolute(&db).context("Failed to resolve --db path")?;
            if !source_db.is_file() {
                bail!("session artifact not found: {}", source_db.display());
            }
            AdoptInput::Local(source_db)
        }
        remote => remote,
    };
    let base_path = std::path::absolute(&base).context("Failed to resolve --base path")?;
    if !base_path.is_dir() {
        bail!(
            "base checkout does not exist or is not a directory: {}",
            base_path.display()
        );
    }
    super::seed::ensure_git_repository(&base_path)?;

    let paths = super::run::SessionPaths::new(home, &session_id);
    let session_dir = paths.run_dir.clone();
    let db_path = paths.db_path.clone();
    if db_path.is_file() {
        bail!("session already exists: {}", session_dir.display());
    }
    let mut cleanup = SessionDirCleanup::new(&session_dir);
    fs::create_dir_all(&session_dir).context("Failed to create run session directory")?;
    let _session_lock =
        super::session_lock::SessionLock::try_exclusive(&session_dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::Error::new(SessionStillRunning)
            } else {
                anyhow::Error::new(error).context("Failed to lock session for adoption")
            }
        })?;
    super::pack::recover_interrupted_publication(&db_path)?;
    super::revert::recover_interrupted_publication(&db_path)?;
    if db_path.is_file() {
        bail!("session already exists: {}", session_dir.display());
    }
    super::pack::ensure_session_inactive(&paths)?;

    let staging =
        StagedArtifact::new(session_dir.join(format!(".delta.db.adopt-{}.tmp", Uuid::new_v4())));
    let (remote_manifest, remote_url) = match input {
        AdoptInput::Local(source_db) => {
            super::pack::copy_database_family(&source_db, staging.path())
                .context("Failed to stage the session artifact")?;
            (None, None)
        }
        AdoptInput::Remote(remote_url) => {
            let remote = RemoteStore::new(&remote_url)?;
            let (manifest, metadata) = fetch_verified_remote_input(&remote, &session_id).await?;
            fs::write(staging.path(), metadata)
                .context("Failed to stage the remote session metadata")?;
            super::pack::sync_file_and_parent(staging.path())?;
            (Some(manifest), Some(remote_url))
        }
    };
    verify_artifact_integrity(staging.path()).await?;
    let artifact_version = migrate_artifact_to_current(staging.path()).await?;

    let mut options = VfsOptions::with_path(staging.path().to_string_lossy());
    if let Some(remote_url) = remote_url.as_deref() {
        options = options.with_chunk_source(Arc::new(RemoteChunkSource::new(remote_url)?));
    }
    let vfs = Vfs::open(options)
        .await
        .context("Failed to open the staged session artifact")?;
    let metadata = vfs.session_metadata().await?;
    let recorded_pin = vfs.seed_pin().await?;
    if let Some(manifest) = remote_manifest.as_ref() {
        verify_remote_manifest_against_db(&vfs, manifest, &metadata, recorded_pin.as_deref())
            .await?;
    }
    vfs.fs
        .finalize()
        .await
        .context("Failed to finalize the staged session artifact")?;
    drop(vfs);
    remove_sqlite_sidecars_after_checkpoint(staging.path())?;

    let base_pin = resolve_expected_pin(&base_path, recorded_pin, pin)?;
    let head = super::seed::resolve_commit(&base_path, "HEAD")
        .context("base checkout has no valid HEAD")?;
    if head != base_pin {
        bail!(
            "base checkout {} is at {head}, but the session requires pin {base_pin}; \
             check out the pin before adopting",
            base_path.display()
        );
    }

    let base_path_file = &paths.base_path_file;
    fs::write(base_path_file, base_path.to_string_lossy().as_bytes())
        .context("Failed to publish session base path")?;
    super::pack::sync_file_and_parent(base_path_file)?;
    if let Some(remote_url) = remote_url.as_deref() {
        super::remote::write_remote_url(&session_dir, remote_url)?;
    }
    fs::rename(staging.path(), &db_path).with_context(|| {
        format!(
            "Failed to install adopted session database {}",
            db_path.display()
        )
    })?;
    super::pack::sync_file_and_parent(&db_path)?;
    cleanup.disarm();

    let manifest = AdoptManifest {
        manifest_version: 1,
        session_id,
        base_path,
        base_pin,
        generation: metadata.generation,
        schema_version: artifact_version.to_string(),
        seeded_paths: metadata.seeded_paths,
        vfs_version: super::version::VERSION.to_string(),
        remote: remote_url.as_ref().map(|_| true),
    };
    serde_json::to_writer(&mut *stdout, &manifest)?;
    writeln!(stdout)?;
    Ok(())
}

async fn fetch_verified_remote_input(
    remote: &RemoteStore,
    session_id: &str,
) -> Result<(RemoteManifest, Vec<u8>)> {
    let manifest_bytes = remote
        .get(&manifest_key(session_id))
        .await
        .with_context(|| format!("Failed to fetch remote manifest for session {session_id}"))?;
    let manifest_json = std::str::from_utf8(&manifest_bytes)
        .context("Remote session manifest is not valid UTF-8")?;
    let manifest = RemoteManifest::from_json(manifest_json)?;
    if manifest.session_id != session_id {
        bail!(
            "remote manifest session ID {:?} does not match requested session {session_id:?}",
            manifest.session_id
        );
    }
    verify_manifest_artifact_version(&manifest.artifact_version)?;

    let metadata = remote
        .get(&manifest.metadata.key)
        .await
        .with_context(|| {
            format!(
                "Failed to fetch remote session metadata {:?}",
                manifest.metadata.key
            )
        })?
        .to_vec();
    let actual_bytes = u64::try_from(metadata.len()).context("Remote metadata size exceeds u64")?;
    if actual_bytes != manifest.metadata.bytes {
        bail!(
            "remote session metadata length mismatch: manifest declares {} bytes, fetched {actual_bytes}",
            manifest.metadata.bytes
        );
    }
    let actual_sha256 = hex::encode(Sha256::digest(&metadata));
    if actual_sha256 != manifest.metadata.sha256 {
        bail!(
            "remote session metadata SHA-256 mismatch: manifest declares {}, fetched {actual_sha256}",
            manifest.metadata.sha256
        );
    }
    Ok((manifest, metadata))
}

fn verify_manifest_artifact_version(marker: &str) -> Result<schema::SchemaVersion> {
    let Some(version) = schema::SchemaVersion::parse(marker) else {
        if numeric_version(marker)
            .zip(numeric_version(schema::CURRENT.as_str()))
            .is_some_and(|(found, current)| found > current)
        {
            bail!(
                "remote manifest requires artifact version {marker}, newer than the newest supported artifact version {} (vfs {}); upgrade vfs to adopt it",
                schema::CURRENT,
                super::version::VERSION
            );
        }
        bail!(
            "remote manifest artifact version {marker:?} is unsupported by vfs {}; supported range is {} through {}",
            super::version::VERSION,
            schema::MIN_SUPPORTED,
            schema::CURRENT
        );
    };
    if version < schema::MIN_SUPPORTED {
        bail!(
            "remote manifest artifact version {version} is older than the oldest supported artifact version {}; create a newer checkpoint",
            schema::MIN_SUPPORTED
        );
    }
    if version > schema::CURRENT {
        bail!(
            "remote manifest requires artifact version {version}, newer than the newest supported artifact version {} (vfs {}); upgrade vfs to adopt it",
            schema::CURRENT,
            super::version::VERSION
        );
    }
    Ok(version)
}

fn numeric_version(marker: &str) -> Option<(u64, u64)> {
    let (major, minor) = marker.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

async fn verify_remote_manifest_against_db(
    vfs: &Vfs,
    manifest: &RemoteManifest,
    metadata: &vfs_core::SessionMetadata,
    seed_pin: Option<&str>,
) -> Result<()> {
    if manifest.generation != metadata.generation {
        bail!(
            "remote manifest generation {} does not match staged database generation {}",
            manifest.generation,
            metadata.generation
        );
    }
    if manifest.seed_pin.as_deref() != seed_pin {
        bail!(
            "remote manifest seed pin {:?} does not match staged database seed pin {:?}",
            manifest.seed_pin,
            seed_pin
        );
    }

    let history = vfs
        .history_status()
        .await
        .context("Failed to read staged database history status")?;
    if manifest.history_epoch != history.epoch || manifest.history_valid != history.valid {
        bail!(
            "remote manifest history claims do not match staged database: manifest epoch {}/valid {}, database epoch {}/valid {}",
            manifest.history_epoch,
            manifest.history_valid,
            history.epoch,
            history.valid
        );
    }

    let conn = vfs
        .get_connection()
        .await
        .context("Failed to inspect staged remote metadata")?;
    let mut rows = conn
        .query(
            "SELECT seq FROM fs_op_journal ORDER BY seq DESC LIMIT 1",
            (),
        )
        .await?;
    let head_seq = rows
        .next()
        .await?
        .map(|row| row.get::<i64>(0))
        .transpose()?
        .unwrap_or(0);
    drop(rows);
    if manifest.head_seq != head_seq {
        bail!(
            "remote manifest head sequence {} does not match staged database head sequence {head_seq}",
            manifest.head_seq
        );
    }

    let mut rows = conn.query("SELECT COUNT(*) FROM fs_chunk", ()).await?;
    let chunk_count = rows
        .next()
        .await?
        .context("staged database chunk count query returned no row")?
        .get::<i64>(0)?;
    let chunk_count =
        u64::try_from(chunk_count).context("staged database has a negative chunk count")?;
    if manifest.chunk_count != chunk_count {
        bail!(
            "remote manifest chunk count {} does not match staged database chunk count {chunk_count}",
            manifest.chunk_count
        );
    }
    // Hollow metadata intentionally omits chunk bytes, so chunkBytes cannot be
    // recomputed locally; every fetched object is verified by the core resolver.
    Ok(())
}

/// Removes a session directory this adopt created if installation fails, so
/// a failed adopt never leaves a half-materialized session behind.
struct SessionDirCleanup {
    session_dir: PathBuf,
    armed: bool,
}

impl SessionDirCleanup {
    fn new(session_dir: &Path) -> Self {
        Self {
            session_dir: session_dir.to_path_buf(),
            armed: !session_dir.exists(),
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionDirCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.session_dir);
        }
    }
}

struct StagedArtifact {
    path: PathBuf,
}

impl StagedArtifact {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedArtifact {
    fn drop(&mut self) {
        super::pack::remove_database_family(&self.path);
    }
}

async fn verify_artifact_integrity(staging: &Path) -> Result<()> {
    let db = build_local_database(staging, None)
        .await
        .context("Failed to open the session artifact")?;
    let conn = db
        .connect()
        .context("Failed to connect to the session artifact")?;
    let mut rows = conn
        .query("PRAGMA integrity_check", ())
        .await
        .context("Failed to check session artifact integrity")?;
    let mut results = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .context("Failed to read session artifact integrity results")?
    {
        results.push(row.get::<String>(0)?);
    }
    if results != ["ok".to_string()] {
        bail!("session artifact failed integrity check: {results:?}");
    }
    Ok(())
}

/// Lands any supported artifact schema at [`schema::CURRENT`] so the adopted
/// session opens under `vfs run` without a separate migrate step, mirroring
/// the staging migration `vfs pack` performs before it ships an artifact.
async fn migrate_artifact_to_current(staging: &Path) -> Result<schema::SchemaVersion> {
    let db = build_local_database(staging, None)
        .await
        .context("Failed to open the session artifact for migration")?;
    let conn = db
        .connect()
        .context("Failed to connect to the session artifact for migration")?;
    if let Err(error) = schema::detect_schema_version(&conn).await {
        if let vfs_core::error::Error::SchemaVersionMismatch { found, expected } = &error {
            bail!(
                "session artifact requires schema {found}, newer than the newest supported \
                 artifact version {expected} (vfs {}); upgrade vfs to adopt it",
                super::version::VERSION
            );
        }
        return Err(anyhow::Error::new(error).context("Failed to detect artifact schema version"));
    }
    schema::ensure_current(&conn)
        .await
        .context("Failed to migrate the session artifact to the current schema")?;
    Ok(schema::CURRENT)
}

fn resolve_expected_pin(
    base_path: &Path,
    recorded_pin: Option<String>,
    requested_pin: Option<String>,
) -> Result<String> {
    let requested = requested_pin
        .map(|requested| {
            super::seed::resolve_commit(base_path, &requested)
                .with_context(|| format!("invalid adopt pin: {requested}"))
        })
        .transpose()?;
    match (recorded_pin, requested) {
        (Some(recorded), Some(requested)) if recorded != requested => bail!(
            "requested pin {requested} does not match the artifact's recorded seed pin {recorded}"
        ),
        (Some(recorded), _) => Ok(recorded),
        (None, Some(requested)) => Ok(requested),
        (None, None) => bail!(
            "session artifact does not record a seed pin; pass --pin <COMMIT> to verify the \
             base checkout"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use bytes::Bytes;
    use tempfile::{tempdir, TempDir};
    use turso::Builder;
    use url::Url;
    use vfs_core::OverlayFS;

    use super::*;

    struct AdoptFixture {
        _root: TempDir,
        home: PathBuf,
        sender_repo: PathBuf,
        receiver_base: PathBuf,
        pin: String,
        head: String,
    }

    impl AdoptFixture {
        fn new() -> Result<Self> {
            let root = tempdir()?;
            let home = root.path().join("home");
            let sender_repo = root.path().join("sender");
            let receiver_base = root.path().join("receiver");
            fs::create_dir_all(&home)?;
            fs::create_dir_all(&sender_repo)?;
            git(&sender_repo, &["init", "-b", "main"])?;
            git(&sender_repo, &["config", "user.name", "Vfs Adopt Test"])?;
            git(&sender_repo, &["config", "user.email", "adopt@example.com"])?;
            fs::write(sender_repo.join("tracked.txt"), "pin content\n")?;
            git(&sender_repo, &["add", "."])?;
            git(&sender_repo, &["commit", "-m", "pin"])?;
            let pin = git_text(&sender_repo, &["rev-parse", "HEAD"])?;
            fs::write(sender_repo.join("tracked.txt"), "post-pin content\n")?;
            git(&sender_repo, &["commit", "-am", "post pin"])?;
            let head = git_text(&sender_repo, &["rev-parse", "HEAD"])?;

            git(
                root.path(),
                &[
                    "clone",
                    "--quiet",
                    sender_repo.to_str().unwrap(),
                    receiver_base.to_str().unwrap(),
                ],
            )?;
            git(&receiver_base, &["checkout", "--quiet", &pin])?;

            Ok(Self {
                _root: root,
                home,
                sender_repo,
                receiver_base,
                pin,
                head,
            })
        }

        async fn create_artifact(&self, name: &str, pin: Option<&str>) -> Result<PathBuf> {
            let scratch = self._root.path().join(format!("{name}-scratch.db"));
            let artifact = self._root.path().join(format!("{name}.db"));
            let vfs = Vfs::open(VfsOptions::with_path(scratch.to_string_lossy())).await?;
            let conn = vfs.get_connection().await?;
            OverlayFS::init_schema(&conn, self.sender_repo.to_string_lossy().as_ref()).await?;
            drop(conn);
            if let Some(pin) = pin {
                vfs.record_seed_state(&["tracked.txt".to_string()], &[], pin)
                    .await?;
            }
            vfs.compact_local_database_into(&artifact).await?;
            vfs.fs.finalize().await?;
            Ok(artifact)
        }

        fn session_dir(&self, session_id: &str) -> PathBuf {
            self.home.join(".vfs").join("run").join(session_id)
        }
    }

    async fn adopt(
        fixture: &AdoptFixture,
        session_id: &str,
        artifact: &Path,
        pin: Option<&str>,
    ) -> Result<AdoptManifest> {
        let mut stdout = Vec::new();
        adopt_session(
            &mut stdout,
            &fixture.home,
            session_id.to_string(),
            AdoptInput::Local(artifact.to_path_buf()),
            fixture.receiver_base.clone(),
            pin.map(str::to_string),
        )
        .await?;
        Ok(serde_json::from_slice(&stdout)?)
    }

    #[tokio::test]
    async fn adopt_installs_a_transferred_session() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture.create_artifact("happy", Some(&fixture.pin)).await?;

        let manifest = adopt(&fixture, "adopt-happy", &artifact, None).await?;
        assert_eq!(manifest.manifest_version, 1);
        assert_eq!(manifest.session_id, "adopt-happy");
        assert_eq!(manifest.base_path, fixture.receiver_base);
        assert_eq!(manifest.base_pin, fixture.pin);
        assert_eq!(manifest.generation, 0);
        assert_eq!(manifest.schema_version, schema::CURRENT.as_str());
        assert_eq!(manifest.seeded_paths, vec!["tracked.txt".to_string()]);
        assert_eq!(manifest.vfs_version, super::super::version::VERSION);
        assert_eq!(manifest.remote, None);

        let session_dir = fixture.session_dir("adopt-happy");
        assert_eq!(
            fs::read_to_string(session_dir.join("base_path"))?,
            fixture.receiver_base.to_string_lossy()
        );
        let installed_db = session_dir.join("delta.db");
        assert!(installed_db.is_file());
        assert_eq!(fs::read(&installed_db)?, fs::read(&artifact)?);

        let installed = Vfs::open(VfsOptions::with_path(installed_db.to_string_lossy())).await?;
        assert_eq!(
            installed.session_metadata().await?.seeded_paths,
            vec!["tracked.txt".to_string()]
        );
        assert_eq!(
            installed.seed_pin().await?.as_deref(),
            Some(fixture.pin.as_str())
        );
        Ok(())
    }

    #[tokio::test]
    async fn adopt_refuses_an_existing_session() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture
            .create_artifact("exists", Some(&fixture.pin))
            .await?;
        let session_dir = fixture.session_dir("adopt-exists");
        fs::create_dir_all(&session_dir)?;
        fs::write(session_dir.join("delta.db"), b"existing session")?;

        let error = adopt(&fixture, "adopt-exists", &artifact, None)
            .await
            .expect_err("existing session must be refused");
        assert!(error.to_string().contains("session already exists"));
        assert_eq!(fs::read(session_dir.join("delta.db"))?, b"existing session");
        Ok(())
    }

    #[tokio::test]
    async fn adopt_refuses_a_pin_mismatch_without_partial_state() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture
            .create_artifact("mismatch", Some(&fixture.head))
            .await?;

        let error = adopt(&fixture, "adopt-mismatch", &artifact, None)
            .await
            .expect_err("pin mismatch must be refused");
        assert!(error.to_string().contains("requires pin"));
        assert!(error.to_string().contains(&fixture.head));
        assert!(!fixture.session_dir("adopt-mismatch").exists());
        Ok(())
    }

    #[tokio::test]
    async fn adopt_refuses_a_requested_pin_conflicting_with_provenance() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture
            .create_artifact("conflict", Some(&fixture.pin))
            .await?;

        let error = adopt(&fixture, "adopt-conflict", &artifact, Some(&fixture.head))
            .await
            .expect_err("conflicting pins must be refused");
        assert!(error
            .to_string()
            .contains("does not match the artifact's recorded seed pin"));
        assert!(!fixture.session_dir("adopt-conflict").exists());
        Ok(())
    }

    #[tokio::test]
    async fn adopt_requires_a_pin_for_artifacts_without_provenance() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture.create_artifact("no-pin", None).await?;

        let error = adopt(&fixture, "adopt-no-pin", &artifact, None)
            .await
            .expect_err("missing provenance without --pin must be refused");
        assert!(error.to_string().contains("pass --pin"));
        assert!(!fixture.session_dir("adopt-no-pin").exists());

        let manifest = adopt(&fixture, "adopt-no-pin", &artifact, Some(&fixture.pin)).await?;
        assert_eq!(manifest.base_pin, fixture.pin);
        assert!(manifest.seeded_paths.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn adopt_refuses_a_corrupt_artifact_without_partial_state() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture._root.path().join("corrupt.db");
        fs::write(&artifact, b"this is not a sqlite database")?;

        let error = adopt(&fixture, "adopt-corrupt", &artifact, Some(&fixture.pin))
            .await
            .expect_err("corrupt artifact must be refused");
        assert!(format!("{error:#}").contains("session artifact"));
        assert!(!fixture.session_dir("adopt-corrupt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn adopt_refuses_an_artifact_newer_than_supported() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture.create_artifact("newer", Some(&fixture.pin)).await?;
        let newer_user_version = schema::CURRENT.user_version() + 1;
        let db = Builder::new_local(artifact.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(&format!("PRAGMA user_version = {newer_user_version}"), ())
            .await?;
        drop(conn);
        drop(db);

        let error = adopt(&fixture, "adopt-newer", &artifact, None)
            .await
            .expect_err("newer artifact must be refused");
        let message = error.to_string();
        assert!(message.contains(&format!("user_version {newer_user_version}")));
        assert!(message.contains(schema::CURRENT.as_str()));
        assert!(message.contains("upgrade vfs"));
        assert!(!fixture.session_dir("adopt-newer").exists());
        Ok(())
    }

    #[tokio::test]
    async fn adopt_migrates_an_older_supported_artifact() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture._root.path().join("older.db");
        let db = Builder::new_local(artifact.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(
            "CREATE TABLE fs_inode (
                ino INTEGER PRIMARY KEY AUTOINCREMENT,
                mode INTEGER NOT NULL,
                uid INTEGER NOT NULL DEFAULT 0,
                gid INTEGER NOT NULL DEFAULT 0,
                size INTEGER NOT NULL DEFAULT 0,
                atime INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                ctime INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            (),
        )
        .await?;
        drop(conn);
        drop(db);

        let manifest = adopt(&fixture, "adopt-older", &artifact, Some(&fixture.pin)).await?;
        assert_eq!(manifest.schema_version, schema::CURRENT.as_str());
        assert_eq!(manifest.generation, 0);

        let installed_db = fixture.session_dir("adopt-older").join("delta.db");
        Vfs::open(VfsOptions::with_path(installed_db.to_string_lossy()))
            .await
            .context("adopted session must open at the current schema")?;
        Ok(())
    }

    #[tokio::test]
    async fn adopt_refuses_a_locked_session_as_still_running() -> Result<()> {
        let fixture = AdoptFixture::new()?;
        let artifact = fixture
            .create_artifact("locked", Some(&fixture.pin))
            .await?;
        let session_dir = fixture.session_dir("adopt-locked");
        fs::create_dir_all(&session_dir)?;
        let _live_lock = super::super::session_lock::SessionLock::try_shared(&session_dir)?;

        let error = adopt(&fixture, "adopt-locked", &artifact, None)
            .await
            .expect_err("locked session must be refused");
        assert!(error.downcast_ref::<SessionStillRunning>().is_some());
        Ok(())
    }

    fn remote_manifest(session_id: &str, metadata_key: &str, metadata: &[u8]) -> RemoteManifest {
        RemoteManifest {
            session_id: session_id.to_string(),
            head_seq: 0,
            history_epoch: 1,
            history_valid: true,
            generation: 0,
            artifact_version: schema::CURRENT.as_str().to_string(),
            seed_pin: None,
            metadata: super::super::remote::RemoteMetadata {
                key: metadata_key.to_string(),
                sha256: hex::encode(Sha256::digest(metadata)),
                bytes: metadata.len() as u64,
            },
            chunk_count: 0,
            chunk_bytes: 0,
            created_at_ms: 0,
            vfs_version: super::super::version::VERSION.to_string(),
        }
    }

    fn remote_store() -> Result<(TempDir, RemoteStore)> {
        let remote = tempfile::tempdir()?;
        let url = Url::from_directory_path(remote.path())
            .map_err(|()| anyhow::anyhow!("failed to create file remote URL"))?
            .to_string();
        let store = RemoteStore::new(&url)?;
        Ok((remote, store))
    }

    #[tokio::test]
    async fn remote_input_refuses_a_wrong_session_id() -> Result<()> {
        let (_remote, store) = remote_store()?;
        let manifest = remote_manifest("other-session", "metadata.db", b"metadata");
        store
            .put(
                &manifest_key("requested-session"),
                Bytes::from(manifest.to_json()?),
            )
            .await?;

        let error = fetch_verified_remote_input(&store, "requested-session")
            .await
            .expect_err("wrong session id must fail");
        assert!(error
            .to_string()
            .contains("does not match requested session"));
        Ok(())
    }

    #[tokio::test]
    async fn remote_input_refuses_a_future_artifact_version() -> Result<()> {
        let (_remote, store) = remote_store()?;
        let mut manifest = remote_manifest("future-session", "metadata.db", b"metadata");
        manifest.artifact_version = "999.0".to_string();
        store
            .put(
                &manifest_key("future-session"),
                Bytes::from(manifest.to_json()?),
            )
            .await?;

        let error = fetch_verified_remote_input(&store, "future-session")
            .await
            .expect_err("future artifact version must fail");
        let message = error.to_string();
        assert!(message.contains("upgrade vfs"));
        assert!(message.contains("999.0"));
        Ok(())
    }

    #[tokio::test]
    async fn remote_input_refuses_a_metadata_sha_mismatch() -> Result<()> {
        let (_remote, store) = remote_store()?;
        let metadata = b"metadata";
        let mut manifest = remote_manifest("sha-session", "metadata.db", metadata);
        manifest.metadata.sha256 = "00".repeat(32);
        store
            .put(
                &manifest_key("sha-session"),
                Bytes::from(manifest.to_json()?),
            )
            .await?;
        store
            .put("metadata.db", Bytes::copy_from_slice(metadata))
            .await?;

        let error = fetch_verified_remote_input(&store, "sha-session")
            .await
            .expect_err("metadata sha mismatch must fail");
        assert!(error.to_string().contains("SHA-256 mismatch"));
        Ok(())
    }

    #[tokio::test]
    async fn remote_input_refuses_short_metadata() -> Result<()> {
        let (_remote, store) = remote_store()?;
        let metadata = b"short";
        let mut manifest = remote_manifest("short-session", "metadata.db", metadata);
        manifest.metadata.bytes += 1;
        store
            .put(
                &manifest_key("short-session"),
                Bytes::from(manifest.to_json()?),
            )
            .await?;
        store
            .put("metadata.db", Bytes::copy_from_slice(metadata))
            .await?;

        let error = fetch_verified_remote_input(&store, "short-session")
            .await
            .expect_err("short metadata must fail");
        assert!(error.to_string().contains("length mismatch"));
        Ok(())
    }

    fn git(repo: &Path, args: &[&str]) -> Result<()> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Vfs Adopt Test")
            .env("GIT_AUTHOR_EMAIL", "adopt@example.com")
            .env("GIT_COMMITTER_NAME", "Vfs Adopt Test")
            .env("GIT_COMMITTER_EMAIL", "adopt@example.com")
            .output()?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    fn git_text(repo: &Path, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }
}
