use agentfs_core::{
    error::Error as SdkError, AgentFSOptions, FileSystem, HostFS, OverlayFS, PartialOriginPolicy,
};
use agentfs_mount::{mount_fs, Backend, MountHandle, MountOpts};
use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use turso::value::Value;

#[cfg(target_os = "linux")]
use agentfs_core::{get_mounts, Mount};
#[cfg(target_os = "linux")]
use std::{
    io::{self, Write},
    os::unix::fs::MetadataExt,
};

#[cfg(target_os = "linux")]
use crate::cmd::init::open_agentfs;

use crate::opts::MountBackend;

/// Arguments for the mount command.
#[derive(Debug, Clone)]
pub struct MountArgs {
    /// The agent filesystem ID or path.
    pub id_or_path: String,
    /// The mountpoint path.
    pub mountpoint: PathBuf,
    /// Automatically unmount when the process exits.
    pub auto_unmount: bool,
    /// Allow root to access the mount.
    pub allow_root: bool,
    /// Allow other system users to access the mount.
    pub allow_other: bool,
    /// Run in foreground (don't daemonize).
    pub foreground: bool,
    /// User ID to report for all files (defaults to current user).
    pub uid: Option<u32>,
    /// Group ID to report for all files (defaults to current group).
    pub gid: Option<u32>,
    /// The mount backend to use (fuse or nfs).
    pub backend: MountBackend,
    /// Partial-origin policy for overlay copy-up.
    pub partial_origin_policy: Option<PartialOriginPolicy>,
}

/// Mount the agent filesystem (Linux).
#[cfg(target_os = "linux")]
pub fn mount(args: MountArgs) -> Result<()> {
    match args.backend {
        MountBackend::Fuse => mount_fuse(args),
        MountBackend::Nfs => {
            let rt = crate::get_runtime();
            rt.block_on(mount_nfs_backend(args))
        }
    }
}

/// Mount the agent filesystem (macOS).
#[cfg(target_os = "macos")]
pub fn mount(args: MountArgs) -> Result<()> {
    match args.backend {
        MountBackend::Fuse => {
            anyhow::bail!(
                "FUSE mounting is not supported on macOS.\n\
                 Use --backend nfs (default) or `agentfs nfs` instead."
            );
        }
        MountBackend::Nfs => {
            let rt = crate::get_runtime();
            rt.block_on(mount_nfs_backend(args))
        }
    }
}

