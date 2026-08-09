#!/bin/sh
#
# Replayable-history CLI end to end: history pagination, historical branch,
# crash-safe offline revert, live/range/boundary/epoch refusals, post-revert
# journaling, and pack/adopt of the reverted state.
#
set -eu

echo -n "TEST history branch-to revert e2e... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
VFS_BIN="${VFS_BIN:-}"
TEST_ROOT=""
LIVE_PID=""

PARENT_ID="history-parent-$$"
HISTORICAL_BRANCH_ID="history-mid-$$"
CURRENT_BRANCH_ID="history-current-$$"
INVALID_ID="history-invalid-$$"
ADOPT_ID="history-adopt-$$"

skip() {
    echo "SKIP: $*"
    exit 0
}

fail() {
    echo "FAILED: $*"
    exit 1
}

wait_pid_exit() {
    pid="$1"
    deadline=$(( $(date +%s) + 10 ))
    while kill -0 "$pid" 2>/dev/null; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.1
    done
}

case "$(uname -s)" in
    Linux) ;;
    *) skip "requires Linux namespaces and FUSE" ;;
esac

[ -n "$VFS_BIN" ] || command -v cargo >/dev/null 2>&1 ||
    skip "cargo is unavailable and VFS_BIN is unset"
command -v python3 >/dev/null 2>&1 || skip "python3 is unavailable"
command -v git >/dev/null 2>&1 || skip "git is unavailable"
[ -x /bin/bash ] || skip "/bin/bash is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"

if [ -r /proc/sys/kernel/unprivileged_userns_clone ] &&
    [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
    skip "unprivileged user namespaces are disabled"
fi

if [ -z "$VFS_BIN" ]; then
    cargo +nightly build --quiet --manifest-path "$CLI_DIR/Cargo.toml" >/dev/null 2>&1 ||
        fail "failed to build vfs CLI"
    VFS_BIN="$REPO_ROOT/target/debug/vfs"
fi
[ -x "$VFS_BIN" ] || fail "VFS_BIN is not executable: $VFS_BIN"

cleanup() {
    set +e
    if [ -n "$LIVE_PID" ] && kill -0 "$LIVE_PID" 2>/dev/null; then
        kill -TERM "$LIVE_PID" 2>/dev/null || true
        wait_pid_exit "$LIVE_PID" || kill -KILL "$LIVE_PID" 2>/dev/null || true
        wait "$LIVE_PID" 2>/dev/null || true
    fi
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/vfs-history-revert.*)
                chmod -R u+w "$TEST_ROOT" 2>/dev/null || true
                rm -rf "$TEST_ROOT"
                ;;
            *) echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT" ;;
        esac
    fi
}
trap cleanup EXIT INT TERM

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vfs-history-revert.XXXXXX")"
TEST_HOME="$TEST_ROOT/home"
ADOPT_HOME="$TEST_ROOT/adopt-home"
BASE="$TEST_ROOT/base"
mkdir -p \
    "$TEST_HOME/.cache" "$TEST_HOME/.config" \
    "$ADOPT_HOME/.cache" "$ADOPT_HOME/.config" \
    "$BASE"

printf "base\n" >"$BASE/base.txt"
(
    cd "$BASE"
    git init -q
    git -c user.email=history@example.invalid \
        -c user.name="Vfs History" add .
    git -c user.email=history@example.invalid \
        -c user.name="Vfs History" commit -qm base
) || fail "failed to initialize base git checkout"
BASE_PIN="$(git -C "$BASE" rev-parse HEAD)" ||
    fail "failed to resolve base pin"

run_vfs() {
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" "$@"
    )
}

run_vfs_without_journal() {
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        VFS_JOURNAL=0 \
        "$VFS_BIN" "$@"
    )
}

run_adopted() {
    (
        cd "$BASE"
        HOME="$ADOPT_HOME" \
        XDG_CACHE_HOME="$ADOPT_HOME/.cache" \
        XDG_CONFIG_HOME="$ADOPT_HOME/.config" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" "$@"
    )
}

# Establish a known intermediate state and record its complete history head.
run_vfs run --session "$PARENT_ID" --seed-pin "$BASE_PIN" -- /bin/bash -c '
set -e
printf "state at target\n" > keep.txt
' || fail "initial parent mutation failed"

MID_HISTORY="$TEST_ROOT/history-mid.json"
run_vfs history "$PARENT_ID" --all --json >"$MID_HISTORY" ||
    fail "history after initial mutation failed"
MID_SEQ="$(python3 - "$MID_HISTORY" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
assert m["manifestVersion"] == 1, m
assert m["historyValid"] is True, m
assert m["historyFloorSeq"] <= m["historyHeadSeq"], m
assert m["targets"], m
print(m["historyHeadSeq"])
PY
)" || fail "failed to select intermediate history target"

