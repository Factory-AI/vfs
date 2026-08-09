//! Birth-time capture of git state into a portable run-session delta.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use vfs_core::{
    EncryptionConfig, ImportEntry, ImportOptions, OverlayFS, Vfs, VfsOptions, S_IFDIR, S_IFREG,
};

use super::pack::SessionStillRunning;

const ROOT_INO: i64 = 1;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedSummary {
    seeded_paths: Vec<String>,
    whiteout_paths: Vec<String>,
    local_commits: u64,
    pin: String,
}

pub(crate) struct SeededSession {
    pub(crate) summary: SeedSummary,
    session_lock: super::session_lock::SessionLock,
}

impl std::fmt::Debug for SeededSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SeededSession")
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

impl SeededSession {
    pub(crate) fn into_shared_lock(self) -> Result<super::session_lock::SessionLock> {
        self.session_lock
            .downgrade_to_shared()
            .context("Failed to downgrade the seed lock for session startup")
    }
}

/// Seed an inactive run session and print its machine-readable summary.
pub async fn handle_seed_command(
    stdout: &mut impl Write,
    session_id: String,
    pin: String,
    _json: bool,
) -> Result<()> {
    let home = dirs::home_dir().context("Failed to get home directory")?;
    let seeded = seed_session(&home, &session_id, &pin, None, false, None).await?;
    serde_json::to_writer(&mut *stdout, &seeded.summary)?;
    writeln!(stdout)?;
    Ok(())
}

/// Seed a session before `vfs run` acquires its lifetime shared lock.
pub(crate) async fn seed_session(
    home: &Path,
    session_id: &str,
    pin: &str,
    encryption: Option<EncryptionConfig>,
    create_database: bool,
    requested_base_path: Option<&Path>,
) -> Result<SeededSession> {
    if !VfsOptions::validate_agent_id(session_id) {
        bail!("invalid session ID: {session_id}");
    }

    let paths = crate::cmd::run::SessionPaths::new(home, session_id);
    let session_dir = paths.run_dir.clone();
    if !session_dir.is_dir() {
        if create_database {
            fs::create_dir_all(&session_dir).context("Failed to create run session directory")?;
        } else {
            bail!("session not found: {}", session_dir.display());
        }
    }
    let session_lock =
        super::session_lock::SessionLock::try_exclusive(&session_dir).map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::Error::new(SessionStillRunning)
            } else {
                anyhow::Error::new(error).context("Failed to lock session for seeding")
            }
        })?;
    crate::cmd::run::recover_stale_session_runtime(home, session_id)?;
    super::pack::ensure_session_inactive(&paths)?;

    let (base_path, publish_base_path) = resolve_base_path(&paths, requested_base_path)?;
    let db_path = paths.db_path.clone();
    recover_interrupted_publication(&db_path)?;
    super::revert::recover_interrupted_publication(&db_path)?;
    if !db_path.is_file() && !create_database {
        bail!("session database not found: {}", db_path.display());
    }
    let staging = StagedDatabase::new(
        session_dir.join(format!(".delta.db.seed-{}.tmp", uuid::Uuid::new_v4())),
    );
    if db_path.is_file() {
        super::pack::copy_database_family(&db_path, staging.path())
            .context("Failed to stage the session database for seeding")?;
    }

    let mut options = VfsOptions::with_path(staging.path().to_string_lossy())
        .with_core_config(crate::config::core_config_from_env());
    if let Some(encryption) = encryption {
        options = options.with_encryption(encryption);
    }
    let vfs = Vfs::open(options)
        .await
        .map_err(|error| super::migrate::open_error_with_guidance(error, session_id))
        .context("Failed to open session database for seeding")?;
    if vfs.session_status_metadata().await?.seeded {
        super::init::finalize_readonly(&vfs).await;
        bail!("session already seeded");
    }

    eprintln!("Inspecting git state in {}...", base_path.display());
    let plan = match build_seed_plan(&base_path, pin) {
        Ok(plan) => plan,
        Err(error) => {
            super::init::finalize_readonly(&vfs).await;
            return Err(error);
        }
    };

    let conn = vfs.get_connection().await?;
    OverlayFS::init_schema(&conn, base_path.to_string_lossy().as_ref())
        .await
        .context("Failed to initialize overlay metadata for seeding")?;
    drop(conn);

    eprintln!(
        "Seeding {} paths and {} whiteouts...",
        plan.seeded_paths.len(),
        plan.whiteout_paths.len()
    );
    import_entries(&vfs, &plan.entries).await?;
    ensure_git_snapshot_unchanged(&base_path, &plan.snapshot)?;
    let seeded_paths = plan.seeded_paths.iter().cloned().collect::<Vec<String>>();
    let whiteout_paths = plan.whiteout_paths.iter().cloned().collect::<Vec<String>>();
    let whiteout_db_paths = whiteout_paths
        .iter()
        .map(|path| format!("/{path}"))
        .collect::<Vec<_>>();
    vfs.record_seed_state(&seeded_paths, &whiteout_db_paths, &plan.pin)
        .await
        .context("Failed to record seed metadata")?;
    if vfs.history_status().await?.valid {
        vfs.capture_root("seed")
            .await
            .context("Failed to capture seeded history provenance")?;
    }
    vfs.fs
        .finalize()
        .await
        .context("Failed to finalize seeded session database")?;
    drop(vfs);
    super::safety::remove_sqlite_sidecars_after_checkpoint(staging.path())?;
    super::pack::ensure_session_inactive(&paths)?;
    if publish_base_path {
        fs::write(
            &paths.base_path_file,
            base_path.to_string_lossy().as_bytes(),
        )
        .context("Failed to publish session base path")?;
    }
    publish_staged_database(staging.path(), &db_path)?;

    Ok(SeededSession {
        summary: SeedSummary {
            seeded_paths,
            whiteout_paths,
            local_commits: plan.local_commits,
            pin: plan.pin,
        },
        session_lock,
    })
}

