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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const CONFIG_ENV_KEYS: &[&str] = &[CLONE_TIMINGS_ENV, SHELL_ENV, TURSO_AUTH_TOKEN_ENV];

    struct EnvSnapshot {
        values: Vec<(&'static str, Option<String>)>,
    }

    impl EnvSnapshot {
        fn capture(keys: &[&'static str]) -> Self {
            let values = keys
                .iter()
                .map(|key| (*key, std::env::var(key).ok()))
                .collect();
            for key in keys {
                std::env::remove_var(key);
            }
            Self { values }
        }
    }

    impl Drop for EnvSnapshot {
        fn drop(&mut self) {
            for (key, value) in &self.values {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn turso_db_auth_token_reads_env_at_cli_edge() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(CONFIG_ENV_KEYS);

        assert_eq!(turso_db_auth_token(), None);

        std::env::set_var(TURSO_AUTH_TOKEN_ENV, "test-token");
        assert_eq!(turso_db_auth_token().as_deref(), Some("test-token"));
    }

    #[test]
    fn clone_timings_enabled_reads_explicit_one_only() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(CONFIG_ENV_KEYS);

        assert!(!clone_timings_enabled());

        std::env::set_var(CLONE_TIMINGS_ENV, "1");
        assert!(clone_timings_enabled());

        std::env::set_var(CLONE_TIMINGS_ENV, "true");
        assert!(!clone_timings_enabled());
    }

    #[test]
    fn current_shell_path_reads_env_at_cli_edge() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(CONFIG_ENV_KEYS);

        assert_eq!(current_shell_path(), None);

        std::env::set_var(SHELL_ENV, "/bin/test-shell");
        assert_eq!(current_shell_path().as_deref(), Some("/bin/test-shell"));
    }
}
