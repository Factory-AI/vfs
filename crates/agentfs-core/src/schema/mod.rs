//! Schema authority for AgentFS databases.
//!
//! This module owns all production Rust DDL, schema-version detection, and
//! user_version keyed migrations for the pre-crate-split SDK core.

pub mod integrity;

use crate::config::{DEFAULT_CHUNK_SIZE, DEFAULT_INLINE_THRESHOLD};
use crate::error::{Error, Result};
use turso::transaction::{Transaction, TransactionBehavior};
use turso::{Connection, Value};

/// Current schema version.
pub const CURRENT: SchemaVersion = SchemaVersion::V0_5;

/// Compatibility string for callers that still surface the historical version.
pub const AGENTFS_SCHEMA_VERSION: &str = CURRENT.as_str();
pub const CONFIG_SCHEMA_VERSION_KEY: &str = "schema_version";
pub const CONFIG_CHUNK_SIZE_KEY: &str = "chunk_size";
pub const CONFIG_INLINE_THRESHOLD_KEY: &str = "inline_threshold";

/// Detected schema version based on PRAGMA user_version, with fs_config and
/// column-sniffing compatibility for pre-user_version databases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchemaVersion {
    /// Base schema: fs_inode, fs_dentry, fs_data, fs_symlink, fs_config, kv_store, tool_calls
    V0_0,
    /// Added nlink column to fs_inode
    V0_2,
    /// Added atime_nsec, mtime_nsec, ctime_nsec, rdev columns to fs_inode
    V0_4,
    /// Added inline small-file storage columns and overlay sidecar tables
    V0_5,
}

impl std::fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl SchemaVersion {
    /// Returns the version string.
    pub const fn as_str(self) -> &'static str {
        match self {
            SchemaVersion::V0_0 => "0.0",
            SchemaVersion::V0_2 => "0.2",
            SchemaVersion::V0_4 => "0.4",
            SchemaVersion::V0_5 => "0.5",
        }
    }

    /// Returns the PRAGMA user_version value for this schema.
    pub const fn user_version(self) -> i64 {
        match self {
            SchemaVersion::V0_0 => 0,
            SchemaVersion::V0_2 => 2,
            SchemaVersion::V0_4 => 4,
            SchemaVersion::V0_5 => 5,
        }
    }

    /// Returns true if this version is the current version.
    pub const fn is_current(self) -> bool {
        matches!(self, CURRENT)
    }

    fn from_user_version(version: i64) -> Option<Self> {
        match version {
            0 => Some(SchemaVersion::V0_0),
            2 => Some(SchemaVersion::V0_2),
            4 => Some(SchemaVersion::V0_4),
            5 => Some(SchemaVersion::V0_5),
            _ => None,
        }
    }
}

/// A single ordered migration keyed by SQLite PRAGMA user_version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    pub from: SchemaVersion,
    pub to: SchemaVersion,
    pub description: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        from: SchemaVersion::V0_0,
        to: SchemaVersion::V0_2,
        description: "add fs_inode.nlink",
    },
    Migration {
        from: SchemaVersion::V0_2,
        to: SchemaVersion::V0_4,
        description: "add nanosecond timestamps and rdev",
    },
    Migration {
        from: SchemaVersion::V0_4,
        to: SchemaVersion::V0_5,
        description: "add inline storage and overlay schema sections",
    },
];

/// Ordered migrations to the current schema.
pub fn migrations() -> &'static [Migration] {
    MIGRATIONS
}

/// Migrations that would be applied from `from` to [`CURRENT`].
pub fn pending_migrations(from: SchemaVersion) -> Vec<&'static Migration> {
    let mut version = from;
    let mut pending = Vec::new();
    while version != CURRENT {
        let Some(migration) = MIGRATIONS
            .iter()
            .find(|migration| migration.from == version)
        else {
            break;
        };
        pending.push(migration);
        version = migration.to;
    }
    pending
}

/// Single production DDL source.
pub mod ddl {
    use super::SchemaVersion;

