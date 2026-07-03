# Implementation Notes — 2026-06-11-per-phase-1-5x-roadmap-read-ttls-per-request-cost-native-bulk-ingest

Spec: 2026-06-11-per-phase-1-5x-roadmap-read-ttls-per-request-cost-native-bulk-ingest.md
Approved: 2026-06-11
User comment: none

---

## 2026-06-11T14:30-07:00 — FUSE-over-io_uring: protocol notes and integration decisions
**Type**: decision
**Context**: Kernel 7.0 (CONFIG_FUSE_IO_URING=y, `fuse.enable_uring` flipped by user). Protocol learned from fs/fuse/dev_uring.c + libfuse lib/fuse_uring.c: REGISTER parks header(≥288B)+payload(≥max(8K,max_write,max_pages*4K)) iovecs per ring entry; a request completes the CQE with fuse_in_header + first-arg in the split header buffer and remaining args in payload; reply = write fuse_out_header + payload + payload_sz, then COMMIT_AND_FETCH with commit_id (= request unique). FORGET/INTERRUPT/notify stay on the legacy fd channel; REGISTER EAGAINs until the kernel processes our INIT reply; on REGISTER failure the kernel clears fc->io_uring and recovers to legacy itself.
**Resolution**: Raw syscall implementation (io_uring_setup/enter + ring mmaps, no new crate — needed exact SQE128 byte control). Inline dispatch on per-CPU queue threads keeps each SQ single-threaded; requests are reassembled into the classic contiguous layout so the existing parser/dispatcher/reply stack is reused unchanged; ChannelSender became Fd|Uring. max_write clamped to 1MiB in uring mode to bound ring memory (kernel caps WRITEs at 256 pages regardless). Header buffers oversized to 1KiB because the kernel copies the first request arg into the 128B op_in area without bounds checks; >128B first args are rejected with EIO defensively (255-char-name lookups verified working empirically). Probe-before-advertise in INIT avoids stalling the mount when ring setup would fail.

