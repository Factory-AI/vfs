# AGENTS.md

Working contract for anyone — human or agent — changing this repository.
[README.md](README.md) explains what Vfs *is*; this file explains how to
change it without breaking it.

Read this before your first edit. The rules below are not style preferences:
most of them are mechanically enforced by `scripts/gate.sh`, and the ones that
aren't protect invariants that a plausible-looking patch can silently violate.

## What this repo is

A hard fork of [tursodatabase/agentfs](https://github.com/tursodatabase/agentfs)
at 0.6.4, maintained by Factory as the session substrate behind Droid portable
sessions and live session handoff. One Cargo workspace, five crates, one
shipped binary (`vfs`). Linux is first-tier; macOS is second-tier; nothing else
is supported.

**Do not port upstream code back in without reading it.** Upstream still has
the SDKs, the ptrace sandbox, the Windows stubs, and the god-file this fork
deleted on purpose. Divergence is intentional.

## The four invariants

Everything else is negotiable. These are not.

1. **One database is the whole authority.** All writable virtual filesystem
   state lives in the Vfs SQLite database. Mounts, caches, handle tables, and
   overlay inode maps are acceleration structures that MUST be reconstructible
   from the database plus the configured base path.
2. **Sandboxed writes never reach the host filesystem.** The only real-FS
   writes are the database and its transient sidecars. Guarded in the standing
   gate by `metadata-mutation-no-real-write.py` and
   `partial-origin-no-real-write.py`.
3. **A durability claim must be true.** Any operation that promises durability
   (`fsync`, an NFSv3 `WRITE` acked `FILE_SYNC`, unmount finalization) must not
   return before the bytes are committed. `AckDurability` exists so the reply's
   claim is derived from what the core actually did — the pre-fork
   "`FILE_SYNC` lie" is structurally unreachable, keep it that way.
4. **A transferred session reconstructs the sender's view exactly, or is
   refused.** No partial adopts, no adopting onto an unverified base, no
   observable half-published session.

If a change makes one of these harder to hold, that is the change to
reconsider — not the invariant.

## Architecture

Five crates in a strict DAG (`consistency-canon.sh` fails if an edge drifts):

```
vfs-cli ──▶ vfs-mount ──▶ vfs-fuse ──▶ vfs-core
   │            └───────▶ vfs-nfs  ──▶ vfs-core
   └──────────────────────────────────▶ vfs-core
```

| Crate | Owns |
|---|---|
| `vfs-core` | Storage engine, overlay/CoW, schema authority + migrations, typed config, telemetry, `Semantics` (access, durability, handles), session metadata. The only crate meant for external consumption. |
| `vfs-fuse` | Linux FUSE transport + adapter. Sealed: `pub use adapter::{mount, FuseMountOptions, SessionHandle}` and nothing else. |
| `vfs-nfs` | NFSv3 transport + adapter. Sealed: `serve`, `NfsServeOptions`, `ServerHandle`. |
| `vfs-mount` | One mount lifecycle: `mount_fs`, `MountHandle`, supervision, daemonize, stale-mount detection. |
| `vfs-cli` | The `vfs` binary. A thin edge: argument parsing, one error reporter, user-facing output. |

Two rules follow from the shape:

* **New behavior belongs in `vfs-core`, not in an adapter.** If FUSE and NFS
  would each need their own copy, it belongs in `Semantics`. Adapter drift is
  what the cross-surface parity conformance suite exists to prevent.
* **`vfs-cli` stays thin.** Command modules orchestrate; they do not implement
  filesystem semantics.

## Structural canon (mechanically enforced)

`scripts/validation/consistency-canon.sh` runs in the gate and fails the build
on any of these. Know them before you write, not after CI tells you:

| Rule | What it means |
|---|---|
| Crate DAG | First-party dependency edges must match the diagram above. |
| Sealed surfaces | `vfs-fuse` / `vfs-nfs` `lib.rs` export exactly the items listed above; no `pub mod`. |
| Line-count cap | No production `.rs` file over 2,500 non-test code lines. This fork exists partly because a 9,338-line file was allowed to grow. |
| Logging | `tracing` only. `println!`/`eprintln!` are user-facing CLI output, allowed only under `crates/vfs-cli/src/cmd/`, `main.rs`, and build scripts. |
| Env reads | `env::var`/`var_os` only inside a crate's config module (`vfs-core/src/config/`, `vfs-fuse/src/adapter/config.rs`, `vfs-cli/src/config.rs`). Parse at the edge into typed config; the core never reads the environment. |
| EnvFilter | `logging.rs` must name every first-party crate target. |
| Lock discipline | `clippy::await_holding_lock = "deny"` workspace-wide; every crate opts in. Multi-lock modules (batcher, adapter cache, overlay, handle table) must carry a documented lock-order header. |
| Docs layout | `MANUAL.md`, `TESTING.md`, `SPEC.md`, `KNOBS.md` live under `docs/`, never at the root. `CHANGELOG.md` is non-empty at the root. |

Other conventions the canon does not check but reviewers do:

* **Errors**: typed `thiserror` errors in `vfs-core`; `anyhow` at the edge
  crates. One error reporter in `main.rs` — do not add a second exit path.
* **Comments**: explain a constraint, an invariant, or a workaround. Do not
  narrate the code, and do not reference a task or PR; that context goes stale.
* **No backwards-compatibility scaffolding** unless someone explicitly asks
  for it. One canonical code path, no dual-shape adapters.

## Generated files — never hand-edit

Two docs are generated from code and pinned by unit tests. Editing them by
hand fails the gate.

```bash
# docs/MANUAL.md command reference (from the clap definitions)
VFS_UPDATE_MANUAL=1 cargo +nightly test -p vfs-cli --lib docs::tests::manual_help_parity -- --exact

# docs/KNOBS.md ledger (from the typed config declarations)
VFS_UPDATE_KNOBS=1 cargo +nightly test -p vfs-cli --lib knobs::tests::generated_knobs_doc_matches_declarations -- --exact
```

If you add a CLI flag or a runtime knob, regenerate and commit the result in
the same change. Every knob needs a class, default, owner, and gate; a
compatibility kill switch also needs a documented sunset criterion.

## Build, test, gate

```bash
cargo +nightly build --release --workspace --bins   # toolchain is pinned; use nightly
scripts/gate.sh                                     # the single dev + CI entrypoint
```

`gate.sh` runs, failing on the first error: `fmt --check`, `clippy -D
warnings`, workspace tests, release build, the shell suite
(`crates/vfs-cli/tests/all.sh` with `VFS_GATE_STRICT=1`, where a SKIP is red),
`phase8-validation.py --smoke`, and the consistency canon.

Narrower loops while iterating:

```bash
cargo +nightly test --workspace
VFS_GATE_STRICT=1 VFS_BIN="$PWD/target/release/vfs" crates/vfs-cli/tests/all.sh
VFS_BIN="$PWD/target/release/vfs" crates/vfs-cli/tests/test-run-resume-hardening.sh
```

Shell tests each run out of their own `mktemp -d` with trap cleanup and honor
`VFS_BIN`. Never run the corruption torture test concurrently with another
mount, suite, or benchmark.

Full detail — Python harnesses, pjdfstest profiles, benchmark policy, the
manual macOS gate — is in [docs/TESTING.md](docs/TESTING.md).

## Contracts you cannot change quietly

These are consumed by an external daemon (the Droid handoff stack). Changing
them is a breaking change requiring a coordinated rollout, not a refactor.

* **Reserved startup exit statuses**: `3` session genuinely live, `4` mount or
  sandbox install failed, `5` session missing/malformed, `126` found but not
  executable, `127` not found. The wrapped command's own status passes through
  unchanged.
* **Artifact version negotiation**: `vfs version --json` reports
  `artifactVersion`, `minSupportedArtifactVersion`, and the `features` map.
  Bumping the artifact version without extending `adopt`'s forward migration
  strands every receiver on an older build.
* **Manifest shapes**: the one-line JSON emitted by `pack`, `adopt`, and
  `status --json`. Fields are additive; renaming or removing one breaks the
  receiver.
* **Session store layout** (`~/.vfs/run/<id>/`): owned by `vfs`. Sessions are
  *installed* only through `adopt` — the previous hand-written materialization
  contract is exactly what `adopt` replaced, so do not re-document the layout
  as something a receiver may assemble itself.
* **Schema versions**: `vfs migrate` must land any supported old schema at
  CURRENT. A new schema version needs a migration path from every version
  still in `minSupportedArtifactVersion` range.

## Session handoff, in one pass

The pipeline most changes touch, in order:

1. `vfs run --session <id> --seed-pin <commit>` — create the session and
   capture the checkout's dirty state atomically under the exclusive lock,
   then downgrade to the lifetime shared lock before mounting.
2. `vfs seed` — the standalone form. Birth-time only, staging-then-publish, so
   a failed seed leaves the live delta retryable. Git-ignored files are
   excluded by design; that boundary is the portable-state contract.
3. `vfs pack` — exclusive lock, reject live sessions with exit `3`, do all
   pruning/migration/compaction on a private staging copy, publish by rename
   with rollback and crash recovery.
4. `vfs adopt` — verify integrity, migrate forward, verify the receiving
   checkout's `HEAD` against the recorded seed pin, publish by a single
   rename that is the commit point.
5. `vfs run --session <id>` — resume, running the recovery ladder first: roll
   forward an interrupted pack, detach a stale mount, reap dead proc records.

Liveness is derived from advisory locks plus proc records, both released by
the kernel on process death; that is what makes the classification
crash-consistent. Do not add a liveness signal that survives `SIGKILL`.

## Known gaps and gotchas

Not regressions — deliberate, documented state. Don't "fix" them by accident.

* **io_uring coverage is local-only.** CI kernels ship
  `fuse.enable_uring=N`, so the mount falls back to the legacy channel and the
  uring legs are honest only on a local kernel with it enabled. A "starting
  fuse-over-io_uring queues" log line does not mean uring served requests.
* **Anything a harness runs inside `vfs run` must exist inside the sandbox.**
  The sandbox hides `$HOME` and the temp dirs, so a host path that happens to
  work on the CI image can vanish on a developer's machine. Use
  `sandbox_python()` for interpreters and the `GIT` env var from
  `pin_distro_git()` for git, both in `scripts/validation/lib/common.py`.
  Harness helpers live in that module: do not copy one into a harness, because
  the copies drift and the drift is invisible until the gate runs somewhere
  unusual.
* **macOS runtime validation is a manual release gate.** CI builds and clippys
  on macos-latest, but `scripts/validation/macos-nfs-git-validation.sh` plus
  the Seatbelt spot-checks in docs/TESTING.md must run on real hardware before
  shipping a release that advertises macOS.
* **NFS `UNSTABLE`+`COMMIT` is deliberately unimplemented.** Every WRITE is
  `FILE_SYNC`-honest and `NFSPROC3_COMMIT` returns `PROC_UNAVAIL`. Implementing
  it with imperfect verifier semantics reintroduces the data-loss class
  invariant 3 exists to prevent.
* **`TMPDIR` is overridden process-internally** to contain `turso_core`
  0.5.3's un-unlinked `tursodb-ephemeral-*` sort-spill files
  (`vdbe/execute.rs:10096`). The override does not leak into `run`/`exec`
  children. Remove the workaround only after the upstream unlink fix lands.
* **Benchmarks are a local gate, not CI.** Serialized, median-of-5, fresh
  release build, against a pinned baseline; single runs are noise. Red is a
  per-phase median regression >5% relative *and* >10ms absolute.
* **Known flakes**: `concurrency_integrity::active_workers` under full parallel
  load, and an occasional mcp-server "stdio session failed" under back-to-back
  gate load. Rerun before concluding regression.

## Changing docs

* `README.md` — what Vfs is and why the handoff contracts look the way they
  do. Keep examples copy-pasteable and *actually run them* before committing.
* `docs/MANUAL.md` — prose sections are hand-written; the command reference
  between the generation markers is not.
* `docs/SPEC.md` — schema and runtime invariants. Update it in the same change
  as a schema version bump.
* `CHANGELOG.md` — user-visible changes. Behavior-preserving moves don't need
  an entry; anything that changes a contract above does.

Verify before you claim. A file existing, a path matching, or a green CI run
is not evidence that a behavior works — run it.
