#!/bin/sh
#
# CLI smoke suite (VAL-CLI-024): one fast end-to-end pass over the user-level
# command surface — init, run, exec, clone, fs, timeline, backup/materialize,
# integrity, migrate, MCP, ps, completions, and the deprecated `nfs` /
# `mcp-server` aliases. Deep per-command behavior lives in the dedicated
# suites; this gate proves every surface dispatches and succeeds.
set -eu

echo -n "TEST cli smoke... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-cli-smoke.XXXXXX")"
NFS_PID=""

cleanup() {
    if [ -n "$NFS_PID" ]; then
        kill "$NFS_PID" 2>/dev/null || true
        wait "$NFS_PID" 2>/dev/null || true
    fi
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

fail() {
    echo "FAILED: $*"
    exit 1
}

# Resolve the binary once: background legs must track the agentfs PID itself
# (backgrounding a wrapper orphans the real process on cleanup kill).
if [ -n "${AGENTFS_BIN:-}" ]; then
    BIN="$AGENTFS_BIN"
else
    cargo build --quiet --manifest-path "$CLI_DIR/Cargo.toml" || fail "could not build agentfs"
    BIN="$CLI_DIR/../../target/debug/agentfs"
fi
[ -x "$BIN" ] || fail "agentfs binary not found at $BIN"

run_agentfs() {
    "$BIN" "$@"
}

# The user's PATH may route `git` through a hook-manager shim that daemonizes
# out of test repos (library/environment.md); pin the distro binary and keep
# git config isolated.
mkdir -p "$ROOT/bin"
for candidate in /usr/bin/git /bin/git; do
    if [ -x "$candidate" ]; then
        ln -sf "$candidate" "$ROOT/bin/git"
        break
    fi
done
[ -e "$ROOT/bin/git" ] || ln -sf "$(command -v git)" "$ROOT/bin/git"
PATH="$ROOT/bin:$PATH"
export PATH
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null

cd "$ROOT"

# --- init: create, refuse duplicate, --force -----------------------------------
run_agentfs init smoke >"$ROOT/init.log" 2>&1 || fail "init failed: $(cat "$ROOT/init.log")"
DB="$ROOT/.agentfs/smoke.db"
[ -f "$DB" ] || fail "init did not create $DB"
# init's schema writes must be checkpointed away before it returns: a fresh
# init used to leave a ~119KB -wal next to the new DB (invariant I1).
[ ! -e "$DB-wal" ] || fail "init left $DB-wal behind"
[ ! -e "$DB-shm" ] || fail "init left $DB-shm behind"
if run_agentfs init smoke >"$ROOT/init-dup.log" 2>&1; then
    fail "repeated init succeeded without --force"
fi
grep -q "already exists" "$ROOT/init-dup.log" || fail "duplicate init missing refusal message"
run_agentfs init smoke --force >/dev/null 2>&1 || fail "init --force failed"
[ ! -e "$DB-wal" ] || fail "init --force left $DB-wal behind"
[ ! -e "$DB-shm" ] || fail "init --force left $DB-shm behind"

# --- init variants exit single-file too: --base (overlay) and -c (mount+command) --
mkdir -p "$ROOT/initbase"
echo base-payload >"$ROOT/initbase/base.txt"
run_agentfs init smoke-base --base "$ROOT/initbase" >"$ROOT/init-base.log" 2>&1 ||
    fail "init --base failed: $(cat "$ROOT/init-base.log")"
BASE_DB="$ROOT/.agentfs/smoke-base.db"
[ -f "$BASE_DB" ] || fail "init --base did not create $BASE_DB"
[ ! -e "$BASE_DB-wal" ] || fail "init --base left $BASE_DB-wal behind"
[ ! -e "$BASE_DB-shm" ] || fail "init --base left $BASE_DB-shm behind"
run_agentfs init smoke-cmd -c 'echo init-cmd-ok' >"$ROOT/init-cmd.log" 2>&1 ||
    fail "init -c failed: $(cat "$ROOT/init-cmd.log")"
grep -q "init-cmd-ok" "$ROOT/init-cmd.log" || fail "init -c command output missing"
CMD_DB="$ROOT/.agentfs/smoke-cmd.db"
[ -f "$CMD_DB" ] || fail "init -c did not create $CMD_DB"
[ ! -e "$CMD_DB-wal" ] || fail "init -c left $CMD_DB-wal behind"
[ ! -e "$CMD_DB-shm" ] || fail "init -c left $CMD_DB-shm behind"

# --- fs write/cat/ls ------------------------------------------------------------
run_agentfs fs "$DB" write /smoke.txt smoke-payload >/dev/null 2>&1 || fail "fs write failed"
[ "$(run_agentfs fs "$DB" cat /smoke.txt 2>/dev/null)" = "smoke-payload" ] || fail "fs cat mismatch"
run_agentfs fs "$DB" ls / >"$ROOT/ls.log" 2>&1 || fail "fs ls failed"
grep -q "smoke.txt" "$ROOT/ls.log" || fail "fs ls does not list smoke.txt"

# --- run (session sandbox) -------------------------------------------------------
run_agentfs run --session smoke-run -- sh -c 'echo run-ok' >"$ROOT/run.log" 2>&1 ||
    fail "run failed: $(cat "$ROOT/run.log")"
grep -q "run-ok" "$ROOT/run.log" || fail "run output missing"

# --- run session is audited in timeline (VAL-CROSS-006) ---------------------------
mkdir -p "$ROOT/home"
env HOME="$ROOT/home" "$BIN" run --session smoke-audit -- sh -c 'echo audit-ok' \
    >"$ROOT/run-audit.log" 2>&1 ||
    fail "audited run failed: $(cat "$ROOT/run-audit.log")"
AUDIT_DB="$ROOT/home/.agentfs/run/smoke-audit/delta.db"
[ -f "$AUDIT_DB" ] || fail "session delta.db missing at $AUDIT_DB"
# The audit reopen must finalize: a leftover WAL/SHM next to the session DB
# breaks the single-file invariant the run teardown just established.
[ ! -e "$AUDIT_DB-wal" ] || fail "run audit left $AUDIT_DB-wal behind"
[ ! -e "$AUDIT_DB-shm" ] || fail "run audit left $AUDIT_DB-shm behind"
run_agentfs timeline "$AUDIT_DB" --format json >"$ROOT/timeline-audit.json" 2>&1 ||
    fail "timeline on session DB failed: $(cat "$ROOT/timeline-audit.json")"
# Read-style reopens must restore the single-file family too.
[ ! -e "$AUDIT_DB-wal" ] || fail "timeline left $AUDIT_DB-wal behind"
[ ! -e "$AUDIT_DB-shm" ] || fail "timeline left $AUDIT_DB-shm behind"
grep -q '"name": "run"' "$ROOT/timeline-audit.json" || fail "timeline missing run audit row"
grep -q 'smoke-audit' "$ROOT/timeline-audit.json" || fail "run audit row missing session id"
grep -q '"status": "success"' "$ROOT/timeline-audit.json" || fail "run audit row not success"
grep -q 'exit_code' "$ROOT/timeline-audit.json" || fail "run audit row missing exit_code"

# --- exec (mount-owning) ----------------------------------------------------------
run_agentfs exec "$DB" sh -c 'echo exec-ok' >"$ROOT/exec.log" 2>&1 ||
    fail "exec failed: $(cat "$ROOT/exec.log")"
grep -q "exec-ok" "$ROOT/exec.log" || fail "exec output missing"

# Regression: fs-write-created files must be chmod-able inside exec (they were
# owned by uid/gid 0, so chmod as the invoking uid failed EPERM).
run_agentfs exec "$DB" sh -c 'chmod 700 smoke.txt' >"$ROOT/exec-chmod.log" 2>&1 ||
    fail "chmod of an fs-write-created file failed inside exec: $(cat "$ROOT/exec-chmod.log")"

# --- clone a local git repo into a fresh DB ----------------------------------------
mkdir -p "$ROOT/srcrepo"
(
    cd "$ROOT/srcrepo"
    git init -q
    git config user.email smoke@example.invalid
    git config user.name Smoke
    echo hello >hello.txt
    git add hello.txt
    git commit -q -m smoke
) || fail "could not build clone fixture repo"
run_agentfs clone "$ROOT/clone.db" "$ROOT/srcrepo" repo >"$ROOT/clone.log" 2>&1 ||
    fail "clone failed: $(cat "$ROOT/clone.log")"
run_agentfs fs "$ROOT/clone.db" ls / >"$ROOT/clone-ls.log" 2>&1 ||
    fail "fs ls on cloned DB failed"
grep -q "hello.txt" "$ROOT/clone-ls.log" || fail "cloned repo content missing from DB"

# --- timeline -------------------------------------------------------------------------
run_agentfs timeline "$DB" --format json >"$ROOT/timeline.log" 2>&1 ||
    fail "timeline failed: $(cat "$ROOT/timeline.log")"

# --- backup --materialize and materialize --------------------------------------------
run_agentfs backup "$DB" "$ROOT/backup.db" --verify --materialize >/dev/null 2>&1 ||
    fail "backup --verify --materialize failed"
[ -f "$ROOT/backup.db" ] || fail "backup did not create target"
run_agentfs materialize "$DB" --output "$ROOT/materialized.db" --verify >/dev/null 2>&1 ||
    fail "materialize --verify failed"
[ -f "$ROOT/materialized.db" ] || fail "materialize did not create target"

# --- integrity -------------------------------------------------------------------------
run_agentfs integrity --json "$DB" >"$ROOT/integrity.json" 2>&1 ||
    fail "integrity failed: $(cat "$ROOT/integrity.json")"

# --- one-shot commands leave a single-file database family (invariant I1) ---------------
# Census every read-style reopen: each used to leave a header-only -wal next
# to the DB after exit, which the phase8 stress gate's size==0 unlink missed.
# fs write is censused too: it drained but left a truncated 0-byte -wal.
RDB="$ROOT/readonly.db"
cp "$ROOT/backup.db" "$RDB"
no_sidecars() {
    [ ! -e "$RDB-wal" ] || fail "$1 left $RDB-wal behind"
    [ ! -e "$RDB-shm" ] || fail "$1 left $RDB-shm behind"
}
no_sidecars "cp of backup.db"
run_agentfs timeline "$RDB" >/dev/null 2>&1 || fail "timeline (read census) failed"
no_sidecars "timeline"
run_agentfs timeline "$RDB" --format json >/dev/null 2>&1 ||
    fail "timeline --format json (read census) failed"
no_sidecars "timeline --format json"
run_agentfs fs "$RDB" ls / >/dev/null 2>&1 || fail "fs ls (read census) failed"
no_sidecars "fs ls"
run_agentfs fs "$RDB" cat /smoke.txt >/dev/null 2>&1 || fail "fs cat (read census) failed"
no_sidecars "fs cat"
if run_agentfs fs "$RDB" cat /missing.txt >/dev/null 2>&1; then
    fail "fs cat of a missing file succeeded"
fi
no_sidecars "fs cat (error path)"
run_agentfs fs "$RDB" write /census-write.txt census-payload >/dev/null 2>&1 ||
    fail "fs write (census) failed"
no_sidecars "fs write"
[ "$(run_agentfs fs "$RDB" cat /census-write.txt 2>/dev/null)" = "census-payload" ] ||
    fail "fs write payload not durable after finalize"
run_agentfs diff "$RDB" >/dev/null 2>&1 || fail "diff (read census) failed"
no_sidecars "diff"
run_agentfs integrity --json "$RDB" >/dev/null 2>&1 || fail "integrity --json (read census) failed"
no_sidecars "integrity --json"
run_agentfs migrate "$RDB" >/dev/null 2>&1 || fail "migrate already-current (read census) failed"
no_sidecars "migrate (already current)"
cp "$DIR/fixtures/migrate/v0_4.db" "$ROOT/dry-run.db"
run_agentfs migrate "$ROOT/dry-run.db" --dry-run >/dev/null 2>&1 || fail "migrate --dry-run failed"
[ ! -e "$ROOT/dry-run.db-wal" ] || fail "migrate --dry-run left $ROOT/dry-run.db-wal behind"
[ ! -e "$ROOT/dry-run.db-shm" ] || fail "migrate --dry-run left $ROOT/dry-run.db-shm behind"
# sync stats errors on a non-synced local DB but must still exit single-file
run_agentfs sync "$RDB" stats >/dev/null 2>&1 || true
no_sidecars "sync stats"
run_agentfs ps >/dev/null 2>&1 || fail "ps (read census) failed"
no_sidecars "ps"

# --- migrate a committed old-schema fixture ----------------------------------------------
cp "$DIR/fixtures/migrate/v0_4.db" "$ROOT/old.db"
run_agentfs migrate "$ROOT/old.db" >"$ROOT/migrate.log" 2>&1 ||
    fail "migrate failed: $(cat "$ROOT/migrate.log")"
run_agentfs integrity --json "$ROOT/old.db" >/dev/null 2>&1 ||
    fail "integrity on migrated fixture failed"

# Regression: a nonexistent path-shaped argument must report a missing
# database, not "invalid agent ID".
if run_agentfs migrate "$ROOT/definitely/missing.db" >"$ROOT/migrate-missing.log" 2>&1; then
    fail "migrate of a missing path succeeded"
fi
grep -qi "database not found" "$ROOT/migrate-missing.log" ||
    fail "migrate missing-path error is not a not-found report: $(cat "$ROOT/migrate-missing.log")"

# --- ps ------------------------------------------------------------------------------------
run_agentfs ps >/dev/null 2>&1 || fail "ps failed"

# --- completions -----------------------------------------------------------------------------
run_agentfs completions show >"$ROOT/completions.log" 2>&1 || fail "completions show failed"

# --- MCP over stdio via the deprecated `mcp-server` alias -------------------------------------
python3 - "$BIN" "$DB" <<'PY' || fail "mcp-server alias initialize round trip failed"
import json
import subprocess
import sys

bin_path, db = sys.argv[1], sys.argv[2]
argv = [bin_path, "mcp-server", db]
proc = subprocess.Popen(
    argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
)
request = {
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "cli-smoke", "version": "0"},
    },
}
proc.stdin.write(json.dumps(request) + "\n")
proc.stdin.flush()
line = proc.stdout.readline()
proc.stdin.close()
try:
    proc.wait(timeout=30)
except subprocess.TimeoutExpired:
    proc.kill()
    raise SystemExit("mcp-server did not exit after stdin close")
reply = json.loads(line)
if reply.get("id") != 1 or "result" not in reply:
    raise SystemExit(f"unexpected initialize reply: {reply}")
PY

# --- deprecated `nfs` alias serves on an ephemeral port ------------------------------------------
"$BIN" nfs "$DB" --port 0 >"$ROOT/nfs.log" 2>&1 &
NFS_PID=$!
WAITED=0
while [ "$WAITED" -lt 40 ]; do
    if grep -q "AgentFS NFS Server" "$ROOT/nfs.log" 2>/dev/null; then
        break
    fi
    kill -0 "$NFS_PID" 2>/dev/null || fail "nfs alias exited early: $(cat "$ROOT/nfs.log")"
    sleep 0.25
    WAITED=$((WAITED + 1))
done
grep -q "AgentFS NFS Server" "$ROOT/nfs.log" || fail "nfs alias never reported startup"
kill "$NFS_PID" 2>/dev/null || true
wait "$NFS_PID" 2>/dev/null || true
NFS_PID=""

echo "OK"
