use agentfs_sdk::error::Result;
use agentfs_sdk::{AgentFS, AgentFSOptions, DEFAULT_FILE_MODE};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Barrier;
use tokio::time::{sleep, Duration};

const WORKERS: usize = 6;
const ITERATIONS: usize = 4;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sdk_operations_preserve_database_integrity() -> Result<()> {
    let temp_dir = tempfile::tempdir()?;
    let db_path = temp_dir.path().join("concurrent.db");
    let agent = AgentFS::open(AgentFSOptions::with_path(db_path.to_string_lossy())).await?;
    let start_barrier = Arc::new(Barrier::new(WORKERS + 1));
    let active_workers = Arc::new(AtomicUsize::new(0));

    assert_integrity_check_ok(&agent).await?;

    let mut handles = Vec::new();
    for worker in 0..WORKERS {
        let fs = agent.fs.clone();
        let kv = agent.kv.clone();
        let tools = agent.tools.clone();
        let start_barrier = start_barrier.clone();
        let active_workers = active_workers.clone();

        handles.push(tokio::spawn(async move {
            start_barrier.wait().await;
            active_workers.fetch_add(1, Ordering::SeqCst);

            let result: Result<()> = async {
                let worker_dir = format!("/worker-{worker}");
                fs.mkdir(&worker_dir, worker as u32, worker as u32).await?;

                for iteration in 0..ITERATIONS {
                    let iteration_dir = format!("{worker_dir}/iter-{iteration}");
                    fs.mkdir(&iteration_dir, worker as u32, worker as u32)
                        .await?;

                    let file_path = format!("{iteration_dir}/payload.bin");
                    let mut expected = payload_bytes(worker, iteration);
                    let patch_offset = expected.len() / 2;
                    let patch = [
                        worker as u8,
                        iteration as u8,
                        0xAA,
                        0x55,
                        (worker + iteration) as u8,
                    ];
                    expected[patch_offset..patch_offset + patch.len()].copy_from_slice(&patch);

                    let (_, file) = fs
                        .create_file(&file_path, DEFAULT_FILE_MODE, worker as u32, worker as u32)
                        .await?;
                    file.pwrite(0, &payload_bytes(worker, iteration)).await?;
                    file.pwrite(patch_offset as u64, &patch).await?;

                    let read_back = fs.read_file(&file_path).await?.unwrap();
                    assert_eq!(read_back, expected);

                    let stat = fs.stat(&file_path).await?.unwrap();
                    assert!(stat.is_file());
                    assert_eq!(stat.size, expected.len() as i64);

                    let checksum = checksum(&expected);
                    let key = format!("worker:{worker}:iter:{iteration}");
                    let value = json!({
                        "worker": worker,
                        "iteration": iteration,
                        "len": expected.len(),
                        "checksum": checksum,
                    });
                    kv.set(&key, &value).await?;
                    let fetched: Option<Value> = kv.get(&key).await?;
                    assert_eq!(fetched, Some(value));

                    tools
                        .record(
                            "concurrency_integrity_worker",
                            1_800_000_000 + worker as i64 * 100 + iteration as i64,
                            1_800_000_001 + worker as i64 * 100 + iteration as i64,
                            Some(json!({ "worker": worker, "iteration": iteration })),
                            Some(json!({ "checksum": checksum })),
                            None,
                        )
                        .await?;

                    tokio::task::yield_now().await;
                }

                Ok(())
            }
            .await;

            active_workers.fetch_sub(1, Ordering::SeqCst);
            result
        }));
    }

    start_barrier.wait().await;
    for _ in 0..100 {
        if active_workers.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        active_workers.load(Ordering::SeqCst) > 0,
        "workers should be active before overlap integrity checks"
    );

    let mut overlap_checks = 0;
    for _ in 0..200 {
        if active_workers.load(Ordering::SeqCst) == 0 {
            break;
        }
        sleep(Duration::from_millis(5)).await;
        assert_integrity_check_ok(&agent).await?;
        overlap_checks += 1;
    }
    assert_eq!(
        active_workers.load(Ordering::SeqCst),
        0,
        "workers did not finish before overlap integrity-check deadline"
    );
    assert!(overlap_checks > 0);

    for handle in handles {
        handle.await.expect("worker task panicked")?;
    }

    agent.fs.fsync("/").await?;
    assert_integrity_check_ok(&agent).await?;
    assert_final_state(&agent).await?;

    Ok(())
}

async fn assert_final_state(agent: &AgentFS) -> Result<()> {
    for worker in 0..WORKERS {
        let worker_dir = format!("/worker-{worker}");
        let worker_stat = agent.fs.stat(&worker_dir).await?.unwrap();
        assert!(worker_stat.is_directory());

        let mut iteration_entries = agent.fs.readdir(worker_stat.ino).await?.unwrap();
        iteration_entries.sort();
        let expected_entries: Vec<String> = (0..ITERATIONS)
            .map(|iteration| format!("iter-{iteration}"))
            .collect();
        assert_eq!(iteration_entries, expected_entries);

        for iteration in 0..ITERATIONS {
            let file_path = format!("{worker_dir}/iter-{iteration}/payload.bin");
            let mut expected = payload_bytes(worker, iteration);
            let patch_offset = expected.len() / 2;
            let patch = [
                worker as u8,
                iteration as u8,
                0xAA,
                0x55,
                (worker + iteration) as u8,
            ];
            expected[patch_offset..patch_offset + patch.len()].copy_from_slice(&patch);

            let read_back = agent.fs.read_file(&file_path).await?.unwrap();
            assert_eq!(read_back, expected);
            let stats = agent.fs.stat(&file_path).await?.unwrap();
            assert_inline_inode_has_no_chunks(agent, stats.ino, &expected).await?;

            let key = format!("worker:{worker}:iter:{iteration}");
            let value: Option<Value> = agent.kv.get(&key).await?;
            assert_eq!(
                value,
                Some(json!({
                    "worker": worker,
                    "iteration": iteration,
                    "len": expected.len(),
                    "checksum": checksum(&expected),
                }))
            );
        }
    }

    let mut keys = agent.kv.keys().await?;
    keys.sort();
    assert_eq!(keys.len(), WORKERS * ITERATIONS);
    for worker in 0..WORKERS {
        for iteration in 0..ITERATIONS {
            assert!(keys.contains(&format!("worker:{worker}:iter:{iteration}")));
        }
    }

    let stats = agent
        .tools
        .stats_for("concurrency_integrity_worker")
        .await?
        .unwrap();
    assert_eq!(stats.total_calls, (WORKERS * ITERATIONS) as i64);
    assert_eq!(stats.successful, (WORKERS * ITERATIONS) as i64);
    assert_eq!(stats.failed, 0);

    Ok(())
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

async fn assert_inline_inode_has_no_chunks(
    agent: &AgentFS,
    ino: i64,
    expected: &[u8],
) -> Result<()> {
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

fn payload_bytes(worker: usize, iteration: usize) -> Vec<u8> {
    let len = 1_500 + worker * 73 + iteration * 41;
    (0..len)
        .map(|index| {
            (worker as u8)
                .wrapping_mul(31)
                .wrapping_add((iteration as u8).wrapping_mul(17))
                .wrapping_add((index % 251) as u8)
                .wrapping_add((index / 251) as u8)
        })
        .collect()
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0_u64, |sum, byte| sum.wrapping_add(*byte as u64))
}
