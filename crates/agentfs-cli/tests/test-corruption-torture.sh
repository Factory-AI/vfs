#!/bin/sh
#
# Concurrency/corruption torture smoke for `agentfs run --session`.
#
# The workload keeps one owner session alive while concurrent joiners mutate
# isolated worker directories in the same delta database. A background monitor
# runs SQLite integrity checks during the workload, and final verification runs
# from inside the still-live session before the owner is terminated.
#
set -eu

echo -n "TEST corruption torture (agentfs run session concurrency)... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
CARGO_MANIFEST="$CLI_DIR/Cargo.toml"
HOST_HOME="${HOME:-}"
CARGO_HOME_FOR_TEST="${CARGO_HOME:-$HOST_HOME/.cargo}"
RUSTUP_HOME_FOR_TEST="${RUSTUP_HOME:-$HOST_HOME/.rustup}"
RUSTUP_TOOLCHAIN_FOR_TEST="${RUSTUP_TOOLCHAIN:-nightly}"

WORKERS="${CORRUPTION_TORTURE_WORKERS:-4}"
ITERATIONS="${CORRUPTION_TORTURE_ITERATIONS:-3}"
TEST_TIMEOUT="${CORRUPTION_TORTURE_TIMEOUT:-90}"
TEARDOWN_TIMEOUT="${CORRUPTION_TORTURE_TEARDOWN_TIMEOUT:-10}"
INTEGRITY_INTERVAL="${CORRUPTION_TORTURE_INTEGRITY_INTERVAL:-1}"
INTEGRITY_TIMEOUT="${CORRUPTION_TORTURE_INTEGRITY_TIMEOUT:-5}"
START_TIMEOUT="${CORRUPTION_TORTURE_START_TIMEOUT:-30}"

TEST_ROOT=""
TEST_HOME=""
WORKDIR=""
LOGDIR=""
SESSION_ID=""
RUN_DIR=""
DELTA_DB=""
FUSE_MNT=""
OWNER_PID=""
MONITOR_PID=""
WATCHDOG_PID=""
WORKER_PIDS=""
STOP_MONITOR=""
MONITOR_FAILED=""
WORKERS_ACTIVE=""
MONITOR_OVERLAP_COUNT=""
OWNER_LOG=""
MONITOR_LOG=""
TIMEOUT_FLAG=""
OWNER_TEARDOWN_MS=""

skip() {
    echo "SKIP: $*"
    exit 0
}

fail() {
    echo "FAILED: $*"
    if [ -n "$OWNER_LOG" ] && [ -f "$OWNER_LOG" ]; then
        echo "owner log:"
        sed 's/^/  /' "$OWNER_LOG" | tail -n 80
    fi
    if [ -n "$MONITOR_LOG" ] && [ -f "$MONITOR_LOG" ]; then
        echo "integrity monitor log:"
        sed 's/^/  /' "$MONITOR_LOG" | tail -n 80
    fi
    exit 1
}

validate_positive_integer() {
    name="$1"
    value="$2"
    case "$value" in
        ''|*[!0-9]*)
            fail "$name must be a positive integer, got '$value'"
            ;;
    esac
    if [ "$value" -le 0 ]; then
        fail "$name must be a positive integer, got '$value'"
    fi
}

validate_positive_integer CORRUPTION_TORTURE_WORKERS "$WORKERS"
validate_positive_integer CORRUPTION_TORTURE_ITERATIONS "$ITERATIONS"
validate_positive_integer CORRUPTION_TORTURE_TIMEOUT "$TEST_TIMEOUT"
validate_positive_integer CORRUPTION_TORTURE_TEARDOWN_TIMEOUT "$TEARDOWN_TIMEOUT"
validate_positive_integer CORRUPTION_TORTURE_INTEGRITY_TIMEOUT "$INTEGRITY_TIMEOUT"
validate_positive_integer CORRUPTION_TORTURE_START_TIMEOUT "$START_TIMEOUT"

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

if ! python3 - <<'PY' >/dev/null 2>&1
import sqlite3
PY
then
    skip "python3 sqlite3 module is unavailable"
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

