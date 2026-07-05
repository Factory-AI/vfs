#!/bin/sh
set -eu

echo -n "TEST mount... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

TEST_AGENT_ID="test-mount-agent"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-mount.XXXXXX")"
MOUNTPOINT="$ROOT/mnt"
MOUNT_PID=""

unmount_quiet() {
    fusermount3 -u "$MOUNTPOINT" 2>/dev/null ||
        fusermount -u "$MOUNTPOINT" 2>/dev/null || true
}

cleanup() {
    unmount_quiet
    if [ -n "$MOUNT_PID" ]; then
        kill "$MOUNT_PID" 2>/dev/null || true
        wait "$MOUNT_PID" 2>/dev/null || true
    fi
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

echo "OK"
