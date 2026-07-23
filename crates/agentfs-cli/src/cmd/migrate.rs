//! Database schema migration command.
//!
//! One `agentfs migrate` lands any supported old schema at the current
//! version: in place by default (every supported migration is an additive,
//! transactional ALTER), or copy-based via `--copy <target>` for users who
//! want a rebuilt database with the current chunk layout, keeping the
//! hash/verify engine from the historical `migrate-v0-5` command.

use agentfs_core::{
    config::{DEFAULT_CHUNK_SIZE, DEFAULT_INLINE_THRESHOLD},
    error::Error as SdkError,
    schema, AgentFSOptions, SchemaVersion,
};
use anyhow::{Context, Result as AnyhowResult};
use std::collections::{hash_map::DefaultHasher, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::{Read as IoRead, Write};
use std::path::{Path, PathBuf};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

use super::safety::{build_local_database, ReadOnlyOpenSidecars};

const S_IFMT: i64 = 0o170000;
const S_IFREG: i64 = 0o100000;

/// Guidance for a schema-version mismatch surfaced by an open path.
///
/// Names a command that actually finishes the job: supported old versions get
/// `agentfs migrate <id-or-path>` (which lands at CURRENT in one invocation);
/// anything else is from a newer agentfs and no local command can help.
fn schema_upgrade_guidance(found: &str, expected: &str, id_or_path: &str) -> String {
    match SchemaVersion::parse(found) {
        Some(version) if version < schema::CURRENT => format!(
            "Filesystem `{id_or_path}` requires migration\n\n\
             Found schema version {found}, but this version of agentfs requires {expected}.\n\n\
             To upgrade, run:\n\n    agentfs migrate {id_or_path}\n"
        ),
        _ => format!(
            "Filesystem `{id_or_path}` has unsupported schema version {found}; \
             this version of agentfs supports up to {expected}.\n\
             The database was likely created by a newer agentfs. Upgrade agentfs to open it."
        ),
    }
}

/// Convert an SDK open error into an anyhow error, attaching migrate guidance
/// when the failure is a schema-version mismatch.
pub(crate) fn open_error_with_guidance(err: SdkError, id_or_path: &str) -> anyhow::Error {
    match &err {
        SdkError::SchemaVersionMismatch { found, expected } => {
            anyhow::anyhow!("{}", schema_upgrade_guidance(found, expected, id_or_path))
        }
        _ => err.into(),
    }
}

/// Handle the in-place migrate command.
pub async fn handle_migrate_command(
    stdout: &mut impl Write,
    id_or_path: String,
    dry_run: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    let db_path_str = options
        .db_path()
        .context("Failed to resolve database path")?;
    let db_path = Path::new(&db_path_str);

    if !db_path.exists() {
        anyhow::bail!("Database not found: {}", db_path.display());
    }

    writeln!(stdout, "Database: {}", db_path.display())?;

    let sidecars = ReadOnlyOpenSidecars::capture(db_path);
    let result = migrate_in_place(stdout, db_path, dry_run, encryption, &sidecars).await;
    if result.is_err() {
        // A failed migrate must not leave behind the frameless WAL/SHM its
        // own open materialized (single-file invariant I1); the database and
        // connection are already dropped when migrate_in_place returns.
        sidecars.remove_created_frameless();
    }
    result
}

/// The open-to-finish body of the in-place migrate. Owns the database handle
/// so every return path (success or error) has dropped it before the caller
/// runs sidecar cleanup.
async fn migrate_in_place(
    stdout: &mut impl Write,
    db_path: &Path,
    dry_run: bool,
    encryption: Option<&(String, String)>,
    sidecars: &ReadOnlyOpenSidecars,
) -> AnyhowResult<()> {
    // Open directly via turso::Builder (not the SDK) so the open-path version
    // gate does not reject the database migrate exists to upgrade.
    let db = build_local_database(db_path, encryption).await?;
    let conn = db.connect().context("Failed to connect to database")?;

    let current_version = schema::detect_schema_version(&conn)
        .await?
        .unwrap_or(SchemaVersion::V0_0);
    writeln!(stdout, "Current schema version: {}", current_version)?;
    writeln!(
        stdout,
        "Target schema version: {} (CURRENT)",
        schema::CURRENT
    )?;

    if current_version == schema::CURRENT {
        writeln!(stdout, "Database is already at schema {}.", schema::CURRENT)?;
        drop(conn);
        drop(db);
        sidecars.remove_created_frameless();
        return Ok(());
    }

    if dry_run {
        writeln!(
            stdout,
            "\n[DRY RUN] The following migrations would be applied:"
        )?;
        print_pending_migrations(stdout, current_version)?;
        writeln!(stdout, "\nRun without --dry-run to apply migrations.")?;
        drop(conn);
        drop(db);
        sidecars.remove_created_frameless();
    } else {
        writeln!(stdout, "\nApplying migrations...")?;
        schema::ensure_current(&conn)
            .await
            .context("Failed to migrate schema to current")?;

        // Leave a single portable main-db file behind: checkpoint the WAL and
        // drop the now-empty sidecars.
        checkpoint_target(&conn, db_path).await?;
        drop(conn);
        drop(db);
        super::safety::remove_sqlite_sidecars_after_checkpoint(db_path)?;

        writeln!(stdout, "\nMigration completed successfully.")?;
    }

    Ok(())
}

/// Handle the copy-based migration mode (`migrate <source> --copy <target>`).
pub async fn handle_migrate_copy_command(
    stdout: &mut impl Write,
    id_or_path: String,
    target: PathBuf,
    verify: bool,
    overwrite_target: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let options = AgentFSOptions::resolve(&id_or_path)?;
    let source_path_str = options
        .db_path()
        .context("Failed to resolve database path")?;
    let source = PathBuf::from(source_path_str);
    copy_migrate_to_current(
        stdout,
        &source,
        &target,
        verify,
        overwrite_target,
        encryption,
    )
    .await
}

/// Print pending migrations without applying them.
fn print_pending_migrations(
    stdout: &mut impl Write,
    from_version: SchemaVersion,
) -> AnyhowResult<()> {
    match from_version {
        version if version == schema::CURRENT => {}
        version => {
            for migration in schema::pending_migrations(version) {
                writeln!(
                    stdout,
                    "  - {} -> {}: {}",
                    migration.from, migration.to, migration.description
                )?;
            }
        }
    }
    Ok(())
}

async fn copy_migrate_to_current(
    stdout: &mut impl Write,
    source_path: &Path,
    target_path: &Path,
    verify: bool,
    overwrite_target: bool,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    if !source_path.exists() {
        anyhow::bail!("Source database not found: {}", source_path.display());
    }
    if source_path == target_path {
        anyhow::bail!("Source and target must be different paths");
    }
    if target_path.exists() {
        if !overwrite_target {
            anyhow::bail!(
                "Target already exists: {} (pass --overwrite-target to replace it)",
                target_path.display()
            );
        }
        if source_path.canonicalize()? == target_path.canonicalize()? {
            anyhow::bail!("Source and target must be different databases");
        }
        remove_db_family(target_path)?;
    }

    let source_db = build_local_database(source_path, encryption)
        .await
        .context("Failed to open source database")?;
    let source_conn = source_db
        .connect()
        .context("Failed to connect to source database")?;

    let source_txn = Transaction::new_unchecked(&source_conn, TransactionBehavior::Immediate)
        .await
        .context("Failed to lock source database for copy migration")?;
    let source_hash_before = hash_db_family(source_path)
        .with_context(|| format!("Failed to hash source {}", source_path.display()))?;

    run_integrity_check(&source_conn, "source").await?;
    let source_version = schema::detect_schema_version(&source_conn)
        .await?
        .unwrap_or(SchemaVersion::V0_0);
    if source_version == schema::CURRENT {
        anyhow::bail!(
            "Source database is already at schema {}. Run `agentfs migrate {}` for in-place \
             normalization or `agentfs backup` for a portable copy.",
            schema::CURRENT,
            source_path.display()
        );
    }
    let source_chunk_size = read_config_usize(&source_conn, "chunk_size", 4096).await?;

    let target_db = build_local_database(target_path, encryption)
        .await
        .context("Failed to create target database")?;
    let target_conn = target_db
        .connect()
        .context("Failed to connect to target database")?;

    writeln!(stdout, "Source: {}", source_path.display())?;
    writeln!(stdout, "Target: {}", target_path.display())?;
    writeln!(stdout, "Source schema version: {source_version}")?;
    writeln!(
        stdout,
        "Target schema version: {} (CURRENT)",
        schema::CURRENT
    )?;

    create_current_schema(&target_conn).await?;

    let txn = Transaction::new_unchecked(&target_conn, TransactionBehavior::Immediate).await?;
    let copy_result: AnyhowResult<()> = async {
        copy_fs_config(&source_conn, &target_conn).await?;
        migrate_inodes_and_file_data(&source_conn, &target_conn, source_chunk_size).await?;
        copy_table_common_columns(&source_conn, &target_conn, "fs_dentry").await?;
        backfill_target_nlink_if_missing(&source_conn, &target_conn).await?;
        copy_table_common_columns(&source_conn, &target_conn, "fs_symlink").await?;
        copy_optional_whiteouts(&source_conn, &target_conn).await?;
        copy_optional_table_common_columns(&source_conn, &target_conn, "fs_origin").await?;
        copy_optional_table_common_columns(&source_conn, &target_conn, "fs_overlay_config").await?;
        copy_table_common_columns(&source_conn, &target_conn, "kv_store").await?;
        copy_table_common_columns(&source_conn, &target_conn, "tool_calls").await?;
        Ok(())
    }
    .await;

    match copy_result {
        Ok(()) => txn.commit().await?,
        Err(err) => {
            let _ = txn.rollback().await;
            return Err(err);
        }
    }

    if verify {
        verify_migration_equivalence(&source_conn, &target_conn).await?;
        checkpoint_target_and_verify_copy(&source_conn, &target_conn, target_path, encryption)
            .await?;
    } else {
        checkpoint_target(&target_conn, target_path).await?;
    }

    let source_hash_after = hash_db_family(source_path)
        .with_context(|| format!("Failed to re-hash source {}", source_path.display()))?;
    if source_hash_before != source_hash_after {
        anyhow::bail!("Source database changed during copy migration");
    }
    source_txn.rollback().await?;

    writeln!(stdout, "Migration completed successfully.")?;
    writeln!(stdout, "Source database hash unchanged.")?;
    if verify {
        writeln!(stdout, "Verification completed successfully.")?;
    }
    Ok(())
}

async fn create_current_schema(conn: &Connection) -> AnyhowResult<()> {
    schema::ensure_current(conn)
        .await
        .map_err(anyhow::Error::from)
}

/// The v0.0 -> v0.2 nlink backfill rule, applied on the copy target when the
/// source predates the nlink column. Must stay in step with the in-place
/// migration in `agentfs_core::schema`.
const NLINK_BACKFILL_CASE: &str = "CASE
     WHEN fs_inode.ino = 1 THEN 2
     WHEN (fs_inode.mode & 61440) = 16384
         THEN MAX(1, (SELECT COUNT(*) FROM fs_dentry d WHERE d.ino = fs_inode.ino))
     ELSE (SELECT COUNT(*) FROM fs_dentry d WHERE d.ino = fs_inode.ino)
 END";

async fn backfill_target_nlink_if_missing(
    source: &Connection,
    target: &Connection,
) -> AnyhowResult<()> {
    if source_has_column(source, "fs_inode", "nlink").await? {
        return Ok(());
    }
    target
        .execute(
            &format!("UPDATE fs_inode SET nlink = {NLINK_BACKFILL_CASE}"),
            (),
        )
        .await?;
    Ok(())
}

async fn source_has_column(conn: &Connection, table: &str, column: &str) -> AnyhowResult<bool> {
    Ok(get_table_columns(conn, table)
        .await?
        .iter()
        .any(|name| name == column))
}

async fn copy_fs_config(source: &Connection, target: &Connection) -> AnyhowResult<()> {
    let mut rows = source
        .query(
            "SELECT key, value FROM fs_config
             WHERE key NOT IN (?, ?, ?)
             ORDER BY key",
            (
                schema::CONFIG_SCHEMA_VERSION_KEY,
                schema::CONFIG_CHUNK_SIZE_KEY,
                schema::CONFIG_INLINE_THRESHOLD_KEY,
            ),
        )
        .await?;

    while let Some(row) = rows.next().await? {
        let key: String = row.get(0)?;
        let value: String = row.get(1)?;
        target
            .execute(
                "INSERT OR REPLACE INTO fs_config (key, value) VALUES (?, ?)",
                (key, value),
            )
            .await?;
    }

    target
        .execute(
            "INSERT OR REPLACE INTO fs_config (key, value) VALUES (?, ?)",
            (schema::CONFIG_SCHEMA_VERSION_KEY, schema::CURRENT.as_str()),
        )
        .await?;
    target
        .execute(
            "INSERT OR REPLACE INTO fs_config (key, value) VALUES (?, ?)",
            (
                schema::CONFIG_CHUNK_SIZE_KEY,
                DEFAULT_CHUNK_SIZE.to_string(),
            ),
        )
        .await?;
    target
        .execute(
            "INSERT OR REPLACE INTO fs_config (key, value) VALUES (?, ?)",
            (
                schema::CONFIG_INLINE_THRESHOLD_KEY,
                DEFAULT_INLINE_THRESHOLD.to_string(),
            ),
        )
        .await?;
    Ok(())
}

async fn migrate_inodes_and_file_data(
    source: &Connection,
    target: &Connection,
    source_chunk_size: usize,
) -> AnyhowResult<()> {
    // Older schemas predate some inode columns (v0.0: nlink; v0.0/v0.2:
    // rdev and the *_nsec columns); select zeros for the missing ones so one
    // copy loop serves every supported source version.
    let source_columns = get_table_columns(source, "fs_inode").await?;
    let select_column = |name: &str| {
        if source_columns.iter().any(|column| column == name) {
            quote_identifier(name)
        } else {
            format!("0 AS {}", quote_identifier(name))
        }
    };
    let select_sql = format!(
        "SELECT ino, mode, {}, uid, gid, size, atime, mtime, ctime, {}, {}, {}, {}
         FROM fs_inode
         ORDER BY ino",
        select_column("nlink"),
        select_column("rdev"),
        select_column("atime_nsec"),
        select_column("mtime_nsec"),
        select_column("ctime_nsec"),
    );
    let mut rows = source.query(&select_sql, ()).await?;

    while let Some(row) = rows.next().await? {
        let ino = row_i64(&row, 0)?;
        let mode = row_i64(&row, 1)?;
        let nlink = row_i64(&row, 2)?;
        let uid = row_i64(&row, 3)?;
        let gid = row_i64(&row, 4)?;
        let size = row_i64(&row, 5)?;
        let atime = row_i64(&row, 6)?;
        let mtime = row_i64(&row, 7)?;
        let ctime = row_i64(&row, 8)?;
        let rdev = row_i64(&row, 9)?;
        let atime_nsec = row_i64(&row, 10)?;
        let mtime_nsec = row_i64(&row, 11)?;
        let ctime_nsec = row_i64(&row, 12)?;

        let is_regular = (mode & S_IFMT) == S_IFREG;
        let (storage_kind, data_inline) =
            if is_regular && (size as usize) <= DEFAULT_INLINE_THRESHOLD {
                let (bytes, dense) =
                    read_source_file_bytes(source, ino, size as usize, source_chunk_size).await?;
                if dense {
                    (1_i64, Value::Blob(bytes))
                } else {
                    (0_i64, Value::Null)
                }
            } else {
                (0_i64, Value::Null)
            };

        target
            .execute(
                "INSERT INTO fs_inode (
                    ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
                    atime_nsec, mtime_nsec, ctime_nsec, data_inline, storage_kind
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    Value::Integer(ino),
                    Value::Integer(mode),
                    Value::Integer(nlink),
                    Value::Integer(uid),
                    Value::Integer(gid),
                    Value::Integer(size),
                    Value::Integer(atime),
                    Value::Integer(mtime),
                    Value::Integer(ctime),
                    Value::Integer(rdev),
                    Value::Integer(atime_nsec),
                    Value::Integer(mtime_nsec),
                    Value::Integer(ctime_nsec),
                    data_inline,
                    Value::Integer(storage_kind),
                ],
            )
            .await?;

        if is_regular && storage_kind == 0 {
            copy_source_file_chunks_to_target(
                source,
                target,
                ino,
                size as usize,
                source_chunk_size,
            )
            .await?;
        }
    }

    Ok(())
}

