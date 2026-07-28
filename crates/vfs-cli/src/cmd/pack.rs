//! Atomic transfer preparation for inactive `vfs run` sessions.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use vfs_core::{schema, Vfs, VfsOptions};

use super::safety::{
    build_local_database, copy_file_exclusive, remove_sqlite_sidecars_after_checkpoint,
    sidecar_path, ReadOnlyOpenSidecars,
};

pub const SESSION_STILL_RUNNING_EXIT_CODE: i32 = 3;

const DEFAULT_PRUNES: &[&str] = &[
    "**/node_modules/**",
    "**/target/**",
    "**/.venv/**",
    "**/__pycache__/**",
    "**/.next/**",
    "**/dist/**",
    "**/build/**",
];

/// Typed failure used by the CLI edge to return the pack teardown-gate code.
#[derive(Debug)]
pub struct SessionStillRunning;

impl std::fmt::Display for SessionStillRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("session still running; exit the wrapped process first")
    }
}

impl std::error::Error for SessionStillRunning {}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PackManifest {
    manifest_version: u32,
    session_id: String,
    db_path: PathBuf,
    db_sha256: String,
    db_size_bytes: u64,
    base_repo: Option<String>,
    base_pin: Option<String>,
    base_path: PathBuf,
    pruned_paths: Vec<String>,
    seeded_paths: Vec<String>,
    vfs_version: String,
    generation: u64,
}

struct TempDatabase {
    path: PathBuf,
}

impl TempDatabase {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDatabase {
    fn drop(&mut self) {
        remove_database_family(&self.path);
    }
}

/// Prepare a session database for transfer and emit its manifest.
pub async fn handle_pack_command(
    stdout: &mut impl Write,
    session_id: String,
    extra_prunes: Vec<String>,
    no_default_prunes: bool,
    output: Option<PathBuf>,
    _json: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    pack_session(
        stdout,
        &home,
        session_id,
        extra_prunes,
        no_default_prunes,
        output,
    )
    .await
}

async fn pack_session(
    stdout: &mut impl Write,
    home: &Path,
    session_id: String,
    extra_prunes: Vec<String>,
    no_default_prunes: bool,
    output: Option<PathBuf>,
) -> Result<()> {
    if !VfsOptions::validate_agent_id(&session_id) {
        anyhow::bail!("invalid session ID: {session_id}");
    }

    let session_dir = home.join(".vfs").join("run").join(&session_id);
    if !session_dir.is_dir() {
        anyhow::bail!("session not found: {}", session_dir.display());
    }
    let _session_lock =
        super::session_lock::SessionLock::try_exclusive(&session_dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::Error::new(SessionStillRunning)
            } else {
                anyhow::Error::new(error).context("Failed to lock session for packing")
            }
        })?;
    let db_path = session_dir.join("delta.db");
    recover_interrupted_publication(&db_path)?;
    if !db_path.is_file() {
        anyhow::bail!("session database not found: {}", db_path.display());
    }
    let base_path = read_base_path(&session_dir)?;
    let output = normalize_output_path(output, &db_path)?;
    validate_output_target(output.as_deref(), &db_path)?;

    ensure_session_inactive(&session_dir)?;

    let staging =
        TempDatabase::new(session_dir.join(format!(".delta.db.pack-{}.tmp", Uuid::new_v4())));
    copy_database_family(&db_path, staging.path())?;
    ensure_session_inactive(&session_dir)?;

    migrate_staging_database(staging.path()).await?;
    let vfs = Vfs::open(VfsOptions::with_path(staging.path().to_string_lossy()))
        .await
        .map_err(|error| super::migrate::open_error_with_guidance(error, &session_id))
        .context("Failed to open the staged session database")?;

    let prune_set = build_prune_set(&extra_prunes, no_default_prunes)?;
    let pruned_paths = prune_delta_paths(&vfs, &prune_set).await?;
    let metadata = vfs.increment_session_generation().await?;
    vfs.fs
        .finalize()
        .await
        .context("Failed to finalize staged session metadata")?;
    drop(vfs);
    remove_sqlite_sidecars_after_checkpoint(staging.path())?;
    verify_packed_metadata(staging.path(), &metadata)
        .await
        .context("Staged metadata verification before compaction failed")?;

    let staged_vfs = Vfs::open(VfsOptions::with_path(staging.path().to_string_lossy()))
        .await
        .context("Failed to reopen the staged session database for compaction")?;
    let compacted =
        TempDatabase::new(session_dir.join(format!(".delta.db.vacuum-{}.tmp", Uuid::new_v4())));
    staged_vfs
        .compact_local_database_into(compacted.path())
        .await
        .context("Failed to checkpoint and compact the staged session database")?;
    drop(staged_vfs);
    remove_database_family(staging.path());
    fs::rename(compacted.path(), staging.path()).with_context(|| {
        format!(
            "Failed to install compacted staging database {}",
            staging.path().display()
        )
    })?;
    verify_packed_metadata(staging.path(), &metadata)
        .await
        .context("Staged metadata verification after compaction failed")?;
    remove_sqlite_sidecars_after_checkpoint(staging.path())?;

    let (db_sha256, db_size_bytes) = hash_file(staging.path())?;
    let output_temp = output
        .as_deref()
        .map(|path| create_output_temp(staging.path(), path))
        .transpose()?;

    ensure_session_inactive(&session_dir)?;
    let backup_path = publish_live_database(staging.path(), &db_path)?;
    if let Err(error) = verify_packed_metadata(&db_path, &metadata)
        .await
        .context("Published session metadata verification failed")
    {
        rollback_live_database(&db_path, &backup_path)?;
        return Err(error);
    }
    if let (Some(output_temp), Some(output_path)) = (output_temp.as_ref(), output.as_deref()) {
        if let Err(error) = publish_output_database(output_temp.path(), output_path) {
            rollback_live_database(&db_path, &backup_path)?;
            return Err(error);
        }
    }
    cleanup_backup_family(&backup_path);

    let manifest_path = output.unwrap_or_else(|| db_path.clone());
    let (base_repo, base_pin) = git_base_identity(&base_path);
    let manifest = PackManifest {
        manifest_version: 1,
        session_id,
        db_path: manifest_path,
        db_sha256,
        db_size_bytes,
        base_repo,
        base_pin,
        base_path,
        pruned_paths,
        seeded_paths: metadata.seeded_paths,
        vfs_version: super::version::VERSION.to_string(),
        generation: metadata.generation,
    };

    serde_json::to_writer(&mut *stdout, &manifest)?;
    writeln!(stdout)?;
    Ok(())
}

