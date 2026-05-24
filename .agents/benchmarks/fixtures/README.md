# Benchmark fixtures

The git workload benchmark scripts (`scripts/validation/git-workload-benchmark.py`
and `scripts/validation/git-workload-benchmark-multi.py`) accept any local git
checkout via `--source`. The canonical fixture used for the Tier One baselines
and post-impl measurements in `.agents/benchmarks/*.agg.json` is a fresh clone
of `openai/codex`.

To regenerate locally before re-running the multi-iteration wrapper:

```bash
mkdir -p .agents/benchmarks/fixtures
git clone --bare https://github.com/openai/codex.git \
    .agents/benchmarks/fixtures/codex
```

The fixture itself is gitignored (see `.gitignore`) because it is ~63 MiB and
its content changes upstream. Pin to a specific commit if the comparison
across machines needs to be apples-to-apples.