async fn copy_source_file_chunks_to_target(
    source: &Connection,
    target: &Connection,
    ino: i64,
    size: usize,
    source_chunk_size: usize,
) -> AnyhowResult<()> {
    let mut rows = source
        .query(
            "SELECT chunk_index, data FROM fs_data WHERE ino = ? ORDER BY chunk_index",
            (ino,),
        )
        .await?;
    let mut target_chunk_index: Option<i64> = None;
    let mut target_chunk = Vec::new();
    let mut target_chunk_has_data = false;

    while let Some(row) = rows.next().await? {
        let chunk_index = row_i64(&row, 0)? as usize;
        let chunk_data = match row.get_value(1)? {
            Value::Blob(data) => data.clone(),
            _ => Vec::new(),
        };
        let mut source_offset = chunk_index.saturating_mul(source_chunk_size);
        if source_offset >= size {
            continue;
        }
        let mut remaining = &chunk_data[..std::cmp::min(chunk_data.len(), size - source_offset)];

        while !remaining.is_empty() {
            let next_target_index = (source_offset / DEFAULT_CHUNK_SIZE) as i64;
            if target_chunk_index != Some(next_target_index) {
                flush_target_chunk(
                    target,
                    ino,
                    target_chunk_index,
                    &target_chunk,
                    target_chunk_has_data,
                )
                .await?;
                target_chunk_index = Some(next_target_index);
                let chunk_start = next_target_index as usize * DEFAULT_CHUNK_SIZE;
                let chunk_len = std::cmp::min(DEFAULT_CHUNK_SIZE, size - chunk_start);
                target_chunk = vec![0; chunk_len];
            }

            let in_chunk_offset = source_offset % DEFAULT_CHUNK_SIZE;
            let copy_len = std::cmp::min(remaining.len(), target_chunk.len() - in_chunk_offset);
            target_chunk[in_chunk_offset..in_chunk_offset + copy_len]
                .copy_from_slice(&remaining[..copy_len]);
            target_chunk_has_data = true;
            source_offset += copy_len;
            remaining = &remaining[copy_len..];
        }
    }

    flush_target_chunk(
        target,
        ino,
        target_chunk_index,
        &target_chunk,
        target_chunk_has_data,
    )
    .await
}