async fn verify_packed_metadata(
    staging_path: &Path,
    expected: &vfs_core::SessionMetadata,
) -> Result<()> {
    let sidecars = ReadOnlyOpenSidecars::capture(staging_path);
    let db = build_local_database(staging_path, None)
        .await
        .context("Failed to open the compacted session database for verification")?;
    let conn = db
        .connect()
        .context("Failed to connect to the compacted session database")?;
    let generation = read_metadata_value(&conn, "generation")
        .await?
        .map(|value| value.parse::<u64>())
        .transpose()
        .context("Invalid compacted session generation")?
        .unwrap_or(0);
    let seeded_paths = read_metadata_value(&conn, "seeded_paths")
        .await?
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .context("Invalid compacted seeded paths")?
        .unwrap_or_default();
    let found = vfs_core::SessionMetadata {
        generation,
        seeded_paths,
    };
    drop(conn);
    drop(db);
    sidecars.remove_created_frameless();
    if &found != expected {
        anyhow::bail!(
            "compacted session metadata changed unexpectedly: expected {:?}, found {:?}",
            expected,
            found
        );
    }
    Ok(())
}

async fn read_metadata_value(conn: &turso::Connection, key: &str) -> Result<Option<String>> {
    let mut rows = conn
        .query(
            "SELECT value FROM fs_session_metadata WHERE key = ?",
            (key,),
        )
        .await?;
    Ok(rows.next().await?.map(|row| row.get(0)).transpose()?)
}

fn ensure_session_inactive(session_dir: &Path) -> Result<()> {
    let mountpoint = session_dir.join("mnt");
    if vfs_mount::is_mountpoint(&mountpoint)
        || super::ps::procs_dir_has_live_processes(&session_dir.join("procs"))
    {
        return Err(SessionStillRunning.into());
    }
    Ok(())
}

