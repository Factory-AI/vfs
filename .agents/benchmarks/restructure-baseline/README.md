# Restructure Benchmark Baseline

Authoritative performance baseline for the VFS restructure mission. Future
BENCH features compare their median-of-5 codex workload results against the
active `medians.json` artifact with the mission performance contract's
per-phase regression band.

## Active baseline: post-M3

`medians.json` now contains the orchestrator-approved post-M3 rebaseline. Full
provenance, load checks, agreement checks, and recorded medians are in
`provenance-post-m3.md`.

| Field | Value |
| --- | --- |
| Repository | `/home/ain3sh/factory/vfs` |
| Branch | `dev` |
| Commit | `2b811f03493e861e5a7786592d25ac73b39e1aba` |
| Produced at | `2026-07-04T01:14:37Z` |
| Fixture | `.agents/benchmarks/fixtures/codex` |
| AgentFS binary | `/home/ain3sh/factory/vfs/target/release/agentfs` |
| Build command | `cargo +nightly build --release --workspace --bins` |

## Benchmark invocation

The post-M3 active baseline was produced by the standard median-of-5 contract:

```bash
cd /home/ain3sh/factory/vfs
python3 scripts/validation/git-workload-benchmark-multi.py \
  --label misc-a-rebaseline-run3 \
  --iterations 5 \
  --warmup 1 \
  --agentfs-bin "$PWD/target/release/agentfs" \
  --output /tmp/vfs-val/misc-a-bench-rebaseline/run3.json \
  --keep-iterations \
  --source .agents/benchmarks/fixtures/codex \
  --read-files 64 \
  --read-bytes 4096 \
  --edit-files 8
```

After the confirmation checks documented in `provenance-post-m3.md`, that run
was copied into `.agents/benchmarks/restructure-baseline/medians.json`.

## Artifact layout

- `medians.json`: active post-M3 machine-readable aggregate used by
  `scripts/validation/bench-compare.py`.
- `provenance-post-m3.md`: authoritative provenance for the active baseline.
- `medians-post-m3.json.iterations/`: raw warmup and measured iteration JSON
  files for the active `medians.json`. The directory name is historical: it
  disambiguates the post-M3 raw iterations from the archived M1 raw iterations,
  and there is intentionally no sibling `medians-post-m3.json` file.
- `medians-m1.json`: archived M1 aggregate retained for auditability.
- `medians.json.iterations/`: archived M1 warmup and measured iteration JSON
  files retained for auditability.

## Archived M1 baseline

The original M1 baseline was produced at commit
`ae1e13fcb1c52d5c57be051610a04aa9a318f2a5` on
`2026-07-03T11:39:55Z`. It is no longer the active comparator. Use
`medians-m1.json` and `medians.json.iterations/` only for historical audits.
