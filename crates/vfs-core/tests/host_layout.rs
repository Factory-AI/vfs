use std::fs;
use std::path::Path;

#[test]
fn host_file_impl_lives_in_common_module() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let host_dir = manifest_dir.join("src/fs/host");
    let common = fs::read_to_string(host_dir.join("common.rs"))
        .expect("fs/host/common.rs should hold the shared HostFSFile implementation");
    let linux = fs::read_to_string(host_dir.join("linux.rs")).expect("read linux hostfs module");
    let darwin = fs::read_to_string(host_dir.join("darwin.rs")).expect("read darwin hostfs module");

    assert!(
        common.contains("pub(super) struct HostFSFile"),
        "HostFSFile should be declared once in common.rs"
    );
    assert!(
        common.contains("impl File for HostFSFile"),
        "the shared File impl should live in common.rs"
    );
    assert!(
        !linux.contains("impl File for HostFSFile"),
        "linux.rs should consume common::HostFSFile, not duplicate the File impl"
    );
    assert!(
        !darwin.contains("impl File for HostFSFile"),
        "darwin.rs should consume common::HostFSFile, not duplicate the File impl"
    );
}