fn read_base_path(session_dir: &Path) -> Result<PathBuf> {
    let base_path_file = session_dir.join("base_path");
    let raw = fs::read_to_string(&base_path_file)
        .with_context(|| format!("Failed to read {}", base_path_file.display()))?;
    let path = PathBuf::from(raw.trim());
    if !path.is_absolute() {
        anyhow::bail!(
            "session base path is not absolute in {}",
            base_path_file.display()
        );
    }
    Ok(path)
}

fn normalize_output_path(output: Option<PathBuf>, live_db: &Path) -> Result<Option<PathBuf>> {
    let Some(output) = output else {
        return Ok(None);
    };
    let output = std::path::absolute(output).context("Failed to resolve --output path")?;
    let live_db =
        std::path::absolute(live_db).context("Failed to resolve session database path")?;
    Ok((output != live_db).then_some(output))
}

fn validate_output_target(output: Option<&Path>, live_db: &Path) -> Result<()> {
    let Some(output) = output else {
        return Ok(());
    };
    if output.exists() {
        anyhow::bail!("output already exists: {}", output.display());
    }
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(output, suffix);
        if sidecar.exists() {
            anyhow::bail!("output sidecar already exists: {}", sidecar.display());
        }
    }
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        anyhow::bail!("output parent does not exist: {}", parent.display());
    }
    if output == live_db {
        anyhow::bail!("output path must differ from the live database");
    }
    Ok(())
}

fn copy_database_family(source: &Path, target: &Path) -> Result<()> {
    copy_file_exclusive(source, target)?;
    for suffix in ["-wal", "-shm"] {
        let source_sidecar = sidecar_path(source, suffix);
        if source_sidecar.exists() {
            copy_file_exclusive(&source_sidecar, &sidecar_path(target, suffix))?;
        }
    }
    Ok(())
}

async fn migrate_staging_database(staging_path: &Path) -> Result<()> {
    let db = build_local_database(staging_path, None).await?;
    let conn = db
        .connect()
        .context("Failed to connect to the staged session database")?;
    schema::ensure_current(&conn)
        .await
        .context("Failed to migrate the staged session database")?;
    drop(conn);
    drop(db);
    Ok(())
}

fn build_prune_set(extra_prunes: &[String], no_default_prunes: bool) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    let patterns = DEFAULT_PRUNES
        .iter()
        .copied()
        .filter(|_| !no_default_prunes)
        .map(str::to_string)
        .chain(extra_prunes.iter().cloned());

    for pattern in patterns {
        let pattern = pattern.trim_start_matches('/');
        builder.add(Glob::new(pattern).with_context(|| format!("invalid prune glob: {pattern}"))?);
        if let Some(directory_pattern) = pattern.strip_suffix("/**") {
            builder.add(Glob::new(directory_pattern).with_context(|| {
                format!("invalid derived directory prune glob: {directory_pattern}")
            })?);
        }
    }
    builder.build().context("Failed to build prune glob set")
}

async fn prune_delta_paths(vfs: &Vfs, prune_set: &GlobSet) -> Result<Vec<String>> {
    let mut paths = vfs
        .get_delta_paths()
        .await
        .context("Failed to enumerate delta paths")?
        .into_iter()
        .filter(|path| prune_set.is_match(path.trim_start_matches('/')))
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| {
        right
            .matches('/')
            .count()
            .cmp(&left.matches('/').count())
            .then_with(|| left.cmp(right))
    });

    for path in &paths {
        vfs.fs
            .remove(path)
            .await
            .with_context(|| format!("Failed to prune delta path {path}"))?;
    }
    paths.sort();
    Ok(paths)
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut file =
        fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut size = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    Ok((hex::encode(hasher.finalize()), size))
}

fn create_output_temp(staging_path: &Path, output: &Path) -> Result<TempDatabase> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = output
        .file_name()
        .context("output path has no file name")?
        .to_string_lossy();
    let temp = parent.join(format!(".{file_name}.pack-{}.tmp", Uuid::new_v4()));
    let temp = TempDatabase::new(temp);
    copy_file_exclusive(staging_path, temp.path())?;
    Ok(temp)
}

