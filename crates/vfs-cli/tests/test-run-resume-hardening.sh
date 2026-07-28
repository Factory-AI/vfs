#!/bin/sh
#
# Daemon-facing `vfs run` resume contract: repeat starts, externally
# materialized sessions, UID squash, status JSON, and live-conflict codes.
#
set -eu

echo -n "TEST run resume hardening... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
VFS_BIN="${VFS_BIN:-}"
HOST_HOME="${HOME:-}"
CARGO_HOME_FOR_TEST="${CARGO_HOME:-$HOST_HOME/.cargo}"
RUSTUP_HOME_FOR_TEST="${RUSTUP_HOME:-$HOST_HOME/.rustup}"
RUSTUP_TOOLCHAIN_FOR_TEST="${RUSTUP_TOOLCHAIN:-nightly}"
TEST_ROOT=""
OWNER_PID=""
SOURCE_MNT=""
TARGET_MNT=""

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
    deadline=$(( $(date +%s) + 5 ))
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 0.1
    done
}

unmount_path() {
    path="$1"
    [ -n "$path" ] || return
    if command -v fusermount3 >/dev/null 2>&1; then
        fusermount3 -uz "$path" 2>/dev/null || true
    elif command -v fusermount >/dev/null 2>&1; then
        fusermount -u "$path" 2>/dev/null || true
    else
        umount -l "$path" 2>/dev/null || true
    fi
}

case "$(uname -s)" in
    Linux) ;;
    *) skip "requires Linux namespaces and FUSE" ;;
esac

command -v cargo >/dev/null 2>&1 || skip "cargo is unavailable"
command -v python3 >/dev/null 2>&1 || skip "python3 is unavailable"
command -v mountpoint >/dev/null 2>&1 || skip "mountpoint is unavailable"
[ -x /bin/bash ] || skip "/bin/bash is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"

if [ -r /proc/sys/kernel/unprivileged_userns_clone ] &&
    [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
    skip "unprivileged user namespaces are disabled"
fi

if [ -z "$VFS_BIN" ]; then
    cargo build --quiet --manifest-path "$CLI_DIR/Cargo.toml" >/dev/null 2>&1 ||
        fail "failed to build vfs CLI"
    VFS_BIN="$REPO_ROOT/target/debug/vfs"
fi
[ -x "$VFS_BIN" ] || fail "VFS_BIN is not executable: $VFS_BIN"

cleanup() {
    set +e
    if [ -n "$OWNER_PID" ] && kill -0 "$OWNER_PID" 2>/dev/null; then
        kill -TERM "$OWNER_PID" 2>/dev/null || true
        wait_pid_exit "$OWNER_PID" || kill -KILL "$OWNER_PID" 2>/dev/null || true
        wait "$OWNER_PID" 2>/dev/null || true
    fi
    unmount_path "$TARGET_MNT"
    unmount_path "$SOURCE_MNT"
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/vfs-run-resume.*) rm -rf "$TEST_ROOT" ;;
            *) echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT" ;;
        esac
    fi
}
trap cleanup EXIT INT TERM

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vfs-run-resume.XXXXXX")"
TEST_HOME="$TEST_ROOT/home"
BASE="$TEST_ROOT/base"
OTHER="$TEST_ROOT/other"
SOURCE_ID="resume-source-$$"
TARGET_ID="resume-target-$$"
SOURCE_DIR="$TEST_HOME/.vfs/run/$SOURCE_ID"
TARGET_DIR="$TEST_HOME/.vfs/run/$TARGET_ID"
SOURCE_MNT="$SOURCE_DIR/mnt"
TARGET_MNT="$TARGET_DIR/mnt"
PACKED="$TEST_ROOT/packed.db"
OWNER_LOG="$TEST_ROOT/owner.log"
mkdir -p "$TEST_HOME/.cache" "$TEST_HOME/.config" "$BASE" "$OTHER"
printf "base payload\n" >"$BASE/base.txt"

run_from() {
    workdir="$1"
    shift
    (
        cd "$workdir"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        CARGO_HOME="$CARGO_HOME_FOR_TEST" \
        RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
        RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" "$@"
    )
}

# Three clean incarnations of one session; later starts come from a different
# host cwd and must still use the persisted absolute base_path.
run_from "$BASE" run --session "$SOURCE_ID" /bin/bash -c '
set -e
test "$(cat base.txt)" = "base payload"
printf "one\n" > persisted.txt
printf "foreign readable\n" > foreign.txt
chmod 600 foreign.txt
'
run_from "$OTHER" run --session "$SOURCE_ID" /bin/bash -c "
set -e
test \"\$PWD\" = \"$BASE\"
test \"\$(cat persisted.txt)\" = one
printf \"two\\n\" > persisted.txt
"
run_from "$OTHER" run --session "$SOURCE_ID" /bin/bash -c '
set -e
test "$(cat persisted.txt)" = two
test "$(cat foreign.txt)" = "foreign readable"
'

SOURCE_STATUS="$TEST_ROOT/source-status.json"
run_from "$OTHER" status "$SOURCE_ID" --json >"$SOURCE_STATUS"
python3 - "$SOURCE_STATUS" "$SOURCE_ID" <<'PY'
import json, sys
status = json.load(open(sys.argv[1]))
assert status == {
    "sessionId": sys.argv[2],
    "state": "stopped",
    "mounted": False,
    "pid": None,
    "generation": 0,
    "seeded": False,
}, status
PY

