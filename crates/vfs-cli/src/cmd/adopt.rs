//! First-class installation of externally transferred run sessions.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use vfs_core::{schema, Vfs, VfsOptions};

use super::pack::SessionStillRunning;
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
}

/// Install a transferred session artifact and print its machine-readable manifest.
pub async fn handle_adopt_command(
    stdout: &mut impl Write,
    session_id: String,
    db: PathBuf,
    base: PathBuf,
    pin: Option<String>,
    _json: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    adopt_session(stdout, &home, session_id, db, base, pin).await
}

async fn adopt_session(
    stdout: &mut impl Write,
    home: &Path,
    session_id: String,
    db: PathBuf,
    base: PathBuf,
    pin: Option<String>,
) -> Result<()> {
    if !VfsOptions::validate_agent_id(&session_id) {
        bail!("invalid session ID: {session_id}");
    }
    let source_db = std::path::absolute(&db).context("Failed to resolve --db path")?;
    if !source_db.is_file() {
        bail!("session artifact not found: {}", source_db.display());
    }
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
    if db_path.is_file() {
        bail!("session already exists: {}", session_dir.display());
    }
    super::pack::ensure_session_inactive(&paths)?;

    let staging =
        StagedArtifact::new(session_dir.join(format!(".delta.db.adopt-{}.tmp", Uuid::new_v4())));
    super::pack::copy_database_family(&source_db, staging.path())
        .context("Failed to stage the session artifact")?;
    verify_artifact_integrity(staging.path()).await?;
    let artifact_version = migrate_artifact_to_current(staging.path()).await?;

    let vfs = Vfs::open(VfsOptions::with_path(staging.path().to_string_lossy()))
        .await
        .context("Failed to open the staged session artifact")?;
    let metadata = vfs.session_metadata().await?;
    let recorded_pin = vfs.seed_pin().await?;
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
    };
    serde_json::to_writer(&mut *stdout, &manifest)?;
    writeln!(stdout)?;
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

    use tempfile::{tempdir, TempDir};
    use turso::Builder;
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
            artifact.to_path_buf(),
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
