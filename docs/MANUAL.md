# Vfs Reference Guide

Command-line reference for the Vfs CLI.

The command and option sections between the `GENERATED COMMAND REFERENCE`
markers are rendered from the CLI's clap definitions and checked by
`docs::tests::manual_help_parity`, so this manual always matches
`vfs --help`.

## Installation

Build from source (Linux is the first-tier platform; macOS is second-tier,
NFS mount only):

```bash
cargo +nightly build --release --workspace --bins
install -m 0755 target/release/vfs ~/.local/bin/
```

<!-- BEGIN GENERATED COMMAND REFERENCE (do not edit by hand) -->
<!-- Regenerate with: `VFS_UPDATE_MANUAL=1 cargo +nightly test -p vfs-cli --lib docs::tests::manual_help_parity -- --exact` -->

## Commands

Every section below is generated from the clap definitions the binary actually parses; `vfs <command> --help` and this reference cannot disagree.

### vfs completions

Manage shell completions (supported shells: bash, zsh, fish, elvish, powershell)

```
vfs completions <COMMAND>
```

#### vfs completions install

Install shell completions to your shell rc file

```
vfs completions install [SHELL]
```

**Arguments:**

- `[SHELL]` — Shell to install completions for (defaults to current shell) [possible values: bash, zsh, fish, elvish, power-shell]

#### vfs completions uninstall

Uninstall shell completions from your shell rc file

```
vfs completions uninstall [SHELL]
```

**Arguments:**

- `[SHELL]` — Shell to uninstall completions for (defaults to current shell) [possible values: bash, zsh, fish, elvish, power-shell]

#### vfs completions show

Print instructions for manual installation

```
vfs completions show
```

### vfs init

Initialize a new agent filesystem

```
vfs init [OPTIONS] [ID]
```

**Arguments:**

- `[ID]` — Agent identifier (if not provided, generates a unique one)

**Options:**

- `--force` — Overwrite existing file if it exists
- `--base <BASE>` — Base directory for overlay filesystem (copy-on-write)
- `--key <KEY>` — Hex-encoded encryption key. Enables local encryption when provided [env: VFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: VFS_CIPHER]
- `-c, --command <COMMAND>` — Command to execute after initialization (mounts the filesystem, runs command, unmounts)
- `--backend <BACKEND>` — Backend to use for mounting when using -c (default: fuse on Linux, nfs on macOS) [possible values: fuse, nfs; default: fuse]
- `--sync-remote-url <SYNC_REMOTE_URL>`
- `--sync-partial-prefetch <SYNC_PARTIAL_PREFETCH>` [possible values: true, false]
- `--sync-partial-segment-size <SYNC_PARTIAL_SEGMENT_SIZE>`
- `--sync-partial-bootstrap-query <SYNC_PARTIAL_BOOTSTRAP_QUERY>`
- `--sync-partial-bootstrap-length <SYNC_PARTIAL_BOOTSTRAP_LENGTH>`

### vfs sync

Remote sync operations

```
vfs sync <ID_OR_PATH> <COMMAND>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

#### vfs sync pull

Pull remote changes (only of vfs was initialized with remote sync)

```
vfs sync <ID_OR_PATH> pull
```

#### vfs sync push

Push remote changes (only of vfs was initialized with remote sync)

```
vfs sync <ID_OR_PATH> push
```

#### vfs sync stats

Print synced database stats

```
vfs sync <ID_OR_PATH> stats
```

#### vfs sync checkpoint

Checkpoint local synced db

```
vfs sync <ID_OR_PATH> checkpoint
```

### vfs fs

Filesystem operations

```
vfs fs [OPTIONS] <ID_OR_PATH> <COMMAND>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: VFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: VFS_CIPHER]

#### vfs fs ls

List files in the filesystem

```
vfs fs <ID_OR_PATH> ls [FS_PATH]
```

**Arguments:**

- `[FS_PATH]` — Path to list (default: /) [default: /]

#### vfs fs cat

Display file contents

```
vfs fs <ID_OR_PATH> cat <FILE_PATH>
```

**Arguments:**