    /// Returns all DDL statements needed for the requested schema version.
    pub fn create_all(_version: SchemaVersion) -> &'static [&'static str] {
        CURRENT_DDL
    }

    const CURRENT_DDL: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS fs_config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        "CREATE TABLE IF NOT EXISTS fs_inode (
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
        "CREATE TABLE IF NOT EXISTS fs_dentry (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            parent_ino INTEGER NOT NULL,
            ino INTEGER NOT NULL,
            UNIQUE(parent_ino, name)
        )",
        "CREATE INDEX IF NOT EXISTS idx_fs_dentry_parent ON fs_dentry(parent_ino, name)",
        "CREATE TABLE IF NOT EXISTS fs_data (
            ino INTEGER NOT NULL,
            chunk_index INTEGER NOT NULL,
            data BLOB NOT NULL,
            PRIMARY KEY (ino, chunk_index)
        )",
        "CREATE TABLE IF NOT EXISTS fs_symlink (
            ino INTEGER PRIMARY KEY,
            target TEXT NOT NULL
        )",
        "CREATE TABLE IF NOT EXISTS fs_whiteout (
            path TEXT PRIMARY KEY,
            parent_path TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
        "CREATE INDEX IF NOT EXISTS idx_fs_whiteout_parent ON fs_whiteout(parent_path)",
        "CREATE TABLE IF NOT EXISTS fs_overlay_config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        "CREATE TABLE IF NOT EXISTS fs_origin (
            delta_ino INTEGER PRIMARY KEY,
            base_ino INTEGER NOT NULL
        )",
        "CREATE TABLE IF NOT EXISTS fs_partial_origin (
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
        "CREATE TABLE IF NOT EXISTS fs_chunk_override (
            delta_ino INTEGER NOT NULL,
            chunk_index INTEGER NOT NULL,
            PRIMARY KEY (delta_ino, chunk_index)
        )",
        "CREATE TABLE IF NOT EXISTS kv_store (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            created_at INTEGER DEFAULT (unixepoch()),
            updated_at INTEGER DEFAULT (unixepoch())
        )",
        "CREATE INDEX IF NOT EXISTS idx_kv_store_created_at ON kv_store(created_at)",
        "CREATE TABLE IF NOT EXISTS tool_calls (
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
        "CREATE INDEX IF NOT EXISTS idx_tool_calls_name ON tool_calls(name)",
        "CREATE INDEX IF NOT EXISTS idx_tool_calls_started_at ON tool_calls(started_at)",
    ];
}

#[derive(Debug)]
struct ColumnInfo {
    name: String,
    type_name: String,
    not_null: bool,
    default_value: Option<String>,
}

#[derive(Clone, Copy)]
struct ColumnSpec {
    table_name: &'static str,
    column_name: &'static str,
    type_name: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
}

const CURRENT_COLUMN_SPECS: &[ColumnSpec] = &[
    ColumnSpec {
        table_name: "fs_inode",
        column_name: "nlink",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
    },
    ColumnSpec {
        table_name: "fs_inode",
        column_name: "atime_nsec",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
    },
    ColumnSpec {
        table_name: "fs_inode",
        column_name: "mtime_nsec",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
    },
    ColumnSpec {
        table_name: "fs_inode",
        column_name: "ctime_nsec",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
    },
    ColumnSpec {
        table_name: "fs_inode",
        column_name: "rdev",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
    },
    ColumnSpec {
        table_name: "fs_inode",
        column_name: "data_inline",
        type_name: "BLOB",
        not_null: false,
        default_value: None,
    },
    ColumnSpec {
        table_name: "fs_inode",
        column_name: "storage_kind",
        type_name: "INTEGER",
        not_null: true,
        default_value: Some("0"),
    },
];

const REQUIRED_CURRENT_TABLES: &[&str] = &[
    "fs_config",
    "fs_inode",
    "fs_dentry",
    "fs_data",
    "fs_symlink",
    "fs_whiteout",
    "fs_overlay_config",
    "fs_origin",
    "fs_partial_origin",
    "fs_chunk_override",
    "kv_store",
    "tool_calls",
];

/// Detect the schema version of an existing database.
///
/// Returns `None` if the database has no `fs_inode` table and is therefore a
/// new database from the schema authority's perspective.
pub async fn detect_schema_version(conn: &Connection) -> Result<Option<SchemaVersion>> {
    let raw_user_version = user_version(conn).await?;
    if raw_user_version > 0 {
        let version = SchemaVersion::from_user_version(raw_user_version).ok_or_else(|| {
            Error::SchemaVersionMismatch {
                found: format!("user_version {raw_user_version}"),
                expected: CURRENT.to_string(),
            }
        })?;
        return Ok(Some(version));
    }

    if !table_exists(conn, "fs_inode").await? {
        return Ok(None);
    }

    let columns = get_table_columns(conn, "fs_inode").await?;
    let has_nlink = columns.iter().any(|c| c.name == "nlink");
    let has_atime_nsec = columns.iter().any(|c| c.name == "atime_nsec");
    let has_mtime_nsec = columns.iter().any(|c| c.name == "mtime_nsec");
    let has_ctime_nsec = columns.iter().any(|c| c.name == "ctime_nsec");
    let has_rdev = columns.iter().any(|c| c.name == "rdev");
    let has_data_inline = columns.iter().any(|c| c.name == "data_inline");
    let has_storage_kind = columns.iter().any(|c| c.name == "storage_kind");

    // Pre-user_version v0.5 databases are recognized by columns. The old
    // fs_config markers are compatibility hints, not authoritative identity.
    if has_data_inline && has_storage_kind {
        return Ok(Some(SchemaVersion::V0_5));
    }

    if has_atime_nsec && has_mtime_nsec && has_ctime_nsec && has_rdev {
        return Ok(Some(SchemaVersion::V0_4));
    }

    if has_nlink {
        return Ok(Some(SchemaVersion::V0_2));
    }

    Ok(Some(SchemaVersion::V0_0))
}

