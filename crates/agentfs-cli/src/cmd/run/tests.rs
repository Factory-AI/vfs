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
