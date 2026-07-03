#!/bin/sh
set -e

echo -n "TEST overlay base directory rename EXDEV fallback... "

TEST_AGENT_ID="test-overlay-base-dir-rename-agent"
TMPDIR="${TMPDIR:-/tmp}/agentfs-test-overlay-base-dir-rename-$$"
MOUNTPOINT="$TMPDIR/mnt"
BASEDIR="$TMPDIR/base"
MOUNT_LOG="$TMPDIR/mount.log"

cleanup() {
    fusermount3 -u "$MOUNTPOINT" 2>/dev/null || fusermount -u "$MOUNTPOINT" 2>/dev/null || true
    if [ "${MOUNT_PID:-}" ]; then
        wait "$MOUNT_PID" 2>/dev/null || true
    fi
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
    rm -rf "$TMPDIR" 2>/dev/null || true
}

trap cleanup EXIT
cleanup

mkdir -p "$BASEDIR/rename_probe/sub" "$BASEDIR/mv_src/sub" "$MOUNTPOINT"
printf "probe root\n" > "$BASEDIR/rename_probe/root.txt"
printf "probe nested\n" > "$BASEDIR/rename_probe/sub/nested.txt"
printf "mv root\n" > "$BASEDIR/mv_src/root.txt"
printf "mv nested\n" > "$BASEDIR/mv_src/sub/nested.txt"

if ! output=$(cargo run -- init "$TEST_AGENT_ID" --base "$BASEDIR" 2>&1); then
    echo "FAILED: init with --base failed"
    echo "Output was: $output"
    exit 1
fi

cargo run -- mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground >"$MOUNT_LOG" 2>&1 &
MOUNT_PID=$!

MAX_WAIT=10
WAITED=0
while [ "$WAITED" -lt "$MAX_WAIT" ]; do
    if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
        break
    fi
    sleep 0.5
    WAITED=$((WAITED + 1))
done

if ! mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "FAILED: mount did not become ready in time"
    cat "$MOUNT_LOG" 2>/dev/null || true
    exit 1
fi

python3 - "$MOUNTPOINT/rename_probe" "$MOUNTPOINT/rename_probe_moved" <<'PY'
import errno
import os
import sys

src, dst = sys.argv[1], sys.argv[2]
try:
    os.rename(src, dst)
except OSError as exc:
    if exc.errno == errno.EXDEV:
        raise SystemExit(0)
    print(f"expected EXDEV from os.rename, got errno={exc.errno}: {exc}", file=sys.stderr)
    raise SystemExit(1)
else:
    print("expected EXDEV from os.rename, but rename succeeded", file=sys.stderr)
    raise SystemExit(1)
PY

if [ ! -f "$MOUNTPOINT/rename_probe/root.txt" ] || [ ! -f "$MOUNTPOINT/rename_probe/sub/nested.txt" ]; then
    echo "FAILED: source tree changed after EXDEV rename"
    exit 1
fi
if [ -e "$MOUNTPOINT/rename_probe_moved" ]; then
    echo "FAILED: destination exists after EXDEV rename"
    exit 1
fi

mv "$MOUNTPOINT/mv_src" "$MOUNTPOINT/mv_dst"

if [ -e "$MOUNTPOINT/mv_src" ]; then
    echo "FAILED: mv fallback left old overlay path visible"
    exit 1
fi
if [ ! -f "$MOUNTPOINT/mv_dst/root.txt" ] || [ ! -f "$MOUNTPOINT/mv_dst/sub/nested.txt" ]; then
    echo "FAILED: mv fallback did not create complete destination tree"
    exit 1
fi
cmp "$MOUNTPOINT/mv_dst/root.txt" "$BASEDIR/mv_src/root.txt"
cmp "$MOUNTPOINT/mv_dst/sub/nested.txt" "$BASEDIR/mv_src/sub/nested.txt"

if [ ! -f "$BASEDIR/mv_src/root.txt" ] || [ ! -f "$BASEDIR/mv_src/sub/nested.txt" ]; then
    echo "FAILED: base backing tree was modified"
    exit 1
fi

fusermount3 -u "$MOUNTPOINT" 2>/dev/null || fusermount -u "$MOUNTPOINT"
wait "$MOUNT_PID" 2>/dev/null || true
MOUNT_PID=

echo "OK"