## 2026-06-11T14:40-07:00 — io_uring eval: 25-40% on RT-bound shapes, not the 2x promise; stays opt-in
**Type**: surprise
**Context**: Hypothesis was that removing the read/writev syscall ping-pong halves per-round-trip cost. Measured (loaded host): phase8 repeated-read 3.00x→1.81x, base-read steady-state 7.34x→4.86x (−34%), read-path paired 0.911, git workload parity (clone SQLite-bound; checkout −70%). All correctness gates green; serialization gate fixed by adding uring-side dispatch-concurrency accounting (counter artifact). The residual per-request cost moved from syscall+wakeup into kernel task-work + queue-thread wakeup; a CQ busy-poll knob (AGENTFS_FUSE_URING_SPIN_US) was inconclusive under host load.
**Resolution**: Ships opt-in (AGENTFS_FUSE_URING=1; also requires root for fuse.enable_uring). Promotion to default needs an idle-host A/B reproducing the repeated-read win at total-workload parity. The 1.81x repeated-read is the closest any lever has gotten to the 1.5x micro target; combining uring with ENOSYS-FLUSH (lever #2, removes one of the two RTs per open/close cycle) is the most promising compound next step.

## 2026-06-11T13:00-07:00 — Sticky drift-guard drop relaxed; keep-cache extended to DB-backed files
**Type**: decision
**Context**: Upper/Delta (DB-backed) files never qualified for `FOPEN_KEEP_CACHE` (`Layer::Base`-only), and the drift guard's sticky `dropped` set meant any file ever written through the mount (i.e. every git-created file) lost eligibility for the life of the mount. Walked the state machine for both relaxations: kernel-originated writes keep the kernel's own pages coherent; adapter-notified invalidations purge pages before any re-grant; out-of-band SDK writers change mtime/ctime/size and fail the per-open fingerprint check — the same risk model the base layer always had for external host-file edits (content swap + timestamp restore defeats both, accepted).
**Resolution**: Drop now just removes the stored fingerprint (re-grant revalidates); `AGENTFS_FUSE_STICKY_KEEPCACHE_DROP=1` restores old behaviour. `AgentFS::keep_cache_for_read_open` grants for regular files (`AGENTFS_KEEPCACHE_DELTA=0` kill switch); overlay delegates Delta inodes. Git workload: grants 20→1,694, READs −80%, paired wall 0.906, status 0.71x / diff sub-native. The overlay unit test asserting "delta must not keep" was updated to the new contract (eligible + fingerprint must move across copy-up).

## 2026-06-11T13:05-07:00 — Remaining gap is the OPEN+FLUSH pair; passthrough deprioritized; radical options listed
**Type**: followup
**Context**: After WS4+WS5, the >1.5x stragglers (read_search 2.25x, read-path micro 3.35x) are bound by two synchronous FUSE round trips per open/close cycle, not by data movement or handler time. FUSE passthrough only accelerates read/write data and warm READs are already eliminated.
**Resolution**: Logged for brainstorm: (1) FUSE-over-io_uring (kernel 6.14+; host runs 7.0) — cuts per-round-trip cost rather than round-trip count; (2) ENOSYS-on-FLUSH — removes one RT per close connection-wide, needs a pending-buffer guarantee on the getattr path to close the stat-after-close window; (3) open-by-handle batching is not a FUSE concept; nothing in the protocol elides OPEN.

## 2026-06-11T12:30-07:00 — Read-path 12.7x root cause: FLUSH on read-only fds permanently revoked keep-cache
**Type**: surprise
**Context**: Counters on the read profile showed `base_fast_open_keep_cache=64` vs `base_fast_open_rejected=1216` with `base_fast_inode_invalidations=1280` — one invalidation per close. Stepping the state machine: every close(2) sends FLUSH; the handler called `invalidate_inode_cache_self` unconditionally, which feeds the drift guard's STICKY `dropped` set, so the first close of a file revoked `FOPEN_KEEP_CACHE` eligibility forever. Each re-open of an unchanged base file then re-read everything through FUSE. The kernel page cache was being destroyed by the very flag machinery built to preserve it.
**Resolution**: FLUSH now invalidates only when it actually drained buffered writes (`drain.is_some()`); a no-write FLUSH is not a mutation (MutationAudit gets an explicit `discard_no_mutation`). Kill switch `AGENTFS_FUSE_FLUSH_INVAL=1`. After: 1,280/1,280 opens keep-cache, READs 1,280→64 (one cold read per file), stale rejections 0. 8/8 A/B pairs win, paired wall median 0.744.

## 2026-06-11T12:35-07:00 — FOPEN_CACHE_DIR requires giving back the OPENDIR round trip
**Type**: decision
**Context**: readdirplus dominated handler time (482 calls × 30.6µs) because the kernel re-fetched directory contents on every scandir. Granting `FOPEN_CACHE_DIR|FOPEN_KEEP_CACHE` lets warm getdents hit the page cache, but the mount advertised `FUSE_NO_OPENDIR_SUPPORT`, so the kernel never sent OPENDIR and there was no reply to carry the flag.
**Resolution**: `FUSE_NO_OPENDIR_SUPPORT` is now advertised only when dir caching is off (`AGENTFS_FUSE_CACHE_DIR=0`). Trade: one OPENDIR+RELEASEDIR round trip per opendir(3) (handler ~1.5µs) buys cached getdents for every warm re-listing — readdirplus 482→24 on the read profile, 2,858→1,425 on the git workload. Coherency: mount-local mutations notify the parent inode (kernel drops dir pages); cross-mount divergence is TTL-bounded like attrs.

## 2026-06-11T12:40-07:00 — Read-path verdict: 4.0x, floor is the OPEN+FLUSH round-trip pair; next levers logged
**Type**: deviation
**Context**: Target was ≤1.5x. With READs and readdirplus mostly eliminated, each warm open/read/close cycle still pays two synchronous FUSE round trips (OPEN ~11µs handler + FLUSH ~1.6µs handler, ~60µs wall vs native ~14µs). FOPEN_NOFLUSH is ignored by the kernel under writeback cache (re-confirmed reasoning from the earlier spike), and connection-wide ENOSYS-on-FLUSH was evaluated and rejected for now: the per-fh write buffer tail would only land at async RELEASE, opening a stat-after-close staleness window.
**Resolution**: 4.0x recorded honestly (3.2x better than the 12.7x start). Logged next levers, in order of expected value: (1) extend `keep_cache_for_read_open` beyond `Layer::Base` to upper/DB-backed files — requires relaxing the drift guard's sticky drop to fingerprint-based revalidation, since files created through the mount (git clone) currently lose eligibility permanently at first write; (2) FUSE passthrough for read fds (infrastructure counters already exist); (3) ENOSYS-on-FLUSH revisited only with a getattr-side pending-flush guarantee.

## 2026-06-11T10:25-07:00 — WS1 TTL hypothesis falsified by counters; warm-read target moves to WS2
**Type**: surprise
**Context**: The spec predicted raising entry/attr TTLs 1s→10s would fix the read-path warm steady-state (12.7x). Counter measurement shows request counts are IDENTICAL across TTL settings in the read benchmark (getattr 235, open 256, readdirplus 482, cold AND warm): the kernel already caches within iteration loops at 1s, and "warm" remounts, so every object pays exactly one round trip per mount regardless of TTL. Steady-state cost is ~1,229 requests x ~98us = per-request cost, not TTL expiry.
**Resolution**: TTL 10s kept anyway: on the git workload it cuts lookups −32% (18.2k→12.3k, stable across pairs) with getattrs partially replacing them (+2.6k revalidation), net dispatches −4-9%. Read-path steady-state ≤1.5x acceptance moves from WS1 to WS2 (per-request cost). WS1 wall-time A/B descoped: a −4-9% request delta is below host noise floor; verdict rests on deterministic counters + correctness gates instead.

## 2026-06-11T10:15-07:00 — Cross-mount staleness narrower than spec assumed
**Type**: surprise
**Context**: WS1 sanity check: `agentfs run --session <id>` from a second terminal prints "Joining existing session" and attaches to the SAME mount rather than creating a second FUSE mount; create-visibility measured <=1s and modify-visibility immediate in the joined flow.
**Resolution**: TTL staleness exposure applies only to genuinely separate mounts of the same DB (rare/manual). Both sanity directions pass within bounds; 10s positive TTL is safe for the supported flows.

## 2026-06-11T10:20-07:00 — phase8 perf thresholds are stale (pre-existing, not WS1)
**Type**: followup
**Context**: phase8-validation perf-threshold gate fails (clone 164x vs thr 5.0, etc.) on its tiny synthetic fixture where native phases are sub-ms; last night's pre-WS1 run failed the same set worse (clone 413x). Correctness/durability/stress gates all pass.
**Resolution**: Treated as pre-existing flake/stale baseline. Followup: re-baseline phase8 perf thresholds on an idle host or switch that gate to the codex fixture; not blocking WS1.

## 2026-06-11T10:50-07:00 — WS2: dispatch-time ranking != critical-path ranking; deferred SETATTR stays opt-in
**Type**: decision
**Context**: New per-op dispatch latency counters (fuse_op_*_nanos) rank setattr #1 (857ms, 180us x 4.8k) and create #2 (680ms, 145us x 4.7k) on the codex workload. But a fresh deferred-vs-legacy A/B stacked on suppression+TTL10 is AGAIN parity (paired median 1.008): kernel writeback issues SETATTR asynchronously, so its cost never blocks git. Dispatch totals overstate ops that run off the critical path (setattr, release, most writes).
**Resolution**: Deferred SETATTR remains opt-in permanently (two parity A/Bs). WS2 pivots to the synchronous, git-visible ops: create 680ms (open(O_CREAT) blocks), read 195ms, lookup 139ms, open 122ms, getattr 114ms, flush 77ms (~1.4s total). Create plan: quick wins first (drop pre-check SELECT in favor of dentry UNIQUE-constraint mapping; stash parent mtime/ctime into the batcher overlay instead of an in-txn UPDATE), then reassess whether full create-deferral (pending namespace) is still required.

## 2026-06-11T11:25-07:00 — WS3 pipeline: fabricated index instead of archive+refresh
**Type**: deviation
**Context**: Spec planned `git archive | import` then `git reset --mixed` + `git update-index --refresh` to produce a clean index. Walking that flow: `update-index --refresh` lstat()s every worktree file through FUSE AND re-reads content to confirm shas (entries are racy vs a just-written index), i.e. it reintroduces ~2x per-file FUSE round trips that the bulk import just avoided. `git archive` also serializes to tar only for us to deserialize.
**Resolution**: Replaced with `ls-tree -r -z` (modes+shas+paths) + `cat-file --batch` (blob bytes, writer thread to avoid pipe deadlock) + `import_entries`, then fabricate the index v2 directly: cached stat fields (ino/dev/uid/gid/size/mtime/ctime) copied from what the import created, sha/mode from ls-tree. First `git status` is clean with zero per-file FUSE traffic, and it stays clean across FRESH mounts because ino and times live in the DB. Verified empirically (status clean + fsck --strict + sha256 equality vs native, 5/5 iterations).

## 2026-06-11T11:30-07:00 — WS3 result 2.34x vs ≤1.5x target; residual is the content double write
**Type**: surprise
**Context**: Expected ~0.3s import to dominate. Stage budget on codex (0.85s total vs native 0.374s): git-clone-no-checkout 330ms + import 288ms are co-dominant; both are 42.8MB content writes into the DB (pack, then worktree). cat-file 104ms, mount+process ~85ms, ls-tree 37ms, index 6ms.
**Resolution**: 2.34x recorded honestly in the scoreboard (53% better than the plain-FUSE floor ~5x, 3.6x better than measured 8.41x). Future shaves if the target is revisited: pipeline cat-file into import (saves ≤100ms), larger import transactions, pack reuse via `--reference`/local hardlink semantics (not allowed by the no-host-writes invariant for the pack itself). gitoxide fallback not needed: git orchestration costs only ~40ms beyond unavoidable content IO.

## 2026-06-11T11:05-07:00 — WS2 closed early: create-deferral and ~15µs/req target deferred behind WS3
**Type**: deviation
**Context**: Spec planned "fix top-3 measured offenders" toward ~15µs/req. Measurement shows: create quick wins landed (145→125µs; txn boundary ~115µs is the floor), and clone-phase sync dispatch totals only ~1.07s of the 2.84s clone overhead — the rest is queue wait, kernel round trips, and SQLite write-lock contention. Zeroing ALL sync dispatch still leaves FUSE clone ~5x.
**Resolution**: Full create-deferral (pending namespace: pending creates must survive the tmp→rename git object flow) is high-complexity for at most ~0.5s of critical path, while WS3's `agentfs clone` bypasses per-file FUSE costs entirely and is the only route to clone ≤1.5x. WS2 banked as: per-op instrumentation + create fast path + critical-path model. Read-path per-request work (read 83µs/op) resumes after WS3 against the read-benchmark ≤1.5x target.
