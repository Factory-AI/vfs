#!/bin/sh
set -eu

echo -n "TEST overlay readdir/unlink for delta files in base directories... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

TEST_AGENT_ID="test-overlay-delta-in-base-dir-agent"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-overlay-delta.XXXXXX")"
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

# The session DB lands under $ROOT/.agentfs instead of the repo working tree.
cd "$ROOT"

# Create base directory with a subdirectory (simulating .git)
mkdir -p "$BASEDIR/.git"
echo "[core]" > "$BASEDIR/.git/config"
echo "ref: refs/heads/main" > "$BASEDIR/.git/HEAD"

# Initialize the database with --base for overlay
output=$(run_agentfs init "$TEST_AGENT_ID" --base "$BASEDIR" 2>&1) ||
    fail "init with --base failed
Output was: $output"

mkdir -p "$MOUNTPOINT"

# Mount in foreground mode (background it ourselves so we can control it)
run_agentfs mount ".agentfs/${TEST_AGENT_ID}.db" "$MOUNTPOINT" --foreground &
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

# Verify base directory structure is visible
[ -d "$MOUNTPOINT/.git" ] || fail "base .git directory not visible through overlay"
[ -f "$MOUNTPOINT/.git/config" ] || fail "base .git/config file not visible through overlay"

# Create a new file in the base subdirectory through the overlay
# This triggers ensure_parent_dirs which creates .git in delta with origin mapping
echo "lock content" > "$MOUNTPOINT/.git/index.lock"

# Verify the file was created
[ -f "$MOUNTPOINT/.git/index.lock" ] || fail "could not create index.lock in .git directory"

# Verify readdir shows both base and delta files
# This is the first bug: delta files in base directories were invisible in readdir
LS_OUTPUT=$(ls "$MOUNTPOINT/.git")
echo "$LS_OUTPUT" | grep -q "index.lock" || fail "readdir does not show delta file index.lock
ls output was: $LS_OUTPUT"
echo "$LS_OUTPUT" | grep -q "config" || fail "readdir does not show base file config
ls output was: $LS_OUTPUT"
echo "$LS_OUTPUT" | grep -q "HEAD" || fail "readdir does not show base file HEAD
ls output was: $LS_OUTPUT"

# Delete the delta file
# This is the second bug: unlink failed for delta files in base directories
rm "$MOUNTPOINT/.git/index.lock"

# Verify the file is actually deleted
[ ! -f "$MOUNTPOINT/.git/index.lock" ] || fail "index.lock still exists after deletion"

# Verify readdir no longer shows it
LS_OUTPUT_AFTER=$(ls "$MOUNTPOINT/.git")
if echo "$LS_OUTPUT_AFTER" | grep -q "index.lock"; then
    fail "readdir still shows index.lock after deletion
ls output was: $LS_OUTPUT_AFTER"
fi

# Base files should still be visible
echo "$LS_OUTPUT_AFTER" | grep -q "config" ||
    fail "base file config disappeared after delta file deletion"

# Test creating and deleting a subdirectory in a base directory
mkdir "$MOUNTPOINT/.git/objects"
[ -d "$MOUNTPOINT/.git/objects" ] || fail "could not create objects subdirectory in .git"

# Verify readdir shows the new directory
LS_WITH_DIR=$(ls "$MOUNTPOINT/.git")
echo "$LS_WITH_DIR" | grep -q "objects" || fail "readdir does not show delta directory objects
ls output was: $LS_WITH_DIR"

# Remove the directory (rmdir)
rmdir "$MOUNTPOINT/.git/objects"
[ ! -d "$MOUNTPOINT/.git/objects" ] || fail "objects directory still exists after rmdir"

# Unmount
unmount_quiet
wait $MOUNT_PID 2>/dev/null || true
MOUNT_PID=""

echo "OK"
