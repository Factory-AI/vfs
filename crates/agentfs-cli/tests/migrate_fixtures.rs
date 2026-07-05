//! Committed old-schema fixture databases for migrate tests and validators.
//!
//! Fixtures live in `tests/fixtures/migrate/` and are exercised by
//! `test-migrate-consolidation.sh` and by validation flows against real old
//! databases (v0.0, v0.2, v0.4, and an encrypted v0.4).
//!
//! Regenerate after a schema-authority or turso format change with:
//!
//! ```sh
//! cargo +nightly test -p agentfs-cli --test migrate_fixtures -- --ignored
//! ```

use agentfs_core::schema::{self, SchemaVersion};
use std::fs;
use std::path::{Path, PathBuf};
use turso::{Builder, Connection, EncryptionOpts, Value};

/// Key/cipher for `v0_4-encrypted.db`; kept in step with
/// `test-migrate-consolidation.sh`.
const ENCRYPTED_FIXTURE_KEY: &str =
    "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const ENCRYPTED_FIXTURE_CIPHER: &str = "aes256gcm";

const FIXTURES: &[(&str, SchemaVersion, bool)] = &[
    ("v0_0.db", SchemaVersion::V0_0, false),
    ("v0_2.db", SchemaVersion::V0_2, false),
    ("v0_4.db", SchemaVersion::V0_4, false),
    ("v0_4-encrypted.db", SchemaVersion::V0_4, true),
];

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/migrate")
}

#[tokio::test]
#[ignore = "rewrites the committed fixture databases; run explicitly"]
async fn regenerate_migrate_fixtures() {
    let dir = fixtures_dir();
    fs::create_dir_all(&dir).unwrap();
    for (name, version, encrypted) in FIXTURES {
        let path = dir.join(name);
        for suffix in ["", "-wal", "-shm"] {
            let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
            match fs::remove_file(&sidecar) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => panic!("cannot remove {}: {err}", sidecar.display()),
            }
        }
        create_fixture(&path, *version, *encrypted).await;
        assert!(path.is_file());
        println!(
            "regenerated {} ({} bytes)",
            path.display(),
            fs::metadata(&path).unwrap().len()
        );
    }
}

#[tokio::test]
async fn committed_fixtures_detect_as_expected_versions() {
    let scratch = tempfile::tempdir().unwrap();
    for (name, version, encrypted) in FIXTURES {
        let committed = fixtures_dir().join(name);
        assert!(
            committed.is_file(),
            "missing committed fixture {} (regenerate with --ignored)",
            committed.display()
        );
        let copy = scratch.path().join(name);
        fs::copy(&committed, &copy).unwrap();

        let mut builder = Builder::new_local(copy.to_str().unwrap());
        if *encrypted {
            builder = builder
                .experimental_encryption(true)
                .with_encryption(EncryptionOpts {
                    cipher: ENCRYPTED_FIXTURE_CIPHER.to_string(),
                    hexkey: ENCRYPTED_FIXTURE_KEY.to_string(),
                });
        }
        let db = builder.build().await.unwrap();
        let conn = db.connect().unwrap();
        let detected = schema::detect_schema_version(&conn).await.unwrap();
        assert_eq!(detected, Some(*version), "{name}");

        let mut rows = conn.query("PRAGMA user_version", ()).await.unwrap();
        let row = rows.next().await.unwrap().unwrap();
        let user_version: i64 = row.get(0).unwrap();
        assert_eq!(user_version, 0, "{name}: old fixtures predate user_version");
    }

    let committed = fixtures_dir().join("v0_4-encrypted.db");
    let copy = scratch.path().join("v0_4-encrypted-nokey.db");
    fs::copy(&committed, &copy).unwrap();
    let without_key = async {
        let db = Builder::new_local(copy.to_str().unwrap()).build().await?;
        let conn = db.connect()?;
        let mut rows = conn.query("SELECT COUNT(*) FROM fs_inode", ()).await?;
        rows.next().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    assert!(
        without_key.is_err(),
        "encrypted fixture must not open without the key"
    );
}

async fn create_fixture(path: &Path, version: SchemaVersion, encrypted: bool) {
    let mut builder = Builder::new_local(path.to_str().unwrap());
    if encrypted {
        builder = builder
            .experimental_encryption(true)
            .with_encryption(EncryptionOpts {
                cipher: ENCRYPTED_FIXTURE_CIPHER.to_string(),
                hexkey: ENCRYPTED_FIXTURE_KEY.to_string(),
            });
    }
    let db = builder.build().await.unwrap();
    let conn = db.connect().unwrap();
    populate_fixture(&conn, version).await;

    let mut rows = conn
        .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
        .await
        .unwrap();
    while rows.next().await.unwrap().is_some() {}
    drop(conn);
    drop(db);
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        if let Ok(metadata) = fs::metadata(&sidecar) {
            assert_eq!(
                metadata.len(),
                0,
                "fixture sidecar {} not empty after checkpoint",
                sidecar.display()
            );
            fs::remove_file(&sidecar).unwrap();
        }
    }
}