run_from "$OTHER" pack "$SOURCE_ID" --output "$PACKED" --json >/dev/null
mkdir -p "$TARGET_DIR"
cp "$PACKED" "$TARGET_DIR/delta.db"
printf "%s" "$BASE" >"$TARGET_DIR/base_path"

# This is the exact receiver-created layout before first run.
[ "$(find "$TARGET_DIR" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort | tr '\n' ' ')" = \
    "base_path delta.db " ] || fail "external session contains hidden runtime requirements"

# Simulate a sender UID/GID that is unmapped on this machine. Mode 0600 would
# be unreadable if run did not pass current uid/gid into the FUSE adapter.
python3 - "$TARGET_DIR/delta.db" <<'PY'
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.execute(
    """UPDATE fs_inode SET uid = 424242, gid = 424242
       WHERE ino = (
         SELECT ino FROM fs_dentry WHERE parent_ino = 1 AND name = 'foreign.txt'
       )"""
)
assert db.total_changes == 1
db.commit()
db.close()
PY

run_from "$OTHER" run --session "$TARGET_ID" /bin/bash -c '
set -e
test "$(cat persisted.txt)" = two
test "$(cat foreign.txt)" = "foreign readable"
test "$(stat -c %u foreign.txt)" = "$(id -u)"
test "$(stat -c %g foreign.txt)" = "$(id -g)"
test "$(stat -c %a foreign.txt)" = 600
'

# A checkout materialized beneath an allowed ancestor keeps its overlay for
# absolute-path resolution. A plain (non-recursive) self-bind of the allowed
# ancestor would shadow the nested overlay mount, so a subprocess spawned with
# an explicit absolute cwd would silently see the raw base tree.
NESTED_ID="resume-nested-$$"
NESTED_DIR="$TEST_HOME/.vfs/run/$NESTED_ID"
NESTED_BASE="$TEST_HOME/.data/handoff/$NESTED_ID/checkout"
mkdir -p "$NESTED_BASE" "$NESTED_DIR"
printf "base payload\n" >"$NESTED_BASE/base.txt"
cp "$PACKED" "$NESTED_DIR/delta.db"
printf "%s" "$NESTED_BASE" >"$NESTED_DIR/base_path"
run_from "$NESTED_BASE" run --session "$NESTED_ID" \
    --allow "$TEST_HOME/.data" /bin/bash -c '
set -e
test "$(cat persisted.txt)" = two
cd / && cd "$OLDPWD"
test "$(cat persisted.txt)" = two
env --chdir "$PWD" cat persisted.txt >/dev/null
test "$(env --chdir "$PWD" cat persisted.txt)" = two
'

TARGET_STATUS="$TEST_ROOT/target-status.json"
run_from "$OTHER" status "$TARGET_ID" --json >"$TARGET_STATUS"
python3 - "$TARGET_STATUS" "$TARGET_ID" <<'PY'
import json, sys
status = json.load(open(sys.argv[1]))
assert status["sessionId"] == sys.argv[2], status
assert status["state"] == "stopped", status
assert status["mounted"] is False, status
assert status["pid"] is None, status
assert status["generation"] == 1, status
assert status["seeded"] is False, status
assert set(status) == {
    "sessionId", "state", "mounted", "pid", "generation", "seeded"
}, status
PY

# A genuinely live incarnation accepts joiners without being mistaken for
# stale state or opening the database a second time.
(
    cd "$OTHER"
    exec env \
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        CARGO_HOME="$CARGO_HOME_FOR_TEST" \
        RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
        RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" run --session "$TARGET_ID" \
        /bin/bash -c 'while :; do sleep 1; done'
) >"$OWNER_LOG" 2>&1 &
OWNER_PID=$!
deadline=$(( $(date +%s) + 30 ))
while [ "$(date +%s)" -le "$deadline" ]; do
    if [ -d "$TARGET_DIR/procs" ] && mountpoint -q "$TARGET_DIR/mnt" 2>/dev/null; then
        break
    fi
    kill -0 "$OWNER_PID" 2>/dev/null || {
        cat "$OWNER_LOG"
        fail "live owner exited before conflict probe"
    }
    sleep 0.1
done
mountpoint -q "$TARGET_DIR/mnt" 2>/dev/null || fail "live owner never mounted"

run_from "$OTHER" run --session "$TARGET_ID" /bin/bash -c '
set -e
test "$(cat persisted.txt)" = two
printf "joined\n" > live-join.txt
'

LIVE_STATUS="$TEST_ROOT/live-status.json"
run_from "$OTHER" status "$TARGET_ID" --json >"$LIVE_STATUS"
python3 - "$LIVE_STATUS" <<'PY'
import json, sys
status = json.load(open(sys.argv[1]))
assert status["state"] == "live", status
assert status["mounted"] is True, status
assert isinstance(status["pid"], int) and status["pid"] > 0, status
PY

kill -TERM "$OWNER_PID"
wait "$OWNER_PID" 2>/dev/null || true
OWNER_PID=""

run_from "$OTHER" run --session "$TARGET_ID" \
    /bin/bash -c 'test "$(cat live-join.txt)" = joined'

set +e
run_from "$OTHER" run --session "../invalid" /bin/true >/dev/null 2>&1
INVALID_CODE=$?
set -e
[ "$INVALID_CODE" -eq 5 ] || fail "invalid session returned $INVALID_CODE instead of 5"

echo "OK"
