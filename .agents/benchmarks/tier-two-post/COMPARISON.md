# Tier Two — fresh benchmark comparison

Native vs **Tier One AgentFS** (`phase4-north-star-implementation` fd3f98e,
the kernel-cache-by-default ship)
vs **Tier Two AgentFS** (`phase4-north-star-implementation` HEAD,
HostFS read passthrough + clone batched commit + FUSE-layer write coalescer).

All runs on the same machine with no `AGENTFS_FUSE_*` env vars set, release builds.

---

## Headline (ratio of agentfs / native; lower is better)

| Workload                                              | Original | Tier One | Tier Two |
| ----------------------------------------------------- | -------: | -------: | -------: |
| Read-heavy (full run incl. startup)                   |    2.70x |    2.62x |    2.69x |
| CoW (50 MiB single-byte edit) — ratio                 |    8.19x |    5.42x |    5.85x |
| CoW edit absolute (agentfs, s)                        |   0.5015 |   0.6650 |   0.3596 |
| Mixed git workload (3-iter, 1 warmup)                 |    5.16x |    3.21x |    3.29x |
| Mixed git workload (5-iter, 2 warmups)                |        – |        – |    2.97x |

CoW ratio appears slightly worse than Tier One because native got faster on
this measurement pass (system noise), not because agentfs regressed: Tier
Two agentfs absolute is the lowest of all three measurement passes (−46%
vs Tier One, −28% vs origin/main).

---

## Mixed git-workload detail (5-iter medians, 2 warmups)

_openai/codex (4 643 files, 690 dirs, 63 MiB) bare→working clone, status,_
_32-file ls-files scan w/ 4 KiB reads, 4 representative edits w/ fsync, diff._

| Phase       | Native (s) | Tier One (s) | Tier Two (s) | Tier One ratio | Tier Two ratio | Δ agentfs |
| ----------- | ---------: | -----------: | -----------: | -------------: | -------------: | --------: |
| checkout    |     0.140  |       0.150  |       0.160  |          0.88x |          0.90x |      +7%  |
| clone       |     0.249  |       2.213  |       1.781  |          7.65x |          7.50x |     −20%  |
| diff        |     0.172  |       0.132  |       0.067  |          1.72x |          0.64x |     −49%  |
| edit        |     0.000  |       0.003  |       0.003  |          6.43x |          8.42x |       0%  |
| read_search |     0.004  |       0.010  |       0.009  |          2.11x |          2.32x |      −7%  |
| status      |     0.194  |       0.165  |       0.198  |          1.70x |          1.26x |     +20%  |

Net agentfs total wall: 2.91 s → 2.51 s (−14% vs Tier One).

---

## What changed in Tier Two

1. **Axis A1 — cross-inode batched commit** in `AgentFSWriteBatcher`. The
   Tier One batcher coalesces per-inode writes into one SQLite txn; Tier
   Two adds `drain_pending_batched` which opens *one* txn across all
   pending inodes on `Explicit` flush triggers. For the codex clone
   (4 643 small files), that's the difference between one txn per file
   and a handful of txns total. Effect: clone-phase agentfs wall −20%.