cleanup() {
    status=$?
    trap - EXIT INT TERM
    set +e

    [ -n "$WATCHDOG_PID" ] && kill "$WATCHDOG_PID" 2>/dev/null
    [ -n "$WATCHDOG_PID" ] && wait "$WATCHDOG_PID" 2>/dev/null

    if [ -n "$MONITOR_PID" ]; then
        [ -n "$STOP_MONITOR" ] && touch "$STOP_MONITOR" 2>/dev/null
        kill "$MONITOR_PID" 2>/dev/null
        wait "$MONITOR_PID" 2>/dev/null
    fi

    for pid in $WORKER_PIDS; do
        kill "$pid" 2>/dev/null
    done
    for pid in $WORKER_PIDS; do
        wait "$pid" 2>/dev/null
    done

    if [ -n "$OWNER_PID" ]; then
        kill "$OWNER_PID" 2>/dev/null
        waited=0
        while kill -0 "$OWNER_PID" 2>/dev/null && [ "$waited" -lt 20 ]; do
            sleep 0.2
            waited=$((waited + 1))
        done
        if kill -0 "$OWNER_PID" 2>/dev/null; then
            kill -KILL "$OWNER_PID" 2>/dev/null
        fi
        wait "$OWNER_PID" 2>/dev/null
    fi

    unmount_if_needed

    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/agentfs-corruption-torture.*)
                rm -rf "$TEST_ROOT"
                ;;
            *)
                echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT"
                ;;
        esac
    fi

    exit "$status"
}

trap cleanup EXIT
trap 'echo "FAILED: interrupted"; exit 130' INT TERM

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-corruption-torture.XXXXXX")"
TEST_HOME="$TEST_ROOT/home"
WORKDIR="$TEST_ROOT/work"
LOGDIR="$TEST_ROOT/logs"
mkdir -p "$TEST_HOME/.cache" "$TEST_HOME/.config" "$WORKDIR" "$LOGDIR"

# The user's PATH may route `git` through a hook-manager shim that daemonizes
# out of test repos (library/environment.md); pin the distro binary and give
# the temp HOME a hookless git config so nothing survives the suite.
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

SESSION_ID="corruption-torture-$$"
RUN_DIR="$TEST_HOME/.agentfs/run/$SESSION_ID"
DELTA_DB="$RUN_DIR/delta.db"
FUSE_MNT="$RUN_DIR/mnt"
STOP_MONITOR="$TEST_ROOT/stop-monitor"
MONITOR_FAILED="$TEST_ROOT/monitor-failed"
WORKERS_ACTIVE="$TEST_ROOT/workers-active"
MONITOR_OVERLAP_COUNT="$TEST_ROOT/monitor-overlap-count"
OWNER_LOG="$LOGDIR/owner.log"
MONITOR_LOG="$LOGDIR/integrity.log"
TIMEOUT_FLAG="$TEST_ROOT/timed-out"

integrity_check() {
    db_path="$1"
    check_timeout="$2"
    python3 - "$db_path" "$check_timeout" <<'PY'
import os
import shutil
import sqlite3
import sys
import tempfile
import time

db_path = sys.argv[1]
timeout = float(sys.argv[2])
deadline = time.monotonic() + timeout
transient_fragments = (
    "database is locked",
    "database table is locked",
    "database is busy",
    "unable to open database file",
)

def run_integrity(path, *, uri=False):
    conn = sqlite3.connect(path, uri=uri, timeout=0.25)
    try:
        conn.execute("PRAGMA busy_timeout = 250")
        return [row[0] for row in conn.execute("PRAGMA integrity_check")]
    finally:
        conn.close()

def run_snapshot_integrity():
    # The live AgentFS owner keeps SQLite locks for the active FUSE session.
    # If direct read-only access is locked, check a same-basename copy of the
    # delta database and any WAL/SHM sidecars. A copy racing a writer may be
    # transiently inconsistent, so callers retry until their deadline.
    with tempfile.TemporaryDirectory(prefix="agentfs-integrity-") as tmpdir:
        snapshot = os.path.join(tmpdir, os.path.basename(db_path))
        shutil.copy2(db_path, snapshot)
        for suffix in ("-wal", "-shm"):
            sidecar = db_path + suffix
            if os.path.exists(sidecar):
                shutil.copy2(sidecar, snapshot + suffix)
        return run_integrity(snapshot)

last_error = None
last_rows = None

while True:
    try:
        if not os.path.exists(db_path):
            raise sqlite3.OperationalError("unable to open database file")

        rows = run_integrity(f"file:{db_path}?mode=ro", uri=True)

        if rows == ["ok"]:
            sys.exit(0)

        print(f"integrity_check returned {rows!r}", file=sys.stderr)
        sys.exit(2)
    except sqlite3.OperationalError as exc:
        message = str(exc).lower()
        if not any(fragment in message for fragment in transient_fragments):
            print(f"integrity_check operational error: {exc}", file=sys.stderr)
            sys.exit(3)

        last_error = exc
        try:
            rows = run_snapshot_integrity()
            if rows == ["ok"]:
                sys.exit(0)
            last_rows = rows
        except (OSError, sqlite3.DatabaseError) as snapshot_exc:
            last_error = snapshot_exc
    except sqlite3.DatabaseError as exc:
        print(f"integrity_check database error: {exc}", file=sys.stderr)
        sys.exit(4)

    if time.monotonic() >= deadline:
        if last_rows is not None:
            print(f"integrity_check snapshot returned {last_rows!r}", file=sys.stderr)
        else:
            print(f"integrity_check timed out on transient lock/copy race: {last_error}", file=sys.stderr)
        sys.exit(5)
    time.sleep(0.1)
PY
}

