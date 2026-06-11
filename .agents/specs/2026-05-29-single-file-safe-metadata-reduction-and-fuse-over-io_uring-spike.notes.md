# Implementation Notes — 2026-05-29-single-file-safe-metadata-reduction-and-fuse-over-io_uring-spike

Spec: 2026-05-29-single-file-safe-metadata-reduction-and-fuse-over-io_uring-spike.md
Approved: 2026-05-29
User comment: none

---

## 2026-05-29 — Adapter-level metadata cache counters added (distinct from SDK)
**Type**: decision
**Context**: Spec P1.2 asked for FUSE-adapter `entry_cache`/`attr_cache` hit-miss + invalidation notification counters. Discovered `record_negative_cache_hit/miss` and `record_attr_cache_*` are SHARED between the FUSE adapter (`cli/src/fuse.rs`) and the SDK backend (`sdk/.../agentfs.rs`, `overlayfs.rs`), so the existing counters conflate the two layers and cannot isolate kernel-callback cache effectiveness.
**Resolution**: Added 8 new distinct adapter counters in `sdk/rust/src/profiling.rs`: `fuse_adapter_{entry,attr,negative}_{hits,misses}` + `fuse_adapter_inval_{inode,entry}_notifications`. Wired them at the lookup positive/negative cache decision points, the getattr attr-cache decision point, and both `notify_inval_*` entry points (covering the default deferred path, which previously had zero instrumentation — only the sync path had `fuse_sync_inval_*`). Rejected reusing the shared counters because they'd remain ambiguous. Snapshot serializes the whole struct via serde, so summary JSON auto-includes the new keys. Extended the accumulate unit test; phase65 JSON test left intact.

## 2026-05-29 — Per-phase profiling via SIGUSR1 checkpoints (in-process delta, not isolated remounts)
**Type**: decision
**Context**: Spec P1.2b wants per-phase profile summaries (clone/checkout/status/read_search/diff) instead of one aggregate. The daemon emits a single cumulative summary at process exit. Two options considered: (A) run each phase in its own `agentfs run` over the persisted single-file DB (remount) so each process's exit summary == that phase; (B) emit cumulative checkpoint snapshots mid-run and subtract. Option A gives perfectly isolated counts but starts each phase with COLD adapter/SDK caches, which misrepresents cache effectiveness — exactly the metric the metadata gate needs. Rejected A.
**Resolution**: Implemented B. Added `profiling::report_checkpoint()` (monotonic `phase-checkpoint-<seq>` tagged cumulative summary). In `cli/src/sandbox/linux.rs` the parent installs a SIGUSR1 sigaction (NO SA_RESTART) that only increments an atomic; the existing `wait_for_child` waitpid loop returns EINTR and drains the counter via `drain_profile_checkpoints()` → async-signal-unsafe emission happens in normal context. Sandbox uses only CLONE_NEWUSER|CLONE_NEWNS (no PID namespace), so the workload's `os.getppid()` is the `agentfs run` parent holding the counters. Workload (`git-workload-benchmark.py` GIT_WORKLOAD) calls `profile_checkpoint(label)` after each phase: appends label, signals parent, sleeps 100ms to let stderr flush. Guarded on `AGENTFS==1` so native runs never signal the harness. Analyzer `per_phase_profile_counters()` sorts checkpoints by seq, zips with ordered labels, subtracts consecutive cumulative snapshots → per-phase deltas in `agentfs.per_phase_counters`.
**Smoke result (generated fixture, 40 files)**: 6 checkpoints, labels_aligned=true. Clone dominates (594 lookup / 597 getattr / 119 readdirplus). **Key finding**: `fuse_adapter_entry_hits == 0` in every phase — the positive dentry cache never serves a hit (entry_miss high), while `fuse_adapter_attr_hits` is partially effective (67/530 in clone). This is direct evidence for Phase 2 analysis: readdir-seeded positive entries are not being reused, so `READDIRPLUS=always` may not help lookups unless the retain/forget + epoch path is also addressed.

## 2026-05-29 — P1.1 mutation safety harness (new script, not an extension)
**Type**: deviation
**Context**: Spec said "reuse/extend partial-origin-no-real-write.py". That script is tightly specialized to in-place partial-origin writes against one large base file (sampling ranges, partial-origin env, override-row assertions). Bolting 7 discrete metadata mutation classes + remount reproduction onto it would muddy its single purpose.
**Resolution (continued below)**: Created a sibling script `scripts/validation/metadata-mutation-no-real-write.py` instead. It builds a small base tree, runs a mutation workload through the mount (create/overwrite/truncate/rename/unlink/chmod/utimens + threaded concurrent read-after-write), host-hashes the base tree before/after, then does a SECOND `agentfs run` over the same `--session` DB to confirm remount reproduces every mutation. Asserts base tree byte+metadata identical before vs after mutation AND after remount. All 20 checks pass on the release binary: base unchanged both times, all mutations reproduced on remount. partial-origin script left untouched (still covers its distinct case).

