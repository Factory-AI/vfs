# Final Mission Attestation — VFS Right-Thing Restructure

Machine-readable definition-of-done record tying the full honest gate and the
codex benchmark contract to the exact commit (VAL-GATE-012, VAL-CROSS-012).

## Attested commit

- Commit: `230c30a6f2e7184b0578eb2dec087b79c0f78078` (`dev`)
- Binary: `target/release/agentfs` built from that commit by `scripts/gate.sh`
  (release workspace build; `rust-toolchain.toml` nightly)
- Runner: `Linux 7.1.2-3-cachyos x86_64 GNU/Linux`; toolchain
  `cargo 1.97.0-nightly (4f9b52075 2026-05-01)`
- Attestation artifacts: `full-gate.json` (gate + shell suite + phase8 + canon +
  pjdfstest + cross-surface flows), `medians.json` (benchmark contract),
  `bench-milestones.json` (per-milestone BENCH record, VAL-GATE-015)

## Full gate (all green)

`scripts/gate.sh` at the attested commit: fmt, clippy `--workspace
--all-targets -D warnings`, `cargo test --workspace`, release build, strict
shell suite (`AGENTFS_GATE_STRICT=1 crates/agentfs-cli/tests/all.sh`, both
corruption-torture transport legs), `phase8-validation.py --smoke` (noopen /
flush / external-base-drift coherence harnesses run inside it), and
`scripts/validation/consistency-canon.sh`. Result: exit 0 —
`PASS=24 SKIP=0 FAIL=0`, phase8 `failed_gates: []`, canon all rules PASS.
Full detail: `full-gate.json`.

pjdfstest: `scripts/validation/posix/run-pjdfstest.sh --profile phase5-ci
--agentfs-bin target/release/agentfs` — PASS (311 tests, run standalone at the
attested commit).

## Benchmark contract

Exact invocation (library/perf-contract.md, services.yaml `bench-multi`):

```bash
python3 scripts/validation/git-workload-benchmark-multi.py \
  --label m7-opt-sandbox-bringup-repeat --iterations 5 --warmup 1 \
  --agentfs-bin "$PWD/target/release/agentfs" \
  --output <out.json> --keep-iterations \
  --source .agents/benchmarks/fixtures/codex \
  --read-files 64 --read-bytes 4096 --edit-files 8
```

Baseline: the ACTIVE post-M3 baseline
`.agents/benchmarks/restructure-baseline/medians.json` (recorded at
`2b811f0`, orchestrator-signed rebaseline; the archived M1 artifact is
`medians-m1.json`). Gate rule: a phase is red iff its median regresses >5%
relative AND >10ms absolute.

Result (median-of-5 per phase, all 7 phases in band, `all_phases_passed: true`
in `medians.json`):

| phase | baseline (ms) | current (ms) | delta | verdict |
| --- | ---: | ---: | ---: | --- |
| checkout | 56.7 | 56.9 | +0.4% | OK |
| clone | 2800.9 | 2882.8 | +2.9% | OK |
| diff | 65.6 | 25.9 | -60.6% | OK |
| edit | 5.0 | 4.9 | -1.6% | OK |
| fsck | 215.0 | 177.9 | -17.2% | OK |
| read_search | 8.8 | 8.0 | -8.6% | OK |
| status | 178.0 | 145.8 | -18.1% | OK |

Provenance: the official quiet-window candidate/sentinel pair plus the
mandated brittle-phase repeat was recorded at `dae8aa6` (feature
m7-opt-sandbox-bringup; the same-window sentinel of the baseline binary was
green, and the first candidate run's clone red evaporated on the repeat per
the perf-contract brittle-phase rule). The pair is cited at the attested
commit per the orchestrator round-3 ruling: the only commit after `dae8aa6`
(`230c30a`, darwin seatbelt read scoping) changes `cfg(target_os = "macos")`
code, docs, and clap help text only — Linux binary perf semantics are
unchanged. Raw run JSONs (`bench-cand.json`, `bench-cand-repeat.json`,
`bench-sent.json`, with per-iteration walls) live in the mission evidence dir
`evidence/m7-cli-docs/m7-opt-sandbox-bringup/`.

The earlier attestation-round checkout reds (rounds 1-2, bisected to
`ea416c8`) were closed with zero code change: the tax was the user's PATH
git-ai shim taking a slow path inside the run-sandbox namespace, removed by
`dae8aa6`'s universal /usr/bin/git pinning. Full history:
`bench-milestones.json` M7 rows and library/perf-contract.md "M7 checkout red
— CLOSED".

## Per-milestone BENCH record (VAL-GATE-015)

Every BENCH-flagged milestone held the band; the stored-artifact reds that
occurred along the way (M2 fsck, M3 clone/fsck, M4a checkout/clone, M7
checkout) were each adjudicated per the procedure in
library/perf-contract.md — the M2-M4a set as ambient drift via same-window
sentinel reruns of the baseline code (leading to the orchestrator-signed
post-M3 rebaseline), and the M7 checkout red as the unpinned git-ai shim,
closed by harness/sandbox git pinning with an all-green pair at `dae8aa6`.
Full per-milestone table with sources: `bench-milestones.json`.

## Cross-surface flows

All flows executed end-to-end at the attested commit against the release
binary (30/30 checks OK; transcript `cross-surface5.log` in the mission
evidence dir, summary in `full-gate.json.cross_surface_flows`):

- clone -> mount -> git workflow -> unmount -> remount persistence
  (VAL-CROSS-001/002): clean clone status, `cross-persist` commit and hash
  stable across remount.
- moved single DB file reopens (VAL-CROSS-003): exactly one file in the DB
  family, `mv` + `integrity --json` + remount all green.
- sandboxed run leaves the host untouched (VAL-CROSS-004): before/after host
  census identical outside `.agentfs/run/cross-host`.
- run writes visible via `agentfs fs` (VAL-CROSS-005) and the session recorded
  in `agentfs timeline` (VAL-CROSS-006).
- mounted-FS and fs-CLI writes mutually visible byte-exact (VAL-CROSS-013).
- combined kill-switch leg noopen=0 noflush=0 uring=0 (VAL-CROSS-011):
  create/append/fsync/unlink-while-open/git all green, remount verified,
  integrity clean.

## File-size cap census (VAL-CONS-006)

Binding rule (architecture.md section 7): no production file over 2,500 lines
of NON-TEST CODE; sibling `tests.rs` files are excluded. The gate-enforced
census (`consistency-canon.sh` line-count-cap, cfg(test)-aware, comment/blank
stripping) reports zero offenders at the attested commit. For the audit trail,
the three files whose RAW line counts exceed 2,500 are all under the cap by
the binding rule:

| file | raw lines | non-test code lines |
| --- | ---: | ---: |
| crates/agentfs-fuse/src/adapter/mod.rs | 3073 | 2213 |
| crates/agentfs-nfs/src/server/nfs_handlers.rs | 3827 | 2354 |
| crates/agentfs-cli/src/cmd/migrate.rs | 2546 | 1144 |

## Notes

- `.agents/benchmarks/restructure-baseline/attestation` is a symlink to this
  directory for the mission feature's expected path.
- Benchmark runs were serialized on this machine per the perf contract; load
  at each accepted run start is recorded in the run logs.
- All gate/flow harnesses pin `/usr/bin/git` with a hookless temp-HOME git
  config (AGENTS.md universal-pinning rule); the post-run daemon census was
  clean.
