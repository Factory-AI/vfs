# Testing Vfs

Linux is the first-tier platform: every gate below runs on Linux. macOS is
second-tier (NFS mount only); its CI job and the remaining manual
spot-checks are at the end of this document.

## The honest gate: `scripts/gate.sh`

`scripts/gate.sh` is the single developer and CI entrypoint. It fails on the
first failing command and runs, in order:

1. `cargo +nightly fmt --all -- --check`
2. `cargo +nightly clippy --workspace --all-targets -- -D warnings`
3. `cargo +nightly test --workspace`, then the workspace `--lib` tests again
   with `TMPDIR` routed through a symlink — macOS serves its temp dir through
   the `/private` symlink, so a test that canonicalizes one side of a path
   comparison passes on plain-Linux `TMPDIR` and fails only on macOS CI; the
   symlinked re-run surfaces that class on Linux
4. `cargo +nightly build --release --workspace --bins`
5. `crates/vfs-cli/tests/all.sh` with `VFS_GATE_STRICT=1` and
   `VFS_BIN` pointing at the release binary
   (a SKIP is a failure on the designated runner)
6. `scripts/validation/phase8-validation.py --smoke` — the top-level Python
   gate; it runs the noopen/flush/base-drift coherence harnesses internally
7. `scripts/validation/consistency-canon.sh` — the structural canon census
   (crate DAG, sealed transport surfaces, file-size cap, tracing-only
   logging, env-reads-at-the-config-edge, `await_holding_lock`, lock-order
   headers, docs layout, schema-DDL centralization, changelog)

Steps 1-4 are the `cargo` phase, 5 the `shell` phase, 6 the `python` phase
and 7 the `canon` phase. `gate.sh --phases shell,canon` runs a subset; with
no argument you get all four, which is what the gate means locally.

Knobs: `VFS_BIN` (defaults to `target/release/vfs`),
`VFS_GATE_SHELL_TIMEOUT` (default 900 s), `VFS_GATE_PHASE8_TIMEOUT`
(default 20 s), `VFS_GATE_ALLOWED_SKIPS` (forwarded to the shell suite,
see below), `VFS_GATE_SHARD=<index>/<total>` (1-based; runs one round-robin
slice of the shell suite), and the `CORRUPTION_TORTURE_*` variables forwarded
to the shell suite. The gate pins `TMPDIR` to a per-run scratch dir cleaned on
exit so dependency temp-file litter cannot accumulate on the host.

A shard must own a whole machine. The corruption-torture legs may not run
concurrently with another mount, which holds across CI jobs because each is
its own runner; backgrounding two shards on one host breaks it.

CI (`.github/workflows/rust.yml`) runs the workspace job (fmt/clippy/build/test
on Linux and macOS, build+test on Linux arm64), then builds the release binary
once in `gate-binary` and fans it out as an artifact to four `gate-shell`
shards and one `gate-python` job (`--phases python,canon` plus pjdfstest
`phase5-ci`). Sharing one prebuilt binary keeps the release build off every
shard's critical path, and the cargo phase is not repeated there because the
workspace job already covers fmt, clippy, and the workspace tests. A
separate `macos-runtime` job builds its own release binary and runs the
macOS runtime gates described at the end of this document.
The gate jobs first set `kernel.apparmor_restrict_unprivileged_userns=0` so
the `vfs run` suites exercise the sandbox instead of skipping on the
Ubuntu 24.04 runner image. FUSE-over-io_uring coverage stays local-only: the
CI kernel ships `/sys/module/fuse/parameters/enable_uring` disabled, so the
panic-census uring leg can never run there and the gate job allowlists that
one skip (`VFS_GATE_ALLOWED_SKIPS=fuse-sigint-panic-census`). The
`corruption-torture-uring` leg needs no entry: with uring disabled the mount
falls back to the legacy channel and the leg passes. A "starting
fuse-over-io_uring queues" log line precedes kernel acceptance of the ring
registration and does not mean uring served requests; the uring legs are
honest only on a kernel with `enable_uring=Y`.

## Workspace tests and generated-docs parity

```bash
cargo +nightly test --workspace
```

Two documentation files are generated from code and pinned by unit tests, so
doc drift fails the gate:

- `docs/KNOBS.md` — regenerate with
  `VFS_UPDATE_KNOBS=1 cargo +nightly test -p vfs-cli --lib knobs::tests::generated_knobs_doc_matches_declarations -- --exact`
