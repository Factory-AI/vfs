//! Production safety commands for local AgentFS databases.

use agentfs_sdk::{AgentFSOptions, AGENTFS_SCHEMA_VERSION};
use anyhow::{Context, Result as AnyhowResult};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use turso::{Builder, Connection, Value};

const S_IFMT: i64 = 0o170000;
const S_IFREG: i64 = 0o100000;
const S_IFDIR: i64 = 0o040000;
const S_IFLNK: i64 = 0o120000;

#[derive(Debug, Serialize)]
pub struct IntegrityReport {
    database: String,
    ok: bool,
    checks: Vec<IntegrityCheck>,
}

#[derive(Debug, Serialize)]
struct IntegrityCheck {
    name: String,
    ok: bool,
    detail: String,
    violating_rows: Option<i64>,
}

impl IntegrityReport {
    fn new(database: &Path) -> Self {
        Self {
            database: database.display().to_string(),
            ok: true,
            checks: Vec::new(),
        }
    }

    fn push_check(
        &mut self,
        name: impl Into<String>,
        ok: bool,
        detail: impl Into<String>,
        violating_rows: Option<i64>,
    ) {
        self.ok &= ok;
        self.checks.push(IntegrityCheck {
            name: name.into(),
            ok,
            detail: detail.into(),
            violating_rows,
        });
    }
}

/// Run integrity and schema-invariant checks for a local AgentFS database.
pub async fn handle_integrity_command(
    stdout: &mut impl Write,
    id_or_path: String,
    json: bool,
) -> AnyhowResult<()> {
    let db_path = resolve_local_db_path(&id_or_path)?;
    let db = Builder::new_local(path_as_str(&db_path)?)
        .build()
        .await
        .context("Failed to open database")?;
    let conn = db.connect().context("Failed to connect to database")?;

    let report = integrity_report(&conn, &db_path).await?;
    write_integrity_report(stdout, &report, json)?;
    if !report.ok {
        anyhow::bail!("integrity checks failed for {}", db_path.display());
    }
    Ok(())
}

