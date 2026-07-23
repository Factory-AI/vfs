//! Read-only integrity checks for AgentFS databases.

use crate::error::{Error, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use turso::{Connection, Value};

const S_IFMT: i64 = 0o170000;
const S_IFREG: i64 = 0o100000;
const S_IFDIR: i64 = 0o040000;
const S_IFLNK: i64 = 0o120000;

#[derive(Debug, Clone)]
struct PartialOriginRow {
    base_path: String,
    base_fingerprint_size: i64,
    base_mtime: i64,
    base_mtime_nsec: i64,
    base_ctime: i64,
    base_ctime_nsec: i64,
}

/// Options controlling read-only integrity checks.
#[derive(Debug, Clone)]
pub struct CheckOpts {
    database: PathBuf,
    require_portable: bool,
    check_base: bool,
}

impl CheckOpts {
    pub fn new(database: impl Into<PathBuf>) -> Self {
        Self {
            database: database.into(),
            require_portable: false,
            check_base: false,
        }
    }

    pub fn require_portable(mut self, require_portable: bool) -> Self {
        self.require_portable = require_portable;
        self
    }

    pub fn check_base(mut self, check_base: bool) -> Self {
        self.check_base = check_base;
        self
    }
}

/// Integrity report emitted by the CLI and usable by SDK callers.
#[derive(Debug, Serialize)]
pub struct Report {
    pub database: String,
    pub ok: bool,
    pub portable: bool,
    origin_backed: bool,
    partial_origin_rows: i64,
    pub checks: Vec<Check>,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    violating_rows: Option<i64>,
}

impl Report {
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
        self.checks.push(Check {
            name: name.into(),
            ok,
            detail: detail.into(),
            violating_rows,
        });
    }
}

/// Run the integrity battery without mutating the database family.
pub async fn check(conn: &Connection, opts: &CheckOpts) -> Result<Report> {
    let mut report = Report::new(&opts.database);

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
    check_portability_status(conn, &mut report, opts.require_portable).await?;
    check_optional_overlay_invariants(conn, &mut report, opts.check_base).await?;

    Ok(report)
}

