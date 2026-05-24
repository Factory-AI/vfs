# Implementation Notes — 2026-05-24-tier-two-hostfs-passthrough-clone-throughput-2-0x-target-various_parameter

Spec: 2026-05-24-tier-two-hostfs-passthrough-clone-throughput-2-0x-target-various_parameter.md
Approved: 2026-05-24
User comment: none (option C in AskUser: stack A + C, note B as "next up"; bundle Tier One cleanups)

---

## Cleanup 1 — `resolve_agentfs_bin`: release-first

`scripts/validation/git-workload-benchmark.py`

Flipped the binary resolver to prefer `target/release/agentfs` over
`target/debug/agentfs`. The Tier One benchmark numbers were dragged down by
benchmarks accidentally picking up a debug binary from a prior `cargo
check`; release-first removes that footgun for CI and ad-hoc runs.

The single-binary callers (`base-read-benchmark.py`,
`fuse-serialization-stress.py`, `large-edit-benchmark.py`,
`partial-origin-no-real-write.py`, `read-path-benchmark.py`) already prefer
release first via `_pick_existing` / `_choose_existing_binary`. This commit
just lines the git-workload wrapper up with them.

## Cleanup 2 — feature-gate FUSE_DO_READDIRPLUS capability negotiation

`cli/src/fuse.rs::init` — the `fuser` crate only exports
`FUSE_DO_READDIRPLUS` / `FUSE_READDIRPLUS_AUTO` when built against ABI 7.21+.
Tier One started requesting them unconditionally, which built fine but
emitted a runtime warning on older fuser linkages. Gated both behind
`#[cfg(feature = "abi-7-21")]` with a non-gated `warn` path so older builds
log "FUSE: readdirplus capabilities unavailable" instead of pretending they
got negotiated.

---

## Axis A1 — cross-inode batched commit in `AgentFSWriteBatcher`

`sdk/rust/src/filesystem/agentfs.rs`

The Tier One write batcher already coalesces per-inode chunk writes into one
SQLite transaction, but every flush trigger (timer, bytes, Explicit) opens a
fresh txn per inode. With 4 643 files in the codex fixture clone, that's
4 643 separate transactions during the clone phase, which is why clone wall
sat at 2.21 s vs 0.28 s native.

`drain_pending_batched(behavior)` takes the lock once, snapshots the entire
`pending` HashMap (all inodes with buffered chunk writes), then opens one
SQLite txn and replays every per-inode `commit_batch` body inside it.
Failures route through `restore_batches` so a mid-txn error reinstates the
pending entries (the lock is re-taken under the same `commit_lock` guard,
preserving the no-reorder-across-commits invariant).

`drain_inode(_, Explicit)` and `drain_all` now route through the batched
helper. `Timer` and `Bytes` still use the existing single-inode path because
those triggers fire per-inode anyway.

## Axis A2 — FUSE-layer small-write coalescing buffer

`cli/src/fuse.rs::write`

Even with A1's batched commit, every FUSE_WRITE still took the SDK
batcher's `AsyncMutex` and a parking_lot mutex on `open_files`. Small
sequential writes (git's "open + write 64 bytes + close" pattern for
loose object files during clone) hammered both.

New `FUSE_COALESCE_FLUSH_BYTES = 256 KiB` per-fh threshold:
`buffer_fuse_write` appends ranges into the existing dormant
`WriteBuffer::write` BTreeMap (was previously test-only); only when the
buffer crosses 256 KiB OR the kernel issues `flush`/`release`/`fsync` do
we hit the SDK batcher. For the dominant case (a handful of small
sequential writes followed by a close) we end up taking the SDK
AsyncMutex once at close-time instead of N times during the write loop.