- `docs/MANUAL.md` command reference — regenerate with
  `VFS_UPDATE_MANUAL=1 cargo +nightly test -p vfs-cli --lib docs::tests::manual_help_parity -- --exact`

The vfs-core history tests include a seeded randomized replay conformance
suite. Three fixed seeds each drive 320 weighted filesystem mutations across
create/write/truncate, namespace, link, metadata, and unlink-while-open paths;
checkpoints are captured every 40 operations and every retained checkpoint is
reconstructed and compared byte-for-byte across replayable live tables and
live-reachable chunks. Separate fixed cases force snapshot-covered GC
roll-forward (including typed refusal below the floor) and many batched writes
to one inode before each drain:

```bash
cargo +nightly test -p vfs-core randomized_replay_conformance
```

## Shell integration suite

```bash
VFS_GATE_STRICT=1 crates/vfs-cli/tests/all.sh
```

The suite covers init/mount/run/exec flows, syscall coverage, signal
teardown, corruption torture (both `VFS_FUSE_URING=1` and `=0` legs),
sidecar cleanup, MCP server behavior, and a `cli-smoke` pass over the whole
user-level command surface (init, run, exec, clone, fs, timeline,
backup/materialize, integrity, migrate, MCP, ps, completions, and the
deprecated `nfs`/`mcp-server` aliases), and prints a PASS/SKIP/FAIL
summary. Every test runs out of its own `mktemp -d` root with trap cleanup
and honors `VFS_BIN` (falling back to `cargo run`). In strict mode a SKIP
is red unless its test label is named in
`VFS_GATE_ALLOWED_SKIPS=<label[,label...]>`, the escape hatch for runner
kernels that cannot provide a prerequisite at all;
`VFS_GATE_FORCE_SKIP=<label|all>` synthesizes a SKIP for testing the
runner itself. Never run the corruption
torture test concurrently with another mount, test suite, or benchmark.

`test-history-revert-e2e.sh` owns the release-binary history lifecycle:
history pagination/manifests, historical branching, range and epoch refusal,
offline revert publication and recovery, KV/tool-call preservation, resumed
mutation after revert, and pack/adopt of the restored state. Run it directly
after a release build with:

```bash
VFS_GATE_STRICT=1 VFS_BIN="$PWD/target/release/vfs" \
  crates/vfs-cli/tests/test-history-revert-e2e.sh
```

`test-remote-checkpoint-e2e.sh` owns the release-binary remote tier against a
`file://` remote: offline and live-under-churn checkpoints, wire-layout and
manifest assertions, hollow-artifact containment, idempotent re-checkpoint,
delta-only re-upload, branch chain materialization, journal-off honesty, the
background streamer, failure injection against an unwritable remote, and a
full hydration round trip proving the remote holds everything needed to
reconstruct. Run it the same way, substituting its path.

`test-remote-adopt-e2e.sh` owns the lazy consumption side: adopt from the
manifest alone, lazy read and read-modify-write correctness against the
origin bytes, explicit failure (never zeros) when the remote is unreachable,
hollow containment across pack/branch/revert/checkpoint/backup,
`materialize --in-place` hydration with sidecar removal and idempotence,
pack-after-materialize closing the loop onto a third machine, adopt refusals
(missing configuration, corrupted metadata, future artifact version), and
the streamer's empty-body guard. Run it the same way, substituting its path.

Both remote suites are platform-gated rather than Linux-only: on macOS they
run over the NFS mount path (CI runs them on macos-latest), with the
live-checkpoint and streamer legs guarded to Linux because the session
control socket and the background streamer exist only in the Linux mount
owner.

## Python validation gates

All harnesses take `--vfs-bin` (or `VFS_BIN`); build a release binary
first for anything timing-sensitive.

