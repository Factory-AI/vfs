use agentfs_core::error::Result;
use agentfs_core::fs::{AgentFS as AgentFsCore, FileSystem, FsError};
use agentfs_core::{AgentFS, AgentFSOptions, ToolCallStatus, DEFAULT_FILE_MODE};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const ROOT_INO: i64 = 1;

#[derive(Debug, Clone)]
struct SnapshotCase {
    seed: usize,
    crossing_path: String,
    hardlink_path: String,
    inline_path: String,
    sparse_path: String,
    symlink_path: String,
    crossing_data: Vec<u8>,
    inline_data: Vec<u8>,
    sparse_offset: u64,
    sparse_tail: Vec<u8>,
}

#[derive(Debug, Clone)]
struct ToolIds {
    success: i64,
    failure: i64,
}

#[tokio::test]
async fn snapshot_restore_preserves_one_file_agent_state_after_checkpoint() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let source_db = temp_dir.path().join("source.db");
    let restored_db = temp_dir.path().join("restored.db");

    let agent = AgentFS::open(AgentFSOptions::with_path(source_db.to_string_lossy())).await?;
    let chunk_size = agent.fs.chunk_size();

    agent.fs.mkdir("/workspace", 0, 0).await?;

    let mut cases = Vec::new();
    let mut tool_ids = Vec::new();
    for seed in 0..3 {
        cases.push(create_snapshot_case(&agent, chunk_size, seed).await?);
        tool_ids.push(record_tool_calls(&agent, seed).await?);
    }

    assert_generated_state(&agent, &cases, &tool_ids).await?;
    assert_integrity_check_ok(&agent).await?;

    agent.fs.fsync().await?;
    assert_journal_mode_is_wal(&agent).await?;
    assert_wal_sidecar_checkpointed(&source_db);

    std::fs::copy(&source_db, &restored_db)?;

    let restored = AgentFS::open(AgentFSOptions::with_path(restored_db.to_string_lossy())).await?;
    assert_eq!(restored.fs.chunk_size(), chunk_size);
    assert_generated_state(&restored, &cases, &tool_ids).await?;
    assert_integrity_check_ok(&restored).await?;

    Ok(())
}

async fn create_snapshot_case(
    agent: &AgentFS,
    chunk_size: usize,
    seed: usize,
) -> Result<SnapshotCase> {
    let dir = format!("/workspace/seed-{seed}");
    let nested_dir = format!("{dir}/nested");
    let crossing_path = format!("{nested_dir}/crossing.bin");
    let hardlink_path = format!("{dir}/hardlink.bin");
    let inline_path = format!("{dir}/inline.txt");
    let sparse_path = format!("{dir}/sparse.bin");
    let symlink_path = format!("{dir}/link-to-crossing");

    agent.fs.mkdir(&dir, seed as u32, seed as u32).await?;
    agent
        .fs
        .mkdir(&nested_dir, seed as u32, seed as u32)
        .await?;

    let mut crossing_data = patterned_bytes(chunk_size * 2 + 137 + seed * 29, seed as u8);
    let patch_offset = chunk_size - 3 + seed;
    let patch = patterned_bytes(17 + seed, 0xA0 + seed as u8);
    crossing_data[patch_offset..patch_offset + patch.len()].copy_from_slice(&patch);

    let (_, crossing_file) = agent
        .fs
        .create_file(&crossing_path, DEFAULT_FILE_MODE, seed as u32, seed as u32)
        .await?;
    crossing_file
        .pwrite(0, &patterned_bytes(crossing_data.len(), seed as u8))
        .await?;
    crossing_file.pwrite(patch_offset as u64, &patch).await?;

    link_path(&agent.fs, &crossing_path, &hardlink_path).await?;

    let inline_data = patterned_bytes(512 + seed, 0x30 + seed as u8);
    let (_, inline_file) = agent
        .fs
        .create_file(&inline_path, DEFAULT_FILE_MODE, seed as u32, seed as u32)
        .await?;
    inline_file.pwrite(0, &inline_data).await?;

    let sparse_offset = (chunk_size * (seed + 1) + 31) as u64;
    let sparse_tail = patterned_bytes(19 + seed, 0x70 + seed as u8);
    let (_, sparse_file) = agent
        .fs
        .create_file(&sparse_path, DEFAULT_FILE_MODE, seed as u32, seed as u32)
        .await?;
    sparse_file.pwrite(sparse_offset, &sparse_tail).await?;

    agent
        .fs
        .stat(&nested_dir)
        .await?
        .expect("nested dir should exist");
    create_symlink_path(
        &agent.fs,
        "nested/crossing.bin",
        &symlink_path,
        seed as u32,
        seed as u32,
    )
    .await?;

    agent
        .kv
        .set(
            &format!("snapshot:{seed}:metadata"),
            &json!({
                "seed": seed,
                "crossing_len": crossing_data.len(),
                "sparse_offset": sparse_offset,
            }),
        )
        .await?;
    agent
        .kv
        .set(&format!("snapshot:{seed}:label"), &format!("case-{seed}"))
        .await?;

    Ok(SnapshotCase {
        seed,
        crossing_path,
        hardlink_path,
        inline_path,
        sparse_path,
        symlink_path,
        crossing_data,
        inline_data,
        sparse_offset,
        sparse_tail,
    })
}

