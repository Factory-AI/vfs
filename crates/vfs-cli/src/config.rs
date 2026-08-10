//! CLI-owned runtime config assembly.

use vfs_core::{CoreConfig, EnvReader};

use crate::cmd::remote::RemoteConfig;

const CLONE_TIMINGS_ENV: &str = "VFS_CLONE_TIMINGS";
const REMOTE_CONCURRENCY_ENV: &str = "VFS_REMOTE_CONCURRENCY";
const REMOTE_STREAM_INTERVAL_MS_ENV: &str = "VFS_REMOTE_STREAM_INTERVAL_MS";
const REMOTE_URL_ENV: &str = "VFS_REMOTE_URL";
const SHELL_ENV: &str = "SHELL";

#[cfg(target_os = "linux")]
const FUSE_WRITEBACK_ENV: &str = "VFS_FUSE_WRITEBACK";

pub(crate) const DEFAULT_CLONE_TIMINGS_ENABLED: bool = false;
pub(crate) const DEFAULT_REMOTE_CONCURRENCY: usize = 4;
pub(crate) const DEFAULT_REMOTE_STREAM_INTERVAL_MS: u64 = 5_000;

pub(crate) fn core_config_from_env() -> CoreConfig {
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
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

/// PATH as a spawned child inherits it, for execvp-style spawn preflight.
/// The fallback mirrors execvp's behavior when PATH is unset.
#[cfg(target_os = "macos")]
pub(crate) fn host_path_var() -> std::ffi::OsString {
    std::env::var_os("PATH").unwrap_or_else(|| std::ffi::OsString::from("/usr/bin:/bin"))
}

pub fn remote_config() -> Option<RemoteConfig> {
    let reader = EnvReader::new();
    let url = reader.string(REMOTE_URL_ENV)?;
    let concurrency = remote_concurrency_with_reader(&reader);
    let stream_interval_ms = reader.string(REMOTE_STREAM_INTERVAL_MS_ENV).map_or(
        DEFAULT_REMOTE_STREAM_INTERVAL_MS,
        |value| {
            value.parse::<u64>().unwrap_or_else(|_| {
                tracing::warn!(
                    "Ignoring invalid {}={:?}; using default {}",
                    REMOTE_STREAM_INTERVAL_MS_ENV,
                    value,
                    DEFAULT_REMOTE_STREAM_INTERVAL_MS
                );
                DEFAULT_REMOTE_STREAM_INTERVAL_MS
            })
        },
    );

    Some(RemoteConfig {
        url,
        concurrency,
        stream_interval_ms,
    })
}

pub(crate) fn remote_concurrency() -> usize {
    remote_concurrency_with_reader(&EnvReader::new())
}

fn remote_concurrency_with_reader(reader: &EnvReader) -> usize {
    reader
        .string(REMOTE_CONCURRENCY_ENV)
        .map_or(DEFAULT_REMOTE_CONCURRENCY, |value| {
            value
                .parse::<usize>()
                .ok()
                .filter(|value| *value >= 1)
                .unwrap_or_else(|| {
                    tracing::warn!(
                        "Ignoring invalid {}={:?}; using default {}",
                        REMOTE_CONCURRENCY_ENV,
                        value,
                        DEFAULT_REMOTE_CONCURRENCY
                    );
                    DEFAULT_REMOTE_CONCURRENCY
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const CONFIG_ENV_KEYS: &[&str] = &[
        CLONE_TIMINGS_ENV,
        REMOTE_CONCURRENCY_ENV,
        REMOTE_STREAM_INTERVAL_MS_ENV,
        REMOTE_URL_ENV,
        SHELL_ENV,
    ];

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

    #[test]
    fn remote_config_is_absent_until_url_opts_in() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(CONFIG_ENV_KEYS);

        assert_eq!(remote_config(), None);
        std::env::set_var(REMOTE_CONCURRENCY_ENV, "12");
        assert_eq!(remote_config(), None);
        assert_eq!(remote_concurrency(), 12);
    }

    #[test]
    fn remote_config_uses_typed_defaults() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(CONFIG_ENV_KEYS);
        std::env::set_var(REMOTE_URL_ENV, "file:///tmp/vfs-remote");

        assert_eq!(
            remote_config(),
            Some(RemoteConfig {
                url: "file:///tmp/vfs-remote".to_string(),
                concurrency: DEFAULT_REMOTE_CONCURRENCY,
                stream_interval_ms: DEFAULT_REMOTE_STREAM_INTERVAL_MS,
            })
        );

        std::env::set_var(REMOTE_CONCURRENCY_ENV, "12");
        std::env::set_var(REMOTE_STREAM_INTERVAL_MS_ENV, "0");
        assert_eq!(
            remote_config(),
            Some(RemoteConfig {
                url: "file:///tmp/vfs-remote".to_string(),
                concurrency: 12,
                stream_interval_ms: 0,
            })
        );
    }

    #[test]
    fn remote_config_rejects_zero_concurrency() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _snapshot = EnvSnapshot::capture(CONFIG_ENV_KEYS);
        std::env::set_var(REMOTE_URL_ENV, "s3://checkpoints/vfs");
        std::env::set_var(REMOTE_CONCURRENCY_ENV, "0");

        assert_eq!(
            remote_config().unwrap().concurrency,
            DEFAULT_REMOTE_CONCURRENCY
        );
    }
}
