//! CLI-owned runtime config assembly.

use agentfs_sdk::{CoreConfig, EnvReader};

#[cfg(target_os = "linux")]
const FUSE_WRITEBACK_ENV: &str = "AGENTFS_FUSE_WRITEBACK";

pub(crate) fn core_config_from_env() -> CoreConfig {
    let mut config = CoreConfig::from_env();

    #[cfg(target_os = "linux")]
    {
        config.batcher.enabled = EnvReader::new().bool(FUSE_WRITEBACK_ENV, config.batcher.enabled);
    }

    config
}
