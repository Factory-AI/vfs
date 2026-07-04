#!/bin/sh
set -e

echo -n "TEST overlay whiteout persistence... "

TEST_AGENT_ID="test-overlay-whiteout-agent"
MOUNTPOINT="/tmp/agentfs-test-overlay-mount-$$"
BASEDIR="/tmp/agentfs-test-overlay-base-$$"

cleanup() {
    # Unmount if mounted
    fusermount -u "$MOUNTPOINT" 2>/dev/null || true
    # Remove directories
    rm -rf "$MOUNTPOINT" "$BASEDIR" 2>/dev/null || true
    # Remove test database
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
}

# Ensure cleanup on exit
trap cleanup EXIT

# Clean up any existing test artifacts
cleanup

# Create base directory with a test file
mkdir -p "$BASEDIR"
echo "original content" > "$BASEDIR/testfile.txt"

# Initialize the database with --base for overlay
if ! output=$(cargo run -- init "$TEST_AGENT_ID" --base "$BASEDIR" 2>&1); then
    echo "FAILED: init with --base failed"
    echo "Output was: $output"
    exit 1
fi

# Create mountpoint
mkdir -p "$MOUNTPOINT"

# Mount in foreground mode (background it ourselves so we can control it)
cargo run -- mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground &
MOUNT_PID=$!

# Wait for mount to be ready
MAX_WAIT=10
WAITED=0
while [ $WAITED -lt $MAX_WAIT ]; do
    if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
        break
    fi
    sleep 0.5
    WAITED=$((WAITED + 1))
done

if ! mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "FAILED: mount did not become ready in time"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Verify base file is visible through overlay
if [ ! -f "$MOUNTPOINT/testfile.txt" ]; then
    echo "FAILED: base file not visible through overlay"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

CONTENT=$(cat "$MOUNTPOINT/testfile.txt")
if [ "$CONTENT" != "original content" ]; then
    echo "FAILED: base file content mismatch"
    echo "Expected: original content"
    echo "Got: $CONTENT"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Delete the file through the overlay
rm "$MOUNTPOINT/testfile.txt"

# Verify file is deleted
if [ -f "$MOUNTPOINT/testfile.txt" ]; then
    echo "FAILED: file still exists after deletion"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Unmount
fusermount -u "$MOUNTPOINT"
wait $MOUNT_PID 2>/dev/null || true

# Remount to test persistence
cargo run -- mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground &
MOUNT_PID=$!

# Wait for mount to be ready
WAITED=0
while [ $WAITED -lt $MAX_WAIT ]; do
    if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
        break
    fi
    sleep 0.5
    WAITED=$((WAITED + 1))
done

if ! mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
    echo "FAILED: remount did not become ready in time"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Verify file is still deleted after remount (whiteout was persisted)
if [ -f "$MOUNTPOINT/testfile.txt" ]; then
    echo "FAILED: deleted file reappeared after remount (whiteout not persisted)"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Verify base file still exists in original location (untouched)
if [ ! -f "$BASEDIR/testfile.txt" ]; then
    echo "FAILED: base file was modified (should be untouched)"
    kill $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Unmount
fusermount -u "$MOUNTPOINT"
wait $MOUNT_PID 2>/dev/null || true

echo "OK"