/// Check that a database has the current schema version.
///
/// This is a read-only check. Opening paths should call [`ensure_current`] so
/// pending migrations run instead of performing unversioned implicit changes.
pub async fn check_schema_version(conn: &Connection) -> Result<()> {
    if let Some(version) = detect_schema_version(conn).await? {
        if !version.is_current() {
            return Err(Error::SchemaVersionMismatch {
                found: version.to_string(),
                expected: CURRENT.to_string(),
            });
        }
        validate_current_schema(conn).await?;
    }
    Ok(())
}

/// Ensure the database is at [`CURRENT`], running all pending migrations inside
/// a single IMMEDIATE transaction and stamping `PRAGMA user_version` before the
/// DDL transaction commits.
pub async fn ensure_current(conn: &Connection) -> Result<()> {
    let raw_user_version = user_version(conn).await?;
    let detected = detect_schema_version(conn).await?;

    if raw_user_version == CURRENT.user_version() {
        validate_current_schema(conn).await?;
        return Ok(());
    }

    let txn = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).await?;
    let result = async {
        // Legacy overlay sidecar tables can predate columns that CURRENT_DDL
        // indexes. Repair and backfill those columns before running any
        // dependent CREATE INDEX statements from the single DDL list.
        ensure_overlay_compat_columns(conn).await?;
        execute_current_ddl(conn).await?;
        if let Some(version) = detected {
            apply_pending_migrations(conn, version).await?;
        }
        ensure_config_defaults(conn).await?;
        set_user_version(conn, CURRENT).await?;
        Ok(())
    }
    .await;

    match result {
        Ok(()) => txn.commit().await?,
        Err(err) => {
            let _ = txn.rollback().await;
            return Err(err);
        }
    }

    validate_current_schema(conn).await?;
    Ok(())
}

/// Set or update the overlay base-path marker without owning any DDL locally.
pub async fn set_overlay_base_path(conn: &Connection, base_path: &str) -> Result<()> {
    ensure_current(conn).await?;
    conn.execute(
        "INSERT OR REPLACE INTO fs_overlay_config (key, value) VALUES ('base_path', ?1)",
        [Value::Text(base_path.to_string())],
    )
    .await?;
    Ok(())
}

async fn apply_pending_migrations(conn: &Connection, from: SchemaVersion) -> Result<()> {
    let mut version = from;
    while version != CURRENT {
        let Some(migration) = MIGRATIONS
            .iter()
            .find(|migration| migration.from == version)
        else {
            return Err(Error::SchemaVersionMismatch {
                found: version.to_string(),
                expected: CURRENT.to_string(),
            });
        };
        apply_migration(conn, migration).await?;
        set_user_version(conn, migration.to).await?;
        version = migration.to;
    }
    Ok(())
}

