//! CLI-owned runtime config assembly.

use agentfs_core::{CoreConfig, EnvReader};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const CLONE_TIMINGS_ENV: &str = "AGENTFS_CLONE_TIMINGS";
const SHELL_ENV: &str = "SHELL";
const TURSO_AUTH_TOKEN_ENV: &str = "TURSO_DB_AUTH_TOKEN";

#[cfg(target_os = "linux")]
const FUSE_WRITEBACK_ENV: &str = "AGENTFS_FUSE_WRITEBACK";

pub(crate) const DEFAULT_CLONE_TIMINGS_ENABLED: bool = false;

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

pub(crate) fn turso_db_auth_token() -> Option<String> {
    EnvReader::new().string(TURSO_AUTH_TOKEN_ENV)
}

// ─── private sort-spill dir ──────────────────────────────────────────────────
//
// turso_core 0.5.3 creates `tursodb-ephemeral-*` sort-spill files in the
// ambient temp dir and never unlinks them (vdbe/execute.rs:10096), so every
// sort-heavy query litters the host. Until the upstream fix lands, the CLI
// points TMPDIR at a per-process directory removed on exit. Child processes
// of `run`/`exec` must NOT inherit that override: the snapshot below is
// restored into their environment by the spawn paths.

const SPILL_DIR_PREFIX: &str = "agentfs-spill-";

struct SpillDir {
    owner_pid: u32,
    path: PathBuf,
}

/// The user's TMPDIR as it was before the CLI overrode it (`None` inside the
/// option = TMPDIR was unset). Outer `None` = the override never ran.
static ORIGINAL_TMPDIR: OnceLock<Option<OsString>> = OnceLock::new();
static SPILL_DIR: Mutex<Option<SpillDir>> = Mutex::new(None);

/// Point TMPDIR at a fresh per-process spill dir, removed again at process
/// exit (including the `process::exit` child-status passthrough sites, via
/// `atexit`). Called once at CLI startup, before any threads exist.
pub fn init_private_spill_dir() {
    let original = std::env::var_os("TMPDIR");
    let _ = ORIGINAL_TMPDIR.set(original);
    reap_stale_spill_dirs(&std::env::temp_dir());
    adopt_private_spill_dir();

    static ATEXIT: std::sync::Once = std::sync::Once::new();
    ATEXIT.call_once(|| {
        extern "C" fn cleanup_spill_dir_at_exit() {
            remove_spill_dir_if_owner();
        }
        // SAFETY: registering a handler that only touches process-global
        // statics; atexit itself has no preconditions.
        unsafe {
            libc::atexit(cleanup_spill_dir_at_exit);
        }
    });
}

/// Give a daemonized child its own spill dir. After `fork()` the child shares
/// the parent's TMPDIR override, but the parent removes that directory when
/// it exits; the daemon must move off it before doing any database work.
pub(crate) fn adopt_private_spill_dir() {
    if ORIGINAL_TMPDIR.get().is_none() {
        return;
    }
    let base = original_temp_dir();
    let pid = std::process::id();
    for attempt in 0u32..16 {
        let dir = base.join(format!("{SPILL_DIR_PREFIX}{pid}-{attempt}"));
        match std::fs::create_dir(&dir) {
            Ok(()) => {
                std::env::set_var("TMPDIR", &dir);
                *SPILL_DIR.lock().unwrap() = Some(SpillDir {
                    owner_pid: pid,
                    path: dir,
                });
                return;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                tracing::debug!(%err, "could not create private spill dir; keeping ambient TMPDIR");
                return;
            }
        }
    }
}

/// The temp dir the user actually configured, independent of the override.
fn original_temp_dir() -> PathBuf {
    match ORIGINAL_TMPDIR.get() {
        Some(Some(dir)) if !dir.is_empty() => PathBuf::from(dir),
        Some(None) => PathBuf::from("/tmp"),
        _ => std::env::temp_dir(),
    }
}

/// Restore the user's TMPDIR into a child about to be spawned.
pub(crate) fn restore_original_tmpdir(command: &mut tokio::process::Command) {
    match ORIGINAL_TMPDIR.get() {
        Some(Some(original)) => {
            command.env("TMPDIR", original);
        }
        Some(None) => {
            command.env_remove("TMPDIR");
        }
        None => {}
    }
}

