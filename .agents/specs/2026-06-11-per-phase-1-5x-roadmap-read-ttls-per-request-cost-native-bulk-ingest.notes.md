# Implementation Notes — 2026-06-11-per-phase-1-5x-roadmap-read-ttls-per-request-cost-native-bulk-ingest

Spec: 2026-06-11-per-phase-1-5x-roadmap-read-ttls-per-request-cost-native-bulk-ingest.md
Approved: 2026-06-11
User comment: none

---

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

## 2026-06-11T11:05-07:00 — WS2 closed early: create-deferral and ~15µs/req target deferred behind WS3
**Type**: deviation
**Context**: Spec planned "fix top-3 measured offenders" toward ~15µs/req. Measurement shows: create quick wins landed (145→125µs; txn boundary ~115µs is the floor), and clone-phase sync dispatch totals only ~1.07s of the 2.84s clone overhead — the rest is queue wait, kernel round trips, and SQLite write-lock contention. Zeroing ALL sync dispatch still leaves FUSE clone ~5x.
**Resolution**: Full create-deferral (pending namespace: pending creates must survive the tmp→rename git object flow) is high-complexity for at most ~0.5s of critical path, while WS3's `agentfs clone` bypasses per-file FUSE costs entirely and is the only route to clone ≤1.5x. WS2 banked as: per-op instrumentation + create fast path + critical-path model. Read-path per-request work (read 83µs/op) resumes after WS3 against the read-benchmark ≤1.5x target.
