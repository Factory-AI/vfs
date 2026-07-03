#!/bin/sh
#
# Bounded teardown regression for `agentfs run --session`.
#
# The owner process must exit promptly after SIGTERM while a real FUSE session
# has had joiners and filesystem activity. This exercises both the legacy
# /dev/fuse reader path and the FUSE-over-io_uring path on capable kernels.
#
set -eu

echo -n "TEST bounded FUSE teardown on SIGTERM... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
CARGO_MANIFEST="$CLI_DIR/Cargo.toml"
AGENTFS_BIN="${AGENTFS_BIN:-}"
TEARDOWN_TIMEOUT="${TEARDOWN_TIMEOUT:-10}"
START_TIMEOUT="${TEARDOWN_START_TIMEOUT:-30}"
HOST_HOME="${HOME:-}"
CARGO_HOME_FOR_TEST="${CARGO_HOME:-$HOST_HOME/.cargo}"
RUSTUP_HOME_FOR_TEST="${RUSTUP_HOME:-$HOST_HOME/.rustup}"
RUSTUP_TOOLCHAIN_FOR_TEST="${RUSTUP_TOOLCHAIN:-nightly}"

TEST_ROOT=""
TEST_HOME=""
WORKDIR=""
LOGDIR=""
SESSION_ID=""
RUN_DIR=""
DELTA_DB=""
FUSE_MNT=""
OWNER_PID=""
OWNER_LOG=""
JOINER_LOG=""

skip() {
    echo "SKIP: $*"
    exit 0
}

validate_positive_integer() {
    name="$1"
    value="$2"
    case "$value" in
        ''|*[!0-9]*)
            echo "FAILED: $name must be a positive integer, got '$value'"
            exit 1
            ;;
    esac
    if [ "$value" -le 0 ]; then
        echo "FAILED: $name must be a positive integer, got '$value'"
        exit 1
    fi
}

case "$(uname -s)" in
    Linux)
        ;;
    *)
        skip "requires Linux namespaces and FUSE"
        ;;
esac

command -v cargo >/dev/null 2>&1 || skip "cargo is unavailable"
command -v git >/dev/null 2>&1 || skip "git is unavailable"
command -v mountpoint >/dev/null 2>&1 || skip "mountpoint is unavailable"
[ -x /bin/bash ] || skip "/bin/bash is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"

if [ -r /proc/sys/kernel/unprivileged_userns_clone ] &&
    [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
    skip "unprivileged user namespaces are disabled"
fi

validate_positive_integer TEARDOWN_TIMEOUT "$TEARDOWN_TIMEOUT"
validate_positive_integer TEARDOWN_START_TIMEOUT "$START_TIMEOUT"

if [ -z "$AGENTFS_BIN" ]; then
    cargo build --quiet --manifest-path "$CARGO_MANIFEST" >/dev/null 2>&1 ||
        {
            echo "FAILED: failed to build agentfs CLI"
            exit 1
        }
    AGENTFS_BIN="$CLI_DIR/target/debug/agentfs"
fi

if [ ! -x "$AGENTFS_BIN" ]; then
    echo "FAILED: AGENTFS_BIN is not executable: $AGENTFS_BIN"
    exit 1
fi

unmount_if_needed() {
    if [ -n "$FUSE_MNT" ] && command -v mountpoint >/dev/null 2>&1 &&
        mountpoint -q "$FUSE_MNT" 2>/dev/null; then
        if command -v fusermount3 >/dev/null 2>&1; then
            fusermount3 -u "$FUSE_MNT" 2>/dev/null || true
        elif command -v fusermount >/dev/null 2>&1; then
            fusermount -u "$FUSE_MNT" 2>/dev/null || true
        elif command -v umount >/dev/null 2>&1; then
            umount "$FUSE_MNT" 2>/dev/null || true
        fi
    fi
}

cleanup_current() {
    set +e

    if [ -n "$OWNER_PID" ] && kill -0 "$OWNER_PID" 2>/dev/null; then
        kill "$OWNER_PID" 2>/dev/null || true
        waited=0
        while kill -0 "$OWNER_PID" 2>/dev/null && [ "$waited" -lt 20 ]; do
            sleep 0.1
            waited=$((waited + 1))
        done
        if kill -0 "$OWNER_PID" 2>/dev/null; then
            kill -KILL "$OWNER_PID" 2>/dev/null || true
        fi
        wait "$OWNER_PID" 2>/dev/null || true
    fi
    OWNER_PID=""

    unmount_if_needed

    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/agentfs-teardown-bounded.*)
                rm -rf "$TEST_ROOT"
                ;;
            *)
                echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT"
                ;;
        esac
    fi

    TEST_ROOT=""
    TEST_HOME=""
    WORKDIR=""
    LOGDIR=""
    SESSION_ID=""
    RUN_DIR=""
    DELTA_DB=""
    FUSE_MNT=""
    OWNER_LOG=""
    JOINER_LOG=""
    set -e
}

finish_with_signal() {
    cleanup_current
    exit 130
}

trap cleanup_current EXIT
trap finish_with_signal INT TERM

dump_failure_context() {
    label="$1"
    echo
    echo "FAILED: $label"
    if [ -n "$OWNER_LOG" ] && [ -f "$OWNER_LOG" ]; then
        echo "owner log:"
        sed 's/^/  /' "$OWNER_LOG" | tail -n 120
    fi
    if [ -n "$JOINER_LOG" ] && [ -f "$JOINER_LOG" ]; then
        echo "joiner log:"
        sed 's/^/  /' "$JOINER_LOG" | tail -n 120
    fi
    if [ -n "$OWNER_PID" ] && kill -0 "$OWNER_PID" 2>/dev/null; then
        echo "owner threads:"
        ps -T -p "$OWNER_PID" -o pid,tid,comm,wchan:32 2>/dev/null || true
    fi
    if [ -n "$FUSE_MNT" ]; then
        echo "mount table:"
        mount | grep "$FUSE_MNT" || true
    fi
}

