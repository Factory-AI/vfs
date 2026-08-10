# Changelog

## [Unreleased]

### Changed

- The bundled turso storage engine advances from 0.5.3 to 0.7.2 (`turso`,
  `turso_sdk_kit`, `turso_core`), picking up two releases of upstream
  corruption fixes (WAL frame-cache slot reuse, page-spill frame reuse,
  nonblocking `read_page` race, WAL-header init ordering). The vendored
  read-only open patch is re-applied onto 0.7.2; upstream's Rust SDK still
  has no read-only open mode, and immutable parent artifacts must never be
  opened writable. Default features are trimmed to `mimalloc` (the 0.5-era
  allocator, unchanged); the new `fts`/tantivy default stays out of the
  build. No experimental engine feature (MVCC, multiprocess WAL) is enabled:
  session liveness stays derived from Vfs's own advisory locks.
- `turso_core` joins `turso` and `turso_sdk_kit` as a vendored tree, carrying
  one patch: `Statement` drop no longer invalidates the connection's
  prepared-statement cache. Upstream 0.6+ bumps the prepare-context
  generation on every statement drop once the TEMP database exists — and
  `BEGIN IMMEDIATE` initializes TEMP on every writer connection — so each
  of a clone workload's ~89,000 statements re-prepared from scratch, the
  bulk of a 40%+ git-clone CPU regression against 0.5.3. With the patch the
  workload re-prepares zero statements and the clone runs ~15-20% less CPU
  than the 0.5.3 baseline (interleaved A/B, CPU time, verified clones).
  Details in `third_party/turso_core/PATCHES.md`.
- The vendored SDK caches one-shot `execute`/`query` statements per
  connection (bounded at 512 entries; PRAGMAs exempt), removing 0.7's
  higher fresh-prepare cost from hot write and journal statements.
- `PRAGMA temp_store = MEMORY` is gone from connection setup. Any
  `temp_store` assignment initializes the TEMP database eagerly, Vfs's
  statements never materialize temp B-trees, and the engine since 0.6 wraps
  spill files in self-cleaning temp handles.

### Removed

- The private `TMPDIR` sort-spill override, its `SIGKILL` reaper, and the
  child-environment restoration in `run`/`exec`/`init -c` spawn paths.
  turso 0.6.0 fixed the `tursodb-ephemeral-*` unlink leak the machinery
  contained; children now simply inherit the ambient `TMPDIR`.

### Changed (internal)

- Journal-head queries are spelled `SELECT MAX(seq)` again: turso 0.6.0's
  min/max optimization plans the bare aggregate as a reverse seek. The
  empty-journal NULL is handled in Rust because wrapping the aggregate
  (`COALESCE`) defeats the optimization.

## [1.1.0] - 2026-08-10 - Replayable history, session branching, and the remote tier

This release builds one time-travel path from content-addressed storage through
replayable filesystem history: `vfs branch` can fork the current state or a
retained historical boundary, `vfs history` exposes those boundaries, and
offline `vfs revert` publishes a checked reconstruction back over a session.
The same content-addressed store now extends off the machine: `vfs checkpoint`
publishes sessions to S3-compatible object storage as chunk objects plus a
hollowed metadata artifact, replacing the turso page-level sync family as the
one replication mechanism, and `vfs adopt --remote` installs directly from
that tier as a lazy session whose reads fault chunks by digest until
`vfs materialize --in-place` cuts the dependency.
The database and artifact schema advances through v0.7 content addressing to
v0.8 replay history. `artifactVersion` changes from `0.7` to `0.8`; `adopt`
migrates supported older artifacts forward automatically, while existing
pack/adopt manifest fields and reserved exit statuses remain unchanged.

### Added

- Schema v0.7 replaces per-file chunk blobs with a content-addressed
  `fs_chunk` store keyed by raw 32-byte BLAKE3 digests. `fs_data` maps
  `(ino, chunk_index)` to a digest, identical chunks deduplicate, and exact
  live-mapping refcounts support safe reclamation.
- The v0.7 journal records logical mutations in commit groups and pins chunk
  digests without duplicating bytes. Pack bounds the retained journal on its
  staging copy. On the codex workload (median-of-5), the CAS reshape holds the
  git-clone phase at the pre-v0.7 time and journaling adds about 26µs per
  logical operation (+395ms over ~19,600 operations);
  `VFS_JOURNAL=0` removes that cost.
