# Session Tail

## user


## assistant
# Read-Only Phase 4 Test/Validator Review

## Actions Taken

- Read the Phase 4 north-star spec, README, Phase 0-3 validation scripts, `cli/tests/all.sh`, changed SDK tests, migration/FUSE code, and Rust CI workflow.
- Inspected current `git status`, recent commit history, branch change list, and untracked files.
- Did **not** edit files or run validators.

## Blockers

**No Blocker findings.**

## Findings

### Major — FUSE integration tests can be masked by `|| true`

- **File/lines:** `cli/tests/all.sh:18-21`, `cli/tests/all.sh:33`
- **Issue:** Several FUSE/`agentfs run` integration tests are allowed to fail unconditionally. That prevents CI from distinguishing host-prerequisite skips from real regressions.
- **Suggested fix:** Move prerequisite detection into each test and remove blanket `|| true`, or only ignore an explicit skip code.

### Major — Performance baseline is a report, not a gate

- **File/lines:** `scripts/validation/workload-baseline.py:470-511`, `scripts/validation/workload-baseline.py:557-560`
- **Issue:** The baseline harness reports ratio/equivalence but has no threshold enforcement for the Phase 4 target. In command mode, output equivalence is unchecked unless `--compare-stdout` is provided.
- **Suggested fix:** Add/require an explicit ratio threshold gate for the agreed factory-mono workload and require deterministic equivalence checks where possible.

### Major — Replay harness can pass while skipping unsupported operations

- **File/lines:** `scripts/validation/replay/replay_workload.py:520-528`, `scripts/validation/replay/replay_workload.py:669-710`
- **Issue:** Unsupported operations are summarized and skipped, but not fatal. This is okay for smoke coverage, but weak as a replay correctness gate.
- **Suggested fix:** Add a strict mode such as `--fail-on-unsupported`, and use it for required replay gate traces.

### Major — FUSE coalescer has unit coverage but limited end-to-end gate coverage

- **File/lines:** `cli/src/fuse.rs:1089-1215`, `cli/src/fuse.rs:1458-1523`
- **Issue:** `WriteBuffer` merge behavior is unit-tested, and existing append/git tests indirectly exercise FUSE writes, but there is no explicit end-to-end test asserting buffered write visibility/ordering across `read`, `flush`, `fsync`, `release`, and truncate boundaries.
- **Suggested fix:** Add a CLI/FUSE integration test that writes multiple ranges, reads before close, fsyncs, truncates, releases, and reopens to verify persisted content.

### Minor — Inline storage coverage is good for happy paths, thin on edge transitions

- **File/lines:** `sdk/rust/tests/snapshot_restore.rs:96-109`, `sdk/rust/tests/snapshot_restore.rs:225-230`, `sdk/rust/tests/snapshot_restore.rs:336-356`, `cli/src/cmd/migrate.rs:1380-1460`
- **Issue:** Tests cover inline files, chunk-boundary files, sparse files, migration re-chunking, source preservation, and invariants. Missing edge-focused coverage for exact `4096`/`4097` threshold and chunked→inline truncate transition.
- **Suggested fix:** Add targeted SDK tests for threshold boundaries and truncate transition.

### Minor — CI replay smoke has FUSE platform risk

- **File/lines:** `.github/workflows/rust.yml:72-81`
- **Issue:** Replay smoke directly mounts AgentFS and does not handle skip code like the pjdfstest step does. If GitHub Ubuntu FUSE prerequisites change, this can become a platform false failure.
- **Suggested fix:** Either make replay return/use a skip code for missing FUSE prerequisites, or wrap the workflow step like the pjdfstest step.

### Minor — Generated/session artifacts present in working tree

- **Files:**
  - `.agents/05_09_2026/*_tail.md`
  - `.agents/specs/2026-05-10-agentfs-phase-4-north-star.md`
  - `.agents/specs/2026-05-10-next-step-after-phase-0-3.md`
- **Issue:** `.agents/05_09_2026/*_tail.md` look like generated session artifacts and should not be committed. Spec files may be intentional, but are untracked and should be explicitly decided.
- **Suggested fix:** Exclude/remove generated tail files before commit; intentionally add only wanted specs.

## Coverage Assessment

- **Migration:** Present, mostly right layer. Unit test covers v0.4→v0.5 copy migration, source preservation, re-chunking, inline conversion, sparse data, whiteout/origin, KV, tool calls.
- **Inline storage:** Present in SDK snapshot/concurrency tests and migration test; edge transitions need more coverage.
- **FUSE coalescer:** Unit-tested for range merge logic; needs stronger end-to-end FUSE flush/visibility/order coverage.
- **Snapshot/restore:** Present and strong for main-db copy after checkpoint, KV, tool calls, hardlinks, symlinks, sparse/chunked/inline files.
- **Concurrency/integrity:** Present in SDK and CLI torture smoke.
- **Torture:** Present; short CI parameters are reasonable, but skip/pass visibility could improve.
- **Replay:** Present as smoke; not strict enough as a correctness gate.
- **Baseline/performance:** Harness present; gate enforcement absent.

## Recommended Validator Commands

```bash
cd /home/ain3sh/factory/vfs/cli
cargo +nightly fmt -- --check
cargo +nightly clippy -- -D warnings
cargo +nightly build --verbose
cargo +nightly test --verbose
cargo +nightly check --all-features
cargo +nightly check --no-default-features
tests/all.sh
```

```bash
cd /home/ain3sh/factory/vfs/sdk/rust
cargo +nightly fmt -- --check
cargo +nightly clippy -- -D warnings
cargo +nightly build --verbose
cargo +nightly test --verbose
cargo +nightly check --all-features
```

```bash
cd /home/ain3sh/factory/vfs
scripts/validation/phase0.sh
```

```bash
cd /home/ain3sh/factory/vfs
cat > /tmp/agentfs-replay-smoke.jsonl <<'EOF'
{"op":"mkdir","path":"/app"}
{"op":"write_file","path":"/app/hello.txt","content":"hello"}
{"op":"read_file","path":"/app/hello.txt"}
{"op":"stat","path":"/app/hello.txt"}
EOF
scripts/validation/replay/replay_workload.py --agentfs-bin cli/target/debug/agentfs /tmp/agentfs-replay-smoke.jsonl
```

```bash
cd /home/ain3sh/factory/vfs
scripts/validation/posix/run-pjdfstest.sh --agentfs-bin cli/target/debug/agentfs
```

```bash
cd /home/ain3sh/factory/vfs
AGENTFS_BIN=cli/target/debug/agentfs \
  scripts/validation/workload-baseline.py \
  --source /path/to/factory-mono \
  --command 'AGREED_REPRESENTATIVE_COMMAND' \
  --iterations 3 \
  --timeout 300
```

## Files Written

None.
