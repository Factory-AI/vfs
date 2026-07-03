# Phase 1+2 findings — metadata profiling and READDIRPLUS A/B

Fixture: `.agents/benchmarks/fixtures/codex` (~63 MiB real bare clone).
Binary: `cli/target/release/agentfs`. N=9 measurement iterations, 2 warmups.
Workload: clone → checkout → status → read_search → edit → diff → fsck.

## Headline: clone dominates everything

Control (readdirplus=auto), agentfs absolute medians:

| phase       | native | agentfs | ratio  |
|-------------|--------|---------|--------|
| clone       | 0.637s | 11.43s  | 14.67x |
| checkout    | 0.290s | 0.319s  | 1.26x  |
| status      | 0.278s | 0.532s  | 1.77x  |
| read_search | 0.011s | 0.082s  | 6.22x  |
| diff        | 0.062s | 0.121s  | 2.72x  |
| fsck        | 0.341s | 0.392s  | 1.07x  |
| **overall** | 1.97s  | 14.04s  | 7.39x  |

Clone is ~80% of total agentfs time. checkout/fsck are already near native.
NOTE: this is far worse than the stale "clone ~1.87s" figure the Tier-4 spec
assumed; on a real packed repo the clone phase is the entire problem.

## Why clone is slow (per-phase counters, single profiled run)

Clone phase counters of note:
- `fuse_write_count` 4939, `fuse_write_bytes` 52.7 MB, `fuse_flush_count` 4738,
  `fuse_release_count` 4783.
- `agentfs_batcher_enqueues` 4738 vs `agentfs_batcher_drains_explicit` 4692 —
  **nearly one explicit drain (SQLite commit) per file**. The write batcher is
  defeated because git flushes/closes each loose object and pack, forcing a
  drain on release.
- `agentfs_batcher_commit_latency_ns_total` ~1593 ms (SQLite commit time).
- `fuse_dispatch_wait_nanos` ~1531 ms (workers waiting).
- `connection_wait_count` 63,705 (cheap each, but enormous count).
- `fuse_adapter_inval_inode_notifications` 19,914 + entry 5,448.
- `fuse_readdir_plus_count` only 21 — **clone barely uses readdir.**

Conclusion: clone is bound by per-file write→flush→release→explicit-drain→
SQLite-commit amplification, plus raw FUSE write volume. It is a storage-path
cost, not a metadata-lookup or transport cost.

## READDIRPLUS=always A/B (per-phase callback medians, 9 iters each)

| phase    | lookup+getattr auto | always | change | readdir→readdirplus |
|----------|---------------------|--------|--------|---------------------|
| clone    | 23,483              | 23,509 | +0.1%  | unaffected          |
| checkout | 7,569               | 7,350  | -2.9%  | getattr -10.5%      |
| status   | 3,228               | 3,016  | -6.6%  | 814 readdir→0       |
| diff     | 1,180               | 779    | -34.0% | lookup -91.7%       |
| read_sea | 71                  | 70     | -1.4%  | n/a                 |
| fsck     | 294                 | 295    | +0.3%  | unaffected          |

- `always` strictly reduces metadata callbacks where readdir is used (diff -34%,
  status -6.6%, checkout getattr -10.5%); **no phase increases** lookup+getattr.
- Safety is identical (same entry/attr TTLs and invalidation regime).
- It does **not** touch clone, so it cannot move the overall ratio.

### Metadata gate verdict
- Callback criterion: PASS (diff -34% ≥ 10%, no increases elsewhere).
- Wall-time criterion: INCONCLUSIVE — clone's large variance swamps the
  sub-second phases; no evidence of wall regression, callbacks strictly down.
- Safety: unchanged.

`READDIRPLUS=always` is a clean, safe, measurable reduction in kernel
round-trips, but it is a second-order win: the 1.5x target is gated entirely by
the clone storage path, which neither readdirplus nor a transport (io_uring)
change addresses.

## CORRECTION (post-implementation, cleaner profiled runs)

The earlier "clone is SQLite-commit-bound (4692 explicit drains, ~1593ms commit
latency)" conclusion came from a single COLD profiled run and is wrong on the
mechanism. Cleaner profiled runs show:

- `agentfs_batcher_drains_explicit` = 4692 in BOTH deferred-release and legacy
  commit-on-close modes — i.e. removing the flush/release drain did **not**
  change the drain count. Those explicit drains come from **git's own fsync()
  calls** (durability barriers) and truncate, routed through `File::fsync ->
  drain_writes`, NOT from file close. They cannot be deferred without breaking
  the fsync durability contract.
- Clone commit latency is only ~0.7s and dispatch wait ~0.4s of a ~4-12s clone.
  The dominant cost is **per-operation overhead across ~28,000 FUSE→SQLite
  round-trips** (13.7k lookups + 9.8k getattrs + 4.9k writes), plus 63.7k
  connection-pool acquisitions.

Implication: the real clone lever is **per-operation cost** (FUSE transport
+ SQLite/connection overhead × op-count), i.e. the originally-planned io_uring
transport spike and/or reducing per-op connection/query overhead — NOT commit
batching. readdirplus=always (shipped, real callback win on diff/status/checkout)
and the deferred-release change do not move clone.

The deferred-release drain + global pending-bytes cap are kept because they are
correct and safe (global cap bounds memory; deferral is neutral-to-win for
non-fsync write bursts and a no-op under git's fsync-heavy clone), but they are
NOT the clone speedup the pivot hoped for.

NOTE: wall-clock medians during this session are unreliable (concurrent load on
the host inflated clone to 9.9-12.4s with >1s stdev vs 3.8-5s unloaded). Counter
deltas (deterministic) are the trustworthy signal here.
