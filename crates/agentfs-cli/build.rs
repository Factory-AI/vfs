use std::path::Path;
use std::process::Command;

fn main() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("agentfs-cli should live under crates/agentfs-cli");

    // Capture git version from tags for --version flag
    // Rerun if git HEAD changes (new commits or tags)
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join(".git/HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join(".git/refs/tags").display()
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