## 2026-05-29 — SURPRISE: spec's cost model is wrong; clone (storage path) is the entire wall
**Type**: surprise
**Context**: Phase 1 profiling on the real codex fixture (N=9, 2 warmup) contradicts the spec's working assumption (stale "clone ~1.87s"). Reality: clone agentfs median = 11.43s vs native 0.637s = 14.67x, ~80% of the 14.04s total (overall 7.39x). checkout/fsck are already ~1.1-1.3x. Per-phase counters show clone is bound by per-file write→flush→release→explicit-drain→SQLite-commit amplification: 4692 explicit batcher drains for 4738 flushes, ~1593ms commit latency, ~1531ms dispatch wait, 63,705 connection acquisitions, 19,914 deferred inode invalidations, and only 21 readdirplus calls.
**Resolution**: Recorded full evidence in `.agents/benchmarks/metadata-ab/FINDINGS.md` (+ control/always aggregates + single clone profile). Implication: NEITHER approved lever moves the dominant phase — readdirplus doesn't touch clone (clone barely reads dirs), and io_uring reduces per-callback transport cost while clone is SQLite-commit-bound, not transport-bound. The real lever is clone's per-file explicit-drain (Tier-5 Axis E: defer release/forget drain so many file closes batch into few commits), which is a STORAGE change outside this spec's approved metadata+transport scope. Surfacing to the user before continuing, per spec Phase 4 ("identify remaining costs from counters and stop rather than introducing scope/security compromises").

## 2026-05-29 — Phase 2 READDIRPLUS=always: clean callback win, second-order
**Type**: decision (pending user direction on overall scope)
**Context**: A/B with N=9 each. `always` reduces lookup+getattr on diff -34% (lookup -91.7%), status -6.6%, checkout getattr -10.5%; no phase increases; clone flat (+0.1%). Safety identical (same TTL + invalidation regime). Wall-time criterion inconclusive due to clone variance swamping sub-second phases.
**Resolution**: Metadata gate callback criterion PASSES. `always` is safe and strictly fewer kernel round-trips, but cannot move the 1.5x target because clone dominates and is unaffected. Holding the default-flip + invalidation tests (P2b) pending the user's call on whether to (a) ship readdirplus + proceed to io_uring as approved (modest, won't hit 1.5x), or (b) pivot to the clone storage path (the real wall).

## 2026-05-29 — P2b shipped + P3 clone storage path implemented (defer release drain + global cap)
**Type**: decision
**User direction**: "Ship readdirplus=always now, then PIVOT to the clone storage path instead of io_uring" + "pick the pareto-optimal version across resource usage and speed gains".
**P2b**: Flipped `readdirplus_mode_from_env` default `Auto`→`Always` (keeps `auto`/`off` explicit rollbacks). Unit test `readdirplus_mode_defaults_to_always_with_rollbacks`. Invalidation correctness is structurally identical (entry/inode invalidation paths are shared regardless of how an entry was seeded) and is covered end-to-end by the mutation harness + the 9-iter `always` A/B run (all rc=0, equivalence held).
**P3 (pareto clone storage path)**:
- Root cause (from P1 counters): `flush`/`release` each forced `file.drain_writes()` = one synchronous SQLite commit per file close (4692 during clone), on the critical path. The original Tier-3 reason for this (SDK reads preluded with `drain_writes`) is OBSOLETE under Tier-4 overlay reads (`pread`/`pwrite` no longer drain).
- Change 1 (latency): `cli/src/fuse.rs` flush/release now only move the per-fh FUSE WriteBuffer into the SDK batcher overlay; they no longer force a commit. Durability preserved by fsync (still drains), the batcher timer/bytes/global triggers, and `finalize()`-on-unmount (drain_all + WAL checkpoint, verified in destroy/Drop). Kill switch `AGENTFS_DRAIN_ON_RELEASE=1` restores legacy commit-on-close.
- Change 2 (memory, pareto): added a global cross-inode pending-bytes cap (`AGENTFS_BATCH_GLOBAL_BYTES`, default 64 MiB). The batcher now tracks `total_pending_bytes` in lock-step with the pending map (debug_assert validates no drift); when a write crosses the cap, enqueue triggers `drain_all(Bytes)`. This bounds RSS so `AGENTFS_BATCH_MS` can be widened to coalesce many closes into far fewer commits without unbounded memory during a clone burst.
- Tests: env-free `test_batcher_global_cap_triggers_full_drain_and_tracks_total` (constructs a batcher with explicit config over a real fs pool — robust against the suite's env-var races) + `test_batcher_discard_pending_updates_total`. 80/80 agentfs tests pass single-threaded. (`overlay_reads_flag_off...` is a pre-existing parallel env-race flake — passes in isolation and single-threaded; my changes don't touch that env var.)
**Next**: release build, run mutation harness + benchmark matrix (control / always / +deferred-release) and sweep `AGENTFS_BATCH_MS` × global cap for the pareto point.

## 2026-05-29 — PA: connection-free cache fast paths (-50.6% clone connection acquisitions)
**Type**: decision + win
**User direction**: "maybe both" — investigate per-op SDK overhead first (counter-measurable), then io_uring.
**Root cause**: `AgentFS::lookup` and `AgentFS::getattr` each acquired a pool connection BEFORE consulting the in-memory caches (`dentry_cache`/`negative_dentry_cache` in `lookup_child`, `attr_cache` in `getattr_with_conn`). Every cache hit therefore paid a full acquire/release of the (async-Mutex + semaphore + timeout-future) connection machinery. Clone's `OverlayFS::resolve_delta_parent` does O(depth) negative delta-parent probes per base-layer lookup — all negative-cache hits, each wasting a connection. Result: 63,733 connection acquisitions for clone (~2.3 per FUSE op), all reuses.
**Fix**: moved the cache checks ahead of `get_connection` in both methods (same caches, same invalidation semantics the code already trusts — provably equivalent correctness, just no connection on a hit): negative-dentry hit → `Ok(None)` connection-free; dentry+attr hit → cached stats + in-memory pending merge connection-free; attr-cache hit in getattr → connection-free.
**Measured (deterministic counters, reliable under host load)**: clone `connection_wait_count` 63,733 → 31,505 (**-50.6%**); total acquisitions -47.5%; `lookup_count`/`getattr_count` unchanged (same logical work). 161/161 SDK tests pass; mutation harness 20/20 (base untouched, remount reproduces all). Profile saved: `clone-profile-fastpath.json`.
**Note**: wall-time benefit not validated this session (host loaded); the counter reduction is the trustworthy signal. Next: io_uring transport spike (PB).

