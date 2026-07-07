# Post-M3 Benchmark Rebaseline Provenance

This file records the orchestrator-approved rebaseline for
`.agents/benchmarks/restructure-baseline/medians.json`.

## Artifact changes

- Archived M1 medians: `.agents/benchmarks/restructure-baseline/medians-m1.json`
- New active medians: `.agents/benchmarks/restructure-baseline/medians.json`
- New active raw iterations: `.agents/benchmarks/restructure-baseline/medians-post-m3.json.iterations/`
- Original M1 raw iterations remain in place at `.agents/benchmarks/restructure-baseline/medians.json.iterations/`

## Provenance

| Field | Value |
| --- | --- |
| Repository | `/home/ain3sh/factory/vfs` |
| Branch | `dev` |
| Commit | `2b811f03493e861e5a7786592d25ac73b39e1aba` |
| Produced at | `2026-07-04T01:14:37Z` |
| Fixture | `.agents/benchmarks/fixtures/codex` |
| AgentFS binary | `/home/ain3sh/factory/vfs/target/release/agentfs` |
| Build command | `cargo +nightly build --release --workspace --bins` |
| Benchmark command | `python3 scripts/validation/git-workload-benchmark-multi.py --label misc-a-rebaseline-run3 --iterations 5 --warmup 1 --agentfs-bin "$PWD/target/release/agentfs" --output /tmp/vfs-val/misc-a-bench-rebaseline/run3.json --keep-iterations --source .agents/benchmarks/fixtures/codex --read-files 64 --read-bytes 4096 --edit-files 8` |

## Cached sentinel binary

`bin/agentfs` is a prebuilt baseline-code sentinel binary for future BENCH
features that need an ambient-drift check. It was built from the same recorded
baseline commit, `2b811f03493e861e5a7786592d25ac73b39e1aba`, with:

```bash
git worktree add --detach /tmp/vfs-restructure-baseline-bin.<tmp> 2b811f03493e861e5a7786592d25ac73b39e1aba
CARGO_TARGET_DIR=/home/ain3sh/factory/vfs/target \
  cargo +nightly build --release --workspace --bins \
  --manifest-path /tmp/vfs-restructure-baseline-bin.<tmp>/Cargo.toml
install -m 0755 /home/ain3sh/factory/vfs/target/release/agentfs \
  /home/ain3sh/factory/vfs/.agents/benchmarks/restructure-baseline/bin/agentfs
```

The cached binary reports `agentfs v0.6.4-107-g2b811f0`. Its SHA-256 is:

```text
3062585011a5a58da5b92781b808c58856be81125ee073b972513c4e4a3c0e53  .agents/benchmarks/restructure-baseline/bin/agentfs
```

## Load and hygiene checks

All benchmark runs were serialized. Before each run, the worker checked load,
CPU count, active AgentFS mounts, and active AgentFS processes. Runs would have
aborted if the 1-minute load exceeded `2 * cores`.

| Run | UTC start | Load average | Cores | Threshold | Result |
| --- | --- | ---: | ---: | ---: | --- |
| `run1` | `2026-07-04T01:10:45Z` | `8.00 6.23 6.32` | 14 | 28 | Completed, used as confirmation |
| `run2` | `2026-07-04T01:11:59Z` | `5.67 5.89 6.19` | 14 | 28 | Completed, discarded because it did not agree with `run1` or `run3` within band |
| `run3` | `2026-07-04T01:13:29Z` | `4.98 5.63 6.07` | 14 | 28 | Completed, recorded as the new active baseline |

## Agreement checks

The accepted pair is `run1` and `run3`. They agree within the perf-contract band
in both comparison directions:

- `bench-compare.py run1.json run3.json`: 0 red phases
- `bench-compare.py run3.json run1.json`: 0 red phases

After writing `run3` to `medians.json`, the active baseline was compared against
the current-HEAD confirmation run:

- `bench-compare.py .agents/benchmarks/restructure-baseline/medians.json /tmp/vfs-val/misc-a-bench-rebaseline/run1.json`: 0 red phases

## Recorded active medians

| Phase | AgentFS median |
| --- | ---: |
| checkout | `56.7ms` |
| clone | `2800.9ms` |
| diff | `65.6ms` |
| edit | `5.0ms` |
| fsck | `215.0ms` |
| read_search | `8.8ms` |
| status | `178.0ms` |