wait_for_owner_ready() {
    deadline=$(( $(date +%s) + START_TIMEOUT ))
    while [ "$(date +%s)" -le "$deadline" ]; do
        if [ -f "$DELTA_DB" ] && [ -f "$RUN_DIR/base_path" ] &&
            mountpoint -q "$FUSE_MNT" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$OWNER_PID" 2>/dev/null; then
            wait "$OWNER_PID" 2>/dev/null || true
            return 1
        fi
        sleep 0.2
    done
    return 1
}

run_joiner_workload() {
    leg="$1"
    (
        cd "$WORKDIR"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        CARGO_HOME="$CARGO_HOME_FOR_TEST" \
        RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
        RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
        AGENTFS_FUSE_URING="$leg" \
        "$AGENTFS_BIN" run --session "$SESSION_ID" \
            /bin/bash -c '
set -euo pipefail
mkdir -p bounded/repo/src
printf "bounded teardown payload\n" > bounded/payload.txt
cat bounded/payload.txt >/dev/null
cd bounded/repo
git init -q
git config user.email "bounded@example.invalid"
git config user.name "Bounded Teardown"
printf "hello\n" > src/file.txt
git add .
git commit -q -m init
git fsck --strict --no-progress
'
    ) >"$JOINER_LOG" 2>&1
}

wait_for_owner_exit() {
    start_ms="$(date +%s%3N)"
    deadline_ms=$((start_ms + TEARDOWN_TIMEOUT * 1000))
    while kill -0 "$OWNER_PID" 2>/dev/null; do
        now_ms="$(date +%s%3N)"
        if [ "$now_ms" -ge "$deadline_ms" ]; then
            return 1
        fi
        sleep 0.1
    done
    wait "$OWNER_PID" 2>/dev/null || true
    OWNER_PID=""
    end_ms="$(date +%s%3N)"
    echo $((end_ms - start_ms))
    return 0
}

assert_no_session_residue() {
    if [ -n "$FUSE_MNT" ] && mount | grep "$FUSE_MNT" >/dev/null 2>&1; then
        return 1
    fi
    if ps -e -o pid= -o args= | grep "$SESSION_ID" | grep agentfs | grep -v grep >/dev/null 2>&1; then
        return 1
    fi
    return 0
}

run_leg() {
    leg="$1"
    label="legacy"
    if [ "$leg" = "1" ]; then
        label="uring"
    fi

    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-teardown-bounded.XXXXXX")"
    TEST_HOME="$TEST_ROOT/home"
    WORKDIR="$TEST_ROOT/work"
    LOGDIR="$TEST_ROOT/logs"
    SESSION_ID="teardown-bounded-$$-$leg"
    RUN_DIR="$TEST_HOME/.agentfs/run/$SESSION_ID"
    DELTA_DB="$RUN_DIR/delta.db"
    FUSE_MNT="$RUN_DIR/mnt"
    OWNER_LOG="$LOGDIR/owner.log"
    JOINER_LOG="$LOGDIR/joiner.log"
    mkdir -p "$TEST_HOME/.cache" "$TEST_HOME/.config" "$WORKDIR" "$LOGDIR"

    (
        cd "$WORKDIR"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        CARGO_HOME="$CARGO_HOME_FOR_TEST" \
        RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
        RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
        AGENTFS_FUSE_URING="$leg" \
        RUST_LOG=agentfs=info \
        "$AGENTFS_BIN" run --session "$SESSION_ID" \
            /bin/bash -c 'trap "exit 0" TERM INT; while :; do sleep 1; done'
    ) >"$OWNER_LOG" 2>&1 &
    OWNER_PID=$!

    if ! wait_for_owner_ready; then
        dump_failure_context "$label owner did not become ready"
        cleanup_current
        return 1
    fi

    if ! run_joiner_workload "$leg"; then
        dump_failure_context "$label joiner workload failed"
        cleanup_current
        return 1
    fi

    if [ "$leg" = "1" ] && [ -r /sys/module/fuse/parameters/enable_uring ] &&
        [ "$(cat /sys/module/fuse/parameters/enable_uring)" = "Y" ] &&
        ! grep -q "advertising FUSE_OVER_IO_URING" "$OWNER_LOG"; then
        dump_failure_context "uring leg did not advertise FUSE_OVER_IO_URING"
        cleanup_current
        return 1
    fi

    if [ "$leg" = "0" ] && grep -q "advertising FUSE_OVER_IO_URING" "$OWNER_LOG"; then
        dump_failure_context "legacy leg unexpectedly used FUSE_OVER_IO_URING"
        cleanup_current
        return 1
    fi

    kill "$OWNER_PID" 2>/dev/null || true
    elapsed_ms=""
    if ! elapsed_ms="$(wait_for_owner_exit)"; then
        dump_failure_context "$label owner did not exit within ${TEARDOWN_TIMEOUT}s after SIGTERM"
        cleanup_current
        return 1
    fi

    if ! assert_no_session_residue; then
        dump_failure_context "$label session left mount or process residue after ${elapsed_ms}ms teardown"
        cleanup_current
        return 1
    fi

    cleanup_current
    printf " %s=%sms" "$label" "$elapsed_ms"
    return 0
}

failed=0
for leg in 1 0; do
    if ! run_leg "$leg"; then
        failed=1
    fi
done

if [ "$failed" -ne 0 ]; then
    exit 1
fi

echo " OK"
