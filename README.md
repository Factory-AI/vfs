# Vfs

**The filesystem for agents.** A SQLite-backed virtual filesystem with
copy-on-write sandboxing, mountable over FUSE or NFS — whose live sessions
seal into a single verifiable file and resume on another machine with the
agent's uncommitted work intact.

> **⚠️ Beta.** Use caution with production data and keep backups.

## Teleporting a live session

An agent is mid-task on your laptop: dirty working tree, staged and unstaged
edits split across files, untracked scratch files, a couple of local commits.
Move all of it to another machine:

```console
# --- sender ---------------------------------------------------------------
$ vfs run --session demo --seed-pin "$(git rev-parse HEAD)" -- bash
# ... agent works. exit when it's time to hand off ...

$ vfs pack demo --output /tmp/artifact.db
{"sessionId":"demo","dbSha256":"0c7a79…","dbSizeBytes":122880,"artifactVersion":"0.6",
 "chunks":[{"index":0,"sizeBytes":122880,"sha256":"0c7a79…"}],"basePin":"6b8da73…",
 "seededPaths":[".git/HEAD",".git/index","main.rs","untracked.txt"],"generation":1}

# --- receiver: a pristine checkout at the same commit ---------------------
$ vfs adopt demo --db /tmp/artifact.db --base ~/src/checkout
{"sessionId":"demo","basePin":"6b8da73…","generation":1,"schemaVersion":"0.6", …}

$ vfs run --session demo -- git status --short
 M main.rs
?? inside.txt
?? untracked.txt
```

Same working tree, same staged-vs-unstaged split, same sandbox. The artifact
is a *delta* — it carries what the session changed, not the repository — so
the transfer is proportional to the work done, not to the size of the repo.

Point it at the wrong base and it refuses before touching anything:

```console
$ vfs adopt demo --db /tmp/artifact.db --base ./some-other-commit
Error: base checkout ./some-other-commit is at 3cf0b0a…, but the session requires
pin 6b8da73…; check out the pin before adopting
```

## Forking a live session

Try a risky refactor without betting the session on it. `vfs branch` forks a
session into an independent one that starts at the parent's exact current
state — including a *running* parent, snapshotted through its mount without
stopping it:

```console
$ vfs branch demo --session probe
{"manifestVersion":1,"sessionId":"probe","parentSessionId":"demo",
 "parentArtifactSha256":"4b0dc4a…","artifactPath":"…/.vfs/artifacts/4b0dc4a….db",
 "basePath":"/home/you/src/checkout","seedPin":"6b8da73…","parentLive":true,
 "vfsVersion":"1.0.2"}

$ vfs run --session probe -- bash -c 'printf "risky refactor\n" > experiment.txt'

$ vfs run --session demo -- cat experiment.txt
cat: experiment.txt: No such file or directory
```

The fork is a delta over a frozen, content-addressed snapshot in
`~/.vfs/artifacts/<sha256>.db`, so branches taken at the same state share one
artifact and the branch itself starts empty. Any run of the parent is a new
state (every run leaves an audit row), so fork–run–fork produces two
artifacts. The branch mounts as a stack — its delta over the read-only parent
snapshot over the host base — and every mount re-hashes the snapshot first: a
missing or tampered parent refuses to serve rather than presenting a view
that is not the branched state. Branches of branches chain the same way, and
`vfs pack` of a branch folds the whole chain into one self-contained
artifact, so a forked session teleports like any other. Unreferenced
snapshots are collected with `vfs prune artifacts`.

## Why it's a database

Vfs stores everything an agent does — every file it writes, every piece of
key-value state, every tool call — in one SQLite file. Four properties fall
out that ordinary filesystems don't have:

* **Auditable** — every file operation and tool call is queryable with SQL.
* **Reproducible** — snapshot state by copying one file; restore to replay an
  exact execution or roll back a mistake.
* **Portable** — files, state, and history move between machines as one `.db`.
* **Transferable** — a *running* sandbox session becomes a verifiable
  artifact, mid-flight.

