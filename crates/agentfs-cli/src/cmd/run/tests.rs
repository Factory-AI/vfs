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