async fn flush_target_chunk(
    target: &Connection,
    ino: i64,
    chunk_index: Option<i64>,
    chunk: &[u8],
    has_data: bool,
) -> AnyhowResult<()> {
    if !has_data || chunk.iter().all(|byte| *byte == 0) {
        return Ok(());
    }

    let Some(chunk_index) = chunk_index else {
        return Ok(());
    };
    target
        .execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (?, ?, ?)",
            (ino, chunk_index, Value::Blob(chunk.to_vec())),
        )
        .await?;
    Ok(())
}

async fn read_source_file_bytes(
    conn: &Connection,
    ino: i64,
    size: usize,
    chunk_size: usize,
) -> AnyhowResult<(Vec<u8>, bool)> {
    let mut bytes = vec![0; size];
    let mut rows = conn
        .query(
            "SELECT chunk_index, data FROM fs_data WHERE ino = ? ORDER BY chunk_index",
            (ino,),
        )
        .await?;
    let mut expected_offset = 0usize;
    let mut dense = true;

    while let Some(row) = rows.next().await? {
        let chunk_index = row_i64(&row, 0)? as usize;
        let chunk_data = match row.get_value(1)? {
            Value::Blob(data) => data.clone(),
            _ => Vec::new(),
        };
        let start = chunk_index.saturating_mul(chunk_size);
        if start != expected_offset {
            dense = false;
        }
        if start >= size {
            dense = false;
            continue;
        }
        let copy_len = std::cmp::min(chunk_data.len(), size - start);
        bytes[start..start + copy_len].copy_from_slice(&chunk_data[..copy_len]);

        let expected_len = std::cmp::min(chunk_size, size - start);
        if chunk_data.len() < expected_len {
            dense = false;
        }
        if chunk_data.len() > expected_len && start + chunk_data.len() > size {
            dense = false;
        }
        expected_offset = start + expected_len;
    }

    if expected_offset < size {
        dense = false;
    }
    if size == 0 {
        dense = true;
    }
    Ok((bytes, dense))
}

async fn copy_optional_table_common_columns(
    source: &Connection,
    target: &Connection,
    table: &str,
) -> AnyhowResult<()> {
    if table_exists(source, table).await? {
        copy_table_common_columns(source, target, table).await?;
    }
    Ok(())
}

async fn copy_optional_whiteouts(source: &Connection, target: &Connection) -> AnyhowResult<()> {
    if !table_exists(source, "fs_whiteout").await? {
        return Ok(());
    }

    let columns = get_table_columns(source, "fs_whiteout").await?;
    let has_parent_path = columns.iter().any(|column| column == "parent_path");
    let sql = if has_parent_path {
        "SELECT path, parent_path, created_at FROM fs_whiteout ORDER BY path"
    } else {
        "SELECT path, created_at FROM fs_whiteout ORDER BY path"
    };
    let mut rows = source.query(sql, ()).await?;
    while let Some(row) = rows.next().await? {
        let path = row.get::<String>(0)?;
        let (parent_path, created_at) = if has_parent_path {
            (row.get::<String>(1)?, row_i64(&row, 2)?)
        } else {
            (parent_path_for_path(&path), row_i64(&row, 1)?)
        };
        target
            .execute(
                "INSERT INTO fs_whiteout (path, parent_path, created_at) VALUES (?, ?, ?)",
                (path, parent_path, created_at),
            )
            .await?;
    }
    Ok(())
}

async fn copy_table_common_columns(
    source: &Connection,
    target: &Connection,
    table: &str,
) -> AnyhowResult<()> {
    let source_columns = get_table_columns(source, table).await?;
    let target_columns = get_table_columns(target, table).await?;
    let target_set = target_columns.iter().cloned().collect::<HashSet<_>>();
    let columns = source_columns
        .into_iter()
        .filter(|column| target_set.contains(column))
        .collect::<Vec<_>>();
    if columns.is_empty() {
        return Ok(());
    }

    let select_sql = format!(
        "SELECT {} FROM {}",
        columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", "),
        quote_identifier(table)
    );
    let placeholders = std::iter::repeat_n("?", columns.len())
        .collect::<Vec<_>>()
        .join(", ");
    let insert_sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_identifier(table),
        columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", "),
        placeholders
    );

    let mut rows = source.query(&select_sql, ()).await?;
    while let Some(row) = rows.next().await? {
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            values.push(row.get_value(index)?.clone());
        }
        target.execute(&insert_sql, values).await?;
    }
    Ok(())
}

async fn verify_migration_equivalence(
    source: &Connection,
    target: &Connection,
) -> AnyhowResult<()> {
    run_integrity_check(source, "source").await?;
    run_integrity_check(target, "target").await?;
    verify_target_v0_5_invariants(target).await?;
    verify_target_v0_5_config(target).await?;
    // Compare the inode columns both sides share; columns the source predates
    // (nlink for v0.0, rdev/*_nsec for v0.0/v0.2) are zero-defaulted or
    // backfilled on the target and verified against their rule below.
    compare_common_table_rows(source, target, "fs_inode").await?;
    verify_target_nlink_rule_if_source_missing(source, target).await?;
    compare_table_rows(
        source,
        target,
        "fs_dentry",
        &["id", "name", "parent_ino", "ino"],
    )
    .await?;
    compare_table_rows(source, target, "fs_symlink", &["ino", "target"]).await?;
    compare_optional_whiteouts(source, target).await?;
    compare_optional_table_rows(source, target, "fs_origin", &["delta_ino", "base_ino"]).await?;
    compare_optional_table_rows(source, target, "fs_overlay_config", &["key", "value"]).await?;
    compare_table_rows(
        source,
        target,
        "kv_store",
        &["key", "value", "created_at", "updated_at"],
    )
    .await?;
    compare_common_table_rows(source, target, "tool_calls").await?;
    compare_regular_file_contents(source, target).await?;
    Ok(())
}

async fn verify_target_nlink_rule_if_source_missing(
    source: &Connection,
    target: &Connection,
) -> AnyhowResult<()> {
    if source_has_column(source, "fs_inode", "nlink").await? {
        return Ok(());
    }
    let sql = format!("SELECT ino FROM fs_inode WHERE nlink != {NLINK_BACKFILL_CASE} LIMIT 1");
    let mut rows = target.query(&sql, ()).await?;
    if let Some(row) = rows.next().await? {
        let ino = row_i64(&row, 0).unwrap_or_default();
        anyhow::bail!("Target nlink backfill violates the migration rule (ino {ino})");
    }
    Ok(())
}

