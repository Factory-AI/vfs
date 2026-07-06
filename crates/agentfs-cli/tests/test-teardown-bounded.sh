#!/bin/sh
#
# Bounded teardown regression for `agentfs run --session`.
#
# The owner process must exit promptly after SIGTERM while a real FUSE session
# has had joiners and filesystem activity. This exercises both the legacy
# /dev/fuse reader path and the FUSE-over-io_uring path on capable kernels.
#
# A third leg covers the parent abort error path: when the sandbox child dies
# before the namespace handshake completes, the parent must tear down its live
# FUSE mount gracefully before exiting instead of leaving a dead mount table
# entry behind.
#
set -eu

echo -n "TEST bounded FUSE teardown on SIGTERM... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
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
    AGENTFS_BIN="$REPO_ROOT/target/debug/agentfs"
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
"${GIT_REAL:-git}" init -q
"${GIT_REAL:-git}" config user.email "bounded@example.invalid"
"${GIT_REAL:-git}" config user.name "Bounded Teardown"
printf "hello\n" > src/file.txt
"${GIT_REAL:-git}" add .
"${GIT_REAL:-git}" commit -q -m init
"${GIT_REAL:-git}" fsck --strict --no-progress
'
    ) >"$JOINER_LOG" 2>&1
}

verify_session_after_teardown() {
    leg="$1"
    verify_log="$LOGDIR/verify-after-term.log"
    integrity_log="$LOGDIR/integrity-after-term.json"

    HOME="$TEST_HOME" \
    XDG_CACHE_HOME="$TEST_HOME/.cache" \
    XDG_CONFIG_HOME="$TEST_HOME/.config" \
    "$AGENTFS_BIN" integrity "$DELTA_DB" --json >"$integrity_log" 2>&1 ||
        {
            dump_failure_context "$leg integrity check failed after SIGTERM teardown"
            sed 's/^/  /' "$integrity_log"
            return 1
        }

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
test "$(cat bounded/payload.txt)" = "bounded teardown payload"
"${GIT_REAL:-git}" -C bounded/repo fsck --strict --no-progress
test -z "$("${GIT_REAL:-git}" -C bounded/repo status --porcelain)"
'
    ) >"$verify_log" 2>&1 || {
        dump_failure_context "$leg remount verification failed after SIGTERM teardown"
        sed 's/^/  /' "$verify_log"
        return 1
    }
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

    # The user's PATH may route `git` through a hook-manager shim that
    # daemonizes out of test repos (library/environment.md); pin the distro
    # binary and give the temp HOME a hookless git config.
    mkdir -p "$TEST_HOME/bin" "$TEST_HOME/git-hooks-none"
    GIT_REAL=""
    for candidate in /usr/bin/git /bin/git; do
        if [ -x "$candidate" ]; then
            GIT_REAL="$candidate"
            break
        fi
    done
    [ -n "$GIT_REAL" ] || GIT_REAL="$(command -v git)"
    ln -sf "$GIT_REAL" "$TEST_HOME/bin/git"
    printf '[core]\n\thooksPath = %s\n' "$TEST_HOME/git-hooks-none" >"$TEST_HOME/.gitconfig"
    PATH="$TEST_HOME/bin:$PATH"
    # The PATH shim only holds host-side: inside `agentfs run` temp dirs are
    # hidden, so sandboxed workloads must call "$GIT_REAL" by absolute path.
    export PATH GIT_REAL

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

    if ! verify_session_after_teardown "$leg"; then
        cleanup_current
        return 1
    fi

    if ! assert_no_session_residue; then
        dump_failure_context "$label verification remount left residue"
        cleanup_current
        return 1
    fi

    cleanup_current
    printf " %s=%sms" "$label" "$elapsed_ms"
    return 0
}

