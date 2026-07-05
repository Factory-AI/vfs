# Testing AgentFS

Linux is the first-tier platform: every gate below runs on Linux. macOS is
second-tier (NFS mount only) and is covered by the manual release gate at the
end of this document.

## The honest gate: `scripts/gate.sh`

`scripts/gate.sh` is the single developer and CI entrypoint. It fails on the
first failing command and runs, in order:

1. `cargo +nightly fmt --all -- --check`
2. `cargo +nightly clippy --workspace --all-targets -- -D warnings`
3. `cargo +nightly test --workspace`
4. `cargo +nightly build --release --workspace --bins`
5. `crates/agentfs-cli/tests/all.sh` with `AGENTFS_GATE_STRICT=1`
   (a SKIP is a failure on the designated runner)
6. `scripts/validation/phase8-validation.py --smoke`
7. `scripts/validation/noopen-coherence.py`
8. `scripts/validation/flush-coherence.py`

Knobs: `AGENTFS_BIN` (defaults to `target/release/agentfs`),
`AGENTFS_GATE_SHELL_TIMEOUT` (default 900 s), `AGENTFS_GATE_PHASE8_TIMEOUT`
(default 20 s), and the `CORRUPTION_TORTURE_*` variables forwarded to the
shell suite.

CI (`.github/workflows/rust.yml`) runs the workspace job (fmt/clippy/build/test
on Linux and macOS, build+test on Linux arm64), the honest milestone gate
(`scripts/gate.sh` plus pjdfstest `phase5-ci`), and the release workflow.
FUSE-over-io_uring coverage is local-only: CI kernels do not expose
`/sys/module/fuse/parameters/enable_uring`, so the uring legs below are honest
only on a local machine that does.

## Workspace tests and generated-docs parity

```bash
cargo +nightly test --workspace
```

Two documentation files are generated from code and pinned by unit tests, so
doc drift fails the gate:

- `docs/KNOBS.md` — regenerate with
  `AGENTFS_UPDATE_KNOBS=1 cargo +nightly test -p agentfs-cli --lib knobs::tests::generated_knobs_doc_matches_declarations -- --exact`
- `docs/MANUAL.md` command reference — regenerate with
  `AGENTFS_UPDATE_MANUAL=1 cargo +nightly test -p agentfs-cli --lib docs::tests::manual_help_parity -- --exact`

## Shell integration suite

```bash
AGENTFS_GATE_STRICT=1 crates/agentfs-cli/tests/all.sh
```

The suite covers init/mount/run/exec flows, syscall coverage, signal
teardown, corruption torture (both `AGENTFS_FUSE_URING=1` and `=0` legs),
sidecar cleanup, and MCP server behavior, and prints a PASS/SKIP/FAIL
summary. In strict mode a SKIP is red; `AGENTFS_GATE_FORCE_SKIP=<label|all>`
synthesizes a SKIP for testing the runner itself. Never run the corruption
torture test concurrently with another mount, test suite, or benchmark.

## Python validation gates

All harnesses take `--agentfs-bin` (or `AGENTFS_BIN`); build a release binary
first for anything timing-sensitive.

```bash
# Orchestrated Phase 8 policy gate (smoke profile is the milestone gate)
python3 scripts/validation/phase8-validation.py --smoke --timeout 20 \
  --agentfs-bin "$PWD/target/release/agentfs" --output /tmp/vfs-val/phase8.json

# Default-on FUSE semantics coherence (no-open and no-flush legs)
python3 scripts/validation/noopen-coherence.py --agentfs-bin "$PWD/target/release/agentfs"
python3 scripts/validation/flush-coherence.py --agentfs-bin "$PWD/target/release/agentfs"

# Overlay base-drift rejection
python3 scripts/validation/external-base-mutation-coherence.py \
  --agentfs-bin "$PWD/target/release/agentfs" \
  --output /tmp/vfs-val/external-base-mutation.json
```

Focused stress harnesses used by Phase 8 and available directly:
`phase8-concurrent-git-stress.py` (concurrent git correctness, base
immutability, portability), `fuse-serialization-stress.py` (read-lane
parallelism), `phase8-writeback-durability.py` (fsynced data survives
SIGKILL + remount), and `phase8-writeback-no-fsync-crash.py` (no-fsync crash
consistency: missing/prefix data allowed, corruption rejected).