# Add namespace and content mutations after the target.
run_vfs run --session "$PARENT_ID" -- /bin/bash -c '
set -e
/bin/mkdir later-dir
printf "delete later\n" > disposable.txt
/bin/mv keep.txt renamed.txt
/bin/rm disposable.txt
printf "future\n" > future.txt
' || fail "later parent mutations failed"

HISTORY_JSON="$TEST_ROOT/history.json"
run_vfs history "$PARENT_ID" --all --json >"$HISTORY_JSON" ||
    fail "full history command failed"
python3 - "$HISTORY_JSON" "$MID_SEQ" <<'PY' || fail "history manifest contract failed"
import json, sys
m = json.load(open(sys.argv[1]))
assert set(m) == {
    "manifestVersion", "sessionId", "historyEpoch", "historyValid",
    "historyFloorSeq", "historyHeadSeq", "targets",
}, m
assert m["historyValid"] is True, m
assert m["historyFloorSeq"] <= int(sys.argv[2]) < m["historyHeadSeq"], m
targets = m["targets"]
assert targets == sorted(targets, key=lambda target: target["seq"], reverse=True), targets
labels = {target["label"] for target in targets}
assert {"create_file", "write", "mkdir", "rename", "unlink"} <= labels, sorted(labels)
for target in targets:
    assert set(target) == {
        "seq", "txnId", "label", "wallclockMs", "tables", "rows",
    }, target
    assert target["rows"] > 0, target
    assert target["tables"] == sorted(set(target["tables"])), target
PY

run_vfs history "$PARENT_ID" --limit 2 --json >"$TEST_ROOT/history-limit.json" ||
    fail "limited history command failed"
python3 - "$TEST_ROOT/history-limit.json" <<'PY' || fail "history limit was not enforced"
import json, sys
targets = json.load(open(sys.argv[1]))["targets"]
assert len(targets) == 2, targets
assert targets[0]["seq"] > targets[1]["seq"], targets
PY
run_vfs history "$PARENT_ID" --limit 1 >"$TEST_ROOT/history-human.txt" ||
    fail "human history command failed"
grep -q "History: epoch .* valid, available" "$TEST_ROOT/history-human.txt" ||
    fail "human history omitted range/epoch header"
grep -q "^seq " "$TEST_ROOT/history-human.txt" ||
    fail "human history omitted transaction rows"

# Historical branch sees exactly the target state.
run_vfs branch "$PARENT_ID" --session "$HISTORICAL_BRANCH_ID" --to "$MID_SEQ" \
    >"$TEST_ROOT/branch-mid.json" ||
    fail "historical branch failed"
python3 - "$TEST_ROOT/branch-mid.json" "$MID_SEQ" <<'PY' || fail "historical branch manifest failed"
import json, sys
m = json.load(open(sys.argv[1]))
assert m["targetSeq"] == int(sys.argv[2]), m
assert m["sourceHeadSeq"] > m["targetSeq"], m
assert m["rootSnapshotSeq"] <= m["targetSeq"], m
PY
run_vfs run --session "$HISTORICAL_BRANCH_ID" -- /bin/bash -c '
set -e
test "$(/bin/cat keep.txt)" = "state at target"
test ! -e renamed.txt
test ! -e future.txt
test ! -e later-dir
' || fail "historical branch did not serve target state"

# Plain branch remains current-state behavior and omits historical fields.
run_vfs branch "$PARENT_ID" --session "$CURRENT_BRANCH_ID" \
    >"$TEST_ROOT/branch-current.json" ||
    fail "plain branch failed"
python3 - "$TEST_ROOT/branch-current.json" <<'PY' || fail "plain branch manifest changed"
import json, sys
m = json.load(open(sys.argv[1]))
for field in ("targetSeq", "sourceHeadSeq", "rootSnapshotSeq"):
    assert field not in m, m
PY
run_vfs run --session "$CURRENT_BRANCH_ID" -- /bin/bash -c '
set -e
test "$(/bin/cat renamed.txt)" = "state at target"
test "$(/bin/cat future.txt)" = future
test -d later-dir
test ! -e keep.txt
' || fail "plain branch did not serve current state"

# Reject out-of-range and mid-transaction targets with actionable ranges.
HEAD_SEQ="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["historyHeadSeq"])' "$HISTORY_JSON")"
MID_TXN_SEQ="$(python3 - "$HISTORY_JSON" <<'PY'
import json, sys
for target in json.load(open(sys.argv[1]))["targets"]:
    if target["rows"] > 1 and target["txnId"] < target["seq"]:
        print(target["txnId"])
        break