- `vfs branch <SESSION_ID> [--session <ID>]` forks a session. A live parent
  is snapshotted through its mount's control socket without stopping it (the
  snapshot is a drained `VACUUM INTO` copy, so every write acknowledged
  before the call is included); an inactive parent is snapshotted under the
  exclusive session lock. The command emits a one-line JSON manifest
  (`sessionId`, `parentSessionId`, `parentArtifactSha256`, `artifactPath`,
  `basePath`, `seedPin`, `parentLive`, `vfsVersion`).
- Frozen parent artifacts live in a content-addressed store at
  `~/.vfs/artifacts/<sha256>.db`, published write-protected (0444) by
  same-filesystem rename. Branches taken at the same parent state share one
  artifact.
- Branch sessions mount as a stacked overlay — branch delta over the
  read-only parent chain over the host base — on every surface (`vfs run`,
  `vfs mount`, `vfs exec`, FUSE and NFS). Parent artifacts are re-hashed at
  mount time; a missing or drifted artifact refuses the mount, and `vfs run`
  reports that refusal with the invalid-session exit status `5`.
- Branch-of-branch chains are supported to a depth of 8.
- `vfs pack` of a branch session materializes the parent chain into the
  staged database, so the published artifact is self-contained and `vfs
  adopt` on a receiver without the artifact store reconstructs the branched
  view exactly. Pack refuses to publish if a parent artifact is missing or
  drifted.
- `vfs prune artifacts [--dry-run]` collects artifacts no session chain
  references and emits a one-line JSON report (`removed`, `kept`,
  `reclaimedBytes`). Classification is conservative: inactive sessions are
  read under the exclusive lock, live sessions are asked over their control
  socket, and any session that cannot be classified aborts the prune. A
  store-level advisory lock makes prune and a concurrent `vfs branch`
  publication mutually exclusive.
- Each `vfs run` session now exposes a control socket
  (`~/.vfs/run/<id>/ctl.sock`) accepting snapshot and parent-digest requests
  from same-machine tooling while the session is live.
- Schema v0.8 replaces the semantic journal with complete table-row
  post-images grouped by SQLite transaction. Immutable roots cover inode,
  namespace, content, overlay, and provenance state; inline bytes remain
  content-addressed rather than embedded in history JSON. The journal
  carries no pin table and no secondary index — chunk retention is derived
  from the digests retained rows name, and `txn_id` lookups are `seq` range
  scans on offline paths — so a mutating commit pays only its own journal
  row inserts.
- In-place and copy migration from v0.7 discard the old semantic journal,
  initialize durable history markers, and establish one migration root at
  epoch 1 through sequence 0.
- `vfs-core` exposes root capture, history status and target validation, and
  exact reconstruction of a private staged database to any retained complete
  transaction boundary. Reconstruction verifies digests, recomputes chunk
  refcounts, preserves inode allocator high-water marks, trims the future, and
  runs relational plus visible-tree integrity checks.
- Journal collection is snapshot-covered: GC rolls the root forward to a
  complete boundary before removing older groups. Pack establishes a fresh
  `pack` root after parent-chain materialization, and revert establishes a
  fresh `revert` root, so each published generation has an explicit floor.
- The journaling kill switch has a durable epoch contract. An unjournaled
  mutation marks history invalid; re-enabling journaling bumps the epoch,
  removes stale history, and captures the current state as a fresh root.
  Read-only and no-op maintenance opens never mutate these markers.
- `vfs history <SESSION_ID> [--limit N | --all] [--json]` lists retained
  complete-transaction targets newest-first with the epoch, validity marker,
  floor/head range, diagnostic label, wall clock, touched tables, and row
  count.
- `vfs branch <SESSION_ID> --to <SEQ>` reconstructs the private parent
  snapshot before immutable artifact publication. Historical manifests add
  `targetSeq`, `sourceHeadSeq`, and `rootSnapshotSeq`; plain branch manifests
  remain unchanged.
- `vfs revert <SESSION_ID> --to <SEQ>` performs an offline staged rewind with
  exit-status-3 live-session refusal, generation increment, backup-rename
  publication, rollback, integrity verification, and run/resume crash
  recovery. KV and tool-call rows remain outside the rewind.
