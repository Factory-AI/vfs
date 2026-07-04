//! `agentfs clone`: populate an AgentFS database from a git repository
//! without per-file FUSE round trips.
//!
//! A regular `git clone` through the mount pays ~9-11 FUSE round trips plus
//! two SQLite transactions per worktree file. This command instead runs
//! `git clone --no-checkout` through a temporary mount (pack files are a few
//! large sequential writes), then streams the worktree content out of the
//! object database: a producer thread parses `git ls-tree` + `git cat-file
//! --batch` output while an [`agentfs_sdk::ImportSession`] consumer bulk
//! imports each chunk (large multi-inode transactions), so blob decoding
//! overlaps SQLite writes instead of buffering every blob in memory first.
//! Finally it fabricates a git index whose cached stat data matches exactly
//! what the filesystem serves — so `git status` is clean without re-reading
//! any content.
//!
//! Invariants: all state lands in the single database file; nothing is
//! written to the host filesystem. Limitations (v1): submodules are
//! rejected; smudge/clean filters and `core.autocrlf` rewriting are not
//! applied (blobs are imported verbatim); SHA-1 repositories only.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentfs_sdk::{AgentFSOptions, FileSystem, ImportEntry, ImportOptions, ImportedEntry};
use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};

use crate::cmd::init::open_agentfs;
use crate::cmd::supervise::{set_parent_death_signal_std, supervise_command, ChildOutcome};
use crate::mount::{mount_fs, MountBackend, MountOpts};

const S_IFDIR: u32 = 0o040000;
const MODE_FILE: u32 = 0o100644;
const MODE_EXEC: u32 = 0o100755;
const MODE_SYMLINK: u32 = 0o120000;
const MODE_GITLINK: u32 = 0o160000;

/// One blob-bearing row of `git ls-tree -r HEAD`.
struct TreeRow {
    /// Tree entry mode (0o100644 / 0o100755 / 0o120000).
    mode: u32,
    /// Lowercase hex SHA-1 of the blob.
    sha: String,
    /// Repository-relative path.
    path: String,
}

pub async fn handle_clone_command(
    id_or_path: String,
    source: String,
    name: Option<String>,
    backend: MountBackend,
    verify: bool,
) -> Result<()> {
    let repo_name = match name {
        Some(name) => name,
        None => derive_repo_name(&source)?,
    };

    let options = AgentFSOptions::resolve(&id_or_path)
        .unwrap_or_else(|_| AgentFSOptions::with_path(&id_or_path));
    let agentfs = open_agentfs(options)
        .await
        .with_context(|| format!("failed to open AgentFS database: {id_or_path}"))?;
    let agent = agentfs.fs.clone();
    let fs: Arc<dyn FileSystem> = Arc::new(agentfs.fs);

    let clone_id = uuid::Uuid::new_v4().to_string();
    let mountpoint = std::env::temp_dir().join(format!("agentfs-clone-{clone_id}"));
    std::fs::create_dir_all(&mountpoint).context("failed to create mount directory")?;

    let mount_opts = MountOpts {
        mountpoint: mountpoint.clone(),
        backend,
        fsname: format!("agentfs:{id_or_path}"),
        uid: None,
        gid: None,
        allow_other: false,
        allow_root: false,
        auto_unmount: false,
        lazy_unmount: true,
        timeout: std::time::Duration::from_secs(10),
    };
    let mount_handle = mount_fs(fs, mount_opts).await?;

    let result = tokio::select! {
        result = clone_into_mount(&agent, &mountpoint, &source, &repo_name, verify) => result,
        signal = crate::mount::termination_signal() => {
            match signal {
                Ok(signo) => Err(InterruptedSignal(signo).into()),
                Err(error) => Err(error.into()),
            }
        }
    };

    drop(mount_handle);
    let _ = std::fs::remove_dir_all(&mountpoint);

    let summary = match result {
        Ok(summary) => summary,
        Err(error) => {
            if let Some(interrupted) = error.downcast_ref::<InterruptedSignal>() {
                std::process::exit(128 + interrupted.0);
            }
            return Err(error);
        }
    };
    eprintln!(
        "Cloned {} into {} ({} files, {} bytes imported)",
        source, id_or_path, summary.files, summary.bytes
    );
    Ok(())
}

#[derive(Debug)]
struct InterruptedSignal(i32);

impl std::fmt::Display for InterruptedSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interrupted by signal {}", self.0)
    }
}

impl std::error::Error for InterruptedSignal {}

struct CloneSummary {
    files: usize,
    bytes: u64,
}

