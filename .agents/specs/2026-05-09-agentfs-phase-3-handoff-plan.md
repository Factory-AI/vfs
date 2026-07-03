# AgentFS Phase 3 Quick Wins: Handoff Plan

**Status:** Ready for implementation delegation  
**Ground truth:** `.agents/specs/2026-05-09-agentfs-phase-3-quick-wins.md`  
**Stop point:** Delegate from this document; do not start Phase 4 schema/write-path work in this slice.

## Scope

Implement only the approved Phase 3 quick wins:

1. Configurable Rust `ConnectionPool` width for file-backed databases.
2. Per-new-connection setup pragmas: WAL, `synchronous = NORMAL`, `busy_timeout = 5000`.
3. `fsync` restores the durable baseline (`NORMAL`) instead of `OFF`.
4. Hot `tool_calls` statements use cached prepares where supported.
5. macOS NFS mount options include explicit `wsize` / `rsize`.
6. Add focused Rust tests for pool behavior, pragma setup, and concurrent access.
7. Run validators from the validation section below.

Do **not** implement inline files, variable chunk sizes, migration tooling, chunk-granularity copy-up, sidecar `tool_calls` databases, Turso upgrades, or FSKit work.

## Current code map

- `sdk/rust/src/connection_pool.rs`
  - Previously had `const MAX_CONNECTIONS: usize = 1`.
  - Keep raw `ConnectionPool::new` conservative/single-connection for standalone `:memory:` safety; use explicit file-backed options where widening is safe.
  - New connections are created in `get_connection`; this is the correct hook for per-connection setup SQL.

- `sdk/rust/src/filesystem/agentfs.rs`
  - `DEFAULT_CHUNK_SIZE = 4096`; do not change in this phase.
  - `AgentFS::from_pool` currently initializes schema, sets `PRAGMA synchronous = OFF`, and sets `busy_timeout`.
  - `AgentFSFile::fsync` temporarily switches to `FULL`, commits, then restores `OFF`; restore `NORMAL` instead.

- `sdk/rust/src/lib.rs`
  - `AgentFS::open` creates the local `turso::Database` and wraps it in `ConnectionPool::new`.
  - `open_with_pool` shares one pool across KV, FS, and Tools; preserve this one-file database invariant.
  - Preserve `:memory:` serialization unless Turso proves shared in-memory state across connections.

- `sdk/rust/src/toolcalls.rs`
  - Several repeated queries still use `prepare` or `conn.query`; convert stable hot statements to `prepare_cached` where the API supports it.
  - Do not split `tool_calls` into another database in this pass.

- `cli/src/mount/nfs.rs`, `cli/src/cmd/mount.rs`, `cli/src/cmd/run_darwin.rs`
  - macOS mount option strings need explicit `wsize` / `rsize` so foreground, daemon, and Darwin run paths do not drift.

- Validators
  - Rust CI runs `cargo fmt -- --check`, `cargo clippy -- -D warnings`, `cargo check --all-features`, and `cargo test --verbose` in both `sdk/rust` and `cli`.
  - `cli/tests/all.sh` exists but some integration tests are allowed to fail internally due host prerequisites; run if feasible after Rust unit validators.

## Implementation sequence

1. **Pool options**
   - Introduce a small public `ConnectionPoolOptions` with defaults.
   - Defaults:
     - file-backed local DB: `max_connections = 8`
     - sync DB: keep conservative unless verified safe; prefer `1` initially.
     - `:memory:` DB: `1`
   - Add constructors:
     - `ConnectionPool::new(db)` keeps existing caller ergonomics and stays single-connection for safe standalone `:memory:` use.
     - `ConnectionPool::new_single_connection(db)` or equivalent for tests / in-memory safety.
     - `ConnectionPool::with_options(db, options)` for explicit configuration.
   - Store setup SQL in the pool inner state and apply it after every newly-created connection.

2. **Pragma setup**
   - Define a small canonical pragma list for local file-backed FS databases:
     - `PRAGMA journal_mode = WAL`
     - `PRAGMA synchronous = NORMAL`
     - `PRAGMA busy_timeout = 5000`
   - Ensure every new connection runs the setup list.
   - Keep `from_pool` schema initialization, but remove the durability downgrade to `OFF`.
   - Do not rely on one startup connection for connection-scoped pragmas.

3. **AgentFS open routing**
   - In `sdk/rust/src/lib.rs`, choose pool options after resolving `db_path`.
   - If `db_path == ":memory:"`, use single connection.
   - If file-backed local DB, use Phase 3 local defaults and setup pragmas.
   - Keep sync DB behavior conservative; do not broaden sync connections unless tests explicitly cover it.

4. **Fsync**
   - Change the restore pragma after the forced commit from `OFF` to `NORMAL`.
   - Add/adjust a test so a future change back to `OFF` is caught.

5. **Tool calls**
   - Convert stable hot statements in `start`, `success`, `record`, `error`, `get`, `recent`, `stats_for`, and `stats` to `prepare_cached` where practical.
   - Preserve observable behavior and schema.
   - Do not enforce the spec’s insert-only ideal yet; existing APIs update pending records and that behavioral change is out of scope.