owner_failed_for_host_prereq() {
    [ -f "$OWNER_LOG" ] || return 1
    grep -Eiq 'Failed to unshare|user namespace|Operation not permitted|/dev/fuse|fuse:|FUSE|fusermount|permission denied' "$OWNER_LOG"
}

start_owner() {
    (
        cd "$WORKDIR"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        CARGO_HOME="$CARGO_HOME_FOR_TEST" \
        RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
        RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
        cargo run --quiet --manifest-path "$CARGO_MANIFEST" -- run --session "$SESSION_ID" \
            /bin/bash -c 'trap "exit 0" TERM INT; while :; do sleep 1; done'
    ) >"$OWNER_LOG" 2>&1 &
    OWNER_PID=$!
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
            OWNER_PID=""
            if owner_failed_for_host_prereq; then
                echo "SKIP: agentfs run prerequisites unavailable during owner startup"
                sed 's/^/  /' "$OWNER_LOG" | tail -n 40
                exit 0
            fi
            fail "owner session exited before it became ready"
        fi
        sleep 0.2
    done

    if owner_failed_for_host_prereq; then
        echo "SKIP: agentfs run prerequisites unavailable during owner startup"
        sed 's/^/  /' "$OWNER_LOG" | tail -n 40
        exit 0
    fi
    fail "owner session did not become ready within ${START_TIMEOUT}s"
}

start_watchdog() {
    main_pid=$$
    (
        sleep "$TEST_TIMEOUT"
        echo "FAILED: corruption torture timed out after ${TEST_TIMEOUT}s" >&2
        touch "$TIMEOUT_FLAG"
        kill -TERM "$main_pid" 2>/dev/null || true
    ) &
    WATCHDOG_PID=$!
}

monitor_integrity() {
    while [ ! -f "$STOP_MONITOR" ]; do
        if ! integrity_check "$DELTA_DB" "$INTEGRITY_TIMEOUT" >>"$MONITOR_LOG" 2>&1; then
            echo "FAILED: integrity_check failed during concurrent workload" >>"$MONITOR_LOG"
            touch "$MONITOR_FAILED"
            kill -TERM "$$" 2>/dev/null || true
            exit 1
        fi
        if [ -f "$WORKERS_ACTIVE" ]; then
            record_overlap_integrity_check
        fi
        sleep "$INTEGRITY_INTERVAL"
    done
}

start_integrity_monitor() {
    : > "$MONITOR_LOG"
    echo 0 > "$MONITOR_OVERLAP_COUNT"
    monitor_integrity &
    MONITOR_PID=$!
}

record_overlap_integrity_check() {
    count=0
    if [ -f "$MONITOR_OVERLAP_COUNT" ]; then
        count="$(cat "$MONITOR_OVERLAP_COUNT" 2>/dev/null || echo 0)"
    fi
    case "$count" in
        ''|*[!0-9]*) count=0 ;;
    esac
    echo $((count + 1)) > "$MONITOR_OVERLAP_COUNT"
}

