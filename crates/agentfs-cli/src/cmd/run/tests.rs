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
    use super::super::darwin::{generate_sandbox_profile, SandboxConfig, PLATFORM_READ_ROOTS};
    use std::path::PathBuf;

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

    #[test]
    fn reads_are_default_deny_with_no_blanket_allow() {
        let profile = generate_sandbox_profile(&config());

        assert!(
            profile
                .lines()
                .any(|line| line.starts_with("(deny default")),
            "profile must keep the deny-default posture"
        );
        assert!(
            profile
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
            assert!(profile.contains(&rule), "missing platform read root {root}");
        }
        assert!(
            profile.contains(r#"(allow file-read* file-test-existence (literal "/"))"#),
            "getcwd needs a metadata-capable read of the root directory"
        );
        assert!(
            profile.contains(
                r#"(require-all (subpath "/System") (require-not (subpath "/System/Volumes")))"#
            ),
            "/System must exclude the /System/Volumes firmlinks back into the data volume"
        );
    }

    #[test]
    fn session_and_allow_paths_expand_from_config() {
        let profile = generate_sandbox_profile(&config());

        for path in [
            "/Users/tester/.agentfs/run/sess-1/mnt",
            "/Users/tester/.agentfs/run/sess-1",
            "/Users/tester/.codex",
            "/Users/tester/.claude.json",
        ] {
            let rule = format!(
                r#"(allow file-read* file-map-executable file-test-existence (subpath "{path}"))"#
            );
            assert!(profile.contains(&rule), "missing read allow for {path}");
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
            let rule =
                format!(r#"(allow file-read-metadata file-test-existence (literal "{parent}"))"#);
            assert!(profile.contains(&rule), "missing metadata parent {parent}");
        }
        assert!(
            !profile.contains(r#"(subpath "/Users/tester")"#),
            "home outside the session/allow paths must stay data-unreadable"
        );
        for link in ["/etc", "/tmp", "/var", "/System/Volumes/Data"] {
            let rule =
                format!(r#"(allow file-read-metadata file-test-existence (literal "{link}"))"#);
            assert!(
                profile.contains(&rule),
                "missing symlink metadata for {link}"
            );
        }
    }

    #[test]
    fn write_scoping_is_unchanged() {
        let profile = generate_sandbox_profile(&config());

        for rule in [
            r#"(allow file-write* (subpath "/Users/tester/.agentfs/run/sess-1/mnt"))"#,
            r#"(allow file-write* (subpath "/Users/tester/.agentfs/run/sess-1"))"#,
            r#"(allow file-write* (subpath "/Users/tester/.codex"))"#,
            r#"(allow file-write* (subpath "/private/tmp"))"#,
            r#"(allow file-write* (subpath "/tmp"))"#,
            r#"(allow file-write* (subpath "/var/tmp"))"#,
            r#"(allow file-write* (subpath "/private/var/folders"))"#,
            r#"(allow file-write* (subpath "/dev"))"#,
        ] {
            assert!(profile.contains(rule), "missing write rule {rule}");
        }
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