For coding-agent workloads, Vfs layers that database over a read-only view of
the host: reads are scoped to the configured base tree, writes land only in
the delta database, and the real filesystem is never modified. That holds
against a prompt-injected agent too — the isolation is enforced at the
filesystem layer, not by convention.

## What makes the handoff trustworthy

Transferring a live session is easy to do *almost* correctly, and an almost
correct session is worse than a failed one: it looks fine and is subtly wrong.
Five properties keep it honest.

**Dirty state is captured, not read through.** A session over a dirty checkout
sees that dirt only via overlay base read-through, which does not travel.
`seed` imports dirty and untracked files, records deletions as whiteouts, and
ships local-only commits as a compact git pack plus the sender's raw index
bytes — which is what preserves staged-vs-unstaged. Git-ignored files are
excluded by design: build outputs and caches stay base-local.

**Pack is atomic and refuses live sessions.** It takes the exclusive session
lock, rejects live mounts and owner/joiner processes with exit code `3`, then
does every mutation — pruning, migration, generation bump, checkpoint,
compaction — on a private staging copy. Publication is a rename dance with
rollback; a pack that dies between renames is rolled forward by the next one.

**The artifact is content-addressed and version-negotiated.** The manifest
carries a whole-file `dbSha256` plus per-chunk digests over `--chunk-size`
ranges, so a transport can stream, verify chunk-by-chunk, ingest out of order,
and resume across daemon restarts. `vfs version --json` publishes
`artifactVersion` and `minSupportedArtifactVersion`, so two daemons agree on a
floor before a byte moves — a receiver behind on vfs fails preflight instead
of corrupting state.

**Adopt verifies provenance before it publishes.** The receiving checkout's
`HEAD` must equal the seed pin recorded *inside* the artifact. Install is
staged and committed by one rename, so a partial or corrupt session is never
observable — and the store layout stays private to `vfs` rather than becoming
a contract every receiver reimplements.

**Resume recovers, it doesn't assume.** Every start runs a recovery ladder
under the session lock: roll forward an interrupted pack, detach a stale
mount, reap dead proc records. Liveness comes from advisory locks and proc
records that the kernel releases on process death, which is what makes the
classification crash-consistent rather than heuristic. `vfs status --json`
reports `stopped | busy | live | stale-recovered` and runs the same recovery,
so a supervising daemon reads a truthful state:

```console
$ vfs status demo --json
{"sessionId":"demo","state":"stopped","mounted":false,"pid":null,"generation":1,"seeded":true}
```

Startup failures use reserved exit statuses — `3` live, `4` mount/sandbox
install failed, `5` session missing or malformed, `126`/`127` exec conventions
— so a daemon can branch on them. The wrapped command's own status passes
through untouched. Full contract in [docs/MANUAL.md](docs/MANUAL.md).

## Relationship to upstream AgentFS