/// Create a portable main-database backup of a local AgentFS database.
pub async fn handle_backup_command(
    stdout: &mut impl Write,
    id_or_path: String,
    target: PathBuf,
    verify: bool,
) -> AnyhowResult<()> {
    let source_path = resolve_local_db_path(&id_or_path)?;
    ensure_backup_target(&source_path, &target)?;

    let db = Builder::new_local(path_as_str(&source_path)?)
        .build()
        .await
        .context("Failed to open source database")?;
    let conn = db
        .connect()
        .context("Failed to connect to source database")?;

    checkpoint_for_backup(&conn, &source_path).await?;
    fs::copy(&source_path, &target).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            source_path.display(),
            target.display()
        )
    })?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&target)
        .with_context(|| format!("Failed to open backup {}", target.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync backup {}", target.display()))?;

    writeln!(stdout, "Source: {}", source_path.display())?;
    writeln!(stdout, "Backup: {}", target.display())?;
    writeln!(stdout, "Checkpoint: complete")?;
    writeln!(stdout, "Copy: complete")?;

    if verify {
        let backup_db = Builder::new_local(path_as_str(&target)?)
            .build()
            .await
            .context("Failed to reopen backup database")?;
        let backup_conn = backup_db
            .connect()
            .context("Failed to connect to backup database")?;
        let report = integrity_report(&backup_conn, &target).await?;
        if !report.ok {
            anyhow::bail!("backup verification failed for {}", target.display());
        }
        writeln!(stdout, "Verification: complete")?;
    }

    Ok(())
}

async fn integrity_report(conn: &Connection, db_path: &Path) -> AnyhowResult<IntegrityReport> {
    let mut report = IntegrityReport::new(db_path);

    let integrity_rows = query_string_column(conn, "PRAGMA integrity_check").await?;
    report.push_check(
        "pragma.integrity_check",
        integrity_rows == ["ok".to_string()],
        if integrity_rows == ["ok".to_string()] {
            "ok".to_string()
        } else {
            format!("{integrity_rows:?}")
        },
        None,
    );

    let required_tables = [
        "fs_config",
        "fs_inode",
        "fs_dentry",
        "fs_data",
        "fs_symlink",
        "kv_store",
        "tool_calls",
    ];
    let mut tables = BTreeMap::new();
    for table in required_tables {
        let exists = table_exists(conn, table).await?;
        tables.insert(table.to_string(), exists);
        report.push_check(
            format!("schema.table.{table}"),
            exists,
            if exists { "present" } else { "missing" },
            if exists { Some(0) } else { Some(1) },
        );
    }

    if *tables.get("fs_config").unwrap_or(&false) {
        check_config(conn, &mut report).await?;
    }

    let has_inode = *tables.get("fs_inode").unwrap_or(&false);
    let has_dentry = *tables.get("fs_dentry").unwrap_or(&false);
    let has_data = *tables.get("fs_data").unwrap_or(&false);
    let has_symlink = *tables.get("fs_symlink").unwrap_or(&false);
    if has_inode && has_data {
        check_storage_invariants(conn, &mut report).await?;
    }
    if has_inode && has_dentry {
        check_namespace_invariants(conn, &mut report).await?;
    }
    if has_inode && has_symlink {
        check_symlink_invariants(conn, &mut report).await?;
    }
    check_optional_overlay_invariants(conn, &mut report).await?;

    Ok(report)
}

async fn check_config(conn: &Connection, report: &mut IntegrityReport) -> AnyhowResult<()> {
    let schema_version = config_string(conn, "schema_version").await?;
    report.push_check(
        "config.schema_version",
        schema_version.as_deref() == Some(AGENTFS_SCHEMA_VERSION),
        schema_version
            .as_deref()
            .map(|value| format!("found {value}"))
            .unwrap_or_else(|| "missing".to_string()),
        if schema_version.as_deref() == Some(AGENTFS_SCHEMA_VERSION) {
            Some(0)
        } else {
            Some(1)
        },
    );

    let chunk_size = config_i64(conn, "chunk_size").await?;
    report.push_check(
        "config.chunk_size",
        chunk_size.is_some_and(|value| value > 0),
        chunk_size
            .map(|value| format!("found {value}"))
            .unwrap_or_else(|| "missing or invalid".to_string()),
        if chunk_size.is_some_and(|value| value > 0) {
            Some(0)
        } else {
            Some(1)
        },
    );

    let inline_threshold = config_i64(conn, "inline_threshold").await?;
    let inline_ok = match (inline_threshold, chunk_size) {
        (Some(inline), Some(chunk)) => inline >= 0 && inline <= chunk,
        _ => false,
    };
    report.push_check(
        "config.inline_threshold",
        inline_ok,
        inline_threshold
            .map(|value| format!("found {value}"))
            .unwrap_or_else(|| "missing or invalid".to_string()),
        if inline_ok { Some(0) } else { Some(1) },
    );

    Ok(())
}

async fn check_storage_invariants(
    conn: &Connection,
    report: &mut IntegrityReport,
) -> AnyhowResult<()> {
    add_zero_count_check(
        conn,
        report,
        "storage.kind_valid",
        "SELECT COUNT(*) FROM fs_inode WHERE storage_kind NOT IN (0, 1)",
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "storage.inline_has_no_chunks",
        "SELECT COUNT(*)
         FROM fs_inode i
         WHERE i.storage_kind = 1
           AND EXISTS (SELECT 1 FROM fs_data d WHERE d.ino = i.ino)",
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "storage.chunked_has_no_inline_data",
        "SELECT COUNT(*) FROM fs_inode WHERE storage_kind = 0 AND data_inline IS NOT NULL",
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "storage.inline_size_matches_blob",
        "SELECT COUNT(*)
         FROM fs_inode
         WHERE storage_kind = 1
           AND (data_inline IS NULL OR COALESCE(length(data_inline), 0) != size)",
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "storage.inline_only_regular_files",
        &format!(
            "SELECT COUNT(*) FROM fs_inode WHERE storage_kind = 1 AND (mode & {S_IFMT}) != {S_IFREG}"
        ),
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "storage.non_regular_has_no_inline_data",
        &format!(
            "SELECT COUNT(*) FROM fs_inode WHERE (mode & {S_IFMT}) != {S_IFREG} AND data_inline IS NOT NULL"
        ),
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "storage.chunks_reference_inodes",
        "SELECT COUNT(*)
         FROM fs_data d
         LEFT JOIN fs_inode i ON i.ino = d.ino
         WHERE i.ino IS NULL",
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "storage.chunks_nonnegative_index",
        "SELECT COUNT(*) FROM fs_data WHERE chunk_index < 0",
    )
    .await?;

    if let Some(chunk_size) = config_i64(conn, "chunk_size").await? {
        if chunk_size > 0 {
            add_zero_count_check(
                conn,
                report,
                "storage.chunk_length_within_chunk_size",
                &format!("SELECT COUNT(*) FROM fs_data WHERE length(data) > {chunk_size}"),
            )
            .await?;
        }
    }
    add_zero_count_check(
        conn,
        report,
        "storage.chunks_only_regular_files",
        &format!(
            "SELECT COUNT(*)
             FROM fs_data d
             JOIN fs_inode i ON i.ino = d.ino
             WHERE (i.mode & {S_IFMT}) != {S_IFREG}"
        ),
    )
    .await?;

    Ok(())
}

async fn check_namespace_invariants(
    conn: &Connection,
    report: &mut IntegrityReport,
) -> AnyhowResult<()> {
    add_exact_count_check(
        conn,
        report,
        "namespace.root_inode",
        &format!("SELECT COUNT(*) FROM fs_inode WHERE ino = 1 AND (mode & {S_IFMT}) = {S_IFDIR}"),
        1,
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "namespace.dentry_parent_exists",
        "SELECT COUNT(*)
         FROM fs_dentry d
         LEFT JOIN fs_inode p ON p.ino = d.parent_ino
         WHERE p.ino IS NULL",
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "namespace.dentry_parent_is_directory",
        &format!(
            "SELECT COUNT(*)
             FROM fs_dentry d
             JOIN fs_inode p ON p.ino = d.parent_ino
             WHERE (p.mode & {S_IFMT}) != {S_IFDIR}"
        ),
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "namespace.dentry_target_exists",
        "SELECT COUNT(*)
         FROM fs_dentry d
         LEFT JOIN fs_inode i ON i.ino = d.ino
         WHERE i.ino IS NULL",
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "namespace.dentry_names_valid",
        "SELECT COUNT(*)
         FROM fs_dentry
         WHERE name = '' OR name = '.' OR name = '..' OR instr(name, '/') > 0",
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "namespace.non_directory_nlink_matches_dentries",
        &format!(
            "SELECT COUNT(*)
             FROM fs_inode i
             WHERE (i.mode & {S_IFMT}) != {S_IFDIR}
               AND i.nlink != (SELECT COUNT(*) FROM fs_dentry d WHERE d.ino = i.ino)"
        ),
    )
    .await?;
    add_zero_count_check(
        conn,
        report,
        "namespace.directory_nlink_positive",
        &format!("SELECT COUNT(*) FROM fs_inode WHERE (mode & {S_IFMT}) = {S_IFDIR} AND nlink < 1"),
    )
    .await?;

    Ok(())
}

async fn check_symlink_invariants(
    conn: &Connection,
    report: &mut IntegrityReport,
) -> AnyhowResult<()> {
    add_zero_count_check(
        conn,
        report,
        "symlink.rows_reference_symlink_inodes",
        &format!(
            "SELECT COUNT(*)
             FROM fs_symlink s
             LEFT JOIN fs_inode i ON i.ino = s.ino
             WHERE i.ino IS NULL OR (i.mode & {S_IFMT}) != {S_IFLNK}"
        ),
    )
    .await
}

async fn check_optional_overlay_invariants(
    conn: &Connection,
    report: &mut IntegrityReport,
) -> AnyhowResult<()> {
    if table_exists(conn, "fs_origin").await? {
        add_zero_count_check(
            conn,
            report,
            "overlay.origin_delta_inode_exists",
            "SELECT COUNT(*)
             FROM fs_origin o
             LEFT JOIN fs_inode i ON i.ino = o.delta_ino
             WHERE i.ino IS NULL",
        )
        .await?;
    }

    if table_exists(conn, "fs_partial_origin").await? {
        add_zero_count_check(
            conn,
            report,
            "overlay.partial_origin_delta_inode_exists",
            "SELECT COUNT(*)
             FROM fs_partial_origin p
             LEFT JOIN fs_inode i ON i.ino = p.delta_ino
             WHERE i.ino IS NULL",
        )
        .await?;
        add_zero_count_check(
            conn,
            report,
            "overlay.partial_origin_sizes_valid",
            "SELECT COUNT(*)
             FROM fs_partial_origin
             WHERE base_size < 0 OR base_fingerprint_size < -1",
        )
        .await?;
    }

    if table_exists(conn, "fs_chunk_override").await? {
        add_zero_count_check(
            conn,
            report,
            "overlay.chunk_override_delta_inode_exists",
            "SELECT COUNT(*)
             FROM fs_chunk_override c
             LEFT JOIN fs_inode i ON i.ino = c.delta_ino
             WHERE i.ino IS NULL",
        )
        .await?;
        add_zero_count_check(
            conn,
            report,
            "overlay.chunk_override_nonnegative_index",
            "SELECT COUNT(*) FROM fs_chunk_override WHERE chunk_index < 0",
        )
        .await?;
    }

    if table_exists(conn, "fs_whiteout").await? {
        add_zero_count_check(
            conn,
            report,
            "overlay.whiteout_paths_absolute",
            "SELECT COUNT(*)
             FROM fs_whiteout
             WHERE path NOT LIKE '/%' OR parent_path NOT LIKE '/%'",
        )
        .await?;
    }

    Ok(())
}

async fn add_zero_count_check(
    conn: &Connection,
    report: &mut IntegrityReport,
    name: &str,
    sql: &str,
) -> AnyhowResult<()> {
    let count = scalar_i64(conn, sql).await?;
    report.push_check(
        name,
        count == 0,
        if count == 0 {
            "0 violating rows".to_string()
        } else {
            format!("{count} violating rows")
        },
        Some(count),
    );
    Ok(())
}

async fn add_exact_count_check(
    conn: &Connection,
    report: &mut IntegrityReport,
    name: &str,
    sql: &str,
    expected: i64,
) -> AnyhowResult<()> {
    let count = scalar_i64(conn, sql).await?;
    report.push_check(
        name,
        count == expected,
        format!("found {count}, expected {expected}"),
        if count == expected {
            Some(0)
        } else {
            Some((count - expected).abs())
        },
    );
    Ok(())
}

async fn checkpoint_for_backup(conn: &Connection, source_path: &Path) -> AnyhowResult<()> {
    conn.execute("PRAGMA synchronous = FULL", ()).await?;

    let checkpoint_result = async {
        let mut rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
        if let Some(row) = rows.next().await? {
            let busy = value_i64(row.get_value(0)?)?;
            if busy != 0 {
                anyhow::bail!("WAL checkpoint could not complete because the database is busy");
            }
        }
        while rows.next().await?.is_some() {}
        Ok::<_, anyhow::Error>(())
    }
    .await;

    conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
    checkpoint_result?;

    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(source_path)
        .with_context(|| format!("Failed to open source {}", source_path.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync source {}", source_path.display()))?;
    Ok(())
}

fn write_integrity_report(
    stdout: &mut impl Write,
    report: &IntegrityReport,
    json: bool,
) -> AnyhowResult<()> {
    if json {
        serde_json::to_writer_pretty(&mut *stdout, report)?;
        writeln!(stdout)?;
        return Ok(());
    }

    writeln!(stdout, "Database: {}", report.database)?;
    writeln!(
        stdout,
        "Status: {}",
        if report.ok { "ok" } else { "failed" }
    )?;
    for check in &report.checks {
        writeln!(
            stdout,
            "{}\t{}\t{}",
            if check.ok { "ok" } else { "FAIL" },
            check.name,
            check.detail
        )?;
    }
    Ok(())
}

fn resolve_local_db_path(id_or_path: &str) -> AnyhowResult<PathBuf> {
    let options = AgentFSOptions::resolve(id_or_path)?;
    let db_path = options
        .db_path()
        .context("Failed to resolve database path")?;
    if db_path == ":memory:" {
        anyhow::bail!("production safety commands require a local database file");
    }
    let path = PathBuf::from(db_path);
    if !path.is_file() {
        anyhow::bail!("Database not found: {}", path.display());
    }
    Ok(path)
}

fn ensure_backup_target(source_path: &Path, target: &Path) -> AnyhowResult<()> {
    if target.exists() {
        anyhow::bail!("Backup target already exists: {}", target.display());
    }
    for sidecar in [sidecar_path(target, "-wal"), sidecar_path(target, "-shm")] {
        if sidecar.exists() {
            anyhow::bail!(
                "Backup target sidecar already exists: {}",
                sidecar.display()
            );
        }
    }
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        anyhow::bail!("Backup target parent does not exist: {}", parent.display());
    }

    let source_abs = source_path.canonicalize().with_context(|| {
        format!(
            "Failed to canonicalize source database {}",
            source_path.display()
        )
    })?;
    let target_abs = parent
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize target parent {}", parent.display()))?
        .join(
            target
                .file_name()
                .context("Backup target has no file name")?,
        );
    if source_abs == target_abs {
        anyhow::bail!("Backup target must be different from source database");
    }

    Ok(())
}

fn path_as_str(path: &Path) -> AnyhowResult<&str> {
    path.to_str()
        .with_context(|| format!("Path is not valid UTF-8: {}", path.display()))
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), suffix))
}

