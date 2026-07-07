#!/bin/sh
set -eu

echo -n "TEST mount... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

TEST_AGENT_ID="test-mount-agent"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-mount.XXXXXX")"
MOUNTPOINT="$ROOT/mnt"
CYCLE_MOUNTPOINT="$ROOT/mnt-cycle"
MOUNT_PID=""
CYCLE_PIDS=""

unmount_quiet() {
    fusermount3 -u "$MOUNTPOINT" 2>/dev/null ||
        fusermount -u "$MOUNTPOINT" 2>/dev/null || true
}

cleanup() {
    unmount_quiet
    for cycle_mnt in "$CYCLE_MOUNTPOINT".*; do
        if mountpoint -q "$cycle_mnt" 2>/dev/null; then
            fusermount3 -u "$cycle_mnt" 2>/dev/null || true
        fi
    done
    for pid in $MOUNT_PID $CYCLE_PIDS; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

fail() {
    echo "FAILED: $*"
    exit 1
}

# Resolve the binary once: background mount legs must track the agentfs PID
# itself (backgrounding a wrapper orphans the real process on cleanup kill).
if [ -n "${AGENTFS_BIN:-}" ]; then
    BIN="$AGENTFS_BIN"
else
    cargo build --quiet --manifest-path "$CLI_DIR/Cargo.toml" || {
        echo "FAILED: could not build agentfs"
        exit 1
    }
    BIN="$CLI_DIR/../../target/debug/agentfs"
fi
if [ ! -x "$BIN" ]; then
    echo "FAILED: agentfs binary not found at $BIN"
    exit 1
fi

run_agentfs() {
    "$BIN" "$@"
}

# The session DB lands under $ROOT/.agentfs instead of the repo working tree.
cd "$ROOT"

run_agentfs init "$TEST_AGENT_ID" > /dev/null 2>&1

mkdir -p "$MOUNTPOINT"

# Mount in foreground mode (background it ourselves so we can control it)
"$BIN" mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground &
MOUNT_PID=$!

# Wait for mount to be ready
MAX_WAIT=20
WAITED=0
while [ $WAITED -lt $MAX_WAIT ]; do
    if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
        break
    fi
    sleep 0.5
    WAITED=$((WAITED + 1))
done

mountpoint -q "$MOUNTPOINT" 2>/dev/null || fail "mount did not become ready in time"

# Test that 'agentfs mount' (no args) lists our mount
run_agentfs mount 2>/dev/null | grep -q "$MOUNTPOINT" ||
    fail "'agentfs mount' did not list our mountpoint"

# Write a file through the FUSE mount
echo "hello from fuse mount" > "$MOUNTPOINT/hello.txt"

# Read it back
CONTENT=$(cat "$MOUNTPOINT/hello.txt")

[ "$CONTENT" = "hello from fuse mount" ] || fail "file content mismatch
Expected: hello from fuse mount
Got: $CONTENT"

# Test mkdir
mkdir "$MOUNTPOINT/testdir"
[ -d "$MOUNTPOINT/testdir" ] || fail "mkdir did not create directory"

# Test creating file in subdirectory
echo "nested file" > "$MOUNTPOINT/testdir/nested.txt"
NESTED_CONTENT=$(cat "$MOUNTPOINT/testdir/nested.txt")

[ "$NESTED_CONTENT" = "nested file" ] || fail "nested file content mismatch"

# Test symlink creation
ln -s nested.txt "$MOUNTPOINT/testdir/link_to_nested"
[ -L "$MOUNTPOINT/testdir/link_to_nested" ] || fail "symlink was not created"

# Test reading symlink target
LINK_TARGET=$(readlink "$MOUNTPOINT/testdir/link_to_nested")
[ "$LINK_TARGET" = "nested.txt" ] || fail "symlink target mismatch
Expected: nested.txt
Got: $LINK_TARGET"

# Test following symlink to read file
LINKED_CONTENT=$(cat "$MOUNTPOINT/testdir/link_to_nested")
[ "$LINKED_CONTENT" = "nested file" ] || fail "reading through symlink failed
Expected: nested file
Got: $LINKED_CONTENT"

# Test symlink to directory
ln -s testdir "$MOUNTPOINT/link_to_testdir"
[ -L "$MOUNTPOINT/link_to_testdir" ] || fail "symlink to directory was not created"

# Test accessing file through directory symlink
DIR_LINKED_CONTENT=$(cat "$MOUNTPOINT/link_to_testdir/nested.txt")
[ "$DIR_LINKED_CONTENT" = "nested file" ] || fail "reading through directory symlink failed"

# Unmount
unmount_quiet

# Wait for mount process to exit
wait $MOUNT_PID 2>/dev/null || true
MOUNT_PID=""

# Rapid unmount-then-mount cycles: each remount starts while the previous
# owner is still tearing down, racing the kernel-side drain of the just-closed
# FUSE connection. With fuse-over-io_uring enabled that race used to wedge
# mount(2) forever; it must now either become ready or exit nonzero within a
# bounded window (docs/MANUAL.md, "FUSE-over-io_uring and rapid remounts").
# One fresh mountpoint and database per cycle, like the back-to-back suite
# tests that originally hit the wedge. This leg also regresses the recycled
# fusectl-id abort: a prior owner's teardown must not abort the fresh mount
# that inherited its connection id (silent mount-then-exit-0 pre-fix).
CYCLE_WINDOW="${MOUNT_CYCLE_WINDOW:-25}"
for cycle in 1 2 3; do
    CYCLE_MNT="$CYCLE_MOUNTPOINT.$cycle"
    mkdir -p "$CYCLE_MNT"
    run_agentfs init "cycle-$cycle" > /dev/null 2>&1
    "$BIN" mount ".agentfs/cycle-$cycle.db" "$CYCLE_MNT" --foreground \
        >"$ROOT/cycle.$cycle.log" 2>&1 &
    CYCLE_PID=$!
    CYCLE_PIDS="$CYCLE_PIDS $CYCLE_PID"
    T0=$(date +%s)
    OUTCOME=""
    while [ $(( $(date +%s) - T0 )) -le "$CYCLE_WINDOW" ]; do
        if mountpoint -q "$CYCLE_MNT" 2>/dev/null; then
            OUTCOME="mounted"
            break
        fi
        if ! kill -0 "$CYCLE_PID" 2>/dev/null; then
            OUTCOME="exited"
            break
        fi
        sleep 0.1
    done
    if [ -z "$OUTCOME" ]; then
        echo "cycle $cycle log:"
        sed 's/^/  /' "$ROOT/cycle.$cycle.log" || true
        fail "rapid remount cycle $cycle wedged: not mounted and still running after ${CYCLE_WINDOW}s"
    fi
    if [ "$OUTCOME" = "mounted" ]; then
        # Immediately unmount and start the next cycle without reaping,
        # keeping the drain overlap that triggers the race.
        fusermount3 -u "$CYCLE_MNT" 2>/dev/null ||
            fusermount -u "$CYCLE_MNT" 2>/dev/null ||
            fail "rapid remount cycle $cycle could not unmount"
    else
        RC=0
        wait "$CYCLE_PID" 2>/dev/null || RC=$?
        [ "$RC" -ne 0 ] || fail "rapid remount cycle $cycle exited 0 without mounting"
        echo "note: rapid remount cycle $cycle failed bounded with rc=$RC (accepted: clear error beats a wedge)"
        sed 's/^/  /' "$ROOT/cycle.$cycle.log" || true
    fi
done

# Suite invariant: reap every mount owner before returning so no FUSE
# connection drain overlaps the next test's mount.
for pid in $CYCLE_PIDS; do
    WAITED=0
    while kill -0 "$pid" 2>/dev/null && [ "$WAITED" -lt 100 ]; do
        sleep 0.1
        WAITED=$((WAITED + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        sleep 0.5
        kill -0 "$pid" 2>/dev/null && fail "rapid remount owner $pid did not exit"
    fi
    wait "$pid" 2>/dev/null || true
done
CYCLE_PIDS=""

echo "OK"