- `<FILE_PATH>` — Path to the file in the filesystem

#### vfs fs write

Write file content

```
vfs fs <ID_OR_PATH> write <FILE_PATH> <CONTENT>
```

**Arguments:**

- `<FILE_PATH>` — Path to the file in the filesystem
- `<CONTENT>` — Content of the file

### vfs run

Run a command in the sandboxed environment.

By default, uses FUSE+overlay with Linux user and mount namespaces for isolation. The overlay uses the host filesystem as a read-only base and stores all changes in a Vfs-backed delta layer. On macOS the overlay is mounted over NFS and a generated Seatbelt profile scopes writes to the sandbox and reads to the allowed directories plus required platform paths (see the Sandboxing section of docs/MANUAL.md).

```
vfs run [OPTIONS] [COMMAND] [ARGS]...
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
- `--key <KEY>` — Hex-encoded encryption key for the delta layer. Enables local encryption when provided [env: VFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: VFS_CIPHER]
- `--seed-pin <COMMIT>` — Seed dirty and local-commit state against this git commit before mounting. Ignored files are not part of portable state

### vfs exec

Execute a command with a Vfs filesystem mounted.

Mounts the specified Vfs to a temporary directory, runs the command with that directory as the working directory, then automatically unmounts. This is useful for running tools that need filesystem access without a persistent mount.

```
vfs exec [OPTIONS] <ID_OR_PATH> <COMMAND> [ARGS]...
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path
- `<COMMAND>` — Command to execute
- `[ARGS]...` — Arguments for the command

**Options:**

- `--backend <BACKEND>` — Backend to use for mounting (default: fuse on Linux, nfs on macOS) [possible values: fuse, nfs; default: fuse]
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: VFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key) [env: VFS_CIPHER]

### vfs clone

Clone a git repository into a Vfs database (fast bulk ingest).

Runs `git clone --no-checkout` through a temporary mount (pack files are large sequential writes), then materializes the worktree by bulk-importing blobs straight into the database in large transactions and fabricating a matching git index, skipping the per-file FUSE round trips of a regular checkout. The resulting repository lives entirely inside the database; nothing is written to the host filesystem. Submodules and smudge/clean filters are not supported.

```
vfs clone [OPTIONS] <ID_OR_PATH> <SOURCE> [NAME]
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path (created if it does not exist)
- `<SOURCE>` — Git repository to clone (URL or local path)
- `[NAME]` — Directory name for the repository inside the filesystem (default: derived from the source)

**Options:**

- `--backend <BACKEND>` — Backend to use for mounting (default: fuse on Linux, nfs on macOS) [possible values: fuse, nfs; default: fuse]
- `--verify` — Verify `git status` is clean through the mount before finishing

### vfs mount

Mount an agent filesystem using FUSE (or list mounts if no args)

```
vfs mount [OPTIONS] [ID_OR_PATH] [MOUNTPOINT]
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
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: VFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: VFS_CIPHER]

### vfs diff

Show differences between base filesystem and delta (overlay mode only)

