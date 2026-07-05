use std::path::Path;
use std::process::Command;

fn main() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agentfs-cli should live under crates/agentfs-cli");

    // Capture git version from tags for --version flag
    // Rerun if git HEAD changes (new commits or tags)
    let git_dir = repo_root.join(".git");
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("refs/tags").display()
    );
    // .git/HEAD only changes on branch switches. Commits move the branch ref
    // HEAD points at, so track that file too (and packed-refs, where the ref
    // lands after `git pack-refs`) or --version reports stale git metadata.
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        if let Some(head_ref) = head.strip_prefix("ref: ") {
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join(head_ref.trim()).display()
            );
        }
    }
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );

    let version = Command::new("git")
        .current_dir(repo_root)
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        })
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=AGENTFS_VERSION={}", version);
}
