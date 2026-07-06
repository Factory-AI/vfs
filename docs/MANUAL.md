# AgentFS Reference Guide

Command-line reference for the AgentFS CLI.

The command and option sections between the `GENERATED COMMAND REFERENCE`
markers are rendered from the CLI's clap definitions and checked by
`docs::tests::manual_help_parity`, so this manual always matches
`agentfs --help`.

## Installation

Build from source (Linux is the first-tier platform; macOS is second-tier,
NFS mount only):

```bash
cargo +nightly build --release --workspace --bins
install -m 0755 target/release/agentfs ~/.local/bin/
```

<!-- BEGIN GENERATED COMMAND REFERENCE (do not edit by hand) -->
<!-- Regenerate with: `AGENTFS_UPDATE_MANUAL=1 cargo +nightly test -p agentfs-cli --lib docs::tests::manual_help_parity -- --exact` -->

## Commands

Every section below is generated from the clap definitions the binary actually parses; `agentfs <command> --help` and this reference cannot disagree.

### agentfs completions

Manage shell completions (supported shells: bash, zsh, fish, elvish, powershell)

```
agentfs completions <COMMAND>
```

#### agentfs completions install

Install shell completions to your shell rc file

```
agentfs completions install [SHELL]
```

**Arguments:**

- `[SHELL]` — Shell to install completions for (defaults to current shell) [possible values: bash, zsh, fish, elvish, power-shell]

#### agentfs completions uninstall

Uninstall shell completions from your shell rc file

```
agentfs completions uninstall [SHELL]
```

**Arguments:**

- `[SHELL]` — Shell to uninstall completions for (defaults to current shell) [possible values: bash, zsh, fish, elvish, power-shell]

#### agentfs completions show

Print instructions for manual installation

```
agentfs completions show
```

### agentfs init

Initialize a new agent filesystem

```
agentfs init [OPTIONS] [ID]
```

**Arguments:**

- `[ID]` — Agent identifier (if not provided, generates a unique one)

**Options:**

- `--force` — Overwrite existing file if it exists
- `--base <BASE>` — Base directory for overlay filesystem (copy-on-write)
- `--key <KEY>` — Hex-encoded encryption key. Enables local encryption when provided [env: AGENTFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: AGENTFS_CIPHER]
- `-c, --command <COMMAND>` — Command to execute after initialization (mounts the filesystem, runs command, unmounts)
- `--backend <BACKEND>` — Backend to use for mounting when using -c (default: fuse on Linux, nfs on macOS) [possible values: fuse, nfs; default: fuse]
- `--sync-remote-url <SYNC_REMOTE_URL>`
- `--sync-partial-prefetch <SYNC_PARTIAL_PREFETCH>` [possible values: true, false]
- `--sync-partial-segment-size <SYNC_PARTIAL_SEGMENT_SIZE>`
- `--sync-partial-bootstrap-query <SYNC_PARTIAL_BOOTSTRAP_QUERY>`
- `--sync-partial-bootstrap-length <SYNC_PARTIAL_BOOTSTRAP_LENGTH>`

### agentfs sync

Remote sync operations

```
agentfs sync <ID_OR_PATH> <COMMAND>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

#### agentfs sync pull

Pull remote changes (only of agentfs was initialized with remote sync)

```
agentfs sync <ID_OR_PATH> pull
```

#### agentfs sync push

Push remote changes (only of agentfs was initialized with remote sync)

```
agentfs sync <ID_OR_PATH> push
```

#### agentfs sync stats

Print synced database stats

```
agentfs sync <ID_OR_PATH> stats
```

#### agentfs sync checkpoint

Checkpoint local synced db

```
agentfs sync <ID_OR_PATH> checkpoint
```

### agentfs fs

Filesystem operations

```
agentfs fs [OPTIONS] <ID_OR_PATH> <COMMAND>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: AGENTFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: AGENTFS_CIPHER]

#### agentfs fs ls

List files in the filesystem

```
agentfs fs <ID_OR_PATH> ls [FS_PATH]
```

**Arguments:**

- `[FS_PATH]` — Path to list (default: /) [default: /]

#### agentfs fs cat

Display file contents

```
agentfs fs <ID_OR_PATH> cat <FILE_PATH>
```

**Arguments:**

- `<FILE_PATH>` — Path to the file in the filesystem

#### agentfs fs write

Write file content

```
agentfs fs <ID_OR_PATH> write <FILE_PATH> <CONTENT>
```

**Arguments:**

- `<FILE_PATH>` — Path to the file in the filesystem
- `<CONTENT>` — Content of the file

### agentfs run

Run a command in the sandboxed environment.

