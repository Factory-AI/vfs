use std::time::Duration;

use crate::filesystem::{PartialOriginMode, PartialOriginPolicy};

use super::EnvReader;

pub const DEFAULT_CHUNK_SIZE: usize = 65_536;
pub const DEFAULT_INLINE_THRESHOLD: usize = 16_384;
pub const DEFAULT_WRITE_BATCH_MS: u64 = 5;
pub const DEFAULT_WRITE_BATCH_BYTES: usize = 4 * 1024 * 1024;
pub const DEFAULT_WRITE_BATCH_GLOBAL_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_WRITE_BATCH_TXN_INODES: usize = 1024;
pub const DEFAULT_WRITE_BATCH_TXN_BYTES: usize = 32 * 1024 * 1024;

const WRITE_BATCHER_MS_ENV: &str = "AGENTFS_BATCH_MS";
const WRITE_BATCHER_BYTES_ENV: &str = "AGENTFS_BATCH_BYTES";
const WRITE_BATCHER_GLOBAL_BYTES_ENV: &str = "AGENTFS_BATCH_GLOBAL_BYTES";
const WRITE_BATCHER_TXN_INODES_ENV: &str = "AGENTFS_BATCH_TXN_INODES";
const WRITE_BATCHER_TXN_BYTES_ENV: &str = "AGENTFS_BATCH_TXN_BYTES";
const OVERLAY_READS_ENV: &str = "AGENTFS_OVERLAY_READS";
const DRAIN_ON_SETATTR_ENV: &str = "AGENTFS_DRAIN_ON_SETATTR";
const KEEPCACHE_DELTA_ENV: &str = "AGENTFS_KEEPCACHE_DELTA";
const PARTIAL_ORIGIN_ENV: &str = "AGENTFS_OVERLAY_PARTIAL_ORIGIN";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub chunk_size: usize,
    pub inline_threshold: usize,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            inline_threshold: DEFAULT_INLINE_THRESHOLD,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatcherConfig {
    pub enabled: bool,
    pub window: Duration,
    pub inode_bytes: usize,
    pub global_bytes: usize,
    pub txn_max_inodes: usize,
    pub txn_max_bytes: usize,
}

impl Default for BatcherConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window: Duration::from_millis(DEFAULT_WRITE_BATCH_MS),
            inode_bytes: DEFAULT_WRITE_BATCH_BYTES,
            global_bytes: DEFAULT_WRITE_BATCH_GLOBAL_BYTES,
            txn_max_inodes: DEFAULT_WRITE_BATCH_TXN_INODES,
            txn_max_bytes: DEFAULT_WRITE_BATCH_TXN_BYTES,
        }
    }
}

impl BatcherConfig {
    pub fn from_env(reader: EnvReader) -> Self {
        let default = Self::default();
        Self {
            enabled: default.enabled,
            window: reader.duration_millis(WRITE_BATCHER_MS_ENV, DEFAULT_WRITE_BATCH_MS),
            inode_bytes: reader.positive_usize(WRITE_BATCHER_BYTES_ENV, default.inode_bytes),
            global_bytes: reader
                .positive_usize(WRITE_BATCHER_GLOBAL_BYTES_ENV, default.global_bytes),
            txn_max_inodes: reader
                .positive_usize(WRITE_BATCHER_TXN_INODES_ENV, default.txn_max_inodes)
                .max(1),
            txn_max_bytes: reader
                .positive_usize(WRITE_BATCHER_TXN_BYTES_ENV, default.txn_max_bytes)
                .max(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreConfig {
    pub geometry: Geometry,
    pub batcher: BatcherConfig,
    pub overlay_reads: bool,
    pub drain_on_setattr: bool,
    pub keepcache_delta: bool,
    pub partial_origin: PartialOriginPolicy,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            geometry: Geometry::default(),
            batcher: BatcherConfig::default(),
            overlay_reads: true,
            drain_on_setattr: true,
            keepcache_delta: true,
            partial_origin: PartialOriginPolicy::default(),
        }
    }
}

impl CoreConfig {
    pub fn from_env() -> Self {
        let reader = EnvReader::new();
        let default = Self::default();
        Self {
            geometry: default.geometry,
            batcher: BatcherConfig::from_env(reader),
            overlay_reads: reader.bool(OVERLAY_READS_ENV, default.overlay_reads),
            drain_on_setattr: reader.bool(DRAIN_ON_SETATTR_ENV, default.drain_on_setattr),
            keepcache_delta: reader.bool(KEEPCACHE_DELTA_ENV, default.keepcache_delta),
            partial_origin: if reader.bool(PARTIAL_ORIGIN_ENV, false) {
                PartialOriginPolicy::new(PartialOriginMode::On)
            } else {
                default.partial_origin
            },
        }
    }
}
