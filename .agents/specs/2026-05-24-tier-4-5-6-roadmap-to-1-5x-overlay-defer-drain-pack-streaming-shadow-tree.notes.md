# Implementation Notes — Tier 4/5/6 roadmap

Spec: 2026-05-24-tier-4-5-6-roadmap-to-1-5x-overlay-defer-drain-pack-streaming-shadow-tree.md
Approved: 2026-05-24 ("Approve as written — start Tier 4 immediately")

---

## Tier 4 implementation log

### What landed

1. `AgentFSWriteBatcher` got four new methods:
   - `peek_pending(ino, offset, size)` — snapshot of pending writes overlapping the read window, returned as already-normalised, clipped ranges. Read lock only.
   - `peek_pending_max_end(ino)` — largest `offset + len` across all pending ranges. Lets `getattr` / `lookup` reflect pending size growth without a drain.
   - `truncate_pending(ino, new_size)` — drop ranges past `new_size`, clip spanning ranges.
   - `discard_pending(ino)` — used by `unlink` / `rename` overwrite / `remove` when an inode row is deleted, so no orphan `fs_data` rows get inserted by a later batched drain.

2. `AgentFSFile::pread` rewritten to consult the batcher overlay first, then merge over SQLite-resident bytes. Crucially: peeks the batcher BEFORE acquiring the pool connection and DROPS the connection BEFORE the splice loop. The earlier in-progress version held the conn across `state.lock().await` and deadlocked the 1-slot ephemeral pool — the regression test `setattr_guard_mismatch_does_not_truncate` and the cli encrypted-write tests caught it.

3. `AgentFSFile::pwrite` / `pwrite_ranges` routes through `batcher.enqueue` whenever the batcher is wired. `drain_writes` is no longer called on the write path.

4. `AgentFSFile::truncate` calls `batcher.truncate_pending` BEFORE the synchronous drain so the overlay agrees with the SQLite truncate.

5. `AgentFSFile::fsync` remains the explicit durability barrier (still drains).

6. `AgentFS::getattr` / `AgentFS::lookup` / `AgentFS::lstat` / `AgentFS::stat` no longer call `drain_inode_writes`. Instead they read SQLite, then call `merge_pending_size` (new helper) to OR in `peek_pending_max_end`. Lookup's old `drain_inode_writes(child_ino)` was the proximate cause of the 30-second `ConnectionPoolTimeout` once Tier 4 actually put writes into the batcher: lookup held the only conn permit then drain_pending_batched tried to acquire it.

7. `AgentFS::unlink` / `AgentFS::rename` (both the path-based and trait impls) / `AgentFS::remove` call `batcher.discard_pending(ino)` immediately before deleting the inode row. Without this, the batched-drain path (Explicit drains commit ALL pending inodes in one txn) would try to `INSERT` into a missing `fs_inode` row and fail the entire batch with `Fs(NotFound)`. The `unlink_during_pending_writes_no_orphan` unit test pins this invariant.

8. `AgentFSWriteBatcher::enqueue` now calls `attr_cache.remove(ino)` so that consumers of cached attrs (mtime, ctime, link count) don't see pre-write state after a successful `pwrite` returns. `getattr` also re-caches the OR'd size so cached_attr matches what getattr returned.

9. FUSE `flush_pending_inode` no longer calls `drain_inode_writes`. The per-fh FUSE WriteBuffer still flushes into the SDK batcher, but the batcher's pending writes serve reads through the overlay — no synchronous SQLite commit on every FUSE read.

10. CLI `write_filesystem` and the `write_file` test helper call `drain_all` before returning, since they're one-shot operations whose written bytes must be durable for the next opener (which is often a different AgentFS instance with its own pool).

### Tests

- 157 SDK lib tests pass (148 pre-existing + 9 new Tier 4 overlay tests: `pread_after_uncommitted_pwrite_sees_pending`, `..._partial_overlap`, `pread_in_unwritten_region_returns_sqlite`, `truncate_drops_pending_beyond_new_size`, `truncate_clips_range_spanning_boundary`, `getattr_reflects_pending_size_growth`, `concurrent_writers_overlay_merge`, `unlink_during_pending_writes_no_orphan`, `fsync_drains_overlay_to_sqlite`).
- 106 CLI tests pass after the `write_filesystem` drain + the FUSE flush_pending_inode refactor.
- clippy clean on both sdk and cli; cargo fmt applied.
- Phase 8 smoke: all 7 gates pass (`base_read_repeated_read_threshold`, `fuse_serialization_parallelism`, `git_workload_phase8_thresholds`, `phase7_validation_smoke`, `phase8_concurrent_git_stress`, `phase8_writeback_durability`, `phase8_writeback_no_fsync_crash`).

