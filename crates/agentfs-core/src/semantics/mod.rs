//! Shared semantics facade under the transport adapters.
//!
//! M6 fills this module with the permission, durability, and handle-table
//! contracts that sit under the FUSE and NFS adapters. Access control lives
//! here so transport handlers cannot drift on POSIX mode-bit behavior.

pub mod access;
mod durability;

pub use durability::{AckDurability, Semantics, WriteReceipt};

#[cfg(test)]
mod tests {
    use super::{AckDurability, Semantics};
    use crate::config::{BatcherConfig, CoreConfig};
    use crate::{AgentFS, AgentFSOptions, FileSystem, DEFAULT_FILE_MODE, S_IFREG};
    use std::sync::Arc;
    use std::time::Duration;

    fn long_batch_window_config() -> CoreConfig {
        CoreConfig {
            batcher: BatcherConfig {
                enabled: true,
                window: Duration::from_secs(60),
                inode_bytes: 1024 * 1024,
                global_bytes: 16 * 1024 * 1024,
                txn_max_inodes: 1024,
                txn_max_bytes: 16 * 1024 * 1024,
            },
            overlay_reads: true,
            ..CoreConfig::default()
        }
    }

    #[tokio::test]
    async fn committed_write_and_inode_barrier_share_the_durability_contract(
    ) -> crate::error::Result<()> {
        let dir = tempfile::tempdir()?;
        let db_path = dir.path().join("semantics-durability.db");
        let agent = AgentFS::open(
            AgentFSOptions::with_path(db_path.to_str().expect("test DB path is UTF-8"))
                .with_core_config(long_batch_window_config()),
        )
        .await?;
        let fs: Arc<dyn FileSystem> = Arc::new(agent.fs);
        let semantics = Semantics::new(fs.clone());

        let (stats, file) = fs
            .create_file(1, "committed.txt", S_IFREG | 0o644, 0, 0)
            .await?;
        let committed = semantics
            .write(&file, 0, b"committed bytes", AckDurability::Committed)
            .await?;
        assert_eq!(committed.durability, AckDurability::Committed);
        assert_eq!(committed.count, "committed bytes".len());
        let attr = semantics
            .stat_coherent(stats.ino)
            .await?
            .expect("committed file should have attrs");
        assert_eq!(attr.size, "committed bytes".len() as i64);

        let (volatile_stats, volatile_file) = fs
            .create_file(1, "volatile.txt", DEFAULT_FILE_MODE, 0, 0)
            .await?;
        let volatile = semantics
            .write(&volatile_file, 0, b"barrier bytes", AckDurability::Volatile)
            .await?;
        assert_eq!(volatile.durability, AckDurability::Volatile);
        assert_eq!(volatile.count, "barrier bytes".len());
        let attr = semantics
            .stat_coherent(volatile_stats.ino)
            .await?
            .expect("volatile write should be visible to coherent attrs");
        assert_eq!(attr.size, "barrier bytes".len() as i64);
        semantics.commit_barrier(Some(volatile_stats.ino)).await?;

        drop(file);
        drop(volatile_file);
        drop(semantics);
        drop(fs);

        let reopened = AgentFS::open(AgentFSOptions::with_path(
            db_path.to_str().expect("test DB path is UTF-8"),
        ))
        .await?;
        let committed_stats = reopened
            .fs
            .lookup(1, "committed.txt")
            .await?
            .expect("committed file should reopen");
        let committed_file =
            FileSystem::open(&reopened.fs, committed_stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            committed_file.pread(0, 64).await?,
            b"committed bytes",
            "Committed writes must survive reopening without a timer drain"
        );
        let volatile_stats = reopened
            .fs
            .lookup(1, "volatile.txt")
            .await?
            .expect("barrier file should reopen");
        let volatile_file =
            FileSystem::open(&reopened.fs, volatile_stats.ino, libc::O_RDONLY).await?;
        assert_eq!(
            volatile_file.pread(0, 64).await?,
            b"barrier bytes",
            "commit_barrier(Some(ino)) must make a volatile write durable"
        );
        Ok(())
    }
}