6. **macOS NFS tuning**
   - Add `wsize=1048576,rsize=1048576` to the macOS option string in `cli/src/mount/nfs.rs`.
   - Keep existing options (`locallocks`, `vers=3`, `tcp`, ports, `soft`, `timeo`, `retrans`).
   - Linux NFS mount string may remain unchanged unless a compile/test issue requires shared formatting.

7. **Tests**
   - Update connection pool tests that assume one max connection.
   - Add a test for explicit single-connection mode preserving old timeout behavior.
   - Add a file-backed multi-connection test that opens multiple pooled connections and performs concurrent reads.
   - Add a pragma test for `synchronous` baseline and `busy_timeout` on at least one newly-created pooled connection.
   - Add/adjust an `fsync` test to assert the connection returns to `NORMAL`.

8. **Validation**
   - Run worktree pre-check first.
   - Run all Rust validators listed below.
   - Fix failures and rerun until clean.

## Atomic delegation packets

### Worker A: Pool and pragma implementation

**Goal:** Implement configurable Rust connection pooling and per-new-connection pragma setup.

**Files:** `sdk/rust/src/connection_pool.rs`, `sdk/rust/src/filesystem/agentfs.rs`, `sdk/rust/src/lib.rs`

**Constraints:**
- Preserve one-file AgentFS database semantics.
- Keep `:memory:` single-connection safe.
- Do not change schema or chunk size.
- Per-connection setup must run only when a new connection is created, not every checkout from the idle pool.

**Expected output:** Patch summary, changed APIs, tests added/updated, and any Turso pragma limitations encountered.

### Worker B: Toolcalls and macOS NFS patch

**Goal:** Apply the low-blast-radius statement-cache and mount-option quick wins.

**Files:** `sdk/rust/src/toolcalls.rs`, `cli/src/mount/nfs.rs`, `cli/src/cmd/mount.rs`, `cli/src/cmd/run_darwin.rs`

**Constraints:**
- Do not split `tool_calls` into a sidecar DB.
- Do not change `ToolCalls` public behavior.
- Keep NFS changes scoped to the macOS mount option string.

**Expected output:** Patch summary and compile-impact notes.

### Worker C: Test and validator hardening

**Goal:** Add/adjust tests proving the Phase 3 invariants and run validators.

**Files:** primarily `sdk/rust/src/connection_pool.rs`, `sdk/rust/src/filesystem/agentfs.rs`, and any existing affected test modules.

**Invariants to prove:**
- File-backed pools can use more than one connection.
- Explicit single-connection mode still times out under contention.
- New connections receive the baseline pragmas.
- `fsync` restores `synchronous = NORMAL`.

**Expected output:** Test list, validator checklist, failures fixed or blockers with exact commands/output.

### Reviewer: Feature/code review

**Goal:** Review the final patch for correctness, race risks, and spec conformance.

**Review focus:**
- Connection setup is per new connection and cannot be skipped.
- In-memory DBs do not accidentally create isolated databases per pooled connection.
- No Phase 4 schema or migration work slipped in.
- Tool call changes are behavior-preserving.
- macOS NFS option string remains syntactically valid.
- Validators match the checklist below.

## Validation checklist

Before validation, run:

```bash
MAIN_REPO=$(git worktree list | head -1 | awk '{print $1}')
[ "$(git rev-parse --show-toplevel)" != "$MAIN_REPO" ] && echo "WORKTREE -- follow worktree-setup before validators"
```

If it prints `WORKTREE`, run the `worktree-setup` repair/verify commands before any validator.

Then run:

```bash
cd /home/ain3sh/factory/vfs/sdk/rust && cargo fmt -- --check
cd /home/ain3sh/factory/vfs/sdk/rust && cargo clippy -- -D warnings
cd /home/ain3sh/factory/vfs/sdk/rust && cargo check --all-features
cd /home/ain3sh/factory/vfs/sdk/rust && cargo test --verbose

cd /home/ain3sh/factory/vfs/cli && cargo fmt -- --check
cd /home/ain3sh/factory/vfs/cli && cargo clippy -- -D warnings
cd /home/ain3sh/factory/vfs/cli && cargo check --all-features
cd /home/ain3sh/factory/vfs/cli && cargo test --verbose
```

Run `cd /home/ain3sh/factory/vfs/cli && tests/all.sh` only if host prerequisites are available; record if skipped and why.

Final quality-ship checklist to report:

```text
quality-ship checklist:
- worktree:  <main | repaired> (evidence)
- format:    <ran | no signal> (evidence)
- lint:      <ran | no signal> (evidence)
- dead-code: <no signal for Rust-only changes>
- ai-slop:   <no JS/TS changes>
- typecheck: <cargo check commands>
- tests:     <cargo test commands and optional cli/tests/all.sh status>
```

## Handoff stop condition

This document is complete when an implementer can pick up Worker A/B/C independently and a reviewer can audit the combined patch against the approved spec without re-reading the original issue narrative. At that point, stop and hand off/delegate.
