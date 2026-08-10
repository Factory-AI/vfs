use super::*;

#[test]
fn default_allowed_dirs_is_the_cross_platform_superset() {
    // The per-platform lists diverged one agent-tool fix at a time; pin the
    // unified superset so a new entry lands everywhere at once.
    let expected = [
        ".amp",
        ".bun",
        ".cache",
        ".claude",
        ".claude.json",
        ".codex",
        ".gemini",
        ".config",
        ".local",
        ".npm",
    ];
    for entry in expected {
        assert!(
            DEFAULT_ALLOWED_DIRS.contains(&entry),
            "DEFAULT_ALLOWED_DIRS lost {entry}"
        );
    }
    assert_eq!(DEFAULT_ALLOWED_DIRS.len(), expected.len());

    let mut sorted = DEFAULT_ALLOWED_DIRS.to_vec();
    sorted.sort_unstable();
    assert_eq!(
        DEFAULT_ALLOWED_DIRS,
        sorted.as_slice(),
        "keep the list sorted"
    );
}

#[test]
fn default_allowed_paths_keeps_only_existing_entries() {
    let home = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(home.path().join(".cache")).unwrap();
    std::fs::create_dir(home.path().join(".codex")).unwrap();
    std::fs::write(home.path().join(".claude.json"), b"{}").unwrap();

    let paths = default_allowed_paths(home.path());

    assert_eq!(
        paths,
        vec![
            home.path().join(".cache"),
            home.path().join(".claude.json"),
            home.path().join(".codex"),
        ]
    );
}

#[test]
fn externally_materialized_session_needs_only_database_and_base_path() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let base = root.path().join("checkout");
    let session_dir = home.join(".vfs/run/external-session");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("delta.db"), b"packed database").unwrap();
    std::fs::write(
        session_dir.join("base_path"),
        base.to_string_lossy().as_bytes(),
    )
    .unwrap();

    let prepared =
        prepare_session(&home, "external-session".to_string(), root.path(), false).unwrap();

    assert_eq!(prepared.base_path, base);
    assert_eq!(prepared.start_state, StartState::Stopped);
    assert!(prepared.paths.mountpoint.is_dir());
    assert!(session_dir.join(".session.lock").is_file());
}

#[test]
fn stale_lock_and_proc_artifacts_are_recovered_lazily() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let base = root.path().join("checkout");
    let session_dir = home.join(".vfs/run/stale-session");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(session_dir.join("procs")).unwrap();
    std::fs::write(session_dir.join("delta.db"), b"packed database").unwrap();
    std::fs::write(
        session_dir.join("base_path"),
        base.to_string_lossy().as_bytes(),
    )
    .unwrap();
    std::fs::write(session_dir.join(".session.lock"), b"stale inode").unwrap();
    std::fs::write(session_dir.join("procs/999999.json"), b"stale proc").unwrap();
    std::fs::write(session_dir.join("runtime-status.json.tmp"), b"partial").unwrap();

    let prepared = prepare_session(&home, "stale-session".to_string(), root.path(), false).unwrap();

    assert_eq!(prepared.start_state, StartState::StaleRecovered);
    assert!(!session_dir.join("procs").exists());
    assert!(!session_dir.join("runtime-status.json.tmp").exists());
}

#[test]
fn starting_session_without_a_live_mount_uses_the_shared_pack_error() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let base = root.path().join("checkout");
    let session_dir = home.join(".vfs/run/live-session");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("delta.db"), b"packed database").unwrap();
    std::fs::write(
        session_dir.join("base_path"),
        base.to_string_lossy().as_bytes(),
    )
    .unwrap();
    let _starting = crate::cmd::session_lock::SessionLock::try_shared(&session_dir).unwrap();

    let error = match prepare_session(&home, "live-session".to_string(), root.path(), false) {
        Ok(_) => panic!("a starting session without a live mount must conflict"),
        Err(error) => error,
    };
    assert!(error
        .downcast_ref::<crate::cmd::pack::SessionStillRunning>()
        .is_some());
}