start_worker() {
    worker="$1"
    log="$LOGDIR/worker-$worker.log"
    (
        cd "$WORKDIR"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        CARGO_HOME="$CARGO_HOME_FOR_TEST" \
        RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
        RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
        cargo run --quiet --manifest-path "$CARGO_MANIFEST" -- run --session "$SESSION_ID" \
            /bin/bash -c '
set -euo pipefail

worker="$1"
iterations="$2"
base="worker-$worker"

rm -rf "$base"
mkdir -p "$base/appends" "$base/tree" "$base/repo"

append_log="$base/appends/log.txt"
: > "$append_log"

i=1
while [ "$i" -le "$iterations" ]; do
    mkdir -p "$base/tree/iter-$i/deep"
    printf "worker=%s iteration=%s write\n" "$worker" "$i" > "$base/tree/iter-$i/deep/payload.txt"
    printf "worker=%s iteration=%s append-a\n" "$worker" "$i" >> "$append_log"
    cat "$base/tree/iter-$i/deep/payload.txt" >> "$append_log"
    printf "worker=%s iteration=%s append-b\n" "$worker" "$i" >> "$append_log"
    i=$((i + 1))
done

cd "$base/repo"
"${GIT_REAL:-git}" init -q
"${GIT_REAL:-git}" config user.email "worker-$worker@example.invalid"
"${GIT_REAL:-git}" config user.name "Worker $worker"

i=1
while [ "$i" -le "$iterations" ]; do
    mkdir -p "src/iter-$i"
    printf "repo worker=%s iteration=%s\n" "$worker" "$i" > "src/iter-$i/file.txt"
    printf "commit worker=%s iteration=%s\n" "$worker" "$i" >> journal.txt
    "${GIT_REAL:-git}" add .
    "${GIT_REAL:-git}" commit -q -m "worker $worker iteration $i"
    "${GIT_REAL:-git}" fsck --strict --no-progress
    i=$((i + 1))
done
' worker "$worker" "$ITERATIONS"
    ) >"$log" 2>&1 &
    WORKER_PIDS="$WORKER_PIDS $!"
}

run_workers() {
    touch "$WORKERS_ACTIVE"
    worker=1
    while [ "$worker" -le "$WORKERS" ]; do
        start_worker "$worker"
        worker=$((worker + 1))
    done

    if integrity_check "$DELTA_DB" "$INTEGRITY_TIMEOUT" >>"$MONITOR_LOG" 2>&1; then
        record_overlap_integrity_check
    else
        rm -f "$WORKERS_ACTIVE"
        fail "overlap integrity_check failed during concurrent workload"
    fi

    failed=0
    for pid in $WORKER_PIDS; do
        if ! wait "$pid"; then
            failed=1
        fi
    done
    rm -f "$WORKERS_ACTIVE"
    WORKER_PIDS=""

    if [ "$failed" -ne 0 ]; then
        echo "worker logs:"
        for log in "$LOGDIR"/worker-*.log; do
            [ -f "$log" ] || continue
            echo "----- $(basename "$log") -----"
            sed 's/^/  /' "$log" | tail -n 80
        done
        fail "one or more concurrent joiners failed"
    fi
}

stop_integrity_monitor() {
    touch "$STOP_MONITOR"
    if [ -n "$MONITOR_PID" ]; then
        if ! wait "$MONITOR_PID"; then
            MONITOR_PID=""
            fail "integrity monitor failed"
        fi
        MONITOR_PID=""
    fi
    if [ -f "$MONITOR_FAILED" ]; then
        fail "integrity monitor reported a failure"
    fi
    overlap_count="$(cat "$MONITOR_OVERLAP_COUNT" 2>/dev/null || echo 0)"
    case "$overlap_count" in
        ''|*[!0-9]*) overlap_count=0 ;;
    esac
    if [ "$overlap_count" -le 0 ]; then
        fail "integrity monitor did not run while workers were active"
    fi
}