- `vfs version --json` advertises `features.branch` and `features.history`.
- `vfs checkpoint <SESSION_ID> [--json]` publishes a consistent session point
  to S3-compatible object storage or a `file://` root (`VFS_REMOTE_URL`):
  content-addressed `chunks/<blake3>` objects, an immutable hollowed metadata
  artifact carrying the whole database authority, and a per-session
  `manifest.json` whose PUT is the single commit point, read back before the
  command reports success. Live sessions snapshot through the control socket
  after a drain, so the printed seq token names exactly the state the remote
  holds; branch sessions fold their parent chain first, keeping the wire
  branch-agnostic. Encrypted sessions refuse (plaintext chunk upload would
  downgrade at-rest encryption), and journal-off sessions publish
  `historyValid:false` honestly. `vfs version --json` advertises
  `features.checkpoint`.
- A mount-owned background streamer uploads chunk objects ahead of explicit
  checkpoints when the remote is configured
  (`VFS_REMOTE_STREAM_INTERVAL_MS`, 0 disables). It never writes the
  manifest, keeps no persistent state, and dies with the mount.
- Hollow metadata artifacts are contained like partial-origin state: a
  writable open refuses them unless a chunk source backs it, `integrity`
  reports `storage.chunks_hollow`, `--require-portable` fails them, and
  plain `backup` rejects them. Hydration refills every chunk with
  per-digest BLAKE3 verification in one transaction. The chunk
  byte-vs-digest integrity check verifies every non-empty row, hollow or
  not.
- `vfs adopt <SESSION_ID> --remote --base <PATH>` installs a session from
  the checkpoint tier: manifest GET, session and supported-version checks,
  metadata GET with exact length and SHA-256 verification, forward
  migration, manifest-vs-database cross-checks, then the unchanged adopt
  rename commit. The remote locator is durably recorded in the session
  store between the base path and the commit, and the adopt JSON gains an
  additive `"remote":true`.
- Adopted-remote sessions are lazy: the storage engine resolves every chunk
  consumer (reads, partial-write read-modify-write, truncate boundaries,
  chunked-to-inline conversion) through one resolver that fetches missing
  bytes by digest from the recorded remote, BLAKE3-verifies before use, and
  backfills as a journal-invisible cache fill. An unreachable remote is an
  explicit I/O error, never silent zeros. Resumed runs fault from the
  recorded locator alone; `VFS_REMOTE_URL` stays a checkpoint-side knob.
- `vfs materialize` now requires exactly one destination: the existing
  `--output`, or the new `--in-place`, which hydrates an installed session
  offline under the exclusive lock (or a raw path via `VFS_REMOTE_URL`) in
  one all-or-nothing transaction, removes the recorded remote locator, and
  is idempotent. `--output` and `backup --materialize` hydrate hollow
  inputs before the partial-origin conversion and keep the portable gate;
  lazy state cannot leave the machine otherwise — `pack`, `branch`,
  `revert`, `checkpoint`, and plain `backup` refuse while hollow, and the
  streamer never publishes an empty chunk body. `vfs version --json`
  advertises `features.adoptRemote`.
- CI now runs the macOS runtime for real: a `macos-runtime` job on
  macos-latest executes the NFS + Seatbelt validation script (previously a
  manual hardware gate) and both remote-tier suites over the unprivileged
  `mount_nfs` path, failing on any suite SKIP. The suites gate per platform;
  only the live-checkpoint and streamer legs stay Linux-only, because the
  control socket and streamer exist only in the Linux mount owner.

### Fixed

- macOS `vfs run` now honors the reserved startup exit statuses for a bad
  command: `127` when it is missing and `126` when it is present but not
  executable. The sandbox wrapper always spawns (`/usr/bin/sandbox-exec` is
  a pinned system binary), so a missing *target* previously surfaced as
  sandbox-exec's own `EX_OSERR` exit 71. The run path now resolves the
  target execvp-style before spawning and exits with the reserved status,
  matching `vfs exec` and the Linux run path. Caught by the new CI
  exit-parity leg in `macos-nfs-git-validation.sh`, which also gained a
  quoted-profile-path leg; both were previously manual release spot-checks.