async fn clone_into_mount(
    agent: &agentfs_sdk::filesystem::AgentFS,
    mountpoint: &Path,
    source: &str,
    repo_name: &str,
    verify: bool,
) -> Result<CloneSummary> {
    let timings = crate::config::clone_timings_enabled();
    let mut stage_start = std::time::Instant::now();
    let mut stage = |name: &str| {
        if timings {
            eprintln!("stage {name}: {:?}", stage_start.elapsed());
        }
        stage_start = std::time::Instant::now();
    };

    let repo_dir = mountpoint.join(repo_name);
    let repo_dir_str = repo_dir
        .to_str()
        .context("mountpoint path is not valid UTF-8")?;

    run_git(
        Path::new("."),
        &["clone", "--no-checkout", "--quiet", source, repo_dir_str],
    )
    .await?;
    stage("git-clone-no-checkout");

    let head = run_git_capture(&repo_dir, &["rev-parse", "--verify", "--quiet", "HEAD"]).ok();
    let Some(_head) = head else {
        eprintln!("Repository has no HEAD commit; nothing to materialize.");
        return Ok(CloneSummary { files: 0, bytes: 0 });
    };

    let rows = ls_tree(&repo_dir)?;
    stage("ls-tree");

    let dur = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let timestamp = (dur.as_secs() as i64, dur.subsec_nanos() as i64);
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };

    use std::os::unix::fs::MetadataExt;
    let repo_meta = std::fs::metadata(&repo_dir).context("failed to stat repository root")?;
    let dest_parent = repo_meta.ino() as i64;
    let dev = repo_meta.dev();

    let mut session = agent
        .begin_import(
            dest_parent,
            ImportOptions {
                uid,
                gid,
                timestamp,
            },
        )
        .await
        .context("failed to begin bulk import")?;

    // All directories go in one up-front chunk so streamed file chunks may
    // arrive in any order relative to each other.
    session
        .import_chunk(&dir_entries(&rows)?)
        .await
        .context("bulk import failed (directories)")?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<ImportEntry>>(4);
    let producer = spawn_blob_producer(repo_dir.clone(), &rows, tx)?;

    let mut import_err: Option<anyhow::Error> = None;
    while let Some(chunk) = rx.recv().await {
        if let Err(error) = session.import_chunk(&chunk).await {
            import_err = Some(anyhow::Error::from(error));
            break;
        }
    }
    drop(rx); // unblocks the producer if the import bailed early
    let produced = producer
        .join()
        .map_err(|_| anyhow::anyhow!("blob producer thread panicked"))?;
    if let Some(error) = import_err {
        return Err(error.context("bulk import failed"));
    }
    let bytes = produced?;
    let imported = session.finish();
    stage("stream-import");

    let index = build_index_v2(&rows, &imported, timestamp, uid, gid, dev)?;
    std::fs::write(repo_dir.join(".git").join("index"), index)
        .context("failed to write git index")?;
    stage("write-index");

    if verify {
        let status = run_git_capture(&repo_dir, &["status", "--porcelain"])?;
        if !status.trim().is_empty() {
            bail!("post-clone verification failed; git status is not clean:\n{status}");
        }
        stage("verify");
    }

    Ok(CloneSummary {
        files: rows.len(),
        bytes,
    })
}

