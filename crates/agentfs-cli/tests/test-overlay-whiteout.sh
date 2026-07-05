#!/bin/sh
set -eu

echo -n "TEST overlay whiteout persistence... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

TEST_AGENT_ID="test-overlay-whiteout-agent"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-overlay-whiteout.XXXXXX")"
MOUNTPOINT="$ROOT/mnt"
BASEDIR="$ROOT/base"
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

run_agentfs() {
    if [ -n "${AGENTFS_BIN:-}" ]; then
        "$AGENTFS_BIN" "$@"
    else
        cargo run --quiet --manifest-path "$CLI_DIR/Cargo.toml" -- "$@"
    fi
}

wait_mounted() {
    WAITED=0
    while [ $WAITED -lt 20 ]; do
        if mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
            return 0
        fi
        sleep 0.5
        WAITED=$((WAITED + 1))
    done
    mountpoint -q "$MOUNTPOINT" 2>/dev/null
}

# The session DB lands under $ROOT/.agentfs instead of the repo working tree.
cd "$ROOT"

# Create base directory with a test file
mkdir -p "$BASEDIR"
echo "original content" > "$BASEDIR/testfile.txt"

# Initialize the database with --base for overlay
output=$(run_agentfs init "$TEST_AGENT_ID" --base "$BASEDIR" 2>&1) ||
    fail "init with --base failed
Output was: $output"

mkdir -p "$MOUNTPOINT"

# Mount in foreground mode (background it ourselves so we can control it)
run_agentfs mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground &
MOUNT_PID=$!

wait_mounted || fail "mount did not become ready in time"

# Verify base file is visible through overlay
[ -f "$MOUNTPOINT/testfile.txt" ] || fail "base file not visible through overlay"

CONTENT=$(cat "$MOUNTPOINT/testfile.txt")
[ "$CONTENT" = "original content" ] || fail "base file content mismatch
Expected: original content
Got: $CONTENT"

# Delete the file through the overlay
rm "$MOUNTPOINT/testfile.txt"

# Verify file is deleted
[ ! -f "$MOUNTPOINT/testfile.txt" ] || fail "file still exists after deletion"

# Unmount
unmount_quiet
wait $MOUNT_PID 2>/dev/null || true
MOUNT_PID=""

# Remount to test persistence
run_agentfs mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground &
MOUNT_PID=$!

wait_mounted || fail "remount did not become ready in time"

# Verify file is still deleted after remount (whiteout was persisted)
[ ! -f "$MOUNTPOINT/testfile.txt" ] ||
    fail "deleted file reappeared after remount (whiteout not persisted)"

# Verify base file still exists in original location (untouched)
[ -f "$BASEDIR/testfile.txt" ] || fail "base file was modified (should be untouched)"

# Unmount
unmount_quiet
wait $MOUNT_PID 2>/dev/null || true
MOUNT_PID=""

echo "OK"