## 2026-05-29 — PB feasibility research: FUSE-over-io_uring (BLOCKER found before coding)
**Status**: research complete, implementation NOT started — surfaced a hard prerequisite.

### Confirmed feasible
- Kernel: CONFIG_FUSE_IO_URING=y; `/sys/module/fuse/parameters/enable_uring` flipped to `Y` (user ran sudo). Runtime-only, resets on reboot.
- Protocol (authoritative: libfuse `lib/fuse_uring.c` + kernel docs/fuse-io-uring.html):
  - INIT stays on /dev/fuse; negotiate FUSE_OVER_IO_URING (bit 41, in init flags2).
  - One ring per CPU core (qid = core). Each queue: N entries; per entry a page-aligned `fuse_uring_req_header` (in_out[128] + op_in[128] + ring_ent_in_out{flags,commit_id,payload_sz}) + an op_payload buffer (= bufsize - header).
  - REGISTER: IORING_OP_URING_CMD, SQE128, cmd_op=1, 80B cmd = fuse_uring_cmd_req{flags,commit_id=0,qid}; sqe.addr = &iov[2]{header,payload}, sqe.len=2.
  - On CQE: parse fuse_in_header from header.in_out, op_in = fixed op struct, op_payload[0..payload_sz] = variable data, save ent_in_out.commit_id.
  - COMMIT_AND_FETCH: write out_header into header.in_out, reply payload into op_payload, set payload_sz, cmd_op=2, 80B cmd carries commit_id; submit. Deferred submit during cqe processing = natural batch.
- io-uring crate v0.7.12 usable: `opcode::UringCmd80` builds an `Entry128` (SQE128) with fd (types::Fd, no fixed-file needed), cmd_op, 80B cmd. GAP: UringCmd80 does NOT set sqe.addr/len (needed for REGISTER). Fix: both Entry/Entry128 are #[repr(C)] over the stable-ABI kernel SQE, so view `&mut Entry128` as `&mut [u8;128]` and patch addr@16 (u64) + len@24 (u32). Offsets verified via offsetof on this kernel (sqe=64B, addr=16, len=24, user_data=32).
- Request reconstruction is opcode-agnostic: contiguous = in_out[0..40] ++ op_in[0..fixed] ++ op_payload[0..payload_sz], where fixed = fuse_in_header.len - 40 - payload_sz. Feed to existing AlignedRequestBuf::copy_from + Request::new.
- Integration approach: `Request` holds `ChannelSender` concretely (not generic ReplySender), so make `ChannelSender` an enum {Classic(Arc<File>), Uring(handle)}; only `send()` branches (writev vs COMMIT_AND_FETCH). Dispatch inline on each ring thread (matches libfuse per-core model). Notifications/interrupts stay on classic /dev/fuse path. Teardown via per-queue eventfd poll SQE.

### BLOCKER — io_uring requires an 11-version ABI uplift first
- Current build caps the vendored FUSE ABI at **7.31** (`fuse-modern` feature enables abi-7-19..abi-7-31). `FUSE_KERNEL_MINOR_VERSION` is per-abi-feature; highest enabled = 31, so the daemon negotiates 7.31.
- FUSE_OVER_IO_URING lives in init **flags2**, which is only emitted/read under `abi-7-36`. The code HAS `cfg(feature="abi-7-36")` / `abi-7-40` branches, but those features are **not defined** in Cargo.toml -> dead branches -> flags2 is never sent today.
- To negotiate uring: define + enable abi-7-32 .. abi-7-42, add FUSE_KERNEL_MINOR_VERSION constants for each, ensure every conditionally-compiled struct field (7.32–7.42) exists in the vendored abi, and confirm the dispatcher safely handles/ENOSYS-es opcodes 48+ (SETUPMAPPING/REMOVEMAPPING/SYNCFS/TMPFILE/STATX). Bumping the negotiated minor version changes kernel behavior broadly (new opcodes gated behind caps), so it must be verified independently (mutation harness + cli tests) before layering uring on top.

### Honest cost estimate (revised up from the spec's "1-day spike")
- (a) ABI 7.31 -> 7.42 uplift: contained but real; ~1 testable PR. Risk: ABI struct/layout mismatch = catastrophic mount corruption, so must be harness-verified.
- (b) io_uring transport: ~500-700 LOC unsafe (per-core ring threads, entry buffers, REGISTER, CQE parse, ChannelSender enum, COMMIT_AND_FETCH, eventfd teardown, INIT negotiation, Session wiring).
- Perf payoff UNMEASURABLE under current host load; a spike's GO/NO-GO needs a quiet host.
- Recommendation: do (a) as its own harness-verified commit first; then (b). Do not blind-bump ABI + add unsafe transport in one unverifiable step.

