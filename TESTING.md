# Testing AgentFS

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
```

The helper creates identical native and AgentFS-overlay source trees, warms an
AgentFS session with a metadata-only read pass, performs the same one-byte edit
natively and through `agentfs run`, then emits JSON. The AgentFS DB growth is
measured as the total size of `delta.db` plus any `-wal`/`-shm` files after the
edit minus the same total immediately before the edit. If Python's stdlib
`sqlite3` can open the database, the output also includes `fs_data` row count,
stored chunk bytes, inline inode rows, origin rows, and `fs_config`.

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

### Workload baseline profile summaries

`scripts/validation/workload-baseline.py` also attaches parsed
`agentfs_profile_summary` JSON lines to each AgentFS run as
`iterations[].agentfs.profile_summaries` and reports
`agentfs.profile_summary_count` at the top level. This keeps profiling counters
associated with the native-vs-AgentFS timing and correctness result that
produced them.

### Backend-risk spike record

Use `scripts/validation/backend-risk-spike.py` to create a machine-readable
Turso-upgrade/rusqlite-fallback decision input template without changing
dependencies:

```bash
scripts/validation/backend-risk-spike.py \
  --candidate-turso-version 0.5.x \
  --output backend-risk.json
```

The output records current Cargo dependency state, the candidate Turso version,
the fallback crate under consideration, the minimum storage API surface that a
fallback must cover, validation commands to run in an isolated spike, and empty
decision fields for the measured result.

## pjdfstest

AgentFS keeps two pjdfstest modes:

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
