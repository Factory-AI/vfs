//! Production safety commands for local AgentFS databases.

use agentfs_sdk::{AgentFSOptions, AGENTFS_SCHEMA_VERSION};
use anyhow::{Context, Result as AnyhowResult};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, EncryptionOpts, Value};

const S_IFMT: i64 = 0o170000;
const S_IFREG: i64 = 0o100000;
const S_IFDIR: i64 = 0o040000;
const S_IFLNK: i64 = 0o120000;
const STORAGE_CHUNKED: i64 = 0;
const STORAGE_INLINE: i64 = 1;

#[derive(Debug, Clone)]
struct PartialOriginRow {
    delta_ino: i64,
    base_path: String,
    base_size: i64,
    base_fingerprint_size: i64,
    base_mtime: i64,
    base_mtime_nsec: i64,
    base_ctime: i64,
    base_ctime_nsec: i64,
}

#[derive(Debug, Serialize)]
pub struct IntegrityReport {
    database: String,
    ok: bool,
    portable: bool,
    origin_backed: bool,
    partial_origin_rows: i64,
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
            portable: true,
            origin_backed: false,
            partial_origin_rows: 0,
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

#[derive(Debug, Clone, Copy)]
struct IntegrityOptions {
    require_portable: bool,
    check_base: bool,
}

/// Run integrity and schema-invariant checks for a local AgentFS database.
pub async fn handle_integrity_command(
    stdout: &mut impl Write,
    id_or_path: String,
    json: bool,
    require_portable: bool,
    check_base: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let db_path = resolve_local_db_path(&id_or_path)?;
    let db = build_local_database(&db_path, encryption).await?;
    let conn = db.connect().context("Failed to connect to database")?;
    conn.execute("PRAGMA query_only = 1", ())
        .await
        .context("Failed to enable query_only mode")?;

    let report = integrity_report(
        &conn,
        &db_path,
        IntegrityOptions {
            require_portable,
            check_base,
        },
    )
    .await?;
    write_integrity_report(stdout, &report, json)?;
    if !report.ok {
        anyhow::bail!("integrity checks failed for {}", db_path.display());
    }
    drop(conn);
    drop(db);
    let cleanup_db = build_local_database(&db_path, encryption).await?;
    let cleanup_conn = cleanup_db
        .connect()
        .context("Failed to connect to database for sidecar cleanup")?;
    checkpoint_for_backup(&cleanup_conn, &db_path).await?;
    drop(cleanup_conn);
    drop(cleanup_db);
    remove_sqlite_sidecars_after_checkpoint(&db_path)?;
    Ok(())
}

/// Create a portable main-database backup of a local AgentFS database.
pub async fn handle_backup_command(
    stdout: &mut impl Write,
    id_or_path: String,
    target: PathBuf,
    verify: bool,
    materialize: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let source_path = resolve_local_db_path(&id_or_path)?;
    ensure_backup_target(&source_path, &target)?;

    if materialize {
        let materialized =
            copy_and_materialize_database(&source_path, &target, verify, encryption).await?;
        writeln!(stdout, "Source: {}", source_path.display())?;
        writeln!(stdout, "Backup: {}", target.display())?;
        writeln!(stdout, "Checkpoint: complete")?;
        writeln!(stdout, "Copy: complete")?;
        writeln!(stdout, "Materialized partial-origin files: {materialized}")?;
        writeln!(stdout, "Integrity: complete")?;
        if verify {
            writeln!(stdout, "Verification: complete")?;
        }
        return Ok(());
    }

    let db = build_local_database(&source_path, encryption).await?;
    let conn = db
        .connect()
        .context("Failed to connect to source database")?;

    reject_partial_origin_backup(&conn).await?;
    checkpoint_for_backup(&conn, &source_path).await?;
    copy_main_db_exclusive(&source_path, &target)?;
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
        {
            let backup_db = build_local_database(&target, encryption)
                .await
                .context("Failed to reopen backup database")?;
            let backup_conn = backup_db
                .connect()
                .context("Failed to connect to backup database")?;
            let report = integrity_report(
                &backup_conn,
                &target,
                IntegrityOptions {
                    require_portable: true,
                    check_base: false,
                },
            )
            .await?;
            if !report.ok {
                anyhow::bail!("backup verification failed for {}", target.display());
            }
        }
        remove_sqlite_sidecars_after_checkpoint(&target)?;
        writeln!(stdout, "Verification: complete")?;
    }

    Ok(())
}

/// Create a portable materialized copy of a local AgentFS database.
pub async fn handle_materialize_command(
    stdout: &mut impl Write,
    id_or_path: String,
    target: PathBuf,
    verify: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let source_path = resolve_local_db_path(&id_or_path)?;
    ensure_backup_target(&source_path, &target)?;

    let materialized =
        copy_and_materialize_database(&source_path, &target, verify, encryption).await?;
    writeln!(stdout, "Source: {}", source_path.display())?;
    writeln!(stdout, "Output: {}", target.display())?;
    writeln!(stdout, "Checkpoint: complete")?;
    writeln!(stdout, "Copy: complete")?;
    writeln!(stdout, "Materialized partial-origin files: {materialized}")?;
    writeln!(stdout, "Integrity: complete")?;
    if verify {
        writeln!(stdout, "Verification: complete")?;
    }
    Ok(())
}

