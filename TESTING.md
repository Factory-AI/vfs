# Testing AgentFS

## Phase 5.5 read-path benchmark and profiling

Use `scripts/validation/read-path-benchmark.py` to capture reproducible
native-vs-AgentFS read-path baselines before and after read-path changes. The
script creates a deterministic temporary fixture, runs identical read-only
workloads natively and through `agentfs run`, writes JSON under `/tmp` by
default, and also emits the same JSON to stdout.

```bash
# Fast smoke with profile summaries/counters
AGENTFS_PROFILE=1 scripts/validation/read-path-benchmark.py \
  --files 8 \
  --dirs 3 \
  --stat-iterations 1 \
  --readdir-iterations 1 \
  --open-iterations 1 \
  --timeout 60

# Larger bounded read-path baseline
scripts/validation/read-path-benchmark.py \
  --files 256 \
  --dirs 32 \
  --file-size-bytes 8192 \
  --stat-iterations 8 \
  --readdir-iterations 16 \
  --open-iterations 8 \
  --timeout 180

# Only steady warm-session measurement
scripts/validation/read-path-benchmark.py --modes warm --output /tmp/agentfs-read-warm.json
```

The benchmark covers:

- bounded file scan,
- repeated `stat`/`lstat` storm,
- `readdir` storm,
- `readdir_plus` approximation via `os.scandir(...).stat(...)`,
- open/read/close loop,
- cold and warm AgentFS sessions,
- startup/session overhead vs child workload time where measurable.

Environment:

| Variable | Description |
|---|---|
| `AGENTFS_BIN` | path/name of the `agentfs` executable |
| `AGENTFS_PROFILE=1` | include parsed `agentfs_profile_summary` lines and counter summaries |
| `READ_PATH_BENCHMARK_MODES` | comma-separated default modes, e.g. `cold,warm` |
| `READ_PATH_BENCHMARK_TIMEOUT` | per-command timeout in seconds |
| `READ_PATH_BENCHMARK_KEEP_TEMP=1` | keep temporary fixture trees and isolated HOME |

Machine-readable schema (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "benchmark": "phase55-read-path",
  "git_commit": "<repo commit>",
  "command": {
    "argv": ["scripts/validation/read-path-benchmark.py", "..."],
    "workload_argv": ["python", "-c", "..."],
    "agentfs_prefix": ["/path/to/agentfs", "run", "--session", "<session>", "--no-default-allows", "--"]
  },
  "environment": {
    "AGENTFS_PROFILE": "1",
    "AGENTFS_BIN": "/path/to/agentfs"
  },
  "parameters": {
    "files": 64,
    "dirs": 8,
    "file_size_bytes": 4096,
    "scan_bytes": 1024,
    "stat_iterations": 4,
    "readdir_iterations": 8,
    "open_iterations": 3,
    "open_read_bytes": 512,
    "modes": ["cold", "warm"]
  },
  "agentfs": {
    "bin": "/path/to/agentfs",
    "profile_enabled": true,
    "profile_summary_count": 4
  },
  "summary": {
    "native_seconds": 0.01,
    "agentfs_seconds": 0.2,
    "ratio": 20.0,
    "all_equivalent": true
  },
  "modes": [
    {
      "mode": "cold",
      "session": "read-path-...",
      "summary": {
        "native_seconds": 0.01,
        "agentfs_seconds": 0.2,
        "ratio": 20.0
      },
      "steady_state": {
        "native_workload_seconds": 0.009,
        "agentfs_workload_seconds": 0.15,
        "ratio": 16.7
      },
      "equivalence": {
        "checked": true,
        "equivalent": true,
        "native_digest": "...",
        "agentfs_digest": "..."
      },
      "native": {
        "run": {"duration_seconds": 0.01, "returncode": 0},
        "workload": {
          "digest": "...",
          "phase_seconds": {
            "bounded_file_scan": 0.001,
            "stat_lstat_storm": 0.001,
            "readdir_storm": 0.001,
            "readdir_plus_storm": 0.001,
            "open_read_close_loop": 0.001
          },
          "counts": {}
        },
        "timing": {
          "outer_seconds": 0.01,
          "workload_seconds": 0.009,
          "startup_or_session_overhead_seconds": 0.001
        }
      },
      "agentfs": {
        "warmup": null,
        "run": {
          "duration_seconds": 0.2,
          "returncode": 0,
          "profile_summaries": []
        },
        "workload": {"digest": "...", "phase_seconds": {}, "counts": {}},
        "timing": {
          "outer_seconds": 0.2,
          "workload_seconds": 0.15,
          "startup_or_session_overhead_seconds": 0.05
        },
        "profile_summaries": [],
        "profile_counters": {
          "summary_count": 2,
          "last_by_source": {
            "fuse_session": {"fuse_lookup_count": 1},
            "agentfs": {"lookup_count": 1}
          },
          "max_counters": {
            "lookup_count": 1,
            "getattr_count": 1,
            "readdir_count": 1,
            "readdir_plus_count": 1,
            "fuse_callback_count": 1
          }
        }
      }
    }
  ],
  "temp_dir": "/tmp/agentfs-read-path-benchmark-...",
  "kept_temp": false,
  "output_path": "/tmp/agentfs-read-path-benchmark-....json"
}
```

## Phase 5 profiling and backend-risk helpers

### Large base-file single-byte edit benchmark

Use `scripts/validation/large-edit-benchmark.py` to measure the Phase 5
copy-up risk called out in the north-star spec: a one-byte edit to a large
base-layer file should grow the AgentFS delta database by O(changed chunks),
not O(file size).

```bash
# Spec-sized run
scripts/validation/large-edit-benchmark.py --file-size-mib 200 --profile