async fn check_config(conn: &Connection, report: &mut Report) -> Result<()> {
    let schema_version = config_string(conn, "schema_version").await?;
    report.push_check(
        "config.schema_version",
        schema_version.as_deref() == Some(super::AGENTFS_SCHEMA_VERSION),
        schema_version
            .as_deref()
            .map(|value| format!("found {value}"))
            .unwrap_or_else(|| "missing".to_string()),
        if schema_version.as_deref() == Some(super::AGENTFS_SCHEMA_VERSION) {
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

async fn check_storage_invariants(conn: &Connection, report: &mut Report) -> Result<()> {
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

async fn check_namespace_invariants(conn: &Connection, report: &mut Report) -> Result<()> {
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

async fn check_symlink_invariants(conn: &Connection, report: &mut Report) -> Result<()> {
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
    report: &mut Report,
    require_portable: bool,
) -> Result<()> {
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
    report: &mut Report,
    check_base: bool,
) -> Result<()> {
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
    report: &mut Report,
) -> Result<()> {
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
        let result = (|| -> Result<()> {
            let base_path = resolve_materialization_base_path(&base_root, &partial.base_path)?;
            let metadata = fs::metadata(&base_path).map_err(|err| {
                Error::Internal(format!(
                    "Failed to stat base file {}: {err}",
                    base_path.display()
                ))
            })?;
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

async fn load_partial_origin_rows(conn: &Connection) -> Result<Vec<PartialOriginRow>> {
    if !table_exists(conn, "fs_partial_origin").await? {
        return Ok(Vec::new());
    }

    let mut rows = conn
        .query(
            "SELECT base_path, base_size, base_fingerprint_size,
                    base_mtime, base_mtime_nsec, base_ctime, base_ctime_nsec
             FROM fs_partial_origin
             ORDER BY delta_ino",
            (),
        )
        .await?;
    let mut partial_rows = Vec::new();
    while let Some(row) = rows.next().await? {
        let base_path: String = row.get(0)?;
        let base_size = value_i64(row.get_value(1)?)?;
        let raw_fingerprint_size = value_i64(row.get_value(2)?)?;
        partial_rows.push(PartialOriginRow {
            base_path,
            base_fingerprint_size: if raw_fingerprint_size < 0 {
                base_size
            } else {
                raw_fingerprint_size
            },
            base_mtime: value_i64(row.get_value(3)?)?,
            base_mtime_nsec: value_i64(row.get_value(4)?)?,
            base_ctime: value_i64(row.get_value(5)?)?,
            base_ctime_nsec: value_i64(row.get_value(6)?)?,
        });
    }
    Ok(partial_rows)
}

async fn read_overlay_base_root(conn: &Connection) -> Result<PathBuf> {
    if !table_exists(conn, "fs_overlay_config").await? {
        return Err(Error::Internal(
            "partial-origin database is missing fs_overlay_config".to_string(),
        ));
    }
    let mut rows = conn
        .query(
            "SELECT value FROM fs_overlay_config WHERE key = 'base_path'",
            (),
        )
        .await?;
    let row = rows.next().await?.ok_or_else(|| {
        Error::Internal(
            "partial-origin database is missing fs_overlay_config.base_path".to_string(),
        )
    })?;
    let base_path: String = row.get(0)?;
    let base_root = PathBuf::from(base_path);
    base_root.canonicalize().map_err(|err| {
        Error::Internal(format!(
            "Failed to canonicalize base root {}: {err}",
            base_root.display()
        ))
    })
}

fn resolve_materialization_base_path(base_root: &Path, recorded_path: &str) -> Result<PathBuf> {
    let mut candidate = base_root.to_path_buf();
    for component in Path::new(recorded_path).components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => candidate.push(part),
            Component::ParentDir => {
                return Err(Error::Internal(format!(
                    "partial-origin base path escapes base root: {recorded_path}"
                )))
            }
            Component::Prefix(_) => {
                return Err(Error::Internal(format!(
                    "partial-origin base path has an unsupported prefix: {recorded_path}"
                )))
            }
        }
    }

    let canonical = candidate.canonicalize().map_err(|err| {
        Error::Internal(format!(
            "Failed to canonicalize base path {}: {err}",
            candidate.display()
        ))
    })?;
    if !canonical.starts_with(base_root) {
        return Err(Error::Internal(format!(
            "partial-origin base path escapes base root: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn validate_base_fingerprint(
    partial: &PartialOriginRow,
    metadata: &fs::Metadata,
    path: &Path,
) -> Result<()> {
    let fingerprint = metadata_fingerprint(metadata);
    if fingerprint.size != partial.base_fingerprint_size
        || fingerprint.mtime != partial.base_mtime
        || fingerprint.mtime_nsec != partial.base_mtime_nsec
        || fingerprint.ctime != partial.base_ctime
        || fingerprint.ctime_nsec != partial.base_ctime_nsec
    {
        return Err(Error::Internal(format!(
            "partial-origin base changed for {} (stored size={}, current size={}, path={})",
            partial.base_path,
            partial.base_fingerprint_size,
            fingerprint.size,
            path.display()
        )));
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

async fn add_zero_count_check(
    conn: &Connection,
    report: &mut Report,
    name: &str,
    sql: &str,
) -> Result<()> {
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
    report: &mut Report,
    name: &str,
    sql: &str,
    expected: i64,
) -> Result<()> {
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

async fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let mut rows = conn
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
            (table,),
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

async fn query_string_column(conn: &Connection, sql: &str) -> Result<Vec<String>> {
    let mut rows = conn.query(sql, ()).await?;
    let mut values = Vec::new();
    while let Some(row) = rows.next().await? {
        values.push(row.get(0)?);
    }
    Ok(values)
}

async fn scalar_i64(conn: &Connection, sql: &str) -> Result<i64> {
    let mut rows = conn.query(sql, ()).await?;
    let row = rows
        .next()
        .await?
        .ok_or_else(|| Error::Internal(format!("query returned no rows: {sql}")))?;
    value_i64(row.get_value(0)?)
}

async fn config_string(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut rows = conn
        .query("SELECT value FROM fs_config WHERE key = ?", (key,))
        .await?;
    if let Some(row) = rows.next().await? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

async fn config_i64(conn: &Connection, key: &str) -> Result<Option<i64>> {
    let Some(value) = config_string(conn, key).await? else {
        return Ok(None);
    };
    Ok(value.parse::<i64>().ok())
}

fn value_i64(value: Value) -> Result<i64> {
    value
        .as_integer()
        .copied()
        .ok_or_else(|| Error::Internal("Expected integer result".to_string()))
}
