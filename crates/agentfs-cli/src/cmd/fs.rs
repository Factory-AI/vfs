use std::collections::VecDeque;

use agentfs_core::{AgentFSOptions, EncryptionConfig};
use anyhow::{Context, Result as AnyhowResult};
use turso::Value;

use crate::cmd::init::{finalize_readonly, open_agentfs};

const ROOT_INO: i64 = 1;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

pub async fn ls_filesystem(
    stdout: &mut impl std::io::Write,
    id_or_path: String,
    path: &str,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let mut options = AgentFSOptions::resolve(&id_or_path)?;
    if let Some((key, cipher)) = encryption {
        options = options.with_encryption(EncryptionConfig {
            hex_key: key.clone(),
            cipher: cipher.clone(),
        });
    }
    eprintln!("Using agent: {}", id_or_path);

    let agentfs = open_agentfs(options)
        .await
        .map_err(|err| super::migrate::open_error_with_guidance(err, &id_or_path))?;
    let result = ls_opened(stdout, &agentfs, path).await;
    finalize_readonly(&agentfs).await;
    result
}

async fn ls_opened(
    stdout: &mut impl std::io::Write,
    agentfs: &agentfs_core::AgentFS,
    path: &str,
) -> AnyhowResult<()> {
    let conn = agentfs.get_connection().await?;

    let start_ino = resolve_directory_ino(&conn, path).await?;

    let mut queue: VecDeque<(i64, String)> = VecDeque::new();
    queue.push_back((start_ino, String::new()));

    while let Some((parent_ino, prefix)) = queue.pop_front() {
        let query = format!(
            "SELECT d.name, d.ino, i.mode FROM fs_dentry d
             JOIN fs_inode i ON d.ino = i.ino
             WHERE d.parent_ino = {}
             ORDER BY d.name",
            parent_ino
        );

        let mut rows = conn
            .query(&query, ())
            .await
            .context("Failed to query directory entries")?;

        let mut entries = Vec::new();
        while let Some(row) = rows.next().await.context("Failed to fetch row")? {
            let name: String = row
                .get_value(0)
                .ok()
                .and_then(|v| {
                    if let Value::Text(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            let ino: i64 = row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0);

            let mode: u32 = row
                .get_value(2)
                .ok()
                .and_then(|v| v.as_integer().copied())
                .unwrap_or(0) as u32;

            entries.push((name, ino, mode));
        }

        for (name, ino, mode) in entries {
            let is_dir = mode & S_IFMT == S_IFDIR;
            let type_char = if is_dir { 'd' } else { 'f' };
            let full_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };

            stdout
                .write_fmt(format_args!("{} {}\n", type_char, full_path))
                .context("Failed to write to stdout")?;

            if is_dir {
                queue.push_back((ino, full_path));
            }
        }
    }

    Ok(())
}

/// Resolve a directory path (absolute or relative to `/`) to its inode.
async fn resolve_directory_ino(conn: &turso::Connection, path: &str) -> AnyhowResult<i64> {
    let mut ino = ROOT_INO;
    let mut mode = S_IFDIR;

    for component in path.split('/').filter(|component| !component.is_empty()) {
        let mut rows = conn
            .query(
                "SELECT d.ino, i.mode FROM fs_dentry d
                 JOIN fs_inode i ON d.ino = i.ino
                 WHERE d.parent_ino = ? AND d.name = ?",
                (ino, component),
            )
            .await
            .context("Failed to query directory entry")?;
        let Some(row) = rows.next().await.context("Failed to fetch row")? else {
            anyhow::bail!("Path not found: {}", path);
        };
        ino = row
            .get_value(0)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .with_context(|| format!("Corrupt dentry ino for {}", path))?;
        mode = row
            .get_value(1)
            .ok()
            .and_then(|v| v.as_integer().copied())
            .with_context(|| format!("Corrupt inode mode for {}", path))? as u32;
    }

    if mode & S_IFMT != S_IFDIR {
        anyhow::bail!("Not a directory: {}", path);
    }
    Ok(ino)
}

pub async fn cat_filesystem(
    stdout: &mut impl std::io::Write,
    id_or_path: String,
    path: &str,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let mut options = AgentFSOptions::resolve(&id_or_path)?;
    if let Some((key, cipher)) = encryption {
        options = options.with_encryption(EncryptionConfig {
            hex_key: key.clone(),
            cipher: cipher.clone(),
        });
    }
    let agentfs = open_agentfs(options)
        .await
        .map_err(|err| super::migrate::open_error_with_guidance(err, &id_or_path))?;

    let result: AnyhowResult<()> = async {
        match agentfs.fs.read_file(path).await? {
            Some(file) => {
                stdout.write_all(&file)?;
                Ok(())
            }
            None => anyhow::bail!("File not found: {}", path),
        }
    }
    .await;
    finalize_readonly(&agentfs).await;
    result
}

pub async fn write_filesystem(
    id_or_path: String,
    path: &str,
    content: &str,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let mut options = AgentFSOptions::resolve(&id_or_path)?;
    if let Some((key, cipher)) = encryption {
        options = options.with_encryption(EncryptionConfig {
            hex_key: key.clone(),
            cipher: cipher.clone(),
        });
    }
    let agentfs = open_agentfs(options)
        .await
        .map_err(|err| super::migrate::open_error_with_guidance(err, &id_or_path))?;

    // Own created entries as the invoking user, matching mount-created files:
    // uid/gid 0 would make a later chmod inside `agentfs exec` (running as the
    // invoking uid) fail with EPERM.
    // SAFETY: getuid/getgid are always safe
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut components = path.split("/").collect::<Vec<_>>();
    if !path.starts_with("/") {
        components.insert(0, "");
    }
    // /a/b/c is split to ["", "a", "b", "c"]
    // we must start with /a (first TWO entries)
    for i in 2..components.len() {
        let dir_path = components[0..i].join("/");
        if agentfs.fs.stat(&dir_path).await?.is_none() {
            agentfs.fs.mkdir(&dir_path, uid, gid).await?;
        }
    }
    // Remove file if it exists (overwrite behavior)
    if agentfs.fs.stat(path).await?.is_some() {
        agentfs.fs.remove(path).await?;
    }
    let (_, file) = agentfs
        .fs
        .create_file(path, S_IFREG | 0o644, uid, gid)
        .await?;
    file.pwrite(0, content.as_bytes()).await?;
    // Tier Four: writes go into the in-memory batcher first. This CLI is a
    // one-shot operation — finalize so the bytes are durable in SQLite before
    // we drop the AgentFS (otherwise a subsequent process or `cat` against
    // the same path would see only the pre-write state) and the database
    // family is single-file again (invariant I1) instead of keeping the
    // truncated 0-byte -wal a bare drain leaves behind.
    agentfs.fs.finalize().await?;
    Ok(())
}

/// Represents a change type in the overlay filesystem
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChangeType {
    Added,
    Modified,
    Deleted,
}

impl std::fmt::Display for ChangeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeType::Added => write!(f, "A"),
            ChangeType::Modified => write!(f, "M"),
            ChangeType::Deleted => write!(f, "D"),
        }
    }
}