# Fast smoke
scripts/validation/large-edit-benchmark.py --file-size-mib 1 --timeout 60

# Experimental partial-origin smoke
scripts/validation/large-edit-benchmark.py --file-size-mib 1 --partial-origin --timeout 60
```

The helper creates identical native and AgentFS-overlay source trees, warms an
AgentFS session with a metadata-only read pass, performs the same one-byte edit
natively and through `agentfs run`, then emits JSON. The AgentFS DB growth is
measured as the total size of `delta.db` plus any `-wal`/`-shm` files after the
edit minus the same total immediately before the edit. If Python's stdlib
`sqlite3` can open the database, the output also includes `fs_data` row count,
stored chunk bytes, inline inode rows, origin rows, partial-origin rows,
chunk-override rows, and `fs_config`.

Partial-origin overlay copy-up remains opt-in for Phase 5.5. Use
`--partial-origin` or `AGENTFS_OVERLAY_PARTIAL_ORIGIN=1` to enable it; use
`--no-partial-origin` to force the default whole-file copy-up path even when the
environment variable is set. The JSON output reports the selected mode as
`agentfs.partial_origin_enabled` and echoes the effective env flag under
`agentfs.env_flags.AGENTFS_OVERLAY_PARTIAL_ORIGIN`.

Machine-readable schema (`schema_version: 1`):

```json
{
  "schema_version": 1,
  "benchmark": "phase5-large-base-single-byte-edit",
  "git_commit": "<repo commit>",
  "parameters": {
    "file_size_bytes": 209715200,
    "file_size_mib": 200,
    "offset": 104857600,
    "edit_width_bytes": 1
  },
  "agentfs": {
    "bin": "/path/to/agentfs",
    "session": "large-edit-...",
    "db_path": "/tmp/.../home/.agentfs/run/.../delta.db",
    "profile_enabled": true,
    "partial_origin_enabled": false,
    "env_flags": {"AGENTFS_OVERLAY_PARTIAL_ORIGIN": null},
    "profile_summary_count": 2
  },
  "database": {
    "before_edit": {"total_bytes": 32768, "artifacts": []},
    "after_edit": {"total_bytes": 210000000, "artifacts": []},
    "growth_bytes": 209967232,
    "inspect_before": {"inspectable": true},
    "inspect_after": {
      "inspectable": true,
      "fs_data_rows": 3200,
      "fs_data_bytes": 209715200,
      "fs_origin_rows": 1,
      "fs_partial_origin_rows": 0,
      "fs_chunk_override_rows": 0,
      "fs_config": {"schema_version": "0.5", "chunk_size": "65536"}
    }
  },
  "native": {"duration_seconds": 0.1, "run": {}, "result": {}},
  "agentfs_overlay": {
    "duration_seconds": 1.2,
    "warmup": {},
    "run": {"profile_summaries": []},
    "result": {}
  },
  "base_file": {
    "original_sha256": "...",
    "native_sha256_after": "...",
    "agentfs_base_sha256_after": "..."
  },
  "correctness": {
    "warmup_returncode_zero": true,
    "native_returncode_zero": true,
    "agentfs_returncode_zero": true,
    "outputs_match": true,
    "agentfs_base_unchanged": true,
    "native_file_changed": true,
    "passed": true
  }
}
```

When `--profile` or `AGENTFS_PROFILE=1` is set, parsed
`agentfs_profile_summary` lines from AgentFS stderr are attached to the
`agentfs_overlay.warmup.profile_summaries` and
`agentfs_overlay.run.profile_summaries` arrays.

The current recommendation is to keep partial-origin disabled by default. SDK
overlay tests cover remount, main-DB snapshot restore, unlink cleanup, hardlink,
rename/`readdir_plus`, truncate shrink/extend, and drift detection; defaulting
should wait until the same flag is included in supported FUSE/CLI torture and
pjdfstest gates without regressions.

### Workload baseline profile summaries

`scripts/validation/workload-baseline.py` also attaches parsed
`agentfs_profile_summary` JSON lines to each AgentFS run as
`iterations[].agentfs.profile_summaries` and reports
`agentfs.profile_summary_count` at the top level. This keeps profiling counters
associated with the native-vs-AgentFS timing and correctness result that
produced them.

### Backend-risk spike record

Use `scripts/validation/backend-risk-spike.py` to create a machine-readable
Turso-upgrade/rusqlite-fallback decision record without changing dependencies:

```bash
scripts/validation/backend-risk-spike.py \
  --candidate-turso-version 0.5.x \
  --output backend-risk.json