## 2026-05-29 — PB.1 DONE: vendored FUSE ABI uplift 7.31 -> 7.42 (harness-verified)
**Decision**: user chose "staged" — land the ABI uplift as its own verified commit before the io_uring transport.
**Changes**:
- cli/Cargo.toml: define abi-7-36/40/41/42 features; `fuse-modern` now enables abi-7-36, abi-7-41, abi-7-42. Deliberately NOT abi-7-40 (it pulls in unfinished FUSE_PASSTHROUGH scaffolding: FOPEN_PASSTHROUGH / BackingId / open_backing / max_stack_depth). abi-7-41=["abi-7-36"], abi-7-42=["abi-7-41"].
- fuse_abi.rs: version ladder advertises minor 42 on the 7.36 init layout (the 7.36 fuse_init_out is already a kernel-compatible 64 bytes — trailing reserved[7] = kernel max_stack_depth+request_timeout+unused, written as zero). Added FUSE_OVER_IO_URING (1<<41) + the 3 uring uapi structs (fuse_uring_ent_in_out / fuse_uring_req_header / fuse_uring_cmd_req) + cmd consts + header sizes, all gated abi-7-42 (for the upcoming transport).
**Safety basis**: AnyRequest::try_from validates only the fuse_in_header, not the opcode; unknown opcodes (kernel may now send 48+) surface as ENOSYS in operation(), not a session kill. New caps are only sent by the kernel when we request them (config.requested), which we don't for unsupported features.
**Verification**: `INIT response: ABI 7.42` confirmed against kernel ABI 7.45 (debug log); kernel caps 0x7ff73fffffb include bit 41 (FUSE_OVER_IO_URING). 161 SDK + 107 CLI tests, clippy, fmt all green; mutation harness 20/20 (base untouched, remount reproduces); git clone+reads+edits workload rc=0 over the mount.
**Next (PB.2)**: io-uring dep + ChannelSender enum + uring.rs transport + add FUSE_OVER_IO_URING to config.requested when AGENTFS_FUSE_TRANSPORT=uring.