fn publish_live_database(staging: &Path, live: &Path) -> Result<PathBuf> {
    let backup = publication_backup_path(live)?;
    rename_database_family(live, &backup)?;
    if let Err(error) = fs::rename(staging, live)
        .with_context(|| format!("Failed to install packed database at {}", live.display()))
    {
        let _ = rename_database_family(&backup, live);
        return Err(error);
    }
    if let Err(error) = sync_file_and_parent(live) {
        remove_database_family(live);
        rename_database_family(&backup, live)
            .context("Failed to roll back the live database after sync failed")?;
        return Err(error);
    }
    Ok(backup)
}

fn recover_interrupted_publication(live: &Path) -> Result<()> {
    let backup = publication_backup_path(live)?;
    if !backup.exists() {
        return Ok(());
    }
    if live.exists() {
        cleanup_backup_family(&backup);
        return Ok(());
    }
    rename_database_family(&backup, live)
        .context("Failed to recover the live database from an interrupted pack publication")?;
    sync_file_and_parent(live)
}

fn publication_backup_path(live: &Path) -> Result<PathBuf> {
    let file_name = live
        .file_name()
        .context("session database path has no file name")?
        .to_string_lossy();
    Ok(live.with_file_name(format!(".{file_name}.pack-backup")))
}

fn publish_output_database(temp: &Path, output: &Path) -> Result<()> {
    fs::hard_link(temp, output)
        .with_context(|| format!("Failed to publish packed output {}", output.display()))?;
    if let Err(error) = sync_file_and_parent(output) {
        let _ = fs::remove_file(output);
        return Err(error);
    }
    if let Err(error) = fs::remove_file(temp) {
        let _ = fs::remove_file(output);
        return Err(error).context("Failed to remove the packed output temporary file");
    }
    sync_parent_directory(output)
}

fn rollback_live_database(live: &Path, backup: &Path) -> Result<()> {
    remove_database_family(live);
    rename_database_family(backup, live)
        .context("Failed to roll back the live session database after output publication failed")
}

fn rename_database_family(source: &Path, target: &Path) -> Result<()> {
    let mut renamed = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let source_path = if suffix.is_empty() {
            source.to_path_buf()
        } else {
            sidecar_path(source, suffix)
        };
        if !source_path.exists() {
            continue;
        }
        let target_path = if suffix.is_empty() {
            target.to_path_buf()
        } else {
            sidecar_path(target, suffix)
        };
        if let Err(error) = fs::rename(&source_path, &target_path) {
            for (from, to) in renamed.into_iter().rev() {
                let _ = fs::rename(from, to);
            }
            return Err(error).with_context(|| {
                format!(
                    "Failed to rename database artifact {} to {}",
                    source_path.display(),
                    target_path.display()
                )
            });
        }
        renamed.push((target_path, source_path));
    }
    Ok(())
}

fn cleanup_backup_family(backup: &Path) {
    for path in database_family(backup) {
        if let Err(error) = fs::remove_file(&path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "Warning: packed session committed but failed to remove backup {}: {error}",
                    path.display()
                );
            }
        }
    }
}

fn remove_database_family(path: &Path) {
    for path in database_family(path) {
        let _ = fs::remove_file(path);
    }
}

fn database_family(path: &Path) -> [PathBuf; 3] {
    [
        path.to_path_buf(),
        sidecar_path(path, "-wal"),
        sidecar_path(path, "-shm"),
    ]
}

fn sync_file_and_parent(path: &Path) -> Result<()> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open {}", path.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync {}", path.display()))?;
    sync_parent_directory(path)
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::File::open(parent)
        .with_context(|| format!("Failed to open directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync directory {}", parent.display()))
}

fn git_base_identity(base_path: &Path) -> (Option<String>, Option<String>) {
    let base_pin = git_capture(base_path, &["rev-parse", "HEAD"]);
    if base_pin.is_none() {
        return (None, None);
    }
    let base_repo = git_capture(base_path, &["remote", "get-url", "origin"]);
    (base_repo, base_pin)
}

