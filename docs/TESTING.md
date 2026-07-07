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
5. `crates/agentfs-cli/tests/all.sh` with `AGENTFS_GATE_STRICT=1` and
   `AGENTFS_BIN` pointing at the release binary
   (a SKIP is a failure on the designated runner)
6. `scripts/validation/phase8-validation.py --smoke` — the top-level Python
   gate; it runs the noopen/flush/base-drift coherence harnesses internally
7. `scripts/validation/consistency-canon.sh` — the structural canon census
   (crate DAG, sealed transport surfaces, file-size cap, tracing-only
   logging, env-reads-at-the-config-edge, `await_holding_lock`, lock-order
   headers, docs layout, changelog)

Knobs: `AGENTFS_BIN` (defaults to `target/release/agentfs`),
`AGENTFS_GATE_SHELL_TIMEOUT` (default 900 s), `AGENTFS_GATE_PHASE8_TIMEOUT`
(default 20 s), `AGENTFS_GATE_ALLOWED_SKIPS` (forwarded to the shell suite,
see below), and the `CORRUPTION_TORTURE_*` variables forwarded to the
shell suite. The gate pins `TMPDIR` to a per-run scratch dir cleaned on exit
so dependency temp-file litter cannot accumulate on the host.

CI (`.github/workflows/rust.yml`) runs the workspace job (fmt/clippy/build/test
on Linux and macOS, build+test on Linux arm64), the honest milestone gate
(`scripts/gate.sh` plus pjdfstest `phase5-ci`), and the release workflow.
The gate job first sets `kernel.apparmor_restrict_unprivileged_userns=0` so
the `agentfs run` suites exercise the sandbox instead of skipping on the
Ubuntu 24.04 runner image. FUSE-over-io_uring coverage stays local-only: the
CI kernel does not expose `/sys/module/fuse/parameters/enable_uring`, so the
panic-census uring leg can never run there and the gate job allowlists that
one skip with `AGENTFS_GATE_ALLOWED_SKIPS=fuse-sigint-panic-census`. The
`corruption-torture-uring` leg needs no allowlist entry: without
`enable_uring` the mount falls back to the legacy channel and the leg passes,
exercising uring only on kernels that offer it. The uring legs are therefore
honest only on a local machine whose kernel exposes and enables
`enable_uring`.

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
sidecar cleanup, MCP server behavior, and a `cli-smoke` pass over the whole
user-level command surface (init, run, exec, clone, fs, timeline,
backup/materialize, integrity, migrate, MCP, ps, completions, and the
deprecated `nfs`/`mcp-server` aliases), and prints a PASS/SKIP/FAIL
summary. Every test runs out of its own `mktemp -d` root with trap cleanup
and honors `AGENTFS_BIN` (falling back to `cargo run`). In strict mode a SKIP
is red unless its test label is named in
`AGENTFS_GATE_ALLOWED_SKIPS=<label[,label...]>`, the escape hatch for runner
kernels that cannot provide a prerequisite at all;
`AGENTFS_GATE_FORCE_SKIP=<label|all>` synthesizes a SKIP for testing the
runner itself. Never run the corruption
torture test concurrently with another mount, test suite, or benchmark.

## Python validation gates

All harnesses take `--agentfs-bin` (or `AGENTFS_BIN`); build a release binary
first for anything timing-sensitive.

```bash
# Orchestrated Phase 8 policy gate (smoke profile is the milestone gate).
# Includes the noopen/flush/base-drift coherence harnesses as named gates.
python3 scripts/validation/phase8-validation.py --smoke --timeout 20 \
  --agentfs-bin "$PWD/target/release/agentfs" --output /tmp/vfs-val/phase8.json

# Focused standalone runs of the coherence harnesses:
# Default-on FUSE semantics coherence (no-open and no-flush legs)
python3 scripts/validation/noopen-coherence.py --agentfs-bin "$PWD/target/release/agentfs"
python3 scripts/validation/flush-coherence.py --agentfs-bin "$PWD/target/release/agentfs"

# Overlay base-drift rejection
python3 scripts/validation/external-base-mutation-coherence.py \
  --agentfs-bin "$PWD/target/release/agentfs" \
  --output /tmp/vfs-val/external-base-mutation.json
```

Shared harness helpers (binary resolution, subprocess handling with
process-group timeouts, JSON parsing) live in `scripts/validation/lib/`.
Historical one-off validators are archived under
`scripts/validation/archive/` (see its README) and are not part of any gate.

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
loose object was written. It then verifies the `agentfs run` Seatbelt
read-scoping posture: a secret file in an unallowed directory under `$HOME`
must be unreadable from inside the sandbox (permission error, no content
leak), and re-running with `--allow <dir>` must make it readable. The
generated profile itself is pinned by macOS-gated unit tests
(`cmd::run::tests::darwin_read_scoping`) that run in the CI macos-latest
workspace job; this script is the runtime check. A passing run ends with
`macOS NFS git + run read-scoping validation passed`. Unsupported platforms
or missing prerequisites exit `77`; on Linux that skip is expected, not a
failure. A release SHOULD NOT ship without a passing run of this script on
real hardware.

Beyond the script, the manual hardware run must also confirm three behaviors
the Linux toolchain cannot exercise: dynamic profile paths now travel as
Seatbelt `(param "NAME")` references with `-D NAME=value` definitions on the
`/usr/bin/sandbox-exec` command line (spot-check that a session under a
directory with spaces or quotes in its name still mounts and runs);
`/System/Volumes/Preboot` has a metadata literal so path resolution down to
the dyld cryptex root (`/System/Volumes/Preboot/Cryptexes`) can stat every
component (spot-check that dynamically linked binaries start under the
sandbox); and `agentfs run <missing-command>` must exit `127` (`126` for a
present but non-executable file), matching `agentfs exec` and the Linux run
path.
