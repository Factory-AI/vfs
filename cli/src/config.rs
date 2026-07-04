//! CLI-owned runtime config assembly.

use agentfs_core::{CoreConfig, EnvReader};

const CLONE_TIMINGS_ENV: &str = "AGENTFS_CLONE_TIMINGS";
const SHELL_ENV: &str = "SHELL";
const TURSO_AUTH_TOKEN_ENV: &str = "TURSO_DB_AUTH_TOKEN";

#[cfg(target_os = "linux")]
const FUSE_WRITEBACK_ENV: &str = "AGENTFS_FUSE_WRITEBACK";

pub(crate) const DEFAULT_CLONE_TIMINGS_ENABLED: bool = false;

pub(crate) fn core_config_from_env() -> CoreConfig {
    let mut config = CoreConfig::from_env();

    #[cfg(target_os = "linux")]
    {
        config.batcher.enabled = EnvReader::new().bool(FUSE_WRITEBACK_ENV, config.batcher.enabled);
    }

    config
}

pub(crate) fn clone_timings_enabled() -> bool {
    EnvReader::new()
        .string(CLONE_TIMINGS_ENV)
        .map_or(DEFAULT_CLONE_TIMINGS_ENABLED, |value| value == "1")
}

pub(crate) fn current_shell_path() -> Option<String> {
    EnvReader::new().string(SHELL_ENV)
}

pub(crate) fn turso_db_auth_token() -> Option<String> {
    EnvReader::new().string(TURSO_AUTH_TOKEN_ENV)
}