Vfs is a hard fork of [tursodatabase/agentfs](https://github.com/tursodatabase/agentfs),
diverged at 0.6.4 and maintained by Factory. Not a drop-in replacement: the
binary is `vfs`, the crates are `vfs-*`, and the non-Rust SDKs are gone. Three
campaigns account for the distance.

**Performance** ([#2](https://github.com/Factory-AI/vfs/pull/2)) — closing the
gap against native git on the canonical codex workload. Kernel entry/attr
TTLs, ENOSYS-FLUSH and ENOSYS-OPEN protocol levers, a FUSE-over-io_uring
transport, a cross-inode write batcher, bulk+streamed `clone`. Per-phase
wall-clock vs native git, median-of-5:

| phase | before | after |
|---|---|---|
| `status` | ~1.9x | **0.60–0.93x** |
| `diff` | ~80ms | **18ms (0.05x)** |
| `checkout` | — | **0.42x** |
| `fsck` | — | **0.83x** |
| `read_search` | ~4.7x | **1.37–1.41x** |
| `clone` | 9.6x | **2.22x** |

Every remaining miss carries a named, measured floor rather than a shrug —
the warm read path bottoms out on kernel close-time `STATX_BLOCKS`
invalidation, for which a kernel patch is written and VM-validated.

**Restructure** ([#3](https://github.com/Factory-AI/vfs/pull/3)) — a
9,338-line god-file and two vendored crate forks leaking as public API became
five crates in a clean DAG. 96 ad-hoc knobs became typed declarations with a
generated ledger; one `Semantics` layer now serves both adapters, making
FUSE/NFS drift structurally impossible; the CI gate stopped masking its own
failures. Plus the correctness work that surfaced: FUSE teardown deadlocks on
*both* transports, an NFS `FILE_SYNC`-without-durability lie, overlay
base-rename data loss, and default-deny read scoping in the macOS Seatbelt
profile. Sealed by 176 behavioral contract assertions, pjdfstest 311/311, and
all seven benchmark phases held within a 5% band.

**Session handoff** ([#4](https://github.com/Factory-AI/vfs/pull/4)–[#9](https://github.com/Factory-AI/vfs/pull/9))
— the pipeline this README opens with: `seed` → `pack` → `adopt`, resumable
sessions with a recovery ladder, `vfs status` preflight, the reserved
exit-status contract, and the versioned content-addressed wire contract.
Schema v0.6 lands here too, so provenance rides inside the artifact.

Upstream's Go, Python, and TypeScript SDKs, the `examples/` tree, the
experimental ptrace sandbox, the Windows target, and the 17-feature `abi-7-*`
FUSE matrix were deleted rather than carried. See the
[CHANGELOG](CHANGELOG.md).

Vfs is the session substrate behind Factory Droid's portable sessions and live
session handoff, which is what drives the contracts above.

## Repository layout

One Cargo workspace, five crates:

| Crate | Role |
|---|---|
| `crates/vfs-core` | The engine: storage, overlay/copy-on-write, schema authority, typed config, telemetry, semantics (access/durability/handles). The only crate meant for external consumption. |
| `crates/vfs-fuse` | Sealed Linux FUSE mount surface (transport + adapter). |
| `crates/vfs-nfs` | Sealed NFSv3 serve surface (transport + adapter). |
| `crates/vfs-mount` | One mount lifecycle: `mount_fs`, `MountHandle`, supervision, daemonize. |
| `crates/vfs-cli` | The `vfs` binary: thin CLI edge over the crates above. |

**Linux is first-tier** (FUSE and NFS backends, `vfs run` sandbox, full
validation gate). **macOS is second-tier**: NFS mount plus a sandboxed `vfs
run` (Seatbelt with default-deny read scoping), validated by a manual release
gate on real hardware — see [docs/TESTING.md](docs/TESTING.md). No other
platforms are supported.

## Getting started

```bash
cargo +nightly build --release --workspace --bins
install -m 0755 target/release/vfs ~/.local/bin/
```

Initialize a filesystem and inspect it without ever mounting:

```console
$ vfs init my-agent
Created agent filesystem: .vfs/my-agent.db
Agent ID: my-agent

$ vfs fs my-agent ls
f hello.txt

$ vfs fs my-agent cat hello.txt
hello from agent
```

Read the agent's tool-call timeline:

```console
$ vfs timeline my-agent
ID   TOOL                 STATUS       DURATION STARTED
4    execute_code         pending            -- 2024-01-05 09:44:20
3    api_call             error           300ms 2024-01-05 09:44:15
2    read_file            success          50ms 2024-01-05 09:44:10
1    web_search           success        1200ms 2024-01-05 09:43:45
```

Mount it as a real filesystem (FUSE on Linux, NFS on macOS):

```console
$ vfs mount my-agent ./mnt
$ echo "hello" > ./mnt/hello.txt
```

Or sandbox a program over your current directory — copy-on-write, host
untouched:

```console
$ vfs run --session my-session -- bash
# ... every write lands in the delta database ...
$ exit

$ vfs ps
$ vfs diff my-session
```

## The rest of the CLI

* `vfs exec` — one-shot command over a temporary mount, unmounted after.
* `vfs clone` — bulk-ingest a git repository straight into the database.
* `vfs sync` — push/pull to a remote Turso (libSQL) server.
* `vfs serve nfs` / `vfs serve mcp` — export over NFS, or expose filesystem
  and KV tools to agents over MCP.
* `--key` / `--cipher` — local at-rest encryption.
* `vfs backup`, `integrity`, `migrate`, `materialize`, `prune` — portable
  backups, corruption checks, schema migration, partial-origin
  materialization, mount and artifact-store cleanup.

The **[User Manual](docs/MANUAL.md)** documents every command; its reference
is generated from the CLI's own argument definitions, so it cannot drift from
`vfs --help`.

## Using Vfs as a library

`vfs-core` exposes the same engine the CLI uses: filesystem, key-value store,
and tool-call audit trail over one database. See `cargo doc -p vfs-core`.

## How it works

At the core is the [agent filesystem](docs/SPEC.md), a SQLite storage system
built on [Turso](https://github.com/tursodatabase/turso). The schema separates
namespace (dentries) from data (inodes + chunked/inline content), which is
what buys hard links, POSIX metadata, sparse files, and SQL-queryable history.
Schema v0.6 adds `fs_session_metadata` — the reason pack generation and seed
provenance travel *inside* the transferred file instead of in sidecars that
never leave the sender.

On Linux the FUSE backend dispatches through a bounded worker pool with a
read/write lane split, kernel-cache acceleration (entry/attr TTLs, writeback
cache, readdirplus), zero-message opens, and an optional FUSE-over-io_uring
transport. Every one of those is an acceleration structure reconstructible
from the database: the two safety properties — one portable database holds all
virtual filesystem state, and sandboxed writes never reach the host — hold
regardless of cache configuration. Tunables are declared in the generated
[docs/KNOBS.md](docs/KNOBS.md) ledger.

## FAQ

### How is Vfs different from _X_?

[Bubblewrap](https://github.com/containers/bubblewrap) gives you filesystem isolation via namespaces and overlays; a `bwrap` call mounting `/` read-only with `--tmp-overlay` on the working directory gets you close. The difference is persistence and queryability: in Vfs the upper filesystem is one SQLite file you can query, snapshot, and move to another machine.

[Docker Sandbox](https://www.docker.com/blog/docker-sandboxes-a-new-approach-for-coding-agent-safety/) is complementary, not competing. Vfs answers "what happened and what's the state?"; Docker Sandboxes answer "how do I run this safely?" Run the agent in a Docker Sandbox and use Vfs inside it for state and audit.

[Git worktrees](https://git-scm.com/docs/git-worktree) give agents independent copies of the source — but nothing stops an agent from writing outside its worktree, into another agent's worktree or system files. That isolation is conventional. Vfs enforces copy-on-write isolation below git, so it also covers untracked files.

### Why the filesystem layer instead of containers or VMs?

Structured SQLite tables mean you can *query* the filesystem, which is what makes agent behavior auditable and debuggable. SQLite's write-ahead log gives snapshotting and time-travel forking. And the engine runs where mounting a block device is impossible. It composes with containers and VMs rather than replacing them: Vfs is reachable over NFS or virtio-fuse.

### Why pin a transferred session to a git commit?

Because the artifact is a delta layered over a base checkout that stays on
disk — that's what keeps a handoff proportional to the work done. A delta only
reconstructs the sender's view if the receiver's base is byte-identical, so
adopt verifies the checkout against the pin recorded inside the artifact and
refuses otherwise. Adopting onto the wrong base would produce a session that
looks fine and is quietly wrong.

## Learn more

- **[User Manual](docs/MANUAL.md)** — complete CLI reference (generation-checked against the binary)
- **[Agent Filesystem Specification](docs/SPEC.md)** — SQLite schema and runtime invariants
- **[Runtime Knobs](docs/KNOBS.md)** — generated ledger of every tunable
- **[Testing](docs/TESTING.md)** — validation gates, benchmark policy, manual macOS release gate
- **[AGENTS.md](AGENTS.md)** — working contract for changing this repo
- **[CHANGELOG](CHANGELOG.md)** — fork-era summary
- **[Turso](https://github.com/tursodatabase/turso)** — the in-process SQL database Vfs builds on

## License

MIT