async fn build_local_database(
    path: &Path,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<turso::Database> {
    let builder = Builder::new_local(path_as_str(path)?);
    let builder = if let Some((key, cipher)) = encryption {
        builder
            .experimental_encryption(true)
            .with_encryption(EncryptionOpts {
                cipher: cipher.clone(),
                hexkey: key.clone(),
            })
    } else {
        builder
    };
    builder
        .build()
        .await
        .with_context(|| format!("Failed to open database {}", path.display()))
}

async fn copy_and_materialize_database(
    source_path: &Path,
    target: &Path,
    verify: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<usize> {
    let source_db = build_local_database(source_path, encryption).await?;
    let source_conn = source_db
        .connect()
        .context("Failed to connect to source database")?;

    checkpoint_for_backup(&source_conn, source_path).await?;
    copy_main_db_exclusive(source_path, target)?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(target)
        .with_context(|| format!("Failed to open target {}", target.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync target {}", target.display()))?;

    let target_db = build_local_database(target, encryption)
        .await
        .context("Failed to reopen target database")?;
    let target_conn = target_db
        .connect()
        .context("Failed to connect to target database")?;

    let txn = Transaction::new_unchecked(&target_conn, TransactionBehavior::Immediate)
        .await
        .context("Failed to lock target database for materialization")?;
    let materialize_result = materialize_partial_origins_in_target(&target_conn).await;
    let materialized = match materialize_result {
        Ok(materialized) => {
            txn.commit().await?;
            materialized
        }
        Err(err) => {
            let _ = txn.rollback().await;
            return Err(err);
        }
    };

    ensure_no_partial_origin_rows(&target_conn).await?;
    checkpoint_materialized_target(&target_conn, target).await?;

    let report = integrity_report(
        &target_conn,
        target,
        IntegrityOptions {
            require_portable: true,
            check_base: false,
        },
    )
    .await?;
    if !report.ok {
        anyhow::bail!(
            "materialized target integrity checks failed for {}",
            target.display()
        );
    }

    drop(target_conn);
    drop(target_db);

    if verify {
        {
            let verify_db = build_local_database(target, encryption)
                .await
                .context("Failed to reopen materialized target database")?;
            let verify_conn = verify_db
                .connect()
                .context("Failed to connect to materialized target database")?;
            ensure_no_partial_origin_rows(&verify_conn).await?;
            let report = integrity_report(
                &verify_conn,
                target,
                IntegrityOptions {
                    require_portable: true,
                    check_base: false,
                },
            )
            .await?;
            if !report.ok {
                anyhow::bail!(
                    "materialized target verification failed for {}",
                    target.display()
                );
            }
        }
    }
    remove_sqlite_sidecars_after_checkpoint(target)?;

    Ok(materialized)
}

async fn materialize_partial_origins_in_target(conn: &Connection) -> AnyhowResult<usize> {
    let partial_rows = load_partial_origin_rows(conn).await?;
    if partial_rows.is_empty() {
        clear_partial_origin_tables(conn).await?;
        return Ok(0);
    }

    let base_root = read_overlay_base_root(conn).await?;
    let chunk_size = config_i64(conn, "chunk_size")
        .await?
        .context("missing chunk_size config")?;
    if chunk_size <= 0 {
        anyhow::bail!("invalid chunk_size config: {chunk_size}");
    }
    let inline_threshold = config_i64(conn, "inline_threshold").await?.unwrap_or(0);

    for partial in &partial_rows {
        materialize_partial_file(
            conn,
            &base_root,
            partial,
            chunk_size as usize,
            inline_threshold,
        )
        .await?;
    }

    clear_partial_origin_tables(conn).await?;
    Ok(partial_rows.len())
}

async fn load_partial_origin_rows(conn: &Connection) -> AnyhowResult<Vec<PartialOriginRow>> {
    if !table_exists(conn, "fs_partial_origin").await? {
        return Ok(Vec::new());
    }

    let mut rows = conn
        .query(
            "SELECT delta_ino, base_path, base_size, base_fingerprint_size,
                    base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec
             FROM fs_partial_origin
             ORDER BY delta_ino",
            (),
        )
        .await?;
    let mut partial_rows = Vec::new();
    while let Some(row) = rows.next().await? {
        let delta_ino = value_i64(row.get_value(0)?)?;
        let base_path: String = row.get(1)?;
        let base_size = value_i64(row.get_value(2)?)?;
        let raw_fingerprint_size = value_i64(row.get_value(3)?)?;
        partial_rows.push(PartialOriginRow {
            delta_ino,
            base_path,
            base_size,
            base_fingerprint_size: if raw_fingerprint_size < 0 {
                base_size
            } else {
                raw_fingerprint_size
            },
            base_mtime: value_i64(row.get_value(4)?)?,
            base_mtime_nsec: value_i64(row.get_value(5)?)?,
            base_ctime: value_i64(row.get_value(6)?)?,
            base_ctime_nsec: value_i64(row.get_value(7)?)?,
        });
    }
    Ok(partial_rows)
}

async fn materialize_partial_file(
    conn: &Connection,
    base_root: &Path,
    partial: &PartialOriginRow,
    chunk_size: usize,
    inline_threshold: i64,
) -> AnyhowResult<()> {
    let (mode, logical_size) = inode_mode_and_size(conn, partial.delta_ino).await?;
    if (mode & S_IFMT) != S_IFREG {
        anyhow::bail!(
            "partial-origin inode {} is not a regular file",
            partial.delta_ino
        );
    }
    if logical_size < 0 || partial.base_size < 0 {
        anyhow::bail!(
            "partial-origin inode {} has negative size metadata",
            partial.delta_ino
        );
    }

    let base_path = resolve_materialization_base_path(base_root, &partial.base_path)?;
    let metadata = fs::metadata(&base_path)
        .with_context(|| format!("Failed to stat base file {}", base_path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!(
            "partial-origin base path is not a regular file: {}",
            base_path.display()
        );
    }
    validate_base_fingerprint(partial, &metadata, &base_path)?;

    let overrides = load_override_chunks(conn, partial.delta_ino).await?;
    let mut base_file = fs::File::open(&base_path).with_context(|| {
        format!(
            "Failed to open base file read-only: {}",
            base_path.display()
        )
    })?;
    let logical_size = logical_size as usize;

    conn.execute("DELETE FROM fs_data WHERE ino = ?", (partial.delta_ino,))
        .await?;

    if logical_size as i64 <= inline_threshold {
        let bytes = materialized_file_bytes(
            &mut base_file,
            partial.base_size as usize,
            logical_size,
            chunk_size,
            &overrides,
        )?;
        conn.execute(
            "UPDATE fs_inode SET data_inline = ?, storage_kind = ? WHERE ino = ?",
            (Value::Blob(bytes), STORAGE_INLINE, partial.delta_ino),
        )
        .await?;
        return Ok(());
    }

    conn.execute(
        "UPDATE fs_inode SET data_inline = NULL, storage_kind = ? WHERE ino = ?",
        (STORAGE_CHUNKED, partial.delta_ino),
    )
    .await?;

    let chunk_count = logical_size.div_ceil(chunk_size);
    for chunk_index in 0..chunk_count {
        let chunk_start = chunk_index * chunk_size;
        let chunk_len = std::cmp::min(chunk_size, logical_size - chunk_start);
        let chunk = materialized_chunk(
            &mut base_file,
            partial.base_size as usize,
            chunk_index as i64,
            chunk_start,
            chunk_len,
            &overrides,
        )?;
        if !chunk.iter().all(|byte| *byte == 0) {
            conn.execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
                (partial.delta_ino, chunk_index as i64, Value::Blob(chunk)),
            )
            .await?;
        }
    }

    Ok(())
}

async fn inode_mode_and_size(conn: &Connection, ino: i64) -> AnyhowResult<(i64, i64)> {
    let mut rows = conn
        .query("SELECT mode, size FROM fs_inode WHERE ino = ?", (ino,))
        .await?;
    let row = rows
        .next()
        .await?
        .with_context(|| format!("partial-origin inode {ino} is missing"))?;
    Ok((value_i64(row.get_value(0)?)?, value_i64(row.get_value(1)?)?))
}

async fn load_override_chunks(
    conn: &Connection,
    delta_ino: i64,
) -> AnyhowResult<BTreeMap<i64, Vec<u8>>> {
    if !table_exists(conn, "fs_chunk_override").await? {
        return Ok(BTreeMap::new());
    }

    let mut rows = conn
        .query(
            "SELECT c.chunk_index, d.data
             FROM fs_chunk_override c
             LEFT JOIN fs_data d ON d.ino = c.delta_ino AND d.chunk_index = c.chunk_index
             WHERE c.delta_ino = ?
             ORDER BY c.chunk_index",
            (delta_ino,),
        )
        .await?;
    let mut overrides = BTreeMap::new();
    while let Some(row) = rows.next().await? {
        let chunk_index = value_i64(row.get_value(0)?)?;
        let data = match row.get_value(1)? {
            Value::Blob(data) => data,
            Value::Null => {
                anyhow::bail!(
                    "missing fs_data row for partial-origin override inode {delta_ino} chunk {chunk_index}"
                );
            }
            _ => Vec::new(),
        };
        overrides.insert(chunk_index, data);
    }
    Ok(overrides)
}

fn materialized_file_bytes(
    base_file: &mut fs::File,
    base_size: usize,
    logical_size: usize,
    chunk_size: usize,
    overrides: &BTreeMap<i64, Vec<u8>>,
) -> AnyhowResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(logical_size);
    let chunk_count = logical_size.div_ceil(chunk_size);
    for chunk_index in 0..chunk_count {
        let chunk_start = chunk_index * chunk_size;
        let chunk_len = std::cmp::min(chunk_size, logical_size - chunk_start);
        let chunk = materialized_chunk(
            base_file,
            base_size,
            chunk_index as i64,
            chunk_start,
            chunk_len,
            overrides,
        )?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn materialized_chunk(
    base_file: &mut fs::File,
    base_size: usize,
    chunk_index: i64,
    chunk_start: usize,
    chunk_len: usize,
    overrides: &BTreeMap<i64, Vec<u8>>,
) -> AnyhowResult<Vec<u8>> {
    if let Some(override_data) = overrides.get(&chunk_index) {
        let mut chunk = override_data.clone();
        chunk.resize(chunk_len, 0);
        chunk.truncate(chunk_len);
        return Ok(chunk);
    }

    let mut chunk = vec![0; chunk_len];
    if chunk_start < base_size {
        let readable = std::cmp::min(chunk_len, base_size - chunk_start);
        base_file
            .seek(SeekFrom::Start(chunk_start as u64))
            .context("Failed to seek base file")?;
        base_file
            .read_exact(&mut chunk[..readable])
            .context("Failed to read base file bytes")?;
    }
    Ok(chunk)
}

async fn read_overlay_base_root(conn: &Connection) -> AnyhowResult<PathBuf> {
    if !table_exists(conn, "fs_overlay_config").await? {
        anyhow::bail!("partial-origin database is missing fs_overlay_config");
    }
    let mut rows = conn
        .query(
            "SELECT value FROM fs_overlay_config WHERE key = 'base_path'",
            (),
        )
        .await?;
    let row = rows
        .next()
        .await?
        .context("partial-origin database is missing fs_overlay_config.base_path")?;
    let base_path: String = row.get(0)?;
    let base_root = PathBuf::from(base_path);
    base_root
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize base root {}", base_root.display()))
}

fn resolve_materialization_base_path(
    base_root: &Path,
    recorded_path: &str,
) -> AnyhowResult<PathBuf> {
    let mut candidate = base_root.to_path_buf();
    for component in Path::new(recorded_path).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => candidate.push(part),
            Component::ParentDir => {
                anyhow::bail!("partial-origin base path escapes base root: {recorded_path}")
            }
            Component::Prefix(_) => {
                anyhow::bail!("partial-origin base path has an unsupported prefix: {recorded_path}")
            }
        }
    }

    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize base path {}", candidate.display()))?;
    if !canonical.starts_with(base_root) {
        anyhow::bail!(
            "partial-origin base path escapes base root: {}",
            canonical.display()
        );
    }
    Ok(canonical)
}

fn validate_base_fingerprint(
    partial: &PartialOriginRow,
    metadata: &fs::Metadata,
    path: &Path,
) -> AnyhowResult<()> {
    let fingerprint = metadata_fingerprint(metadata);
    if fingerprint.size != partial.base_fingerprint_size
        || fingerprint.mtime != partial.base_mtime
        || fingerprint.mtime_nsec != partial.base_mtime_nsec
        || fingerprint.ctime != partial.base_ctime
        || fingerprint.ctime_nsec != partial.base_ctime_nsec
    {
        anyhow::bail!(
            "partial-origin base changed for {} (stored size={}, current size={}, path={})",
            partial.base_path,
            partial.base_fingerprint_size,
            fingerprint.size,
            path.display()
        );
    }
    Ok(())
}

struct FileFingerprint {
    size: i64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

#[cfg(unix)]
fn metadata_fingerprint(metadata: &fs::Metadata) -> FileFingerprint {
    use std::os::unix::fs::MetadataExt;
    FileFingerprint {
        size: metadata.len() as i64,
        mtime: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
        ctime: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    }
}

#[cfg(not(unix))]
fn metadata_fingerprint(metadata: &fs::Metadata) -> FileFingerprint {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs() as i64, duration.subsec_nanos() as i64))
        .unwrap_or((0, 0));
    FileFingerprint {
        size: metadata.len() as i64,
        mtime: modified.0,
        mtime_nsec: modified.1,
        ctime: 0,
        ctime_nsec: 0,
    }
}

async fn clear_partial_origin_tables(conn: &Connection) -> AnyhowResult<()> {
    if table_exists(conn, "fs_chunk_override").await? {
        conn.execute("DELETE FROM fs_chunk_override", ()).await?;
    }
    if table_exists(conn, "fs_partial_origin").await? {
        conn.execute("DELETE FROM fs_partial_origin", ()).await?;
    }
    Ok(())
}

async fn ensure_no_partial_origin_rows(conn: &Connection) -> AnyhowResult<()> {
    if table_exists(conn, "fs_partial_origin").await? {
        let count = scalar_i64(conn, "SELECT COUNT(*) FROM fs_partial_origin").await?;
        if count != 0 {
            anyhow::bail!("materialized target still has {count} fs_partial_origin row(s)");
        }
    }
    if table_exists(conn, "fs_chunk_override").await? {
        let count = scalar_i64(conn, "SELECT COUNT(*) FROM fs_chunk_override").await?;
        if count != 0 {
            anyhow::bail!("materialized target still has {count} fs_chunk_override row(s)");
        }
    }
    Ok(())
}

async fn checkpoint_materialized_target(conn: &Connection, target_path: &Path) -> AnyhowResult<()> {
    conn.execute("PRAGMA synchronous = FULL", ()).await?;
    let checkpoint_result = async {
        let mut rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
        if let Some(row) = rows.next().await? {
            let busy = value_i64(row.get_value(0)?)?;
            if busy != 0 {
                anyhow::bail!(
                    "WAL checkpoint could not complete because the target database is busy"
                );
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
        .open(target_path)
        .with_context(|| format!("Failed to open target {}", target_path.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync target {}", target_path.display()))?;
    Ok(())
}

async fn integrity_report(
    conn: &Connection,
    db_path: &Path,
    options: IntegrityOptions,
) -> AnyhowResult<IntegrityReport> {
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
    check_portability_status(conn, &mut report, options.require_portable).await?;
    check_optional_overlay_invariants(conn, &mut report, options.check_base).await?;

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
        "namespace.non_root_inode_has_dentry",
        // nlink = 0 rows are POSIX orphans: files unlinked while open, whose
        // reap is deferred until the last handle closes (and swept at the
        // next mount after a crash). Dentry-less is legal only in that state.
        "SELECT COUNT(*)
         FROM fs_inode i
         WHERE i.ino != 1
           AND i.nlink != 0
           AND NOT EXISTS (SELECT 1 FROM fs_dentry d WHERE d.ino = i.ino)",
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
    .await?;
    add_zero_count_check(
        conn,
        report,
        "symlink.inodes_have_rows",
        &format!(
            "SELECT COUNT(*)
             FROM fs_inode i
             WHERE (i.mode & {S_IFMT}) = {S_IFLNK}
               AND NOT EXISTS (SELECT 1 FROM fs_symlink s WHERE s.ino = i.ino)"
        ),
    )
    .await
}

async fn check_portability_status(
    conn: &Connection,
    report: &mut IntegrityReport,
    require_portable: bool,
) -> AnyhowResult<()> {
    let partial_origin_rows = if table_exists(conn, "fs_partial_origin").await? {
        scalar_i64(conn, "SELECT COUNT(*) FROM fs_partial_origin").await?
    } else {
        0
    };

    report.partial_origin_rows = partial_origin_rows;
    report.origin_backed = partial_origin_rows > 0;
    report.portable = partial_origin_rows == 0;

    report.push_check(
        "overlay.portability_status",
        true,
        if report.portable {
            "portable: no partial-origin rows".to_string()
        } else {
            format!("origin-backed: {partial_origin_rows} partial-origin row(s)")
        },
        Some(partial_origin_rows),
    );

    if require_portable {
        report.push_check(
            "overlay.require_portable",
            report.portable,
            if report.portable {
                "portable requirement satisfied".to_string()
            } else {
                format!("portable requirement failed: {partial_origin_rows} partial-origin row(s)")
            },
            Some(partial_origin_rows),
        );
    }

    Ok(())
}

async fn check_optional_overlay_invariants(
    conn: &Connection,
    report: &mut IntegrityReport,
    check_base: bool,
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
            "overlay.partial_origin_delta_inode_regular",
            &format!(
                "SELECT COUNT(*)
                 FROM fs_partial_origin p
                 LEFT JOIN fs_inode i ON i.ino = p.delta_ino
                 WHERE i.ino IS NULL OR (i.mode & {S_IFMT}) != {S_IFREG}"
            ),
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
        add_zero_count_check(
            conn,
            report,
            "overlay.partial_origin_paths_absolute",
            "SELECT COUNT(*)
             FROM fs_partial_origin
             WHERE base_path = '' OR base_path NOT LIKE '/%' OR instr(base_path, '/../') > 0 OR base_path LIKE '%/..'",
        )
        .await?;
        if check_base {
            check_partial_origin_base_fingerprints(conn, report).await?;
        }
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
        if table_exists(conn, "fs_partial_origin").await? {
            add_zero_count_check(
                conn,
                report,
                "overlay.chunk_override_references_partial_origin",
                "SELECT COUNT(*)
                 FROM fs_chunk_override c
                 LEFT JOIN fs_partial_origin p ON p.delta_ino = c.delta_ino
                 WHERE p.delta_ino IS NULL",
            )
            .await?;
        } else {
            add_zero_count_check(
                conn,
                report,
                "overlay.chunk_override_requires_partial_origin_table",
                "SELECT COUNT(*) FROM fs_chunk_override",
            )
            .await?;
        }
        add_zero_count_check(
            conn,
            report,
            "overlay.chunk_override_unique",
            "SELECT COUNT(*)
             FROM (
               SELECT delta_ino, chunk_index, COUNT(*) AS n
               FROM fs_chunk_override
               GROUP BY delta_ino, chunk_index
               HAVING n > 1
             )",
        )
        .await?;
        if let Some(chunk_size) = config_i64(conn, "chunk_size").await? {
            if chunk_size > 0 {
                add_zero_count_check(
                    conn,
                    report,
                    "overlay.chunk_override_index_in_range",
                    &format!(
                        "SELECT COUNT(*)
                         FROM fs_chunk_override c
                         JOIN fs_inode i ON i.ino = c.delta_ino
                         WHERE c.chunk_index * {chunk_size} >= i.size"
                    ),
                )
                .await?;
            }
        }
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

async fn check_partial_origin_base_fingerprints(
    conn: &Connection,
    report: &mut IntegrityReport,
) -> AnyhowResult<()> {
    let partial_rows = load_partial_origin_rows(conn).await?;
    if partial_rows.is_empty() {
        report.push_check(
            "overlay.partial_origin_base_fingerprints",
            true,
            "no partial-origin rows".to_string(),
            Some(0),
        );
        return Ok(());
    }

    let base_root = match read_overlay_base_root(conn).await {
        Ok(root) => root,
        Err(err) => {
            report.push_check(
                "overlay.partial_origin_base_fingerprints",
                false,
                err.to_string(),
                Some(partial_rows.len() as i64),
            );
            return Ok(());
        }
    };

    let mut violations = 0;
    let mut first_error = None;
    for partial in &partial_rows {
        let result = (|| -> AnyhowResult<()> {
            let base_path = resolve_materialization_base_path(&base_root, &partial.base_path)?;
            let metadata = fs::metadata(&base_path)
                .with_context(|| format!("Failed to stat base file {}", base_path.display()))?;
            validate_base_fingerprint(partial, &metadata, &base_path)
        })();
        if let Err(err) = result {
            violations += 1;
            if first_error.is_none() {
                first_error = Some(err.to_string());
            }
        }
    }

    report.push_check(
        "overlay.partial_origin_base_fingerprints",
        violations == 0,
        if violations == 0 {
            format!("{} base fingerprint(s) valid", partial_rows.len())
        } else {
            format!(
                "{violations} base fingerprint violation(s); first: {}",
                first_error.unwrap_or_else(|| "unknown".to_string())
            )
        },
        Some(violations),
    );
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

async fn reject_partial_origin_backup(conn: &Connection) -> AnyhowResult<()> {
    if !table_exists(conn, "fs_partial_origin").await? {
        return Ok(());
    }

    let count = scalar_i64(conn, "SELECT COUNT(*) FROM fs_partial_origin").await?;
    if count != 0 {
        anyhow::bail!(
            "portable backup is not supported for partial-origin overlay databases ({count} partial-origin row(s)); materialize the overlay first or keep the base tree with the database"
        );
    }
    Ok(())
}

fn copy_main_db_exclusive(source: &Path, target: &Path) -> AnyhowResult<()> {
    let mut src = fs::File::open(source)
        .with_context(|| format!("Failed to open source {}", source.display()))?;
    let mut dst = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .with_context(|| format!("Failed to create backup {}", target.display()))?;

    std::io::copy(&mut src, &mut dst).with_context(|| {
        format!(
            "Failed to copy {} to {}",
            source.display(),
            target.display()
        )
    })?;
    dst.sync_all()
        .with_context(|| format!("Failed to sync backup {}", target.display()))?;
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
    writeln!(
        stdout,
        "Portability: {}",
        if report.portable {
            "portable"
        } else {
            "origin-backed"
        }
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

fn remove_sqlite_sidecars_after_checkpoint(path: &Path) -> AnyhowResult<()> {
    let wal = sidecar_path(path, "-wal");
    if let Ok(metadata) = fs::metadata(&wal) {
        if metadata.len() != 0 {
            anyhow::bail!(
                "Refusing to remove non-empty WAL sidecar after checkpoint: {} ({} bytes)",
                wal.display(),
                metadata.len()
            );
        }
        fs::remove_file(&wal)
            .with_context(|| format!("Failed to remove WAL sidecar {}", wal.display()))?;
    }

    let shm = sidecar_path(path, "-shm");
    if shm.exists() {
        fs::remove_file(&shm)
            .with_context(|| format!("Failed to remove SHM sidecar {}", shm.display()))?;
    }
    Ok(())
}

#[cfg(test)]
fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
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

    async fn write_agent_file(agent: &AgentFS, path: &str, data: &[u8]) {
        let (_, file) = agent
            .fs
            .create_file(path, (S_IFREG | 0o644) as u32, 0, 0)
            .await
            .unwrap();
        file.pwrite(0, data).await.unwrap();
        agent.fs.drain_all().await.unwrap();
    }

    #[tokio::test]
    async fn integrity_succeeds_for_valid_database() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("valid.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await
                .unwrap();
            write_agent_file(&agent, "/hello.txt", b"hello").await;
        }

        let mut stdout = Vec::new();
        handle_integrity_command(
            &mut stdout,
            db_path.to_string_lossy().to_string(),
            true,
            false,
            false,
            None,
        )
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
            write_agent_file(&agent, "/bad.txt", b"bad").await;
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
        let err = handle_integrity_command(
            &mut stdout,
            db_path.to_string_lossy().to_string(),
            true,
            false,
            false,
            None,
        )
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
    async fn integrity_fails_for_orphan_inode() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("orphan.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await
                .unwrap();
            let conn = agent.get_connection().await.unwrap();
            conn.execute(
                "INSERT INTO fs_inode
                 (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
                  atime_nsec, mtime_nsec, ctime_nsec, data_inline, storage_kind)
                 VALUES (9001, ?, 1, 0, 0, 0, 1, 1, 1, 0, 0, 0, 0, NULL, 0)",
                (S_IFDIR,),
            )
            .await
            .unwrap();
        }

        let mut stdout = Vec::new();
        let err = handle_integrity_command(
            &mut stdout,
            db_path.to_string_lossy().to_string(),
            true,
            false,
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("integrity checks failed"));
        let json: JsonValue = serde_json::from_slice(&stdout).unwrap();
        let failed = json["checks"].as_array().unwrap().iter().any(|check| {
            check["name"] == "namespace.non_root_inode_has_dentry" && check["ok"] == false
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
            write_agent_file(&agent, "/small.txt", b"small").await;
            write_agent_file(&agent, "/large.bin", &large).await;
        }

        let mut stdout = Vec::new();
        handle_backup_command(
            &mut stdout,
            source.to_string_lossy().to_string(),
            target.clone(),
            true,
            false,
            None,
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

    #[tokio::test]
    async fn encrypted_integrity_and_backup_use_key_options() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("encrypted.db");
        let target = temp_dir.path().join("encrypted-backup.db");
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let cipher = "aegis256";
        {
            let agent = AgentFS::open(
                AgentFSOptions::with_path(source.to_string_lossy())
                    .with_encryption_key(key, cipher),
            )
            .await
            .unwrap();
            write_agent_file(&agent, "/secret.txt", b"secret").await;
        }

        let encryption = (key.to_string(), cipher.to_string());
        let mut stdout = Vec::new();
        handle_integrity_command(
            &mut stdout,
            source.to_string_lossy().to_string(),
            true,
            false,
            false,
            Some(&encryption),
        )
        .await
        .unwrap();
        let json: JsonValue = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(json["ok"], true);

        let mut backup_stdout = Vec::new();
        handle_backup_command(
            &mut backup_stdout,
            source.to_string_lossy().to_string(),
            target.clone(),
            true,
            false,
            Some(&encryption),
        )
        .await
        .unwrap();

        let backup = AgentFS::open(
            AgentFSOptions::with_path(target.to_string_lossy()).with_encryption_key(key, cipher),
        )
        .await
        .unwrap();
        assert_eq!(
            backup.fs.read_file("/secret.txt").await.unwrap().unwrap(),
            b"secret"
        );
    }

    #[tokio::test]
    async fn backup_rejects_partial_origin_database() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("partial.db");
        let target = temp_dir.path().join("partial-backup.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(source.to_string_lossy()))
                .await
                .unwrap();
            let conn = agent.get_connection().await.unwrap();
            conn.execute(
                "CREATE TABLE fs_partial_origin (
                    delta_ino INTEGER PRIMARY KEY,
                    base_ino INTEGER NOT NULL,
                    base_path TEXT NOT NULL,
                    base_size INTEGER NOT NULL,
                    base_fingerprint_size INTEGER NOT NULL DEFAULT -1,
                    base_mtime INTEGER NOT NULL DEFAULT 0,
                    base_mtime_nsec INTEGER NOT NULL DEFAULT 0,
                    base_ctime INTEGER NOT NULL DEFAULT 0,
                    base_ctime_nsec INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL
                )",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO fs_partial_origin
                 (delta_ino, base_ino, base_path, base_size, created_at)
                 VALUES (1, 1, '/', 0, 1)",
                (),
            )
            .await
            .unwrap();
        }

        let mut stdout = Vec::new();
        let err = handle_backup_command(
            &mut stdout,
            source.to_string_lossy().to_string(),
            target,
            true,
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("partial-origin"));
    }

    #[tokio::test]
    async fn materialize_reconstructs_tiny_partial_origin_database() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_dir = temp_dir.path().join("base");
        fs::create_dir(&base_dir).unwrap();
        let source = temp_dir.path().join("partial.db");
        let target = temp_dir.path().join("materialized.db");
        let expected = create_synthetic_partial_origin_database(&source, &base_dir).await;

        let mut stdout = Vec::new();
        handle_materialize_command(
            &mut stdout,
            source.to_string_lossy().to_string(),
            target.clone(),
            true,
            None,
        )
        .await
        .unwrap();

        assert_eq!(partial_table_count(&source, "fs_partial_origin").await, 1);
        assert_eq!(partial_table_count(&source, "fs_chunk_override").await, 1);
        assert_portable_materialized_file(&target, &expected).await;
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Materialized partial-origin files: 1"));
        assert!(output.contains("Verification: complete"));
    }

    #[tokio::test]
    async fn backup_materialize_creates_portable_database() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base_dir = temp_dir.path().join("base");
        fs::create_dir(&base_dir).unwrap();
        let source = temp_dir.path().join("partial.db");
        let target = temp_dir.path().join("portable-backup.db");
        let expected = create_synthetic_partial_origin_database(&source, &base_dir).await;

        let mut stdout = Vec::new();
        handle_backup_command(
            &mut stdout,
            source.to_string_lossy().to_string(),
            target.clone(),
            true,
            true,
            None,
        )
        .await
        .unwrap();

        assert_portable_materialized_file(&target, &expected).await;
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Backup:"));
        assert!(output.contains("Materialized partial-origin files: 1"));
        assert!(output.contains("Verification: complete"));
    }

    async fn create_synthetic_partial_origin_database(db_path: &Path, base_dir: &Path) -> Vec<u8> {
        let base_file = base_dir.join("file.bin");
        fs::write(&base_file, b"abcdefghij").unwrap();
        let fingerprint = metadata_fingerprint(&fs::metadata(&base_file).unwrap());
        let expected = b"abcdWXYZij".to_vec();

        let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
            .await
            .unwrap();
        let conn = agent.get_connection().await.unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO fs_config (key, value) VALUES
             ('chunk_size', '4'),
             ('inline_threshold', '0')",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE fs_overlay_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_overlay_config (key, value) VALUES ('base_path', ?)",
            (base_dir.to_string_lossy().to_string(),),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE fs_partial_origin (
                delta_ino INTEGER PRIMARY KEY,
                base_ino INTEGER NOT NULL,
                base_path TEXT NOT NULL,
                base_size INTEGER NOT NULL,
                base_fingerprint_size INTEGER NOT NULL DEFAULT -1,
                base_mtime INTEGER NOT NULL DEFAULT 0,
                base_mtime_nsec INTEGER NOT NULL DEFAULT 0,
                base_ctime INTEGER NOT NULL DEFAULT 0,
                base_ctime_nsec INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE fs_chunk_override (
                delta_ino INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                PRIMARY KEY (delta_ino, chunk_index)
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_inode (
                ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
                atime_nsec, mtime_nsec, ctime_nsec, data_inline, storage_kind
             ) VALUES (?, ?, 1, 0, 0, ?, 1, ?, ?, 0, 0, ?, ?, NULL, ?)",
            (
                2_i64,
                S_IFREG | 0o644,
                10_i64,
                fingerprint.mtime,
                fingerprint.ctime,
                fingerprint.mtime_nsec,
                fingerprint.ctime_nsec,
                STORAGE_CHUNKED,
            ),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES ('file.bin', 1, 2)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_partial_origin (
                delta_ino, base_ino, base_path, base_size, base_fingerprint_size,
                base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec, created_at
             ) VALUES (2, 42, '/file.bin', 10, ?, ?, ?, ?, ?, 1)",
            (
                fingerprint.size,
                fingerprint.mtime,
                fingerprint.mtime_nsec,
                fingerprint.ctime,
                fingerprint.ctime_nsec,
            ),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (2, 1, ?)",
            (Value::Blob(b"WXYZ".to_vec()),),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_chunk_override (delta_ino, chunk_index) VALUES (2, 1)",
            (),
        )
        .await
        .unwrap();

        expected
    }

    async fn assert_portable_materialized_file(db_path: &Path, expected: &[u8]) {
        assert_eq!(partial_table_count(db_path, "fs_partial_origin").await, 0);
        assert_eq!(partial_table_count(db_path, "fs_chunk_override").await, 0);

        let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
            .await
            .unwrap();
        assert_eq!(
            agent.fs.read_file("/file.bin").await.unwrap().unwrap(),
            expected
        );
        let conn = agent.get_connection().await.unwrap();
        let report = integrity_report(
            &conn,
            db_path,
            IntegrityOptions {
                require_portable: true,
                check_base: false,
            },
        )
        .await
        .unwrap();
        assert!(report.ok);
    }

    async fn partial_table_count(db_path: &Path, table: &str) -> i64 {
        let db = build_local_database(db_path, None).await.unwrap();
        let conn = db.connect().unwrap();
        if !table_exists(&conn, table).await.unwrap() {
            return 0;
        }
        scalar_i64(
            &conn,
            &format!("SELECT COUNT(*) FROM {}", quote_identifier(table)),
        )
        .await
        .unwrap()
    }
}