- Fresh databases now create inode 1 before capturing the sequence-0 `init`
  root, so the advertised history floor reconstructs a valid visible tree.
- `rmdir` now removes an empty directory inode with its dentry instead of
  leaving an unreachable inode at `nlink=1`.

### Changed

- Add `blake3` as a first-party dependency for content-addressed chunk
  identity and schema migration.
- The bundled turso SDK crates (`turso`, `turso_sdk_kit` 0.5.3) are vendored
  under `third_party/` with a patch adding a read-only open mode, used to
  guarantee parent artifacts are never opened writable. `turso_core` is
  unchanged.

### Removed

- Remove the turso page-level `vfs sync` family (`pull`, `push`, `checkpoint`,
  and `stats`), the `vfs init` sync flags, and `TURSO_DB_AUTH_TOKEN`. The CAS
  remote tier is this release's only replication mechanism, and the top-level
  `checkpoint` verb is available for `vfs checkpoint`.

## [1.0.2] - 2026-08-08 - Lifecycle benchmark hardening

This release makes the chaos workload the unambiguous primary performance
record and adds focused instruments for the lifecycle costs outside its
workload timer. The handoff wire contract and database schema do not change:
`artifactVersion` remains `0.6`.

### Added

- `scripts/validation/mount-startup-benchmark.py`, measuring parent process
  spawn through completion of a minimal child process's first successful
  filesystem probe. It reports native process overhead beside legacy FUSE and
  FUSE-over-io_uring distributions, alternates transport order, isolates every
  sample, and requires clean teardown.

### Changed

- Upgrade the chaos benchmark report to schema version 2. The primary
  `absolute_wall_seconds` workload distribution retains its post-preparation,
  post-warmup boundary; `absolute_startup_seconds` now records process spawn
  through the first successful child request so mount cost remains visible
  without contaminating the workload timer. Measured pairs alternate which
  engine runs first, balancing residual first-leg cache effects.
- Rebuild `vfs-clone-benchmark.py` around the same fixture and isolation
  contract as the chaos benchmark. It now refuses implicit synthetic
  substitution, discards leading warmups, interleaves native and Vfs legs,
  reports absolute distributions before derived ratios, and verifies clean
  Git status, strict fsck, identical tracked content, and teardown outside the
  command timer.
- Consolidate benchmark HOME, XDG, TMPDIR, Git pinning, mount census, process
  census, and teardown recovery in the shared validation library.
- Document the chaos workload as the primary scoreboard and define exactly
  which lifecycle phases each benchmark includes.

## [1.0.1] - 2026-08-08 - Protocol, cache, and validation hardening

Patch release for two runtime bugs and the benchmark defects found while
validating them. The handoff wire contract and database schema do not change:
`artifactVersion` remains `0.6`.

### Fixed

- Restore normal kernel metadata TTL grants for base-origin files while
  retaining fail-closed external-drift detection. The drift guard previously
  returned zero entry and attribute TTLs for every base inode, turning the
  reader workload from roughly 47,000 FUSE callbacks into 578,000 and making
  legacy FUSE 3.7x slower in an exact parent/commit comparison. A recursive
  host watcher now invalidates tracked positive entries, negative entries, and
  inode metadata after external base mutations; it excludes the delta database
  and its SQLite sidecars. Base reads retain their data-drift guard,
  adapter-local caches remain conservative, and the coherence gate now pins
  immediate metadata and namespace freshness alongside callback volume and
  stale-byte rejection.
- Send tracing to stderr. The formatter previously wrote diagnostics to
  stdout, where `vfs run` and `vfs exec` expose the wrapped command's output
  and `vfs mcp-server` speaks JSON-RPC. An asynchronous mount log could
  corrupt either data stream.
- Refuse to substitute the generated 96x1KB repository when the gitignored
  canonical benchmark fixture is missing. Callers must provide a source,
  materialize the canonical fixture, or request `--synthetic`; reports state
  whether their workload is comparable to the scoreboard.
- Make the Phase 7 and Phase 8 validation callers name their workload and
  apply scoreboard thresholds only to the canonical workload.
- Isolate every chaos-benchmark leg with its own checkout, HOME, TMPDIR, XDG
  directories, Git configuration, and Vfs session store. Discard leading
  warmups from measurements, and reject legs that leave a mount or
  session-bound process behind.

