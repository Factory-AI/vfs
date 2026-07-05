#!/bin/sh
set -u

# Read scoping (sacred invariant 2 / VAL-CROSS-015): inside `agentfs run
# --no-default-allows` a command must not be able to read host files placed
# outside the allowed set (home and temp dirs are hidden), while re-running
# with `--allow <dir>` re-exposes that directory; writes to scoped host paths
# never reach the host.

echo "TEST run read scoping..."

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

run_agentfs() {
    if [ -n "${AGENTFS_BIN:-}" ]; then
        "$AGENTFS_BIN" "$@"
    else
        cargo run --quiet --manifest-path "$CLI_DIR/Cargo.toml" -- "$@"
    fi
}

session_prefix="scope-test-$$"
tmp_secret_dir="$(mktemp -d)"
home_secret_dir="$(mktemp -d "${HOME}/.agentfs-scope-test.XXXXXX")"

cleanup() {
    rm -rf "$tmp_secret_dir" "$home_secret_dir"
    rm -rf "${HOME}/.agentfs/run/${session_prefix}-denied" \
        "${HOME}/.agentfs/run/${session_prefix}-write" \
        "${HOME}/.agentfs/run/${session_prefix}-allowed"
}
trap cleanup EXIT

echo "s3cr3t-tmp" >"$tmp_secret_dir/secret.txt"
echo "s3cr3t-home" >"$home_secret_dir/secret.txt"

fail() {
    echo "FAILED: $1"
    shift
    for extra in "$@"; do
        echo "$extra"
    done
    exit 1
}

# Leg 1 (denied reads): neither the tmp nor the home secret is reachable.
denied_output=$(run_agentfs run --no-default-allows --session "${session_prefix}-denied" \
    /bin/sh -c "cat '$tmp_secret_dir/secret.txt'; cat '$home_secret_dir/secret.txt'" 2>&1)
denied_status=$?
case "$denied_output" in
*s3cr3t*) fail "secret bytes leaked into the denied sandbox" "$denied_output" ;;
esac
[ "$denied_status" -ne 0 ] || fail "denied run exited 0; expected the final cat to fail" "$denied_output"

# Leg 2 (denied writes): a write to a scoped host path must not reach the host.
write_output=$(run_agentfs run --no-default-allows --session "${session_prefix}-write" \
    /bin/sh -c "echo intruder >'$tmp_secret_dir/evil.txt'" 2>&1)
[ ! -f "$tmp_secret_dir/evil.txt" ] || fail "sandbox write escaped to the host" "$write_output"
[ "$(cat "$tmp_secret_dir/secret.txt")" = "s3cr3t-tmp" ] || fail "host secret was mutated" "$write_output"

# Leg 3 (allowed reads): --allow re-exposes the directory with exact bytes.
allowed_output=$(run_agentfs run --no-default-allows --allow "$tmp_secret_dir" \
    --session "${session_prefix}-allowed" \
    /bin/sh -c "cat '$tmp_secret_dir/secret.txt'" 2>&1)
allowed_status=$?
[ "$allowed_status" -eq 0 ] || fail "allowed run failed" "$allowed_output"
case "$allowed_output" in
*s3cr3t-tmp*) : ;;
*) fail "allowed read did not return the secret bytes" "$allowed_output" ;;
esac

# Host census: the secret dirs hold exactly the files this test created.
[ "$(ls -A "$tmp_secret_dir")" = "secret.txt" ] || fail "unexpected host mutation in $tmp_secret_dir"
[ "$(ls -A "$home_secret_dir")" = "secret.txt" ] || fail "unexpected host mutation in $home_secret_dir"

echo "OK"
