# Session Tail

## user


## assistant
# Summary

Implemented `scripts/validation/git-workload-benchmark.py` in `/home/ain3sh/factory/vfs-phase7-worktrees/git-benchmark`.

# Actions Taken

- Added a deterministic, dependency-free Python Git workload benchmark.
- Supports default local generated repo, plus `--source` and `--remote`.
- Compares native vs AgentFS for:
  - clone
  - checkout
  - status
  - bounded read/search
  - representative edits
  - diff
  - `git fsck --strict`
- Reports:
  - phase timings and ratios
  - overall ratio
  - equivalence
  - AgentFS base tree hash before/after
  - AgentFS profile counters
  - AgentFS DB path/size/row stats
  - portability status from DB inspection
- Fails if AgentFS base tree changes, outputs differ, commands fail, or inspectable DB is non-portable.

# Files Changed

- `/home/ain3sh/factory/vfs-phase7-worktrees/git-benchmark/scripts/validation/git-workload-benchmark.py`

# Output Files Written

- `/tmp/agentfs-git-workload-smoke.json`
- `/tmp/agentfs-git-workload-help.txt`
- `/tmp/agentfs-git-workload-exec-help.txt`
- `/tmp/git-workload-benchmark.pyc`

# Validation Results

- `py_compile`: passed
- executable `--help`: passed
- smoke benchmark with small fixture: passed

Key smoke output:

```text
passed True
ratio 62.48281705396787
base_unchanged True
equivalent True
profile_count 2
db_bytes 159744
portable True
phase_keys checkout,clone,diff,edit,fsck,read_search,status
```

# Limitations / Notes

- The worktree-local AgentFS debug binary was missing, so the passing smoke run used `AGENTFS_BIN=/home/ain3sh/factory/vfs/cli/target/debug/agentfs`.
- A default run without `AGENTFS_BIN` attempted to build AgentFS and timed out in this environment.