## 2026-05-29 — PB.2/PB.3 DONE: working FUSE-over-io_uring transport (opt-in, correctness-verified)
**Result**: a functional FUSE-over-io_uring transport landed and verified end-to-end. Opt-in via `AGENTFS_FUSE_TRANSPORT=uring`; default stays classic /dev/fuse.
**Files**:
- cli/Cargo.toml: `io-uring = "0.7"`.
- cli/src/fuser/channel.rs: `ChannelSender` is now an enum {Classic{device}, Uring(UringReplySender)}; `notify_sender()` always yields a classic sender (notifications never traverse the ring); `UringReplySender::commit_reply` writes the out-header into the entry header buffer (offset 0), concatenates reply payload into the payload buffer, and stamps ring_ent_in_out.payload_sz (offset 272).
- cli/src/fuser/uring.rs (new, ~430 LOC): one CPU-pinned io_uring per core (nr_queues = _SC_NPROCESSORS_CONF, depth default 2 via AGENTFS_FUSE_URING_DEPTH). Per-entry page-aligned header buf (4096) + payload buf (= max_write). REGISTER via opcode::UringCmd80 (Entry128/SQE128) with addr/len byte-patched to the [header,payload] iovec (UringCmd80 leaves them zero); CQE -> reconstruct classic contiguous request (in_header ++ op_in[0..fixed] ++ payload, fixed = len-40-payload_sz) -> Request::new + dispatch inline -> COMMIT_AND_FETCH (commit_id in 80B cmd) re-arms the entry. Teardown: AtomicBool + submit_with_args timeout (200ms) so threads exit on UringRuntime drop.
- session.rs: SessionShared gains uring_negotiated/uring_max_write; `run_uring()` keeps the classic serial /dev/fuse loop (FORGET/interrupts/INIT stay there — libfuse's fuse_reply_none does not commit, confirming reply-less ops are not ring-delivered) and starts the per-core queues immediately after INIT negotiates the cap.
- request.rs: INIT adds FUSE_OVER_IO_URING to config.requested when requested AND kernel-advertised, caps max_write to 1 MiB, records negotiation.
**Verification (correctness only; perf deferred)**:
- Smoke mount: `FUSE-over-io_uring negotiated`, INIT reply flags 0x20001852021 (bit 41 set), `nr_queues=14 depth=2`, reads + ls served over the ring.
- Mutation harness 20/20 with AGENTFS_FUSE_TRANSPORT=uring (writes/mkdir/rename/symlink/unlink over the ring; base untouched; remount reproduces).
- git clone+reads+edits over uring: correctness_passed=True, agentfs_base_unchanged=True, integrity require-portable=True. performance_passed=False (expected: loaded host + un-tuned spike).
- Default classic path: mutation harness 20/20 (no regression from the ChannelSender enum). 161 SDK + 107 CLI tests, clippy, fmt green.
**Known spike limits / next**: depth=2 + inline per-queue dispatch (a slow op blocks its core's queue); no eventfd wakeup (200ms timeout poll on teardown only); payload cap 1 MiB (max_write reduced). PERF GO/NO-GO still requires a quiet host (P4): compare clone+reads+edits wall time uring vs classic and check fuse_dispatch_wait / connection counters.

## 2026-05-29 — CRITICAL: intermittent git-clone data corruption (pre-existing, NOT io_uring-specific)
**Discovered while doing the uring vs classic A/B.** `git clone` over the AgentFS mount intermittently fails with `error: inflate: data stream error` (corrupt object data). Findings:
- **Affects BOTH transports.** Classic (default, production) path also fails: classic A/B iter1 and a later classic run both hit the same inflate error with correctness.passed=False. So the io_uring transport is NOT the cause — it is at correctness parity with classic.
- **Load-dependent / highly intermittent.** Failure rate ~30-40% when the box is under heavy load (many benchmarks back-to-back, the pinned/depth sweeps); 0/4 in a clean batch afterwards. base_tree.unchanged stays True (the read-only base copy is never corrupted) — only the in-overlay clone output is corrupted.
- **Localization signal (not conclusive):** a 5x batch with `AGENTFS_OVERLAY_READS=0` (Tier 3 drain-on-write, bypassing the Tier 4 consistent-without-drain overlay) passed 5/5, while overlay-ON batches failed under load. The corruption is file DATA (inflate), so the suspect is the Tier 4 `pread` overlay-splice path (peek_pending / merge of batched writes), not the metadata size merge. BUT the bug is intermittent enough that 5/5 could be partly luck; needs a controlled high-load repro (e.g., `stress-ng` + N serial clones with a pass/fail counter) to confirm overlay reads as the sole cause.
- **Provenance:** the Tier 4 overlay shipped on this branch in a prior session (default ON). This session's changes (P2 readdirplus, P3 deferred-release, PA connection fast paths) touch metadata/lookup/getattr, not the data `pread` overlay, so they are unlikely to be the cause — but this was not bisected. The ABI uplift and io_uring transport are also unrelated (corruption predates them on the classic path).

**Severity:** HIGH — default-on, affects the production classic read path, silent data corruption under load.
**Recommended next step (higher priority than perf):** build a deterministic high-load corruption harness (background CPU/IO stress + repeated clone, count failures), confirm overlay_reads ON vs OFF failure rates, then audit the Tier 4 `pread`/`peek_pending`/`truncate_pending` overlay logic in sdk/rust/src/filesystem/agentfs.rs for a read-vs-enqueue/drain race (the parking_lot::RwLock peek window vs concurrent enqueue/commit). Kill switch for production in the meantime: `AGENTFS_OVERLAY_READS=0`.

## io_uring spike — final status
PB.1 (ABI 7.42 uplift) and PB.2/PB.3 (io_uring transport) are committed locally (not pushed). The transport is functional and at correctness parity with classic; PERF GO/NO-GO still requires a quiet host AND resolution of the corruption bug (so clone runs are reliable enough to time). Recommend resolving the corruption bug before any perf A/B.

## 2026-05-29 — FIXED: Tier 4 overlay-read data corruption (commit-then-remove drain)
**RCA (confirmed independently by a heavy worker subagent — same conclusion):** every drain removed pending ranges from the in-memory overlay BEFORE the SQLite txn committed. `drain_pending_batched` did `std::mem::take(&mut state.pending)` then opened/committed the txn; `drain_inode` (Bytes) did `take_inode_locked` then `commit_batch`. `AgentFSFile::pread` peeks the overlay then reads SQLite with no lock spanning the two, so a read landing in the take→commit gap found the write in NEITHER place and returned stale bytes → `inflate: data stream error`. Load-dependent because the BEGIN IMMEDIATE + chunk-write + WAL-fsync window lengthens under load and the 8-slot pool saturates. `AGENTFS_OVERLAY_READS=0` avoided it because that path never populates the overlay (pwrite commits directly; pread drains first) so the gap can't exist.
**Fix:** commit-then-remove. Both drains now SNAPSHOT pending ranges by cloning WITHOUT removing them, commit the snapshot to SQLite, and only after `txn.commit()` drop exactly the committed ranges (`remove_committed_prefix`: removes the first N front ranges per inode — enqueue is append-only — with `.min(len)` to tolerate concurrent truncate/discard, reschedules a timer if ranges remain). Invariant restored: a write is always visible in the overlay OR in committed SQLite, never neither. On commit error the overlay is left intact (retried next drain), so `restore_batch`/`restore_batches`/`take_inode_locked`/`commit_batch` are gone (dead).
**Cost:** a clone of pending bytes per drain (transient 2x peak memory during the txn). Negligible vs the SQLite chunk writes + WAL fsync the drain already does; read hot path is UNCHANGED.
**Verification (overlay reads ON = default):** post-fix 0/20 clone runs failed under heavy CPU stress (12x `yes`), vs ~25-40% pre-fix; classic 0/(8+6), uring 0/6. Mutation harness 20/20 on BOTH classic and uring. 161 SDK tests, clippy, fmt green. The bug affected both transports (it was in the shared SDK), so the io_uring transport is unaffected by this fix beyond inheriting the correctness.

## 2026-05-29 — io_uring perf GO/NO-GO: **NO-GO** (transport is not the bottleneck)
Ran on an idle host (load ~3/14 cores), release binary at HEAD, canonical codex fixture, --read-files 64 --read-bytes 4096 --edit-files 8, 8 iters/mode (warmup dropped), alternating classic/uring.

**Result (lower is better; uring vs classic, agentfs total workload seconds):**
- uring default (1 MiB max_write, depth 2): median **+8.7%** slower, min +2.4%, clone +9.0%.
- uring tuned (16 MiB max_write to match classic, depth 4): median **+17.5%** slower, min +8.2%, clone +22.3% — WORSE, because 16 MiB × 14 queues × 4 = 896 MiB of page-aligned buffers allocated/zeroed at mount adds latency + memory pressure.

**Why (profile, classic clone phase, ~3 s wall):** `agentfs_batcher_commit_latency_ns_total ≈ 841 ms` (SQLite BEGIN IMMEDIATE + chunk writes + WAL fsync across 4692 explicit drains) and `fuse_dispatch_wait_nanos ≈ 341 ms` (worker queue) dominate; `connection_wait_nanos ≈ 18 ms` (tiny after the PA fast-path fix). The FUSE transport syscalls (read/writev on /dev/fuse) are not even a measurable cost. io_uring optimizes the transport, which is NOT the bottleneck — so it cannot win, and its overhead (one ring thread per core spinning vs the tuned 7-worker lane-scheduled pool, plus large buffer allocs) makes it net slower.

**Verdict:** NO-GO for the io_uring transport on this SQLite-backed workload. To move the needle, target the actual cost centers: batcher commit latency (fewer/larger transactions, WAL tuning, group-commit) and dispatch wait. Keep the io_uring transport opt-in/off-by-default as a documented dead-end experiment (it already paid for itself by surfacing the Tier 4 corruption bug). The ABI 7.42 uplift is independently valuable and stays regardless.

## 2026-05-30 — Drain-source RCA + forget no-drain (shipped); setattr-deferral findings (WIP, not shipped)
**RCA of the 4,692 per-file SQLite commits during clone:** per-call-site tracing + FUSE op counts show the kernel's writeback **SETATTR (mtime) per written file → `utimens` prelude `drain_inode_writes`** is the real committer (4,736 setattrs/clone); the **forget-time drains** (5,418 FORGETs, in both the fuse handler and the SDK `AgentFS::forget` override) were a redundant second pass; flush/release/fstat/fsync are not the source.
**Shipped — forget no-drain:** fuse forget/batch_forget drains gated behind `AGENTFS_DRAIN_ON_FORGET` (default off) and the SDK forget override removed. A FORGET only drops the kernel ref; pending stays readable via the Tier-4 overlay and commits via timer/bytes/fsync/finalize. Verified: clone **-9.0%** median / total -4.2% vs legacy (alternating A/B, warmup dropped), mutation 20/20, 0/8 clone failures under 12x CPU stress, 161 SDK + 107 CLI tests, both overlay modes green.
**Not shipped — setattr-deferral (utimens/chmod/chown no-drain) findings, for the next attempt:**
- Mechanism works: `times_explicit` mark + `preserve_times` commit (flag read after BEGIN IMMEDIATE) keeps explicit setattr times from being clobbered by deferred data commits; unit tests written. Explicit drains collapse 4,692→3, dispatch wait 341→162 ms, connection wait 18→6.6 ms.
- BUT (1) commits just move to ~per-inode timer txns (drains_timer ≈ 4,681 inode-commits; commit latency only 841→735 ms) plus a new per-file IMMEDIATE txn for the time UPDATE → no net win, higher variance. Needs true group-commit shaping (longer/coalesced batch window; commit times together with data in the same txn) to pay off.
- (2) turso reports autocommit-vs-txn write races as "database snapshot is stale" → EIO. Wrapping chmod/chown/utimens in BEGIN IMMEDIATE fixed those, but other autocommit metadata writers then surfaced (intermittent `unlink config.lock: EIO` 1/8 runs). A uniform discipline (txn-wrap or stale-snapshot retry at the connection layer) is prerequisite.
- (3) `AGENTFS_OVERLAY_READS=0` needs the legacy drain kept (no pending-size merge there → stale st_size breaks git config reads).
- Full WIP diff saved at `/tmp/wip_setattr_deferral.patch` (938 lines: preserve_times plumbing, txn-wrapped attr ops, NotFound-tolerant batched drain, AGENTFS_DRAIN_ON_SETATTR kill switch, error_to_errno debug tracing, 2 unit tests).

## 2026-05-30 — Prior-art research: SQLite/turso group commit + FUSE small-file writes
**Scope**: primary-source web research against the observed clone shape (~4,700 file drains/transactions, WAL + `synchronous=NORMAL`); no implementation change.

### 1. SQLite many-small-transactions → group commit
- SQLite's FAQ states the central result plainly: an average desktop can do “50,000 or more INSERT statements per second”, but only “a few dozen transactions per second”, because transaction completion is storage-wait bound; its remedy is many inserts inside one transaction. Source: https://sqlite.org/faq.html
- WAL improves read/write overlap and sequentializes writes, but does not make thousands of transaction boundaries free. SQLite documents the default 1000-page auto-checkpoint and that the commit crossing the threshold may run checkpoint work; WAL `synchronous=NORMAL` changes sync placement, not per-transaction engine/lock/frame cost. Sources: https://sqlite.org/wal.html and https://sqlite.org/pragma.html
- SQLite's favorable small-blob benchmark (10K blobs, average 10KB) measures WAL + `synchronous=NORMAL` writes through transaction commit but before checkpoint; it supports storing small files in SQLite, not committing every POSIX close separately. Source: https://sqlite.org/fasterthanfs.html
- `BEGIN CONCURRENT` and `wal2` are branch features rather than ordinary WAL tuning: `BEGIN CONCURRENT` may fail COMMIT with `SQLITE_BUSY_SNAPSHOT` on page conflict and still serializes commits. They improve independent writers, not N-per-file commit amplification. Sources: https://sqlite.org/src/doc/begin-concurrent/doc/begin_concurrent.md and https://sqlite.org/src/doc/wal2/doc/wal2.md
- `page_size`/`cache_size` and prepared-statement reuse (`sqlite3_reset()` prepares a statement to execute again) can reduce secondary page/cache/compile overhead; none reduces commit count. Sources: https://sqlite.org/pragma.html and https://sqlite.org/c3ref/reset.html
- SQLite publishes no portable microsecond transaction floor: VFS/device/durability decide it. The realistic prediction for AgentFS is that an N-into-1 transaction avoids nearly N-1 commit boundaries, directly targeting its measured ~700–840ms commit aggregate.

### 2. SQLite-backed filesystem/archive prior art
- `libsqlfs`/`sqlfs` implements a POSIX-style filesystem in one SQLite/SQLCipher file and exposes FUSE, but its published repository/project page provide no reproducible git-clone or many-small-write performance numbers. Sources: https://github.com/guardianproject/libsqlfs and https://guardianproject.info/archive/libsqlfs/
- SQLAR stores an archive as SQLite rows/BLOBs (optionally compressed): useful precedent for packing a tree in one transactional file, but not a mutable FUSE/POSIX writeback implementation. Source: https://sqlite.org/sqlar.html
- AgentFS's public FUSE article confirms this same one-database/FUSE architecture but publishes no comparable small-file transaction benchmark. Source: https://turso.tech/blog/agentfs-fuse
- Thus the closest directly measured primary prior art found is SQLite's packed small-BLOB benchmark, not a FUSE FS benchmark; it points to fewer/larger DB transactions rather than a cheap-close FUSE flag.

### 3. Turso (formerly Limbo) concurrency implications
- Turso's manual documents SQLite-style modes: `BEGIN IMMEDIATE` attempts the write lock at `BEGIN`, and `EXCLUSIVE` is its alias in WAL mode. One serialized group-drain writer is therefore the documented low-conflict shape for today's AgentFS path. Source: https://github.com/tursodatabase/turso/blob/main/docs/manual.md
- Turso v0.5.0 announces concurrent writes as **beta**, using MVCC; its design post describes a `BEGIN CONCURRENT` mode. AgentFS cannot assume its embedded version/config enables this without a capability/correctness test. Sources: https://turso.tech/blog/turso-0.5.0 and https://turso.tech/blog/beyond-the-single-writer-limitation-with-tursos-concurrent-writes
- Indexed public documentation did not surface the exact Limbo error text “database snapshot is stale, rollback and retry” or group-commit guidance. AgentFS's observed autocommit-vs-transaction failure is nevertheless consistent with conflicting snapshots: eliminate competing metadata writers first; evaluate MVCC/retry later.
- A Turso `BEGIN CONCURRENT` request is provenance, not evidence that AgentFS's embedded build supports it. Source: https://github.com/tursodatabase/turso/issues/86

### 4. FUSE writeback-cache and close-time metadata
- Kernel docs say writeback-cache lets `write(2)` finish into cache; dirty pages are later written in background or explicitly on `close(2)`, `fsync(2)`, and last-reference release. It explains rather than eliminates git clone close/writeback traffic. Source: https://docs.kernel.org/filesystems/fuse/fuse-io.html
- libfuse maintainers say writeback userspace is not initially authoritative for size/mtime and should eventually receive `setattr`; a trace reports kernel mtime/atime/ctime `setattr` after writeback. Sources: https://github.com/libfuse/libfuse/discussions/868 and https://github.com/libfuse/libfuse/issues/342
- `FUSE_HANDLE_KILLPRIV_V2` governs setuid/setgid clearing on write/truncate, not mtime coalescing; attribute timeouts cache read answers, not required metadata persistence. Source: https://libfuse.github.io/doxygen/include_2fuse__common_8h.html
- No libfuse flag found safely omits written-file SETATTR; the lever is internal staging/group commit while retaining genuine `fsync` and finalize barriers.

### 5. Compatible practitioner pattern
- SQLAR plus SQLite's BLOB benchmark validate packing content/tree records inside the DB file. An in-memory per-inode overlay with bounded cross-inode transactions fits AgentFS's single-file/no-host-writes rule; a durable queue-table insert on every close only recreates per-close transactions unless it is itself group-committed.

### Known dead ends (from the field)
- FUSE-over-io_uring already measured NO-GO here: it optimizes transport rather than DB transaction shape.
- Deferring SETATTR/FORGET while per-inode timers still commit merely moves transactions and exposes Turso snapshot conflicts.
- `BEGIN CONCURRENT`/WAL2 first: conflict/retry plus serialized COMMIT does not coalesce ~4,700 logical transactions.
- `FUSE_HANDLE_KILLPRIV_V2`, attr timeouts, or writeback-cache toggles do not remove writeback metadata lifecycle.
- A per-close durable queue row in the same DB remains a per-close transaction unless enqueue is grouped.

### Ranked next experiments for AgentFS
1. **Cross-inode group drain in one `BEGIN IMMEDIATE`**: drain eligible data and staged metadata in one bounded global timer/byte-triggered transaction; make `fsync` drain its barrier and finalize drain all. Highest expected payoff: reduce thousands of boundaries to O(windows), targeting the measured 700–840ms and resultant dispatch wait without host writes.
2. **Stage writeback SETATTR into that group drain**: preserve overlay-visible size/times, remove its standalone/autocommit write, and use one-writer discipline. Expected payoff: prevent the per-inode timer-transaction replacement and the observed stale-snapshot races.
3. **Then sweep WAL/cache/checkpoint knobs with barrier verification**: compare `wal_autocheckpoint` thresholds/manual-finalize checkpoint and bounded `cache_size`/page metrics; explicitly assert `fsync` durability. Expected modest payoff: avoid checkpoint burst spikes, not fix amplification alone.
4. **Only if SQL/page work remains costly, prototype packed/chunked content rows**: SQLAR/BLOB precedent fits one-file storage and may reduce page churn, but it is a larger schema/read-path change than correcting transaction shape.

## 2026-05-30 — Experiment 1+2: cross-inode group commit (results)
**Code (uncommitted, on top of the setattr-deferral WIP):**
- New profiling counters `agentfs_batcher_commit_txns` / `_txn_inodes_total` / `_txn_inodes_max` count actual batcher `BEGIN IMMEDIATE`/`COMMIT` pairs (the old `drains_*` are per-inode ticks, not txns).
- Drain reshape: the per-inode timer storm is gone. One coalescing scheduler task is armed by the first pending write, sleeps `AGENTFS_BATCH_MS` (default 5 ms), then commits everything pending in bounded back-to-back txns (`AGENTFS_BATCH_TXN_INODES`=1024, `AGENTFS_BATCH_TXN_BYTES`=32 MiB), exits when nothing is pending. Bytes triggers, explicit drains (fsync/kill switches), finalize-on-unmount, commit-then-remove and times_explicit/preserve-times ordering all preserved.
- SETATTR staging hardened: `stash_pending_times` now CREATES the pending entry when the inode has nothing pending, so a writeback SETATTR never pays a dedicated foreground txn (the old fallback per-file IMMEDIATE time-UPDATE was still firing for most files and was a major hidden cost); the stash is committed by the next group txn and overlaid by `merge_pending_view` until then.

**Ground truth (clone phase, codex fixture, deferred default env):** WIP-as-found = 356 txns (4,682 inode-commits, max 224/txn, 748 ms commit total). Reshaped = 222–268 txns, max 131–255 inodes/txn. Legacy (DRAIN_ON_SETATTR=1, FORGET=1) = 4,698 txns of 1 inode, 402–589 ms commit, 563 ms dispatch wait. Reshaped deferred profile: commit 606 ms, dispatch wait 210 ms, connection wait 5.4 ms, drains_explicit 3.

**A/B (8 alternating iters/mode after warmup, --read-files 64 --read-bytes 4096 --edit-files 8, all runs correctness+fsck clean):**
- legacy:   total median 3.936 s (min 3.627), clone median 2.569 s (min 2.253)
- deferred: total median 4.314 s (min 4.050), clone median 2.380 s (min 2.176)
- delta: clone median **-7.4 %** (deferred wins; -3.4 % on min), total median **+9.6 %** (deferred loses; +11.7 % on min).
- Per-phase: the entire loss is post-clone — checkout +157 % (0.235→0.606 s), status +128 % (0.142→0.324 s); diff/read/edit/fsck flat or better.
- RCA of the post-clone loss: deferred commits (data + staged times) land AFTER git has written its index, so the FUSE adapter's deferred inode invalidations (~4.7k during checkout vs ~0.7k legacy) blow the kernel attr cache and the FS-served times no longer match what git recorded → `git checkout -B` re-reads ~4,700 files serially (fuse_open_count 4,701 vs 650 legacy) and status re-stats everything. Legacy avoids it because its per-setattr drains finish before the index write. Follow-up experiment: make deferred commits attribute-transparent (stashed kernel times always win; suppress inval notifications when a commit changes no kernel-visible attr).

**fsck anomaly (GATE):** the historical 1/29 `git fsck --strict` failure did NOT reproduce post-reshape: 32/32 deferred-mode benchmark runs (16 idle + 16 under 12× `yes` CPU stress) passed fsck --strict, `git status`, base-tree-unchanged and full correctness; in total 58 deferred-mode runs this session were fsck-clean. No artifacts to preserve.

**Validation gates (all green):** SDK fmt/clippy --lib 0 warnings/165 tests single-threaded; CLI fmt/clippy --release 0 warnings/107 tests/release build; metadata-mutation-no-real-write 20/20 passed; overlay-OFF clone (AGENTFS_OVERLAY_READS=0) correctness true; AGENTFS_DRAIN_ON_RELEASE=1 clone correctness true; high-load 8/8 default env and 8/8 legacy env (12× yes, timeout 75 s) all rc=0 + base unchanged.

**Verdict:** transaction-shape goal met (4,698 → ~220–270 clone txns, low hundreds) and the fsck anomaly is cleared, but **NO-GO for default-on**: deferred wins the clone (≥5 % target met) yet loses the workload total (+9.6 %) to the post-clone kernel-cache/index re-read storm. Keep the work as an unshipped WIP (kill switches intact); next lever is attribute-transparent deferred commits + invalidation suppression, then re-run this A/B.