`AGENTFS_PROFILE=1` makes AgentFS emit `agentfs_profile_summary` counter
lines on exit; most harnesses parse and attach them to their JSON reports.

## Benchmarks (local-only policy)

The codex workload benchmark is a local performance gate, not a CI job. Run
it serialized on a quiet machine with a fresh release build, and compare
per-phase medians of 5 runs; single runs are noise:

```bash
cargo +nightly build --release --workspace --bins
python3 scripts/validation/git-workload-benchmark-multi.py \
  --label bench --iterations 5 --warmup 1 \
  --agentfs-bin "$PWD/target/release/agentfs" \
  --source <local benchmark fixture checkout> \
  --read-files 64 --read-bytes 4096 --edit-files 8 \
  --output /tmp/vfs-val/bench-multi.json --keep-iterations

# Gate rule: red = >5% relative AND >10ms absolute per-phase median regression
python3 scripts/validation/bench-compare.py <baseline-medians.json> /tmp/vfs-val/bench-multi.json
```

Focused local benchmarks: `git-workload-benchmark.py` (single run with
`--profile` phase breakdown), `read-path-benchmark.py`,
`large-edit-benchmark.py` (one-byte edit to a large base file must grow the
delta DB by O(changed chunks), with `--partial-origin` / `--no-partial-origin`
legs), `base-read-benchmark.py`, and `agentfs-clone-benchmark.py`.

## pjdfstest

AgentFS keeps three pjdfstest modes:

- `phase45-ci`: a conservative, unprivileged supported subset.
- `phase5-ci`: the expanded unprivileged supported subset (CI-wired in the
  milestone gate).
- `full`: the upstream suite, used for exploratory POSIX triage.

The supported subsets intentionally exclude root-only capabilities (`mknod`
for block/char devices, successful `chown`/`lchown`, alternate uid/gid
execution); exclusions are tracked in
`scripts/validation/posix/pjdfstest/known-gaps.tsv`.

Install pjdfstest locally:

```bash
git clone https://github.com/pjd/pjdfstest.git
cd pjdfstest
autoreconf -ifs
./configure --prefix="$HOME/.local"
make pjdfstest
install -m 0755 pjdfstest "$HOME/.local/bin/pjdfstest"
```

Run the supported gate against a workspace build:

```bash
cargo +nightly build --workspace
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile phase5-ci
```

The harness writes a report directory (TAP log, exit status, selected
profile/manifest/tests, known-gap taxonomy) and exits `77` when
prerequisites are missing. `--list-profiles` lists profiles;
`--partial-origin` mounts the fixture with the partial-origin policy enabled.
Summarize a log with
`scripts/validation/posix/summarize-pjdfstest-log.py <pjdfstest.log>
--known-gaps scripts/validation/posix/pjdfstest/known-gaps.tsv`.
Do not treat `full` as a required gate while known gaps remain.

## Production safety checks

```bash
# SQLite + AgentFS schema invariants (exit nonzero on any failed check)
agentfs integrity .agentfs/my-agent.db --json

# Portable snapshot: checkpoint WAL, copy main DB, reopen, re-verify
agentfs backup .agentfs/my-agent.db /tmp/my-agent-backup.db --verify
```

Partial-origin overlay databases are rejected by plain `backup` because
their contents depend on an external base tree; use `backup --materialize`
or `agentfs materialize` first, and audit the dependency with
`agentfs integrity --require-portable --check-base`.

## macOS: second-tier platform and the manual release gate

macOS support is explicitly second-tier: mounting uses the NFS backend only
(no FUSE, no `agentfs ps`), and NFS protocol semantics are validated by cargo
protocol/unit tests on Linux. There is no macOS coverage in the automated
gate, so the macOS NFS git validation script is a **manual release gate**: it
must be run on real macOS hardware before a release is cut.

```bash
cargo +nightly build --release --workspace --bins
scripts/validation/macos-nfs-git-validation.sh \
  --agentfs-bin "$PWD/target/release/agentfs"
```

The harness is temp-directory scoped, initializes a fresh AgentFS database,
mounts it with `agentfs mount --backend nfs`, then runs `git init`,
`git add`, `git commit`, and `git fsck --strict`, and verifies at least one
loose object was written. A passing run ends with
`macOS NFS git validation passed`. Unsupported platforms or missing
prerequisites exit `77`; on Linux that skip is expected, not a failure. A
release SHOULD NOT ship without a passing run of this script on real
hardware.
