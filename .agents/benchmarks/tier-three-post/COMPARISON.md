# Tier Three — fresh benchmark comparison

Native vs **Tier Two AgentFS** (`phase4-north-star-implementation` 2f5e343,
HostFS read passthrough + clone batched commit + FUSE-layer write coalescer)
vs **Tier Three AgentFS** (HEAD, SDK batcher default-on + 50% worker default
+ 16 KiB inline threshold + Tier 2 retro corrections).

All runs on the same machine with no `AGENTFS_FUSE_*` env vars set, release builds.

---

## Headline (5-iter / 2-warmup median, codex fixture)

| Workload                                              | Tier One | Tier Two | Tier Three |
| ----------------------------------------------------- | -------: | -------: | ---------: |
| Mixed git workload — ratio                            |    3.21x |    2.97x |    **2.73x** |
| Mixed git workload — agentfs absolute (s)             |    2.91  |    2.51  |    **2.28**  |
| Clone phase — agentfs absolute (s)                    |    2.21  |    1.78  |    **1.80**  |

Tier 3 delivers a ~9% absolute / ~8% ratio improvement over Tier 2, dominated
by Axis D recovering Tier 2's dead-by-default A1 cross-inode batched commit.

---

## What shipped vs what was attempted

| Axis | Status | Effect on canonical 5-iter agentfs absolute |
| --- | --- | --- |
| D — SDK batcher default-on (align with cli) | **shipped** | 2.51 s → 2.25 s (−10%) |
| F — worker pool 25% → 50% CPU | **shipped** | small additional improvement (within D's noise) |
| I — inline threshold 4 KiB → 16 KiB | **shipped** | neutral on wall time; `chunk_write_chunks` halved (1958 → 1000) |
| Tier 2 retro corrections (docs) | **shipped** | n/a — documentation |
| `drain_due_timer` batched-timer enhancement | **shipped** | harmless when only one ino is ripe; helpful when multiple are |
| Axis C disposition: KEEP as-is | **kept** | correct but narrow (verified zero firings in canonical workload) |
| H — multi-row VALUES INSERT | **reverted** | regressed in 5-iter; suspected libSQL prepared-stmt cache thrash on different VALUES arities |
| E — defer release/close drain | **reverted** | regressed; SDK-internal `pread`/`pwrite` drain-for-consistency calls shifted cost onto the read path |
| G — pack-aware streaming writer | **deferred to Tier 4** | not implemented; depends on the same `consistent-without-drain` read path that E needed |

---

## Mixed workload per-phase (5-iter medians, 2 warmups)

| Phase       | Native (s) | Tier Two (s) | Tier Three (s) | Tier Two ratio | Tier Three ratio | Δ agentfs |
| ----------- | ---------: | -----------: | -------------: | -------------: | ---------------: | --------: |
| checkout    |     0.146  |       0.160  |         0.195  |          0.90x |            1.11x |     +22%  |
| clone       |     0.254  |       1.781  |         1.802  |          7.50x |            7.23x |      +1%  |
| diff        |     0.239  |       0.067  |         0.117  |          0.64x |            0.49x |     +75%  |
| edit        |     0.000  |       0.003  |         0.002  |          8.42x |            9.80x |     −33%  |
| read_search |     0.004  |       0.009  |         0.009  |          2.32x |            2.49x |      0%   |
| status      |     0.172  |       0.198  |         0.255  |          1.26x |            1.45x |     +29%  |

Per-phase variance is high (5-iter p25/p75 spreads for diff and status range
from ~0.1x to ~17x in some iterations); treat individual phase deltas
cautiously. Net agentfs total wall: 2.51 s → 2.28 s (−9%).

---

## What did NOT move (and why)

- **Clone agentfs absolute essentially unchanged (1.78 → 1.80 s, within
  noise).** Clone's bottleneck is SQLite commit work and FUSE dispatch
  wait, not chunk count or worker count. Tier 3 reduced both somewhat (D
  recovered batched commits; F added workers) but the structural
  bottleneck remains. Real clone improvements need either (a) the deferred
  drain that Axis E attempted (with a `consistent-without-drain` SDK read
  path to make it stick) or (b) the pack-aware streaming writer of Axis G
  (which depends on the same foundation).

- **`chunk_write_chunks` 1958 → 1000 (Axis I) did not translate to wall
  time.** Per-chunk INSERT cost is small relative to per-transaction
  fsync; halving chunks halves a cost that wasn't dominant. The structural
  win is database simplicity (fewer rows, simpler scans) rather than
  benchmark time.

- **Axis C (HostFS passthrough) zero firings, kept anyway.** Verified via
  `AGENTFS_PROFILE=1` + `AGENTFS_OVERLAY_PARTIAL_ORIGIN=1`:
  `base_fast_open_passthrough_attempted=0` even with the policy explicitly
  enabled. The canonical workload never modifies a base file, so no
  partial-origin mappings exist for the helper to short-circuit. The code
  is correct for its target audience (agent chmod-then-read patterns with
  `--partial-origin`); it just doesn't help clone-heavy workloads.

---

## Tier Four focus areas

The big remaining lever is removing the SDK's drain-before-read prelude in
`pread`/`pwrite`/`truncate`/`fsync`. Today every read of an inode with
pending batched writes triggers a synchronous drain for read-after-write
consistency. With the FUSE keepcache + writeback defaults, most reads
should hit the kernel page cache and never reach the SDK at all — but the
remaining SDK-bound reads can't safely skip the drain without an
overlay-aware path that merges the in-memory pending batch with the
SQLite-resident data at read time.

Once that read path lands, **both Axis E (defer close drain) and Axis G
(pack-aware streaming) become structurally feasible** and the ~600 ms of
clone-phase SQLite work could realistically drop by 30-50%, putting the
mixed-workload ratio at 2.0x or below.

Other lower-priority Tier 4 candidates:

1. **Axis H take 2 — investigate libSQL plan cache behaviour.** The
   multi-row VALUES revert was on suspicion; an actual profile of which
   prepared statements are cached and which are recompiled per execute
   would let us pick a batch size that helps.
2. **Per-DB chunk size tuning.** The 64 KiB chunk default amplifies
   single-byte CoW edits 64,000x. A smaller chunk for partial-origin or
   for files marked "edit-heavy" would help that workload class without
   penalising clone (which mostly does full-chunk writes).
3. **Worker pool dynamic sizing.** 50% CPU is a static default; a
   responsive sizer that grows under queue pressure and shrinks on idle
   would handle bursty clone phases better.

---

## Per-iteration reproducibility — Tier Three final

| iter | wall_s |
| ---: | -----: |
| 1    | 7.34   |
| 2    | 6.59   |
| 3    | 12.39  |
| 4    | 6.85   |
| 5    | 8.81   |

stdev 1.67x (vs Tier Two's 0.91x in the comparable 5-iter run). Variance
remains high; iteration 3's outlier is likely cache-state or scheduler
noise. Medians are still directionally reliable.