#[test]
fn malformed_external_session_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let base = root.path().join("checkout");
    let session_dir = home.join(".vfs/run/malformed-session");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("base_path"),
        base.to_string_lossy().as_bytes(),
    )
    .unwrap();

    let error = match prepare_session(&home, "malformed-session".to_string(), root.path(), false) {
        Ok(_) => panic!("missing delta.db must be rejected"),
        Err(error) => error,
    };
    assert!(error.downcast_ref::<InvalidRunSession>().is_some());
}

#[test]
fn interrupted_first_start_can_retry_before_database_creation() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let base = root.path().join("checkout");
    let session_dir = home.join(".vfs/run/retry-first-start");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("base_path"),
        base.to_string_lossy().as_bytes(),
    )
    .unwrap();
    std::fs::write(session_dir.join(".session.lock"), b"").unwrap();

    let prepared =
        prepare_session(&home, "retry-first-start".to_string(), root.path(), false).unwrap();

    assert_eq!(prepared.base_path, base);
    assert!(prepared.paths.mountpoint.is_dir());
}

#[test]
fn interrupted_pack_publication_recovers_before_database_validation() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let base = root.path().join("checkout");
    let session_dir = home.join(".vfs/run/recover-pack");
    std::fs::create_dir_all(&base).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("base_path"),
        base.to_string_lossy().as_bytes(),
    )
    .unwrap();
    std::fs::write(session_dir.join(".delta.db.pack-backup"), b"recover me").unwrap();

    let prepared = prepare_session(&home, "recover-pack".to_string(), root.path(), false).unwrap();

    assert_eq!(prepared.base_path, base);
    assert_eq!(
        std::fs::read(session_dir.join("delta.db")).unwrap(),
        b"recover me"
    );
    assert!(!session_dir.join(".delta.db.pack-backup").exists());
}

#[test]
fn status_json_schema_is_stable() {
    let status = SessionStatus {
        session_id: "session-1".to_string(),
        state: SessionState::StaleRecovered,
        mounted: false,
        pid: None,
        generation: 7,
        seeded: true,
    };

    assert_eq!(
        serde_json::to_value(status).unwrap(),
        serde_json::json!({
            "sessionId": "session-1",
            "state": "stale-recovered",
            "mounted": false,
            "pid": null,
            "generation": 7,
            "seeded": true,
        })
    );
}

#[test]
fn live_runtime_status_carries_locked_database_metadata() {
    let root = tempfile::tempdir().unwrap();
    let paths = SessionPaths::new(root.path(), "live-session");
    std::fs::create_dir_all(&paths.run_dir).unwrap();

    write_runtime_status(&paths.runtime_status_file, 9, true).unwrap();
    let status = read_runtime_status(&paths).unwrap().unwrap();

    assert_eq!(status.pid, std::process::id());
    assert_eq!(status.generation, 9);
    assert!(status.seeded);
}

#[test]
fn missing_runtime_status_represents_a_busy_non_run_operation() {
    let root = tempfile::tempdir().unwrap();
    let paths = SessionPaths::new(root.path(), "busy-session");

    assert!(read_runtime_status(&paths).unwrap().is_none());
    assert_eq!(
        serde_json::to_value(SessionState::Busy).unwrap(),
        serde_json::json!("busy")
    );
}

#[tokio::test]
async fn stopped_status_reads_encrypted_session_metadata() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("encrypted.db");
    let encryption = vfs_core::EncryptionConfig {
        hex_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        cipher: "aes256gcm".to_string(),
    };
    let vfs = vfs_core::Vfs::open(
        vfs_core::VfsOptions::with_path(db_path.to_string_lossy())
            .with_encryption(encryption.clone()),
    )
    .await
    .unwrap();
    vfs.increment_session_generation().await.unwrap();
    vfs.set_seeded_paths(&[]).await.unwrap();
    vfs.fs.finalize().await.unwrap();
    drop(vfs);

    let status = read_session_metadata(&db_path, Some(&encryption))
        .await
        .unwrap();

    assert_eq!(status.generation, 1);
    assert!(status.seeded);
}