else:
    raise SystemExit("no multi-row transaction found")
PY
)" || fail "failed to find a mid-transaction sequence"

set +e
run_vfs revert "$PARENT_ID" --to "$((HEAD_SEQ + 1))" --json \
    >"$TEST_ROOT/range.out" 2>"$TEST_ROOT/range.err"
RANGE_CODE=$?
run_vfs revert "$PARENT_ID" --to "$MID_TXN_SEQ" --json \
    >"$TEST_ROOT/mid-txn.out" 2>"$TEST_ROOT/mid-txn.err"
MID_TXN_CODE=$?
run_vfs revert "missing-history-$$" --to 0 --json \
    >"$TEST_ROOT/missing.out" 2>"$TEST_ROOT/missing.err"
MISSING_CODE=$?
set -e
[ "$RANGE_CODE" -ne 0 ] || fail "out-of-range revert unexpectedly succeeded"
[ "$MID_TXN_CODE" -ne 0 ] || fail "mid-transaction revert unexpectedly succeeded"
[ "$MISSING_CODE" -eq 5 ] ||
    fail "missing revert session returned $MISSING_CODE instead of 5"
grep -q "available range" "$TEST_ROOT/range.err" ||
    fail "out-of-range error omitted available range"
grep -q "inside transaction" "$TEST_ROOT/mid-txn.err" ||
    fail "mid-transaction error was not actionable"
grep -q "available range" "$TEST_ROOT/mid-txn.err" ||
    fail "mid-transaction error omitted available range"

run_vfs_without_journal run --session "$INVALID_ID" --seed-pin "$BASE_PIN" -- \
    /bin/bash -c 'printf "gap\n" > gap.txt' ||
    fail "failed to create invalid history epoch"
set +e
run_vfs_without_journal revert "$INVALID_ID" --to 0 --json \
    >"$TEST_ROOT/invalid.out" 2>"$TEST_ROOT/invalid.err"
INVALID_CODE=$?
set -e
[ "$INVALID_CODE" -ne 0 ] || fail "invalid-epoch revert unexpectedly succeeded"
grep -q "not replayable" "$TEST_ROOT/invalid.err" ||
    fail "invalid-epoch error was not actionable"
grep -q "available range" "$TEST_ROOT/invalid.err" ||
    fail "invalid-epoch error omitted available range"

# Non-filesystem tables survive the destructive filesystem rewind.
PARENT_DB="$TEST_HOME/.vfs/run/$PARENT_ID/delta.db"
python3 - "$PARENT_DB" <<'PY' || fail "failed to seed non-filesystem rows"
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.execute("INSERT INTO kv_store(key, value) VALUES ('preserve', 'kv')")
db.execute(
    """INSERT INTO tool_calls(name, parameters, result, status, started_at)
       VALUES ('preserve', '{}', 'ok', 'success', 1)"""
)
db.commit()
db.close()
PY

run_vfs revert "$PARENT_ID" --to "$MID_SEQ" --json >"$TEST_ROOT/revert.json" ||
    fail "offline revert failed"
python3 - "$TEST_ROOT/revert.json" "$PARENT_DB" "$MID_SEQ" "$TEST_ROOT/revert-floor.txt" <<'PY' || fail "revert manifest or publication state failed"
import json, sqlite3, sys
m = json.load(open(sys.argv[1]))
assert set(m) == {
    "manifestVersion", "sessionId", "targetSeq", "sourceHeadSeq",
    "rootSnapshotSeq", "historyEpoch", "generation", "dbPath",
}, m
assert m["targetSeq"] == int(sys.argv[3]), m
assert m["sourceHeadSeq"] > m["targetSeq"], m
assert m["rootSnapshotSeq"] <= m["targetSeq"], m
assert m["generation"] == 1, m
assert m["dbPath"] == sys.argv[2], m
assert not __import__("os").path.exists(sys.argv[2] + "-wal")
assert not __import__("os").path.exists(sys.argv[2] + "-shm")
db = sqlite3.connect(f"file:{sys.argv[2]}?mode=ro", uri=True)
assert db.execute(
    "SELECT value FROM kv_store WHERE key = 'preserve'"
).fetchone() == ("kv",)
assert db.execute(
    "SELECT name, result, status FROM tool_calls WHERE name = 'preserve'"
).fetchone() == ("preserve", "ok", "success")
floor = int(db.execute(
    "SELECT value FROM fs_config WHERE key = 'history_floor_seq'"
).fetchone()[0])
head = db.execute("SELECT COALESCE(MAX(seq), ? ) FROM fs_op_journal", (floor,)).fetchone()[0]
assert floor >= int(sys.argv[3]) and floor == head, (floor, head)
assert db.execute(
    "SELECT reason, through_seq FROM fs_snapshot"
).fetchall() == [("revert", floor)]
open(sys.argv[4], "w").write(str(floor))
db.close()
PY
run_vfs run --session "$PARENT_ID" -- /bin/bash -c '
set -e
test "$(/bin/cat keep.txt)" = "state at target"
test ! -e renamed.txt
test ! -e future.txt
test ! -e later-dir
' || fail "reverted parent did not serve target state"