```bash
# Orchestrated Phase 8 policy gate (smoke profile is the milestone gate).
# Includes the noopen/flush/base-drift coherence harnesses as named gates.
python3 scripts/validation/phase8-validation.py --smoke --timeout 20 \
  --vfs-bin "$PWD/target/release/vfs" --output /tmp/vfs-val/phase8.json

# Focused standalone runs of the coherence harnesses:
# Default-on FUSE semantics coherence (no-open and no-flush legs)
python3 scripts/validation/noopen-coherence.py --vfs-bin "$PWD/target/release/vfs"
python3 scripts/validation/flush-coherence.py --vfs-bin "$PWD/target/release/vfs"

# Immediate overlay base metadata/namespace coherence with bounded callback volume
python3 scripts/validation/external-base-mutation-coherence.py \
  --vfs-bin "$PWD/target/release/vfs" \
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

`VFS_PROFILE=1` makes Vfs emit `vfs_profile_summary` counter
lines on exit; most harnesses parse and attach them to their JSON reports.

## Benchmarks (local-only policy)

The chaos workload is the primary product-level performance scoreboard. The
older phase benchmark remains a focused regression instrument for identifying
which Git phase moved; it is not the current end-to-end result. Neither runs
in CI. Run every benchmark serialized on a quiet machine with a fresh release
build; single runs are noise.

Focused per-phase regression command:

```bash
cargo +nightly build --release --workspace --bins
python3 scripts/validation/git-workload-benchmark-multi.py \
  --label bench --iterations 5 --warmup 1 \
  --vfs-bin "$PWD/target/release/vfs" \
  --source <local benchmark fixture checkout> \
  --read-files 64 --read-bytes 4096 --edit-files 8 \
  --output /tmp/vfs-val/bench-multi.json --keep-iterations

# Gate rule: red = >5% relative AND >10ms absolute per-phase median regression
python3 scripts/validation/bench-compare.py <baseline-medians.json> /tmp/vfs-val/bench-multi.json
```

### The ratio is not the measurement

Read the **absolute** per-phase medians, for both legs, before you read any
`vfs / native` ratio. The two legs do not share controlled page-cache state,
so the ratio moves when the *native* denominator moves, which it does a lot.
Worked example from the two committed baselines in `.agents/benchmarks/`:

| Phase | native | vfs | ratio |
|---|---|---|---|
| status | 0.1775s → 0.0271s | 0.3418s → 0.2999s | 1.93x → 11.06x |
| diff | 0.2491s → 0.0204s | 0.2955s → 0.2751s | 1.19x → 13.50x |

Vfs got *faster* on both phases while both ratios got roughly ten times
worse. Anyone reading only the ratio column would report a severe regression
that did not happen. A ratio is only meaningful against a native median
captured under the same cache state, in the same session, with dispersion
reported next to it.

Corollary: the single-run JSONs in `.agents/benchmarks/` (`baseline-*.json`)
are profiled single shots, not medians. They are provenance records, not the
scoreboard. Do not quote them as targets.

### The benchmark refuses to guess its workload

`.agents/benchmarks/fixtures/` is gitignored, so the canonical codex fixture
is absent on CI and on every fresh clone. `git-workload-benchmark.py` used to
fall back to a generated 96x1KB fixture with a warning on stderr and still
emit a scoreboard-shaped report. It now **errors out** instead; state your
intent explicitly:

* `--source <path>` / `--remote <url>` — measure a specific repository
* nothing, with the fixture materialized — the canonical codex workload
* `--synthetic` — the toy fixture, deliberately

Every report carries `source.kind` and `source.comparable_to_scoreboard`.
Check that field before comparing two runs.

### Chaos workload benchmark

`chaos-workload-benchmark.py` runs a seeded, concurrent approximation of an
agent workload rather than a sequence of isolated Git phases. Its actors mix
Git status/diff/add/commit/branch switching, scattered edits, one-byte edits
to large base files, metadata-heavy scans and small reads, build-artifact
create/rewrite/delete churn, rename/unlink including unlink-while-open, and
`git fetch` over a hermetic loopback Git daemon.
Each Git-churn actor uses a nested checkout inside the same mount, so branch
switching cannot invalidate an editor's correctness assertion in the root
checkout. Operations that mutate the same Git control directory share a
narrow lock; Git reads and all non-Git actors remain concurrent.

```bash
python3 scripts/validation/chaos-workload-benchmark.py \
  --samples 5 --warmup 1 --seed 20250808 --actors 6 --operations 8 \
  --vfs-bin "$PWD/target/release/vfs" \
  --output /tmp/vfs-val/chaos.json