```

The output records current Cargo dependency state, the candidate Turso version,
the fallback crate under consideration, the minimum storage API surface that a
fallback must cover, validation commands to run in an isolated spike, and
decision fields for measured results.

#### Phase 5.5 Turso backend spike workflow

Run dependency-upgrade experiments in an isolated worktree/branch, not the main
worktree:

```bash
git worktree add ../agentfs-backend-spike -b phase55-backend-spike
cd ../agentfs-backend-spike
```

Attempt the candidate Turso upgrade by changing the Rust manifests to the
candidate version/range, then resolve and build with Cargo:

```bash
cargo check --manifest-path sdk/rust/Cargo.toml
cargo check --manifest-path cli/Cargo.toml
```

If the default CLI build is blocked by optional sandbox/nightly-only
dependencies, also run the no-sandbox build to separate backend API breakage
from unrelated optional-feature blockers:

```bash
cargo check --manifest-path cli/Cargo.toml --no-default-features
```

When the candidate builds, run the meaningful gates that are available in the
spike environment:

```bash
cargo test --manifest-path sdk/rust/Cargo.toml
cargo test --manifest-path cli/Cargo.toml --no-default-features
cli/tests/all.sh
scripts/validation/phase0.sh
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile phase45-ci
```

Record the actual candidate results in JSON. Repeat `--validation-*` options for
each command that was run, and use blockers for exact compiler/API/runtime
failures:

```bash
scripts/validation/backend-risk-spike.py \
  --candidate-turso-version 0.5.x \
  --resolved-turso-version 0.5.3 \
  --upgrade-built true \
  --validation-result sdk_tests=passed \
  --validation-command 'sdk_tests=cargo test --manifest-path sdk/rust/Cargo.toml' \
  --validation-exit-code sdk_tests=0 \
  --validation-summary 'sdk_tests=130 passed' \
  --validation-result cli_tests=passed \
  --validation-command 'cli_tests=cargo test --manifest-path cli/Cargo.toml --no-default-features' \
  --validation-exit-code cli_tests=0 \
  --validation-summary 'cli_tests=89 passed, 1 ignored' \
  --decision-status upgraded \
  --selected-path turso-upgrade-now \
  --rationale 'Turso 0.5.x built with minimal test expectation updates.' \
  --output /tmp/backend-risk.json
```

If the upgrade is blocked, set `--upgrade-built blocked`, add every exact
compiler/API blocker with `--turso-blocker`, and fill the rusqlite fallback
fields:

```bash
scripts/validation/backend-risk-spike.py \
  --candidate-turso-version 0.5.x \
  --upgrade-built blocked \
  --turso-blocker 'cargo check: exact compiler/API error here' \
  --fallback-trait-practicality 'requires async boundary around open/connect/execute/query/transactions' \
  --fallback-invasiveness 'high: current SDK and CLI directly expose turso Connection, Row, Value, sync Database, and checkpoint/encryption APIs' \
  --fallback-risk-reduction 'useful only if Turso remains blocked after a minimal compatibility patch' \
  --decision-status fallback-required \
  --selected-path rusqlite-fallback-spike \
  --output /tmp/backend-risk.json
