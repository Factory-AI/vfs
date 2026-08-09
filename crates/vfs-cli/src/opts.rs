use crate::cmd::completions::Shell;
use clap::{Parser, Subcommand};
use clap_complete::{
    engine::ValueCompleter, ArgValueCompleter, CompletionCandidate, PathCompleter,
};
use std::path::{Path, PathBuf};
use vfs_core::vfs_dir;

/// Mount backend type
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum MountBackend {
    /// FUSE filesystem (Linux only)
    Fuse,
    /// NFS over localhost
    Nfs,
}

// Platform-specific default: FUSE on Linux, NFS elsewhere
#[allow(clippy::derivable_impls)]
impl Default for MountBackend {
    fn default() -> Self {
        #[cfg(target_os = "linux")]
        {
            MountBackend::Fuse
        }
        #[cfg(not(target_os = "linux"))]
        {
            MountBackend::Nfs
        }
    }
}

impl std::fmt::Display for MountBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountBackend::Fuse => write!(f, "fuse"),
            MountBackend::Nfs => write!(f, "nfs"),
        }
    }
}

impl From<MountBackend> for vfs_mount::Backend {
    fn from(value: MountBackend) -> Self {
        match value {
            MountBackend::Fuse => vfs_mount::Backend::Fuse,
            MountBackend::Nfs => vfs_mount::Backend::Nfs,
        }
    }
}

/// Partial-origin copy-up policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PartialOriginMode {
    /// Whole-file copy-up; portable by default
    Off,
    /// Partial-origin copy-up for eligible regular base files
    On,
    /// Partial-origin copy-up above a conservative size threshold
    Auto,
}

impl From<PartialOriginMode> for vfs_core::PartialOriginMode {
    fn from(value: PartialOriginMode) -> Self {
        match value {
            PartialOriginMode::Off => vfs_core::PartialOriginMode::Off,
            PartialOriginMode::On => vfs_core::PartialOriginMode::On,
            PartialOriginMode::Auto => vfs_core::PartialOriginMode::Auto,
        }
    }
}

/// Resolved knobs for `vfs run`.
///
/// One struct threaded through `cmd/run/*` so adding a run knob touches the
/// clap arm, this struct, and the platform backend that consumes it — not a
/// parallel parameter list in every platform file.
#[derive(Debug)]
pub struct RunOptions {
    /// Additional host directories granted read/write access in the sandbox.
    pub allow: Vec<PathBuf>,
    /// Skip the built-in `DEFAULT_ALLOWED_DIRS` grants.
    pub no_default_allows: bool,
    /// Session identifier for sharing a delta layer across runs.
    pub session: Option<String>,
    /// Allow other system users to access the mount (FUSE allow_other).
    pub system: bool,
    /// Delta-layer encryption, already validated at the CLI edge.
    pub encryption: Option<vfs_core::EncryptionConfig>,
    /// Partial-origin copy-up policy resolved from CLI flags.
    pub partial_origin_policy: Option<vfs_core::PartialOriginPolicy>,
    /// Git commit whose pristine tree is the portable session base.
    pub seed_pin: Option<String>,
    /// Command to execute inside the sandbox.
    pub command: PathBuf,
    /// Arguments for the command.
    pub args: Vec<String>,
}

