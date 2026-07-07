#!/bin/sh
#
# SIGKILL recovery regression for mount-owning run sessions.
#
# Killing the owner cannot run normal teardown, so this pins the recovery
# contract: the supervised workload dies with the owner, the stale mount is
# externally cleanable, fsynced data is still present after remount, and the DB
# integrity check stays clean.
#
set -eu

echo -n "TEST SIGKILL recovery for run owner... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
CARGO_MANIFEST="$CLI_DIR/Cargo.toml"
AGENTFS_BIN="${AGENTFS_BIN:-}"
START_TIMEOUT="${SIGKILL_RECOVERY_START_TIMEOUT:-30}"
EXIT_TIMEOUT="${SIGKILL_RECOVERY_EXIT_TIMEOUT:-10}"
HOST_HOME="${HOME:-}"
CARGO_HOME_FOR_TEST="${CARGO_HOME:-$HOST_HOME/.cargo}"
RUSTUP_HOME_FOR_TEST="${RUSTUP_HOME:-$HOST_HOME/.rustup}"
RUSTUP_TOOLCHAIN_FOR_TEST="${RUSTUP_TOOLCHAIN:-nightly}"

TEST_ROOT=""
TEST_HOME=""
WORKDIR=""
LOGDIR=""
SESSION_ID=""
RUN_DIR=""
DELTA_DB=""
FUSE_MNT=""
ACK_LOG=""
READY_FILE=""
CHILD_PID_FILE=""
OWNER_PID=""
OWNER_LOG=""
REMOUNT_PID=""
REMOUNT=""
REMOUNT_LOG=""

skip() {
    echo "SKIP: $*"
    exit 0
}

fail() {
    echo "FAILED: $*"
    exit 1
}

case "$(uname -s)" in
    Linux)
        ;;
    *)
        skip "requires Linux namespaces and FUSE"
        ;;
esac

command -v cargo >/dev/null 2>&1 || skip "cargo is unavailable"
command -v mountpoint >/dev/null 2>&1 || skip "mountpoint is unavailable"
command -v sha256sum >/dev/null 2>&1 || skip "sha256sum is unavailable"
command -v timeout >/dev/null 2>&1 || skip "timeout is unavailable"
[ -x /bin/bash ] || skip "/bin/bash is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"

if [ -r /proc/sys/kernel/unprivileged_userns_clone ] &&
    [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
    skip "unprivileged user namespaces are disabled"
fi

if [ -z "$AGENTFS_BIN" ]; then
    cargo build --quiet --manifest-path "$CARGO_MANIFEST" >/dev/null 2>&1 ||
        fail "failed to build agentfs CLI"
    AGENTFS_BIN="$REPO_ROOT/target/debug/agentfs"
fi

[ -x "$AGENTFS_BIN" ] || fail "AGENTFS_BIN is not executable: $AGENTFS_BIN"

unmount_path() {
    path="$1"
    if [ -z "$path" ]; then
        return
    fi
    if ! mountpoint -q "$path" 2>/dev/null; then
        return
    fi
    if command -v fusermount3 >/dev/null 2>&1; then
        fusermount3 -u "$path" 2>/dev/null || fusermount3 -uz "$path" 2>/dev/null || true
    elif command -v fusermount >/dev/null 2>&1; then
        fusermount -u "$path" 2>/dev/null || true
    else
        umount "$path" 2>/dev/null || umount -l "$path" 2>/dev/null || true
    fi
}

wait_pid_exit() {
    pid="$1"
    seconds="$2"
    deadline=$(( $(date +%s) + seconds ))
    while kill -0 "$pid" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 0.1
    done
    return 0
}

cleanup() {
    set +e
    if [ -n "$REMOUNT_PID" ] && kill -0 "$REMOUNT_PID" 2>/dev/null; then
        kill -TERM "$REMOUNT_PID" 2>/dev/null || true
        wait_pid_exit "$REMOUNT_PID" 5 || kill -KILL "$REMOUNT_PID" 2>/dev/null || true
        wait "$REMOUNT_PID" 2>/dev/null || true
    fi
    REMOUNT_PID=""

    if [ -n "$OWNER_PID" ] && kill -0 "$OWNER_PID" 2>/dev/null; then
        kill -TERM "$OWNER_PID" 2>/dev/null || true
        wait_pid_exit "$OWNER_PID" 5 || kill -KILL "$OWNER_PID" 2>/dev/null || true
        wait "$OWNER_PID" 2>/dev/null || true
    fi
    OWNER_PID=""

    if [ -n "$CHILD_PID_FILE" ] && [ -f "$CHILD_PID_FILE" ]; then
        child_pid="$(cat "$CHILD_PID_FILE" 2>/dev/null)"
        case "$child_pid" in
            ''|*[!0-9]*)
                ;;
            *)
                if kill -0 "$child_pid" 2>/dev/null; then
                    kill -KILL "$child_pid" 2>/dev/null || true
                    wait_pid_exit "$child_pid" 5 || true
                fi
                ;;
        esac
    fi

    unmount_path "$REMOUNT"
    unmount_path "$FUSE_MNT"

    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/agentfs-sigkill-recovery.*)
                rm -rf "$TEST_ROOT"
                ;;
            *)
                echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT"
                ;;
        esac
    fi
    set -e
}

