# AgentFS

The filesystem for agents: a SQLite-backed virtual filesystem with
copy-on-write sandboxing, mountable over FUSE (Linux) or NFS.

> **⚠️ Warning:** This software is in BETA. It may still contain bugs and unexpected behavior. Use caution with production data and ensure you have backups.

## What is AgentFS?

AgentFS stores everything an agent does — every file it creates, every piece
of key-value state, every tool call it records — in a single SQLite database
file. That gives agent state three properties ordinary filesystems don't
have:

* **Auditability**: every file operation and tool call is queryable with SQL.
* **Reproducibility**: snapshot an agent's state by copying one file; restore
  it to reproduce exact execution states or roll back mistakes.
* **Portability**: the entire agent runtime (files, state, history) moves
  between machines as a single `.db` file.

For sandboxed coding-agent workloads, AgentFS layers that database over a
read-only view of the host filesystem: reads are scoped to the configured
base tree, writes go only to the AgentFS delta database, and the real
filesystem is never modified. This holds even against prompt-injected agents:
the isolation is enforced at the filesystem layer, not by convention.

## Repository layout

This repository is one Cargo workspace with five crates:

| Crate | Role |
|---|---|
| `crates/agentfs-core` | The engine: storage, overlay/copy-on-write, schema authority, typed config, telemetry, semantics (access/durability/handles). The only crate meant for external consumption. |
| `crates/agentfs-fuse` | Sealed Linux FUSE mount surface (transport + adapter). |
| `crates/agentfs-nfs` | Sealed NFSv3 serve surface (transport + adapter). |
| `crates/agentfs-mount` | One mount lifecycle: `mount_fs`, `MountHandle`, supervision, daemonize. |
| `crates/agentfs-cli` | The `agentfs` binary: thin CLI edge over the crates above. |

Platform support: **Linux is first-tier** (FUSE and NFS backends, `agentfs
run` sandbox, full validation gate). **macOS is second-tier**: NFS mount
only, validated by a manual release gate
(`scripts/validation/macos-nfs-git-validation.sh`) run on real hardware —
see [docs/TESTING.md](docs/TESTING.md). No other platforms are supported.

## Getting started

Build and install the CLI from source:

```bash
cargo +nightly build --release --workspace --bins
install -m 0755 target/release/agentfs ~/.local/bin/
```

Initialize an agent filesystem:

```bash
$ agentfs init my-agent
Created agent filesystem: .agentfs/my-agent.db
Agent ID: my-agent
```

Inspect it without mounting:

```bash
$ agentfs fs my-agent ls
f hello.txt

$ agentfs fs my-agent cat hello.txt
hello from agent

# Or address the database file directly
$ agentfs fs .agentfs/my-agent.db cat hello.txt
hello from agent
```

View the agent's tool-call timeline:

```bash
$ agentfs timeline my-agent
ID   TOOL                 STATUS       DURATION STARTED
4    execute_code         pending            -- 2024-01-05 09:44:20
3    api_call             error           300ms 2024-01-05 09:44:15
2    read_file            success          50ms 2024-01-05 09:44:10
1    web_search           success        1200ms 2024-01-05 09:43:45
```

Mount it as a real filesystem (FUSE on Linux, NFS on macOS):

```bash
$ agentfs mount my-agent ./mnt
$ echo "hello" > ./mnt/hello.txt
$ cat ./mnt/hello.txt
hello
```

Run a program in a copy-on-write sandbox over your current directory
(FUSE + user/mount namespaces on Linux):

```bash
$ agentfs run --session my-session -- bash
# ... work normally; every write lands in the delta database,
# the host filesystem is untouched ...
$ exit

# List live sandbox sessions; inspect what a session changed
$ agentfs ps
$ agentfs diff ~/.agentfs/run/my-session/delta.db
```

The **[User Manual](docs/MANUAL.md)** documents every command; its command
reference is generated from the CLI's own argument definitions, so it cannot
drift from `agentfs --help`.

## Using AgentFS as a library

The `agentfs-core` crate provides programmatic access to the same engine the
CLI uses: filesystem, key-value store, and tool-call audit trail over a
single database. See the crate rustdoc (`cargo doc -p agentfs-core`).

## How it works

At the heart of AgentFS is the [agent filesystem](docs/SPEC.md), a complete
SQLite-based storage system implemented on
[Turso](https://github.com/tursodatabase/turso). The database schema
separates namespace (dentries) from data (inodes + chunked/inline content),
which enables hard links, POSIX metadata, sparse files, and SQL-queryable
history.

On Linux, the FUSE backend dispatches requests through a bounded worker pool
with a read/write lane split, kernel-cache acceleration (entry/attr TTLs,
writeback cache, readdirplus), zero-message opens, and an optional
FUSE-over-io_uring transport. All acceleration structures are reconstructible
from the database; the two safety properties — one portable database holds
all virtual filesystem state, and sandboxed writes never reach the host
filesystem — hold regardless of cache configuration. Runtime tunables are
declared in the generated [docs/KNOBS.md](docs/KNOBS.md) ledger.

## FAQ

### How is AgentFS different from _X_?

[Bubblewrap](https://github.com/containers/bubblewrap) provides filesystem isolation using Linux namespaces and overlays. While you could achieve similar isolation with a `bwrap` call that mounts `/` read-only and uses `--tmp-overlay` on the working directory, the key difference is persistence and queryability: with AgentFS, the upper filesystem is stored in a single SQLite database file, which you can query, snapshot, and move to another machine.

[Docker Sandbox](https://www.docker.com/blog/docker-sandboxes-a-new-approach-for-coding-agent-safety/) and AgentFS are complementary rather than competing. AgentFS answers "what happened and what's the state?" while Docker Sandboxes answer "how do I run this safely?" You could use both together: run an agent inside a Docker Sandbox for security, while using AgentFS inside that sandbox for structured state management and audit trails.

[Git worktrees](https://git-scm.com/docs/git-worktree) let you check out multiple branches of a repository into separate directories, allowing agents to work on independent copies of the source code — similar to AgentFS. But AgentFS solves the problem at a lower level. With git worktrees, nothing prevents an agent from modifying files outside its worktree: another agent's worktree, system files, or anything else on the filesystem. The isolation is purely conventional, not enforced. AgentFS provides filesystem-level copy-on-write isolation that cannot be bypassed, and because it operates below git, it also handles untracked files.

### Why implement AgentFS at the filesystem layer instead of using containers or VMs?

The filesystem layer gives us capabilities that block devices can't. First, because everything is stored in structured SQLite tables, you can query the filesystem, which is essential for auditability and debugging agent behavior. Second, SQLite's write-ahead log enables snapshotting and time-travel forking by capturing every filesystem change. Third, the engine works in environments where mounting a block device is impossible. This approach also composes with containers and VMs: AgentFS is reachable over remote filesystem protocols like NFS or through mechanisms like virtio-fuse.

## Learn more

- **[User Manual](docs/MANUAL.md)** - complete CLI reference (generation-checked against the binary)
- **[Agent Filesystem Specification](docs/SPEC.md)** - the SQLite schema and runtime invariants
- **[Runtime Knobs](docs/KNOBS.md)** - generated ledger of every tunable
- **[Testing](docs/TESTING.md)** - validation gates, benchmark policy, and the manual macOS release gate
- **[CHANGELOG](CHANGELOG.md)** - including the fork-era restructure summary
- **[Turso database](https://github.com/tursodatabase/turso)** - the in-process SQL database AgentFS builds on

## License

MIT