#[derive(Parser, Debug)]
#[command(name = "vfs")]
#[command(version = env!("VFS_VERSION"))]
#[command(about = "The filesystem for agents", long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Parser)]
pub struct SyncCommandOptions {
    #[arg(long)]
    pub(crate) sync_remote_url: Option<String>,
    #[arg(long)]
    pub(crate) sync_partial_prefetch: Option<bool>,
    #[arg(long)]
    pub(crate) sync_partial_segment_size: Option<usize>,
    #[arg(long)]
    pub(crate) sync_partial_bootstrap_query: Option<String>,
    #[arg(long)]
    pub(crate) sync_partial_bootstrap_length: Option<usize>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage shell completions (supported shells: bash, zsh, fish, elvish, powershell)
    Completions {
        #[command(subcommand)]
        command: CompletionsCommand,
    },
    /// Initialize a new agent filesystem
    Init {
        /// Agent identifier (if not provided, generates a unique one)
        id: Option<String>,

        /// Overwrite existing file if it exists
        #[arg(long)]
        force: bool,

        /// Base directory for overlay filesystem (copy-on-write)
        #[arg(long)]
        base: Option<PathBuf>,

        /// Hex-encoded encryption key.
        /// Enables local encryption when provided.
        #[arg(long, env = "VFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "VFS_CIPHER")]
        cipher: Option<String>,

        /// Command to execute after initialization (mounts the filesystem, runs command, unmounts)
        #[arg(short = 'c', long = "command")]
        command: Option<String>,

        /// Backend to use for mounting when using -c (default: fuse on Linux, nfs on macOS)
        #[arg(long, default_value_t = MountBackend::default())]
        backend: MountBackend,

        #[command(flatten)]
        sync: SyncCommandOptions,
    },
    /// Remote sync operations
    Sync {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        #[command(subcommand)]
        command: SyncCommand,
    },
    /// Filesystem operations
    Fs {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Hex-encoded encryption key for encrypted databases.
        #[arg(long, env = "VFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "VFS_CIPHER")]
        cipher: Option<String>,

        #[command(subcommand)]
        command: FsCommand,
    },
    /// Run a command in the sandboxed environment.
    ///
    /// By default, uses FUSE+overlay with Linux user and mount namespaces for isolation.
    /// The overlay uses the host filesystem as a read-only base and stores
    /// all changes in a Vfs-backed delta layer. On macOS the overlay is
    /// mounted over NFS and a generated Seatbelt profile scopes writes to the
    /// sandbox and reads to the allowed directories plus required platform
    /// paths (see the Sandboxing section of docs/MANUAL.md).
    Run {
        /// Allow read/write access to additional directories (can be specified multiple times)
        #[arg(long = "allow", value_name = "PATH")]
        allow: Vec<PathBuf>,

        /// Disable default allowed directories (~/.config, ~/.cache, ~/.local, ~/.claude, etc.)
        #[arg(long = "no-default-allows")]
        no_default_allows: bool,

        /// Session identifier for sharing delta layer across multiple runs.
        /// If not provided, a unique session ID is generated for each run.
        /// Use the same session ID to share the delta layer between runs.
        #[arg(long = "session", value_name = "ID")]
        session: Option<String>,

        /// Allow other system users to access this mount (requires /etc/fuse.conf
        /// user_allow_other; use cautiously)
        #[arg(long = "system")]
        system: bool,

        /// Partial-origin policy for base-file writes: off, on, or auto
        #[arg(long = "partial-origin", value_enum, value_name = "MODE")]
        partial_origin: Option<PartialOriginMode>,

        /// Size threshold for --partial-origin auto
        #[arg(long = "partial-origin-threshold-bytes", value_name = "BYTES")]
        partial_origin_threshold_bytes: Option<u64>,

        /// Hex-encoded encryption key for the delta layer.
        /// Enables local encryption when provided.
        #[arg(long, env = "VFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "VFS_CIPHER")]
        cipher: Option<String>,

        /// Seed dirty and local-commit state against this git commit before mounting.
        /// Ignored files are not part of portable state.
        #[arg(long = "seed-pin", value_name = "COMMIT")]
        seed_pin: Option<String>,

        /// Command to execute (defaults to bash on Linux, zsh on macOS)
        command: Option<PathBuf>,

        /// Arguments for the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Execute a command with a Vfs filesystem mounted.
    ///
    /// Mounts the specified Vfs to a temporary directory, runs the command
    /// with that directory as the working directory, then automatically unmounts.
    /// This is useful for running tools that need filesystem access without
    /// a persistent mount.
    #[cfg(unix)]
    Exec {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Command to execute
        #[arg(value_name = "COMMAND")]
        command: PathBuf,

        /// Arguments for the command
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,

        /// Backend to use for mounting (default: fuse on Linux, nfs on macOS)
        #[arg(long, default_value_t = MountBackend::default())]
        backend: MountBackend,

        /// Hex-encoded encryption key for encrypted databases.
        #[arg(long, env = "VFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        #[arg(long, env = "VFS_CIPHER")]
        cipher: Option<String>,
    },
    /// Clone a git repository into a Vfs database (fast bulk ingest).
    ///
    /// Runs `git clone --no-checkout` through a temporary mount (pack files
    /// are large sequential writes), then materializes the worktree by
    /// bulk-importing blobs straight into the database in large transactions
    /// and fabricating a matching git index, skipping the per-file FUSE
    /// round trips of a regular checkout. The resulting repository lives
    /// entirely inside the database; nothing is written to the host
    /// filesystem. Submodules and smudge/clean filters are not supported.
    #[cfg(unix)]
    Clone {
        /// Agent ID or database path (created if it does not exist)
        #[arg(value_name = "ID_OR_PATH")]
        id_or_path: String,

        /// Git repository to clone (URL or local path)
        #[arg(value_name = "SOURCE")]
        source: String,

        /// Directory name for the repository inside the filesystem
        /// (default: derived from the source)
        #[arg(value_name = "NAME")]
        name: Option<String>,

        /// Backend to use for mounting (default: fuse on Linux, nfs on macOS)
        #[arg(long, default_value_t = MountBackend::default())]
        backend: MountBackend,

        /// Verify `git status` is clean through the mount before finishing
        #[arg(long)]
        verify: bool,
    },
    /// Mount an agent filesystem using FUSE (or list mounts if no args)
    Mount {
        /// Agent ID or database path (if omitted, lists current mounts)
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: Option<String>,

        /// Mount point directory
        #[arg(value_name = "MOUNTPOINT", add = ArgValueCompleter::new(PathCompleter::dir()))]
        mountpoint: Option<PathBuf>,

        /// Automatically unmount on exit
        #[arg(short = 'a', long)]
        auto_unmount: bool,

        /// Allow root user to access filesystem
        #[arg(long)]
        allow_root: bool,

        /// Allow other system users to access this mount (requires /etc/fuse.conf
        /// user_allow_other; use cautiously)
        #[arg(long = "system")]
        system: bool,

        /// Run in foreground (don't daemonize)
        #[arg(short = 'f', long)]
        foreground: bool,

        /// User ID to report for all files (defaults to current user)
        #[arg(long)]
        uid: Option<u32>,

        /// Group ID to report for all files (defaults to current group)
        #[arg(long)]
        gid: Option<u32>,

        /// Backend to use for mounting
        #[arg(long, default_value_t = MountBackend::default())]
        backend: MountBackend,

        /// Partial-origin policy for base-file writes: off, on, or auto
        #[arg(long = "partial-origin", value_enum, value_name = "MODE")]
        partial_origin: Option<PartialOriginMode>,

        /// Size threshold for --partial-origin auto
        #[arg(long = "partial-origin-threshold-bytes", value_name = "BYTES")]
        partial_origin_threshold_bytes: Option<u64>,

        /// Hex-encoded encryption key for encrypted databases.
        #[arg(long, env = "VFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "VFS_CIPHER")]
        cipher: Option<String>,
    },
    /// Show differences between base filesystem and delta (overlay mode only)
    Diff {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,
    },
    /// Display agent action timeline from tool call audit log
    Timeline {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Limit number of entries to display
        #[arg(long, default_value = "100")]
        limit: i64,

        /// Filter by tool name
        #[arg(long)]
        filter: Option<String>,

        /// Filter by status (pending/success/error)
        #[arg(long, value_parser = ["pending", "success", "error"])]
        status: Option<String>,

        /// Output format
        #[arg(long, default_value = "table", value_parser = ["table", "json"])]
        format: String,
    },
    /// Start an NFS server to export a Vfs filesystem over the network
    /// (deprecated: use `vfs serve nfs` instead)
    #[cfg(unix)]
    Nfs {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// IP address to bind to
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Port to listen on
        #[arg(long, default_value = "11111")]
        port: u32,

        /// Hex-encoded encryption key for encrypted databases.
        #[arg(long, env = "VFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "VFS_CIPHER")]
        cipher: Option<String>,
    },

    /// Start an MCP server exposing filesystem and KV-store tools
    /// (deprecated: use `vfs serve mcp` instead)
    McpServer {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Tools to expose (comma-separated). If not provided, all tools are exposed.
        /// Available tools: read_file, write_file, readdir, mkdir, remove, rename,
        /// stat, access, kv_get, kv_set, kv_delete, kv_list
        #[arg(long, value_delimiter = ',')]
        tools: Option<Vec<String>>,
    },

    /// Serve a Vfs filesystem via different protocols
    Serve {
        #[command(subcommand)]
        command: ServeCommand,
    },
    /// Prepare an inactive run session database for transfer
    Pack {
        /// Run session identifier
        #[arg(value_name = "SESSION_ID")]
        session_id: String,

        /// Additional delta path glob to prune (can be specified multiple times)
        #[arg(long, value_name = "GLOB")]
        prune: Vec<String>,

        /// Disable the default generated-artifact prune globs
        #[arg(long)]
        no_default_prunes: bool,

        /// Copy the packed database to this path
        #[arg(long, value_name = "PATH")]
        output: Option<PathBuf>,

        /// Byte size of the per-chunk verification digests reported in the
        /// manifest's `chunks` list
        #[arg(long = "chunk-size", value_name = "BYTES", default_value_t = 4_194_304)]
        chunk_size: u64,

        /// Emit machine-readable JSON (pack output is always JSON)
        #[arg(long)]
        json: bool,
    },
    /// Capture a run session's live git state into its portable delta.
    ///
    /// Compares the session base checkout with --pin, imports dirty files and
    /// local commits without mounting, and records deletions as whiteouts.
    /// Ignored files are not part of portable state.
    Seed {
        /// Run session identifier
        #[arg(value_name = "SESSION_ID")]
        session_id: String,

        /// Git commit used as the pristine portable base
        #[arg(long, value_name = "COMMIT")]
        pin: String,

        /// Emit machine-readable JSON (seed output is always JSON)
        #[arg(long)]
        json: bool,
    },
    /// Fork a run session at its current or a retained historical state.
    ///
    /// Snapshots the parent session (live sessions are snapshotted through
    /// the mount without stopping them), publishes the snapshot as an
    /// immutable content-addressed artifact under ~/.vfs/artifacts, and
    /// installs a new session whose overlay reads fall through to that
    /// artifact. Branches taken at the same state share one artifact, so
    /// forking N times is cheap. The branch refuses to mount if the artifact
    /// no longer matches its recorded digest.
    #[cfg(unix)]
    Branch {
        /// Parent run session identifier
        #[arg(value_name = "SESSION_ID")]
        parent_session_id: String,

        /// Identifier for the new branch session (default: generated)
        #[arg(long = "session", value_name = "ID")]
        session: Option<String>,

        /// Reconstruct the parent at this complete history sequence
        #[arg(long, value_name = "SEQ")]
        to: Option<i64>,

        /// Emit machine-readable JSON (branch output is always JSON)
        #[arg(long)]
        json: bool,
    },
    /// List retained replayable history for a run session
    History {
        /// Run session identifier
        #[arg(value_name = "SESSION_ID")]
        session_id: String,

        /// Maximum newest transaction groups to list (default: 100)
        #[arg(long, value_name = "N", conflicts_with = "all")]
        limit: Option<usize>,

        /// List every retained transaction group
        #[arg(long)]
        all: bool,

        /// Emit a one-line machine-readable JSON manifest
        #[arg(long)]
        json: bool,
    },
    /// Rewind an inactive run session to a retained history sequence
    Revert {
        /// Run session identifier
        #[arg(value_name = "SESSION_ID")]
        session_id: String,

        /// Complete history sequence to restore
        #[arg(long, value_name = "SEQ", required = true)]
        to: i64,

        /// Emit a one-line machine-readable JSON manifest
        #[arg(long)]
        json: bool,
    },
    /// Install an externally transferred run session.
    ///
    /// Verifies a packed session database against the receiver's base git
    /// checkout (the checkout's HEAD must equal the artifact's recorded seed
    /// pin), migrates supported older artifact schemas to the current
    /// version, and atomically publishes ~/.vfs/run/<SESSION_ID>. After
    /// adopt, `vfs run --session <SESSION_ID>` resumes the transferred
    /// session.
    Adopt {
        /// Run session identifier
        #[arg(value_name = "SESSION_ID")]
        session_id: String,

        /// Packed session database produced by `vfs pack`
        #[arg(long, value_name = "PATH")]
        db: PathBuf,

        /// The receiver's base git checkout for the session
        #[arg(long, value_name = "PATH", add = ArgValueCompleter::new(PathCompleter::dir()))]
        base: PathBuf,

        /// Git commit the base checkout must be at (required only when the
        /// artifact does not record a seed pin)
        #[arg(long, value_name = "COMMIT")]
        pin: Option<String>,

        /// Emit machine-readable JSON (adopt output is always JSON)
        #[arg(long)]
        json: bool,
    },
    /// Show run session state for daemon preflight
    Status {
        /// Run session identifier
        #[arg(value_name = "SESSION_ID")]
        session_id: String,

        /// Emit machine-readable JSON (status output is always JSON)
        #[arg(long)]
        json: bool,

        /// Hex-encoded encryption key for an encrypted session database.
        #[arg(long, env = "VFS_KEY")]
        key: Option<String>,

        /// Encryption cipher (required with --key).
        #[arg(long, env = "VFS_CIPHER")]
        cipher: Option<String>,
    },
    /// Show vfs version and feature capabilities
    Version {
        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// List active vfs run sessions
    Ps,
    /// Prune unused resources
    Prune {
        #[command(subcommand)]
        command: PruneCommand,
    },
    /// Check a local Vfs database for SQLite and schema corruption
    Integrity {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Emit machine-readable JSON
        #[arg(long)]
        json: bool,

        /// Fail if the database depends on external partial-origin base files
        #[arg(long)]
        require_portable: bool,

        /// Validate partial-origin base file fingerprints against the current base tree
        #[arg(long)]
        check_base: bool,

        /// Checkpoint the WAL and remove empty SQLite sidecars after checks pass
        #[arg(long)]
        checkpoint: bool,

        /// Hex-encoded encryption key for encrypted databases
        #[arg(long)]
        key: Option<String>,

        /// Encryption cipher (required with --key)
        #[arg(long)]
        cipher: Option<String>,
    },
    /// Create a portable local Vfs database backup
    Backup {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Target database path to create
        target: PathBuf,

        /// Reopen and verify the copied main database
        #[arg(long)]
        verify: bool,

        /// Materialize partial-origin files into a portable backup
        #[arg(long)]
        materialize: bool,

        /// Hex-encoded encryption key for encrypted databases
        #[arg(long)]
        key: Option<String>,

        /// Encryption cipher (required with --key)
        #[arg(long)]
        cipher: Option<String>,
    },
    /// Create a portable database by materializing partial-origin files
    Materialize {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Target database path to create
        #[arg(long)]
        output: PathBuf,

        /// Reopen and verify the materialized database
        #[arg(long)]
        verify: bool,

        /// Hex-encoded encryption key for encrypted databases
        #[arg(long)]
        key: Option<String>,

        /// Encryption cipher (required with --key)
        #[arg(long)]
        cipher: Option<String>,
    },
    /// Migrate database schema to the current version
    Migrate {
        /// Agent ID or database path
        #[arg(add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Preview migration without applying changes
        #[arg(long, conflicts_with = "copy")]
        dry_run: bool,

        /// Copy-migrate into a new database file at this path instead of
        /// migrating in place
        #[arg(long, value_name = "TARGET")]
        copy: Option<PathBuf>,

        /// Verify migrated state equivalence (requires --copy)
        #[arg(long, requires = "copy")]
        verify: bool,

        /// Allow replacing an existing --copy target database
        #[arg(long, requires = "copy")]
        overwrite_target: bool,

        /// Hex-encoded encryption key for encrypted databases
        #[arg(long)]
        key: Option<String>,

        /// Encryption cipher (required with --key)
        #[arg(long)]
        cipher: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum FsCommand {
    /// List files in the filesystem
    Ls {
        /// Path to list (default: /)
        #[arg(default_value = "/")]
        fs_path: String,
    },
    /// Display file contents
    Cat {
        /// Path to the file in the filesystem
        file_path: String,
    },
    /// Write file content
    Write {
        /// Path to the file in the filesystem
        file_path: String,

        /// Content of the file
        content: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SyncCommand {
    /// Pull remote changes (only of vfs was initialized with remote sync)
    Pull,
    /// Push remote changes (only of vfs was initialized with remote sync)
    Push,
    /// Print synced database stats
    Stats,
    /// Checkpoint local synced db
    Checkpoint,
}

#[derive(Subcommand, Debug)]
pub enum ServeCommand {
    /// Start an NFS server to export a Vfs filesystem over the network
    #[cfg(unix)]
    Nfs {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// IP address to bind to
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Port to listen on
        #[arg(long, default_value = "11111")]
        port: u32,

        /// Hex-encoded encryption key for encrypted databases.
        #[arg(long, env = "VFS_KEY")]
        key: Option<String>,

        /// Cipher algorithm for encryption (required with --key).
        /// Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm
        #[arg(long, env = "VFS_CIPHER")]
        cipher: Option<String>,
    },

    /// Start an MCP server exposing filesystem and KV-store tools
    Mcp {
        /// Agent ID or database path
        #[arg(value_name = "ID_OR_PATH", add = ArgValueCompleter::new(id_or_path_completer))]
        id_or_path: String,

        /// Tools to expose (comma-separated). If not provided, all tools are exposed.
        /// Available tools: read_file, write_file, readdir, mkdir, remove, rename,
        /// stat, access, kv_get, kv_set, kv_delete, kv_list
        #[arg(long, value_delimiter = ',')]
        tools: Option<Vec<String>>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PruneCommand {
    /// Unmount unused vfs mount points
    Mounts {
        /// Skip confirmation prompt and unmount immediately
        #[arg(long)]
        force: bool,
    },
    /// Remove branch parent artifacts no session references
    ///
    /// Walks every installed run session (live sessions are asked over their
    /// control socket) and the artifact chains they reference, then deletes
    /// unreferenced artifacts from ~/.vfs/artifacts. Refuses to guess: a
    /// session that cannot be classified aborts the prune. Output is a
    /// one-line JSON report.
    #[cfg(unix)]
    Artifacts {
        /// Report what would be removed without deleting anything
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand, Debug, Clone, Copy)]
pub enum CompletionsCommand {
    /// Install shell completions to your shell rc file
    Install {
        /// Shell to install completions for (defaults to current shell)
        #[arg(value_enum)]
        shell: Option<Shell>,
    },
    /// Uninstall shell completions from your shell rc file
    Uninstall {
        /// Shell to uninstall completions for (defaults to current shell)
        #[arg(value_enum)]
        shell: Option<Shell>,
    },
    /// Print instructions for manual installation
    Show,
}

fn id_completer(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let mut completions = vec![];
    let Some(current) = current.to_str() else {
        return completions;
    };

    let vfs_dir = vfs_dir();
    let Ok(read_dir) = vfs_dir.read_dir() else {
        return completions;
    };

    let mut ids = read_dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let file_name = e.file_name();
            let path = Path::new(&file_name);
            let name = path.file_prefix()?.to_str()?;
            if name.starts_with(current) {
                Some(CompletionCandidate::new(name).help(Some("Agent ID".into())))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();

    ids.sort();
    ids.dedup();

    completions.append(&mut ids);
    completions
}

fn id_or_path_completer(current: &std::ffi::OsStr) -> Vec<CompletionCandidate> {
    let mut completions = vec![];

    // TODO: maybe filter files by `.db`
    let path_completer = PathCompleter::any();
    let mut path_completions = path_completer.complete(current);

    let mut ids = id_completer(current);

    completions.append(&mut ids);

    completions.append(&mut path_completions);

    completions
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn run_partial_origin_options_parse() {
        let args = Args::try_parse_from([
            "vfs",
            "run",
            "--partial-origin",
            "auto",
            "--partial-origin-threshold-bytes",
            "4096",
            "--seed-pin",
            "abc123",
            "bash",
        ])
        .unwrap();

        match args.command {
            Command::Run {
                partial_origin,
                partial_origin_threshold_bytes,
                seed_pin,
                command,
                ..
            } => {
                assert_eq!(partial_origin, Some(PartialOriginMode::Auto));
                assert_eq!(partial_origin_threshold_bytes, Some(4096));
                assert_eq!(seed_pin.as_deref(), Some("abc123"));
                assert_eq!(command, Some(PathBuf::from("bash")));
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }

    #[test]
    fn mount_partial_origin_options_parse() {
        let args = Args::try_parse_from([
            "vfs",
            "mount",
            "--partial-origin",
            "on",
            "--partial-origin-threshold-bytes",
            "8192",
            "agent",
            "/tmp/vfs-mnt",
        ])
        .unwrap();

        match args.command {
            Command::Mount {
                partial_origin,
                partial_origin_threshold_bytes,
                id_or_path,
                mountpoint,
                ..
            } => {
                assert_eq!(partial_origin, Some(PartialOriginMode::On));
                assert_eq!(partial_origin_threshold_bytes, Some(8192));
                assert_eq!(id_or_path.as_deref(), Some("agent"));
                assert_eq!(mountpoint, Some(PathBuf::from("/tmp/vfs-mnt")));
            }
            other => panic!("expected mount command, got {other:?}"),
        }
    }

    #[test]
    fn pack_options_parse() {
        let args = Args::try_parse_from([
            "vfs",
            "pack",
            "session-1",
            "--prune",
            "**/.cache/**",
            "--no-default-prunes",
            "--output",
            "/tmp/packed.db",
            "--json",
        ])
        .unwrap();

        match args.command {
            Command::Pack {
                session_id,
                prune,
                no_default_prunes,
                output,
                chunk_size,
                json,
            } => {
                assert_eq!(session_id, "session-1");
                assert_eq!(prune, vec!["**/.cache/**"]);
                assert!(no_default_prunes);
                assert_eq!(output, Some(PathBuf::from("/tmp/packed.db")));
                assert_eq!(chunk_size, 4_194_304);
                assert!(json);
            }
            other => panic!("expected pack command, got {other:?}"),
        }

        let args =
            Args::try_parse_from(["vfs", "pack", "session-1", "--chunk-size", "65536"]).unwrap();
        match args.command {
            Command::Pack { chunk_size, .. } => assert_eq!(chunk_size, 65_536),
            other => panic!("expected pack command, got {other:?}"),
        }
    }

    #[test]
    fn seed_options_parse() {
        let args = Args::try_parse_from(["vfs", "seed", "session-1", "--pin", "abc123", "--json"])
            .unwrap();

        match args.command {
            Command::Seed {
                session_id,
                pin,
                json,
            } => {
                assert_eq!(session_id, "session-1");
                assert_eq!(pin, "abc123");
                assert!(json);
            }
            other => panic!("expected seed command, got {other:?}"),
        }
    }

    #[test]
    fn adopt_options_parse() {
        let args = Args::try_parse_from([
            "vfs",
            "adopt",
            "session-1",
            "--db",
            "/tmp/packed.db",
            "--base",
            "/tmp/checkout",
            "--pin",
            "abc123",
            "--json",
        ])
        .unwrap();

        match args.command {
            Command::Adopt {
                session_id,
                db,
                base,
                pin,
                json,
            } => {
                assert_eq!(session_id, "session-1");
                assert_eq!(db, PathBuf::from("/tmp/packed.db"));
                assert_eq!(base, PathBuf::from("/tmp/checkout"));
                assert_eq!(pin.as_deref(), Some("abc123"));
                assert!(json);
            }
            other => panic!("expected adopt command, got {other:?}"),
        }
    }

    #[test]
    fn version_json_option_parses() {
        let args = Args::try_parse_from(["vfs", "version", "--json"]).unwrap();
        match args.command {
            Command::Version { json } => assert!(json),
            other => panic!("expected version command, got {other:?}"),
        }
    }

    #[test]
    fn status_json_option_parses() {
        let args = Args::try_parse_from(["vfs", "status", "session-1", "--json"]).unwrap();
        match args.command {
            Command::Status {
                session_id,
                json,
                key,
                cipher,
            } => {
                assert_eq!(session_id, "session-1");
                assert!(json);
                assert!(key.is_none());
                assert!(cipher.is_none());
            }
            other => panic!("expected status command, got {other:?}"),
        }
    }
}
