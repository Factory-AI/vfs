#!/bin/sh
set -u

# Set AGENTFS_GATE_FORCE_SKIP=<label|all> to force a synthetic SKIP without
# running the selected test. This hook exists only to validate SKIP accounting.

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
cd "$CLI_DIR"

PASS_COUNT=0
SKIP_COUNT=0
FAIL_COUNT=0
RESULTS=""

truthy() {
    case "${1:-}" in
        1|true|TRUE|yes|YES|on|ON)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

record_result() {
    label="$1"
    result="$2"

    RESULTS="${RESULTS}${label}: ${result}
"
    case "$result" in
        PASS)
            PASS_COUNT=$((PASS_COUNT + 1))
            ;;
        SKIP)
            SKIP_COUNT=$((SKIP_COUNT + 1))
            ;;
        FAIL)
            FAIL_COUNT=$((FAIL_COUNT + 1))
            ;;
    esac
    printf 'RESULT %s: %s\n' "$label" "$result"
}

run_test() {
    label="$1"
    shift

    printf '\n==> %s\n' "$label"
    tmp="$(mktemp "${TMPDIR:-/tmp}/agentfs-all.${label}.XXXXXX")"

    if [ "${AGENTFS_GATE_FORCE_SKIP:-}" = "$label" ] ||
        [ "${AGENTFS_GATE_FORCE_SKIP:-}" = "all" ]; then
        printf 'SKIP: forced by AGENTFS_GATE_FORCE_SKIP=%s\n' "${AGENTFS_GATE_FORCE_SKIP:-}" >"$tmp"
        status=0
    else
        "$@" >"$tmp" 2>&1
        status=$?
    fi

    cat "$tmp"

    if [ "$status" -eq 0 ]; then
        if grep -Eq '(^|[[:space:]])SKIP:' "$tmp"; then
            record_result "$label" SKIP
        else
            record_result "$label" PASS
        fi
    else
        record_result "$label" FAIL
    fi

    rm -f "$tmp"
}

run_test "init" "$DIR/test-init.sh"

# Syscall tests in two configurations:
# 1. Linux baseline, establishes expected behavior.
run_test "linux-syscalls" "$DIR/test-linux-syscalls.sh"

# 2. FUSE overlay through agentfs run, tests copy-on-write.
run_test "run-syscalls" "$DIR/test-run-syscalls.sh"

run_test "run-bash" "$DIR/test-run-bash.sh"
run_test "run-git" "$DIR/test-run-git.sh"
run_test "run-read-scoping" "$DIR/test-run-read-scoping.sh"
run_test "profile-error-summary" "$DIR/test-profile-error-summary.sh"
run_test "ephemeral-sidecar-cleanup" "$DIR/test-ephemeral-sidecar-cleanup.sh"
run_test "teardown-bounded" "$DIR/test-teardown-bounded.sh"
run_test "fuse-sigint-panic-census" "$DIR/test-fuse-sigint-panic-census.sh"
run_test "sigkill-recovery" "$DIR/test-sigkill-recovery.sh"
run_test "signal-teardown" env \
    SIGNAL_TEARDOWN_SIGNAL_DELAY="${SIGNAL_TEARDOWN_SIGNAL_DELAY:-8s}" \
    "$DIR/test-signal-teardown.sh"

# Corruption/concurrency torture runs both FUSE transport legs. The test prints
# SKIP and exits 0 if Linux user namespace/FUSE prerequisites are unavailable;
# AGENTFS_GATE_STRICT=1 makes such skips fail the gate on the designated runner.
run_test "corruption-torture-legacy" env \
    AGENTFS_FUSE_URING=0 \
    CORRUPTION_TORTURE_WORKERS="${CORRUPTION_TORTURE_WORKERS:-4}" \
    CORRUPTION_TORTURE_ITERATIONS="${CORRUPTION_TORTURE_ITERATIONS:-3}" \
    CORRUPTION_TORTURE_TIMEOUT="${CORRUPTION_TORTURE_TIMEOUT:-120}" \
    CORRUPTION_TORTURE_TEARDOWN_TIMEOUT="${CORRUPTION_TORTURE_TEARDOWN_TIMEOUT:-10}" \
    CORRUPTION_TORTURE_INTEGRITY_INTERVAL="${CORRUPTION_TORTURE_INTEGRITY_INTERVAL:-1}" \
    "$DIR/test-corruption-torture.sh"
run_test "corruption-torture-uring" env \
    AGENTFS_FUSE_URING=1 \
    CORRUPTION_TORTURE_WORKERS="${CORRUPTION_TORTURE_WORKERS:-4}" \
    CORRUPTION_TORTURE_ITERATIONS="${CORRUPTION_TORTURE_ITERATIONS:-3}" \
    CORRUPTION_TORTURE_TIMEOUT="${CORRUPTION_TORTURE_TIMEOUT:-120}" \
    CORRUPTION_TORTURE_TEARDOWN_TIMEOUT="${CORRUPTION_TORTURE_TEARDOWN_TIMEOUT:-10}" \
    CORRUPTION_TORTURE_INTEGRITY_INTERVAL="${CORRUPTION_TORTURE_INTEGRITY_INTERVAL:-1}" \
    "$DIR/test-corruption-torture.sh"

run_test "mount" "$DIR/test-mount.sh"
run_test "overlay-whiteout" "$DIR/test-overlay-whiteout.sh"
run_test "overlay-delta-in-base-dir" "$DIR/test-overlay-delta-in-base-dir.sh"
run_test "overlay-base-dir-rename-exdev" "$DIR/test-overlay-base-dir-rename-exdev.sh"
run_test "fuse-cache-invalidation" "$DIR/test-fuse-cache-invalidation.sh"
run_test "symlinks" "$DIR/test-symlinks.sh"

printf '\nShell gate results:\n%s' "$RESULTS"
printf 'SUMMARY PASS=%s SKIP=%s FAIL=%s\n' "$PASS_COUNT" "$SKIP_COUNT" "$FAIL_COUNT"

if [ "$FAIL_COUNT" -ne 0 ]; then
    exit 1
fi

if [ "$SKIP_COUNT" -ne 0 ] && truthy "${AGENTFS_GATE_STRICT:-0}"; then
    printf 'FAILED: AGENTFS_GATE_STRICT=1 treats SKIP as a gate failure\n'
    exit 1
fi

exit 0