By default, uses FUSE+overlay with Linux user and mount namespaces for isolation. The overlay uses the host filesystem as a read-only base and stores all changes in an AgentFS-backed delta layer. On macOS the overlay is mounted over NFS and a generated Seatbelt profile scopes writes to the sandbox and reads to the allowed directories plus required platform paths (see the Sandboxing section of docs/MANUAL.md).

```
agentfs run [OPTIONS] [COMMAND] [ARGS]...
```

**Arguments:**

- `[COMMAND]` — Command to execute (defaults to bash on Linux, zsh on macOS)
- `[ARGS]...` — Arguments for the command

**Options:**

- `--allow <PATH>` — Allow read/write access to additional directories (can be specified multiple times)
- `--no-default-allows` — Disable default allowed directories (~/.config, ~/.cache, ~/.local, ~/.claude, etc.)
- `--session <ID>` — Session identifier for sharing delta layer across multiple runs. If not provided, a unique session ID is generated for each run. Use the same session ID to share the delta layer between runs
- `--system` — Allow other system users to access this mount (requires /etc/fuse.conf user_allow_other; use cautiously)
- `--partial-origin <MODE>` — Partial-origin policy for base-file writes: off, on, or auto [possible values: off, on, auto]
- `--partial-origin-threshold-bytes <BYTES>` — Size threshold for --partial-origin auto
- `--key <KEY>` — Hex-encoded encryption key for the delta layer. Enables local encryption when provided [env: AGENTFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: AGENTFS_CIPHER]

### agentfs exec

Execute a command with an AgentFS filesystem mounted.

Mounts the specified AgentFS to a temporary directory, runs the command with that directory as the working directory, then automatically unmounts. This is useful for running tools that need filesystem access without a persistent mount.

```
agentfs exec [OPTIONS] <ID_OR_PATH> <COMMAND> [ARGS]...
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path
- `<COMMAND>` — Command to execute
- `[ARGS]...` — Arguments for the command

**Options:**

- `--backend <BACKEND>` — Backend to use for mounting (default: fuse on Linux, nfs on macOS) [possible values: fuse, nfs; default: fuse]
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: AGENTFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key) [env: AGENTFS_CIPHER]

### agentfs clone

Clone a git repository into an AgentFS database (fast bulk ingest).

Runs `git clone --no-checkout` through a temporary mount (pack files are large sequential writes), then materializes the worktree by bulk-importing blobs straight into the database in large transactions and fabricating a matching git index, skipping the per-file FUSE round trips of a regular checkout. The resulting repository lives entirely inside the database; nothing is written to the host filesystem. Submodules and smudge/clean filters are not supported.

```
agentfs clone [OPTIONS] <ID_OR_PATH> <SOURCE> [NAME]
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path (created if it does not exist)
- `<SOURCE>` — Git repository to clone (URL or local path)
- `[NAME]` — Directory name for the repository inside the filesystem (default: derived from the source)

**Options:**

- `--backend <BACKEND>` — Backend to use for mounting (default: fuse on Linux, nfs on macOS) [possible values: fuse, nfs; default: fuse]
- `--verify` — Verify `git status` is clean through the mount before finishing

### agentfs mount

Mount an agent filesystem using FUSE (or list mounts if no args)

```
agentfs mount [OPTIONS] [ID_OR_PATH] [MOUNTPOINT]
```

**Arguments:**

- `[ID_OR_PATH]` — Agent ID or database path (if omitted, lists current mounts)
- `[MOUNTPOINT]` — Mount point directory

**Options:**

- `-a, --auto-unmount` — Automatically unmount on exit
- `--allow-root` — Allow root user to access filesystem
- `--system` — Allow other system users to access this mount (requires /etc/fuse.conf user_allow_other; use cautiously)
- `-f, --foreground` — Run in foreground (don't daemonize)
- `--uid <UID>` — User ID to report for all files (defaults to current user)
- `--gid <GID>` — Group ID to report for all files (defaults to current group)
- `--backend <BACKEND>` — Backend to use for mounting [possible values: fuse, nfs; default: fuse]
- `--partial-origin <MODE>` — Partial-origin policy for base-file writes: off, on, or auto [possible values: off, on, auto]
- `--partial-origin-threshold-bytes <BYTES>` — Size threshold for --partial-origin auto
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: AGENTFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: AGENTFS_CIPHER]

### agentfs diff

Show differences between base filesystem and delta (overlay mode only)