### Added

- `scripts/validation/chaos-workload-benchmark.py`, a seeded concurrent local
  benchmark covering Git churn, scattered edits, tree scans, build-artifact
  churn, unlink-while-open, and hermetic loopback fetches. It reports absolute
  distributions before derived ratios and is not part of CI.

## [1.0.0] - 2026-08-07 - Fork era: restructure, rename, and session handoff

First release under Factory's own identity. The fork diverged from agentfs
after 0.6.4; this is the user-visible summary of three campaigns: the
Right-Thing Restructure onto a five-crate Rust workspace, the rename of the
product surface to `vfs`, and the session-handoff pipeline
(`seed` → `pack` → `adopt`). Behavior-preserving moves are not listed
individually.

Upgrading from agentfs 0.6.x is not a drop-in swap: the binary, the env vars
(`VFS_*`), the session store (`~/.vfs/run/`) and the default database
directory (`.vfs/`) all changed name, and the NFS write-handle magic changed,
so write handles minted by a pre-rename server are invalid after upgrade.
The handoff artifact contract is unaffected — `artifactVersion` stays `0.6`
and `minSupportedArtifactVersion` stays `0.0`, so a 1.0.0 receiver still
adopts artifacts produced by the 0.6.x fork builds.

### Added

- `vfs pack <session-id>`: prepares an inactive run session's `delta.db` as a
  single-file transfer artifact. Pack takes the exclusive `.session.lock`,
  rejects live mounts and owner/joiner processes with exit code `3`, and
  performs pruning, schema migration, generation bump, checkpoint, and
  compaction on a private staging copy only. Publication renames the old
  database family to a deterministic backup, renames the completed staging
  database into place, verifies its metadata, and rolls back on failure; a
  later pack recovers the backup if the process died between renames.
  `--output` artifacts publish via no-replace hard link. Pruning removes
  matching delta paths through the core filesystem API rather than by
  whiteout, so a pruned base-shadowing path falls back to the base version
  (`--prune <GLOB>`, `--no-default-prunes`).
- `vfs seed <session-id> --pin <commit>`: captures a run session's live git
  state — dirty and untracked files, deletions since the pin as whiteouts,
  local-only commits as a compact git pack plus `HEAD` and the branch ref,
  and the sender's raw index bytes to preserve staged-vs-unstaged — into its
  portable delta, without mounting. Git-ignored files are excluded as the
  portable-state boundary. Seed is birth-time only and writes a private
  staging database published only after import, whiteouts, metadata, and
  finalization all succeed. `vfs run --session <id> --seed-pin <commit>` is
  the atomic startup form, creating and seeding the delta under the exclusive
  lock before downgrading to the lifetime shared run lock.
- Schema v0.6: `fs_session_metadata(key, value)` carries `generation`,
  `seeded_paths`, and `seed_pin` inside the transferable database, so handoff
  provenance travels with the artifact instead of in session-directory
  sidecars. The v0.5 → v0.6 migration is additive and runs in the same schema
  transaction.
- `vfs status <session-id> --json`: run-session state for daemon preflight,
  reporting `stopped | busy | live | stale-recovered`, pid, pack generation,
  and the seeded flag. Preflight performs the same recovery as resume, so the
  reported state is truthful rather than cached.
- Resumable run sessions: every start runs a recovery ladder under the session
  lock — roll forward an interrupted pack publication, detach a stale mount,
  reap dead owner/joiner proc records. Liveness is derived from the advisory
  lock plus proc records, both released by the kernel on process death, which
  makes the classification crash-consistent. A `vfs run --session <id>`
  against a genuinely live session now joins it (shared lock, validated mount
  and runtime status) instead of being rejected.
- Reserved startup exit statuses, now API for daemon callers: `2` usage, `3`
  session genuinely live, `4` mount or sandbox install failed, `5` session
  missing or malformed, `126`/`127` exec conventions. The wrapped command's
  own status passes through unchanged, and signals report as `128 + n`.
- `vfs version` / `vfs version --json`: version, commit, and a `features`
  capability map for callers that must detect a verb before invoking it.