```

## macOS NFS git validation (#333)

Use `scripts/validation/macos-nfs-git-validation.sh` on a real macOS host to
validate the NFS CREATE-returned write-handle path used by git loose-object
writes:

```bash
cd /path/to/agentfs
cargo build --manifest-path cli/Cargo.toml --no-default-features
scripts/validation/macos-nfs-git-validation.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs"
```

The harness is temp-directory scoped under `/tmp`, initializes a fresh AgentFS
database, mounts it with `agentfs mount --backend nfs`, then runs:

```bash
git init
git add README.txt
git commit -m "validate macos nfs git loose objects"
git fsck --strict
```

It also verifies that the repository produced at least one loose object under
`.git/objects/[0-9a-f][0-9a-f]/`. Expected successful output includes:

```text
AgentFS binary: /path/to/agentfs
Report directory: /tmp/agentfs-macos-nfs-git-report...
Loose object count: <nonzero>
macOS NFS git validation passed. Logs: /tmp/agentfs-macos-nfs-git-report...
```

Unsupported platforms or missing prerequisites exit with `77`; on Linux this is
an expected skip, not a failure. If macOS `mount_nfs` requires privileges in the
local environment, run the same command from a shell where user NFS mounts are
permitted, or inspect the reported `mount.log`. Until this script passes on a
real macOS host, #333 should be treated as code-fixed but platform-validation
pending.

## pjdfstest

AgentFS keeps three pjdfstest modes:

- `phase45-ci`: a conservative, unprivileged supported subset that should pass on the current FUSE implementation.
- `phase5-ci`: the expanded Phase 5 unprivileged supported subset. It includes `phase45-ci` plus additional currently-passing path, FIFO, symlink-loop, sparse large-file, socket-open, and rename/rmdir error-path coverage.
- `full`: the upstream pjdfstest suite, used for exploratory POSIX triage and Phase 5 planning.

The supported subset intentionally excludes tests that require root-only capabilities (`mknod` for block/char devices, successful `chown`/`lchown`, and alternate uid/gid execution). Those exclusions are tracked in `scripts/validation/posix/pjdfstest/known-gaps.tsv` so Phase 5 can separate unsupported-by-contract gaps from real filesystem bugs.

### Install pjdfstest locally

```bash
git clone https://github.com/pjd/pjdfstest.git
cd pjdfstest
autoreconf -ifs
./configure --prefix="$HOME/.local"
make pjdfstest
install -m 0755 pjdfstest "$HOME/.local/bin/pjdfstest"
command -v prove
command -v pjdfstest
```

### Run the supported AgentFS gate

Build the CLI first if needed:

```bash
cd cli
cargo build
cd ..
```

Then run the Phase 4.5 supported profile:

```bash
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile phase45-ci
```

Expected result:

```text
All tests successful.
Files=37, Tests=142
Result: PASS
```

The harness writes a report directory containing:

- `pjdfstest.log` - TAP output from `prove`
- `status.txt` - `prove` exit status
- `selected-profile.txt` - selected profile name
- `selected-manifest.tsv` - selected manifest path and SHA-256 when a manifest-backed profile is used
- `selected-tests.txt` - exact test files run
- `known-unsupported.tsv` - current known POSIX gaps and triage rationale

Missing prerequisites exit with `77`. A nonzero exit other than `77` means the selected supported profile failed and should be treated as a real regression.

List supported profiles:

```bash
scripts/validation/posix/run-pjdfstest.sh --list-profiles
```

Run the expanded Phase 5 supported profile:

```bash
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile phase5-ci
```

Summarize a pjdfstest report and map failures to the known-gap taxonomy:

```bash
scripts/validation/posix/summarize-pjdfstest-log.py \
  /path/to/pjdfstest.log \
  --known-gaps scripts/validation/posix/pjdfstest/known-gaps.tsv
```

### Run the full exploratory suite

```bash
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile full
```

Full pjdfstest currently exposes known AgentFS POSIX gaps. Use it to expand `known-gaps.tsv` and to choose the next Phase 5 correctness work; do not use `full` as a required CI pass gate until the gaps are resolved or explicitly declared unsupported.

## xftests

First, build the `agentfs` executable and install it locally including the `mount.fuse.agentfs` helper:

```bash
cd cli
cargo build --release
cp target/release/agentfs /usr/local/bin
cp scripts/mount.fuse.agentfs /sbin
```

Then, clone the xfstests repo:

```bash
git clone git://git.kernel.org/pub/scm/fs/xfs/xfstests-dev.git
```

Configure the filesystem under test:

```bash
cat local.config
export FSTYP=fuse
export FUSE_SUBTYP=.agentfs
export TEST_DEV=<database file>
export TEST_DIR=<mount directory>
```

Then, run xfstests:

```bash
sudo ./check -g quick generic/
```