#[cfg(target_os = "linux")]
mod read_scoping {
    use super::super::linux::{plan_read_scoping, ZonePlan};
    use std::path::PathBuf;

    fn zones() -> Vec<(PathBuf, bool)> {
        vec![
            (PathBuf::from("/home/user"), false),
            (PathBuf::from("/tmp"), true),
        ]
    }

    #[test]
    fn keeps_cwd_and_in_zone_allows_only() {
        let cwd = PathBuf::from("/home/user/project");
        let allowed = vec![
            PathBuf::from("/home/user/.claude"),
            PathBuf::from("/opt/tool"),
            PathBuf::from("/tmp/scratch"),
        ];

        let plans = plan_read_scoping(&zones(), &cwd, &allowed);

        assert_eq!(
            plans,
            vec![
                ZonePlan {
                    root: PathBuf::from("/home/user"),
                    world_writable: false,
                    keep: vec![
                        PathBuf::from("/home/user/.claude"),
                        PathBuf::from("/home/user/project"),
                    ],
                },
                ZonePlan {
                    root: PathBuf::from("/tmp"),
                    world_writable: true,
                    keep: vec![PathBuf::from("/tmp/scratch")],
                },
            ]
        );
    }

    #[test]
    fn allowed_paths_inside_cwd_are_covered_by_the_overlay() {
        let cwd = PathBuf::from("/home/user/project");
        let allowed = vec![PathBuf::from("/home/user/project/.cache")];

        let plans = plan_read_scoping(&zones(), &cwd, &allowed);

        assert_eq!(plans[0].keep, vec![PathBuf::from("/home/user/project")]);
    }

    #[test]
    fn zone_covered_by_cwd_is_skipped() {
        let cwd = PathBuf::from("/home/user");
        let allowed = vec![PathBuf::from("/home/user/.claude")];

        let plans = plan_read_scoping(&zones(), &cwd, &allowed);

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].root, PathBuf::from("/tmp"));
    }

    #[test]
    fn keep_orders_parents_before_children() {
        let cwd = PathBuf::from("/srv/work");
        let allowed = vec![
            PathBuf::from("/home/user/.local/share/tool"),
            PathBuf::from("/home/user/.local"),
        ];

        let plans = plan_read_scoping(&zones(), &cwd, &allowed);

        assert_eq!(
            plans[0].keep,
            vec![
                PathBuf::from("/home/user/.local"),
                PathBuf::from("/home/user/.local/share/tool"),
            ]
        );
    }

    #[test]
    fn sibling_lookalike_prefixes_are_not_treated_as_children() {
        let cwd = PathBuf::from("/home/user/project");
        let allowed = vec![PathBuf::from("/home/user/project-notes")];

        let plans = plan_read_scoping(&zones(), &cwd, &allowed);

        assert_eq!(
            plans[0].keep,
            vec![
                PathBuf::from("/home/user/project-notes"),
                PathBuf::from("/home/user/project"),
            ]
        );
    }
}

#[cfg(target_os = "macos")]
mod darwin_read_scoping {
    use super::super::darwin::{
        generate_sandbox_profile, SandboxConfig, SandboxProfile, PLATFORM_READ_ROOTS,
    };
    use std::path::{Path, PathBuf};

    fn config() -> SandboxConfig {
        SandboxConfig {
            mountpoint: PathBuf::from("/Users/tester/.vfs/run/sess-1/mnt"),
            allow_paths: vec![
                PathBuf::from("/Users/tester/.codex"),
                PathBuf::from("/Users/tester/.claude.json"),
            ],
            allow_network: true,
            session_id: "sess-1".to_string(),
        }
    }

