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
            mountpoint: PathBuf::from("/Users/tester/.agentfs/run/sess-1/mnt"),
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
            "/Users/tester/.agentfs/run/sess-1/mnt",
            "/Users/tester/.agentfs/run/sess-1",
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
            "/Users/tester/.agentfs",
            "/Users/tester/.agentfs/run",
            "/Users/tester/.agentfs/run/sess-1",
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

        let mountpoint = param_for(&profile, "/Users/tester/.agentfs/run/sess-1/mnt");
        let run_dir = param_for(&profile, "/Users/tester/.agentfs/run/sess-1");
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
                r#"(deny default (with message "agentfs-evilallowfile-readdenysignalx: access denied"))"#
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
                .contains(r#"(deny default (with message "agentfs-session: access denied"))"#),
            "a session id sanitized to nothing must fall back to a fixed tag:\n{}",
            profile.policy
        );
        assert!(
            !profile.policy.contains("agentfs-:"),
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