fn git_capture(base_path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(base_path)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::tempdir;
    use vfs_core::{FileSystem, HostFS, OverlayFS, DEFAULT_FILE_MODE};

    use super::*;

    fn test_options(path: &Path) -> VfsOptions {
        let mut config = vfs_core::CoreConfig::default();
        config.batcher.enabled = false;
        VfsOptions::with_path(path.to_string_lossy()).with_core_config(config)
    }

    async fn create_session(home: &Path, session_id: &str, base: &Path) -> Result<PathBuf> {
        let session_dir = home.join(".vfs").join("run").join(session_id);
        fs::create_dir_all(session_dir.join("mnt"))?;
        fs::create_dir_all(session_dir.join("procs"))?;
        fs::write(
            session_dir.join("base_path"),
            base.to_string_lossy().as_bytes(),
        )?;
        let db_path = session_dir.join("delta.db");
        let vfs = Vfs::open(test_options(&db_path)).await?;
        let conn = vfs.get_connection().await?;
        OverlayFS::init_schema(&conn, base.to_string_lossy().as_ref()).await?;
        drop(conn);
        vfs.fs.finalize().await?;
        Ok(db_path)
    }

    async fn write_delta_file(vfs: &Vfs, path: &str, content: &[u8]) -> Result<()> {
        let (_, file) = vfs.fs.create_file(path, DEFAULT_FILE_MODE, 0, 0).await?;
        file.pwrite(0, content).await?;
        file.fsync().await?;
        Ok(())
    }

    async fn read_overlay_file(db_path: &Path, base: &Path, path: &[&str]) -> Result<Vec<u8>> {
        let vfs = Vfs::open(test_options(db_path)).await?;
        let overlay = OverlayFS::new(Arc::new(HostFS::new(base)?), vfs.fs);
        overlay.load().await?;
        let mut ino = 1;
        let mut stats = None;
        for component in path {
            stats = overlay.lookup(ino, component).await?;
            ino = stats
                .as_ref()
                .with_context(|| format!("missing overlay path component {component}"))?
                .ino;
        }
        let stats = stats.context("empty overlay path")?;
        let file = overlay.open(stats.ino, libc::O_RDONLY).await?;
        Ok(file.pread(0, stats.size as u64).await?)
    }

    #[tokio::test]
    async fn pack_prunes_compacts_hashes_and_increments_generation() -> Result<()> {
        let root = tempdir()?;
        let home = root.path().join("home");
        let base = root.path().join("base");
        fs::create_dir_all(base.join("node_modules"))?;
        fs::write(base.join("node_modules/shadow.txt"), b"base shadow")?;
        fs::write(base.join("keep.txt"), b"base keep")?;
        let db_path = create_session(&home, "pack-test", &base).await?;

        {
            let vfs = Vfs::open(test_options(&db_path)).await?;
            vfs.fs.mkdir("/node_modules", 0, 0).await?;
            write_delta_file(&vfs, "/node_modules/delta.txt", b"delta only").await?;
            write_delta_file(&vfs, "/node_modules/shadow.txt", b"delta shadow").await?;
            write_delta_file(&vfs, "/keep.txt", b"delta keep").await?;
            vfs.set_seeded_paths(&["/seeded.txt".to_string()]).await?;

            let conn = vfs.get_connection().await?;
            conn.execute("CREATE TABLE pack_padding (value BLOB)", ())
                .await?;
            conn.execute(
                "INSERT INTO pack_padding (value) VALUES (zeroblob(2097152))",
                (),
            )
            .await?;
            conn.execute("DELETE FROM pack_padding", ()).await?;
            drop(conn);
            vfs.fs.finalize().await?;
        }
        let padded_size = fs::metadata(&db_path)?.len();

        let output_path = root.path().join("packed.db");
        let mut first_stdout = Vec::new();
        pack_session(
            &mut first_stdout,
            &home,
            "pack-test".to_string(),
            Vec::new(),
            false,
            Some(output_path.clone()),
        )
        .await?;
        let first: PackManifest = serde_json::from_slice(&first_stdout)?;
        assert_eq!(first.manifest_version, 1);
        assert_eq!(first.session_id, "pack-test");
        assert_eq!(first.db_path, output_path);
        assert_eq!(first.generation, 1);
        assert_eq!(first.seeded_paths, vec!["/seeded.txt".to_string()]);
        assert!(first.pruned_paths.contains(&"/node_modules".to_string()));
        assert!(first
            .pruned_paths
            .contains(&"/node_modules/delta.txt".to_string()));
        assert!(first
            .pruned_paths
            .contains(&"/node_modules/shadow.txt".to_string()));
        assert_eq!(first.db_size_bytes, fs::metadata(&output_path)?.len());
        assert!(first.db_size_bytes < padded_size);

        let independent_hash = hex::encode(Sha256::digest(fs::read(&output_path)?));
        assert_eq!(first.db_sha256, independent_hash);
        assert_eq!(fs::read(&output_path)?, fs::read(&db_path)?);
        assert!(
            !sidecar_path(&db_path, "-wal").exists()
                || fs::metadata(sidecar_path(&db_path, "-wal"))?.len() == 0
        );

        let packed = Vfs::open(test_options(&db_path)).await?;
        assert_eq!(packed.session_metadata().await?.generation, 1);
        assert_eq!(packed.get_whiteouts().await?.len(), 0);
        let delta_paths = packed.get_delta_paths().await?;
        assert!(!delta_paths.iter().any(|path| path.contains("node_modules")));
        assert_eq!(
            packed.fs.read_file("/keep.txt").await?.as_deref(),
            Some(b"delta keep".as_slice())
        );
        drop(packed);

        assert_eq!(
            read_overlay_file(&output_path, &base, &["node_modules", "shadow.txt"]).await?,
            b"base shadow"
        );

        let mut second_stdout = Vec::new();
        pack_session(
            &mut second_stdout,
            &home,
            "pack-test".to_string(),
            Vec::new(),
            false,
            None,
        )
        .await?;
        let second: PackManifest = serde_json::from_slice(&second_stdout)?;
        assert_eq!(second.generation, 2);
        assert_eq!(second.db_path, db_path);
        Ok(())
    }

    #[tokio::test]
    async fn pack_refuses_a_live_supervised_process_without_changing_database() -> Result<()> {
        let root = tempdir()?;
        let home = root.path().join("home");
        let base = root.path().join("base");
        fs::create_dir_all(&base)?;
        let db_path = create_session(&home, "live-test", &base).await?;
        let before = fs::read(&db_path)?;

        let mut child = std::process::Command::new("sleep").arg("30").spawn()?;
        let procs_dir = home.join(".vfs/run/live-test/procs");
        fs::write(
            procs_dir.join(format!("{}.json", child.id())),
            serde_json::to_vec(&serde_json::json!({
                "pid": child.id(),
                "owner": true,
                "command": "sleep 30",
                "started_at": chrono::Utc::now(),
                "cwd": base.to_string_lossy(),
            }))?,
        )?;

        let mut stdout = Vec::new();
        let error = pack_session(
            &mut stdout,
            &home,
            "live-test".to_string(),
            Vec::new(),
            false,
            None,
        )
        .await
        .expect_err("live session must be rejected");
        let _ = child.kill();
        let _ = child.wait();

        assert!(error.downcast_ref::<SessionStillRunning>().is_some());
        assert_eq!(
            error.to_string(),
            "session still running; exit the wrapped process first"
        );
        assert!(stdout.is_empty());
        assert_eq!(fs::read(&db_path)?, before);
        Ok(())
    }

    #[test]
    fn custom_prunes_extend_or_replace_defaults() -> Result<()> {
        let extended = build_prune_set(&["**/.cache/**".to_string()], false)?;
        assert!(extended.is_match("node_modules/pkg/index.js"));
        assert!(extended.is_match("src/.cache/item"));

        let custom_only = build_prune_set(&["**/.cache/**".to_string()], true)?;
        assert!(!custom_only.is_match("node_modules/pkg/index.js"));
        assert!(custom_only.is_match("src/.cache/item"));
        Ok(())
    }

    #[test]
    fn interrupted_live_publication_is_recovered_before_pack() -> Result<()> {
        let dir = tempdir()?;
        let live = dir.path().join("delta.db");
        fs::write(&live, b"original")?;
        let backup = publication_backup_path(&live)?;
        fs::rename(&live, &backup)?;

        recover_interrupted_publication(&live)?;

        assert_eq!(fs::read(&live)?, b"original");
        assert!(!backup.exists());
        Ok(())
    }
}