async fn apply_migration(conn: &Connection, migration: &Migration) -> Result<()> {
    match (migration.from, migration.to) {
        (SchemaVersion::V0_0, SchemaVersion::V0_2) => {
            add_column_idempotent(
                conn,
                ColumnSpec {
                    table_name: "fs_inode",
                    column_name: "nlink",
                    type_name: "INTEGER",
                    not_null: true,
                    default_value: Some("0"),
                },
                "ALTER TABLE fs_inode ADD COLUMN nlink INTEGER NOT NULL DEFAULT 0",
            )
            .await?;
            conn.execute(
                "UPDATE fs_inode
                 SET nlink = CASE
                     WHEN ino = 1 THEN 2
                     WHEN (mode & 61440) = 16384 THEN MAX(1, (SELECT COUNT(*) FROM fs_dentry d WHERE d.ino = fs_inode.ino))
                     ELSE (SELECT COUNT(*) FROM fs_dentry d WHERE d.ino = fs_inode.ino)
                 END",
                (),
            )
            .await?;
            Ok(())
        }
        (SchemaVersion::V0_2, SchemaVersion::V0_4) => {
            add_column_idempotent(
                conn,
                ColumnSpec {
                    table_name: "fs_inode",
                    column_name: "atime_nsec",
                    type_name: "INTEGER",
                    not_null: true,
                    default_value: Some("0"),
                },
                "ALTER TABLE fs_inode ADD COLUMN atime_nsec INTEGER NOT NULL DEFAULT 0",
            )
            .await?;
            add_column_idempotent(
                conn,
                ColumnSpec {
                    table_name: "fs_inode",
                    column_name: "mtime_nsec",
                    type_name: "INTEGER",
                    not_null: true,
                    default_value: Some("0"),
                },
                "ALTER TABLE fs_inode ADD COLUMN mtime_nsec INTEGER NOT NULL DEFAULT 0",
            )
            .await?;
            add_column_idempotent(
                conn,
                ColumnSpec {
                    table_name: "fs_inode",
                    column_name: "ctime_nsec",
                    type_name: "INTEGER",
                    not_null: true,
                    default_value: Some("0"),
                },
                "ALTER TABLE fs_inode ADD COLUMN ctime_nsec INTEGER NOT NULL DEFAULT 0",
            )
            .await?;
            add_column_idempotent(
                conn,
                ColumnSpec {
                    table_name: "fs_inode",
                    column_name: "rdev",
                    type_name: "INTEGER",
                    not_null: true,
                    default_value: Some("0"),
                },
                "ALTER TABLE fs_inode ADD COLUMN rdev INTEGER NOT NULL DEFAULT 0",
            )
            .await
        }
        (SchemaVersion::V0_4, SchemaVersion::V0_5) => {
            add_column_idempotent(
                conn,
                ColumnSpec {
                    table_name: "fs_inode",
                    column_name: "data_inline",
                    type_name: "BLOB",
                    not_null: false,
                    default_value: None,
                },
                "ALTER TABLE fs_inode ADD COLUMN data_inline BLOB",
            )
            .await?;
            add_column_idempotent(
                conn,
                ColumnSpec {
                    table_name: "fs_inode",
                    column_name: "storage_kind",
                    type_name: "INTEGER",
                    not_null: true,
                    default_value: Some("0"),
                },
                "ALTER TABLE fs_inode ADD COLUMN storage_kind INTEGER NOT NULL DEFAULT 0",
            )
            .await
        }
        _ => Err(Error::Internal(format!(
            "unsupported schema migration {} -> {}",
            migration.from, migration.to
        ))),
    }
}

async fn execute_current_ddl(conn: &Connection) -> Result<()> {
    for sql in ddl::create_all(CURRENT) {
        conn.execute(*sql, ()).await?;
    }
    Ok(())
}

async fn ensure_overlay_compat_columns(conn: &Connection) -> Result<()> {
    if table_exists(conn, "fs_partial_origin").await? {
        add_column_if_missing(
            conn,
            "fs_partial_origin",
            "base_fingerprint_size",
            "ALTER TABLE fs_partial_origin ADD COLUMN base_fingerprint_size INTEGER NOT NULL DEFAULT -1",
        )
        .await?;
        add_column_if_missing(
            conn,
            "fs_partial_origin",
            "base_mtime",
            "ALTER TABLE fs_partial_origin ADD COLUMN base_mtime INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        add_column_if_missing(
            conn,
            "fs_partial_origin",
            "base_mtime_nsec",
            "ALTER TABLE fs_partial_origin ADD COLUMN base_mtime_nsec INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        add_column_if_missing(
            conn,
            "fs_partial_origin",
            "base_ctime",
            "ALTER TABLE fs_partial_origin ADD COLUMN base_ctime INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        add_column_if_missing(
            conn,
            "fs_partial_origin",
            "base_ctime_nsec",
            "ALTER TABLE fs_partial_origin ADD COLUMN base_ctime_nsec INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
    }

    if table_exists(conn, "fs_whiteout").await?
        && add_column_if_missing(
            conn,
            "fs_whiteout",
            "parent_path",
            "ALTER TABLE fs_whiteout ADD COLUMN parent_path TEXT NOT NULL DEFAULT '/'",
        )
        .await?
    {
        let mut rows = conn.query("SELECT path FROM fs_whiteout", ()).await?;
        let mut paths = Vec::new();
        while let Some(row) = rows.next().await? {
            let path: String = row.get(0)?;
            paths.push(path);
        }
        for path in paths {
            conn.execute(
                "UPDATE fs_whiteout SET parent_path = ? WHERE path = ?",
                (parent_path_for_whiteout(&path), path),
            )
            .await?;
        }
    }

    Ok(())
}

async fn ensure_config_defaults(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO fs_config (key, value) VALUES (?, ?)",
        (CONFIG_SCHEMA_VERSION_KEY, CURRENT.as_str()),
    )
    .await?;
    conn.execute(
        "INSERT OR IGNORE INTO fs_config (key, value) VALUES (?, ?)",
        (CONFIG_CHUNK_SIZE_KEY, DEFAULT_CHUNK_SIZE.to_string()),
    )
    .await?;
    conn.execute(
        "INSERT OR IGNORE INTO fs_config (key, value) VALUES (?, ?)",
        (
            CONFIG_INLINE_THRESHOLD_KEY,
            DEFAULT_INLINE_THRESHOLD.to_string(),
        ),
    )
    .await?;
    Ok(())
}

