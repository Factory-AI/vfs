//! Production safety commands for local AgentFS databases.

use agentfs_core::{schema::integrity, AgentFSOptions};
use anyhow::{Context, Result as AnyhowResult};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Builder, Connection, EncryptionOpts, Value};

const S_IFMT: i64 = 0o170000;
const S_IFREG: i64 = 0o100000;
#[cfg(test)]
const S_IFDIR: i64 = 0o040000;
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

/// Run integrity and schema-invariant checks for a local AgentFS database.
pub async fn handle_integrity_command(
    stdout: &mut impl Write,
    id_or_path: String,
    json: bool,
    require_portable: bool,
    check_base: bool,
    checkpoint: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    // Failures before a report exists (unresolvable path, unopenable or
    // hard-corrupt database) still emit a minimal JSON object under --json so
    // scripted consumers always get `ok` on stdout.
    let (db_path, report) =
        match open_and_check_integrity(&id_or_path, require_portable, check_base, encryption).await
        {
            Ok(checked) => checked,
            Err(err) => {
                if json {
                    let error_report = serde_json::json!({
                        "ok": false,
                        "error": format!("{err:#}"),
                    });
                    serde_json::to_writer_pretty(&mut *stdout, &error_report)?;
                    writeln!(stdout)?;
                }
                return Err(err);
            }
        };
    write_integrity_report(stdout, &report, json)?;
    if !report.ok {
        anyhow::bail!("integrity checks failed for {}", db_path.display());
    }
    if !checkpoint {
        return Ok(());
    }
    let cleanup_db = build_local_database(&db_path, encryption).await?;
    let cleanup_conn = cleanup_db
        .connect()
        .context("Failed to connect to database for sidecar cleanup")?;
    checkpoint_for_backup(&cleanup_conn, &db_path).await?;
    drop(cleanup_conn);
    drop(cleanup_db);
    remove_sqlite_sidecars_after_checkpoint(&db_path)?;
    if !json {
        writeln!(stdout, "Checkpoint: complete")?;
    }
    Ok(())
}