/// Mount the agent filesystem using FUSE (Linux only).
#[cfg(target_os = "linux")]
fn mount_fuse(args: MountArgs) -> Result<()> {
    let opts = AgentFSOptions::resolve(&args.id_or_path)?;

    // Check schema version before daemonizing. This allows us to show the error
    // message to the user directly, rather than having it appear in daemon logs.
    {
        let rt = crate::get_runtime();
        let db_path = opts.db_path()?;
        let result = rt.block_on(ensure_schema_current_for_mount_precheck(&db_path));
        if let Err(SdkError::SchemaVersionMismatch { found, expected }) = result {
            exit_schema_version_mismatch(&found, &expected, &args.id_or_path);
        }
    }

    let fsname = format!(
        "agentfs:{}",
        std::fs::canonicalize(&args.id_or_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| args.id_or_path.clone())
    );

    if !args.mountpoint.exists() {
        anyhow::bail!("Mountpoint does not exist: {}", args.mountpoint.display());
    }

    let mountpoint = std::fs::canonicalize(args.mountpoint.clone())?;
    let mountpoint_ino = {
        use anyhow::Context as _;
        std::fs::metadata(mountpoint.clone())
            .context("Failed to get mountpoint inode")?
            .ino()
    };

    let mount_opts = MountOpts {
        mountpoint: args.mountpoint.clone(),
        backend: Backend::Fuse,
        auto_unmount: args.auto_unmount,
        allow_root: args.allow_root,
        allow_other: args.allow_other,
        fsname,
        uid: args.uid,
        gid: args.gid,
        lazy_unmount: true,
        timeout: std::time::Duration::from_secs(10),
    };

    let id_or_path = args.id_or_path.clone();
    let foreground = args.foreground;
    let partial_origin_policy = args.partial_origin_policy;
    let mount = move || {
        let rt = crate::get_runtime();
        let agentfs = match rt.block_on(open_agentfs(opts)) {
            Ok(fs) => fs,
            Err(SdkError::SchemaVersionMismatch { found, expected }) => {
                exit_schema_version_mismatch(&found, &expected, &id_or_path);
            }
            Err(e) => return Err(e.into()),
        };

        // Check for overlay configuration
        let fs: Arc<dyn FileSystem> = rt.block_on(async {
            // Query base_path in a separate scope so connection is released
            let base_path: Option<String> = {
                let conn = agentfs.get_connection().await?;
                let query = "SELECT value FROM fs_overlay_config WHERE key = 'base_path'";
                match conn.query(query, ()).await {
                    Ok(mut rows) => {
                        if let Ok(Some(row)) = rows.next().await {
                            row.get_value(0).ok().and_then(|v| {
                                if let Value::Text(s) = v {
                                    Some(s.clone())
                                } else {
                                    None
                                }
                            })
                        } else {
                            None
                        }
                    }
                    Err(_) => None, // Table doesn't exist or query failed
                }
            }; // conn is dropped here

            if let Some(base_path) = base_path {
                // Create OverlayFS with HostFS base, loading existing whiteouts
                eprintln!("Using overlay filesystem with base: {}", base_path);
                let hostfs = HostFS::new(&base_path)?;
                let hostfs = hostfs.with_fuse_mountpoint(mountpoint_ino);
                let overlay = if let Some(policy) = partial_origin_policy {
                    OverlayFS::new_with_partial_origin_policy(Arc::new(hostfs), agentfs.fs, policy)
                } else {
                    OverlayFS::new(Arc::new(hostfs), agentfs.fs)
                };
                overlay.load().await?; // Load persisted whiteouts and origin mappings
                Ok::<Arc<dyn FileSystem>, anyhow::Error>(Arc::new(overlay))
            } else {
                // Plain AgentFS
                Ok(Arc::new(agentfs.fs) as Arc<dyn FileSystem>)
            }
        })?;

        rt.block_on(run_mount_session(fs, mount_opts, foreground))
    };

    if foreground {
        mount()
    } else {
        agentfs_mount::daemon::daemonize(
            mount,
            move || agentfs_mount::is_mountpoint(&mountpoint),
            std::time::Duration::from_secs(10),
        )
    }
}

#[cfg(target_os = "linux")]
async fn ensure_schema_current_for_mount_precheck(
    db_path: &str,
) -> std::result::Result<(), SdkError> {
    let db = turso::Builder::new_local(db_path).build().await?;
    let conn = db.connect()?;
    agentfs_core::schema::ensure_current(&conn).await
}