struct StagedDatabase {
    path: PathBuf,
}

impl StagedDatabase {
    fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StagedDatabase {
    fn drop(&mut self) {
        super::pack::remove_database_family(&self.path);
    }
}

#[cfg(test)]
async fn prepare_session_database(
    db_path: &Path,
    base_path: &Path,
    encryption: Option<EncryptionConfig>,
) -> Result<()> {
    let mut options = VfsOptions::with_path(db_path.to_string_lossy())
        .with_core_config(crate::config::core_config_from_env());
    if let Some(encryption) = encryption {
        options = options.with_encryption(encryption);
    }
    let vfs = Vfs::open(options)
        .await
        .context("Failed to create session database before seeding")?;
    let conn = vfs.get_connection().await?;
    OverlayFS::init_schema(&conn, base_path.to_string_lossy().as_ref())
        .await
        .context("Failed to initialize session overlay before seeding")?;
    drop(conn);
    vfs.fs
        .finalize()
        .await
        .context("Failed to finalize session database before seeding")
}

struct SeedPlan {
    entries: Vec<PlannedEntry>,
    seeded_paths: BTreeSet<String>,
    whiteout_paths: BTreeSet<String>,
    local_commits: u64,
    pin: String,
    snapshot: GitSnapshot,
    _temp_git_pack: Option<TempGitPackDir>,
}

enum PlannedEntry {
    Host {
        source: PathBuf,
        path: String,
        mode: u32,
        fingerprint: FileFingerprint,
    },
    Inline(ImportEntry),
}

impl PlannedEntry {
    fn path(&self) -> &str {
        match self {
            Self::Host { path, .. } => path,
            Self::Inline(entry) => &entry.path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    mode: u32,
    size: u64,
    modified_secs: i64,
    modified_nanos: i64,
    changed_secs: i64,
    changed_nanos: i64,
}

impl FileFingerprint {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            mode: metadata.mode(),
            size: metadata.size(),
            modified_secs: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
            changed_secs: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct GitSnapshot {
    head: String,
    head_file_sha256: String,
    index_sha256: String,
    status: Vec<u8>,
}

fn build_seed_plan(base_path: &Path, requested_pin: &str) -> Result<SeedPlan> {
    ensure_git_repository(base_path)?;
    let pin = resolve_commit(base_path, requested_pin)
        .with_context(|| format!("invalid seed pin: {requested_pin}"))?;
    let head = resolve_commit(base_path, "HEAD").context("git repository has no valid HEAD")?;
    ensure_pin_is_ancestor(base_path, &pin, &head)?;

    let snapshot = git_snapshot(base_path, &head)?;
    let mut candidates = status_paths(&snapshot.status)?;
    let (tracked_candidates, deleted_paths) = diff_paths(base_path, &pin)?;
    candidates.extend(tracked_candidates);

    let mut entries = Vec::new();
    let mut seeded_paths = BTreeSet::new();
    let mut whiteout_paths = BTreeSet::new();
    for path in candidates {
        let host_path = base_path.join(&path);
        match fs::symlink_metadata(&host_path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                entries.push(host_entry(&host_path, path.clone(), &metadata));
                seeded_paths.insert(path);
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                entries.push(host_entry(&host_path, path.clone(), &metadata));
                seeded_paths.insert(path);
            }
            Ok(_) => bail!("unsupported non-file git path: {path}"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if deleted_paths.contains(&path) {
                    whiteout_paths.insert(path);
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to inspect {}", host_path.display()));
            }
        }
    }

    let local_commits = git_capture_text(
        base_path,
        &["rev-list", "--count", &format!("{pin}..{head}")],
    )?
    .trim()
    .parse::<u64>()
    .context("git returned an invalid local commit count")?;
    let temp_git_pack = add_git_state(
        base_path,
        &pin,
        &head,
        local_commits,
        &mut entries,
        &mut seeded_paths,
    )?;

    Ok(SeedPlan {
        entries,
        seeded_paths,
        whiteout_paths,
        local_commits,
        pin,
        snapshot,
        _temp_git_pack: temp_git_pack,
    })
}

pub(crate) fn ensure_git_repository(base_path: &Path) -> Result<()> {
    let inside = git_capture_text(base_path, &["rev-parse", "--is-inside-work-tree"])
        .context("session base is not a git repository")?;
    if inside.trim() != "true" {
        bail!("session base is not a git repository");
    }
    Ok(())
}

pub(crate) fn resolve_commit(base_path: &Path, revision: &str) -> Result<String> {
    Ok(git_capture_text(
        base_path,
        &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )?
    .trim()
    .to_string())
}

fn ensure_pin_is_ancestor(base_path: &Path, pin: &str, head: &str) -> Result<()> {
    let status = git_status(base_path, &["merge-base", "--is-ancestor", pin, head])?;
    match status.code() {
        Some(0) => Ok(()),
        Some(1) => bail!("seed pin is not an ancestor of HEAD: {pin}"),
        _ => bail!("git merge-base --is-ancestor failed with {status}"),
    }
}

fn git_snapshot(base_path: &Path, expected_head: &str) -> Result<GitSnapshot> {
    let head = resolve_commit(base_path, "HEAD")?;
    if head != expected_head {
        bail!("git checkout changed while seeding; retry");
    }
    let git_dir_text = git_capture_text(base_path, &["rev-parse", "--git-dir"])?;
    let status = git_capture(
        base_path,
        &["status", "--porcelain=v2", "-z", "--untracked-files=all"],
    )?;
    let git_dir = absolute_git_dir(base_path, git_dir_text.trim());
    let head_file = fs::read(git_dir.join("HEAD"))
        .context("Failed to read git HEAD while capturing seed snapshot")?;
    let index = fs::read(git_dir.join("index"))
        .context("Failed to read git index while capturing seed snapshot")?;
    Ok(GitSnapshot {
        head,
        head_file_sha256: hex::encode(Sha256::digest(head_file)),
        index_sha256: hex::encode(Sha256::digest(index)),
        status,
    })
}

fn ensure_git_snapshot_unchanged(base_path: &Path, expected: &GitSnapshot) -> Result<()> {
    let current = git_snapshot(base_path, &expected.head)?;
    if &current != expected {
        bail!("git checkout changed while seeding; retry");
    }
    Ok(())
}

fn status_paths(output: &[u8]) -> Result<BTreeSet<String>> {
    let records = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut paths = BTreeSet::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.is_empty() {
            index += 1;
            continue;
        }
        match record[0] {
            b'1' => paths.insert(field_after_spaces(record, 8)?),
            b'2' => {
                let inserted = paths.insert(field_after_spaces(record, 9)?);
                index += 1;
                let old = records
                    .get(index)
                    .context("malformed porcelain v2 rename record")?;
                paths.insert(path_string(old)?);
                inserted
            }
            b'u' => paths.insert(field_after_spaces(record, 10)?),
            b'?' => paths.insert(path_string(record.get(2..).unwrap_or_default())?),
            b'!' => false,
            other => bail!("unsupported git status record type: {}", other as char),
        };
        index += 1;
    }
    Ok(paths)
}

fn diff_paths(base_path: &Path, pin: &str) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let output = git_capture(base_path, &["diff", "--name-status", "-z", pin])?;
    let records = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut paths = BTreeSet::new();
    let mut deleted = BTreeSet::new();
    let mut index = 0;
    while index < records.len() {
        let status = records[index];
        if status.is_empty() {
            index += 1;
            continue;
        }
        index += 1;
        let first = records
            .get(index)
            .context("malformed git diff --name-status output")?;
        let first = path_string(first)?;
        match status[0] {
            b'D' => {
                deleted.insert(first);
            }
            b'R' => {
                deleted.insert(first);
                index += 1;
                let second = records.get(index).context("malformed git rename output")?;
                paths.insert(path_string(second)?);
            }
            b'C' => {
                index += 1;
                let second = records.get(index).context("malformed git copy output")?;
                paths.insert(path_string(second)?);
            }
            _ => {
                paths.insert(first);
            }
        }
        index += 1;
    }
    Ok((paths, deleted))
}

fn field_after_spaces(record: &[u8], spaces: usize) -> Result<String> {
    let mut seen = 0;
    for (index, byte) in record.iter().enumerate() {
        if *byte == b' ' {
            seen += 1;
            if seen == spaces {
                return path_string(&record[index + 1..]);
            }
        }
    }
    bail!("malformed git status record")
}

fn path_string(bytes: &[u8]) -> Result<String> {
    let path = std::str::from_utf8(bytes).context("non-UTF-8 git paths are not supported")?;
    validate_relative_path(path)?;
    Ok(path.to_string())
}

fn validate_relative_path(path: &str) -> Result<()> {
    if path.is_empty() || Path::new(path).is_absolute() {
        bail!("invalid repository-relative path: {path:?}");
    }
    if Path::new(path)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid repository-relative path: {path:?}");
    }
    Ok(())
}

fn host_entry(host_path: &Path, path: String, metadata: &fs::Metadata) -> PlannedEntry {
    PlannedEntry::Host {
        source: host_path.to_path_buf(),
        path,
        mode: metadata.mode(),
        fingerprint: FileFingerprint::from_metadata(metadata),
    }
}

fn add_git_state(
    base_path: &Path,
    pin: &str,
    head: &str,
    local_commits: u64,
    entries: &mut Vec<PlannedEntry>,
    seeded_paths: &mut BTreeSet<String>,
) -> Result<Option<TempGitPackDir>> {
    let git_dir_text = git_capture_text(base_path, &["rev-parse", "--git-dir"])?;
    let git_dir = absolute_git_dir(base_path, git_dir_text.trim());

    add_host_git_file(&git_dir, "HEAD", entries, seeded_paths)?;
    add_host_git_file(&git_dir, "index", entries, seeded_paths)?;

    if let Ok(symbolic_head) = git_capture_text(base_path, &["symbolic-ref", "-q", "HEAD"]) {
        let symbolic_head = symbolic_head.trim();
        validate_relative_path(symbolic_head)?;
        entries.push(PlannedEntry::Inline(ImportEntry {
            path: format!(".git/{symbolic_head}"),
            mode: S_IFREG | 0o644,
            data: format!("{head}\n").into_bytes(),
        }));
        seeded_paths.insert(format!(".git/{symbolic_head}"));
    }

    if local_commits > 0 {
        let pack_dir = TempGitPackDir::new()?;
        let pack_base = pack_dir.path().join("seed");
        let mut command = git_command(base_path);
        command
            .args(["pack-objects", "--revs"])
            .arg(&pack_base)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().context("Failed to run git pack-objects")?;
        {
            let mut stdin = child
                .stdin
                .take()
                .context("git pack-objects has no stdin")?;
            writeln!(stdin, "{head}")?;
            writeln!(stdin, "^{pin}")?;
        }
        let output = child
            .wait_with_output()
            .context("Failed to wait for git pack-objects")?;
        if !output.status.success() {
            bail!(
                "git pack-objects failed with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let hash = String::from_utf8(output.stdout)?.trim().to_string();
        if hash.is_empty() {
            bail!("git pack-objects returned an empty pack hash");
        }
        for extension in ["pack", "idx"] {
            let source = pack_dir.path().join(format!("seed-{hash}.{extension}"));
            let path = format!(".git/objects/pack/pack-{hash}.{extension}");
            let metadata = fs::metadata(&source)
                .with_context(|| format!("Failed to inspect {}", source.display()))?;
            entries.push(host_entry(&source, path.clone(), &metadata));
            seeded_paths.insert(path);
        }
        return Ok(Some(pack_dir));
    }
    Ok(None)
}

struct TempGitPackDir {
    path: PathBuf,
}

impl TempGitPackDir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("vfs-seed-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path)
            .with_context(|| format!("Failed to create temporary directory {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempGitPackDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn absolute_git_dir(base_path: &Path, git_dir: &str) -> PathBuf {
    let git_dir = PathBuf::from(git_dir);
    if git_dir.is_absolute() {
        git_dir
    } else {
        base_path.join(git_dir)
    }
}

fn add_host_git_file(
    git_dir: &Path,
    relative: &str,
    entries: &mut Vec<PlannedEntry>,
    seeded_paths: &mut BTreeSet<String>,
) -> Result<()> {
    let source = git_dir.join(relative);
    let metadata = fs::symlink_metadata(&source)
        .with_context(|| format!("Failed to inspect git state {}", source.display()))?;
    let path = format!(".git/{relative}");
    entries.push(host_entry(&source, path.clone(), &metadata));
    seeded_paths.insert(path);
    Ok(())
}

fn parent_directories(entries: &[PlannedEntry]) -> Vec<ImportEntry> {
    let mut dirs = BTreeSet::new();
    for entry in entries {
        let mut parent = Path::new(entry.path()).parent();
        while let Some(path) = parent {
            if path.as_os_str().is_empty() {
                break;
            }
            dirs.insert(path.to_string_lossy().replace('\\', "/"));
            parent = path.parent();
        }
    }
    let mut directories = dirs
        .into_iter()
        .map(|path| ImportEntry {
            path,
            mode: S_IFDIR | 0o755,
            data: Vec::new(),
        })
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        left.path
            .matches('/')
            .count()
            .cmp(&right.path.matches('/').count())
            .then_with(|| left.path.cmp(&right.path))
    });
    directories
}

async fn import_entries(vfs: &Vfs, entries: &[PlannedEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let mut session = vfs
        .fs
        .begin_import(
            ROOT_INO,
            ImportOptions {
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
                timestamp: (timestamp.as_secs() as i64, timestamp.subsec_nanos() as i64),
            },
        )
        .await
        .context("Failed to begin seed import")?;
    let directories = parent_directories(entries);
    for chunk in directories.chunks(512) {
        session
            .import_chunk(chunk)
            .await
            .context("Failed to import seed directories")?;
    }

    const CHUNK_BYTES: usize = 4 * 1024 * 1024;
    const CHUNK_ENTRIES: usize = 512;
    let mut chunk = Vec::new();
    let mut chunk_bytes = 0usize;
    for planned in entries {
        let entry = materialize_entry(planned)?;
        chunk_bytes += entry.data.len();
        chunk.push(entry);
        if chunk_bytes >= CHUNK_BYTES || chunk.len() >= CHUNK_ENTRIES {
            session
                .import_chunk(&chunk)
                .await
                .context("Failed to import seed content")?;
            chunk.clear();
            chunk_bytes = 0;
        }
    }
    if !chunk.is_empty() {
        session
            .import_chunk(&chunk)
            .await
            .context("Failed to import seed content")?;
    }
    session.finish();
    Ok(())
}

fn materialize_entry(planned: &PlannedEntry) -> Result<ImportEntry> {
    match planned {
        PlannedEntry::Inline(entry) => Ok(entry.clone()),
        PlannedEntry::Host {
            source,
            path,
            mode,
            fingerprint,
        } => {
            let before = fs::symlink_metadata(source)
                .with_context(|| format!("Failed to inspect seed source {}", source.display()))?;
            if FileFingerprint::from_metadata(&before) != *fingerprint {
                bail!("seed source changed while reading: {}", source.display());
            }
            let data = if before.file_type().is_symlink() {
                fs::read_link(source)
                    .with_context(|| format!("Failed to read seed symlink {}", source.display()))?
                    .as_os_str()
                    .as_bytes()
                    .to_vec()
            } else {
                fs::read(source)
                    .with_context(|| format!("Failed to read seed file {}", source.display()))?
            };
            let after = fs::symlink_metadata(source).with_context(|| {
                format!("Failed to re-inspect seed source {}", source.display())
            })?;
            if FileFingerprint::from_metadata(&after) != *fingerprint {
                bail!("seed source changed while reading: {}", source.display());
            }
            Ok(ImportEntry {
                path: path.clone(),
                mode: *mode,
                data,
            })
        }
    }
}

fn resolve_base_path(
    paths: &crate::cmd::run::SessionPaths,
    requested_base_path: Option<&Path>,
) -> Result<(PathBuf, bool)> {
    let path_file = &paths.base_path_file;
    let existing = match fs::read_to_string(path_file) {
        Ok(raw) => Some(PathBuf::from(raw.trim())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path_file.display()));
        }
    };
    let publish = existing.is_none();
    let path = match (existing, requested_base_path) {
        (Some(existing), Some(requested)) if existing != requested => {
            bail!(
                "session base path {} does not match requested base {}",
                existing.display(),
                requested.display()
            );
        }
        (Some(existing), _) => existing,
        (None, Some(requested)) => requested.to_path_buf(),
        (None, None) => bail!("session base path not found: {}", path_file.display()),
    };
    if !path.is_absolute() {
        bail!(
            "session base path is not absolute in {}",
            path_file.display()
        );
    }
    Ok((path, publish))
}

fn publish_staged_database(staging: &Path, live: &Path) -> Result<()> {
    if !live.exists() {
        fs::rename(staging, live)
            .with_context(|| format!("Failed to publish seeded database {}", live.display()))?;
        return super::pack::sync_file_and_parent(live);
    }

    let backup = seed_backup_path(live);
    super::pack::rename_database_family(live, &backup)
        .context("Failed to stage the live database for seed publication")?;
    if let Err(error) = fs::rename(staging, live)
        .with_context(|| format!("Failed to publish seeded database {}", live.display()))
        .and_then(|()| super::pack::sync_file_and_parent(live))
    {
        super::pack::remove_database_family(live);
        super::pack::rename_database_family(&backup, live)
            .context("Failed to restore the live database after seed publication failed")?;
        return Err(error);
    }
    super::pack::remove_database_family(&backup);
    Ok(())
}

fn recover_interrupted_publication(live: &Path) -> Result<()> {
    let backup = seed_backup_path(live);
    if !backup.exists() {
        return Ok(());
    }
    if live.exists() {
        super::pack::remove_database_family(&backup);
        return Ok(());
    }
    super::pack::rename_database_family(&backup, live)
        .context("Failed to recover an interrupted seed publication")?;
    super::pack::sync_file_and_parent(live)
}

fn seed_backup_path(live: &Path) -> PathBuf {
    live.with_file_name(".delta.db.seed-backup")
}

fn git_capture_text(base_path: &Path, args: &[&str]) -> Result<String> {
    String::from_utf8(git_capture(base_path, args)?).context("git output was not valid UTF-8")
}

fn git_capture(base_path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = git_command(base_path)
        .args(args)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn git_status(base_path: &Path, args: &[&str]) -> Result<std::process::ExitStatus> {
    git_command(base_path)
        .args(args)
        .status()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))
}

fn git_command(base_path: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(base_path);
    vfs_mount::supervise::set_parent_death_signal_std(&mut command);
    command
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sha2::{Digest, Sha256};
    use tempfile::{tempdir, TempDir};
    use vfs_core::{FileSystem, HostFS};

    use super::*;

    struct GitFixture {
        _root: TempDir,
        home: PathBuf,
        repo: PathBuf,
        origin: PathBuf,
        pin: String,
        local_head: String,
    }

    impl GitFixture {
        fn full_dirty_state() -> Result<Self> {
            let root = tempdir()?;
            let home = root.path().join("home");
            let repo = root.path().join("repo");
            let origin = root.path().join("origin.git");
            fs::create_dir_all(&home)?;
            fs::create_dir_all(&repo)?;
            git(&repo, &["init", "-b", "main"])?;
            git(&repo, &["config", "user.name", "Vfs Seed Test"])?;
            git(&repo, &["config", "user.email", "seed@example.com"])?;

            fs::write(repo.join(".gitignore"), "ignored.log\n")?;
            fs::write(repo.join("dirty.txt"), "pin dirty\n")?;
            fs::write(repo.join("delete.txt"), "delete me\n")?;
            fs::write(repo.join("exec.sh"), "#!/bin/sh\necho pin\n")?;
            let mut executable = fs::metadata(repo.join("exec.sh"))?.permissions();
            use std::os::unix::fs::PermissionsExt;
            executable.set_mode(0o755);
            fs::set_permissions(repo.join("exec.sh"), executable)?;
            fs::write(repo.join("local.txt"), "pin local\n")?;
            fs::write(repo.join("rename-old.txt"), "renamed\n")?;
            git(&repo, &["add", "."])?;
            git(&repo, &["commit", "-m", "pin"])?;
            let pin = git_text(&repo, &["rev-parse", "HEAD"])?;

            git(root.path(), &["init", "--bare", origin.to_str().unwrap()])?;
            git(
                &repo,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            )?;
            git(&repo, &["push", "origin", "main"])?;

            fs::write(repo.join("local.txt"), "local commit\n")?;
            git(&repo, &["add", "local.txt"])?;
            git(&repo, &["commit", "-m", "local only"])?;
            let local_head = git_text(&repo, &["rev-parse", "HEAD"])?;

            fs::write(repo.join("dirty.txt"), "dirty worktree\n")?;
            fs::write(repo.join("untracked.txt"), "untracked\n")?;
            fs::write(repo.join("ignored.log"), "ignored\n")?;
            fs::remove_file(repo.join("delete.txt"))?;
            fs::write(repo.join("exec.sh"), "#!/bin/sh\necho dirty\n")?;
            fs::write(repo.join("staged.txt"), "staged\n")?;
            git(&repo, &["add", "staged.txt"])?;
            git(&repo, &["mv", "rename-old.txt", "rename-new.txt"])?;

            Ok(Self {
                _root: root,
                home,
                repo,
                origin,
                pin,
                local_head,
            })
        }

        async fn create_session(&self, id: &str) -> Result<PathBuf> {
            let session_dir = self.home.join(".vfs").join("run").join(id);
            fs::create_dir_all(session_dir.join("mnt"))?;
            fs::create_dir_all(session_dir.join("procs"))?;
            fs::write(
                session_dir.join("base_path"),
                self.repo.to_string_lossy().as_bytes(),
            )?;
            let db_path = session_dir.join("delta.db");
            prepare_session_database(&db_path, &self.repo, None).await?;
            Ok(db_path)
        }

        fn pristine_clone(&self, name: &str) -> Result<PathBuf> {
            let clone = self._root.path().join(name);
            git(
                self._root.path(),
                &[
                    "clone",
                    "--quiet",
                    self.origin.to_str().unwrap(),
                    clone.to_str().unwrap(),
                ],
            )?;
            git(&clone, &["checkout", "--quiet", &self.pin])?;
            Ok(clone)
        }
    }

    #[test]
    fn parses_porcelain_v2_paths() -> Result<()> {
        let record = b"1 .M N... 100644 100644 100644 abc def path with spaces";
        assert_eq!(field_after_spaces(record, 8)?, "path with spaces");
        Ok(())
    }

    #[test]
    fn imported_modes_are_supported_file_kinds() {
        assert_eq!((S_IFREG | 0o755) & vfs_core::S_IFMT, S_IFREG);
        assert_eq!(
            (vfs_core::S_IFLNK | 0o777) & vfs_core::S_IFMT,
            vfs_core::S_IFLNK
        );
    }

    #[test]
    fn interrupted_seed_publication_recovers_live_database() -> Result<()> {
        let dir = tempdir()?;
        let live = dir.path().join("delta.db");
        let backup = seed_backup_path(&live);
        fs::write(&backup, b"original")?;

        recover_interrupted_publication(&live)?;

        assert_eq!(fs::read(&live)?, b"original");
        assert!(!backup.exists());
        Ok(())
    }

    #[tokio::test]
    async fn seed_round_trips_all_dirty_classes_and_git_state() -> Result<()> {
        let fixture = GitFixture::full_dirty_state()?;
        let db_path = fixture.create_session("seed-all").await?;

        let seeded =
            seed_session(&fixture.home, "seed-all", &fixture.pin, None, false, None).await?;
        let summary = seeded.summary.clone();
        assert_eq!(summary.local_commits, 1);
        assert_eq!(summary.pin, fixture.pin);
        assert!(summary.seeded_paths.contains(&"dirty.txt".to_string()));
        assert!(summary.seeded_paths.contains(&"untracked.txt".to_string()));
        assert!(summary.seeded_paths.contains(&"staged.txt".to_string()));
        assert!(summary.seeded_paths.contains(&"exec.sh".to_string()));
        assert!(summary.seeded_paths.contains(&"local.txt".to_string()));
        assert!(summary.seeded_paths.contains(&"rename-new.txt".to_string()));
        assert!(!summary.seeded_paths.contains(&"ignored.log".to_string()));
        assert_eq!(
            summary.whiteout_paths,
            vec!["delete.txt".to_string(), "rename-old.txt".to_string()]
        );
        assert!(summary
            .seeded_paths
            .windows(2)
            .all(|pair| pair[0] <= pair[1]));

        let pristine = fixture.pristine_clone("pristine")?;
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;
        assert_eq!(
            vfs.session_metadata().await?.seeded_paths,
            summary.seeded_paths
        );
        assert_eq!(vfs.seed_pin().await?.as_deref(), Some(fixture.pin.as_str()));
        let overlay = OverlayFS::new(Arc::new(HostFS::new(&pristine)?), vfs.fs);
        overlay.load().await?;

        assert_eq!(
            overlay_read(&overlay, "dirty.txt").await?.as_deref(),
            Some(b"dirty worktree\n".as_slice())
        );
        assert_eq!(
            overlay_read(&overlay, "untracked.txt").await?.as_deref(),
            Some(b"untracked\n".as_slice())
        );
        assert_eq!(
            overlay_read(&overlay, "local.txt").await?.as_deref(),
            Some(b"local commit\n".as_slice())
        );
        assert!(overlay_stats(&overlay, "delete.txt").await?.is_none());
        assert!(overlay_stats(&overlay, "ignored.log").await?.is_none());
        let exec = overlay_stats(&overlay, "exec.sh")
            .await?
            .context("missing executable")?;
        assert_ne!(exec.mode & 0o111, 0);

        materialize_seeded_overlay(
            &overlay,
            &pristine,
            &summary.seeded_paths,
            &summary.whiteout_paths,
        )
        .await?;
        assert_eq!(
            git_text(&pristine, &["rev-parse", "HEAD"])?,
            fixture.local_head
        );
        assert_eq!(
            git_text(
                &pristine,
                &["rev-list", "--count", &format!("{}..HEAD", fixture.pin)]
            )?,
            "1"
        );
        assert_eq!(
            portable_worktree_hashes(&pristine)?,
            portable_worktree_hashes(&fixture.repo)?
        );
        assert_eq!(
            git_bytes(
                &pristine,
                &["status", "--porcelain=v2", "-z", "--untracked-files=all"]
            )?,
            git_bytes(
                &fixture.repo,
                &["status", "--porcelain=v2", "-z", "--untracked-files=all"]
            )?
        );
        assert!(!pristine.join("ignored.log").exists());

        drop(seeded);
        let error = seed_session(&fixture.home, "seed-all", &fixture.pin, None, false, None)
            .await
            .expect_err("second seed must fail");
        assert_eq!(error.to_string(), "session already seeded");

        let json = serde_json::to_value(&summary)?;
        assert!(json["seededPaths"].is_array());
        assert!(json["whiteoutPaths"].is_array());
        assert_eq!(json["localCommits"], 1);
        assert_eq!(json["pin"], fixture.pin);
        Ok(())
    }

    #[tokio::test]
    async fn empty_seed_manifest_still_blocks_a_second_seed() -> Result<()> {
        let fixture = GitFixture::full_dirty_state()?;
        let db_path = fixture.create_session("empty-seeded").await?;
        let vfs = Vfs::open(VfsOptions::with_path(db_path.to_string_lossy())).await?;
        vfs.set_seeded_paths(&[]).await?;
        vfs.fs.finalize().await?;
        drop(vfs);

        let error = seed_session(
            &fixture.home,
            "empty-seeded",
            &fixture.pin,
            None,
            false,
            None,
        )
        .await
        .expect_err("an empty seed manifest must still be one-shot");

        assert_eq!(error.to_string(), "session already seeded");
        Ok(())
    }

    #[tokio::test]
    async fn seed_rejects_invalid_non_ancestor_and_live_sessions() -> Result<()> {
        let fixture = GitFixture::full_dirty_state()?;
        let invalid_db = fixture.create_session("invalid-pin").await?;
        let invalid_before = fs::read(&invalid_db)?;
        let invalid = seed_session(
            &fixture.home,
            "invalid-pin",
            "not-a-commit",
            None,
            false,
            None,
        )
        .await
        .expect_err("invalid pin must fail");
        assert!(format!("{invalid:#}").contains("invalid seed pin"));
        assert_eq!(fs::read(&invalid_db)?, invalid_before);

        fixture.create_session("non-ancestor").await?;
        let tree = git_text(&fixture.repo, &["rev-parse", "HEAD^{tree}"])?;
        let unrelated = git_with_stdin(
            &fixture.repo,
            &["commit-tree", &tree],
            b"unrelated commit\n",
        )?;
        let error = seed_session(&fixture.home, "non-ancestor", &unrelated, None, false, None)
            .await
            .expect_err("non-ancestor pin must fail");
        assert!(error.to_string().contains("not an ancestor"));

        fixture.create_session("live").await?;
        let live_dir = fixture.home.join(".vfs/run/live");
        let _live_lock = super::super::session_lock::SessionLock::try_shared(&live_dir)?;
        let base_before = fs::read(live_dir.join("base_path"))?;
        let error = seed_session(
            &fixture.home,
            "live",
            &fixture.pin,
            None,
            false,
            Some(fixture._root.path()),
        )
        .await
        .expect_err("live session must fail");
        assert!(error.downcast_ref::<SessionStillRunning>().is_some());
        assert_eq!(fs::read(live_dir.join("base_path"))?, base_before);
        Ok(())
    }

    #[tokio::test]
    async fn failed_import_leaves_live_database_retryable() -> Result<()> {
        use std::os::unix::ffi::OsStringExt;

        let fixture = GitFixture::full_dirty_state()?;
        let db_path = fixture.create_session("retry").await?;
        let before = fs::read(&db_path)?;
        let bad_target = std::ffi::OsString::from_vec(vec![0xff]);
        std::os::unix::fs::symlink(bad_target, fixture.repo.join("zz-invalid-link"))?;

        let error = seed_session(&fixture.home, "retry", &fixture.pin, None, false, None)
            .await
            .expect_err("invalid symlink target must fail");
        assert!(format!("{error:#}").contains("Failed to import seed content"));
        assert_eq!(fs::read(&db_path)?, before);

        fs::remove_file(fixture.repo.join("zz-invalid-link"))?;
        fs::write(fixture.repo.join("zz-invalid-link"), "now valid\n")?;
        let seeded = seed_session(&fixture.home, "retry", &fixture.pin, None, false, None).await?;
        assert!(seeded
            .summary
            .seeded_paths
            .contains(&"zz-invalid-link".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn run_seed_creation_publishes_base_and_downgrades_lock() -> Result<()> {
        let fixture = GitFixture::full_dirty_state()?;
        let session_dir = fixture.home.join(".vfs/run/run-seed");
        fs::create_dir_all(session_dir.join("mnt"))?;
        fs::create_dir_all(session_dir.join("procs"))?;

        let seeded = seed_session(
            &fixture.home,
            "run-seed",
            &fixture.pin,
            None,
            true,
            Some(&fixture.repo),
        )
        .await?;
        assert!(session_dir.join("delta.db").is_file());
        assert_eq!(
            fs::read_to_string(session_dir.join("base_path"))?,
            fixture.repo.to_string_lossy()
        );

        let run_lock = seeded.into_shared_lock()?;
        assert_eq!(
            super::super::session_lock::SessionLock::try_exclusive(&session_dir)
                .err()
                .context("exclusive lock must remain blocked")?
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(run_lock);
        super::super::session_lock::SessionLock::try_exclusive(&session_dir)?;
        Ok(())
    }

    async fn materialize_seeded_overlay(
        overlay: &OverlayFS,
        destination: &Path,
        seeded_paths: &[String],
        whiteout_paths: &[String],
    ) -> Result<()> {
        for path in whiteout_paths {
            let target = destination.join(path);
            if target.is_dir() {
                fs::remove_dir_all(&target)?;
            } else if target.exists() {
                fs::remove_file(&target)?;
            }
        }
        for path in seeded_paths {
            let stats = overlay_stats(overlay, path)
                .await?
                .with_context(|| format!("missing seeded overlay path {path}"))?;
            let target = destination.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            if stats.is_symlink() {
                if target.exists() {
                    fs::remove_file(&target)?;
                }
                let target_value = overlay
                    .readlink(stats.ino)
                    .await?
                    .context("missing seeded symlink target")?;
                std::os::unix::fs::symlink(target_value, target)?;
            } else {
                fs::write(
                    &target,
                    overlay_read(overlay, path).await?.unwrap_or_default(),
                )?;
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, fs::Permissions::from_mode(stats.mode & 0o7777))?;
            }
        }
        Ok(())
    }

    async fn overlay_read(overlay: &OverlayFS, path: &str) -> Result<Option<Vec<u8>>> {
        let Some(stats) = overlay_stats(overlay, path).await? else {
            return Ok(None);
        };
        let file = overlay.open(stats.ino, libc::O_RDONLY).await?;
        Ok(Some(file.pread(0, stats.size as u64).await?))
    }

    async fn overlay_stats(overlay: &OverlayFS, path: &str) -> Result<Option<vfs_core::Stats>> {
        let mut ino = ROOT_INO;
        let mut stats = None;
        for component in path.split('/').filter(|component| !component.is_empty()) {
            stats = overlay.lookup(ino, component).await?;
            let Some(found) = stats.as_ref() else {
                return Ok(None);
            };
            ino = found.ino;
        }
        Ok(stats)
    }

    fn git(repo: &Path, args: &[&str]) -> Result<()> {
        let output = git_output(repo, args, None)?;
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
        Ok(String::from_utf8(git_bytes(repo, args)?)?
            .trim()
            .to_string())
    }

    fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let output = git_output(repo, args, None)?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(output.stdout)
    }

    fn git_with_stdin(repo: &Path, args: &[&str], stdin: &[u8]) -> Result<String> {
        let output = git_output(repo, args, Some(stdin))?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(String::from_utf8(output.stdout)?.trim().to_string())
    }

    fn git_output(
        repo: &Path,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<std::process::Output> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Vfs Seed Test")
            .env("GIT_AUTHOR_EMAIL", "seed@example.com")
            .env("GIT_COMMITTER_NAME", "Vfs Seed Test")
            .env("GIT_COMMITTER_EMAIL", "seed@example.com")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn()?;
        if let Some(stdin) = stdin {
            child
                .stdin
                .take()
                .context("git child has no stdin")?
                .write_all(stdin)?;
        }
        Ok(child.wait_with_output()?)
    }

    fn portable_worktree_hashes(repo: &Path) -> Result<Vec<(String, u32, String)>> {
        let paths = git_bytes(
            repo,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?;
        let mut hashes = Vec::new();
        for path in paths
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = path_string(path)?;
            let host_path = repo.join(&path);
            let Ok(metadata) = fs::symlink_metadata(&host_path) else {
                continue;
            };
            let data = if metadata.file_type().is_symlink() {
                fs::read_link(&host_path)?.as_os_str().as_bytes().to_vec()
            } else {
                fs::read(&host_path)?
            };
            hashes.push((
                path,
                metadata.mode() & 0o170777,
                hex::encode(Sha256::digest(data)),
            ));
        }
        hashes.sort();
        Ok(hashes)
    }
}