- `vfs adopt <session-id> --db <path> --base <path> [--pin <commit>]`:
  first-class installation of a transferred pack artifact as a local run
  session, replacing the hand-written "externally materialized run
  sessions" receiver contract. Adopt integrity-checks the artifact,
  migrates supported older artifact schemas to the current version,
  verifies the receiving base checkout's `HEAD` against the artifact's
  recorded seed pin (or a required `--pin` for artifacts without recorded
  provenance), and publishes the session atomically so a partial or
  corrupt session is never observable. `vfs version --json` reports the
  capability as `features.adopt`.
- `vfs seed` records its resolved pin in `fs_session_metadata` as
  `seed_pin`, giving packed artifacts the base provenance `vfs adopt`
  verifies on the receiving machine.
- Artifact wire contract for daemon-to-daemon handoff: `vfs version --json`
  reports `artifactVersion` and `minSupportedArtifactVersion` for
  version-floor negotiation, and the `vfs pack` manifest adds
  `artifactVersion`, `chunkSizeBytes`, and a `chunks` list of
  `{index, sizeBytes, sha256}` digests over consecutive `--chunk-size`-byte
  ranges (default 4194304) of the published artifact, alongside the existing
  whole-file `dbSha256`.

### Removed

- Deleted SDKs: the Go, Python, and TypeScript SDKs and their CI workflows;
  the Rust library survives as the `vfs-core` crate.
- The standalone example projects (built against the deleted SDKs).
- The experimental ptrace sandbox and the `--experimental-sandbox` flag;
  `vfs run` is FUSE+overlay in Linux user/mount namespaces (NFS +
  Sandbox on macOS).
- Windows stubs and the Windows dist target; supported platforms are Linux
  (first-tier) and macOS (second-tier: NFS mount plus a sandboxed
  `vfs run`).
- The `abi-7-*` FUSE feature matrix (17 features collapsed into the one
  compiled ABI level) and the dead vendored fuser/nfsserve surface.
- The legacy path-based SDK API and the `AGENTFS_OVERLAY_PARTIAL_ORIGIN`
  env opt-in (superseded by the first-class `--partial-origin` policy).
- `migrate-v0-5`: one `vfs migrate` now lands any supported old schema
  (v0.0, v0.2, v0.4) at the current version, in place by default or with
  `--copy`-based re-chunking.
- The hand-written "externally materialized run session" receiver contract,
  in which a receiver assembled `~/.vfs/run/<id>/` by hand. The store layout
  is private to `vfs` and reachable only through `vfs adopt`.

### Changed

- The run-store layout (`~/.vfs/run/<id>/`) has a single owner. Six command
  modules had each rebuilt the path set by hand, and `run/darwin.rs` carried a
  wholesale duplicate of `SessionPaths`; a layout change had to be made in
  seven places to be correct. `SessionPaths` is now the sole authority, with
  `SessionLock` owning the `.session.lock` name. Remaining literals are
  confined to test fixtures, where asserting the layout independently is the
  point.
- The Python validation harnesses no longer carry private copies of the
  shared helpers in `scripts/validation/lib/common.py`. Sixteen files
  duplicated `run_subprocess`, `resolve_vfs_bin`, `tail_text`,
  `parse_json_stdout`, `git_commit`, and friends, and the copies had drifted:
  eleven resolved the binary through the pre-restructure `cli/` path, several
  called `git` directly instead of the `GIT`-pinned distro binary that
  `pin_distro_git()` exists to enforce, one dropped the `TimeoutExpired`
  guard that keeps a stuck process tree from crashing the harness, and one
  scanned only the tail of stderr for profile summaries. All now import the
  canonical implementations.
- Toolchain moved to `nightly-2026-08-07` with dependencies refreshed;
  `async-trait` 0.1.91 clears the `double_must_use` errors the newer clippy
  reports against `#[async_trait]` expansions.
- Renamed the product surface from `agentfs` to `vfs`: workspace crates, the
  CLI binary and its output, validation tooling, benchmarks, and docs. Env
  vars follow (`VFS_*`), as does the session store (`~/.vfs/run/`) and the
  default database directory (`.vfs/`). One deliberate protocol break: the
  NFS write-handle magic changes `AFSWRIT\0` → `VFSWRIT\0`, so write handles
  minted by a pre-rename server are invalid after upgrade (clients re-open;
  no data loss).