/// Restore the user's TMPDIR into this process's own environment. Used by the
/// `run` sandbox child between `fork()` and `execvp()`.
#[cfg(target_os = "linux")]
pub(crate) fn restore_original_tmpdir_env() {
    match ORIGINAL_TMPDIR.get() {
        Some(Some(original)) => std::env::set_var("TMPDIR", original),
        Some(None) => std::env::remove_var("TMPDIR"),
        None => {}
    }
}

fn remove_spill_dir_if_owner() {
    let Ok(guard) = SPILL_DIR.lock() else {
        return;
    };
    if let Some(spill) = guard.as_ref() {
        // Forked children (sandbox child, daemon parent/child pairs) inherit
        // this state; only the process that created the dir removes it.
        if spill.owner_pid == std::process::id() {
            let _ = std::fs::remove_dir_all(&spill.path);
        }
    }
}

/// Best-effort reaping of spill dirs left behind by SIGKILLed processes:
/// the exit handler never ran for them, so their PID-stamped dirs survive.
fn reap_stale_spill_dirs(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = stale_spill_dir_pid(&name.to_string_lossy()) else {
            continue;
        };
        if pid != std::process::id() && !process_alive(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Parse the owner PID out of a `agentfs-spill-<pid>-<n>` directory name.
fn stale_spill_dir_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(SPILL_DIR_PREFIX)?;
    let (pid, suffix) = rest.split_once('-')?;
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    pid.parse().ok()
}

fn process_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only performs the existence/permission check.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

    #[test]
    fn stale_spill_dir_pid_parses_only_generated_names() {
        assert_eq!(stale_spill_dir_pid("agentfs-spill-1234-0"), Some(1234));
        assert_eq!(stale_spill_dir_pid("agentfs-spill-1234-15"), Some(1234));
        assert_eq!(stale_spill_dir_pid("agentfs-spill-1234"), None);
        assert_eq!(stale_spill_dir_pid("agentfs-spill-abc-0"), None);
        assert_eq!(stale_spill_dir_pid("agentfs-spill-1234-x"), None);
        assert_eq!(stale_spill_dir_pid("agentfs-spill--0"), None);
        assert_eq!(stale_spill_dir_pid("tursodb-ephemeral-1"), None);
    }

    #[test]
    fn reap_stale_spill_dirs_removes_only_dead_owners() {
        let base = tempfile::tempdir().unwrap();
        // Beyond the kernel's maximum pid_max (4194304), so guaranteed dead.
        let dead = base.path().join(format!("{SPILL_DIR_PREFIX}99999999-0"));
        let alive = base
            .path()
            .join(format!("{SPILL_DIR_PREFIX}{}-0", std::process::id()));
        let unrelated = base.path().join("agentfs-spill-notes");
        std::fs::create_dir(&dead).unwrap();
        std::fs::create_dir(&alive).unwrap();
        std::fs::create_dir(&unrelated).unwrap();

        reap_stale_spill_dirs(base.path());

        assert!(!dead.exists(), "dead-owner spill dir must be reaped");
        assert!(alive.exists(), "live-owner spill dir must survive");
        assert!(unrelated.exists(), "non-matching names must survive");
    }

    #[test]
    fn private_spill_dir_redirects_temp_dir_and_children_keep_the_original() {
        let _lock = ENV_LOCK.lock().unwrap();
        let ambient = std::env::var_os("TMPDIR");

        init_private_spill_dir();

        let spill = PathBuf::from(std::env::var_os("TMPDIR").expect("TMPDIR overridden"));
        assert!(
            spill
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(SPILL_DIR_PREFIX),
            "override points at a spill dir, got {}",
            spill.display()
        );
        assert!(spill.is_dir(), "spill dir exists");
        assert_eq!(
            std::env::temp_dir(),
            spill,
            "temp_dir() follows the override"
        );

        let mut cmd = tokio::process::Command::new("true");
        restore_original_tmpdir(&mut cmd);
        let configured: Vec<_> = cmd.as_std().get_envs().collect();
        let expected = ORIGINAL_TMPDIR
            .get()
            .expect("snapshot latched by init")
            .as_deref();
        assert!(
            configured.contains(&(std::ffi::OsStr::new("TMPDIR"), expected)),
            "child command must see the pre-override TMPDIR ({expected:?}), got {configured:?}"
        );

        match ambient {
            Some(value) => std::env::set_var("TMPDIR", value),
            None => std::env::remove_var("TMPDIR"),
        }
    }
}