async fn add_column_if_missing(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
    sql: &str,
) -> Result<bool> {
    if column_exists(conn, table_name, column_name).await? {
        return Ok(false);
    }
    conn.execute(sql, ()).await?;
    Ok(true)
}

async fn column_exists(conn: &Connection, table_name: &str, column_name: &str) -> Result<bool> {
    Ok(get_table_columns(conn, table_name)
        .await?
        .iter()
        .any(|column| column.name == column_name))
}

fn parent_path_for_whiteout(path: &str) -> String {
    if path == "/" {
        return "/".to_string();
    }
    match path.rsplit_once('/') {
        Some(("", _)) | None => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
    }
}

async fn validate_current_schema(conn: &Connection) -> Result<()> {
    for table in REQUIRED_CURRENT_TABLES {
        if !table_exists(conn, table).await? {
            return Err(Error::Internal(format!(
                "current schema is missing required table {table}"
            )));
        }
    }

    for spec in CURRENT_COLUMN_SPECS {
        ensure_column_matches(conn, *spec).await?;
    }

    Ok(())
}

async fn user_version(conn: &Connection) -> Result<i64> {
    let mut rows = conn.query("PRAGMA user_version", ()).await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| Error::Internal("PRAGMA user_version returned no rows".to_string()))?;
    row.get(0).map_err(Error::from)
}

async fn set_user_version(conn: &Connection, version: SchemaVersion) -> Result<()> {
    conn.execute(
        &format!("PRAGMA user_version = {}", version.user_version()),
        (),
    )
    .await?;
    Ok(())
}

async fn table_exists(conn: &Connection, table_name: &str) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
            (table_name,),
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

async fn get_table_columns(conn: &Connection, table_name: &str) -> Result<Vec<ColumnInfo>> {
    let mut rows = conn
        .query(&format!("PRAGMA table_info({})", table_name), ())
        .await?;

    let mut columns = Vec::new();
    while let Some(row) = rows.next().await? {
        let name: String = row.get(1)?;
        let type_name: String = row.get(2)?;
        let not_null: i64 = row.get(3)?;
        let default_value = match row.get_value(4).ok() {
            Some(Value::Text(value)) => Some(value.clone()),
            Some(Value::Integer(value)) => Some(value.to_string()),
            Some(Value::Null) | None => None,
            Some(value) => Some(format!("{value:?}")),
        };
        columns.push(ColumnInfo {
            name,
            type_name,
            not_null: not_null != 0,
            default_value,
        });
    }

    Ok(columns)
}

async fn add_column_idempotent(conn: &Connection, spec: ColumnSpec, sql: &str) -> Result<()> {
    match conn.execute(sql, ()).await {
        Ok(_) => Ok(()),
        Err(err) if is_duplicate_column_error(&err) => ensure_column_matches(conn, spec).await,
        Err(err) => Err(Error::Internal(format!(
            "schema ALTER failed while adding {}.{}: {err}",
            spec.table_name, spec.column_name
        ))),
    }
}

async fn ensure_column_matches(conn: &Connection, spec: ColumnSpec) -> Result<()> {
    let columns = get_table_columns(conn, spec.table_name).await?;
    for column in columns {
        if column.name != spec.column_name {
            continue;
        }

        let type_matches = column.type_name.eq_ignore_ascii_case(spec.type_name);
        let default_matches = column.default_value.as_deref() == spec.default_value;
        if type_matches && column.not_null == spec.not_null && default_matches {
            return Ok(());
        }

        return Err(Error::Internal(format!(
            "schema column {}.{} already exists with incompatible definition: \
             expected type={} not_null={} default={:?}; \
             found type={} not_null={} default={:?}",
            spec.table_name,
            spec.column_name,
            spec.type_name,
            spec.not_null,
            spec.default_value,
            column.type_name,
            column.not_null,
            column.default_value
        )));
    }

    Err(Error::Internal(format!(
        "schema column {}.{} is missing",
        spec.table_name, spec.column_name
    )))
}