run_final_session_checks() {
    final_log="$LOGDIR/final-session-check.log"
    (
        cd "$WORKDIR"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        CARGO_HOME="$CARGO_HOME_FOR_TEST" \
        RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
        RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
        cargo run --quiet --manifest-path "$CARGO_MANIFEST" -- run --session "$SESSION_ID" \
            /bin/bash -c '
set -euo pipefail

workers="$1"
iterations="$2"
w=1

while [ "$w" -le "$workers" ]; do
    base="worker-$w"
    test -f "$base/appends/log.txt"

    i=1
    while [ "$i" -le "$iterations" ]; do
        grep -q "worker=$w iteration=$i append-a" "$base/appends/log.txt"
        grep -q "worker=$w iteration=$i append-b" "$base/appends/log.txt"
        test -f "$base/tree/iter-$i/deep/payload.txt"
        grep -q "worker=$w iteration=$i write" "$base/tree/iter-$i/deep/payload.txt"
        i=$((i + 1))
    done

    "${GIT_REAL:-git}" -C "$base/repo" fsck --strict --no-progress
    commits="$("${GIT_REAL:-git}" -C "$base/repo" rev-list --count HEAD)"
    test "$commits" -eq "$iterations"

    w=$((w + 1))
done
' check "$WORKERS" "$ITERATIONS"
    ) >"$final_log" 2>&1 || {
        echo "final session check log:"
        sed 's/^/  /' "$final_log" | tail -n 120
        fail "final in-session verification failed"
    }
}

terminate_owner() {
    if [ -n "$OWNER_PID" ]; then
        kill "$OWNER_PID" 2>/dev/null || true
        start_ms="$(date +%s%3N)"
        deadline_ms=$((start_ms + TEARDOWN_TIMEOUT * 1000))
        while kill -0 "$OWNER_PID" 2>/dev/null; do
            now_ms="$(date +%s%3N)"
            if [ "$now_ms" -ge "$deadline_ms" ]; then
                fail "owner did not exit within ${TEARDOWN_TIMEOUT}s after SIGTERM"
            fi
            sleep 0.1
        done
        wait "$OWNER_PID" 2>/dev/null || true
        OWNER_PID=""
        end_ms="$(date +%s%3N)"
        OWNER_TEARDOWN_MS=$((end_ms - start_ms))
    fi
}

HOME="$TEST_HOME" \
XDG_CACHE_HOME="$TEST_HOME/.cache" \
XDG_CONFIG_HOME="$TEST_HOME/.config" \
CARGO_HOME="$CARGO_HOME_FOR_TEST" \
RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
cargo build --quiet --manifest-path "$CARGO_MANIFEST" >/dev/null 2>&1 ||
    fail "failed to build agentfs CLI before torture test"

start_owner
wait_for_owner_ready
integrity_check "$DELTA_DB" "$INTEGRITY_TIMEOUT" >>"$MONITOR_LOG" 2>&1 ||
    fail "initial integrity_check failed"

start_watchdog
start_integrity_monitor
run_workers
stop_integrity_monitor

if [ -f "$TIMEOUT_FLAG" ]; then
    fail "timed out"
fi

if [ -n "$OWNER_PID" ] && ! kill -0 "$OWNER_PID" 2>/dev/null; then
    fail "owner exited before final verification"
fi

integrity_check "$DELTA_DB" "$INTEGRITY_TIMEOUT" >>"$MONITOR_LOG" 2>&1 ||
    fail "final integrity_check failed"

run_final_session_checks
terminate_owner

if [ "${AGENTFS_FUSE_URING:-}" = "1" ] && [ -r /sys/module/fuse/parameters/enable_uring ] &&
    [ "$(cat /sys/module/fuse/parameters/enable_uring)" = "Y" ] &&
    ! grep -q "advertising FUSE_OVER_IO_URING" "$OWNER_LOG"; then
    fail "uring leg did not advertise FUSE_OVER_IO_URING"
fi

if [ "${AGENTFS_FUSE_URING:-}" = "0" ] && grep -q "advertising FUSE_OVER_IO_URING" "$OWNER_LOG"; then
    fail "legacy leg unexpectedly used FUSE_OVER_IO_URING"
fi

echo "OK (workers=$WORKERS iterations=$ITERATIONS owner_teardown_ms=$OWNER_TEARDOWN_MS)"
