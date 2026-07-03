# M1 Restructure Benchmark Baseline

Authoritative M1 performance baseline for the VFS restructure mission. Future BENCH
features compare their median-of-5 codex workload results against
`medians.json` with the 5% per-phase regression band from the mission
performance contract.

## Provenance

| Field | Value |
| --- | --- |
| Repository | `/home/ain3sh/factory/vfs` |
| Branch | `dev` |
| Commit | `ae1e13fcb1c52d5c57be051610a04aa9a318f2a5` |
| Short commit | `ae1e13f` |
| Produced at | `2026-07-03T11:39:55Z` |
| Fixture | `.agents/benchmarks/fixtures/codex` |
| Fixture commit | `7d47056ea42636271ac020b86347fbbef49490aa` |
| AgentFS binary | `/home/ain3sh/factory/vfs/target/release/agentfs` |
| AgentFS version | `agentfs v0.6.4-89-gae1e13f` |
| Kernel | `Linux factory-ain3sh 7.1.2-3-cachyos #1 SMP PREEMPT_DYNAMIC Mon, 29 Jun 2026 14:34:33 +0000 x86_64 GNU/Linux` |
| CPU | `Intel(R) Core(TM) Ultra 7 255U` |
| Logical CPUs | `14` |
| Memory | `30Gi` |
| Load average after run | `5.19 7.17 7.02` |
| Rust | `rustc 1.97.0-nightly (fb0a5a5a9 2026-05-08)` |
| Cargo | `cargo 1.97.0-nightly (4f9b52075 2026-05-01)` |
| Python | `Python 3.14.6` |

The run was serialized by this worker: no other validators, test suites, or
benchmarks were started concurrently. Pre-run `mount | rg agentfs` and
`pgrep -a agentfs` returned no output.

## Release Build

```bash
cargo +nightly build --release --workspace --bins
```

## Benchmark Invocation

Exact invocation used after checking `git-workload-benchmark-multi.py --help`:

```bash
cd /home/ain3sh/factory/vfs
python3 scripts/validation/git-workload-benchmark-multi.py \
  --label m1-restructure-baseline \
  --iterations 5 \
  --warmup 1 \
  --agentfs-bin "$PWD/target/release/agentfs" \
  --output .agents/benchmarks/restructure-baseline/medians.json \
  --keep-iterations \
  --source .agents/benchmarks/fixtures/codex \
  --read-files 64 \
  --read-bytes 4096 \
  --edit-files 8
```

This produced five measured iterations and one discarded warmup iteration.

## Median Results

| Phase | Native median (s) | AgentFS median (s) | Ratio median |
| --- | ---: | ---: | ---: |
| checkout | 0.180396 | 0.070580 | 0.380195x |
| clone | 0.360556 | 3.039249 | 8.387362x |
| diff | 0.217945 | 0.044665 | 0.270021x |
| edit | 0.000536 | 0.005347 | 8.011537x |
| fsck | 0.201479 | 0.186784 | 0.910591x |
| read_search | 0.006169 | 0.008318 | 1.264564x |
| status | 0.231208 | 0.169804 | 0.734421x |

Overall median: native `1.043254s`, AgentFS `4.286349s`, ratio `4.273160x`.

## Artifact Layout

- `medians.json`: machine-readable aggregate with `iterations = 5`,
  `warmup_iterations = 1`, per-phase medians, quartiles, stdev, and iteration
  return codes.
- `medians.json.iterations/warmup-00.json`: discarded warmup run.
- `medians.json.iterations/iter-00.json` through `iter-04.json`: raw measured
  iteration JSON files retained for auditability.
