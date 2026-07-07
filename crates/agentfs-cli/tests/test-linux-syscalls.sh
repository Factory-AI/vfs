#!/bin/sh
#
# Test syscalls directly on Linux (baseline).
#
# This establishes the expected behavior that AgentFS should match.
#
set -e

echo -n "TEST syscalls (Linux baseline)... "

DIR="$(dirname "$0")"

# Compile the test program
make -C "$DIR/syscall" clean > /dev/null 2>&1
make -C "$DIR/syscall" > /dev/null 2>&1

# Create a temporary test directory
TEST_DIR=$(mktemp -d)
trap "rm -rf '$TEST_DIR'" EXIT

# Create pre-existing files for overlay-style tests
echo -n "original content" > "$TEST_DIR/existing.txt"
echo "Hello from test setup!" > "$TEST_DIR/test.txt"

# Create nested directory structure for COW parent dir test
mkdir -p "$TEST_DIR/subdir"
echo -n "nested content" > "$TEST_DIR/subdir/nested.txt"

# Create read-only file (mode 0444) to test open flags handling
echo -n "readonly content" > "$TEST_DIR/readonly.txt"
chmod 0444 "$TEST_DIR/readonly.txt"

# Run syscall tests directly on Linux
if ! output=$("$DIR/syscall/test-syscalls" "$TEST_DIR" 2>&1); then
    echo "FAILED"
    echo "Output was: $output"
    exit 1
fi

echo "$output" | grep -q "All tests passed!" || {
    echo "FAILED: 'All tests passed!' not found"
    echo "Output was: $output"
    exit 1
}

echo "OK"