/// Mount the agent filesystem using NFS over localhost.
async fn mount_nfs_backend(args: MountArgs) -> Result<()> {
    use crate::cmd::init::open_agentfs;

    let opts = AgentFSOptions::resolve(&args.id_or_path)?;

    if !args.mountpoint.exists() {
        anyhow::bail!("Mountpoint does not exist: {}", args.mountpoint.display());
    }

    let mountpoint = std::fs::canonicalize(args.mountpoint.clone())?;

    let fsname = format!(
        "agentfs:{}",
        std::fs::canonicalize(&args.id_or_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| args.id_or_path.clone())
    );

    // Open AgentFS
    let agentfs = match open_agentfs(opts).await {
        Ok(fs) => fs,
        Err(SdkError::SchemaVersionMismatch { found, expected }) => {
            exit_schema_version_mismatch(&found, &expected, &args.id_or_path);
        }
        Err(e) => return Err(e.into()),
    };

    // Check for overlay configuration
    // Query base_path in a separate scope so connection is released before load_whiteouts
    let base_path: Option<String> = {
        let conn = agentfs.get_connection().await?;
        let query = "SELECT value FROM fs_overlay_config WHERE key = 'base_path'";
        match conn.query(query, ()).await {
            Ok(mut rows) => {
                if let Ok(Some(row)) = rows.next().await {
                    row.get_value(0).ok().and_then(|v| {
                        if let Value::Text(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            }
            Err(_) => None, // Table doesn't exist or query failed
        }
    }; // conn is dropped here

    let fs: Arc<dyn FileSystem> = if let Some(base_path) = base_path {
        // Create OverlayFS with HostFS base, loading existing whiteouts
        eprintln!("Using overlay filesystem with base: {}", base_path);
        let hostfs = HostFS::new(&base_path)?;
        let overlay = if let Some(policy) = args.partial_origin_policy {
            OverlayFS::new_with_partial_origin_policy(Arc::new(hostfs), agentfs.fs, policy)
        } else {
            OverlayFS::new(Arc::new(hostfs), agentfs.fs)
        };
        overlay.load().await?; // Load persisted whiteouts and origin mappings
        Arc::new(overlay) as Arc<dyn FileSystem>
    } else {
        // Plain AgentFS
        Arc::new(agentfs.fs) as Arc<dyn FileSystem>
    };

    let mount_opts = MountOpts {
        mountpoint: mountpoint.clone(),
        backend: Backend::Nfs,
        fsname,
        uid: args.uid,
        gid: args.gid,
        allow_other: args.allow_other,
        allow_root: args.allow_root,
        auto_unmount: args.auto_unmount,
        lazy_unmount: true,
        timeout: std::time::Duration::from_secs(10),
    };

    if args.foreground {
        run_mount_session(fs, mount_opts, true).await
    } else {
        let ready_mountpoint = mountpoint.clone();
        agentfs_mount::daemon::daemonize(
            move || {
                let rt = crate::get_runtime();
                rt.block_on(run_mount_session(fs, mount_opts, false))
            },
            move || agentfs_mount::is_mountpoint(&ready_mountpoint),
            std::time::Duration::from_secs(10),
        )
    }
}

async fn run_mount_session(
    fs: Arc<dyn FileSystem>,
    mount_opts: MountOpts,
    foreground: bool,
) -> Result<()> {
    let handle = mount_fs(fs, mount_opts).await?;
    if foreground {
        eprintln!("Mounted at {}", handle.mountpoint().display());
        eprintln!("Press Ctrl+C to unmount and exit.");
    }

    let _interrupted = wait_for_mount_end(&handle).await?;
    handle.unmount().await
}

async fn wait_for_mount_end(handle: &MountHandle) -> Result<bool> {
    tokio::select! {
        result = agentfs_mount::shutdown_signal() => result.map(|_| true).map_err(Into::into),
        _ = async {
            loop {
                if handle.is_finished() || !agentfs_mount::is_mountpoint(handle.mountpoint()) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        } => Ok(false),
    }
}

/// List all currently mounted agentfs filesystems (Linux)
#[cfg(target_os = "linux")]
pub fn list_mounts<W: Write>(out: &mut W) {
    let mounts = get_mounts();

    if mounts.is_empty() {
        let _ = writeln!(out, "No agentfs filesystems mounted.");
        return;
    }

    // Calculate column widths
    let id_width = mounts.iter().map(|m| m.id.len()).max().unwrap_or(2).max(2);
    let mount_width = mounts
        .iter()
        .map(|m| m.mountpoint.to_string_lossy().len())
        .max()
        .unwrap_or(10)
        .max(10);

    // Print header
    let _ = writeln!(
        out,
        "{:<id_width$}  {:<mount_width$}",
        "ID",
        "MOUNTPOINT",
        id_width = id_width,
        mount_width = mount_width
    );

    // Print mounts
    for mount in &mounts {
        let _ = writeln!(
            out,
            "{:<id_width$}  {:<mount_width$}",
            mount.id,
            mount.mountpoint.display(),
            id_width = id_width,
            mount_width = mount_width
        );
    }
}

/// List all currently mounted agentfs filesystems (macOS stub)
#[cfg(target_os = "macos")]
pub fn list_mounts<W: std::io::Write>(out: &mut W) {
    let _ = writeln!(out, "Mount listing is only available on Linux.");
}

/// Check if a mount point is in use by any process.
///
/// Scans /proc to find processes with open files or current working directory
/// on the given mountpoint.
#[cfg(target_os = "linux")]
fn is_mount_in_use(mountpoint: &Path) -> bool {
    let mountpoint = match mountpoint.canonicalize() {
        Ok(p) => p,
        Err(_) => return false, // Can't check, assume not in use
    };

    let proc_dir = match std::fs::read_dir("/proc") {
        Ok(dir) => dir,
        Err(_) => return false,
    };

    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Only check numeric directories (PIDs)
        if !name_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        let pid_path = entry.path();

        // Check cwd
        if let Ok(cwd) = std::fs::read_link(pid_path.join("cwd")) {
            if cwd.starts_with(&mountpoint) {
                return true;
            }
        }

        // Check open file descriptors
        let fd_dir = pid_path.join("fd");
        if let Ok(fds) = std::fs::read_dir(&fd_dir) {
            for fd_entry in fds.flatten() {
                if let Ok(target) = std::fs::read_link(fd_entry.path()) {
                    if target.starts_with(&mountpoint) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Unmount a FUSE filesystem.
///
/// Tries fusermount3 first, then falls back to fusermount.
#[cfg(target_os = "linux")]
fn unmount_fuse(mountpoint: &Path) -> Result<()> {
    const FUSERMOUNT_COMMANDS: &[&str] = &["fusermount3", "fusermount"];

    for cmd in FUSERMOUNT_COMMANDS {
        let result = std::process::Command::new(cmd)
            .args(["-u"])
            .arg(mountpoint.as_os_str())
            .status();

        match result {
            Ok(status) if status.success() => return Ok(()),
            Ok(_) => continue,  // Command ran but failed, try next
            Err(_) => continue, // Command not found, try next
        }
    }

    anyhow::bail!(
        "Failed to unmount {}. You may need to unmount manually with: fusermount -u {}",
        mountpoint.display(),
        mountpoint.display()
    )
}

/// Ask for user confirmation.
#[cfg(target_os = "linux")]
fn confirm(prompt: &str) -> bool {
    eprint!("{} ", prompt);
    let _ = io::stderr().flush();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Prune unused agentfs mount points.
///
/// Finds all mounted agentfs filesystems that are not in use by any process
/// and unmounts them.
#[cfg(target_os = "linux")]
pub fn prune_mounts(force: bool) -> Result<()> {
    let mounts = get_mounts();

    // Get active session IDs to exclude from pruning
    let active_sessions = super::ps::active_session_ids();

    // Find unused mounts (not in use by any process and no active session)
    let unused_mounts: Vec<&Mount> = mounts
        .iter()
        .filter(|m| !is_mount_in_use(&m.mountpoint) && !active_sessions.contains(&m.id))
        .collect();

    if unused_mounts.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }

    // Display what will be unmounted
    println!("The following unused mount points will be unmounted:");
    println!();
    for mount in &unused_mounts {
        println!("  {} -> {}", mount.id, mount.mountpoint.display());
    }
    println!();

    // Ask for confirmation unless --force
    if !force && !confirm("Are you sure? (y/N)") {
        println!("Aborted.");
        return Ok(());
    }

    // Unmount each unused mount
    let mut errors = Vec::new();
    for mount in &unused_mounts {
        print!("Unmounting {}... ", mount.mountpoint.display());
        let _ = io::stdout().flush();

        match unmount_fuse(&mount.mountpoint) {
            Ok(()) => println!("done"),
            Err(e) => {
                println!("failed");
                errors.push(format!("{}: {}", mount.mountpoint.display(), e));
            }
        }
    }

    if !errors.is_empty() {
        eprintln!();
        eprintln!("Some mounts could not be unmounted:");
        for error in &errors {
            eprintln!("  {}", error);
        }
        anyhow::bail!("Failed to unmount {} mount(s)", errors.len());
    }

    Ok(())
}

/// Prune unused agentfs mount points (macOS stub).
#[cfg(target_os = "macos")]
pub fn prune_mounts(_force: bool) -> Result<()> {
    anyhow::bail!("Mount pruning is only available on Linux")
}

/// Print schema version mismatch error and exit.
fn exit_schema_version_mismatch(found: &str, expected: &str, id_or_path: &str) -> ! {
    eprintln!("Error: Filesystem `{}` requires migration", id_or_path);
    eprintln!();
    eprintln!(
        "Found schema version {}, but this version of agentfs requires {}.",
        found, expected
    );
    eprintln!();
    eprintln!("To upgrade, run:");
    eprintln!();
    eprintln!("    agentfs migrate {}", id_or_path);
    eprintln!();
    crate::profiling::emit_cli_report();
    std::process::exit(1);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::tempdir;
    use turso::{Builder, Connection};

    #[tokio::test]
    async fn mount_precheck_backfills_legacy_whiteout_parent_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("legacy-whiteout.db");
        create_currentish_db_with_legacy_whiteout(&db_path).await;

        ensure_schema_current_for_mount_precheck(db_path.to_str().unwrap())
            .await
            .unwrap();

        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let columns = table_columns(&conn, "fs_whiteout").await;
        assert!(
            columns.iter().any(|column| column == "parent_path"),
            "mount precheck did not add fs_whiteout.parent_path; columns={columns:?}"
        );
        let mut rows = conn
            .query(
                "SELECT parent_path, created_at FROM fs_whiteout WHERE path = '/dir/deleted'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let parent_path = row.get::<String>(0).unwrap();
        let created_at = row.get::<i64>(1).unwrap();
        println!(
            "mount precheck: fs_whiteout columns={columns:?}; /dir/deleted parent_path={parent_path}"
        );
        assert_eq!(parent_path, "/dir");
        assert_eq!(created_at, 123);
    }

    async fn create_currentish_db_with_legacy_whiteout(db_path: &Path) {
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "CREATE TABLE fs_inode (
                ino INTEGER PRIMARY KEY AUTOINCREMENT,
                mode INTEGER NOT NULL,
                nlink INTEGER NOT NULL DEFAULT 0,
                uid INTEGER NOT NULL DEFAULT 0,
                gid INTEGER NOT NULL DEFAULT 0,
                size INTEGER NOT NULL DEFAULT 0,
                atime INTEGER NOT NULL,
                mtime INTEGER NOT NULL,
                ctime INTEGER NOT NULL,
                rdev INTEGER NOT NULL DEFAULT 0,
                atime_nsec INTEGER NOT NULL DEFAULT 0,
                mtime_nsec INTEGER NOT NULL DEFAULT 0,
                ctime_nsec INTEGER NOT NULL DEFAULT 0,
                data_inline BLOB,
                storage_kind INTEGER NOT NULL DEFAULT 0
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE fs_dentry (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                parent_ino INTEGER NOT NULL,
                ino INTEGER NOT NULL,
                UNIQUE(parent_ino, name)
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE fs_data (
                ino INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                data BLOB NOT NULL,
                PRIMARY KEY (ino, chunk_index)
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE fs_symlink (
                ino INTEGER PRIMARY KEY,
                target TEXT NOT NULL
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE fs_whiteout (
                path TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_whiteout (path, created_at) VALUES ('/dir/deleted', 123)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE kv_store (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                created_at INTEGER DEFAULT (unixepoch()),
                updated_at INTEGER DEFAULT (unixepoch())
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE tool_calls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                parameters TEXT,
                result TEXT,
                error TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                duration_ms INTEGER
            )",
            (),
        )
        .await
        .unwrap();
    }

    async fn table_columns(conn: &Connection, table_name: &str) -> Vec<String> {
        let mut rows = conn
            .query(&format!("PRAGMA table_info({table_name})"), ())
            .await
            .unwrap();
        let mut columns = Vec::new();
        while let Some(row) = rows.next().await.unwrap() {
            columns.push(row.get::<String>(1).unwrap());
        }
        columns
    }
}