async fn checkpoint_target_and_verify_copy(
    source: &Connection,
    target: &Connection,
    target_path: &Path,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    checkpoint_target(target, target_path).await?;
    let snapshot_path = target_path.with_extension("snapshot-check.db");
    remove_db_family(&snapshot_path)?;
    fs::copy(target_path, &snapshot_path).with_context(|| {
        format!(
            "Failed to copy target main database {} to {}",
            target_path.display(),
            snapshot_path.display()
        )
    })?;
    let snapshot_db = build_local_database(&snapshot_path, encryption)
        .await
        .context("Failed to open target main-db snapshot")?;
    let snapshot_conn = snapshot_db
        .connect()
        .context("Failed to connect to target main-db snapshot")?;
    verify_migration_equivalence(source, &snapshot_conn).await?;
    remove_db_family(&snapshot_path)?;
    Ok(())
}

async fn checkpoint_target(conn: &Connection, target_path: &Path) -> AnyhowResult<()> {
    conn.execute("PRAGMA synchronous = FULL", ()).await?;
    let mut rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;
    while rows.next().await?.is_some() {}
    conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(target_path)?
        .sync_all()?;
    Ok(())
}

async fn compare_regular_file_contents(
    source: &Connection,
    target: &Connection,
) -> AnyhowResult<()> {
    let source_chunk_size = read_config_usize(source, "chunk_size", 4096).await?;
    let target_chunk_size = read_config_usize(target, "chunk_size", DEFAULT_CHUNK_SIZE).await?;
    let mut rows = source
        .query("SELECT ino, mode, size FROM fs_inode ORDER BY ino", ())
        .await?;

    while let Some(row) = rows.next().await? {
        let ino = row_i64(&row, 0)?;
        let mode = row_i64(&row, 1)?;
        let size = row_i64(&row, 2)? as usize;
        if (mode & S_IFMT) != S_IFREG {
            continue;
        }

        let source_hash =
            hash_regular_file_contents(source, ino, size, source_chunk_size, false).await?;
        let target_hash =
            hash_regular_file_contents(target, ino, size, target_chunk_size, true).await?;
        if source_hash != target_hash {
            anyhow::bail!("Regular file content mismatch for inode {ino}");
        }
    }
    Ok(())
}

async fn hash_regular_file_contents(
    conn: &Connection,
    ino: i64,
    size: usize,
    chunk_size: usize,
    allow_inline: bool,
) -> AnyhowResult<u64> {
    let mut hasher = DefaultHasher::new();

    if allow_inline {
        let mut inode_rows = conn
            .query(
                "SELECT storage_kind, data_inline FROM fs_inode WHERE ino = ?",
                (ino,),
            )
            .await?;
        let row = inode_rows
            .next()
            .await?
            .with_context(|| format!("Missing target inode {ino}"))?;
        if row_i64(&row, 0)? == 1 {
            let inline = match row.get_value(1)? {
                Value::Blob(data) => data.clone(),
                Value::Null => Vec::new(),
                _ => Vec::new(),
            };
            let copy_len = std::cmp::min(inline.len(), size);
            hasher.write(&inline[..copy_len]);
            hash_zero_bytes(&mut hasher, size - copy_len);
            return Ok(hasher.finish());
        }
    }

    let mut rows = conn
        .query(
            "SELECT chunk_index, data FROM fs_data WHERE ino = ? ORDER BY chunk_index",
            (ino,),
        )
        .await?;
    let mut position = 0usize;
    while let Some(row) = rows.next().await? {
        let chunk_index = row_i64(&row, 0)? as usize;
        let data = match row.get_value(1)? {
            Value::Blob(data) => data.clone(),
            _ => Vec::new(),
        };
        let chunk_start = chunk_index.saturating_mul(chunk_size);
        if chunk_start >= size {
            continue;
        }
        if chunk_start > position {
            hash_zero_bytes(&mut hasher, chunk_start - position);
        }
        let copy_len = std::cmp::min(data.len(), size - chunk_start);
        hasher.write(&data[..copy_len]);
        position = chunk_start + copy_len;
    }
    if position < size {
        hash_zero_bytes(&mut hasher, size - position);
    }
    Ok(hasher.finish())
}

fn hash_zero_bytes(hasher: &mut DefaultHasher, mut len: usize) {
    const ZEROES: [u8; 8192] = [0; 8192];
    while len > 0 {
        let chunk_len = std::cmp::min(len, ZEROES.len());
        hasher.write(&ZEROES[..chunk_len]);
        len -= chunk_len;
    }
}

#[cfg(test)]
async fn read_target_file_bytes(
    conn: &Connection,
    ino: i64,
    size: usize,
    chunk_size: usize,
) -> AnyhowResult<Vec<u8>> {
    let mut inode_rows = conn
        .query(
            "SELECT storage_kind, data_inline FROM fs_inode WHERE ino = ?",
            (ino,),
        )
        .await?;
    let row = inode_rows
        .next()
        .await?
        .with_context(|| format!("Missing target inode {ino}"))?;
    let storage_kind = row_i64(&row, 0)?;
    if storage_kind == 1 {
        let mut bytes = match row.get_value(1)? {
            Value::Blob(data) => data.clone(),
            Value::Null => Vec::new(),
            _ => Vec::new(),
        };
        bytes.truncate(size);
        return Ok(bytes);
    }

    let (bytes, _) = read_source_file_bytes(conn, ino, size, chunk_size).await?;
    Ok(bytes)
}

async fn verify_target_v0_5_config(conn: &Connection) -> AnyhowResult<()> {
    let schema_version = read_config_string(conn, schema::CONFIG_SCHEMA_VERSION_KEY).await?;
    if schema_version.as_deref() != Some(schema::CURRENT.as_str()) {
        anyhow::bail!(
            "Target {} is not {}",
            schema::CONFIG_SCHEMA_VERSION_KEY,
            schema::CURRENT
        );
    }
    let chunk_size = read_config_usize(conn, schema::CONFIG_CHUNK_SIZE_KEY, 0).await?;
    if chunk_size != DEFAULT_CHUNK_SIZE {
        anyhow::bail!("Target chunk_size is not {DEFAULT_CHUNK_SIZE}");
    }
    let inline_threshold = read_config_usize(conn, schema::CONFIG_INLINE_THRESHOLD_KEY, 0).await?;
    if inline_threshold != DEFAULT_INLINE_THRESHOLD {
        anyhow::bail!("Target inline_threshold is not {DEFAULT_INLINE_THRESHOLD}");
    }
    Ok(())
}

async fn verify_target_v0_5_invariants(conn: &Connection) -> AnyhowResult<()> {
    let checks = [
        (
            "inline files must not have chunks",
            "SELECT i.ino
             FROM fs_inode i
             JOIN fs_data d ON d.ino = i.ino
             WHERE i.storage_kind = 1
             LIMIT 1",
        ),
        (
            "chunked files must not carry inline data",
            "SELECT ino
             FROM fs_inode
             WHERE storage_kind = 0 AND data_inline IS NOT NULL
             LIMIT 1",
        ),
        (
            "inline sizes must match blob length",
            "SELECT ino
             FROM fs_inode
             WHERE storage_kind = 1
               AND COALESCE(length(data_inline), 0) != size
             LIMIT 1",
        ),
    ];

    for (description, sql) in checks {
        let mut rows = conn.query(sql, ()).await?;
        if let Some(row) = rows.next().await? {
            let ino = row_i64(&row, 0).unwrap_or_default();
            anyhow::bail!("Target v0.5 invariant failed: {description} (ino {ino})");
        }
    }
    Ok(())
}

async fn compare_optional_table_rows(
    source: &Connection,
    target: &Connection,
    table: &str,
    columns: &[&str],
) -> AnyhowResult<()> {
    if !table_exists(source, table).await? {
        let count = table_count(target, table).await?;
        if count != 0 {
            anyhow::bail!("Target optional table {table} should be empty");
        }
        return Ok(());
    }
    compare_table_rows(source, target, table, columns).await
}

async fn compare_optional_whiteouts(source: &Connection, target: &Connection) -> AnyhowResult<()> {
    if !table_exists(source, "fs_whiteout").await? {
        let count = table_count(target, "fs_whiteout").await?;
        if count != 0 {
            anyhow::bail!("Target optional table fs_whiteout should be empty");
        }
        return Ok(());
    }

    let source_rows = select_whiteouts_for_compare(source).await?;
    let target_rows = select_whiteouts_for_compare(target).await?;
    if source_rows != target_rows {
        anyhow::bail!("Table row mismatch for fs_whiteout");
    }
    Ok(())
}