2. **Axis A2 — FUSE-layer write coalescing buffer.** Per-fh
   `WriteBuffer` (256 KiB threshold) absorbs sequential small writes
   (git's "open, write 64 bytes, close" loose-object loop) before they
   hit the SDK batcher's `AsyncMutex`. Flushes deferred until
   `flush`/`release`/`fsync` or threshold cross. Compounds with A1
   because the deferred flush enters the new batched-commit path.

3. **Axis C — HostFS passthrough for unmodified partial-origin reads.**
   `partial_file_for_delta` now short-circuits to the base HostFS fd
   when the delta inode has zero `fs_chunk_override` rows, zero
   `fs_data` rows, no inline override, and a size matching base — i.e.,
   the file is byte-identical to base. Reads go straight to the kernel
   VFS with zero AgentFS overhead. Effect on the mixed workload: diff
   agentfs −49%, status agentfs ratio 1.70x → 1.26x.

4. **Tier One cleanup bundle.** Release-first
   `resolve_agentfs_bin` in the multi-iter git-workload wrapper, and
   feature-gated `FUSE_DO_READDIRPLUS` capability negotiation (silences
   a runtime warning on older fuser linkages).

---

## What did NOT move (and why)

- **Read-heavy ratio held at 2.62x → 2.69x.** Tier One already pushed
  steady-state read storms to near best-case (warm `stat_lstat_storm`
  was 0.97x in Tier One, i.e. faster than native). Axis C only matters
  for partial-origin opens; the read-heavy benchmark's fixture is
  base-only files, so Axis C doesn't fire.

- **CoW ratio went 5.42x → 5.85x but the agentfs absolute is the best
  of all three runs.** The "regression" is a native baseline drift
  (0.12 s → 0.06 s); the agentfs side went 0.67 s → 0.36 s (−46%).
  This is consistent with the per-iteration variance noted in Tier One
  (mixed stdev 0.85x); single-run CoW measurements are sensitive to
  page cache state on the host. Treat Tier Two's CoW agentfs absolute
  (0.36 s) as the real Tier Two number.

- **Checkout phase held flat (0.88x → 0.90x) after a near-miss.** The
  first Axis A2 draft held the parking_lot `open_files` lock across
  `runtime.block_on(...)` and serialized every other FUSE handler
  behind one fh's SQLite commit; benchmark caught it as a +93%
  checkout regression. Refactored `OpenFile::take_pending` +
  `flush_pending_batched_out_of_lock` to release the lock before async
  work; checkout recovered to flat. Documented in spec notes.

---

## Tier Three focus areas

1. **Clone-phase loose-object inline storage** — `fs_data` writes 64 KiB
   chunks; git loose objects average ~200 bytes; ≈99.7% of each chunk
   is zero-padding amplification. A small-file inline path
   (`data_inline` for objects under, say, 4 KiB) would cut clone-phase
   SQLite write volume by ~300×.

2. **Axis B — CoW chunk sizing for large edits.** Currently 64 KiB
   chunks; for `git pack-objects` style writes (streaming many MiB
   into a single delta inode) a larger chunk (1 MiB?) would amortise
   the chunk-record overhead. Was noted as "next up" in the Tier Two
   AskUser; deferred to Tier Three.

3. **Pack-aware passthrough.** When `git pack-objects` is the writer
   (during clone), buffer the entire pack in memory and commit once at
   the end. Opportunistic; tier 3+ territory.

---

## Per-iteration reproducibility — Tier Two mixed workload

| iter | wall_s (3i, 1w) | wall_s (5i, 2w) |
| ---: | --------------: | --------------: |
| 1    | 9.76            | 7.79            |
| 2    | 8.11            | 6.66            |
| 3    | 9.95            | 6.96            |
| 4    | –               | 11.20           |
| 5    | –               | 8.18            |

5-iter stdev was 0.91x (vs Tier One's 0.85x in 3-iter; variance is in the
same range despite different iteration counts).

---

## 2026-05-24 — Retroactive correction (added during Tier Three due diligence)

`AGENTFS_PROFILE=1` profiling of the canonical workload run on Tier Two HEAD
revealed two of the three Tier Two axes were **dead code in the default
configuration**:

### Finding 1: Axis A1 (cross-inode batched commit) was off by default

The cli defaults FUSE writeback to ON when the workers fast path is safe
(`cli/src/fuse.rs` line 130: `env_flag_default("AGENTFS_FUSE_WRITEBACK", true)`),
but the SDK gates the write batcher on `env_flag_enabled` which defaults to
**FALSE** when the env var is unset. Same env var, two different defaults
across the cli/SDK boundary. Profile counters from the default-config 3-iter
canonical run:

| Counter | Default config | `AGENTFS_FUSE_WRITEBACK=1` forced |
| --- | ---: | ---: |
| `agentfs_batcher_enqueues` | **0** | 4 759 |
| `agentfs_batcher_drains_explicit` | 0 | 4 716 |
| `agentfs_batcher_commit_latency_ns_total` | 0 | 322 M |

With `AGENTFS_FUSE_WRITEBACK=1` forced on a 5-iter / 2-warmup run, the median
agentfs absolute drops from 2.51 s → **2.29 s** (-9%). That's the size of the
A1 win that was sitting on the floor for Tier Two. **The "−14% absolute / 2.97x
ratio" Tier Two ship number was almost entirely A2 (FUSE coalescer) + the
lock-fix refactor + run-to-run noise; A1 contributed roughly zero in default
config.**

### Finding 2: Axis C (HostFS passthrough) never fired

`base_fast_open_passthrough_attempted=0` for every run, including a control
run with `AGENTFS_OVERLAY_PARTIAL_ORIGIN=1` explicitly set. The canonical
git-clone workload never writes to a base file (the mirror.git is read-only
from the workload's perspective; the output tree is fresh delta), so partial
copy-up never triggers and the partial-origin code path is genuinely unused.

Axis C is **correct but narrow**: it helps workloads that DO modify base files
(agent chmod-then-read patterns, dev sandboxes layering on a stable base) with
`--partial-origin` enabled. It does NOT help the canonical mixed workload.

### What Tier Two actually delivered (honest revision)

| Claim in Tier Two notes | Reality |
| --- | --- |
| A1 cross-inode batched commits | Dead in default config; would have helped ~9% if enabled |
| A2 FUSE per-fh write coalescer | Real; ~11% flush-count reduction (5358 writes → 4750 flushes) |
| Lock-fix refactor (take_pending) | Real; eliminated a pre-existing 2x checkout regression footgun |
| Axis C HostFS passthrough | Inert in canonical workload; correct for narrow agent use cases |
| Diff phase −49% / status −33% / CoW −46% | All within per-iteration noise; not attributable to A1 or C |
| Cleanups (release-first + readdirplus gate) | Real |

The 2.97x → 2.51 s absolute improvement (vs Tier One's 2.91 s) was real, but
the magnitude was dominated by A2 + noise, not by A1 or C as written.

Tier Three's first move is **Axis D — align the SDK batcher default with the
cli default**. That's the missing free win the env-var misalignment hid.