trap cleanup EXIT

wait_for_owner_ready() {
    deadline=$(( $(date +%s) + START_TIMEOUT ))
    while [ "$(date +%s)" -le "$deadline" ]; do
        if [ -s "$ACK_LOG" ] && [ -f "$READY_FILE" ] && [ -f "$CHILD_PID_FILE" ] &&
            [ -f "$DELTA_DB" ] && mountpoint -q "$FUSE_MNT" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$OWNER_PID" 2>/dev/null; then
            wait "$OWNER_PID" 2>/dev/null || true
            return 1
        fi
        sleep 0.1
    done
    return 1
}

dump_context() {
    label="$1"
    echo
    echo "FAILED: $label"
    if [ -n "$OWNER_LOG" ] && [ -f "$OWNER_LOG" ]; then
        echo "owner log:"
        sed 's/^/  /' "$OWNER_LOG" | tail -n 120
    fi
    if [ -n "$REMOUNT_LOG" ] && [ -f "$REMOUNT_LOG" ]; then
        echo "remount log:"
        sed 's/^/  /' "$REMOUNT_LOG" | tail -n 80
    fi
    if [ -n "$FUSE_MNT" ]; then
        echo "mount table:"
        mount | grep "$FUSE_MNT" || true
    fi
}

wait_for_remount() {
    deadline=$(( $(date +%s) + START_TIMEOUT ))
    while [ "$(date +%s)" -le "$deadline" ]; do
        if mountpoint -q "$REMOUNT" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$REMOUNT_PID" 2>/dev/null; then
            wait "$REMOUNT_PID" 2>/dev/null || true
            return 1
        fi
        sleep 0.1
    done
    return 1
}

assert_acked_files_present() {
    while read -r rel_path expected_hash; do
        [ -n "$rel_path" ] || continue
        if [ ! -f "$REMOUNT/$rel_path" ]; then
            dump_context "acked file missing after remount: $rel_path"
            exit 1
        fi
        observed_hash="$(sha256sum "$REMOUNT/$rel_path" | awk '{print $1}')"
        if [ "$observed_hash" != "$expected_hash" ]; then
            dump_context "acked file hash mismatch for $rel_path: expected $expected_hash observed $observed_hash"
            exit 1
        fi
    done <"$ACK_LOG"
}

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-sigkill-recovery.XXXXXX")"
TEST_HOME="$TEST_ROOT/home"
WORKDIR="$TEST_ROOT/work"
LOGDIR="$TEST_ROOT/logs"
SESSION_ID="sigkill-recovery-$$"
RUN_DIR="$TEST_HOME/.agentfs/run/$SESSION_ID"
DELTA_DB="$RUN_DIR/delta.db"
FUSE_MNT="$RUN_DIR/mnt"
ACK_LOG="$LOGDIR/acked.txt"
READY_FILE="$LOGDIR/ready"
CHILD_PID_FILE="$LOGDIR/child.pid"
OWNER_LOG="$LOGDIR/owner.log"
REMOUNT="$TEST_ROOT/remount"
REMOUNT_LOG="$LOGDIR/remount.log"
mkdir -p "$TEST_HOME/.cache" "$TEST_HOME/.config" "$WORKDIR" "$LOGDIR" "$REMOUNT"
: >"$ACK_LOG"