/// Derive the destination directory name the way git does.
fn derive_repo_name(source: &str) -> Result<String> {
    let trimmed = source.trim_end_matches('/');
    let last = trimmed
        .rsplit(['/', ':'])
        .next()
        .filter(|s| !s.is_empty())
        .context("cannot derive repository name from source; pass NAME explicitly")?;
    Ok(last.trim_end_matches(".git").to_string())
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<()> {
    let mut command = tokio::process::Command::new("git");
    command.args(args).current_dir(cwd);
    match supervise_command(command)
        .await
        .context("failed to run git")?
    {
        ChildOutcome::Exited(status) => {
            if !status.success() {
                bail!("git {} failed with {status}", args.join(" "));
            }
        }
        ChildOutcome::Interrupted(signo) => return Err(InterruptedSignal(signo).into()),
    }
    Ok(())
}

fn run_git_capture(repo: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo).args(args);
    set_parent_death_signal_std(&mut command);
    let output = command.output().context("failed to run git")?;
    if !output.status.success() {
        bail!(
            "git {} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn ls_tree(repo: &Path) -> Result<Vec<TreeRow>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        .args(["ls-tree", "-r", "-z", "HEAD"]);
    set_parent_death_signal_std(&mut command);
    let output = command.output().context("failed to run git ls-tree")?;
    if !output.status.success() {
        bail!(
            "git ls-tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let mut rows = Vec::new();
    for record in output.stdout.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let record = std::str::from_utf8(record).context("non-UTF-8 path in repository")?;
        let (meta, path) = record
            .split_once('\t')
            .context("malformed ls-tree record")?;
        let mut fields = meta.split(' ');
        let mode = u32::from_str_radix(fields.next().context("missing mode")?, 8)?;
        let _objtype = fields.next().context("missing object type")?;
        let sha = fields.next().context("missing object id")?;
        if mode == MODE_GITLINK {
            bail!("repository contains submodules ({path}); not supported by agentfs clone");
        }
        if sha.len() != 40 {
            bail!("non-SHA-1 repository detected; not supported by agentfs clone");
        }
        rows.push(TreeRow {
            mode,
            sha: sha.to_string(),
            path: path.to_string(),
        });
    }
    Ok(rows)
}

/// Synthesize one import entry per parent directory, first-seen order.
/// `ls-tree -r` emits paths in index order, so parents always precede
/// children. Also validates every row's tree entry mode so the streaming
/// pipeline never starts for an unsupported repository.
fn dir_entries(rows: &[TreeRow]) -> Result<Vec<ImportEntry>> {
    let mut entries = Vec::new();
    let mut known_dirs: HashSet<&str> = HashSet::new();

    for row in rows {
        match row.mode {
            MODE_FILE | MODE_EXEC | MODE_SYMLINK => {}
            // Tolerate historical non-canonical modes git itself normalizes.
            other => bail!("unsupported tree entry mode {other:o} for {}", row.path),
        }
        let mut offset = 0;
        while let Some(pos) = row.path[offset..].find('/') {
            let dir = &row.path[..offset + pos];
            if known_dirs.insert(dir) {
                entries.push(ImportEntry {
                    path: dir.to_string(),
                    mode: S_IFDIR | 0o755,
                    data: Vec::new(),
                });
            }
            offset += pos + 1;
        }
    }
    Ok(entries)
}

/// Producer half of the streaming import: fetch every unique blob via one
/// `git cat-file --batch` process (a writer thread feeds requests so neither
/// side blocks on a full pipe), fan each blob out to the tree rows that
/// reference it, and send bounded chunks of import entries down `tx` as they
/// accumulate. Returns the total content bytes emitted.
fn spawn_blob_producer(
    repo: std::path::PathBuf,
    rows: &[TreeRow],
    tx: tokio::sync::mpsc::Sender<Vec<ImportEntry>>,
) -> Result<std::thread::JoinHandle<Result<u64>>> {
    // sha -> (path, mode) fanout, plus unique shas in first-seen order.
    let mut unique: Vec<String> = Vec::new();
    let mut fanout: HashMap<String, Vec<(String, u32)>> = HashMap::new();
    for row in rows {
        let refs = fanout.entry(row.sha.clone()).or_insert_with(|| {
            unique.push(row.sha.clone());
            Vec::new()
        });
        refs.push((row.path.clone(), row.mode));
    }

    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    set_parent_death_signal_std(&mut command);
    let mut child = command
        .spawn()
        .context("failed to spawn git cat-file --batch")?;
    let mut stdin = child.stdin.take().context("missing cat-file stdin")?;
    let stdout = child.stdout.take().context("missing cat-file stdout")?;

    let requests = unique.clone();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        for sha in &requests {
            stdin.write_all(sha.as_bytes())?;
            stdin.write_all(b"\n")?;
        }
        Ok(())
    });

    let handle = std::thread::spawn(move || -> Result<u64> {
        let streamed = stream_blobs(&unique, &mut fanout, stdout, &tx);
        if streamed.is_err() {
            // Consumer went away or the stream broke; don't leave a git
            // process wedged on a dead pipe.
            let _ = child.kill();
        }
        let writer_result = writer
            .join()
            .map_err(|_| anyhow::anyhow!("cat-file writer thread panicked"));
        let status = child.wait()?;
        let bytes = streamed?;
        writer_result??;
        if !status.success() {
            bail!("git cat-file --batch failed with {status}");
        }
        Ok(bytes)
    });
    Ok(handle)
}

/// Parse `cat-file --batch` output blob by blob, emitting bounded chunks.
fn stream_blobs(
    unique: &[String],
    fanout: &mut HashMap<String, Vec<(String, u32)>>,
    stdout: std::process::ChildStdout,
    tx: &tokio::sync::mpsc::Sender<Vec<ImportEntry>>,
) -> Result<u64> {
    const CHUNK_BYTES: usize = 4 * 1024 * 1024;
    const CHUNK_ENTRIES: usize = 512;

    let mut stdout = BufReader::new(stdout);
    let mut chunk: Vec<ImportEntry> = Vec::new();
    let mut chunk_bytes = 0usize;
    let mut total_bytes = 0u64;

    for sha in unique {
        let mut header = String::new();
        stdout.read_line(&mut header)?;
        let mut fields = header.trim_end().split(' ');
        let echoed = fields.next().unwrap_or_default();
        let kind = fields.next().unwrap_or_default();
        if kind == "missing" || echoed != sha.as_str() {
            bail!("git cat-file returned unexpected header for {sha}: {header}");
        }
        let size: usize = fields
            .next()
            .context("missing blob size")?
            .parse()
            .context("invalid blob size")?;
        let mut data = vec![0u8; size];
        stdout.read_exact(&mut data)?;
        let mut newline = [0u8; 1];
        stdout.read_exact(&mut newline)?;

        let refs = fanout
            .remove(sha.as_str())
            .with_context(|| format!("no tree rows reference blob {sha}"))?;
        let last = refs.len() - 1;
        for (index, (path, mode)) in refs.into_iter().enumerate() {
            let data = if index == last {
                std::mem::take(&mut data)
            } else {
                data.clone()
            };
            total_bytes += data.len() as u64;
            chunk_bytes += data.len();
            chunk.push(ImportEntry { path, mode, data });
            if chunk_bytes >= CHUNK_BYTES || chunk.len() >= CHUNK_ENTRIES {
                tx.blocking_send(std::mem::take(&mut chunk))
                    .map_err(|_| anyhow::anyhow!("import consumer stopped"))?;
                chunk_bytes = 0;
            }
        }
    }
    if !chunk.is_empty() {
        tx.blocking_send(chunk)
            .map_err(|_| anyhow::anyhow!("import consumer stopped"))?;
    }
    Ok(total_bytes)
}

/// Serialize a git index (version 2) whose cached stat data matches exactly
/// what the filesystem serves for the imported files, so the first
/// `git status` is clean without re-reading content.
fn build_index_v2(
    rows: &[TreeRow],
    imported: &[ImportedEntry],
    timestamp: (i64, i64),
    uid: u32,
    gid: u32,
    dev: u64,
) -> Result<Vec<u8>> {
    let by_path: HashMap<&str, &ImportedEntry> = imported
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();

    let mut sorted: Vec<&TreeRow> = rows.iter().collect();
    sorted.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));

    let mut buf = Vec::with_capacity(64 + sorted.len() * 80);
    buf.extend_from_slice(b"DIRC");
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&(sorted.len() as u32).to_be_bytes());

    let (ts_secs, ts_nsec) = timestamp;
    for row in sorted {
        let node = by_path
            .get(row.path.as_str())
            .with_context(|| format!("imported entry missing for {}", row.path))?;

        let entry_start = buf.len();
        for value in [
            ts_secs as u32,
            ts_nsec as u32,
            ts_secs as u32,
            ts_nsec as u32,
            dev as u32,
            node.ino as u32,
            row.mode,
            uid,
            gid,
            node.size as u32,
        ] {
            buf.extend_from_slice(&value.to_be_bytes());
        }
        let sha = hex::decode(&row.sha).context("invalid object id")?;
        buf.extend_from_slice(&sha);
        let name_len = row.path.len().min(0xFFF) as u16;
        buf.extend_from_slice(&name_len.to_be_bytes());
        buf.extend_from_slice(row.path.as_bytes());
        // Pad with 1-8 NUL bytes so the entry length is a multiple of 8.
        let entry_len = buf.len() - entry_start;
        let pad = 8 - (entry_len % 8);
        buf.extend_from_slice(&[0u8; 8][..pad]);
    }

    let digest = Sha1::digest(&buf);
    buf.extend_from_slice(&digest);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_repo_name_handles_common_shapes() {
        assert_eq!(
            derive_repo_name("https://github.com/foo/bar.git").unwrap(),
            "bar"
        );
        assert_eq!(derive_repo_name("/tmp/mirrors/baz/").unwrap(), "baz");
        assert_eq!(derive_repo_name("git@host:owner/repo.git").unwrap(), "repo");
    }

    #[test]
    fn index_entries_are_eight_byte_aligned_with_trailer() {
        let rows = vec![TreeRow {
            mode: MODE_FILE,
            sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            path: "a.txt".to_string(),
        }];
        let imported = vec![ImportedEntry {
            path: "a.txt".to_string(),
            ino: 42,
            mode: 0o100644,
            size: 5,
        }];
        let index = build_index_v2(&rows, &imported, (1, 2), 1000, 1000, 7).unwrap();
        assert_eq!(&index[..4], b"DIRC");
        // header 12 + (fixed 62 + path 5 = 67, padded to 72) + sha1 trailer 20.
        assert_eq!(index.len(), 12 + 72 + 20);
        let expected = Sha1::digest(&index[..index.len() - 20]);
        assert_eq!(&index[index.len() - 20..], expected.as_slice());
    }
}