    fn param_for<'a>(profile: &'a SandboxProfile, path: &str) -> &'a str {
        profile
            .params
            .iter()
            .find(|(_, value)| value == Path::new(path))
            .map(|(name, _)| name.as_str())
            .unwrap_or_else(|| panic!("no -D param defined for {path}"))
    }

    #[test]
    fn reads_are_default_deny_with_no_blanket_allow() {
        let profile = generate_sandbox_profile(&config());

        assert!(
            profile
                .policy
                .lines()
                .any(|line| line.starts_with("(deny default")),
            "profile must keep the deny-default posture"
        );
        assert!(
            profile
                .policy
                .lines()
                .all(|line| line.trim() != "(allow file-read*)"),
            "a bare (allow file-read*) reopens unscoped reads"
        );
    }

    #[test]
    fn platform_read_roots_are_all_present() {
        let profile = generate_sandbox_profile(&config());

        for root in PLATFORM_READ_ROOTS {
            let rule = format!(
                r#"(allow file-read* file-map-executable file-test-existence (subpath "{root}"))"#
            );
            assert!(
                profile.policy.contains(&rule),
                "missing platform read root {root}"
            );
        }
        assert!(
            profile
                .policy
                .contains(r#"(allow file-read* file-test-existence (literal "/"))"#),
            "getcwd needs a metadata-capable read of the root directory"
        );
        assert!(
            profile.policy.contains(
                r#"(require-all (subpath "/System") (require-not (subpath "/System/Volumes")))"#
            ),
            "/System must exclude the /System/Volumes firmlinks back into the data volume"
        );
    }

    #[test]
    fn session_and_allow_paths_expand_from_config_as_params() {
        let profile = generate_sandbox_profile(&config());

        for path in [
            "/Users/tester/.vfs/run/sess-1/mnt",
            "/Users/tester/.vfs/run/sess-1",
            "/Users/tester/.codex",
            "/Users/tester/.claude.json",
        ] {
            let name = param_for(&profile, path);
            let rule = format!(
                r#"(allow file-read* file-map-executable file-test-existence (subpath (param "{name}")))"#
            );
            assert!(
                profile.policy.contains(&rule),
                "missing read allow for {path}"
            );
        }
    }

    #[test]
    fn path_resolution_parents_get_metadata_only() {
        let profile = generate_sandbox_profile(&config());

        for parent in [
            "/Users",
            "/Users/tester",
            "/Users/tester/.vfs",
            "/Users/tester/.vfs/run",
            "/Users/tester/.vfs/run/sess-1",
        ] {
            let name = param_for(&profile, parent);
            let rule = format!(
                r#"(allow file-read-metadata file-test-existence (literal (param "{name}")))"#
            );
            assert!(
                profile.policy.contains(&rule),
                "missing metadata parent {parent}"
            );
        }
        let home_param = param_for(&profile, "/Users/tester");
        assert!(
            !profile
                .policy
                .contains(&format!(r#"(subpath (param "{home_param}"))"#)),
            "home outside the session/allow paths must stay data-unreadable"
        );
        for link in ["/etc", "/tmp", "/var", "/System/Volumes/Data"] {
            let rule =
                format!(r#"(allow file-read-metadata file-test-existence (literal "{link}"))"#);
            assert!(
                profile.policy.contains(&rule),
                "missing symlink metadata for {link}"
            );
        }
    }

    #[test]
    fn dyld_cryptex_chain_has_metadata_ancestors() {
        let profile = generate_sandbox_profile(&config());

        assert!(
            profile.policy.contains(
                r#"(allow file-read* file-map-executable file-test-existence (subpath "/System/Volumes/Preboot/Cryptexes"))"#
            ),
            "dyld shared cache cryptex must stay a data read root"
        );
        for ancestor in ["/System/Volumes", "/System/Volumes/Preboot"] {
            let rule =
                format!(r#"(allow file-read-metadata file-test-existence (literal "{ancestor}"))"#);
            assert!(
                profile.policy.contains(&rule),
                "path resolution to the cryptex root must be able to stat {ancestor}"
            );
        }
    }

    #[test]
    fn write_scoping_is_unchanged() {
        let profile = generate_sandbox_profile(&config());

        let mountpoint = param_for(&profile, "/Users/tester/.vfs/run/sess-1/mnt");
        let run_dir = param_for(&profile, "/Users/tester/.vfs/run/sess-1");
        let codex = param_for(&profile, "/Users/tester/.codex");
        for rule in [
            format!(r#"(allow file-write* (subpath (param "{mountpoint}")))"#),
            format!(r#"(allow file-write* (subpath (param "{run_dir}")))"#),
            format!(r#"(allow file-write* (subpath (param "{codex}")))"#),
            r#"(allow file-write* (subpath "/private/tmp"))"#.to_string(),
            r#"(allow file-write* (subpath "/tmp"))"#.to_string(),
            r#"(allow file-write* (subpath "/var/tmp"))"#.to_string(),
            r#"(allow file-write* (subpath "/private/var/folders"))"#.to_string(),
            r#"(allow file-write* (subpath "/dev"))"#.to_string(),
        ] {
            assert!(profile.policy.contains(&rule), "missing write rule {rule}");
        }
    }

    #[test]
    fn dynamic_paths_never_appear_in_the_policy_text() {
        let mut config = config();
        config.allow_paths.push(PathBuf::from(
            r#"/Users/tester/pwn") (allow file-read* (subpath "/"#,
        ));

        let profile = generate_sandbox_profile(&config);

        assert!(
            !profile.policy.contains("pwn"),
            "a user-controlled path leaked into the SBPL text:\n{}",
            profile.policy
        );
        // A dynamic path can only be interpolated raw as a quoted string, so
        // no quote in the policy may be followed by a /Users-rooted path
        // (static literals like /System/Volumes/Data/Users are fine).
        assert!(
            !profile.policy.contains(r#""/Users"#),
            "a session/allow/home path was interpolated instead of parameterized:\n{}",
            profile.policy
        );
        assert!(
            profile
                .params
                .iter()
                .any(|(_, value)| value.to_string_lossy().contains("pwn")),
            "the quote-bearing path must still be granted, via a -D param"
        );

        let mut names: Vec<&str> = profile.params.iter().map(|(n, _)| n.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            profile.params.len(),
            "sandbox-exec -D definitions must not repeat a param name"
        );
    }

    #[test]
    fn session_id_is_sanitized_in_the_policy_text() {
        let mut config = config();
        config.session_id = r#"evil")(allow file-read*)(deny signal "x"#.to_string();

        let profile = generate_sandbox_profile(&config);

        assert!(
            profile.policy.contains(
                r#"(deny default (with message "vfs-evilallowfile-readdenysignalx: access denied"))"#
            ),
            "session id must be reduced to a conservative charset:\n{}",
            profile.policy
        );
        assert!(
            profile
                .policy
                .lines()
                .all(|line| line.trim() != "(allow file-read*)"),
            "an injected session id must not open unscoped reads"
        );
    }

    #[test]
    fn fully_hostile_session_id_falls_back_to_a_fixed_log_tag() {
        let mut config = config();
        config.session_id = r#""()[]{}<>#;$!"#.to_string();

        let profile = generate_sandbox_profile(&config);

        assert!(
            profile
                .policy
                .contains(r#"(deny default (with message "vfs-session: access denied"))"#),
            "a session id sanitized to nothing must fall back to a fixed tag:\n{}",
            profile.policy
        );
        assert!(
            !profile.policy.contains("vfs-:"),
            "the log tag must never be empty:\n{}",
            profile.policy
        );
    }
}

#[cfg(target_os = "macos")]
mod darwin_spawn_exit_codes {
    use super::super::darwin::spawn_error_exit_code;
    use anyhow::Context;

    fn spawn_error(kind: std::io::ErrorKind) -> anyhow::Error {
        anyhow::Error::from(std::io::Error::new(kind, "spawn failed"))
    }

    #[test]
    fn missing_command_maps_to_127() {
        assert_eq!(
            spawn_error_exit_code(&spawn_error(std::io::ErrorKind::NotFound)),
            Some(127)
        );
    }

    #[test]
    fn non_executable_command_maps_to_126() {
        assert_eq!(
            spawn_error_exit_code(&spawn_error(std::io::ErrorKind::PermissionDenied)),
            Some(126)
        );
    }

    #[test]
    fn mapping_survives_anyhow_context_wrapping() {
        let error: anyhow::Error = Err::<(), _>(spawn_error(std::io::ErrorKind::NotFound))
            .context("Darwin/NFS run supervision failed for cmd")
            .unwrap_err();
        assert_eq!(spawn_error_exit_code(&error), Some(127));
    }

    #[test]
    fn other_errors_go_to_the_reporter() {
        assert_eq!(
            spawn_error_exit_code(&spawn_error(std::io::ErrorKind::BrokenPipe)),
            None
        );
        assert_eq!(spawn_error_exit_code(&anyhow::anyhow!("not io")), None);
    }
}

#[cfg(target_os = "macos")]
mod darwin_exec_preflight {
    use super::super::darwin::preflight_exec_exit_code;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn code(command: &Path, cwd: &Path) -> Option<i32> {
        preflight_exec_exit_code(command, cwd).map(|(code, _)| code)
    }

    #[test]
    fn missing_absolute_command_is_127() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            code(&dir.path().join("does-not-exist"), dir.path()),
            Some(127)
        );
    }

    #[test]
    fn present_non_executable_is_126() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("not-executable");
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(code(&target, dir.path()), Some(126));
    }

    #[test]
    fn executable_paths_pass() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(code(Path::new("/bin/sh"), dir.path()), None);
    }

    #[test]
    fn bare_names_search_path() {
        let dir = tempfile::tempdir().unwrap();
        // `sh` resolves through PATH on every runner; a name that cannot
        // exist anywhere on PATH is not found.
        assert_eq!(code(Path::new("sh"), dir.path()), None);
        assert_eq!(
            code(Path::new("vfs-preflight-test-missing-cmd"), dir.path()),
            Some(127)
        );
    }

    #[test]
    fn relative_paths_resolve_against_the_child_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("tool.sh");
        std::fs::write(&target, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(code(Path::new("./tool.sh"), dir.path()), None);
        assert_eq!(code(Path::new("./absent.sh"), dir.path()), Some(127));
    }
}

#[cfg(target_os = "linux")]
mod skip_mount {
    use super::super::linux::skip_mount;
    use std::path::Path;

    #[test]
    fn matches_virtual_fs_roots_and_descendants() {
        for path in ["/proc", "/sys/kernel", "/dev", "/dev/shm", "/tmp/x"] {
            assert!(skip_mount(Path::new(path)), "{path} should be skipped");
        }
    }

    #[test]
    fn sibling_lookalike_prefixes_are_remounted() {
        for path in ["/devfoo", "/tmpfoo", "/procfs", "/system", "/data/tmp"] {
            assert!(
                !skip_mount(Path::new(path)),
                "{path} must not be skipped by the ro-remount pass"
            );
        }
    }
}

#[test]
fn group_paths_by_parent_uses_brace_expansion() {
    let paths = vec![
        PathBuf::from("/home/user/.claude"),
        PathBuf::from("/home/user/.claude.json"),
        PathBuf::from("/home/user/.codex"),
        PathBuf::from("/opt/tool"),
    ];

    assert_eq!(
        group_paths_by_parent(&paths),
        vec![
            "/home/user/{.claude, .claude.json, .codex}".to_string(),
            "/opt/tool".to_string(),
        ]
    );
}