```

The default leading sample is run but discarded, so the first cold touch of
the fixture never enters the distribution. After that, each adjacent pair
contains one native and one Vfs leg. `--leg-order alternating` is the default,
balancing which engine runs first across measured pairs; fixed
`native-first` and `vfs-first` modes remain available for diagnosis. Dropping
the global page cache is neither available nor appropriate for an
unprivileged local harness, so each leg performs the same
read/status/loopback warmup before its timer starts and the payload records
that disposition. Read `absolute_wall_seconds.native` and `.vfs` first: each
reports median, p25, p75, min, max, stdev, and n. The
`derived.vfs_over_native_median` value is calculated only after those
distributions and is not a primary measurement or a gate.

The chaos report is the primary performance record. It separates two timing
boundaries that must not be conflated:

* `absolute_startup_seconds` measures parent process spawn through completion
  of the child workload's first successful `stat` on its working directory.
  This includes Vfs mount startup on the Vfs leg, but also includes the common
  Python process/import cost visible in the native leg.
* `absolute_wall_seconds` remains the concurrent workload timer. It starts
  only after nested checkout preparation and the identical per-leg cache
  warmup. Clone, mount startup, fixture copying, verification, and teardown
  are not part of this distribution.

The dedicated startup mini-benchmark below uses a minimal `python -S` probe,
so use it rather than the chaos startup field when investigating mount
workers. The chaos field exists to keep the end-to-end lifecycle cost visible
beside the primary workload result.

Each warmup or measured leg gets a fresh context with sibling `checkout/`,
`home/`, and `tmp/` trees. HOME, the Vfs session store, XDG caches, Git
configuration, and TMPDIR are therefore never shared between legs and never
live inside the host base tree being checked. A leg does not pass until its
base fingerprint is unchanged and no mount or session-bound process remains
under its context. The final report also rejects leaked `git-ai` processes.

Fixed-operation runs reproduce the same actor action plan for a given seed;
the payload carries its digest. `--duration` is available for exploratory
soak runs but is explicitly marked less reproducible because scheduling
determines the final operation count. Actor count must be at least six so
every actor class remains represented. Per-actor intensity flags multiply
the base operation count.

The fixture contract matches the Git workload benchmark: the canonical codex
fixture is used when materialized, otherwise the benchmark refuses to run
unless given `--source`, `--remote`, or the explicit non-comparable
`--synthetic`. Only the resolved canonical fixture is marked scoreboard
comparable; an arbitrary source path or remote has a different workload
identity. Real fetch egress is available only through `--fetch-url`; the
default loopback remote is deterministic, and any override also marks
`source.comparable_to_scoreboard` false.

The harness probes the engine named by `--vfs-bin` and records its version and
capabilities. It enables partial-origin copy-up and requires
`vfs integrity --check-base --checkpoint` when the engine offers them; an
older engine reports those checks as unavailable rather than pretending they
passed. Actor read-back, `git fsck --strict`, host-base immutability, and
teardown cleanliness remain mandatory for every engine.

Normal performance runs leave profiling off. `--profile` is for diagnosis and
attaches the FUSE per-operation counters emitted at teardown. For a transport
A/B, run once with `VFS_FUSE_URING=1` and once with `VFS_FUSE_URING=0`; verify
`engine.requested_fuse_transport` and `fuse_uring_requests` in the payload
before reading the times. This is a measurement instrument: it has no
performance threshold and is not part of `gate.sh` or CI. As with the
corruption-torture tests, never run it beside another mount workload.

Focused local benchmarks: `git-workload-benchmark.py` (single run with
`--profile` phase breakdown), `read-path-benchmark.py`,
`large-edit-benchmark.py` (one-byte edit to a large base file must grow the
delta DB by O(changed chunks), with `--partial-origin` / `--no-partial-origin`
legs), and `base-read-benchmark.py`.

### Clone mini-benchmark

`vfs-clone-benchmark.py` measures the complete user-visible clone commands
from one prepared local mirror:

* native: `git clone --no-hardlinks`
* Vfs: `vfs clone`

The mirror setup and post-command correctness checks are outside the timer.
Every result must have clean Git status, pass `git fsck --strict`, and match
the canonical tracked-content hash. Each leg gets a fresh HOME, TMPDIR, XDG
tree, and destination or database.

```bash
python3 scripts/validation/vfs-clone-benchmark.py \
  --samples 10 --warmup 1 \
  --vfs-bin "$PWD/target/release/vfs" \
  --output /tmp/vfs-val/clone.json