- One root workspace after the crate split, five crates in a clean DAG —
  `vfs-core` (storage engine, overlay, schema authority, typed config,
  telemetry, semantics), `vfs-fuse` and `vfs-nfs` (sealed transport
  + adapter crates), `vfs-mount` (one mount/supervision lifecycle), and
  `vfs-cli` (thin edge with a single error reporter).
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

- `vfs run`'s epilogue advertises `vfs diff <session-id>`, but `diff` only
  resolved the `.vfs/<id>.db` agent-database convention, so the command it
  printed always failed for run sessions. `diff` now falls back to the run
  store after the agent lookup misses, and the not-found error names both
  lookups and points at `vfs ps`.

- The validation harnesses launched workloads with `sys.executable`. Under
  `vfs run --no-default-allows` the sandbox hides `$HOME`, so on any machine
  whose `python3` lives under `~/.local` (pyenv, uv, a user-installed
  CPython) the interpreter did not exist inside the sandbox and the workload
  died with exit `127` before it could test anything. Nine harnesses were
  affected, including the `noopen-coherence` and `partial-origin-no-real-write`
  gates. They now resolve a system interpreter via `sandbox_python()`, the
  counterpart to the existing `pin_distro_git()`. Machines whose `python3` is
  the distro one were never affected, which is why CI never saw it.

- `scripts/gate.sh` defaulted to a bare `+nightly`, which overrides the
  `rust-toolchain.toml` pin: the gate could lint and test against a different
  compiler than CI and than every plain `cargo` invocation in the tree. It now
  derives the channel from the pin.

- The FUSE mount surface accepted `--uid`/`--gid` ("User ID to report for
  all files") but never consumed them: attributes always reported the
  stored inode ownership. A delta created by one user and resumed by
  another (e.g. a session database moved to a different machine) surfaced
  every file as an unmappable foreign uid — `nobody` inside the `vfs
  run` user namespace — making owner-only (0600) files unreadable. The
  adapter now squashes reported ownership to the configured uid/gid; since
  `vfs run` already mounts with the current user, resumed foreign
  deltas are fully readable. Modes are untouched, and stored ownership in
  the database is preserved.

- A directory passed to `vfs run --allow` was self-bound with a plain
  `MS_BIND`, which shadowed any mount nested beneath it. An externally
  materialized checkout under an allowed data directory therefore kept its
  overlay for the inherited-cwd view but served the raw base tree to any
  subprocess spawned with an absolute cwd, silently losing the delta.
  Directory allows now bind with `MS_BIND | MS_REC`. Found by the live
  cross-machine handoff demo and pinned by
  `crates/vfs-cli/tests/test-run-resume-hardening.sh`.

- macOS `vfs run` left reads unscoped (a blanket `(allow file-read*)`
  in the generated Seatbelt profile) while Linux hid home and temp dirs
  behind namespaces. The profile is now default-deny for reads: only the
  session paths, the allowed directories (defaults plus `--allow`), and a
  curated set of platform read roots stay readable; write scoping is
  unchanged. Pinned by macOS-gated unit tests; runtime behavior is covered
  by a new read-scoping leg in the manual macOS release gate
  (`scripts/validation/macos-nfs-git-validation.sh`).
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
- Structural canon census (`scripts/validation/consistency-canon.sh`), wired
  into the gate: crate DAG, sealed transport surfaces, production file-size
  cap, tracing-only logging, env reads confined to config modules,
  `await_holding_lock`, lock-order headers, and docs layout.
- `crates/vfs-cli/tests/test-run-resume-hardening.sh` pins the receiver
  contract, the crash-resume matrix, and the nested-checkout-under-allowed-
  ancestor case.
- Local-only perf gate: serialized median-of-5 codex workload benchmark
  against a pinned baseline; any per-phase median regression >5% is red.
- The macOS NFS git validation script is documented as a manual release
  gate to run on real hardware.

### Known gaps

- Cross-platform handoff (a macOS sender to a Linux receiver) is exercised
  manually only; there is no macOS runner in the automated gate.
- FUSE-over-io_uring legs are honest only on a local kernel with
  `fuse.enable_uring=1`; CI kernels ship it disabled and fall back to the
  legacy channel.

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