# Parent abort-path teardown: capping max_user_namespaces at 0 inside a nested
# user namespace makes the sandbox child's unshare fail deterministically
# AFTER the parent's FUSE mount is live, driving the parent through its abort
# error path. The parent must exit 1 with the mount gone from the namespace's
# mount table (pre-fix it exited via process::exit without dropping the
# MountHandle, stranding a dead mount entry).
run_abort_leg() {
    if ! command -v unshare >/dev/null 2>&1 || ! unshare -Ur true 2>/dev/null; then
        echo "SKIP: nested user namespaces unavailable for the abort-path leg"
        return 0
    fi

    TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-teardown-bounded.XXXXXX")"
    TEST_HOME="$TEST_ROOT/home"
    WORKDIR="$TEST_ROOT/work"
    LOGDIR="$TEST_ROOT/logs"
    SESSION_ID="abort-path-$$"
    OWNER_LOG="$LOGDIR/abort-leg.log"
    mkdir -p "$TEST_HOME" "$WORKDIR" "$LOGDIR"

    inner="$TEST_ROOT/abort-inner.sh"
    cat >"$inner" <<'INNER'
#!/bin/sh
set -u
BIN="$1"
TEST_HOME="$2"
WORKDIR="$3"
SESSION_ID="$4"
if ! echo 0 >/proc/sys/user/max_user_namespaces 2>/dev/null; then
    echo "ABORT-LEG-SETUP-SKIP"
    exit 0
fi
cd "$WORKDIR" || exit 90
rc=0
HOME="$TEST_HOME" "$BIN" run --session "$SESSION_ID" -- /bin/true || rc=$?
echo "ABORT-LEG-EXIT=$rc"
if grep -qF "$TEST_HOME" /proc/self/mountinfo; then
    echo "ABORT-LEG-STALE-MOUNT=1"
else
    echo "ABORT-LEG-STALE-MOUNT=0"
fi
INNER
    chmod +x "$inner"

    abort_rc=0
    if command -v timeout >/dev/null 2>&1; then
        timeout "$((START_TIMEOUT + TEARDOWN_TIMEOUT))" \
            unshare -Ur -m "$inner" "$AGENTFS_BIN" "$TEST_HOME" "$WORKDIR" "$SESSION_ID" \
            >"$OWNER_LOG" 2>&1 || abort_rc=$?
    else
        unshare -Ur -m "$inner" "$AGENTFS_BIN" "$TEST_HOME" "$WORKDIR" "$SESSION_ID" \
            >"$OWNER_LOG" 2>&1 || abort_rc=$?
    fi

    if grep -q "ABORT-LEG-SETUP-SKIP" "$OWNER_LOG" 2>/dev/null; then
        cleanup_current
        echo "SKIP: cannot cap max_user_namespaces inside a nested user namespace"
        return 0
    fi

    if [ "$abort_rc" -ne 0 ]; then
        dump_failure_context "abort-path leg did not complete within $((START_TIMEOUT + TEARDOWN_TIMEOUT))s (rc=$abort_rc)"
        kill_abort_leg_stragglers
        cleanup_current
        return 1
    fi
    if ! grep -q "ABORT-LEG-EXIT=1" "$OWNER_LOG"; then
        dump_failure_context "abort-path leg: run did not exit 1 after the child handshake failure"
        kill_abort_leg_stragglers
        cleanup_current
        return 1
    fi
    if ! grep -q "ABORT-LEG-STALE-MOUNT=0" "$OWNER_LOG"; then
        dump_failure_context "abort-path leg: FUSE mount left in the mount table after the parent abort"
        kill_abort_leg_stragglers
        cleanup_current
        return 1
    fi

    cleanup_current
    printf " abort-path=OK"
    return 0
}

# A timed-out abort leg can strand the namespaced agentfs owner (timeout only
# kills the unshare wrapper); reap it by PID via its unique session id.
kill_abort_leg_stragglers() {
    ps -e -o pid= -o args= | grep "$SESSION_ID" | grep agentfs | grep -v grep |
        while read -r pid _; do
            kill -KILL "$pid" 2>/dev/null || true
        done
}

failed=0
for leg in 1 0; do
    if ! run_leg "$leg"; then
        failed=1
    fi
done

if ! run_abort_leg; then
    failed=1
fi

if [ "$failed" -ne 0 ]; then
    exit 1
fi

echo " OK"