async fn open_and_check_integrity(
    id_or_path: &str,
    require_portable: bool,
    check_base: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<(PathBuf, integrity::Report)> {
    let db_path = resolve_local_db_path(id_or_path)?;
    let sidecars = ReadOnlyOpenSidecars::capture(&db_path);
    let result = check_integrity_readonly(&db_path, require_portable, check_base, encryption).await;
    sidecars.remove_created_frameless();
    Ok((db_path, result?))
}

async fn check_integrity_readonly(
    db_path: &Path,
    require_portable: bool,
    check_base: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<integrity::Report> {
    let db = build_local_database(db_path, encryption).await?;
    let conn = db.connect().context("Failed to connect to database")?;
    conn.execute("PRAGMA query_only = 1", ())
        .await
        .context("Failed to enable query_only mode")?;

    let report = integrity::check(
        &conn,
        &integrity::CheckOpts::new(db_path.to_path_buf())
            .require_portable(require_portable)
            .check_base(check_base),
    )
    .await?;
    Ok(report)
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
            let report = integrity::check(
                &backup_conn,
                &integrity::CheckOpts::new(target.clone()).require_portable(true),
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

pub(crate) async fn build_local_database(
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

    let report = integrity::check(
        &target_conn,
        &integrity::CheckOpts::new(target.to_path_buf()).require_portable(true),
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
            let report = integrity::check(
                &verify_conn,
                &integrity::CheckOpts::new(target.to_path_buf()).require_portable(true),
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
    report: &integrity::Report,
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

/// Sidecar census around a raw turso open on a read path. turso materializes
/// an empty `-wal` next to the main file the moment a database is opened,
/// even for query-only connections; the frameless sidecars the open itself
/// created must be removed on exit (single-file invariant I1) without ever
/// touching a pre-existing family, which read paths must not mutate.
pub(crate) struct ReadOnlyOpenSidecars {
    wal: PathBuf,
    shm: PathBuf,
    wal_preexisted: bool,
    shm_preexisted: bool,
}

impl ReadOnlyOpenSidecars {
    pub(crate) fn capture(db_path: &Path) -> Self {
        let wal = sidecar_path(db_path, "-wal");
        let shm = sidecar_path(db_path, "-shm");
        let wal_preexisted = wal.exists();
        let shm_preexisted = shm.exists();
        Self {
            wal,
            shm,
            wal_preexisted,
            shm_preexisted,
        }
    }

    /// Best-effort removal of sidecars this open created. A WAL is removed
    /// only while frameless (empty or the bare 32-byte header): once frames
    /// exist, unlinking would drop committed data.
    pub(crate) fn remove_created_frameless(&self) {
        const WAL_HEADER_SIZE: u64 = 32;
        if !self.wal_preexisted {
            if let Ok(metadata) = fs::metadata(&self.wal) {
                if metadata.len() <= WAL_HEADER_SIZE {
                    let _ = fs::remove_file(&self.wal);
                }
            }
        }
        if !self.shm_preexisted && self.shm.exists() {
            let _ = fs::remove_file(&self.shm);
        }
    }
}

pub(crate) fn remove_sqlite_sidecars_after_checkpoint(path: &Path) -> AnyhowResult<()> {
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
    use agentfs_core::{AgentFS, AgentFSOptions};
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

    fn db_family_stats(db_path: &Path) -> Vec<(String, u64, Option<std::time::SystemTime>)> {
        ["", "-wal", "-shm"]
            .into_iter()
            .filter_map(|suffix| {
                let path = PathBuf::from(format!("{}{}", db_path.display(), suffix));
                let metadata = fs::metadata(&path).ok()?;
                Some((suffix.to_string(), metadata.len(), metadata.modified().ok()))
            })
            .collect()
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
            false,
            None,
        )
        .await
        .unwrap();
        let json: JsonValue = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(json["ok"], true);
    }

    #[tokio::test]
    async fn integrity_is_read_only_unless_checkpoint_requested() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("read-only.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await
                .unwrap();
            write_agent_file(&agent, "/hello.txt", b"hello").await;
        }

        let before = db_family_stats(&db_path);
        let mut stdout = Vec::new();
        handle_integrity_command(
            &mut stdout,
            db_path.to_string_lossy().to_string(),
            false,
            false,
            false,
            false,
            None,
        )
        .await
        .unwrap();
        assert_eq!(db_family_stats(&db_path), before);

        let mut checkpoint_stdout = Vec::new();
        handle_integrity_command(
            &mut checkpoint_stdout,
            db_path.to_string_lossy().to_string(),
            false,
            false,
            false,
            true,
            None,
        )
        .await
        .unwrap();
        let output = String::from_utf8(checkpoint_stdout).unwrap();
        assert!(output.contains("Checkpoint: complete"));
    }

    #[tokio::test]
    async fn integrity_json_emits_error_object_for_hard_corruption() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("hard-corrupt.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await
                .unwrap();
            write_agent_file(&agent, "/hello.txt", b"hello").await;
        }
        {
            use std::io::{Seek, SeekFrom, Write as IoWrite};
            let mut file = fs::OpenOptions::new().write(true).open(&db_path).unwrap();
            file.seek(SeekFrom::Start(4096)).unwrap();
            file.write_all(&[0xa0; 4096]).unwrap();
        }

        let mut stdout = Vec::new();
        let err = handle_integrity_command(
            &mut stdout,
            db_path.to_string_lossy().to_string(),
            true,
            false,
            false,
            false,
            None,
        )
        .await
        .unwrap_err();
        let json: JsonValue = serde_json::from_slice(&stdout)
            .unwrap_or_else(|parse| panic!("--json must emit a JSON object on stdout even for hard corruption; stdout={:?} parse={parse} err={err:#}", String::from_utf8_lossy(&stdout)));
        assert_eq!(json["ok"], false);
        let error = json["error"].as_str().unwrap();
        assert!(!error.is_empty());
    }

    #[tokio::test]
    async fn integrity_read_only_tolerates_stale_shm_without_mutating_family() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("stale-shm.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await
                .unwrap();
            write_agent_file(&agent, "/hello.txt", b"hello").await;
        }
        // Crash residue from a SIGKILLed writer: a wal-index sidecar whose
        // contents cannot be trusted. The read-only open must not consult it
        // and must not repair or remove it.
        let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
        fs::write(&shm_path, [0xde, 0xad, 0xbe, 0xef].repeat(8192)).unwrap();

        let family_before: Vec<(String, Vec<u8>)> = ["", "-wal", "-shm"]
            .into_iter()
            .filter_map(|suffix| {
                let path = PathBuf::from(format!("{}{}", db_path.display(), suffix));
                fs::read(&path)
                    .ok()
                    .map(|bytes| (suffix.to_string(), bytes))
            })
            .collect();

        for json in [false, true] {
            let mut stdout = Vec::new();
            handle_integrity_command(
                &mut stdout,
                db_path.to_string_lossy().to_string(),
                json,
                false,
                false,
                false,
                None,
            )
            .await
            .unwrap_or_else(|err| {
                panic!("read-only integrity must tolerate a stale -shm (json={json}): {err:#}")
            });
        }

        let family_after: Vec<(String, Vec<u8>)> = ["", "-wal", "-shm"]
            .into_iter()
            .filter_map(|suffix| {
                let path = PathBuf::from(format!("{}{}", db_path.display(), suffix));
                fs::read(&path)
                    .ok()
                    .map(|bytes| (suffix.to_string(), bytes))
            })
            .collect();
        assert_eq!(
            family_before, family_after,
            "read-only integrity must not mutate the database family"
        );
    }

    #[tokio::test]
    async fn integrity_read_only_creates_no_sidecars() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("single-file.db");
        {
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy()))
                .await
                .unwrap();
            write_agent_file(&agent, "/hello.txt", b"hello").await;
            agent.fs.finalize().await.unwrap();
        }
        let wal = PathBuf::from(format!("{}-wal", db_path.display()));
        let shm = PathBuf::from(format!("{}-shm", db_path.display()));
        assert!(!wal.exists(), "fixture must start as a single file");

        let mut stdout = Vec::new();
        handle_integrity_command(
            &mut stdout,
            db_path.to_string_lossy().to_string(),
            false,
            false,
            false,
            false,
            None,
        )
        .await
        .unwrap();

        assert!(
            !wal.exists(),
            "read-only integrity must not leave a WAL sidecar it created"
        );
        assert!(
            !shm.exists(),
            "read-only integrity must not leave an SHM sidecar it created"
        );
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
            "INSERT OR REPLACE INTO fs_overlay_config (key, value) VALUES ('base_path', ?)",
            (base_dir.to_string_lossy().to_string(),),
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
        let report = integrity::check(
            &conn,
            &integrity::CheckOpts::new(db_path.to_path_buf()).require_portable(true),
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
