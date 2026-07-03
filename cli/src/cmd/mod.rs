pub mod completions;
pub mod fs;
pub mod init;
pub mod mcp_server;
pub mod migrate;
pub mod ps;
pub mod safety;
pub mod sync;
pub mod timeline;

#[cfg(unix)]
pub mod mount;

#[cfg(unix)]
pub(crate) mod supervise;

mod run;

// Standalone NFS server command (Unix only)
#[cfg(unix)]
pub mod nfs;

// Exec command (Unix only)
#[cfg(unix)]
pub mod exec;

// Clone command (Unix only)
#[cfg(unix)]
pub mod clone;

#[cfg(unix)]
pub use mount::{mount, MountArgs, MountBackend};
pub use run::handle_run_command;