async fn populate_fixture(conn: &Connection, version: SchemaVersion) {
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
         ('custom_metadata', 'fixture-preserve-me')",
        (version.as_str(),),
    )
    .await
    .unwrap();

    let mut inode_columns = vec![
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
        inode_columns.insert(2, "nlink INTEGER NOT NULL DEFAULT 0");
    }
    if version >= SchemaVersion::V0_4 {
        inode_columns.extend([
            "rdev INTEGER NOT NULL DEFAULT 0",
            "atime_nsec INTEGER NOT NULL DEFAULT 0",
            "mtime_nsec INTEGER NOT NULL DEFAULT 0",
            "ctime_nsec INTEGER NOT NULL DEFAULT 0",
        ]);
    }
    conn.execute(
        &format!("CREATE TABLE fs_inode ({})", inode_columns.join(", ")),
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

    let dir_mode: i64 = 0o040000 | 0o755;
    let file_mode: i64 = 0o100000 | 0o644;
    let symlink_mode: i64 = 0o120000 | 0o777;
    let small = b"hello fixture".to_vec();
    let large: Vec<u8> = (0..(2 * 4096 + 37))
        .map(|index| 0x37_u8.wrapping_add((index % 251) as u8))
        .collect();

    let inodes: [(i64, i64, i64, i64); 5] = [
        (1, dir_mode, 2, 0),
        (2, dir_mode, 1, 0),
        (3, file_mode, 1, small.len() as i64),
        (4, file_mode, 2, large.len() as i64),
        (5, symlink_mode, 1, 9),
    ];
    for (ino, mode, nlink, size) in inodes {
        let mut columns = vec![
            "ino", "mode", "uid", "gid", "size", "atime", "mtime", "ctime",
        ];
        let mut values = vec![
            Value::Integer(ino),
            Value::Integer(mode),
            Value::Integer(1000),
            Value::Integer(1000),
            Value::Integer(size),
            Value::Integer(1700000000),
            Value::Integer(1700000000),
            Value::Integer(1700000000),
        ];
        if version >= SchemaVersion::V0_2 {
            columns.insert(2, "nlink");
            values.insert(2, Value::Integer(nlink));
        }
        if version >= SchemaVersion::V0_4 {
            columns.extend(["rdev", "atime_nsec", "mtime_nsec", "ctime_nsec"]);
            values.extend([
                Value::Integer(0),
                Value::Integer(111),
                Value::Integer(222),
                Value::Integer(333),
            ]);
        }
        let placeholders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(", ");
        conn.execute(
            &format!(
                "INSERT INTO fs_inode ({}) VALUES ({placeholders})",
                columns.join(", ")
            ),
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
         (4, 'large-hardlink.bin', 2, 4),
         (5, 'small-link', 2, 5)",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO fs_symlink (ino, target) VALUES (5, 'small.txt')",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO fs_data (ino, chunk_index, data) VALUES (3, 0, ?)",
        (Value::Blob(small),),
    )
    .await
    .unwrap();
    for (chunk_index, chunk) in large.chunks(4096).enumerate() {
        conn.execute(
            "INSERT INTO fs_data (ino, chunk_index, data) VALUES (4, ?, ?)",
            (chunk_index as i64, Value::Blob(chunk.to_vec())),
        )
        .await
        .unwrap();
    }
    conn.execute(
        "INSERT INTO kv_store (key, value, created_at, updated_at)
         VALUES ('fixture', '{\"ok\":true}', 20, 21)",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "INSERT INTO tool_calls
         (id, name, parameters, result, error, status, started_at, completed_at, duration_ms)
         VALUES (1, 'fixture-tool', '{}', '{}', '', 'success', 30, 31, 42)",
        (),
    )
    .await
    .unwrap();
}