### Benchmark result — honest assessment

9-iter median on the codex fixture (`.agents/benchmarks/tier-four-post/mixed-head.agg.json`):

| Metric | Tier 3 final | Tier 4 final | Δ |
| --- | ---: | ---: | --- |
| Mixed ratio median | 2.73x | **3.24x** | +18% (worse) |
| agentfs absolute median | 2.28 s | **2.47 s** | +8% (worse) |
| Native median | 0.824 s | 0.717 s | machine drift |
| ratio stdev | 1.67x | **1.72x** | comparable |

**Tier 4 did NOT deliver the spec's ~2.5x target.** The per-iter ratios on the 9-iter run ranged from 1.61x to 4.71x (one rc=1 failure) — the high variance dominates any signal that the overlay alone would have produced.

Per-phase tells the more honest story:

| Phase | Tier 3 agentfs | Tier 4 agentfs | Δ |
| --- | ---: | ---: | --- |
| checkout | 195 ms | **117 ms** | −40% (better) |
| clone | 1800 ms | 1790 ms | flat |
| status | 255 ms | 270 ms | +6% (worse, within noise) |
| diff | 117 ms | 175 ms | +50% (worse) |
| read_search | 9 ms | 14 ms | +56% (worse, small absolute) |
| edit | 2.5 ms | 4 ms | +60% (worse, small absolute) |

The read-heavy `checkout` phase improved meaningfully (overlay paying off), but `diff`/`read_search` regressed — most likely the two `state.lock().await` acquires per `pread` (peek_pending_max_end + peek_pending) adding latency that wasn't there before. The lock contention vs the SQLite drain it replaces is a wash on these tight-read paths.

### Why Tier 4 alone isn't enough

The spec was honest: Tier 4 lands the foundation, Tier 5 (defer release/forget drain + pack-aware streaming writer) is what actually moves the perf needle. With Tier 4 in place:

- `release` / `forget` STILL drain in `cli/src/fuse.rs` (Tier 5 Axis E will defer)
- Sustained sequential writes on a single fh STILL flow through the per-chunk batcher path (Tier 5 Axis G adds a streaming writer)
- Lookups STILL OR in `peek_pending_max_end` even when the inode has no pending writes — could be made cheaper with a fast-path inode-has-pending atomic flag

The good news: the FOUNDATION is right. The unit tests prove read-after-write consistency works without a synchronous SQLite drain. Tier 5 can safely defer the close-time drain because reads will still observe pending writes through the overlay.

### Latent bugs surfaced

Tier 4 exposed three pre-existing bugs that the synchronous drain-on-every-op pattern was masking:

1. **Single-conn pool deadlock**: `lookup` called `drain_inode_writes` while holding the pool's only conn permit. Pre-Tier 4 this was a no-op (batcher always empty after each pwrite); Tier 4 made batcher have actual pending data, exposing the deadlock.

2. **Orphan rows on unlink/rename**: `discard_pending` is now mandatory at every inode-delete site. Pre-Tier 4 the batcher was always empty at those points; Tier 4 made it possible for a later batched drain to commit writes for a deleted ino.

3. **CLI write_filesystem durability**: a fresh AgentFS opener (e.g. `cat`) didn't see writes from a prior `write_filesystem` invocation. Tier 4 surfaced this; we added an explicit `drain_all` on return.

All three are now fixed in this commit set. They would have been Tier 5 footguns if not caught now.

### Go/no-go for Tier 5

Despite the mixed benchmark numbers, recommend GO on Tier 5:
- Foundation is correct (tests + Phase 8 prove it)
- Read-heavy checkout improved (overlay works)
- Bottleneck shifted from SDK to FUSE close-time drain — exactly where Tier 5 attacks
- Tier 4's regressions on diff/read_search are small absolute (~50-80ms) and within the lock-contention overhead that a fast-path optimisation can remove cheaply

Conservative call: run Tier 5 implementation on a feature branch, measure, decide whether to ship.

