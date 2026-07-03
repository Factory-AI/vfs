#!/bin/sh
#
# Test that FUSE kernel cache is properly invalidated after mutations.
#
# After readdirplus populates the dcache, unlink/rmdir/rename must
# invalidate the affected entries so subsequent readdir sees the change.
#
set -e

echo -n "TEST fuse cache invalidation after mutations... "

TEST_AGENT_ID="test-fuse-cache-inval-agent"
MOUNTPOINT="/tmp/agentfs-test-cache-inval-$$"

cleanup() {
    # Unmount if mounted
    fusermount -u "$MOUNTPOINT" 2>/dev/null || true
    # Wait for FUSE process to exit after unmount
    [ -n "$MOUNT_PID" ] && wait $MOUNT_PID 2>/dev/null || true
    # Remove mountpoint
    rmdir "$MOUNTPOINT" 2>/dev/null || true
    # Remove test database
    rm -f ".agentfs/${TEST_AGENT_ID}.db" ".agentfs/${TEST_AGENT_ID}.db-shm" ".agentfs/${TEST_AGENT_ID}.db-wal"
}

# Ensure cleanup on exit
trap cleanup EXIT

# Clean up any existing test artifacts
cleanup

# Initialize the database
cargo run -- init "$TEST_AGENT_ID" > /dev/null 2>&1

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
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

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
    echo "FAILED: readdir still shows file1.txt after unlink"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

if ! echo "$LS_OUTPUT" | grep -q "file2.txt"; then
    echo "FAILED: file2.txt disappeared"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi
if stat "$MOUNTPOINT/file1.txt" > /dev/null 2>&1; then
    echo "FAILED: stat still resolves file1.txt after unlink"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test 2: rmdir should not leave stale entries in readdir
mkdir "$MOUNTPOINT/subdir"
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/subdir" > /dev/null 2>&1
ls -la "$MOUNTPOINT" > /dev/null

rmdir "$MOUNTPOINT/subdir"

LS_OUTPUT=$(ls "$MOUNTPOINT")
if echo "$LS_OUTPUT" | grep -q "subdir"; then
    echo "FAILED: readdir still shows subdir after rmdir"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi
if stat "$MOUNTPOINT/subdir" > /dev/null 2>&1; then
    echo "FAILED: stat still resolves subdir after rmdir"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test 3: rename should not leave stale source entry in readdir
echo "rename me" > "$MOUNTPOINT/before.txt"
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/before.txt" > /dev/null 2>&1
ls -la "$MOUNTPOINT" > /dev/null

mv "$MOUNTPOINT/before.txt" "$MOUNTPOINT/after.txt"

LS_OUTPUT=$(ls "$MOUNTPOINT")
if echo "$LS_OUTPUT" | grep -q "before.txt"; then
    echo "FAILED: readdir still shows before.txt after rename"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

if ! echo "$LS_OUTPUT" | grep -q "after.txt"; then
    echo "FAILED: after.txt not visible after rename"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi
if stat "$MOUNTPOINT/before.txt" > /dev/null 2>&1; then
    echo "FAILED: stat still resolves before.txt after rename"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi
if [ "$(cat "$MOUNTPOINT/after.txt")" != "rename me" ]; then
    echo "FAILED: after.txt content is stale after rename"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test 4: create must defeat a cached negative dentry
# Prime a negative dentry: stat a name that doesn't exist yet
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/negfile.txt" > /dev/null 2>&1 || true   # caches ENOENT
ls -la "$MOUNTPOINT" > /dev/null                          # readdirplus confirms absence

echo "new file" > "$MOUNTPOINT/negfile.txt"

# stat must resolve (not serve cached ENOENT)
if ! stat "$MOUNTPOINT/negfile.txt" > /dev/null 2>&1; then
    echo "FAILED: stat returns ENOENT for negfile.txt after create (negative dentry not invalidated)"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# readdir must list it
LS_OUTPUT=$(ls "$MOUNTPOINT")
if ! echo "$LS_OUTPUT" | grep -q "negfile.txt"; then
    echo "FAILED: readdir does not show negfile.txt after create"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test 5: mkdir must defeat a cached negative dentry
ls -la "$MOUNTPOINT" > /dev/null
stat "$MOUNTPOINT/negdir" > /dev/null 2>&1 || true        # caches ENOENT
ls -la "$MOUNTPOINT" > /dev/null                          # readdirplus confirms absence

mkdir "$MOUNTPOINT/negdir"

if ! stat "$MOUNTPOINT/negdir" > /dev/null 2>&1; then
    echo "FAILED: stat returns ENOENT for negdir after mkdir (negative dentry not invalidated)"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

LS_OUTPUT=$(ls "$MOUNTPOINT")
if ! echo "$LS_OUTPUT" | grep -q "negdir"; then
    echo "FAILED: readdir does not show negdir after mkdir"
    echo "ls output was: $LS_OUTPUT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test 6: truncate must invalidate stale attrs and cached file data
printf "abcdefghij" > "$MOUNTPOINT/truncate.txt"
cat "$MOUNTPOINT/truncate.txt" > /dev/null
stat "$MOUNTPOINT/truncate.txt" > /dev/null 2>&1
truncate -s 4 "$MOUNTPOINT/truncate.txt"

TRUNC_SIZE=$(wc -c < "$MOUNTPOINT/truncate.txt" | tr -d ' ')
if [ "$TRUNC_SIZE" != "4" ]; then
    echo "FAILED: truncate.txt size is stale after truncate: $TRUNC_SIZE"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi
TRUNC_CONTENT=$(cat "$MOUNTPOINT/truncate.txt")
if [ "$TRUNC_CONTENT" != "abcd" ]; then
    echo "FAILED: truncate.txt content is stale after truncate: $TRUNC_CONTENT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Test 7: repeated read/open cache must not serve stale data after write
printf "cache-before" > "$MOUNTPOINT/keep-cache.txt"
cat "$MOUNTPOINT/keep-cache.txt" > /dev/null
cat "$MOUNTPOINT/keep-cache.txt" > /dev/null
printf "cache-after" > "$MOUNTPOINT/keep-cache.txt"
KEEP_CACHE_CONTENT=$(cat "$MOUNTPOINT/keep-cache.txt")
if [ "$KEEP_CACHE_CONTENT" != "cache-after" ]; then
    echo "FAILED: keep-cache.txt content is stale after overwrite: $KEEP_CACHE_CONTENT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi
truncate -s 5 "$MOUNTPOINT/keep-cache.txt"
KEEP_CACHE_CONTENT=$(cat "$MOUNTPOINT/keep-cache.txt")
if [ "$KEEP_CACHE_CONTENT" != "cache" ]; then
    echo "FAILED: keep-cache.txt content is stale after truncate: $KEEP_CACHE_CONTENT"
    kill $MOUNT_PID 2>/dev/null || true
    wait $MOUNT_PID 2>/dev/null || true
    exit 1
fi

# Unmount
fusermount -u "$MOUNTPOINT"

# Wait for mount process to exit
wait $MOUNT_PID 2>/dev/null || true

echo "OK"