```
vfs diff <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

### vfs timeline

Display agent action timeline from tool call audit log

```
vfs timeline [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--limit <LIMIT>` — Limit number of entries to display [default: 100]
- `--filter <FILTER>` — Filter by tool name
- `--status <STATUS>` — Filter by status (pending/success/error) [possible values: pending, success, error]
- `--format <FORMAT>` — Output format [possible values: table, json; default: table]

### vfs nfs

Start an NFS server to export a Vfs filesystem over the network (deprecated: use `vfs serve nfs` instead)

```
vfs nfs [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--bind <BIND>` — IP address to bind to [default: 127.0.0.1]
- `--port <PORT>` — Port to listen on [default: 11111]
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: VFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: VFS_CIPHER]

### vfs mcp-server

Start an MCP server exposing filesystem and KV-store tools (deprecated: use `vfs serve mcp` instead)

```
vfs mcp-server [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--tools <TOOLS>` — Tools to expose (comma-separated). If not provided, all tools are exposed. Available tools: read_file, write_file, readdir, mkdir, remove, rename, stat, access, kv_get, kv_set, kv_delete, kv_list

### vfs serve

Serve a Vfs filesystem via different protocols

```
vfs serve <COMMAND>
```

#### vfs serve nfs

Start an NFS server to export a Vfs filesystem over the network

```
vfs serve nfs [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--bind <BIND>` — IP address to bind to [default: 127.0.0.1]
- `--port <PORT>` — Port to listen on [default: 11111]
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases [env: VFS_KEY]
- `--cipher <CIPHER>` — Cipher algorithm for encryption (required with --key). Options: aegis128l, aegis128x2, aegis128x4, aegis256, aegis256x2, aegis256x4, aes128gcm, aes256gcm [env: VFS_CIPHER]

#### vfs serve mcp

Start an MCP server exposing filesystem and KV-store tools

```
vfs serve mcp [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--tools <TOOLS>` — Tools to expose (comma-separated). If not provided, all tools are exposed. Available tools: read_file, write_file, readdir, mkdir, remove, rename, stat, access, kv_get, kv_set, kv_delete, kv_list

### vfs pack

Prepare an inactive run session database for transfer

```
vfs pack [OPTIONS] <SESSION_ID>
```

**Arguments:**

- `<SESSION_ID>` — Run session identifier

**Options:**

- `--prune <GLOB>` — Additional delta path glob to prune (can be specified multiple times)
- `--no-default-prunes` — Disable the default generated-artifact prune globs
- `--output <PATH>` — Copy the packed database to this path
- `--json` — Emit machine-readable JSON (pack output is always JSON)

### vfs seed

Capture a run session's live git state into its portable delta.

Compares the session base checkout with --pin, imports dirty files and local commits without mounting, and records deletions as whiteouts. Ignored files are not part of portable state.

```
vfs seed [OPTIONS] <SESSION_ID>
```

**Arguments:**

- `<SESSION_ID>` — Run session identifier

**Options:**

- `--pin <COMMIT>` — Git commit used as the pristine portable base
- `--json` — Emit machine-readable JSON (seed output is always JSON)

### vfs adopt

Install an externally transferred run session.

Verifies a packed session database against the receiver's base git checkout (the checkout's HEAD must equal the artifact's recorded seed pin), migrates supported older artifact schemas to the current version, and atomically publishes ~/.vfs/run/<SESSION_ID>. After adopt, `vfs run --session <SESSION_ID>` resumes the transferred session.

```
vfs adopt [OPTIONS] <SESSION_ID>
```

**Arguments:**

- `<SESSION_ID>` — Run session identifier

**Options:**

- `--db <PATH>` — Packed session database produced by `vfs pack`
- `--base <PATH>` — The receiver's base git checkout for the session
- `--pin <COMMIT>` — Git commit the base checkout must be at (required only when the artifact does not record a seed pin)
- `--json` — Emit machine-readable JSON (adopt output is always JSON)

### vfs status

Show run session state for daemon preflight

```
vfs status [OPTIONS] <SESSION_ID>
```

**Arguments:**

- `<SESSION_ID>` — Run session identifier

**Options:**

- `--json` — Emit machine-readable JSON (status output is always JSON)
- `--key <KEY>` — Hex-encoded encryption key for an encrypted session database [env: VFS_KEY]
- `--cipher <CIPHER>` — Encryption cipher (required with --key) [env: VFS_CIPHER]

### vfs version

Show vfs version and feature capabilities

```
vfs version [OPTIONS]
```

**Options:**

- `--json` — Emit machine-readable JSON

### vfs ps

List active vfs run sessions

```
vfs ps
```

### vfs prune

Prune unused resources

```
vfs prune <COMMAND>
```

#### vfs prune mounts

Unmount unused vfs mount points

```
vfs prune mounts [OPTIONS]
```

**Options:**

- `--force` — Skip confirmation prompt and unmount immediately

### vfs integrity

Check a local Vfs database for SQLite and schema corruption

```
vfs integrity [OPTIONS] <ID_OR_PATH>
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

### vfs backup

Create a portable local Vfs database backup

```
vfs backup [OPTIONS] <ID_OR_PATH> <TARGET>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path
- `<TARGET>` — Target database path to create

**Options:**

- `--verify` — Reopen and verify the copied main database
- `--materialize` — Materialize partial-origin files into a portable backup
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases
- `--cipher <CIPHER>` — Encryption cipher (required with --key)

### vfs materialize

Create a portable database by materializing partial-origin files

```
vfs materialize [OPTIONS] <ID_OR_PATH>
```

**Arguments:**

- `<ID_OR_PATH>` — Agent ID or database path

**Options:**

- `--output <OUTPUT>` — Target database path to create
- `--verify` — Reopen and verify the materialized database
- `--key <KEY>` — Hex-encoded encryption key for encrypted databases
- `--cipher <CIPHER>` — Encryption cipher (required with --key)

### vfs migrate

Migrate database schema to the current version

```
vfs migrate [OPTIONS] <ID_OR_PATH>
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

## Seeding run sessions

`vfs seed <session-id> --pin <commit>` captures the live checkout state that
would otherwise be visible only through overlay base read-through. It imports
dirty and untracked files, records paths deleted since the pin as whiteouts,
and includes committed changes between the pin and `HEAD`. The import uses the
core bulk-import API directly; it never requires a FUSE or NFS mount.

For session creation, prefer the atomic startup form:

```bash
vfs run --session <session-id> --seed-pin <commit> -- <command>
```

`vfs run` creates and seeds the delta under the session's exclusive lock, then
atomically downgrades that lock to the lifetime shared run lock before mounting
or starting the wrapped process. Seed writes a private staging database and
publishes it only after import, whiteouts, metadata, and finalization succeed,
so a failed seed leaves the live delta retryable. The standalone `vfs seed`
form is available when a session directory, `base_path`, and `delta.db` already
exist and the session is inactive.

Git-ignored files are not part of portable state. Seed uses
`git status --untracked-files=all`, so ignored build outputs, dependency trees,
and caches remain base-local. For local-only commits, seed writes a compact Git
pack plus `HEAD` and the current branch ref into delta `.git/` paths. It also
copies the sender's index bytes, preserving staged versus unstaged state; Git
revalidates cached stat fields when the delta is materialized over a pristine
checkout at the pin.

Seed records the resolved pin inside the session database as the session's
base provenance; `vfs adopt` verifies the receiving base checkout against it.

Seed is a birth-time operation. A second call fails with
`session already seeded`, and a live mount or wrapped process fails with the
same exit-code-3 teardown gate used by `vfs pack`.

## Packing run sessions

`vfs pack <session-id>` prepares `~/.vfs/run/<session-id>/delta.db` as a
single-file transfer artifact. Pack owns the operation end to end:

1. Every owner/joiner holds a shared advisory lock on `.session.lock` for its
   lifetime; pack must acquire the exclusive lock before it can inspect or
   publish the session. Kernel lock release on process death prevents stale
   lock files from wedging a session.
2. It rejects live mounts and live owner/joiner proc records with exit code
   `3` and the message `session still running; exit the wrapped process first`.
3. It copies the SQLite main database and any WAL/SHM sidecars to a private
   staging family. Pruning, schema migration, generation increment, checkpoint,
   and compaction happen only on that copy.
4. Pruning removes matching delta dentries/inodes through the core filesystem
   API. It never creates whiteouts: a pruned delta-only path disappears, while
   a pruned path that shadowed the base falls back to the base version.
5. The Turso engine currently implements compaction as `VACUUM INTO` rather
   than in-place `VACUUM`; pack checkpoints the resulting database and removes
   its sidecars before publication.
6. Publication renames the old live database family to a deterministic private
   backup, renames the completed single-file staging database into place,
   verifies its metadata, and rolls back on any publication failure. A later
   pack recovers that backup if the process died between renames. An `--output`
   artifact is copied from the same completed staging bytes and published with
   a no-replace hard link, so a concurrently-created target is never
   overwritten. Backup cleanup occurs only after both publications succeed.

The packed transfer artifact is only `delta.db`; `procs/`, `mnt/`,
`.session.lock`, `base_path`, and other session-directory files are never
included in that artifact.

### Adopting run sessions

`vfs adopt <session-id> --db <path> --base <path>` installs a transferred
pack artifact as a local run session. Adopt owns the receiver side of a
handoff end to end:

1. It refuses a session ID whose `~/.vfs/run/<session-id>/delta.db` already
   exists, and takes the same exclusive `.session.lock` used by `run`, `seed`,
   and `pack`, so a live owner fails with exit code `3`.
2. It stages a private copy of the artifact, checks SQLite integrity, and
   migrates supported older artifact schemas to the current version — the
   same staging migration `vfs pack` performs before it ships an artifact, so
   an adopted session always opens under `vfs run` without a separate
   `vfs migrate` step. An artifact written by a newer vfs is refused with an
   error naming both versions.
3. It verifies base provenance: `--base` must be a git checkout whose `HEAD`
   equals the seed pin recorded in the artifact by `vfs seed`. For artifacts
   without recorded provenance, `--pin <commit>` is required and verified the
   same way; a `--pin` that contradicts recorded provenance is refused. A
   base checkout at any other commit is refused before anything is published.
4. Publication writes `base_path` and then atomically renames the staged
   database to `delta.db`. That rename is the commit point: on any earlier
   failure a session directory adopt created is removed again, so a partial
   or corrupt session is never observable.

Adopt prints a one-line JSON manifest:

```json
{
  "manifestVersion": 1,
  "sessionId": "<session-id>",
  "basePath": "/abs/receiver/checkout",
  "basePin": "<commit>",
  "generation": 1,
  "schemaVersion": "0.6",
  "seededPaths": ["src/lib.rs"],
  "vfsVersion": "0.6.4"
}
```

After adopt, resume the session with:

```bash
vfs run --session <session-id> -- <command>
```

The installed layout — `delta.db` plus a `base_path` text file naming the
receiver checkout, with `mnt/`, `procs/`, `.session.lock`, and
`runtime-status.json` created lazily by `vfs run` — is an internal detail
owned by adopt; sessions must be installed through `vfs adopt`, not by
writing those files by hand.

### Run session status

`vfs status <session-id> --json` emits one JSON object:

```json
{
  "sessionId": "<session-id>",
  "state": "stopped",
  "mounted": false,
  "pid": null,
  "generation": 1,
  "seeded": false
}
```

`state` is `stopped`, `busy`, `live`, or `stale-recovered`. `busy` means an
exclusive seed, pack, or first-start transition owns the session; its `pid` is
`null`, and metadata fields are conservative defaults until the transition
finishes. For `live`, `pid` is the run owner PID. `generation` is the pack
generation (`0` before the first pack), and `seeded` reports whether seed
metadata is present. Status preflight takes the session lock; when no owner
exists, it also recovers an interrupted pack publication, detaches a stale
mount, and removes stale process records before reporting `stale-recovered`.

## Exit status

`vfs run` preserves the wrapped command's exit status. A command terminated by
a signal uses the conventional `128 + signal` status. Startup failures use
these reserved statuses before the wrapped command begins:

| Status | Meaning |
|--------|---------|
| `1` | General CLI failure not assigned a more specific status |
| `2` | Command-line usage or argument parsing failure |
| `3` | The requested session is genuinely live; `run`, `pack`, `seed`, or `adopt` cannot take exclusive ownership |
| `4` | `vfs run` could not create, recover, or install the session mount/sandbox |
| `5` | The requested run session ID or adopted session directory is missing, malformed, or invalid |
| `126` | The wrapped command was found but could not be executed |
| `127` | The wrapped command was not found |

Once the wrapped command starts, its status is passed through unchanged and
may numerically coincide with a reserved startup status.

## MCP Server (`vfs serve mcp`)

The `write_file` tool overwrites existing files in place and keeps their
mode; files it creates get the default mode `0644` (`rw-r--r--`).

## Sandboxing (`vfs run`)

`vfs run` scopes both writes and reads at the OS level; the mechanism
differs by platform.

**Linux (first-tier):** FUSE + overlay inside user and mount namespaces.
Writes land only in the copy-on-write overlay and the allowed directories.
Reads are scoped by hiding the home directory and temp dirs behind
namespace-private tmpfs, re-exposing only the overlay cwd and the allowed
paths; all other system paths are remounted read-only. Writes to those
hidden non-allowed paths land in an ephemeral session-private view (the
namespace tmpfs): the host is protected, but that view is not persisted —
files written there vanish when the session exits and are not visible when
the session is resumed with `--session`.

**macOS (second-tier):** NFS mount + a generated `sandbox-exec` (Seatbelt)
profile. Writes are restricted to the mountpoint, temp directories,
`~/Library`, and the allowed paths. Reads are default-deny: only the session
directory (`~/.vfs/run/<ID>`), the allowed directories (the defaults plus
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
`VFS_KEY` / `VFS_CIPHER` provide default encryption credentials for
the commands whose `--key` / `--cipher` options declare them (see the
generated sections above); `TURSO_DB_AUTH_TOKEN` authenticates cloud sync.

### FUSE-over-io_uring and rapid remounts

On Linux kernels with `fuse.enable_uring=1` (the `VFS_FUSE_URING` knob
controls whether Vfs uses the transport), the kernel drains a just-closed
FUSE connection for roughly two seconds, and a new mount racing that drain can
block inside `mount(2)` indefinitely (observed on kernel 7.1.2). Vfs
bounds this: the mount is retried for a few seconds and then fails with a
clear error instead of hanging. If rapid unmount-then-mount cycles keep
hitting the error, wait a couple of seconds between cycles or set
`VFS_FUSE_URING=0` on the mount-owning processes. A mount left wedged by
other tooling can be recovered with
`echo 1 > /sys/fs/fuse/connections/<id>/abort` (verify the connection id
first).

### Temp files (`TMPDIR`)

The `turso_core` dependency (0.5.3) leaks `tursodb-ephemeral-*` sort-spill
files into the temp dir and never unlinks them (`vdbe/execute.rs:10096`). The
CLI therefore points its own `TMPDIR` at a private per-process directory that
is removed on exit, so hosts do not accumulate spill litter. This override is
process-internal: commands spawned by `vfs run`, `vfs exec`, and
`vfs init -c` see the original `TMPDIR`. Stale spill directories from
`SIGKILL`ed processes are garbage-collected on the next CLI start.

Variables set inside an `vfs run` sandbox:

| Variable | Description |
|----------|-------------|
| `VFS` | Set to `1` inside the Vfs sandbox |
| `VFS_SANDBOX` | Sandbox type: `linux-namespace` or `macos-sandbox` |
| `VFS_SESSION` | Current session ID |

## Local Encryption

Vfs supports encrypting the local SQLite database at rest.

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
vfs init --key $KEY --cipher aes256gcm my-secure-agent

# Access the filesystem
vfs fs my-secure-agent --key $KEY --cipher aes256gcm ls /
```

**Example: Encrypted sandbox session**

```bash
vfs run --key $KEY --cipher aes256gcm -- bash
```

**Using environment variables:**

```bash
export VFS_KEY=$(openssl rand -hex 32)
export VFS_CIPHER=aes256gcm

vfs init my-secure-agent
vfs fs my-secure-agent ls /
```

**Limitations:**

- Local encryption cannot be used with cloud sync (`--sync-remote-url`)

## Files

- `.vfs/<ID>.db` - Agent filesystem database (relative to the working
  directory where `vfs init` ran)
- `~/.vfs/run/` - `vfs run` session state (listed by `vfs ps`)

## Unmounting

- Linux (FUSE): `fusermount3 -u <MOUNT_POINT>` (or `fusermount -u`)
- macOS (NFS): `umount <MOUNT_POINT>`

## See Also

- [Agent Filesystem Specification](SPEC.md) - SQLite schema specification
- [Runtime Knobs](KNOBS.md) - generated knob ledger
- [Testing](TESTING.md) - validation gates, benchmarks, and the manual
  macOS release gate
