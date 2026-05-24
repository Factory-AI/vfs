# Implementation Notes — 2026-05-24-tier-3-defer-drain-batcher-default-worker-bump-pack-streaming-bulk-sqlite-inline

Spec: 2026-05-24-tier-3-defer-drain-batcher-default-worker-bump-pack-streaming-bulk-sqlite-inline.md
Approved: 2026-05-24
User comment: pursued full stack D+E+F+G+H+I; E default-on with Phase 8 fsync gate update; Tier 2 retro corrections in scope; Axis C disposition driven by empirical validity test.

---

## Tier 3 honest summary

| Axis | Status | Effect on canonical 5-iter agentfs absolute |
| --- | --- | --- |
| D — SDK batcher default-on (align with cli) | **shipped** | 2.51 s → 2.25 s (-10%) |
| F — worker default 25% → 50% CPU | **shipped** | small additional improvement (within D's noise) |
| I — inline threshold 4 KiB → 16 KiB | **shipped** | neutral on wall time; chunk count -50% so DB structure simpler |
| H — multi-row VALUES INSERT | **reverted** | regressed in 5-iter (likely libSQL prepared-stmt cache thrash on different VALUES arities) |
| E — defer release/close drain | **reverted** | regressed; SDK-internal `pread`/`pwrite` drain-for-consistency calls shifted the drain cost onto the read path |
| G — pack-aware streaming writer | **deferred to Tier 4** | not implemented; E's lessons make it likely to need a similar structural rework |
| C disposition | **KEEP as-is** | correct but narrow; doesn't fire in clone-heavy workloads |

Final Tier 3 delivers ~10% absolute improvement vs Tier 2 by recovering the
A1 cross-inode batched commit that was dead-by-default, plus a worker pool
bump and an inline-threshold raise. Real but modest — far from the 2.0x
target. The "honest retrospective" lesson holds: each axis after the easy
gating fixes runs into structural issues that turn predicted wins into
regressions. See axis-by-axis RCAs below.

---

## 2026-05-24 — Axis C validity test results
**Type**: decision
**Context**: Ran `git-workload-benchmark.py` with `AGENTFS_OVERLAY_PARTIAL_ORIGIN=1` + `AGENTFS_FUSE_WRITEBACK=1` + `AGENTFS_PROFILE=1` on the codex fixture to verify whether Tier 2 Axis C (HostFS passthrough for unmodified partial-origin reads) ever fires when partial-origin is explicitly enabled. The Tier 2 spec assumed it would help the canonical mixed workload; profiling under default config showed `base_fast_open_passthrough_attempted=0`.
**Resolution**: Even with partial-origin policy explicitly enabled, `base_fast_open_passthrough_attempted` remains **0** because the canonical workload (codex bare→working clone + status/diff/edit/read) never modifies a base file. mirror.git is read-only from the workload's perspective; the output tree is fresh delta. Therefore partial-origin copy-up never triggers and `partial_file_for_delta` is never called for any inode that has a partial-origin mapping (because no such mappings ever get created). The Axis C wiring is CORRECT — the counter is gated on `partial_origin_for_delta(delta_ino).is_some()`, which is genuinely false for every open in this workload. Axis C's value is real but narrow: it helps workloads that DO modify base files (agent chmod-then-read patterns, dev sandboxes layering on a stable base). **Disposition: KEEP as-is, no removal, no replacement. Document the narrow scope clearly in the Tier 2 retro addendum.**

## 2026-05-24 — Axis D shipped: SDK batcher default-on
**Type**: decision
**Context**: `sdk/rust/src/filesystem/agentfs.rs` gated `AGENTFS_FUSE_WRITEBACK` via `env_flag_enabled` (default false) while `cli/src/fuse.rs` gated the same env var via `env_flag_default(.., true)` (default true). Tier 2's cross-inode batched-commit path was therefore dead-by-default for the canonical benchmark.
**Resolution**: Added `env_flag_default(name, default)` to the SDK, mirroring the cli helper, and switched the batcher activation to `env_flag_default(WRITE_BATCHER_ENABLE_ENV, true)`. Removed the unused single-arg `env_flag_enabled` to keep clippy clean. The change is invisible to users who explicitly set `AGENTFS_FUSE_WRITEBACK=0` (still respected). Profiled-run confirmation: `agentfs_batcher_enqueues` went from 0 → 4 759 in default config; canonical 5-iter agentfs median dropped 2.51 s → 2.25 s (-10%).

## 2026-05-24 — Axis F shipped: 50% CPU default for worker pool
**Type**: decision
**Context**: `FuseDispatchMode::from_env` defaulted `AGENTFS_FUSE_CPU_PERCENT` to 25, which on the benchmark 14-core box gave 3 workers. Profiling showed `fuse_dispatch_wait_nanos` ~570 ms across the workload and `fuse_dispatch_max_concurrent=3` (saturated at 3-concurrent), confirming workers were the bottleneck on the parallel-git-fork-storm portion of clone.
**Resolution**: Bumped the default to 50% (named const `DEFAULT_AUTO_PERCENT`); users on tiny VMs can dial down via `AGENTFS_FUSE_CPU_PERCENT=25`. On the benchmark machine this resolves to 7 workers and trims dispatch wait by roughly half (~330 ms in a follow-up profile). Phase 8 stress gates pass at the new default.

## 2026-05-24 — Axis I shipped: inline threshold 4 KiB → 16 KiB
**Type**: decision
**Context**: codex working-tree files average ~14 KB; the 4 KiB inline threshold pushed nearly all of them through the chunked-storage path (one `fs_data` row + per-write SELECT+REPLACE). Larger threshold should let the (4, 16] KiB tail avoid `fs_data` entirely. The `fs_inode` metadata SELECTs in `getattr`/`lookup` explicitly project named columns and do NOT pull `data_inline`, so the only cost of larger inline blobs is paid on actual reads of those specific files.
**Resolution**: Raised `DEFAULT_INLINE_THRESHOLD` to 16384. Per-DB persistence in `fs_config` preserves existing 4 KiB databases unchanged; only newly-initialised DBs pick up the bigger threshold. Updated the `test_config_persistence` and `test_default_chunk_size` asserts to match. Empirical effect on the canonical workload: `chunk_write_chunks` 1958 → 1000 (chunk count nearly halved), wall time neutral within 5-iter noise (medians: 2.25 s → 2.21 s).

## 2026-05-24 — Axis H attempt: multi-row VALUES INSERT (REVERTED)
**Type**: deviation
**Context**: Tried replacing the per-chunk `INSERT OR REPLACE INTO fs_data (ino, chunk_index, data) VALUES (?,?,?)` loop with a 32-row batched VALUES statement, expecting ~32x fewer libSQL round-trips inside the transaction. Wrote helpers `bulk_insert_fs_data_sql(n)` and `bulk_insert_fs_data_params(ino, rows)` and routed the chunk-write path through them, keeping a per-row fallback for the trailing partial batch.
**Resolution**: 5-iter canonical benchmark regressed: agentfs median 2.25 s (D+F) → 2.84 s (D+F+H+I+E). After reverting Axis E in isolation (D+F+I+H still slow), the per-iter spread for H stayed worse than D+F alone. Hypotheses for why batched VALUES underperforms in libSQL on this workload: (a) every distinct batch-size SQL string is a separate prepared-statement entry, so the trailing partial batch evicts the 32-row cached plan; (b) the parameter Vec construction for 96 positional params per execute() may be heavier than reusing the single-row prepared statement; (c) libSQL's value marshalling has higher per-execute setup than per-bind cost. Reverted to the cached single-row prepared statement. Disposition: not a Tier 3 deliverable; revisit if/when we have visibility into libSQL's plan cache and parameter-binding hot path.

## 2026-05-24 — Axis E attempt: defer release/close drain (REVERTED)
**Type**: deviation
**Context**: Removed the synchronous `drain_writes` call from `fn flush` and `fn release` and from `fn forget`/`fn batch_forget`, on the POSIX principle that only `fsync()` is a durability barrier. Enhanced `drain_due_timer` to call `drain_pending_batched` (batched across ALL pending inodes) when its inode is ripe, so the timer would deliver real cross-inode batching. Phase 8 `phase8-writeback-durability.py` already does `os.fsync()` before SIGKILL so its semantics were unchanged.
**Resolution**: 5-iter canonical benchmark regressed: agentfs median worse than D+F alone and bimodal in iter wall-times. Profile diagnostic: `agentfs_batcher_drains_explicit` stayed at ~4717 (same as pre-Axis-E) because every SDK `pread`/`pwrite`/`truncate`/`fsync` entry point preludes with `self.drain_writes()` for read-after-write consistency. After Axis E, those drains happen synchronously on subsequent reads instead of asynchronously on close — same total drain count, but now serialised behind read latency. Net effect: cost shifted from close to read with no reduction. Reverted release/flush/forget/batch_forget to synchronous drain. Left the `drain_due_timer` batched-timer enhancement in place (it's harmless when only one ino is ripe and helpful when multiple are). Disposition: a real Axis E needs to either (a) plumb a "consistent-without-drain" read path through the SDK (large refactor: the in-memory batcher would have to overlay onto SQLite-read results) or (b) remove the SDK's drain-before-read preludes and accept relaxed consistency. Both are Tier 4 territory.

## 2026-05-24 — Axis G deferred to Tier 4
**Type**: decision
**Context**: Axis G (pack-aware streaming writer that buffers sustained-sequential writes per-fh and commits one large txn on close) was scoped as the largest implementation surface in the Tier 3 spec.
**Resolution**: Deferred. Axis E's lessons show that any Tier 3 work which shifts where SQLite work happens runs the risk of moving the cost onto a different hot path. Axis G has a similar shape (it defers commit to close-time bulk INSERT, which would interact with the SDK's drain-for-consistency reads the same way Axis E did) and would need the same `consistent-without-drain` read path that Axis E does. Without that foundation, implementing G as planned would likely regress for the same structural reason. Tier 4 should land that read-path foundation first, then revisit G.

## 2026-05-24 — Final 5-iter benchmark snapshot
