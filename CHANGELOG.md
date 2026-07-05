# Changelog

## [Unreleased] - Fork-era: VFS Right-Thing Restructure

This fork restructured the post-0.6.4 tree around a five-crate Rust
workspace. The changes below are the user-visible summary of that campaign;
behavior-preserving moves are not listed individually.

### Removed

- Deleted SDKs: the Go, Python, and TypeScript SDKs and their CI workflows;
  the Rust library survives as the `agentfs-core` crate.
- The standalone example projects (built against the deleted SDKs).
- The experimental ptrace sandbox and the `--experimental-sandbox` flag;
  `agentfs run` is FUSE+overlay in Linux user/mount namespaces (NFS +
  Sandbox on macOS).
- Windows stubs and the Windows dist target; supported platforms are Linux
  (first-tier) and macOS (second-tier, NFS mount only).
- The `abi-7-*` FUSE feature matrix (17 features collapsed into the one
  compiled ABI level) and the dead vendored fuser/nfsserve surface.
- The legacy path-based SDK API and the `AGENTFS_OVERLAY_PARTIAL_ORIGIN`
  env opt-in (superseded by the first-class `--partial-origin` policy).
- `migrate-v0-5`: one `agentfs migrate` now lands any supported old schema
  (v0.0, v0.2, v0.4) at the current version, in place by default or with
  `--copy`-based re-chunking.

### Changed

- One root workspace after the crate split, five crates in a clean DAG —
  `agentfs-core` (storage engine, overlay, schema authority, typed config,
  telemetry, semantics), `agentfs-fuse` and `agentfs-nfs` (sealed transport
  + adapter crates), `agentfs-mount` (one mount/supervision lifecycle), and
  `agentfs-cli` (thin edge with a single error reporter).
- Config: every runtime knob is a typed declaration parsed at the crate
  edge with one truthy grammar; the generated `docs/KNOBS.md` ledger is
  parity-checked in CI, as is the `docs/MANUAL.md` command reference
  (generated from clap). User docs moved under `docs/`.
- Telemetry: one macro registry and a single report sink replace the
  hand-rolled six-way counter boilerplate.
- Semantics: one `Semantics` layer under both adapters — a single
  permission implementation, explicit ack durability on every write path
  (NFS WRITE acks FILE_SYNC only after commit), and one handle/lifecycle
  authority.

### Fixed

- FUSE teardown deadlocks on both transport legs (classic and io_uring).
- NFS durability lie (FILE_SYNC acked without fsync) and non-graceful
  server shutdown.
- Overlay base-directory rename silently emptying the source: now `EXDEV`.
- Stale-overlay reads after external base mutation are rejected.
- Schema `ALTER`s no longer swallow errors blanket-`.ok()`-style.
- Mounts racing the kernel-side drain of a just-closed FUSE-over-io_uring
  connection are bounded (retry, then a clear error) instead of wedging
  inside `mount(2)` forever (kernel constraint, see docs/MANUAL.md).
- `tursodb-ephemeral-*` sort-spill litter from the turso_core 0.5.3
  dependency (never unlinked upstream, `vdbe/execute.rs:10096`): the CLI
  now scopes `TMPDIR` to a per-process spill dir cleaned on exit, without
  leaking the override into `run`/`exec` children. Track the upstream
  unlink fix before removing this workaround.

### Validation

- Honest CI gate (`scripts/gate.sh`): strict shell suite where SKIP is
  red, corruption torture on both uring legs, Phase 8 smoke, and the
  no-open/no-flush coherence gates.
- Local-only perf gate: serialized median-of-5 codex workload benchmark
  against a pinned baseline; any per-phase median regression >5% is red.
- The macOS NFS git validation script is documented as a manual release
  gate to run on real hardware.

## [0.6.4] - 2026-03-25

### Fixed

- TypeScript SDK: Add `@tursodatabase/serverless` to dev dependencies to fix CI build.

## [0.6.3] - 2026-03-25

### Added

- TypeScript SDK: Serverless adapter for `@tursodatabase/serverless`.

### Fixed

- Rust SDK: Fix hostfs `create_file()` failing with `EEXIST` on existing files.
- TypeScript SDK: Re-add statement caching and fix transaction adapter.

### Documentation

- Fix argument order for `agentfs fs` commands in README.

## [0.6.2] - 2026-02-21

### Fixed

- Update native-tls 0.2.17 -> 0.2.18 to fix nightly build.

## [0.6.1] - 2026-02-18

### Added

- Go SDK for AgentFS with overlay filesystem, connection pooling, streaming I/O, `io/fs` implementation, typesafe generic KV support, symlink support, and inode LRU cache.

### Changed

- Update pyturso dependency version.

### Fixed

- Rust SDK: Overlayfs whiteout for base files in promoted directories.
- Rust SDK: Stale base inodes after remount in overlayfs.
- CLI: FUSE kernel cache serving stale directory listings.

## [0.6.0] - 2026-02-05

AgentFS is now beta!

### Added