# A live owner holds the shared lock, so revert must use the reserved code 3.
(
    cd "$BASE" || exit 1
    exec env \
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" run --session "$PARENT_ID" -- \
        /bin/bash -c 'while :; do sleep 0.1; done'
) >"$TEST_ROOT/live.log" 2>&1 &
LIVE_PID=$!
deadline=$(( $(date +%s) + 15 ))
until run_vfs status "$PARENT_ID" --json 2>/dev/null | grep -q '"state":"live"'; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "parent never became live"
    kill -0 "$LIVE_PID" 2>/dev/null || fail "live parent exited prematurely"
    sleep 0.2
done
set +e
run_vfs revert "$PARENT_ID" --to "$MID_SEQ" --json \
    >"$TEST_ROOT/live-revert.out" 2>"$TEST_ROOT/live-revert.err"
LIVE_REVERT_CODE=$?
set -e
[ "$LIVE_REVERT_CODE" -eq 3 ] ||
    fail "live revert returned $LIVE_REVERT_CODE instead of 3"
grep -q "session still running" "$TEST_ROOT/live-revert.err" ||
    fail "live revert error omitted teardown guidance"
kill -TERM "$LIVE_PID" 2>/dev/null || true
wait_pid_exit "$LIVE_PID" || kill -KILL "$LIVE_PID" 2>/dev/null || true
wait "$LIVE_PID" 2>/dev/null || true
LIVE_PID=""

# New mutations continue at target+1 and remain replayable.
run_vfs run --session "$PARENT_ID" -- /bin/bash -c '
printf "after revert\n" > post-revert.txt
' || fail "post-revert mutation failed"
run_vfs history "$PARENT_ID" --all --json >"$TEST_ROOT/post-history.json" ||
    fail "history after revert failed"
REVERT_FLOOR="$(cat "$TEST_ROOT/revert-floor.txt")"
python3 - "$TEST_ROOT/post-history.json" "$REVERT_FLOOR" <<'PY' || fail "post-revert history range is inconsistent"
import json, sys
m = json.load(open(sys.argv[1]))
assert m["historyValid"] is True, m
assert m["historyFloorSeq"] == int(sys.argv[2]), m
assert m["historyHeadSeq"] > m["historyFloorSeq"], m
assert m["targets"], m
assert min(target["seq"] for target in m["targets"]) > m["historyFloorSeq"], m
PY

# Pack/adopt transfers the reverted timeline and visible state.
PACKED="$TEST_ROOT/reverted-packed.db"
run_vfs pack "$PARENT_ID" --output "$PACKED" --json >"$TEST_ROOT/pack.json" ||
    fail "pack after revert failed"
run_adopted adopt "$ADOPT_ID" --db "$PACKED" --base "$BASE" --json \
    >"$TEST_ROOT/adopt.json" ||
    fail "adopt of reverted pack failed"
run_adopted run --session "$ADOPT_ID" -- /bin/bash -c '
set -e
test "$(/bin/cat keep.txt)" = "state at target"
test "$(/bin/cat post-revert.txt)" = "after revert"
test ! -e future.txt
' || fail "adopted reverted pack served the wrong state"

# Resume recovery restores a deterministic revert backup and removes an
# orphaned candidate before opening the session database.
ADOPT_DB="$ADOPT_HOME/.vfs/run/$ADOPT_ID/delta.db"
ADOPT_DIR="$ADOPT_HOME/.vfs/run/$ADOPT_ID"
cp "$ADOPT_DB" "$ADOPT_DIR/.delta.db.revert-orphan.tmp"
mv "$ADOPT_DB" "$ADOPT_DIR/delta.db.revert-backup"
run_adopted run --session "$ADOPT_ID" -- /bin/bash -c '
test "$(/bin/cat post-revert.txt)" = "after revert"
' || fail "run resume did not recover interrupted revert publication"
[ -f "$ADOPT_DB" ] || fail "revert recovery did not restore delta.db"
[ ! -e "$ADOPT_DIR/delta.db.revert-backup" ] ||
    fail "revert recovery left its backup behind"
[ ! -e "$ADOPT_DIR/.delta.db.revert-orphan.tmp" ] ||
    fail "revert recovery left its candidate behind"

echo "OK"