/// Get file type character
fn file_type_char(mode: u32) -> char {
    match mode & S_IFMT {
        S_IFDIR => 'd',
        S_IFLNK => 'l',
        S_IFREG => 'f',
        _ => '?',
    }
}

/// Check if a path exists in the host filesystem (base layer)
fn path_exists_in_base(base_path: &str, rel_path: &str) -> bool {
    let full_path = format!("{}{}", base_path, rel_path);
    std::path::Path::new(&full_path).exists()
}

pub async fn diff_filesystem(id_or_path: String) -> AnyhowResult<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    eprintln!("Using agent: {}", id_or_path);

    let agent = open_agentfs(options)
        .await
        .map_err(|err| super::migrate::open_error_with_guidance(err, &id_or_path))?;
    let result = diff_opened(&agent).await;
    finalize_readonly(&agent).await;
    result
}

async fn diff_opened(agent: &agentfs_core::AgentFS) -> AnyhowResult<()> {
    // Check if overlay is enabled
    let base_path = match agent.is_overlay_enabled().await? {
        Some(path) => path,
        None => {
            println!("No diff (non-overlay filesystem)");
            return Ok(());
        }
    };

    eprintln!("Base: {}", base_path);

    // Collect all changes
    let mut changes: Vec<(ChangeType, char, String)> = Vec::new();

    // Get all paths in delta layer
    let delta_paths = agent.get_delta_paths().await?;

    // Get all whiteouts (deleted paths)
    let whiteouts = agent.get_whiteouts().await?;

    // Process delta paths - determine if added or modified
    for path in &delta_paths {
        let mode = agent.get_file_mode(path).await?.unwrap_or(0);
        let type_char = file_type_char(mode);

        if path_exists_in_base(&base_path, path) {
            // File exists in both - it was modified (copy-on-write)
            changes.push((ChangeType::Modified, type_char, path.clone()));
        } else {
            // File only exists in delta - it was added
            changes.push((ChangeType::Added, type_char, path.clone()));
        }
    }

    // Process whiteouts (deleted files)
    for path in &whiteouts {
        // Determine file type from base if possible, otherwise use '?'
        let full_path = format!("{}{}", base_path, path);
        let base_path_obj = std::path::Path::new(&full_path);
        let type_char = if base_path_obj.is_dir() {
            'd'
        } else if base_path_obj.is_symlink() {
            'l'
        } else if base_path_obj.is_file() {
            'f'
        } else {
            '?'
        };

        changes.push((ChangeType::Deleted, type_char, path.clone()));
    }

    // Sort changes by path for consistent output
    changes.sort_by(|a, b| a.2.cmp(&b.2));

    // Print changes
    if changes.is_empty() {
        println!("No changes");
    } else {
        for (change_type, type_char, path) in changes {
            println!("{} {} {}", change_type, type_char, path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use agentfs_core::{AgentFS, AgentFSOptions, EncryptionConfig};
    use tempfile::NamedTempFile;

    use crate::cmd::fs::{cat_filesystem, ls_filesystem, write_filesystem};

    const TEST_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TEST_CIPHER: &str = "aes256gcm";

    async fn agentfs() -> (AgentFS, String, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let agentfs = AgentFS::open(AgentFSOptions::with_path(path.to_string()))
            .await
            .unwrap();
        (agentfs, file.path().to_str().unwrap().to_string(), file)
    }

    async fn encrypted_agentfs() -> (AgentFS, String, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let agentfs = AgentFS::open(AgentFSOptions::with_path(path.to_string()).with_encryption(
            EncryptionConfig {
                hex_key: TEST_KEY.to_string(),
                cipher: TEST_CIPHER.to_string(),
            },
        ))
        .await
        .unwrap();
        (agentfs, file.path().to_str().unwrap().to_string(), file)
    }

    const S_IFREG: u32 = 0o100000;

    #[tokio::test]
    async fn cat_file_not_found() {
        let (_agentfs, path, _file) = agentfs().await;
        let mut buf = Vec::new();
        let err = cat_filesystem(&mut buf, path, "test.md", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("File not found"));
    }

    #[tokio::test]
    async fn cat_file_found() {
        let (agentfs, path, _file) = agentfs().await;
        let content = b"hello, agentfs";
        write_file(&agentfs.fs, "test.md", content, 0, 0)
            .await
            .unwrap();
        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "test.md", None)
            .await
            .unwrap();
        assert_eq!(buf, content);
    }

    #[tokio::test]
    async fn cat_big_file_found() {
        let (agentfs, path, _file) = agentfs().await;
        let content = vec![100u8; 4 * 1024 * 1024];
        write_file(&agentfs.fs, "test.md", &content, 0, 0)
            .await
            .unwrap();
        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "test.md", None)
            .await
            .unwrap();
        assert_eq!(buf, content);
    }

    #[tokio::test]
    async fn ls_empty() {
        let (_agentfs, path, _file) = agentfs().await;
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/", None).await.unwrap();
        assert_eq!(buf, b"");
    }

    #[tokio::test]
    async fn ls_files_only() {
        let (agentfs, path, _file) = agentfs().await;
        write_file(&agentfs.fs, "1.md", b"1", 0, 0).await.unwrap();
        write_file(&agentfs.fs, "2.md", b"11", 0, 0).await.unwrap();
        let big = vec![100u8; 1024 * 1024];
        write_file(&agentfs.fs, "3.md", &big, 0, 0).await.unwrap();
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/", None).await.unwrap();
        assert_eq!(
            buf,
            b"f 1.md
f 2.md
f 3.md
"
        );
    }

    #[tokio::test]
    async fn ls_dirs() {
        let (agentfs, path, _file) = agentfs().await;
        agentfs.fs.mkdir("a", 0, 0).await.unwrap();
        agentfs.fs.mkdir("a/b", 0, 0).await.unwrap();
        agentfs.fs.mkdir("a/c", 0, 0).await.unwrap();
        agentfs.fs.mkdir("d", 0, 0).await.unwrap();
        agentfs.fs.mkdir("d/e", 0, 0).await.unwrap();
        write_file(&agentfs.fs, "a/b/1.md", b"1", 0, 0)
            .await
            .unwrap();
        write_file(&agentfs.fs, "a/c/2.md", b"11", 0, 0)
            .await
            .unwrap();
        let big = vec![100u8; 1024 * 1024];
        write_file(&agentfs.fs, "d/e/3.md", &big, 0, 0)
            .await
            .unwrap();
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/", None).await.unwrap();
        assert_eq!(
            buf,
            b"d a
d d
d a/b
d a/c
d d/e
f a/b/1.md
f a/c/2.md
f d/e/3.md
"
        );
    }

    #[tokio::test]
    async fn ls_subdir_lists_only_subtree() {
        let (agentfs, path, _file) = agentfs().await;
        agentfs.fs.mkdir("a", 0, 0).await.unwrap();
        agentfs.fs.mkdir("a/b", 0, 0).await.unwrap();
        agentfs.fs.mkdir("a/c", 0, 0).await.unwrap();
        agentfs.fs.mkdir("d", 0, 0).await.unwrap();
        write_file(&agentfs.fs, "a/b/1.md", b"1", 0, 0)
            .await
            .unwrap();
        write_file(&agentfs.fs, "a/c/2.md", b"11", 0, 0)
            .await
            .unwrap();
        write_file(&agentfs.fs, "d/3.md", b"111", 0, 0)
            .await
            .unwrap();
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path.clone(), "/a", None)
            .await
            .unwrap();
        assert_eq!(
            buf,
            b"d b
d c
f b/1.md
f c/2.md
"
        );

        // Relative paths resolve the same as absolute ones.
        let mut relative_buf = Vec::new();
        ls_filesystem(&mut relative_buf, path.clone(), "a/b", None)
            .await
            .unwrap();
        assert_eq!(relative_buf, b"f 1.md\n");
    }

    #[tokio::test]
    async fn ls_missing_path_errors() {
        let (_agentfs, path, _file) = agentfs().await;
        let mut buf = Vec::new();
        let err = ls_filesystem(&mut buf, path, "/missing", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Path not found"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn ls_file_path_errors() {
        let (agentfs, path, _file) = agentfs().await;
        write_file(&agentfs.fs, "file.md", b"1", 0, 0)
            .await
            .unwrap();
        let mut buf = Vec::new();
        let err = ls_filesystem(&mut buf, path, "/file.md", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Not a directory"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn read_commands_leave_no_wal_sidecar() {
        let (agentfs, path, _file) = agentfs().await;
        write_file(&agentfs.fs, "test.md", b"1", 0, 0)
            .await
            .unwrap();
        drop(agentfs);
        let wal = format!("{path}-wal");

        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path.clone(), "/", None)
            .await
            .unwrap();
        assert!(
            !std::path::Path::new(&wal).exists(),
            "fs ls must not leave a WAL sidecar"
        );

        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path.clone(), "test.md", None)
            .await
            .unwrap();
        assert!(
            !std::path::Path::new(&wal).exists(),
            "fs cat must not leave a WAL sidecar"
        );

        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path.clone(), "missing.md", None)
            .await
            .unwrap_err();
        assert!(
            !std::path::Path::new(&wal).exists(),
            "a failing fs cat must not leave a WAL sidecar"
        );
    }

    // Encryption tests

    #[tokio::test]
    async fn encrypted_write_and_cat() {
        let (agentfs, path, _file) = encrypted_agentfs().await;
        let content = b"encrypted content";
        write_file(&agentfs.fs, "secret.txt", content, 0, 0)
            .await
            .unwrap();
        drop(agentfs);

        let encryption = Some((TEST_KEY.to_string(), TEST_CIPHER.to_string()));
        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "secret.txt", encryption.as_ref())
            .await
            .unwrap();
        assert_eq!(buf, content);
    }

    #[tokio::test]
    async fn encrypted_ls() {
        let (agentfs, path, _file) = encrypted_agentfs().await;
        write_file(&agentfs.fs, "file1.txt", b"1", 0, 0)
            .await
            .unwrap();
        write_file(&agentfs.fs, "file2.txt", b"2", 0, 0)
            .await
            .unwrap();
        drop(agentfs);

        let encryption = Some((TEST_KEY.to_string(), TEST_CIPHER.to_string()));
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/", encryption.as_ref())
            .await
            .unwrap();
        assert_eq!(buf, b"f file1.txt\nf file2.txt\n");
    }

    #[tokio::test]
    async fn write_filesystem_owns_entries_as_invoking_user() {
        let (_agentfs, path, _file) = agentfs().await;
        write_filesystem(path.clone(), "/subdir/owned.txt", "content", None)
            .await
            .unwrap();

        // uid/gid 0 entries make a later chmod inside `agentfs exec` (running
        // as the invoking uid) fail EPERM, unlike mount-created files.
        // SAFETY: getuid/getgid are always safe
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let reader = AgentFS::open(AgentFSOptions::with_path(path))
            .await
            .unwrap();
        let file_stats = reader.fs.stat("/subdir/owned.txt").await.unwrap().unwrap();
        assert_eq!(
            (file_stats.uid, file_stats.gid),
            (uid, gid),
            "fs write must create files owned by the invoking uid/gid"
        );
        let dir_stats = reader.fs.stat("/subdir").await.unwrap().unwrap();
        assert_eq!(
            (dir_stats.uid, dir_stats.gid),
            (uid, gid),
            "fs write must create intermediate directories owned by the invoking uid/gid"
        );
    }

    #[tokio::test]
    async fn encrypted_write_filesystem() {
        let (_agentfs, path, _file) = encrypted_agentfs().await;

        let encryption = Some((TEST_KEY.to_string(), TEST_CIPHER.to_string()));
        write_filesystem(
            path.clone(),
            "/new_file.txt",
            "new content",
            encryption.as_ref(),
        )
        .await
        .unwrap();

        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "/new_file.txt", encryption.as_ref())
            .await
            .unwrap();
        assert_eq!(buf, b"new content");
    }

    async fn write_file(
        fs: &agentfs_core::fs::AgentFS,
        path: &str,
        data: &[u8],
        uid: u32,
        gid: u32,
    ) -> anyhow::Result<()> {
        if fs.stat(path).await?.is_some() {
            fs.remove(path).await?;
        }
        let (_, file) = fs.create_file(path, S_IFREG | 0o644, uid, gid).await?;
        file.pwrite(0, data).await?;
        // Tier Four: cat_filesystem opens a fresh AgentFS at the same path.
        // That second instance only sees what's durable in SQLite, so the
        // writer must flush its batcher before another opener can read.
        fs.drain_all().await?;
        Ok(())
    }
}