- `agentfs migrate` command for schema upgrades.
- `agentfs exec` command for running commands in an existing session.
- `-c` option to `agentfs init` for custom configuration.
- `--backend` option to `agentfs mount` for selecting mount backend.
- Local encryption support with `--key` option.
- POSIX special file support (block devices, character devices, FIFOs, sockets).
- POSIX file permissions support.
- NFS hard link support.
- NFS authentication and permissions.

### Changed

- Switch from path-based to inode-based architecture.
- Upgrade to Turso 0.4.4.
- Vendor `fuser` crate.
- Vendor `nfsserve` crate.
- Rust SDK: Nanosecond timestamp precision.
- Rust SDK: Replace anyhow with custom Error type.
- NFS: Increase mount timeouts to prevent I/O failures.

### Performance

- Rust SDK: Connection pooling.
- Rust SDK: Use `BEGIN IMMEDIATE` in write path.
- Rust SDK: Optimized DeltaDirCache operations.
- Rust SDK: Skip whiteout DELETE when no whiteout exists.
- FUSE: Optimize create() to use single create_file operation.

### Fixed

- Overlayfs readdir/unlink for delta files in base directories.
- TypeScript and Python SDK SQLite schema.
- Rust SDK: Opening a read-only file.
- Rust SDK: Overlay lookup using wrong delta parent inode.
- Rust SDK: Overlayfs permissions copy-up.
- Rust SDK: Sparse files in pwrite.
- Overlay filesystem whiteout persistence across mounts.
- NFS: File permissions with NFS backend.
- NFS: Sticky bit semantics for rename and remove.
- FUSE: Preserve setuid, setgid, and sticky bits in fillattr.
- Session join working directory.
- Stale NFS mounts and session joining.
- Various POSIX timestamp compliance fixes (ctime on chmod/chown/truncate/link/unlink, parent directory timestamps).

### Documentation

- Document `agentfs exec` command and `agentfs init -c` option.

## [0.5.3] - 2026-01-10

### Added

- `agentfs ps` command to list active sessions.
- `agentfs prune mounts` command.

### Changed

- `~/.cache`, `~/.gemini`, `~/.amp` added to default read-write allow list in `agentfs run`.
- Group paths by parent directory in `agentfs run` welcome banner.

### Performance

- Rust SDK: Switch to prepared statement caching.

### Fixed

- Rust SDK: Return ENOENT instead of EIO for file not found errors.

## [0.5.2] - 2026-01-09

### Fixed

- Fix Turso dependency.

## [0.5.1] - 2026-01-09

### Performance

- Rust SDK: Add dentry cache and path resolution optimizations.
- Rust SDK: Add in-memory whiteout cache.
- Rust SDK: Add NormalizedPath type for overlay filesystem.
- Update Turso to 0.4.3 pre-release to fix WAL read amplification.

### Fixed

- Inode consistency after copy-up in overlay filesystem.
- `unlink()` path cache invalidation in FUSE module.

### Documentation

- Update installation command for AgentFS CLI.

## [0.5.0] - 2026-01-08

### Added

- `agentfs serve` command for NFS and MCP servers.
- `agentfs mount` command to list all mounted filesystems.
- `agentfs timeline` command to display agent actions.
- `agentfs mcp-server` command.
- Basic sync support to the agentfs CLI.
- Hard link support across Rust SDK, sandbox, and FUSE.
- Local file locking on macOS.
- Explicit sandbox feature in CLI.

### Changed

- `~/.codex` added to default read-write allow list in `agentfs run`.
- Update just-bash to 2.0.
- Restructure `agentfs run` command files for clarity.

### Documentation

- Add FAQ entry for `git worktrees`.
- Add installation guide to README and MANUAL.md.

## [0.4.1] - 2026-01-02

### Added

- Cloudflare Durable Objects integration prototype.
- Sandbox: Intercept `rmdir` system call.

### Changed

- Init tracing subscriber to allow debugging turso_core.

### Fixed

- FUSE overlay deadlock.
- SIGTERM signal handling for graceful shutdown.
- `agentfs run` help text.
- ARM build for `rmdir` syscall.

## [0.4.0] - 2025-12-31

### Added

- `agentfs run` command with overlay filesystem for sandboxed execution.
- `agentfs diff` command to show filesystem changes.
- Multi-session support with `--session` flag and `AGENTFS_SESSION` environment variable.
- `--allow` flag for specifying writable directories in sandbox.
- macOS Sandbox support for filesystem isolation.
- NFS-based `agentfs run` support for macOS.
- Linux ARM64 support.
- TypeScript SDK: `FileSystem` interface for filesystem operations.
- TypeScript SDK: New APIs (`access`, `copyFile`, `rmdir`, `rename`).
- TypeScript SDK: `agentfs()` convenience function for just-bash integration.
- Python SDK: Python 3.10+ support.
- Rust SDK: `base` option for `agentfs::open`.
- Rust SDK: VFS-style `File` trait for efficient file handle operations.
- Rust SDK: `get_runtime()` helper for runtime initialization.
- FUSE: Symlink support.
- FUSE: `readdir_plus` optimization to eliminate N+1 queries.
- Database: `nlink` column for O(1) link count lookups.
- Sandbox: Intercept `chmod` system call.
- `--version` flag using git tags.
- Firecracker + AgentFS example.
- AI SDK + just-bash example with AgentFS integration.