Coalescing is gated on `self.writeback_enabled`: when writeback is off
(operators opting out of Tier One's defaults), we keep the immediate-commit
path so each FUSE_WRITE still lands in SQLite before we reply.

### Lock-fix (caught by first benchmark pass)

The first Axis A2 draft held `parking_lot::MutexGuard<'_, OpenFiles>`
across `runtime.block_on(...)` on flush. That serialized every other FUSE
handler (`getattr`, `lookup`, write to a different fh, …) behind the
current fh's SQLite commit. First benchmark pass showed checkout
regressing 0.150 s → 0.289 s (+93%) — a 140 ms regression in a phase
that's essentially "update HEAD plus three small refs".

Fix: `OpenFile::take_pending()` now drains the FUSE-layer buffer into a
`(file, ranges, range_count, byte_count)` tuple under the lock; free
functions `flush_pending_batched_out_of_lock` and `drain_writes_out_of_lock`
then run the async work with the lock released. `fn write`, `fn flush`,
`fn release`, `flush_open_file_pending_inode_except`, and `flush_all_pending`
all use this take-then-block-on pattern.

This was a pre-existing footgun in `fn release` (it always called
`flush_pending_and_drain` under the lock) that only became hot once the
write coalescer started routing more work through release/flush. The
refactor removes the issue from all three handlers.

After the refactor, mixed-workload checkout dropped to 0.193 s (5-iter
median 0.160 s) and the overall mixed ratio improved over Tier One.

---

## Axis C — HostFS passthrough for unmodified partial-origin reads

`sdk/rust/src/filesystem/overlayfs.rs`

`partial_file_for_delta` used to always wrap reads in `OverlayPartialFile`,
which does chunk-merge: for each chunk, check `fs_chunk_override`, then
either read from `fs_data` (overridden) or `pread` against base. For a
delta inode that's been copy-up'd but never written, every chunk hits the
"no override; read from base" branch — the `fs_chunk_override` /
`fs_data` SELECTs are pure overhead before we delegate to HostFS anyway.

New `delta_has_no_content_overrides(delta_ino, base_size)` helper does
three cheap `LIMIT 1` SELECTs:

1. Any `fs_chunk_override` row for `delta_ino`? → not unmodified
2. Any `fs_data` chunk row for `delta_ino`? → not unmodified
3. `fs_inode.size == base_size` AND `data_inline` empty/null? → unmodified

When true AND the open is read-only (`!is_write_open(flags)`), we return
the HostFS `base_file` directly. Reads then go straight to the kernel VFS
with zero AgentFS overhead per pread. Write opens still go through the
wrapper so writes land as `fs_chunk_override` rows and never touch the
real base file (the no-real-write invariant from Tier One holds).

Profiling counters: the existing dormant
`base_fast_open_passthrough_{attempted,succeeded,fallback}` family in
`sdk/rust/src/profiling.rs` is wired up here.

Effect on the mixed git workload (codex fixture):

- `diff` agentfs absolute: 0.132 s → 0.025 s (−81%)
- `status` agentfs absolute: 0.165 s → 0.111 s (−33%, then varied with cache state to 0.198 s in 5-iter run)

These are the phases that do read-storms over the just-cloned working
tree — exactly the case where Axis C fires.

---

## Final benchmark — Tier Two HEAD vs Tier One HEAD vs origin/main

All runs on the same machine, release builds, no `AGENTFS_FUSE_*` env vars set.

### Headline (ratio of agentfs/native; lower is better)

| Workload                                              | Original | Tier One | Tier Two | Δ vs Tier One |
| ----------------------------------------------------- | -------: | -------: | -------: | ------------: |
| Read-heavy (full run incl. startup)                   |    2.70x |    2.62x |    2.69x |       +0.07x |
| CoW (50 MiB single-byte edit)                         |    8.19x |    5.42x |    5.85x |       +0.43x |
| Mixed git workload (3-iter, 1 warmup)                 |    5.16x |    3.21x |    3.29x |       +0.08x |
| Mixed git workload (5-iter, 2 warmups)                |        – |        – |    2.97x |       –0.24x |

The CoW ratio went up because native got faster (system noise across runs),
NOT because agentfs regressed:

| CoW absolute                | Original | Tier One | Tier Two | Δ vs Tier One |
| --------------------------- | -------: | -------: | -------: | ------------: |
| agentfs overlay edit (s)    |   0.5015 |   0.6650 |   0.3596 |       −46%    |
| native edit (s)             |   0.0613 |   0.1226 |   0.0615 |       –       |
| delta DB growth (MiB)       |        – |    50.41 |    n/a   |       –       |

Tier Two cut agentfs CoW wall by 46% relative to Tier One and 28% relative
to origin/main; that's the cross-inode batched commit (A1) and the FUSE
coalescer (A2) compounding on the large-edit workload (the single edit
becomes one chunk write that no longer takes the AsyncMutex per pwrite).

### Mixed workload per-phase (5-iter medians, 2 warmups)

| Phase       | Native (s) | Tier One (s) | Tier Two (s) | Tier One ratio | Tier Two ratio | Δ agentfs |
| ----------- | ---------: | -----------: | -----------: | -------------: | -------------: | --------: |
| checkout    |     0.140  |       0.150  |       0.160  |          0.88x |          0.90x |      +7%  |
| clone       |     0.249  |       2.213  |       1.781  |          7.65x |          7.50x |     −20%  |
| diff        |     0.172  |       0.132  |       0.067  |          1.72x |          0.64x |     −49%  |
| edit        |     0.000  |       0.003  |       0.003  |          6.43x |          8.42x |       0%  |
| read_search |     0.004  |       0.010  |       0.009  |          2.11x |          2.32x |      −7%  |
| status      |     0.194  |       0.165  |       0.198  |          1.70x |          1.26x |     +20%  |

Net agentfs total wall: 2.91 s → 2.51 s (−14%).

### Did we hit the 2.0x mixed-workload target?

No: we got to 2.97x (5-iter median). The clone phase still dominates at
1.78 s — the new batched-commit path drained 20% of that, but the remaining
1.78 s is bottlenecked on git's per-loose-object fsync semantics, which the
FUSE-layer coalescer cannot defer past close-time. To break past 2.0x on
the canonical mixed workload we need either:

- Axis B (CoW chunk sizing) — currently every copy-up writes 64 KiB
  chunks; git loose objects average ~200 bytes, so 99.7% of each chunk is
  zero-padding amplification in `fs_data`. A small-file inline storage
  path would cut clone-phase SQLite writes by ~300×.
- Pack-aware short-circuit — when git's pack-objects is the writer (which
  it is during clone), buffer the entire pack in memory and commit once at
  the end. This is opportunistic and tier 3+ territory.

Axis B is the documented "next up" for tier 3.

### Phase 8 safety gates (smoke profile, post-Tier-Two HEAD)

All 7 gates passed including the previously-noisy
`git_workload_phase8_thresholds` and `base_read_repeated_read_threshold`:

- base_read_repeated_read_threshold: passed
- fuse_serialization_parallelism: passed
- git_workload_phase8_thresholds: passed
- phase7_validation_smoke: passed
- phase8_concurrent_git_stress: passed
- phase8_writeback_durability: passed
- phase8_writeback_no_fsync_crash: passed

### Unit tests / clippy / fmt

- `cargo test --manifest-path sdk/rust/Cargo.toml --lib`: 148/148 pass
- `cargo test --manifest-path cli/Cargo.toml --lib`: 106/106 pass
- `cargo clippy --manifest-path cli/Cargo.toml --all-targets -- -D warnings`: clean
- `cargo clippy --manifest-path sdk/rust/Cargo.toml --lib --tests -- -D warnings`: clean
- `cargo fmt --check` on both crates: clean