async fn record_tool_calls(agent: &AgentFS, seed: usize) -> Result<ToolIds> {
    let started_at = 1_700_000_000 + seed as i64 * 10;
    let success = agent
        .tools
        .record(
            "snapshot_restore_success",
            started_at,
            started_at + 2,
            Some(json!({ "seed": seed, "op": "copy-main-db" })),
            Some(json!({ "ok": true, "seed": seed })),
            None,
        )
        .await?;

    let failure = agent
        .tools
        .record(
            "snapshot_restore_error",
            started_at + 3,
            started_at + 4,
            Some(json!({ "seed": seed, "op": "negative-path" })),
            None,
            Some("expected test error"),
        )
        .await?;

    Ok(ToolIds { success, failure })
}

async fn assert_generated_state(
    agent: &AgentFS,
    cases: &[SnapshotCase],
    tool_ids: &[ToolIds],
) -> Result<()> {
    let workspace = agent.fs.stat("/workspace").await?.unwrap();
    assert!(workspace.is_directory());

    let mut workspace_entries = agent.fs.readdir(workspace.ino).await?.unwrap();
    workspace_entries.sort();
    assert_eq!(workspace_entries, vec!["seed-0", "seed-1", "seed-2"]);

    for case in cases {
        let dir_path = format!("/workspace/seed-{}", case.seed);
        let dir_stats = agent.fs.stat(&dir_path).await?.unwrap();
        assert!(dir_stats.is_directory());

        let mut entries = agent.fs.readdir(dir_stats.ino).await?.unwrap();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                "hardlink.bin".to_string(),
                "inline.txt".to_string(),
                "link-to-crossing".to_string(),
                "nested".to_string(),
                "sparse.bin".to_string(),
            ]
        );

        let crossing = agent.fs.read_file(&case.crossing_path).await?.unwrap();
        assert_eq!(crossing, case.crossing_data);

        let crossing_stats = agent.fs.stat(&case.crossing_path).await?.unwrap();
        assert!(crossing_stats.is_file());
        assert_eq!(crossing_stats.size, case.crossing_data.len() as i64);

        let hardlink_stats = agent.fs.stat(&case.hardlink_path).await?.unwrap();
        assert_eq!(hardlink_stats.ino, crossing_stats.ino);
        assert_eq!(hardlink_stats.nlink, 2);
        assert_eq!(
            agent.fs.read_file(&case.hardlink_path).await?.unwrap(),
            case.crossing_data
        );

        let inline = agent.fs.read_file(&case.inline_path).await?.unwrap();
        assert_eq!(inline, case.inline_data);
        let inline_stats = agent.fs.stat(&case.inline_path).await?.unwrap();
        assert!(inline_stats.is_file());
        assert_eq!(inline_stats.size, case.inline_data.len() as i64);
        assert_inline_inode_has_no_chunks(agent, inline_stats.ino, &case.inline_data).await?;

        let sparse_stats = agent.fs.stat(&case.sparse_path).await?.unwrap();
        let sparse_size = case.sparse_offset + case.sparse_tail.len() as u64;
        assert_eq!(sparse_stats.size, sparse_size as i64);
        let sparse_file = agent.fs.open(&case.sparse_path).await?;
        let sparse_contents = sparse_file.pread(0, sparse_size).await?;
        let mut expected_sparse = vec![0; case.sparse_offset as usize];
        expected_sparse.extend_from_slice(&case.sparse_tail);
        assert_eq!(sparse_contents, expected_sparse);

        let symlink_stats = lstat_path(&agent.fs, &case.symlink_path).await?.unwrap();
        assert!(symlink_stats.is_symlink());
        assert_eq!(
            agent.fs.readlink(&case.symlink_path).await?,
            Some("nested/crossing.bin".to_string())
        );
        let followed_symlink = agent.fs.stat(&case.symlink_path).await?.unwrap();
        assert_eq!(followed_symlink.ino, crossing_stats.ino);

        let metadata: Option<Value> = agent
            .kv
            .get(&format!("snapshot:{}:metadata", case.seed))
            .await?;
        assert_eq!(
            metadata,
            Some(json!({
                "seed": case.seed,
                "crossing_len": case.crossing_data.len(),
                "sparse_offset": case.sparse_offset,
            }))
        );

        let label: Option<String> = agent
            .kv
            .get(&format!("snapshot:{}:label", case.seed))
            .await?;
        assert_eq!(label, Some(format!("case-{}", case.seed)));
    }

    let mut keys = agent.kv.keys().await?;
    keys.sort();
    assert_eq!(
        keys,
        vec![
            "snapshot:0:label",
            "snapshot:0:metadata",
            "snapshot:1:label",
            "snapshot:1:metadata",
            "snapshot:2:label",
            "snapshot:2:metadata",
        ]
    );

    for ids in tool_ids {
        let success = agent.tools.get(ids.success).await?.unwrap();
        assert_eq!(success.name, "snapshot_restore_success");
        assert_eq!(success.status, ToolCallStatus::Success);
        assert!(success.error.is_none());

        let failure = agent.tools.get(ids.failure).await?.unwrap();
        assert_eq!(failure.name, "snapshot_restore_error");
        assert_eq!(failure.status, ToolCallStatus::Error);
        assert_eq!(failure.error.as_deref(), Some("expected test error"));
    }

    let success_stats = agent
        .tools
        .stats_for("snapshot_restore_success")
        .await?
        .unwrap();
    assert_eq!(success_stats.total_calls, cases.len() as i64);
    assert_eq!(success_stats.successful, cases.len() as i64);
    assert_eq!(success_stats.failed, 0);

    let error_stats = agent
        .tools
        .stats_for("snapshot_restore_error")
        .await?
        .unwrap();
    assert_eq!(error_stats.total_calls, cases.len() as i64);
    assert_eq!(error_stats.successful, 0);
    assert_eq!(error_stats.failed, cases.len() as i64);

    Ok(())
}

