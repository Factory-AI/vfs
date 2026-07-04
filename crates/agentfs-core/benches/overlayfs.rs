//! Performance benchmarks for OverlayFS operations.
//!
//! Run with: cargo bench --bench overlayfs

use agentfs_core::fs::{AgentFS, FileSystem, HostFS, OverlayFS};
use criterion::{criterion_group, criterion_main, Criterion};
use std::sync::Arc;
use tempfile::tempdir;

const ROOT_INO: i64 = 1;

fn bench_remove_file(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("remove_file", |b| {
        b.iter_batched(
            || {
                // Setup: create overlay with one file
                rt.block_on(async {
                    let base_dir = tempdir().expect("Failed to create base temp dir");
                    let delta_dir = tempdir().expect("Failed to create delta temp dir");

                    // Create a file in base
                    let path = base_dir.path().join("test.txt");
                    std::fs::write(&path, "test content").expect("Failed to write file");

                    let base =
                        Arc::new(HostFS::new(base_dir.path()).expect("Failed to create HostFS"));
                    let db_path = delta_dir.path().join("delta.db");
                    let delta = AgentFS::new(db_path.to_str().unwrap())
                        .await
                        .expect("Failed to create AgentFS");

                    let overlay = OverlayFS::new(base, delta);
                    overlay
                        .init(base_dir.path().to_str().unwrap())
                        .await
                        .expect("Failed to init overlay");

                    (overlay, base_dir, delta_dir)
                })
            },
            |(overlay, _base_dir, _delta_dir)| {
                rt.block_on(async {
                    let _ = overlay.unlink(ROOT_INO, "test.txt").await;
                });
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_remove_file);
criterion_main!(benches);
