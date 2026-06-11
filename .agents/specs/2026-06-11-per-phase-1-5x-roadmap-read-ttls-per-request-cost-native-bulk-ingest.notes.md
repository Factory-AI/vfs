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