fn is_duplicate_column_error(err: &turso::Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("duplicate column")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentFS, AgentFSOptions, KvStore, ToolCalls, DEFAULT_FILE_MODE};
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;
    use turso::Builder;

    const S_IFDIR: i64 = 0o040000;
    const S_IFREG: i64 = 0o100000;

    #[tokio::test]
    async fn schema_user_version_migrations_from_all_fixtures() -> Result<()> {
        for version in [
            SchemaVersion::V0_0,
            SchemaVersion::V0_2,
            SchemaVersion::V0_4,
            SchemaVersion::V0_5,
        ] {
            let dir = tempdir()?;
            let db_path = dir.path().join(format!("fixture-{}.db", version.as_str()));
            let db = Builder::new_local(db_path.to_str().unwrap())
                .build()
                .await?;
            let conn = db.connect()?;
            create_legacy_fixture(&conn, version).await?;

            let kv_before = scalar_i64(&conn, "SELECT COUNT(*) FROM kv_store").await?;
            let tool_before = scalar_i64(&conn, "SELECT COUNT(*) FROM tool_calls").await?;
            let data_before = read_fixture_file_bytes(&conn).await?;

            ensure_current(&conn).await?;

            assert_eq!(user_version(&conn).await?, CURRENT.user_version());
            assert_eq!(detect_schema_version(&conn).await?, Some(CURRENT));
            assert_eq!(
                kv_before,
                scalar_i64(&conn, "SELECT COUNT(*) FROM kv_store").await?
            );
            assert_eq!(
                tool_before,
                scalar_i64(&conn, "SELECT COUNT(*) FROM tool_calls").await?
            );
            assert_eq!(data_before, read_fixture_file_bytes(&conn).await?);

            drop(conn);
            drop(db);
            let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy())).await?;
            assert_eq!(agent.fs.read_file("/file.txt").await?.unwrap(), b"abcdef");
            let conn = agent.get_connection().await?;
            let report =
                integrity::check(&conn, &integrity::CheckOpts::new(db_path.clone())).await?;
            assert!(report.ok, "integrity failed for migrated {version}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn legacy_whiteout_without_parent_path_migrates_for_sdk_openers() -> Result<()> {
        let dir = tempdir()?;

        let agent_path =
            create_legacy_whiteout_fixture_file(dir.path(), "agent-open", SchemaVersion::V0_5)
                .await?;
        let agent = AgentFS::open(AgentFSOptions::with_path(agent_path.to_string_lossy())).await?;
        assert_eq!(agent.fs.read_file("/file.txt").await?.unwrap(), b"abcdef");
        drop(agent);
        assert_legacy_whiteout_parent_path(&agent_path, "AgentFS::open").await?;

        let kv_path =
            create_legacy_whiteout_fixture_file(dir.path(), "kv-open", SchemaVersion::V0_5).await?;
        let kv = KvStore::new(kv_path.to_str().unwrap()).await?;
        kv.set("after", &serde_json::json!({ "ok": true })).await?;
        drop(kv);
        assert_legacy_whiteout_parent_path(&kv_path, "KvStore::new").await?;

        let tool_path =
            create_legacy_whiteout_fixture_file(dir.path(), "tool-open", SchemaVersion::V0_5)
                .await?;
        let tools = ToolCalls::new(tool_path.to_str().unwrap()).await?;
        let id = tools.start("after", None).await?;
        tools.success(id, None).await?;
        drop(tools);
        assert_legacy_whiteout_parent_path(&tool_path, "ToolCalls::new").await?;

        Ok(())
    }

    #[tokio::test]
    async fn schema_interrupted_init_reopens_or_errors_cleanly() -> Result<()> {
        let dir = tempdir()?;
        let empty_path = dir.path().join("empty.db");
        let db = Builder::new_local(empty_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        ensure_current(&conn).await?;
        assert_eq!(user_version(&conn).await?, CURRENT.user_version());

        let config_only_path = dir.path().join("config-only.db");
        let db = Builder::new_local(config_only_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(
            "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            (),
        )
        .await?;
        ensure_current(&conn).await?;
        assert_eq!(user_version(&conn).await?, CURRENT.user_version());

        let hybrid_path = dir.path().join("hybrid-v05-no-markers.db");
        let db = Builder::new_local(hybrid_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        create_legacy_fixture(&conn, SchemaVersion::V0_5).await?;
        conn.execute(
            "DELETE FROM fs_config WHERE key = ?",
            (CONFIG_SCHEMA_VERSION_KEY,),
        )
        .await?;
        ensure_current(&conn).await?;
        assert_eq!(detect_schema_version(&conn).await?, Some(CURRENT));
        let agent = AgentFS::open(AgentFSOptions::with_path(hybrid_path.to_string_lossy())).await?;
        assert_eq!(agent.fs.read_file("/file.txt").await?.unwrap(), b"abcdef");

        let corrupt_current_path = dir.path().join("current-missing-table.db");
        let db = Builder::new_local(corrupt_current_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        set_user_version(&conn, CURRENT).await?;
        let err = ensure_current(&conn)
            .await
            .expect_err("missing current tables must error");
        assert!(
            err.to_string()
                .contains("current schema is missing required table"),
            "unexpected error: {err}"
        );

        Ok(())
    }

    async fn create_legacy_whiteout_fixture_file(
        dir: &Path,
        name: &str,
        version: SchemaVersion,
    ) -> Result<PathBuf> {
        let db_path = dir.join(format!("{name}.db"));
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;
        create_legacy_fixture(&conn, version).await?;
        add_legacy_whiteout_without_parent_path(&conn).await?;
        drop(conn);
        drop(db);
        Ok(db_path)
    }

    async fn add_legacy_whiteout_without_parent_path(conn: &Connection) -> Result<()> {
        conn.execute(
            "CREATE TABLE fs_whiteout (
                path TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            )",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO fs_whiteout (path, created_at) VALUES
             ('/dir/deleted', 123),
             ('/top-level', 456)",
            (),
        )
        .await?;
        Ok(())
    }

    async fn assert_legacy_whiteout_parent_path(db_path: &Path, label: &str) -> Result<()> {
        let db = Builder::new_local(db_path.to_str().unwrap())
            .build()
            .await?;
        let conn = db.connect()?;

        let column_names = get_table_columns(&conn, "fs_whiteout")
            .await?
            .into_iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        assert!(
            column_names.iter().any(|column| column == "parent_path"),
            "{label} did not add fs_whiteout.parent_path; columns={column_names:?}"
        );

        let mut rows = conn
            .query(
                "SELECT path, parent_path, created_at
                 FROM fs_whiteout
                 ORDER BY path",
                (),
            )
            .await?;
        let mut migrated = Vec::new();
        while let Some(row) = rows.next().await? {
            migrated.push((
                row.get::<String>(0)?,
                row.get::<String>(1)?,
                row.get::<i64>(2)?,
            ));
        }
        println!("{label}: fs_whiteout columns={column_names:?}; rows={migrated:?}");
        assert_eq!(
            migrated,
            vec![
                ("/dir/deleted".to_string(), "/dir".to_string(), 123),
                ("/top-level".to_string(), "/".to_string(), 456),
            ]
        );
        assert_eq!(user_version(&conn).await?, CURRENT.user_version());
        Ok(())
    }

    async fn create_legacy_fixture(conn: &Connection, version: SchemaVersion) -> Result<()> {
        conn.execute(
            "CREATE TABLE fs_config (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO fs_config (key, value) VALUES ('chunk_size', '4')",
            (),
        )
        .await?;
        if version == SchemaVersion::V0_5 {
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES ('schema_version', '0.5'), ('inline_threshold', '4')",
                (),
            )
            .await?;
        } else {
            conn.execute(
                "INSERT INTO fs_config (key, value) VALUES ('schema_version', ?), ('inline_threshold', '4')",
                (version.as_str(),),
            )
            .await?;
        }

        let mut columns = vec![
            "ino INTEGER PRIMARY KEY AUTOINCREMENT",
            "mode INTEGER NOT NULL",
            "uid INTEGER NOT NULL DEFAULT 0",
            "gid INTEGER NOT NULL DEFAULT 0",
            "size INTEGER NOT NULL DEFAULT 0",
            "atime INTEGER NOT NULL",
            "mtime INTEGER NOT NULL",
            "ctime INTEGER NOT NULL",
        ];
        if version >= SchemaVersion::V0_2 {
            columns.insert(2, "nlink INTEGER NOT NULL DEFAULT 0");
        }
        if version >= SchemaVersion::V0_4 {
            columns.extend([
                "rdev INTEGER NOT NULL DEFAULT 0",
                "atime_nsec INTEGER NOT NULL DEFAULT 0",
                "mtime_nsec INTEGER NOT NULL DEFAULT 0",
                "ctime_nsec INTEGER NOT NULL DEFAULT 0",
            ]);
        }
        if version >= SchemaVersion::V0_5 {
            columns.extend([
                "data_inline BLOB",
                "storage_kind INTEGER NOT NULL DEFAULT 0",
            ]);
        }
        conn.execute(
            &format!("CREATE TABLE fs_inode ({})", columns.join(", ")),
            (),
        )
        .await?;
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
        .await?;
        conn.execute(
            "CREATE TABLE fs_data (
                ino INTEGER NOT NULL,
                chunk_index INTEGER NOT NULL,
                data BLOB NOT NULL,
                PRIMARY KEY (ino, chunk_index)
            )",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE fs_symlink (ino INTEGER PRIMARY KEY, target TEXT NOT NULL)",
            (),
        )
        .await?;
        conn.execute(
            "CREATE TABLE kv_store (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                created_at INTEGER DEFAULT (unixepoch()),
                updated_at INTEGER DEFAULT (unixepoch())
            )",
            (),
        )
        .await?;
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
        .await?;

        insert_legacy_inode(conn, version, 1, S_IFDIR | 0o755, 2, 0).await?;
        insert_legacy_inode(conn, version, 2, S_IFREG | DEFAULT_FILE_MODE as i64, 1, 6).await?;
        conn.execute(
            "INSERT INTO fs_dentry (name, parent_ino, ino) VALUES ('file.txt', 1, 2)",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (2, 0, ?), (2, 1, ?)",
            (Value::Blob(b"abcd".to_vec()), Value::Blob(b"ef".to_vec())),
        )
        .await?;
        conn.execute(
            "INSERT INTO kv_store (key, value) VALUES ('k', '{\"v\":1}')",
            (),
        )
        .await?;
        conn.execute(
            "INSERT INTO tool_calls (name, parameters, status, started_at) VALUES ('tool', '{}', 'success', 1)",
            (),
        )
        .await?;
        Ok(())
    }

    async fn insert_legacy_inode(
        conn: &Connection,
        version: SchemaVersion,
        ino: i64,
        mode: i64,
        nlink: i64,
        size: i64,
    ) -> Result<()> {
        let mut columns = vec![
            "ino", "mode", "uid", "gid", "size", "atime", "mtime", "ctime",
        ];
        let mut values = vec![
            Value::Integer(ino),
            Value::Integer(mode),
            Value::Integer(0),
            Value::Integer(0),
            Value::Integer(size),
            Value::Integer(1),
            Value::Integer(1),
            Value::Integer(1),
        ];
        if version >= SchemaVersion::V0_2 {
            columns.insert(2, "nlink");
            values.insert(2, Value::Integer(nlink));
        }
        if version >= SchemaVersion::V0_4 {
            columns.extend(["rdev", "atime_nsec", "mtime_nsec", "ctime_nsec"]);
            values.extend([
                Value::Integer(0),
                Value::Integer(0),
                Value::Integer(0),
                Value::Integer(0),
            ]);
        }
        if version >= SchemaVersion::V0_5 {
            columns.extend(["data_inline", "storage_kind"]);
            values.extend([Value::Null, Value::Integer(0)]);
        }
        let placeholders = std::iter::repeat_n("?", columns.len())
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute(
            &format!(
                "INSERT INTO fs_inode ({}) VALUES ({})",
                columns.join(", "),
                placeholders
            ),
            values,
        )
        .await?;
        Ok(())
    }

    async fn read_fixture_file_bytes(conn: &Connection) -> Result<Vec<u8>> {
        let mut rows = conn
            .query(
                "SELECT data FROM fs_data WHERE ino = 2 ORDER BY chunk_index",
                (),
            )
            .await?;
        let mut bytes = Vec::new();
        while let Some(row) = rows.next().await? {
            match row.get_value(0)? {
                Value::Blob(chunk) => bytes.extend(chunk),
                other => {
                    return Err(Error::Internal(format!(
                        "unexpected fs_data value in fixture: {other:?}"
                    )))
                }
            }
        }
        Ok(bytes)
    }

    async fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64> {
        let mut rows = conn.query(sql, ()).await?;
        let row = rows
            .next()
            .await?
            .ok_or_else(|| Error::Internal(format!("query returned no rows: {sql}")))?;
        row.get(0).map_err(Error::from)
    }
}