```
agentfs diff <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

### agentfs timeline

Display agent action timeline from tool call audit log

```
agentfs timeline [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--limit <LIMIT>` — Limit number of entries to display [default: 100]
- `--filter <FILTER>` — Filter by tool name
- `--status <STATUS>` — Filter by status (pending/success/error) [possible values: pending, success, error]
- `--format <FORMAT>` — Output format [possible values: table, json; default: table]

### agentfs nfs

Start an NFS server to export an AgentFS filesystem over the network (deprecated: use `agentfs serve nfs` instead)

```
agentfs nfs [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--bind <BIND>` — IP address to bind to [default: 127.0.0.1]
- `--port <PORT>` — Port to listen on [default: 11111]
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: AGENTFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: AGENTFS_CIPHER]

### agentfs mcp-server

Start an MCP server exposing filesystem and KV-store tools (deprecated: use `agentfs serve mcp` instead)

```
agentfs mcp-server [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--tools <TOOLS>` — Tools to expose (comma-separated). If not provided, all tools are exposed. Available tools: read_file, write_file, readdir, mkdir, remove, rename, stat, access, kv_get, kv_set, kv_delete, kv_list

### agentfs serve

Serve an AgentFS filesystem via different protocols

```
agentfs serve <COMMAND>
```

#### agentfs serve nfs

Start an NFS server to export an AgentFS filesystem over the network

```
agentfs serve nfs [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--bind <BIND>` — IP address to bind to [default: 127.0.0.1]
- `--port <PORT>` — Port to listen on [default: 11111]
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: AGENTFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: AGENTFS_CIPHER]

#### agentfs serve mcp

Start an MCP server exposing filesystem and KV-store tools

```
agentfs serve mcp [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--tools <TOOLS>` — Tools to expose (comma-separated). If not provided, all tools are exposed. Available tools: read_file, write_file, readdir, mkdir, remove, rename, stat, access, kv_get, kv_set, kv_delete, kv_list

### agentfs ps

List active agentfs run sessions

```
agentfs ps
```

### agentfs prune

Prune unused resources

```
agentfs prune <COMMAND>
```

#### agentfs prune mounts

Unmount unused agentfs mount points

```
agentfs prune mounts [OPTIONS]
```

**Options:**

- `--force` — Skip confirmation prompt and unmount immediately

### agentfs integrity

Check a local AgentFS database for SQLite and schema corruption

```
agentfs integrity [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--json` — Emit machine-readable JSON
- `--require-portable` — Fail if the database depends on external partial-origin base files
- `--check-base` — Validate partial-origin base file fingerprints against the current base tree
- `--checkpoint` — Checkpoint the WAL and remove empty SQLite sidecars after checks pass
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases
- `--cipher <CIPHER>` — Encryption cipher (required with --key)

### agentfs backup

Create a portable local AgentFS database backup

```
agentfs backup [OPTIONS] <ID_OR_PATH> <TARGET>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path
- `<TARGET>` — Target database path to create

**Options:**

- `--verify` — Reopen and verify the copied main database
- `--materialize` — Materialize partial-origin files into a portable backup
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases
- `--cipher <CIPHER>` — Encryption cipher (required with --key)

### agentfs materialize

Create a portable database by materializing partial-origin files

```
agentfs materialize [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--output <OUTPUT>` — Target database path to create
- `--verify` — Reopen and verify the materialized database
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases
- `--cipher <CIPHER>` — Encryption cipher (required with --key)

### agentfs migrate

Migrate database schema to the current version

```
agentfs migrate [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--dry-run` — Preview migration without applying changes
- `--copy <TARGET>` — Copy-migrate into a new database file at this path instead of migrating in place
- `--verify` — Verify migrated state equivalence (requires --copy)
- `--overwrite-target` — Allow replacing an existing --copy target database
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases
- `--cipher <CIPHER>` — Encryption cipher (required with --key)

<!-- END GENERATED COMMAND REFERENCE -->

## Sandboxing (`agentfs run`)

`agentfs run` scopes both writes and reads at the OS level; the mechanism
differs by platform.

**Linux (first-tier):** FUSE + overlay inside user and mount namespaces.
Writes land only in the copy-on-write overlay and the allowed directories.
Reads are scoped by hiding the home directory and temp dirs behind
namespace-private tmpfs, re-exposing only the overlay cwd and the allowed
paths; all other system paths are remounted read-only.

**macOS (second-tier):** NFS mount + a generated `sandbox-exec` (Seatbelt)
profile. Writes are restricted to the mountpoint, temp directories,
`~/Library`, and the allowed paths. Reads are default-deny: only the session
directory (`~/.agentfs/run/<ID>`), the allowed directories (the defaults plus
`--allow`), and a curated set of platform roots are readable (system
frameworks and libraries, the dyld shared cache cryptex, executable
directories, `/private/etc`, terminfo/locale data under `/usr/share`, temp
directories, `/dev` essentials, `/opt`, `/usr/local`, and `/Applications`).
Ancestors of readable roots are stat-able (metadata only) so path resolution
works. Notable consequences:

- The rest of your home directory, including `~/Library` and credential
  stores such as `~/.ssh` or `~/.aws`, is unreadable unless granted with
  `--allow`. (`~/Library` remains writable for Keychain and preferences
  compatibility, but is not readable.)
- Tools that need read access outside the workspace must be granted it
  explicitly with `--allow <PATH>`, which grants read and write access,
  matching Linux.

CI covers the macOS read posture only via unit tests that pin the generated
profile; the runtime behavior is verified by the manual macOS release gate
(`scripts/validation/macos-nfs-git-validation.sh`), which includes a
read-scoping leg: a secret outside the allow list must be unreadable, and
`--allow` must make it readable. See [docs/TESTING.md](TESTING.md).

## Runtime Knobs and Environment Variables

Every runtime knob (env var or first-class flag) is declared in the generated
[docs/KNOBS.md](KNOBS.md) ledger with its class, default, owner, and gate.
`AGENTFS_KEY` / `AGENTFS_CIPHER` provide default encryption credentials for
the commands whose `--key` / `--cipher` options declare them (see the
generated sections above); `TURSO_DB_AUTH_TOKEN` authenticates cloud sync.

### FUSE-over-io_uring and rapid remounts

On Linux kernels with `fuse.enable_uring=1` (the `AGENTFS_FUSE_URING` knob
controls whether AgentFS uses the transport), the kernel drains a just-closed
FUSE connection for roughly two seconds, and a new mount racing that drain can
block inside `mount(2)` indefinitely (observed on kernel 7.1.2). AgentFS
bounds this: the mount is retried for a few seconds and then fails with a
clear error instead of hanging. If rapid unmount-then-mount cycles keep
hitting the error, wait a couple of seconds between cycles or set
`AGENTFS_FUSE_URING=0` on the mount-owning processes. A mount left wedged by
other tooling can be recovered with
`echo 1 > /sys/fs/fuse/connections/<id>/abort` (verify the connection id
first).

### Temp files (`TMPDIR`)

The `turso_core` dependency (0.5.3) leaks `tursodb-ephemeral-*` sort-spill
files into the temp dir and never unlinks them (`vdbe/execute.rs:10096`). The
CLI therefore points its own `TMPDIR` at a private per-process directory that
is removed on exit, so hosts do not accumulate spill litter. This override is
process-internal: commands spawned by `agentfs run`, `agentfs exec`, and
`agentfs init -c` see the original `TMPDIR`. Stale spill directories from
`SIGKILL`ed processes are garbage-collected on the next CLI start.

Variables set inside an `agentfs run` sandbox:

| Variable | Description |
|----------|-------------|
| `AGENTFS` | Set to `1` inside the AgentFS sandbox |
| `AGENTFS_SANDBOX` | Sandbox type: `linux-namespace` or `macos-sandbox` |
| `AGENTFS_SESSION` | Current session ID |

## Local Encryption

AgentFS supports encrypting the local SQLite database at rest.

**Supported ciphers:**

- `aes256gcm` - AES-256-GCM (requires 64-character hex key)
- `aes128gcm` - AES-128-GCM (requires 32-character hex key)
- `aegis256` - AEGIS-256 (requires 64-character hex key)
- `aegis128l` - AEGIS-128L (requires 32-character hex key)
- `aegis128x2`, `aegis128x4`, `aegis256x2`, `aegis256x4` - AEGIS variants

**Example: Create an encrypted filesystem**

```bash
# Generate a 256-bit key (64 hex characters)
KEY=$(openssl rand -hex 32)

# Initialize with encryption
agentfs init --key $KEY --cipher aes256gcm my-secure-agent

# Access the filesystem
agentfs fs my-secure-agent --key $KEY --cipher aes256gcm ls /
```

**Example: Encrypted sandbox session**

```bash
agentfs run --key $KEY --cipher aes256gcm -- bash
```

**Using environment variables:**

```bash
export AGENTFS_KEY=$(openssl rand -hex 32)
export AGENTFS_CIPHER=aes256gcm

agentfs init my-secure-agent
agentfs fs my-secure-agent ls /
```

**Limitations:**

- Local encryption cannot be used with cloud sync (`--sync-remote-url`)

## Files

- `.agentfs/<ID>.db` - Agent filesystem database (relative to the working
  directory where `agentfs init` ran)
- `~/.agentfs/run/` - `agentfs run` session state (listed by `agentfs ps`)

## Unmounting

- Linux (FUSE): `fusermount3 -u <MOUNT_POINT>` (or `fusermount -u`)
- macOS (NFS): `umount <MOUNT_POINT>`

## See Also

- [Agent Filesystem Specification](SPEC.md) - SQLite schema specification
- [Runtime Knobs](KNOBS.md) - generated knob ledger
- [Testing](TESTING.md) - validation gates, benchmarks, and the manual
  macOS release gate
