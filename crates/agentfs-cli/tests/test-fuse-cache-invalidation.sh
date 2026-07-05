#!/bin/sh
#
# Test that FUSE kernel cache is properly invalidated after mutations.
#
# After readdirplus populates the dcache, unlink/rmdir/rename must
# invalidate the affected entries so subsequent readdir sees the change.
#
set -eu

echo -n "TEST fuse cache invalidation after mutations... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

TEST_AGENT_ID="test-fuse-cache-inval-agent"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-cache-inval.XXXXXX")"
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

# Initialize the database
run_agentfs init "$TEST_AGENT_ID" > /dev/null 2>&1

# Create mountpoint
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

# Test 1: unlink should not leave stale entries in readdir
# Populate the directory with files
echo "content1" > "$MOUNTPOINT/file1.txt"
echo "content2" > "$MOUNTPOINT/file2.txt"
echo "content3" > "$MOUNTPOINT/file3.txt"

# Prime the kernel dcache via stat + ls (readdirplus populates entries)
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/file1.txt" > /dev/null 2>&1
stat "$MOUNTPOINT/file2.txt" > /dev/null 2>&1
stat "$MOUNTPOINT/file3.txt" > /dev/null 2>&1
ls -la "$MOUNTPOINT" > /dev/null

# Delete file1 and verify readdir no longer shows it
rm "$MOUNTPOINT/file1.txt"

LS_OUTPUT=$(ls "$MOUNTPOINT")
if echo "$LS_OUTPUT" | grep -q "file1.txt"; then
    fail "readdir still shows file1.txt after unlink
ls output was: $LS_OUTPUT"
fi

echo "$LS_OUTPUT" | grep -q "file2.txt" || fail "file2.txt disappeared"

if stat "$MOUNTPOINT/file1.txt" > /dev/null 2>&1; then
    fail "stat still resolves file1.txt after unlink"
fi

# Test 2: rmdir should not leave stale entries in readdir
mkdir "$MOUNTPOINT/subdir"
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/subdir" > /dev/null 2>&1
ls -la "$MOUNTPOINT" > /dev/null

rmdir "$MOUNTPOINT/subdir"

LS_OUTPUT=$(ls "$MOUNTPOINT")
if echo "$LS_OUTPUT" | grep -q "subdir"; then
    fail "readdir still shows subdir after rmdir
ls output was: $LS_OUTPUT"
fi
if stat "$MOUNTPOINT/subdir" > /dev/null 2>&1; then
    fail "stat still resolves subdir after rmdir"
fi

# Test 3: rename should not leave stale source entry in readdir
echo "rename me" > "$MOUNTPOINT/before.txt"
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/before.txt" > /dev/null 2>&1
ls -la "$MOUNTPOINT" > /dev/null

mv "$MOUNTPOINT/before.txt" "$MOUNTPOINT/after.txt"

LS_OUTPUT=$(ls "$MOUNTPOINT")
if echo "$LS_OUTPUT" | grep -q "before.txt"; then
    fail "readdir still shows before.txt after rename
ls output was: $LS_OUTPUT"
fi

echo "$LS_OUTPUT" | grep -q "after.txt" || fail "after.txt not visible after rename
ls output was: $LS_OUTPUT"

if stat "$MOUNTPOINT/before.txt" > /dev/null 2>&1; then
    fail "stat still resolves before.txt after rename"
fi
[ "$(cat "$MOUNTPOINT/after.txt")" = "rename me" ] ||
    fail "after.txt content is stale after rename"

# Test 4: create must defeat a cached negative dentry
# Prime a negative dentry: stat a name that doesn't exist yet
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/negfile.txt" > /dev/null 2>&1 || true   # caches ENOENT
ls -la "$MOUNTPOINT" > /dev/null                          # readdirplus confirms absence

echo "new file" > "$MOUNTPOINT/negfile.txt"

# stat must resolve (not serve cached ENOENT)
stat "$MOUNTPOINT/negfile.txt" > /dev/null 2>&1 ||
    fail "stat returns ENOENT for negfile.txt after create (negative dentry not invalidated)"

# readdir must list it
LS_OUTPUT=$(ls "$MOUNTPOINT")
echo "$LS_OUTPUT" | grep -q "negfile.txt" || fail "readdir does not show negfile.txt after create
ls output was: $LS_OUTPUT"

# Test 5: mkdir must defeat a cached negative dentry
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/negdir" > /dev/null 2>&1 || true        # caches ENOENT
ls -la "$MOUNTPOINT" > /dev/null                          # readdirplus confirms absence

mkdir "$MOUNTPOINT/negdir"

stat "$MOUNTPOINT/negdir" > /dev/null 2>&1 ||
    fail "stat returns ENOENT for negdir after mkdir (negative dentry not invalidated)"

LS_OUTPUT=$(ls "$MOUNTPOINT")
echo "$LS_OUTPUT" | grep -q "negdir" || fail "readdir does not show negdir after mkdir
ls output was: $LS_OUTPUT"

# Test 6: truncate must invalidate stale attrs and cached file data
printf "abcdefghij" > "$MOUNTPOINT/truncate.txt"
cat "$MOUNTPOINT/truncate.txt" > /dev/null
stat "$MOUNTPOINT/truncate.txt" > /dev/null 2>&1
truncate -s 4 "$MOUNTPOINT/truncate.txt"

TRUNC_SIZE=$(wc -c < "$MOUNTPOINT/truncate.txt" | tr -d ' ')
[ "$TRUNC_SIZE" = "4" ] || fail "truncate.txt size is stale after truncate: $TRUNC_SIZE"
TRUNC_CONTENT=$(cat "$MOUNTPOINT/truncate.txt")
[ "$TRUNC_CONTENT" = "abcd" ] || fail "truncate.txt content is stale after truncate: $TRUNC_CONTENT"

# Test 7: repeated read/open cache must not serve stale data after write
printf "cache-before" > "$MOUNTPOINT/keep-cache.txt"
cat "$MOUNTPOINT/keep-cache.txt" > /dev/null
cat "$MOUNTPOINT/keep-cache.txt" > /dev/null
printf "cache-after" > "$MOUNTPOINT/keep-cache.txt"
KEEP_CACHE_CONTENT=$(cat "$MOUNTPOINT/keep-cache.txt")
[ "$KEEP_CACHE_CONTENT" = "cache-after" ] ||
    fail "keep-cache.txt content is stale after overwrite: $KEEP_CACHE_CONTENT"
truncate -s 5 "$MOUNTPOINT/keep-cache.txt"
KEEP_CACHE_CONTENT=$(cat "$MOUNTPOINT/keep-cache.txt")
[ "$KEEP_CACHE_CONTENT" = "cache" ] ||
    fail "keep-cache.txt content is stale after truncate: $KEEP_CACHE_CONTENT"

# Unmount
unmount_quiet

# Wait for mount process to exit
wait $MOUNT_PID 2>/dev/null || true
MOUNT_PID=""

echo "OK"