async fn parent_and_name(fs: &AgentFsCore, path: &str) -> Result<(i64, String)> {
    let normalized = normalize_test_path(path);
    let components = normalized
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if components.is_empty() {
        return Err(FsError::RootOperation.into());
    }
    let name = components.last().unwrap().to_string();
    let parent_path = if components.len() == 1 {
        "/".to_string()
    } else {
        format!("/{}", components[..components.len() - 1].join("/"))
    };
    let parent_ino = if parent_path == "/" {
        ROOT_INO
    } else {
        fs.stat(&parent_path).await?.ok_or(FsError::NotFound)?.ino
    };
    Ok((parent_ino, name))
}

fn normalize_test_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

async fn link_path(fs: &AgentFsCore, oldpath: &str, newpath: &str) -> Result<()> {
    let source = fs.stat(oldpath).await?.ok_or(FsError::NotFound)?;
    let (newparent_ino, newname) = parent_and_name(fs, newpath).await?;
    FileSystem::link(fs, source.ino, newparent_ino, &newname)
        .await
        .map(|_| ())
}

async fn create_symlink_path(
    fs: &AgentFsCore,
    target: &str,
    linkpath: &str,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let (parent_ino, name) = parent_and_name(fs, linkpath).await?;
    FileSystem::symlink(fs, parent_ino, &name, target, uid, gid)
        .await
        .map(|_| ())
}

async fn lstat_path(fs: &AgentFsCore, path: &str) -> Result<Option<agentfs_core::fs::Stats>> {
    let normalized = normalize_test_path(path);
    if normalized == "/" {
        return FileSystem::getattr(fs, ROOT_INO).await;
    }
    let (parent_ino, name) = parent_and_name(fs, &normalized).await?;
    FileSystem::lookup(fs, parent_ino, &name).await
}

async fn assert_integrity_check_ok(agent: &AgentFS) -> Result<()> {
    let conn = agent.get_connection().await?;
    let mut rows = conn.query("PRAGMA integrity_check", ()).await?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().await? {
        results.push(row.get::<String>(0)?);
    }
    assert_eq!(results, vec!["ok".to_string()]);
    Ok(())
}

async fn assert_journal_mode_is_wal(agent: &AgentFS) -> Result<()> {
    let conn = agent.get_connection().await?;
    let mut rows = conn.query("PRAGMA journal_mode", ()).await?;
    let row = rows.next().await?.unwrap();
    assert_eq!(row.get::<String>(0)?.to_lowercase(), "wal");
    Ok(())
}

async fn assert_inline_inode_has_no_chunks(
    agent: &AgentFS,
    ino: i64,
    expected: &[u8],
) -> Result<()> {
    agent.fs.drain_all().await?;
    let conn = agent.get_connection().await?;
    let mut rows = conn
        .query(
            "SELECT storage_kind, data_inline FROM fs_inode WHERE ino = ?",
            (ino,),
        )
        .await?;
    let row = rows.next().await?.unwrap();
    assert_eq!(row.get::<i64>(0)?, 1);
    assert_eq!(row.get::<Vec<u8>>(1)?, expected);

    let mut rows = conn
        .query("SELECT COUNT(*) FROM fs_data WHERE ino = ?", (ino,))
        .await?;
    let row = rows.next().await?.unwrap();
    assert_eq!(row.get::<i64>(0)?, 0);
    Ok(())
}

fn assert_wal_sidecar_checkpointed(db_path: &Path) {
    let wal_path = wal_sidecar_path(db_path);
    if let Ok(metadata) = std::fs::metadata(&wal_path) {
        assert_eq!(
            metadata.len(),
            0,
            "WAL sidecar should be empty after fsync checkpoint: {}",
            wal_path.display()
        );
    }
}

fn wal_sidecar_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", db_path.display()))
}

fn patterned_bytes(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|index| {
            seed.wrapping_mul(37)
                .wrapping_add((index % 251) as u8)
                .wrapping_add((index / 251) as u8)
        })
        .collect()
}
