#!/bin/sh
#
# Test syscalls through agentfs run (FUSE overlay).
#
# This tests the copy-on-write behavior when files exist in the base layer
# and are modified through the overlay.
#
# Requires user namespaces support.
#
set -eu

echo -n "TEST syscalls (agentfs run - FUSE overlay)... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-run-syscalls.XXXXXX")"
SESSION_ID="run-syscalls-$$"

cleanup() {
    rm -rf "$ROOT" "${HOME}/.agentfs/run/${SESSION_ID}"
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

# Compile the test program (artifacts stay in the source tree; gitignored).
make -C "$DIR/syscall" clean > /dev/null 2>&1
make -C "$DIR/syscall" > /dev/null 2>&1
cp "$DIR/syscall/test-syscalls" "$ROOT/test-syscalls"

# The temp root is the overlay base layer; `init` puts its DB under
# $ROOT/.agentfs instead of the repo working tree. The run session's delta DB
# lands under ~/.agentfs/run/<session>; pass an explicit session id so cleanup
# can remove exactly the session dir this test created (never sweep
# ~/.agentfs/run).
cd "$ROOT"

run_agentfs init > /dev/null 2>&1

# Create pre-existing files in the BASE LAYER (current directory)
# These will trigger copy-on-write when modified through the overlay
echo -n "original content" > existing.txt
echo "Hello from test setup!" > test.txt

# Create nested directory structure for COW parent dir test
# This catches the bug where .git/logs/HEAD fails because parent dirs
# don't exist in delta layer during copy-on-write
mkdir -p subdir
echo -n "nested content" > subdir/nested.txt

# Create file with executable permissions for copy-up permissions test
# This tests that copy-up preserves base layer permissions (not DEFAULT_FILE_MODE)
echo -n "executable content" > executable_base.txt
chmod 0755 executable_base.txt

# Create read-only file (mode 0444) to test open flags handling
# This tests that O_RDONLY works but O_RDWR fails with EACCES
echo -n "readonly content" > readonly.txt
chmod 0444 readonly.txt

# Create files for copy-up inode stability tests
# Each file will be used to test inode stability when a specific syscall triggers copy-up
# These files exist in the base layer; copy-up happens when modified through overlay
echo -n "content for write copyup test" > copyup_write_test.txt
echo -n "content for truncate copyup test" > copyup_truncate_test.txt
echo -n "content for chmod copyup test" > copyup_chmod_test.txt
echo -n "content for chown copyup test" > copyup_chown_test.txt
echo -n "content for rename copyup test" > copyup_rename_test.txt
echo -n "content for link copyup test" > copyup_link_test.txt
echo -n "content for utimes copyup test" > copyup_utimes_test.txt
echo -n "content for xattr copyup test" > copyup_xattr_test.txt
echo -n "content for fallocate copyup test" > copyup_fallocate_test.txt

# Run syscall tests through FUSE overlay
# The test binary runs inside the overlay where:
# - Files from current directory are visible (base layer)
# - Modifications go to the delta layer (AgentFS database)
# - O_APPEND on existing.txt triggers copy-on-write
output=$(run_agentfs run --session "$SESSION_ID" ./test-syscalls . 2>&1) ||
    fail "run exited nonzero
Output was: $output"

echo "$output" | grep -q "All tests passed!" ||
    fail "'All tests passed!' not found
Output was: $output"

# Note: output.txt is created in the delta layer (session-specific) so we can't
# verify it with a separate agentfs run. The "All tests passed!" check is sufficient.

echo "OK"