```

The fixture rules match the chaos benchmark. With no source flag, the script
uses the materialized canonical codex fixture or fails rather than silently
substituting a toy. `source.comparable_to_scoreboard` is true only for that
fixture. Read `absolute_command_seconds.native` and `.vfs`; the ratio remains
derived context.

### Mount-startup mini-benchmark

`mount-startup-benchmark.py` isolates startup from the chaos workload:

```bash
python3 scripts/validation/mount-startup-benchmark.py \
  --samples 20 --warmup 1 --transport both \
  --vfs-bin "$PWD/target/release/vfs" \
  --output /tmp/vfs-val/mount-startup.json
```

The primary metric is parent process spawn through completion of the probe's
first successful `stat("probe.txt")`. The native leg runs the same
`python -S` probe without Vfs, exposing the common process/interpreter floor.
Transport order alternates each sample. Every Vfs leg uses a fresh session and
must leave no process or mount behind.

Use `--profile` only for a separate transport-attestation run; profiling is
kept out of the final timing distribution. A profiled report requires
`fuse_uring_requests == 0` for legacy and a positive value for io_uring.

## pjdfstest

Vfs keeps three pjdfstest modes:

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
  --vfs-bin "$PWD/target/debug/vfs" \
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
# SQLite + Vfs schema invariants (exit nonzero on any failed check)
vfs integrity .vfs/my-agent.db --json

# Portable snapshot: checkpoint WAL, copy main DB, reopen, re-verify
vfs backup .vfs/my-agent.db /tmp/my-agent-backup.db --verify
```

Partial-origin overlay databases are rejected by plain `backup` because
their contents depend on an external base tree; use `backup --materialize`
or `vfs materialize` first, and audit the dependency with
`vfs integrity --require-portable --check-base`.

## macOS: second-tier platform and its gates

macOS support is explicitly second-tier: mounting uses the NFS backend only
(no FUSE, no `vfs ps`), and NFS protocol semantics are validated by cargo
protocol/unit tests on Linux. Runtime behavior runs in CI in the
`macos-runtime` job on macos-latest: it builds the release binary, puts GNU
coreutils' gnubin on PATH (the suites use `sha256sum` and `truncate`), runs
`scripts/validation/macos-nfs-git-validation.sh`, then runs
`test-remote-checkpoint-e2e.sh` and `test-remote-adopt-e2e.sh` over the NFS
mount path. `/sbin/mount_nfs` mounts the session's loopback NFS server
unprivileged, so no step needs sudo, and a suite `SKIP:` line fails the
step — a runner that cannot run these legs goes red instead of green.

The validation script runs locally the same way:

```bash
cargo +nightly build --release --workspace --bins
scripts/validation/macos-nfs-git-validation.sh \
  --vfs-bin "$PWD/target/release/vfs"
```

The harness is temp-directory scoped, initializes a fresh Vfs database,
mounts it with `vfs mount --backend nfs`, runs `git init`, `git add`,
`git commit`, and `git fsck --strict`, and verifies at least one loose
object was written. It then verifies the `vfs run` Seatbelt read-scoping
posture: a secret file in an unallowed directory under `$HOME` must be
unreadable from inside the sandbox (permission error, no content leak), and
re-running with `--allow <dir>` must make it readable. The generated profile
itself is pinned by macOS-gated unit tests
(`cmd::run::tests::darwin_read_scoping`) in the macos-latest workspace job;
this script is the runtime check. A passing run ends with
`macOS NFS git + run read-scoping validation passed`. Unsupported platforms
or missing prerequisites exit `77`; on Linux that skip is expected, not a
failure. Launching git and `/bin/bash` under the sandbox in these CI legs
also exercises the `/System/Volumes/Preboot` metadata literal that lets
path resolution reach the dyld cryptex root, which used to be a manual
spot-check.

What still needs a manual run on real hardware before a release that
advertises macOS: the rest of the shell suite (CI runs only the two remote
suites there), and two Seatbelt spot-checks the suites do not reach —
dynamic profile paths travel as Seatbelt `(param "NAME")` references with
`-D NAME=value` definitions on the `/usr/bin/sandbox-exec` command line, so
confirm a session under a directory with spaces or quotes in its name still
mounts and runs; and `vfs run <missing-command>` must exit `127` (`126` for
a present but non-executable file), matching `vfs exec` and the Linux run
path.
