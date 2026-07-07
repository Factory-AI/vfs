#!/usr/bin/env bash
set -u

usage() {
    cat <<'USAGE'
Usage: phase0.sh

Runs the Phase 0/1 local validation smoke:
  1. Phase 1 fork governance check
  2. Phase 0 built-in native-vs-AgentFS synthetic workload baseline

Environment:
  AGENTFS_BIN                    optional agentfs executable path/name
  WORKLOAD_BASELINE_ITERATIONS   smoke iterations (default: 1)
  WORKLOAD_BASELINE_TIMEOUT      per-command timeout seconds (default: 120)
  WORKLOAD_BASELINE_KEEP_TEMP    keep temp baseline directories when true/1

For real factory-mono baselines, run workload-baseline.py directly with
--source and --command so the measured workload matches the target repo.
USAGE
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python_bin="${PYTHON:-python3}"
iterations="${WORKLOAD_BASELINE_ITERATIONS:-1}"

status=0

printf '== Phase 1: fork governance ==\n'
if ! "$script_dir/check-fork-governance.sh"; then
    status=1
fi

printf '\n== Phase 0: built-in workload baseline smoke ==\n'
if ! "$python_bin" "$script_dir/workload-baseline.py" \
    --mode synthetic \
    --iterations "$iterations"; then
    status=1
fi

cat <<'NEXT_STEPS'

== Next steps for real factory-mono baselines ==
Run a representative command against a real checkout, for example:

  AGENTFS_BIN=/path/to/agentfs \
    scripts/validation/workload-baseline.py \
    --source /path/to/factory-mono \
    --command 'your representative build/test command'

Notes:
  - By default the source tree is copied into temp directories before timing.
  - Add --exclude PATTERN for large caches that should not be part of the baseline copy.
  - Use --keep-temp when you need to inspect the native and AgentFS worktrees.
  - Use --in-place-native only for known read-only workloads.
NEXT_STEPS

exit "$status"
