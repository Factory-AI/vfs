#!/bin/sh
set -eu

echo -n "TEST symlink handling... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-symlinks.XXXXXX")"
SESSION_PREFIX="symlinks-$$"

cleanup() {
    rm -rf "$ROOT"
    for leg in 1 2 3 4 5; do
        rm -rf "${HOME:?}/.agentfs/run/${SESSION_PREFIX}-${leg}"
    done
}
trap cleanup EXIT INT TERM

run_agentfs() {
    if [ -n "${AGENTFS_BIN:-}" ]; then
        "$AGENTFS_BIN" "$@"
    else
        cargo run --quiet --manifest-path "$CLI_DIR/Cargo.toml" -- "$@"
    fi
}

# The temp root is the overlay base layer. Each leg's session delta DB lands
# under ~/.agentfs/run/<session>; pass explicit session ids so cleanup can
# remove exactly the session dirs this test created (never sweep
# ~/.agentfs/run).
cd "$ROOT"

# Create test directory with symlinks on the host (these will be visible in the sandbox)
TEST_DIR="symlink-test"
mkdir -p "$TEST_DIR/target_dir"
echo "test content" > "$TEST_DIR/target_dir/file.txt"
ln -s target_dir "$TEST_DIR/link_to_dir"
ln -s target_dir/file.txt "$TEST_DIR/link_to_file"

# Test 1 & 2: Verify symlinks are reported correctly (not as directories)
output=$(run_agentfs run --session "$SESSION_PREFIX-1" /bin/bash -c "ls -la $TEST_DIR/" 2>&1)

# The output should contain 'lrwxrwxrwx' for symlinks (not 'drwxr-xr-x' for directory)
if ! echo "$output" | grep -qE "^lrwx.* link_to_dir"; then
    echo "FAILED: symlink to directory not reported as symlink"
    echo "$output"
    exit 1
fi

if ! echo "$output" | grep -qE "^lrwx.* link_to_file"; then
    echo "FAILED: symlink to file not reported as symlink"
    echo "$output"
    exit 1
fi

# Test 3: Verify rm can remove symlink to directory (this was the original bug)
# Previously this would fail with "Is a directory" because symlinks were misidentified
output=$(run_agentfs run --session "$SESSION_PREFIX-2" /bin/bash -c "rm $TEST_DIR/link_to_dir && echo 'symlink removed successfully'" 2>&1)

if ! echo "$output" | grep -q "symlink removed successfully"; then
    echo "FAILED: could not remove symlink to directory"
    echo "$output"
    exit 1
fi

# Test 4: Verify the target directory still exists on host after removing symlink
# (The removal was in the delta layer, host should still have it)
if ! cat "$TEST_DIR/target_dir/file.txt" | grep -q "test content"; then
    echo "FAILED: target directory should still exist after removing symlink"
    exit 1
fi

# Test 5: Create a symlink inside the sandbox (tests FUSE symlink creation)
output=$(run_agentfs run --session "$SESSION_PREFIX-3" /bin/bash -c "ln -s target_dir/file.txt $TEST_DIR/new_symlink && readlink $TEST_DIR/new_symlink" 2>&1)

if ! echo "$output" | grep -q "target_dir/file.txt"; then
    echo "FAILED: could not create symlink in sandbox"
    echo "$output"
    exit 1
fi

# Test 6: Create and follow symlink to read file content
output=$(run_agentfs run --session "$SESSION_PREFIX-4" /bin/bash -c "ln -s target_dir $TEST_DIR/new_dir_link && cat $TEST_DIR/new_dir_link/file.txt" 2>&1)

if ! echo "$output" | grep -q "test content"; then
    echo "FAILED: could not read through newly created symlink"
    echo "$output"
    exit 1
fi

# Test 7: Verify symlinks created in sandbox are visible via ls -l
output=$(run_agentfs run --session "$SESSION_PREFIX-5" /bin/bash -c "ln -s foo $TEST_DIR/test_link && ls -la $TEST_DIR/test_link" 2>&1)

if ! echo "$output" | grep -qE "^lrwx.*test_link -> foo"; then
    echo "FAILED: newly created symlink not shown correctly in ls"
    echo "$output"
    exit 1
fi

echo "OK"