async fn select_whiteouts_for_compare(conn: &Connection) -> AnyhowResult<Vec<Vec<String>>> {
    let columns = get_table_columns(conn, "fs_whiteout").await?;
    let has_parent_path = columns.iter().any(|column| column == "parent_path");
    let sql = if has_parent_path {
        "SELECT path, parent_path, created_at FROM fs_whiteout"
    } else {
        "SELECT path, created_at FROM fs_whiteout"
    };
    let mut rows = conn.query(sql, ()).await?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().await? {
        let path = row.get::<String>(0)?;
        let (parent_path, created_at) = if has_parent_path {
            (row.get::<String>(1)?, value_compare_key(row.get_value(2)?))
        } else {
            (
                parent_path_for_path(&path),
                value_compare_key(row.get_value(1)?),
            )
        };
        result.push(vec![path, parent_path, created_at]);
    }
    result.sort();
    Ok(result)
}

async fn compare_common_table_rows(
    source: &Connection,
    target: &Connection,
    table: &str,
) -> AnyhowResult<()> {
    let source_columns = get_table_columns(source, table).await?;
    let target_columns = get_table_columns(target, table).await?;
    let target_set = target_columns.iter().cloned().collect::<HashSet<_>>();
    let columns = source_columns
        .iter()
        .filter(|column| target_set.contains(*column))
        .map(String::as_str)
        .collect::<Vec<_>>();
    compare_table_rows(source, target, table, &columns).await
}

async fn compare_table_rows(
    source: &Connection,
    target: &Connection,
    table: &str,
    columns: &[&str],
) -> AnyhowResult<()> {
    let source_rows = select_rows_for_compare(source, table, columns).await?;
    let target_rows = select_rows_for_compare(target, table, columns).await?;
    if source_rows != target_rows {
        anyhow::bail!("Table row mismatch for {table}");
    }
    Ok(())
}

async fn select_rows_for_compare(
    conn: &Connection,
    table: &str,
    columns: &[&str],
) -> AnyhowResult<Vec<Vec<String>>> {
    let select_sql = format!(
        "SELECT {} FROM {}",
        columns
            .iter()
            .map(|column| quote_identifier(column))
            .collect::<Vec<_>>()
            .join(", "),
        quote_identifier(table)
    );
    let mut rows = conn.query(&select_sql, ()).await?;
    let mut result = Vec::new();
    while let Some(row) = rows.next().await? {
        let mut values = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            values.push(value_compare_key(row.get_value(index)?));
        }
        result.push(values);
    }
    result.sort();
    Ok(result)
}

async fn run_integrity_check(conn: &Connection, label: &str) -> AnyhowResult<()> {
    let mut rows = conn.query("PRAGMA integrity_check", ()).await?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().await? {
        results.push(row.get::<String>(0)?);
    }
    if results != ["ok".to_string()] {
        anyhow::bail!("{label} integrity_check failed: {results:?}");
    }
    Ok(())
}

async fn read_config_usize(conn: &Connection, key: &str, default: usize) -> AnyhowResult<usize> {
    let Some(value) = read_config_string(conn, key).await? else {
        return Ok(default);
    };
    Ok(value.parse().unwrap_or(default))
}