### Changed

- Default shell is now bash on Linux, zsh on macOS.
- `/tmp` is writable by default in sandbox.
- `~/.bun` added to default allowed directories.
- npm local registry added to allowlist.
- `AGENTFS_SANDBOX` environment variable is more descriptive.
- FUSE optimizations: async read, parallel directory operations, symlink/directory caching.
- Rust SDK: Configure busy timeout instead of failing immediately.

### Fixed

- Overlay filesystem nested `pwrite()`.
- `O_APPEND` not appending to file.
- FUSE error handling.
- Execute permissions in FUSE mount.
- SSH inside user namespace by bypassing system configs.
- Symlink handling in FUSE and overlay filesystem.
- Rust SDK: `pread()` for sparse files.
- Rust SDK: `pwrite()` buffer flushing before returning.
- Rust SDK: `resolve` to prioritize agent ID over file path.
- `agentfs init --force` to reinitialize agent filesystem.
- Overlay mount I/O error by unifying whiteout schema in SDK.
- UID/GID mapping to use current user instead of root.

### Removed

- macFUSE support on macOS (replaced by NFS).

## [0.3.1] - 2025-12-17

- This is the exact same version as 0.3.0, but had to bump version number
  to work around a previous accidental publish on PyPI.

## [0.3.0] - 2025-12-17

### Added

- Python SDK for AgentFS.
- Web browser support for TypeScript SDK.
- Dynamic CLI completions with `completion` command (install/uninstall).

### Changed

- TypeScript SDK: Remove `ready()` method from the API.
- TypeScript SDK: Improve `KvStore.get()` API.
- TypeScript SDK: Improve `FileSystem.readFile()` compatibility.
- TypeScript SDK: Use `RETURNING` clause instead of `lastInsertRowid`.
- TypeScript SDK: Switch to proper Turso dependency versioning.
- Python SDK: Use `RETURNING` clause instead of `lastInsertRowid`.
- Rust SDK: Use `RETURNING` clause instead of `lastInsertRowid`.

### Fixed

- CLI `cat` command bug.
- macFUSE: pass full path to open dynamic libs.

### Documentation

- Add FAQ entry for Docker Sandbox.

## [0.2.3] - 2025-12-10

### Added

- macFUSE support

## [0.2.2] - 2025-12-08

### Added

- Linux/arm64 support.

### Documentation

- Improved FUSE module documentation.

## [0.2.1] - 2025-12-04

### Fixed

- Fix `_Unwind_RaiseException` symbol lookup error on Fedora by linking to `libgcc_s.so` dynamically.
- Eliminate dependency to libfuse by using the `fuser` crate pure Rust FUSE implementation.

## [0.2.0] - 2025-12-04

### Added

- AgentFS FUSE module for mounting agent filesystems.
- TypeScript SDK: Support for custom agent filesystem path.

### Changed

- Switch to fixed-size chunks in AgentFS specification.
- TypeScript SDK: Switch to fixed-size inode chunks.
- Rust SDK: Switch to fixed-size inode chunks.
- Switch AgentFS SDK to use identifier-based API.

## [0.1.2] - 2025-11-14

### Added

- Enable Darwin/x86-64 builds for the CLI.

## [0.1.1] - 2025-11-14

### Added

- Example using OpenAI Agents SDK and AgentFS.
- Example using Claude Agent SDK and AgentFS.

### Fixed

- CLI `ls` command now recursively lists all files.

## [0.1.0] - 2025-11-13

### Added

- Initial release of AgentFS CLI.
- TypeScript SDK with async factory method (`AgentFS.open()`).
- Sandbox command for running agents in isolated environments.
- Passthrough VFS for transparent filesystem access.
- Symlink syscall support in sandbox.
- Cross-platform builds (Linux, macOS).
- Example agent implementations.

[0.6.3]: https://github.com/tursodatabase/agentfs/compare/v0.6.2...v0.6.3
[0.6.2]: https://github.com/tursodatabase/agentfs/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/tursodatabase/agentfs/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/tursodatabase/agentfs/compare/v0.5.3...v0.6.0
[0.5.3]: https://github.com/tursodatabase/agentfs/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/tursodatabase/agentfs/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/tursodatabase/agentfs/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/tursodatabase/agentfs/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/tursodatabase/agentfs/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/tursodatabase/agentfs/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/tursodatabase/agentfs/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/tursodatabase/agentfs/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/tursodatabase/agentfs/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/tursodatabase/agentfs/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/tursodatabase/agentfs/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/tursodatabase/agentfs/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/tursodatabase/agentfs/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/tursodatabase/agentfs/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/tursodatabase/agentfs/releases/tag/v0.1.0
