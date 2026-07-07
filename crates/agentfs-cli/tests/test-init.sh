#!/bin/sh
set -eu

echo -n "TEST init... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

TEST_AGENT_ID="test-agent"
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-init.XXXXXX")"

cleanup() {
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

# agentfs init creates its DB under CWD/.agentfs, so run from the temp root.
cd "$ROOT"

output=$(run_agentfs init "$TEST_AGENT_ID" 2>&1) ||
    fail "init command failed
Output was: $output"

[ -d .agentfs ] || fail ".agentfs directory was not created
Output was: $output"

[ -f ".agentfs/$TEST_AGENT_ID.db" ] ||
    fail "agent database was not created in .agentfs directory
Output was: $output"

echo "$output" | grep -q "Created agent filesystem: .agentfs/$TEST_AGENT_ID.db" ||
    fail "Expected success message not found in output
Output was: $output"

# Running init again must fail without --force.
if run_agentfs init "$TEST_AGENT_ID" 2>&1 | grep -q "already exists"; then
    : # Expected behavior
else
    fail "init should fail when agent database already exists"
fi

output=$(run_agentfs init "$TEST_AGENT_ID" --force 2>&1) ||
    fail "init --force command failed
Output was: $output"

echo "$output" | grep -q "Created agent filesystem: .agentfs/$TEST_AGENT_ID.db" ||
    fail "Expected success message not found in init --force output
Output was: $output"

echo "OK"
