#!/bin/sh
set -eu

echo -n "TEST interactive bash session... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-run-bash.XXXXXX")"

cleanup() {
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

run_agentfs() {
    if [ -n "${AGENTFS_BIN:-}" ]; then
        "$AGENTFS_BIN" "$@"
    else
        cargo run --quiet --manifest-path "$CLI_DIR/Cargo.toml" -- "$@"
    fi
}

# The temp root is the overlay base layer; the session DB lands under
# $ROOT/.agentfs instead of the repo working tree.
cd "$ROOT"

# Run bash session in overlay: write a file and read it back
# The current directory becomes copy-on-write with the overlay sandbox
output=$(run_agentfs run /bin/bash -c '
echo "hello from agent" > hello.txt
cat hello.txt
' 2>&1)

# Verify we got the expected output
echo "$output" | grep -q "hello from agent" || {
    echo "FAILED"
    echo "$output"
    exit 1
}

# Verify the file was NOT written to the host (it's in the delta layer)
if [ -f "hello.txt" ]; then
    echo "FAILED: hello.txt should not exist on host filesystem"
    exit 1
fi

echo "OK"
