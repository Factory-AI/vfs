#!/usr/bin/env sh
#
# Second-opener-of-one-DB regression (m7-cli-polish, VAL-CLI-025):
# while one `agentfs exec` holds a database open through a mount, a second
# opener of the same database must fail cleanly — exit nonzero with a single
# concise `Error:` line (turso file lock), bounded by a timeout, without
# panicking, and without disturbing the first session. Afterwards integrity
# on the database must be clean.
set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

echo -n "TEST second opener of one DB... "

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-second-opener.XXXXXX")"
FIRST_PID=""

cleanup() {
    if [ -n "$FIRST_PID" ]; then
        kill "$FIRST_PID" 2>/dev/null || true
        wait "$FIRST_PID" 2>/dev/null || true
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
        cargo +nightly run --quiet --manifest-path "$CLI_DIR/Cargo.toml" -- "$@"
    fi
}

# agentfs_dir() is cwd-relative, so init from inside $ROOT.
(cd "$ROOT" && run_agentfs init shared) >"$ROOT/init.log" 2>&1 ||
    fail "init failed: $(cat "$ROOT/init.log")"
DB="$ROOT/.agentfs/shared.db"
[ -f "$DB" ] || fail "init did not create $DB"
run_agentfs fs "$DB" write /keep.txt kept >"$ROOT/write.log" 2>&1 ||
    fail "fs write failed: $(cat "$ROOT/write.log")"

# First opener: exec holds the DB (and its file lock) while the child sleeps.
run_agentfs exec "$DB" sh -c 'echo first-ready && sleep 8' \
    >"$ROOT/first.log" 2>&1 &
FIRST_PID=$!

WAITED=0
while [ "$WAITED" -lt 40 ]; do
    if grep -q 'first-ready' "$ROOT/first.log" 2>/dev/null; then
        break
    fi
    kill -0 "$FIRST_PID" 2>/dev/null || fail "first exec exited early: $(cat "$ROOT/first.log")"
    sleep 0.25
    WAITED=$((WAITED + 1))
done
grep -q 'first-ready' "$ROOT/first.log" || fail "first exec never became ready"

# Second opener must fail cleanly and promptly: nonzero exit, exactly one
# Error: line, no panic/debug formatting, bounded (no hang).
run_agentfs exec "$DB" sh -c true >"$ROOT/second.log" 2>&1 &
SECOND_PID=$!
WAITED=0
while kill -0 "$SECOND_PID" 2>/dev/null; do
    if [ "$WAITED" -ge 120 ]; then
        kill "$SECOND_PID" 2>/dev/null || true
        fail "second opener hung past 30s: $(cat "$ROOT/second.log")"
    fi
    sleep 0.25
    WAITED=$((WAITED + 1))
done
set +e
wait "$SECOND_PID"
SECOND_STATUS=$?
set -e

[ "$SECOND_STATUS" -ne 0 ] || fail "second opener unexpectedly succeeded: $(cat "$ROOT/second.log")"

ERROR_LINES=$(grep -c '^Error: ' "$ROOT/second.log" || true)
[ "$ERROR_LINES" -eq 1 ] || fail "expected exactly one Error: line, got $ERROR_LINES: $(cat "$ROOT/second.log")"
if grep -Eq 'panicked|Error \{|backtrace' "$ROOT/second.log"; then
    fail "second opener leaked panic/debug formatting: $(cat "$ROOT/second.log")"
fi

# The first session must finish normally and the DB must stay healthy.
wait "$FIRST_PID"
FIRST_STATUS=$?
FIRST_PID=""
[ "$FIRST_STATUS" -eq 0 ] || fail "first exec failed after second-opener attempt: $(cat "$ROOT/first.log")"

run_agentfs fs "$DB" cat /keep.txt >"$ROOT/cat.log" 2>&1 ||
    fail "post-test cat failed: $(cat "$ROOT/cat.log")"
grep -q 'kept' "$ROOT/cat.log" || fail "post-test content mismatch: $(cat "$ROOT/cat.log")"

run_agentfs integrity --json "$DB" >"$ROOT/integrity.json" 2>&1 ||
    fail "integrity failed: $(cat "$ROOT/integrity.json")"

echo "OK"