(
    cd "$WORKDIR"
    HOME="$TEST_HOME" \
    XDG_CACHE_HOME="$TEST_HOME/.cache" \
    XDG_CONFIG_HOME="$TEST_HOME/.config" \
    CARGO_HOME="$CARGO_HOME_FOR_TEST" \
    RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
    RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
    AGENTFS_FUSE_URING=0 \
    "$AGENTFS_BIN" run --session "$SESSION_ID" --allow "$LOGDIR" \
        /bin/bash -c '
set -euo pipefail
printf "%s\n" "$$" > "$1"
mkdir -p sigkill
for i in 1 2 3 4 5; do
    rel="sigkill/acked-$i.txt"
    printf "acked-payload-%s\n" "$i" > "$rel"
    python3 - "$rel" <<'"'"'PY'"'"'
import os
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
fd = os.open(path, os.O_RDWR)
try:
    os.fsync(fd)
finally:
    os.close(fd)
dir_fd = os.open(path.parent, os.O_RDONLY)
try:
    os.fsync(dir_fd)
finally:
    os.close(dir_fd)
PY
    hash="$(sha256sum "$rel" | awk "{print \$1}")"
    printf "%s %s\n" "$rel" "$hash" >> "$2"
done
: > "$3"
while :; do
    sleep 1
done
' agentfs-sigkill-workload "$CHILD_PID_FILE" "$ACK_LOG" "$READY_FILE"
) >"$OWNER_LOG" 2>&1 &
OWNER_PID=$!

if ! wait_for_owner_ready; then
    dump_context "owner did not reach fsynced write workload"
    exit 1
fi

CHILD_PID="$(cat "$CHILD_PID_FILE")"
kill -KILL "$OWNER_PID"
wait "$OWNER_PID" 2>/dev/null || true
OWNER_PID=""

if ! wait_pid_exit "$CHILD_PID" "$EXIT_TIMEOUT"; then
    dump_context "workload child $CHILD_PID survived owner SIGKILL"
    exit 1
fi

if ! mountpoint -q "$FUSE_MNT" 2>/dev/null; then
    dump_context "expected stale FUSE mountpoint before external cleanup"
    exit 1
fi

unmount_path "$FUSE_MNT"
if mountpoint -q "$FUSE_MNT" 2>/dev/null; then
    dump_context "stale FUSE mountpoint could not be externally cleaned"
    exit 1
fi

"$AGENTFS_BIN" integrity "$DELTA_DB" --json >/dev/null

"$AGENTFS_BIN" mount "$DELTA_DB" "$REMOUNT" --backend fuse --foreground >"$REMOUNT_LOG" 2>&1 &
REMOUNT_PID=$!
if ! wait_for_remount; then
    dump_context "remount did not become ready after SIGKILL recovery"
    exit 1
fi

assert_acked_files_present

kill -TERM "$REMOUNT_PID" 2>/dev/null || true
if ! wait_pid_exit "$REMOUNT_PID" "$EXIT_TIMEOUT"; then
    dump_context "recovery remount did not exit after SIGTERM"
    exit 1
fi
wait "$REMOUNT_PID" 2>/dev/null || true
REMOUNT_PID=""

if mount | grep "$SESSION_ID" >/dev/null 2>&1 || mount | grep "$REMOUNT" >/dev/null 2>&1; then
    dump_context "mount residue after SIGKILL recovery"
    exit 1
fi
if ps -e -o pid= -o args= | grep "$SESSION_ID" | grep agentfs | grep -v grep >/dev/null 2>&1; then
    dump_context "agentfs process residue after SIGKILL recovery"
    exit 1
fi

echo "OK"