async fn read_config_string(conn: &Connection, key: &str) -> AnyhowResult<Option<String>> {
    let mut rows = conn
        .query("SELECT value FROM fs_config WHERE key = ?", (key,))
        .await?;
    if let Some(row) = rows.next().await? {
        Ok(Some(row.get::<String>(0)?))
    } else {
        Ok(None)
    }
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

async fn table_count(conn: &Connection, table: &str) -> AnyhowResult<i64> {
    let sql = format!("SELECT COUNT(*) FROM {}", quote_identifier(table));
    let mut rows = conn.query(&sql, ()).await?;
    let row = rows.next().await?.context("COUNT(*) returned no rows")?;
    row_i64(&row, 0)
}

async fn get_table_columns(conn: &Connection, table: &str) -> AnyhowResult<Vec<String>> {
    let sql = format!("PRAGMA table_info({})", quote_identifier(table));
    let mut rows = conn.query(&sql, ()).await?;
    let mut columns = Vec::new();
    while let Some(row) = rows.next().await? {
        columns.push(row.get::<String>(1)?);
    }
    Ok(columns)
}

fn row_i64(row: &turso::Row, index: usize) -> AnyhowResult<i64> {
    row.get_value(index)?
        .as_integer()
        .copied()
        .with_context(|| format!("Expected integer at column {index}"))
}

fn value_compare_key(value: Value) -> String {
    match value {
        Value::Null => "0:NULL".to_string(),
        Value::Integer(value) => format!("1:{value:020}"),
        Value::Real(value) => format!("2:{value:?}"),
        Value::Text(value) => format!("3:{value}"),
        Value::Blob(value) => format!("4:{}", bytes_to_hex(&value)),
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn parent_path_for_path(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }

    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(index) => trimmed[..index].to_string(),
    }
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
fn hash_file(path: &Path) -> AnyhowResult<u64> {
    hash_paths([path.to_path_buf()])
}

fn hash_db_family(path: &Path) -> AnyhowResult<u64> {
    hash_paths([
        path.to_path_buf(),
        sidecar_path(path, "-wal"),
        sidecar_path(path, "-shm"),
    ])
}

fn hash_paths(paths: impl IntoIterator<Item = PathBuf>) -> AnyhowResult<u64> {
    let mut hasher = DefaultHasher::new();
    for path in paths {
        path.display().to_string().hash(&mut hasher);
        match fs::metadata(&path) {
            Ok(metadata) => {
                true.hash(&mut hasher);
                metadata.len().hash(&mut hasher);
                hash_file_into(&path, &mut hasher)?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                false.hash(&mut hasher);
            }
            Err(err) => {
                return Err(err).with_context(|| format!("Failed to stat {}", path.display()));
            }
        }
    }
    Ok(hasher.finish())
}

fn hash_file_into(path: &Path, hasher: &mut DefaultHasher) -> AnyhowResult<()> {
    let mut file = fs::File::open(path)?;
    let mut buffer = [0_u8; 8192];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.write(&buffer[..bytes_read]);
    }
    Ok(())
}

fn remove_db_family(path: &Path) -> AnyhowResult<()> {
    for candidate in [
        path.to_path_buf(),
        sidecar_path(path, "-wal"),
        sidecar_path(path, "-shm"),
    ] {
        match fs::remove_file(&candidate) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("Failed to remove {}", candidate.display()))
            }
        }
    }
    Ok(())
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;
    use turso::Builder;

    async fn create_test_db_v0_0() -> (turso::Database, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let db = Builder::new_local(path).build().await.unwrap();
        let conn = db.connect().unwrap();

        // Create v0.0 schema (without nlink, nsec columns, or rdev)
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
        .await
        .unwrap();

        conn.execute(
            "CREATE TABLE fs_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await
        .unwrap();

        (db, file)
    }

    async fn create_test_db_v0_2() -> (turso::Database, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let db = Builder::new_local(path).build().await.unwrap();
        let conn = db.connect().unwrap();

        // Create v0.2 schema (with nlink, but without nsec columns or rdev)
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
                ctime INTEGER NOT NULL
            )",
            (),
        )
        .await
        .unwrap();

        conn.execute(
            "CREATE TABLE fs_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await
        .unwrap();

        (db, file)
    }

    async fn create_test_db_v0_4() -> (turso::Database, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let db = Builder::new_local(path).build().await.unwrap();
        let conn = db.connect().unwrap();

        // Create v0.4 schema (with nlink, nsec columns, and rdev)
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
                ctime_nsec INTEGER NOT NULL DEFAULT 0
            )",
            (),
        )
        .await
        .unwrap();

        conn.execute(
            "CREATE TABLE fs_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await
        .unwrap();

        (db, file)
    }

    async fn detect_schema_version_for_test(
        conn: &turso::Connection,
    ) -> AnyhowResult<SchemaVersion> {
        Ok(schema::detect_schema_version(conn)
            .await?
            .unwrap_or(SchemaVersion::V0_0))
    }

    #[tokio::test]
    async fn test_detect_schema_version_v0_0() {
        let (db, _file) = create_test_db_v0_0().await;
        let conn = db.connect().unwrap();

        let version = detect_schema_version_for_test(&conn).await.unwrap();
        assert_eq!(version, SchemaVersion::V0_0);
    }

    #[tokio::test]
    async fn test_detect_schema_version_v0_2() {
        let (db, _file) = create_test_db_v0_2().await;
        let conn = db.connect().unwrap();

        let version = detect_schema_version_for_test(&conn).await.unwrap();
        assert_eq!(version, SchemaVersion::V0_2);
    }

    #[tokio::test]
    async fn test_detect_schema_version_v0_4() {
        let (db, _file) = create_test_db_v0_4().await;
        let conn = db.connect().unwrap();

        let version = detect_schema_version_for_test(&conn).await.unwrap();
        assert_eq!(version, SchemaVersion::V0_4);
    }

    #[tokio::test]
    async fn test_migrate_v0_0_to_current() {
        let (db, _file) = create_test_db_v0_0().await;
        let conn = db.connect().unwrap();

        // Verify starting at v0.0
        assert_eq!(
            detect_schema_version_for_test(&conn).await.unwrap(),
            SchemaVersion::V0_0
        );

        schema::ensure_current(&conn).await.unwrap();

        assert_eq!(
            detect_schema_version_for_test(&conn).await.unwrap(),
            schema::CURRENT
        );
    }

    #[tokio::test]
    async fn test_migrate_v0_2_to_current() {
        let (db, _file) = create_test_db_v0_2().await;
        let conn = db.connect().unwrap();

        // Verify starting at v0.2
        assert_eq!(
            detect_schema_version_for_test(&conn).await.unwrap(),
            SchemaVersion::V0_2
        );

        schema::ensure_current(&conn).await.unwrap();

        assert_eq!(
            detect_schema_version_for_test(&conn).await.unwrap(),
            schema::CURRENT
        );
    }

    #[tokio::test]
    async fn test_migrations_are_idempotent() {
        let (db, _file) = create_test_db_v0_0().await;
        let conn = db.connect().unwrap();

        schema::ensure_current(&conn).await.unwrap();
        schema::ensure_current(&conn).await.unwrap();

        assert_eq!(
            detect_schema_version_for_test(&conn).await.unwrap(),
            schema::CURRENT
        );
    }

    #[tokio::test]
    async fn test_copy_migrate_v0_4_to_v0_5_preserves_source_and_rechunks() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source.db");
        let target = temp_dir.path().join("target.db");
        let small_content = b"inline payload".to_vec();
        let large_content = patterned_bytes(DEFAULT_CHUNK_SIZE + 123, 0x42);
        let sparse_tail = b"tail!".to_vec();

        create_synthetic_v0_4_database(&source, &small_content, &large_content, &sparse_tail).await;
        let source_hash_before = hash_file(&source).unwrap();
        let source_bytes_before = fs::read(&source).unwrap();

        let mut stdout = Vec::new();
        handle_migrate_copy_command(
            &mut stdout,
            source.to_string_lossy().into_owned(),
            target.clone(),
            true,
            false,
            None,
        )
        .await
        .unwrap();

        assert_eq!(hash_file(&source).unwrap(), source_hash_before);
        assert_eq!(fs::read(&source).unwrap(), source_bytes_before);

        let db = Builder::new_local(target.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        verify_target_v0_5_config(&conn).await.unwrap();
        verify_target_v0_5_invariants(&conn).await.unwrap();

        let mut rows = conn
            .query(
                "SELECT storage_kind, data_inline FROM fs_inode WHERE ino = 3",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row_i64(&row, 0).unwrap(), 1);
        assert_eq!(row.get_value(1).unwrap(), Value::Blob(small_content));
        assert_eq!(table_count_for_test(&conn, "fs_data", "ino = 3").await, 0);

        let mut rows = conn
            .query(
                "SELECT storage_kind, data_inline FROM fs_inode WHERE ino = 4",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row_i64(&row, 0).unwrap(), 0);
        assert!(matches!(row.get_value(1).unwrap(), Value::Null));
        assert_eq!(table_count_for_test(&conn, "fs_data", "ino = 4").await, 2);

        let migrated_large =
            read_target_file_bytes(&conn, 4, large_content.len(), DEFAULT_CHUNK_SIZE)
                .await
                .unwrap();
        assert_eq!(migrated_large, large_content);

        let sparse_size = 2 * 4096 + sparse_tail.len();
        let migrated_sparse = read_target_file_bytes(&conn, 5, sparse_size, DEFAULT_CHUNK_SIZE)
            .await
            .unwrap();
        let mut expected_sparse = vec![0; 2 * 4096];
        expected_sparse.extend_from_slice(&sparse_tail);
        assert_eq!(migrated_sparse, expected_sparse);
        assert_eq!(
            table_count_for_test(&conn, "fs_whiteout", "path = '/dir/deleted'").await,
            1
        );
        assert_eq!(
            table_count_for_test(&conn, "fs_origin", "delta_ino = 4").await,
            1
        );
        assert_eq!(
            table_count_for_test(&conn, "fs_overlay_config", "key = 'base_path'").await,
            1
        );
        assert_eq!(
            table_count_for_test(&conn, "kv_store", "key = 'metadata'").await,
            1
        );
        assert_eq!(
            table_count_for_test(&conn, "tool_calls", "name = 'migrate-test'").await,
            1
        );
    }

    #[tokio::test]
    async fn test_copy_migrate_synthesizes_legacy_whiteout_parent_path() {
        let source_file = NamedTempFile::new().unwrap();
        let target_file = NamedTempFile::new().unwrap();
        let source_db = Builder::new_local(source_file.path().to_str().unwrap())
            .build()
            .await
            .unwrap();
        let source_conn = source_db.connect().unwrap();
        source_conn
            .execute(
                "CREATE TABLE fs_whiteout (
                    path TEXT PRIMARY KEY,
                    created_at INTEGER NOT NULL
                )",
                (),
            )
            .await
            .unwrap();
        source_conn
            .execute(
                "INSERT INTO fs_whiteout (path, created_at) VALUES ('/dir/deleted', 123)",
                (),
            )
            .await
            .unwrap();

        let target_db = Builder::new_local(target_file.path().to_str().unwrap())
            .build()
            .await
            .unwrap();
        let target_conn = target_db.connect().unwrap();
        create_current_schema(&target_conn).await.unwrap();
        copy_optional_whiteouts(&source_conn, &target_conn)
            .await
            .unwrap();

        let mut rows = target_conn
            .query(
                "SELECT parent_path, created_at FROM fs_whiteout WHERE path = '/dir/deleted'",
                (),
            )
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<String>(0).unwrap(), "/dir");
        assert_eq!(row_i64(&row, 1).unwrap(), 123);
        compare_optional_whiteouts(&source_conn, &target_conn)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_in_place_migrate_backfills_legacy_whiteout_parent_path() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("legacy-whiteout.db");
        create_synthetic_v0_4_database(&db_path, b"small", b"larger than inline", b"tail").await;

        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        rewrite_whiteouts_without_parent_path(&conn).await;
        drop(conn);
        drop(db);

        let mut stdout = Vec::new();
        handle_migrate_command(
            &mut stdout,
            db_path.to_string_lossy().into_owned(),
            false,
            None,
        )
        .await
        .unwrap();
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(
            stdout.contains("Migration completed successfully."),
            "unexpected migrate output: {stdout}"
        );

        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let columns = get_table_columns(&conn, "fs_whiteout").await.unwrap();
        assert!(
            columns.iter().any(|column| column == "parent_path"),
            "cli migrate did not add fs_whiteout.parent_path; columns={columns:?}"
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
        let created_at = row_i64(&row, 1).unwrap();
        println!(
            "cli migrate: fs_whiteout columns={columns:?}; /dir/deleted parent_path={parent_path}"
        );
        assert_eq!(parent_path, "/dir");
        assert_eq!(created_at, 123);

        let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row_i64(&row, 0).unwrap(), schema::CURRENT.user_version());
    }

    async fn rewrite_whiteouts_without_parent_path(conn: &Connection) {
        conn.execute(
            "CREATE TABLE fs_whiteout_legacy_rows (
                path TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_whiteout_legacy_rows (path, created_at)
             SELECT path, created_at FROM fs_whiteout",
            (),
        )
        .await
        .unwrap();
        conn.execute("DROP TABLE fs_whiteout", ()).await.unwrap();
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
            "INSERT INTO fs_whiteout (path, created_at)
             SELECT path, created_at FROM fs_whiteout_legacy_rows",
            (),
        )
        .await
        .unwrap();
        conn.execute("DROP TABLE fs_whiteout_legacy_rows", ())
            .await
            .unwrap();
    }

    async fn create_synthetic_v0_4_database(
        path: &Path,
        small_content: &[u8],
        large_content: &[u8],
        sparse_tail: &[u8],
    ) {
        let db = Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();

        conn.execute(
            "CREATE TABLE fs_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_config (key, value) VALUES
             ('schema_version', '0.4'),
             ('chunk_size', '4096'),
             ('custom_metadata', 'preserve-me')",
            (),
        )
        .await
        .unwrap();
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
                ctime_nsec INTEGER NOT NULL DEFAULT 0
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
            "CREATE INDEX idx_fs_dentry_parent ON fs_dentry(parent_ino, name)",
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
                parent_path TEXT NOT NULL,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE INDEX idx_fs_whiteout_parent ON fs_whiteout(parent_path)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "CREATE TABLE fs_origin (
                delta_ino INTEGER PRIMARY KEY,
                base_ino INTEGER NOT NULL
            )",
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
            "CREATE INDEX idx_kv_store_created_at ON kv_store(created_at)",
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
        conn.execute("CREATE INDEX idx_tool_calls_name ON tool_calls(name)", ())
            .await
            .unwrap();
        conn.execute(
            "CREATE INDEX idx_tool_calls_started_at ON tool_calls(started_at)",
            (),
        )
        .await
        .unwrap();

        let dir_mode = 0o040000 | 0o755;
        let file_mode = 0o100000 | 0o644;
        let symlink_mode = 0o120000 | 0o777;
        conn.execute(
            "INSERT INTO fs_inode
             (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev, atime_nsec, mtime_nsec, ctime_nsec)
             VALUES
             (1, ?, 2, 1000, 1000, 0, 10, 10, 10, 0, 1, 1, 1),
             (2, ?, 2, 1000, 1000, 0, 11, 11, 11, 0, 2, 2, 2),
             (3, ?, 1, 1000, 1000, ?, 12, 12, 12, 0, 3, 3, 3),
             (4, ?, 2, 1000, 1000, ?, 13, 13, 13, 0, 4, 4, 4),
             (5, ?, 1, 1000, 1000, ?, 14, 14, 14, 0, 5, 5, 5),
             (6, ?, 1, 1000, 1000, 9, 15, 15, 15, 0, 6, 6, 6)",
            (
                dir_mode,
                dir_mode,
                file_mode,
                small_content.len() as i64,
                file_mode,
                large_content.len() as i64,
                file_mode,
                (2 * 4096 + sparse_tail.len()) as i64,
                symlink_mode,
            ),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_dentry (id, name, parent_ino, ino) VALUES
             (1, 'dir', 1, 2),
             (2, 'small.txt', 2, 3),
             (3, 'large.bin', 2, 4),
             (4, 'large-hardlink.bin', 2, 4),
             (5, 'sparse.bin', 2, 5),
             (6, 'small-link', 2, 6)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_symlink (ino, target) VALUES (6, 'small.txt')",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (3, 0, ?)",
            (Value::Blob(small_content.to_vec()),),
        )
        .await
        .unwrap();
        for (chunk_index, chunk) in large_content.chunks(4096).enumerate() {
            conn.execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (4, ?, ?)",
                (chunk_index as i64, Value::Blob(chunk.to_vec())),
            )
            .await
            .unwrap();
        }
        conn.execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (5, 2, ?)",
            (Value::Blob(sparse_tail.to_vec()),),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_whiteout (path, parent_path, created_at)
             VALUES ('/dir/deleted', '/dir', 123)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_origin (delta_ino, base_ino) VALUES (4, 44)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_overlay_config (key, value) VALUES ('base_path', '/tmp/base')",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO kv_store (key, value, created_at, updated_at)
             VALUES ('metadata', '{\"ok\":true}', 20, 21)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO tool_calls
             (id, name, parameters, result, error, status, started_at, completed_at, duration_ms)
             VALUES (1, 'migrate-test', '{\"input\":1}', '{\"ok\":true}', '', 'success', 30, 31, 1000)",
            (),
        )
        .await
        .unwrap();
    }

    #[test]
    fn schema_upgrade_guidance_names_a_command_that_finishes_the_job() {
        for (found, id_or_path) in [
            ("0.0", "my-agent"),
            ("0.2", "/tmp/old.db"),
            ("0.4", "other-agent"),
        ] {
            let guidance = schema_upgrade_guidance(found, "0.5", id_or_path);
            assert!(
                guidance.contains(&format!("agentfs migrate {id_or_path}")),
                "{found}: {guidance}"
            );
            assert!(!guidance.contains("migrate-v0-5"), "{found}: {guidance}");
        }

        let future = schema_upgrade_guidance("user_version 7", "0.5", "my-agent");
        assert!(
            !future.contains("agentfs migrate"),
            "future versions must not promise migrate can fix them: {future}"
        );
        assert!(future.contains("newer agentfs"), "{future}");
    }

    #[tokio::test]
    async fn test_copy_migrate_v0_0_source_lands_current_with_nlink_backfill() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source-v00.db");
        let target = temp_dir.path().join("target.db");
        let small = b"tiny".to_vec();
        let large = patterned_bytes(DEFAULT_CHUNK_SIZE + 77, 0x21);
        create_synthetic_legacy_database(&source, SchemaVersion::V0_0, &small, &large).await;

        let mut stdout = Vec::new();
        handle_migrate_copy_command(
            &mut stdout,
            source.to_string_lossy().into_owned(),
            target.clone(),
            true,
            false,
            None,
        )
        .await
        .unwrap();
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("Source schema version: 0.0"), "{stdout}");
        assert!(stdout.contains("Verification completed successfully."));

        let db = Builder::new_local(target.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row_i64(&row, 0).unwrap(), schema::CURRENT.user_version());

        for (ino, expected_nlink) in [(1, 2), (2, 1), (3, 1), (4, 2)] {
            let mut rows = conn
                .query("SELECT nlink FROM fs_inode WHERE ino = ?", (ino,))
                .await
                .unwrap();
            let row = rows.next().await.unwrap().unwrap();
            assert_eq!(
                row_i64(&row, 0).unwrap(),
                expected_nlink,
                "nlink for ino {ino}"
            );
        }

        let migrated_small = read_target_file_bytes(&conn, 3, small.len(), DEFAULT_CHUNK_SIZE)
            .await
            .unwrap();
        assert_eq!(migrated_small, small);
        let migrated_large = read_target_file_bytes(&conn, 4, large.len(), DEFAULT_CHUNK_SIZE)
            .await
            .unwrap();
        assert_eq!(migrated_large, large);
    }

    #[tokio::test]
    async fn test_copy_migrate_v0_2_source_lands_current() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("source-v02.db");
        let target = temp_dir.path().join("target.db");
        let small = b"inline me".to_vec();
        let large = patterned_bytes(2 * DEFAULT_CHUNK_SIZE + 5, 0x55);
        create_synthetic_legacy_database(&source, SchemaVersion::V0_2, &small, &large).await;

        let mut stdout = Vec::new();
        handle_migrate_copy_command(
            &mut stdout,
            source.to_string_lossy().into_owned(),
            target.clone(),
            true,
            false,
            None,
        )
        .await
        .unwrap();
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("Source schema version: 0.2"), "{stdout}");

        let db = Builder::new_local(target.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row_i64(&row, 0).unwrap(), schema::CURRENT.user_version());
        let migrated_large = read_target_file_bytes(&conn, 4, large.len(), DEFAULT_CHUNK_SIZE)
            .await
            .unwrap();
        assert_eq!(migrated_large, large);
    }

    #[tokio::test]
    async fn test_copy_migrate_rejects_current_source() {
        let temp_dir = tempfile::tempdir().unwrap();
        let source = temp_dir.path().join("current.db");
        let target = temp_dir.path().join("target.db");
        {
            let db = Builder::new_local(source.to_str().unwrap())
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            schema::ensure_current(&conn).await.unwrap();
        }

        let mut stdout = Vec::new();
        let err = handle_migrate_copy_command(
            &mut stdout,
            source.to_string_lossy().into_owned(),
            target.clone(),
            false,
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already at schema"), "{err}");
    }

    #[tokio::test]
    async fn test_in_place_migrate_read_path_leaves_single_file_family() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("current.db");
        {
            let agent = agentfs_core::AgentFS::open(agentfs_core::AgentFSOptions::with_path(
                db_path.to_string_lossy(),
            ))
            .await
            .unwrap();
            agent.fs.finalize().await.unwrap();
        }
        let wal = PathBuf::from(format!("{}-wal", db_path.display()));
        let shm = PathBuf::from(format!("{}-shm", db_path.display()));
        assert!(!wal.exists(), "fixture must start as a single file");

        let mut stdout = Vec::new();
        handle_migrate_command(
            &mut stdout,
            db_path.to_string_lossy().into_owned(),
            false,
            None,
        )
        .await
        .unwrap();
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("already at schema"), "{stdout}");
        assert!(
            !wal.exists(),
            "already-current migrate must not leave a WAL sidecar it created"
        );
        assert!(
            !shm.exists(),
            "already-current migrate must not leave an SHM sidecar it created"
        );
    }

    #[tokio::test]
    async fn test_failing_migrate_removes_frameless_sidecars_it_created() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("future.db");
        {
            let db = Builder::new_local(db_path.to_str().unwrap())
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            // A user_version no SchemaVersion maps to makes detect_schema_version
            // (and thus the migrate) fail after the open already succeeded.
            conn.execute("PRAGMA user_version = 999", ()).await.unwrap();
            let mut rows = conn
                .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
                .await
                .unwrap();
            while rows.next().await.unwrap().is_some() {}
        }
        let wal = PathBuf::from(format!("{}-wal", db_path.display()));
        let shm = PathBuf::from(format!("{}-shm", db_path.display()));
        let _ = std::fs::remove_file(&wal);
        let _ = std::fs::remove_file(&shm);
        assert!(!wal.exists(), "fixture must start as a single file");

        let mut stdout = Vec::new();
        let err = handle_migrate_command(
            &mut stdout,
            db_path.to_string_lossy().into_owned(),
            false,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("user_version 999"),
            "migrate must fail on the unsupported version, got: {err:#}"
        );
        assert!(
            !wal.exists(),
            "failing migrate must not leave the frameless WAL sidecar its own open created"
        );
        assert!(
            !shm.exists(),
            "failing migrate must not leave the SHM sidecar its own open created"
        );
    }

    #[tokio::test]
    async fn test_in_place_migrate_encrypted_v0_4_with_key() {
        let key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
        let cipher = "aes256gcm";
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("encrypted-v04.db");

        {
            let db = Builder::new_local(db_path.to_str().unwrap())
                .experimental_encryption(true)
                .with_encryption(turso::EncryptionOpts {
                    cipher: cipher.to_string(),
                    hexkey: key.to_string(),
                })
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            create_synthetic_v0_4_fixture_tables(&conn).await;
        }

        let encryption = (key.to_string(), cipher.to_string());
        let mut stdout = Vec::new();
        handle_migrate_command(
            &mut stdout,
            db_path.to_string_lossy().into_owned(),
            false,
            Some(&encryption),
        )
        .await
        .unwrap();
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("Current schema version: 0.4"), "{stdout}");
        assert!(
            stdout.contains("Migration completed successfully."),
            "{stdout}"
        );

        // No plaintext copy may appear next to the fixture during migration.
        let names = fs::read_dir(temp_dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with("encrypted-v04.db"))
            .collect::<Vec<_>>();
        assert!(names.is_empty(), "unexpected files: {names:?}");

        let db = build_local_database(&db_path, Some(&encryption))
            .await
            .unwrap();
        let conn = db.connect().unwrap();
        let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row_i64(&row, 0).unwrap(), schema::CURRENT.user_version());
        assert_eq!(
            table_count_for_test(&conn, "fs_data", "ino = 2").await,
            1,
            "file data must survive the encrypted migration"
        );
        drop(conn);
        drop(db);

        let without_key = async {
            let db = build_local_database(&db_path, None).await?;
            let conn = db.connect()?;
            let mut rows = conn.query("SELECT COUNT(*) FROM fs_inode", ()).await?;
            rows.next().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        assert!(
            without_key.is_err(),
            "encrypted database must not open without the key"
        );
    }

    async fn create_synthetic_v0_4_fixture_tables(conn: &Connection) {
        conn.execute(
            "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_config (key, value) VALUES
             ('schema_version', '0.4'),
             ('chunk_size', '4096')",
            (),
        )
        .await
        .unwrap();
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
                ctime_nsec INTEGER NOT NULL DEFAULT 0
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
        let dir_mode = 0o040000 | 0o755;
        let file_mode = 0o100000 | 0o644;
        conn.execute(
            "INSERT INTO fs_inode
             (ino, mode, nlink, uid, gid, size, atime, mtime, ctime, rdev,
              atime_nsec, mtime_nsec, ctime_nsec)
             VALUES
             (1, ?, 2, 1000, 1000, 0, 10, 10, 10, 0, 0, 0, 0),
             (2, ?, 1, 1000, 1000, 6, 11, 11, 11, 0, 0, 0, 0)",
            (dir_mode, file_mode),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_dentry (id, name, parent_ino, ino) VALUES (1, 'file.txt', 1, 2)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (2, 0, ?)",
            (Value::Blob(b"secret".to_vec()),),
        )
        .await
        .unwrap();
    }

    async fn create_synthetic_legacy_database(
        path: &Path,
        version: SchemaVersion,
        small_content: &[u8],
        large_content: &[u8],
    ) {
        assert!(matches!(version, SchemaVersion::V0_0 | SchemaVersion::V0_2));
        let db = Builder::new_local(path.to_str().unwrap())
            .build()
            .await
            .unwrap();
        let conn = db.connect().unwrap();

        conn.execute(
            "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_config (key, value) VALUES
             ('schema_version', ?),
             ('chunk_size', '4096'),
             ('custom_metadata', 'preserve-me')",
            (version.as_str(),),
        )
        .await
        .unwrap();

        let nlink_column = if version >= SchemaVersion::V0_2 {
            "nlink INTEGER NOT NULL DEFAULT 0,"
        } else {
            ""
        };
        conn.execute(
            &format!(
                "CREATE TABLE fs_inode (
                    ino INTEGER PRIMARY KEY AUTOINCREMENT,
                    mode INTEGER NOT NULL,
                    {nlink_column}
                    uid INTEGER NOT NULL DEFAULT 0,
                    gid INTEGER NOT NULL DEFAULT 0,
                    size INTEGER NOT NULL DEFAULT 0,
                    atime INTEGER NOT NULL,
                    mtime INTEGER NOT NULL,
                    ctime INTEGER NOT NULL
                )"
            ),
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
            "CREATE TABLE fs_symlink (ino INTEGER PRIMARY KEY, target TEXT NOT NULL)",
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

        let dir_mode = 0o040000 | 0o755;
        let file_mode = 0o100000 | 0o644;
        let (columns, nlink_values): (&str, Vec<i64>) = if version >= SchemaVersion::V0_2 {
            (
                "ino, mode, nlink, uid, gid, size, atime, mtime, ctime",
                vec![2, 1, 1, 2],
            )
        } else {
            ("ino, mode, uid, gid, size, atime, mtime, ctime", Vec::new())
        };
        for (index, (ino, mode, size)) in [
            (1_i64, dir_mode, 0_i64),
            (2, dir_mode, 0),
            (3, file_mode, small_content.len() as i64),
            (4, file_mode, large_content.len() as i64),
        ]
        .into_iter()
        .enumerate()
        {
            let mut values = vec![Value::Integer(ino), Value::Integer(mode)];
            if version >= SchemaVersion::V0_2 {
                values.push(Value::Integer(nlink_values[index]));
            }
            values.extend([
                Value::Integer(1000),
                Value::Integer(1000),
                Value::Integer(size),
                Value::Integer(10),
                Value::Integer(10),
                Value::Integer(10),
            ]);
            let placeholders = std::iter::repeat_n("?", values.len())
                .collect::<Vec<_>>()
                .join(", ");
            conn.execute(
                &format!("INSERT INTO fs_inode ({columns}) VALUES ({placeholders})"),
                values,
            )
            .await
            .unwrap();
        }
        conn.execute(
            "INSERT INTO fs_dentry (id, name, parent_ino, ino) VALUES
             (1, 'dir', 1, 2),
             (2, 'small.txt', 2, 3),
             (3, 'large.bin', 2, 4),
             (4, 'large-hardlink.bin', 2, 4)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (3, 0, ?)",
            (Value::Blob(small_content.to_vec()),),
        )
        .await
        .unwrap();
        for (chunk_index, chunk) in large_content.chunks(4096).enumerate() {
            conn.execute(
                "INSERT INTO fs_data (ino, chunk_index, data) VALUES (4, ?, ?)",
                (chunk_index as i64, Value::Blob(chunk.to_vec())),
            )
            .await
            .unwrap();
        }
        conn.execute(
            "INSERT INTO kv_store (key, value, created_at, updated_at)
             VALUES ('metadata', '{\"ok\":true}', 20, 21)",
            (),
        )
        .await
        .unwrap();
        conn.execute(
            "INSERT INTO tool_calls
             (id, name, parameters, result, error, status, started_at, completed_at, duration_ms)
             VALUES (1, 'legacy-test', '{}', '{}', '', 'success', 30, 31, 42)",
            (),
        )
        .await
        .unwrap();
    }

    async fn table_count_for_test(conn: &Connection, table: &str, where_clause: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE {where_clause}");
        let mut rows = conn.query(&sql, ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        row_i64(&row, 0).unwrap()
    }

    fn patterned_bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|index| seed.wrapping_add((index % 251) as u8))
            .collect()
    }
}