async fn table_exists(conn: &Connection, table: &str) -> AnyhowResult<bool> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
            (table,),
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

async fn query_string_column(conn: &Connection, sql: &str) -> AnyhowResult<Vec<String>> {
    let mut rows = conn.query(sql, ()).await?;
    let mut values = Vec::new();
    while let Some(row) = rows.next().await? {
        values.push(row.get::<String>(0)?);
    }
    Ok(values)
}

async fn scalar_i64(conn: &Connection, sql: &str) -> AnyhowResult<i64> {
    let mut rows = conn.query(sql, ()).await?;
    let row = rows.next().await?.context("query returned no rows")?;
    value_i64(row.get_value(0)?)
}

async fn config_string(conn: &Connection, key: &str) -> AnyhowResult<Option<String>> {
    let mut rows = conn
        .query("SELECT value FROM fs_config WHERE key = ?", (key,))
        .await?;
    if let Some(row) = rows.next().await? {
        Ok(Some(row.get::<String>(0)?))
    } else {
        Ok(None)
    }
}

async fn config_i64(conn: &Connection, key: &str) -> AnyhowResult<Option<i64>> {
    let Some(value) = config_string(conn, key).await? else {
        return Ok(None);
    };
    Ok(value.parse::<i64>().ok())
}

fn value_i64(value: Value) -> AnyhowResult<i64> {
    value
        .as_integer()
        .copied()
        .context("Expected integer result")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentfs_sdk::{AgentFS, AgentFSOptions};
    use serde_json::Value as JsonValue;

    #[tokio::test]
    async fn integrity_succeeds_for_valid_database() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("valid.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await
                .unwrap();
            agent.fs.pwrite("/hello.txt", 0, b"hello").await.unwrap();
        }

        let mut stdout = Vec::new();
        handle_integrity_command(&mut stdout, db_path.to_string_lossy().to_string(), true)
            .await
            .unwrap();
        let json: JsonValue = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(json["ok"], true);
    }

    #[tokio::test]
    async fn integrity_fails_for_inline_file_with_chunk_rows() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("corrupt.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await
                .unwrap();
            agent.fs.pwrite("/bad.txt", 0, b"bad").await.unwrap();
            let conn = agent.get_connection().await.unwrap();
            let mut rows = conn
                .query(
                    "SELECT ino FROM fs_dentry WHERE parent_ino = 1 AND name = 'bad.txt'",
                    (),
                )
                .await
                .unwrap();
            let row = rows.next().await.unwrap().unwrap();
            let ino = value_i64(row.get_value(0).unwrap()).unwrap();
            conn.execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, 0, ?)",
                (ino, Value::Blob(b"bad".to_vec())),
            )
            .await
            .unwrap();
        }

        let mut stdout = Vec::new();
        let err =
            handle_integrity_command(&mut stdout, db_path.to_string_lossy().to_string(), true)
                .await
                .unwrap_err();
        assert!(err.to_string().contains("integrity checks failed"));
        let json: JsonValue = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(json["ok"], false);
        let failed =
            json["checks"].as_array().unwrap().iter().any(|check| {
                check["name"] == "storage.inline_has_no_chunks" && check["ok"] == false
            });
        assert!(failed);
    }

    #[tokio::test]
    async fn backup_verify_roundtrips_main_database_snapshot() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source.db");
        let target = temp_dir.path().join("backup.db");
        let large = vec![7_u8; 128 * 1024 + 3];
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(source.to_string_lossy()))
                .await
                .unwrap();
            agent.fs.pwrite("/small.txt", 0, b"small").await.unwrap();
            agent.fs.pwrite("/large.bin", 0, &large).await.unwrap();
        }

        let mut stdout = Vec::new();
        handle_backup_command(
            &mut stdout,
            source.to_string_lossy().to_string(),
            target.clone(),
            true,
        )
        .await
        .unwrap();

        assert!(target.is_file());
        let backup = AgentFS::open(AgentFSOptions::with_path(target.to_string_lossy()))
            .await
            .unwrap();
        assert_eq!(
            backup.fs.read_file("/small.txt").await.unwrap().unwrap(),
            b"small"
        );
        assert_eq!(
            backup.fs.read_file("/large.bin").await.unwrap().unwrap(),
            large
        );
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Verification: complete"));
    }
}
