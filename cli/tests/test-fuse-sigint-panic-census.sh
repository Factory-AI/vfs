#!/bin/sh
#
# Panic census for repeated SIGINT teardown cycles.
#
# This regression targets a masked FUSE-over-io_uring queue-thread panic where
# the shutdown wake CQE used u64::MAX as user_data and the queue loop treated it
# as a ring slot. Cleanup still completed, so the only visible failure was
# stderr containing "panicked at".
#
set -eu

echo -n "TEST FUSE SIGINT panic census... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
CARGO_MANIFEST="$CLI_DIR/Cargo.toml"
AGENTFS_BIN="${AGENTFS_BIN:-}"
CYCLES="${PANIC_CENSUS_CYCLES:-8}"
TEARDOWN_TIMEOUT="${PANIC_CENSUS_TEARDOWN_TIMEOUT:-10}"
WORKLOAD_SECONDS="${PANIC_CENSUS_WORKLOAD_SECONDS:-1.5}"
HOST_HOME="${HOME:-}"
CARGO_HOME_FOR_TEST="${CARGO_HOME:-$HOST_HOME/.cargo}"
RUSTUP_HOME_FOR_TEST="${RUSTUP_HOME:-$HOST_HOME/.rustup}"
RUSTUP_TOOLCHAIN_FOR_TEST="${RUSTUP_TOOLCHAIN:-nightly}"

TEST_ROOT=""
MOUNT_PID=""
WRITER_PID=""
CURRENT_MNT=""
PANIC_LINES="$(
    mktemp "${TMPDIR:-/tmp}/agentfs-panic-census-lines.XXXXXX"
)"

skip() {
    echo "SKIP: $*"
    exit 0
}

fail() {
    echo
    echo "FAILED: $*"
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

case "$(uname -s)" in
    Linux)
        ;;
    *)
        skip "requires Linux FUSE"
        ;;
esac

command -v cargo >/dev/null 2>&1 || skip "cargo is unavailable"
command -v mountpoint >/dev/null 2>&1 || skip "mountpoint is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"

if [ -r /proc/sys/kernel/unprivileged_userns_clone ] &&
    [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
    skip "unprivileged user namespaces are disabled"
fi

validate_positive_integer PANIC_CENSUS_CYCLES "$CYCLES"
validate_positive_integer PANIC_CENSUS_TEARDOWN_TIMEOUT "$TEARDOWN_TIMEOUT"

if [ -z "$AGENTFS_BIN" ]; then
    cargo build --quiet --manifest-path "$CARGO_MANIFEST" >/dev/null 2>&1 ||
        fail "failed to build agentfs CLI"
    AGENTFS_BIN="$CLI_DIR/target/debug/agentfs"
fi

if [ ! -x "$AGENTFS_BIN" ]; then
    fail "AGENTFS_BIN is not executable: $AGENTFS_BIN"
fi

if [ "$CYCLES" -lt 5 ]; then
    fail "PANIC_CENSUS_CYCLES must be at least 5"
fi

unmount_current() {
    if [ -n "$CURRENT_MNT" ] && mountpoint -q "$CURRENT_MNT" 2>/dev/null; then
        if command -v fusermount3 >/dev/null 2>&1; then
            fusermount3 -u "$CURRENT_MNT" 2>/dev/null ||
                fusermount3 -uz "$CURRENT_MNT" 2>/dev/null || true
        elif command -v fusermount >/dev/null 2>&1; then
            fusermount -u "$CURRENT_MNT" 2>/dev/null || true
        elif command -v umount >/dev/null 2>&1; then
            umount "$CURRENT_MNT" 2>/dev/null || true
        fi
    fi
}

cleanup_current() {
    set +e
    if [ -n "$WRITER_PID" ] && kill -0 "$WRITER_PID" 2>/dev/null; then
        kill "$WRITER_PID" 2>/dev/null || true
        wait "$WRITER_PID" 2>/dev/null || true
    fi
    WRITER_PID=""

    if [ -n "$MOUNT_PID" ] && kill -0 "$MOUNT_PID" 2>/dev/null; then
        kill -INT "$MOUNT_PID" 2>/dev/null || true
        waited=0
        while kill -0 "$MOUNT_PID" 2>/dev/null && [ "$waited" -lt 20 ]; do
            sleep 0.1
            waited=$((waited + 1))
        done
        if kill -0 "$MOUNT_PID" 2>/dev/null; then
            kill -KILL "$MOUNT_PID" 2>/dev/null || true
        fi
        wait "$MOUNT_PID" 2>/dev/null || true
    fi
    MOUNT_PID=""

    unmount_current
    CURRENT_MNT=""
    set -e
}

cleanup_all() {
    set +e
    cleanup_current
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/agentfs-panic-census.*)
                rm -rf "$TEST_ROOT"
                ;;
            *)
                echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT"
                ;;
        esac
    fi
    rm -f "$PANIC_LINES"
    set -e
}

trap cleanup_all EXIT

start_writer() {
    mountpoint_path="$1"
    (
        i=0
        while :; do
            if ! {
                printf 'panic census payload %s\n' "$i" >"$mountpoint_path/file-$((i % 8)).txt"
            } 2>/dev/null; then
                exit 0
            fi
            cat "$mountpoint_path/file-$((i % 8)).txt" >/dev/null 2>&1 || exit 0
            i=$((i + 1))
            sleep 0.02
        done
    ) &
    WRITER_PID=$!
}

wait_for_mount() {
    mountpoint_path="$1"
    deadline=$(( $(date +%s) + 20 ))
    while [ "$(date +%s)" -le "$deadline" ]; do
        if mountpoint -q "$mountpoint_path" 2>/dev/null; then
            return 0
        fi
        if ! kill -0 "$MOUNT_PID" 2>/dev/null; then
            return 1
        fi
        sleep 0.1
    done
    return 1
}

wait_for_mount_exit() {
    deadline=$(( $(date +%s) + TEARDOWN_TIMEOUT ))
    while kill -0 "$MOUNT_PID" 2>/dev/null; do
        if [ "$(date +%s)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 0.05
    done
    wait "$MOUNT_PID" 2>/dev/null || true
    MOUNT_PID=""
    return 0
}

scan_panic_lines() {
    census_label="$1"
    census_log="$2"
    if awk -v label="$census_label" '
        /panicked at/ {
            printf "%s:%d:%s\n", label, NR, $0
            found = 1
        }
        END { exit found ? 0 : 1 }
    ' "$census_log" >>"$PANIC_LINES"; then
        return 0
    fi
    return 1
}

assert_no_session_residue() {
    residue_label="$1"
    residue_session_id="$2"
    residue_mountpoint_path="$3"

    if mountpoint -q "$residue_mountpoint_path" 2>/dev/null; then
        fail "$residue_label left mount residue at $residue_mountpoint_path"
    fi
    if ps -e -o pid= -o args= | awk -v id="$residue_session_id" '
        index($0, id) && index($0, "agentfs") && !index($0, "awk -v id=") {
            print
            found = 1
        }
        END { exit found ? 0 : 1 }
    ' >/tmp/agentfs-panic-census-ps.$$ 2>/dev/null; then
        cat /tmp/agentfs-panic-census-ps.$$
        rm -f /tmp/agentfs-panic-census-ps.$$
        fail "$residue_label left agentfs process residue for $residue_session_id"
    fi
    rm -f /tmp/agentfs-panic-census-ps.$$
}

run_cycle() {
    cycle_leg="$1"
    cycle_label="$2"
    cycle_index="$3"

    home_dir="$TEST_ROOT/home-$cycle_label-$cycle_index"
    work_dir="$TEST_ROOT/work-$cycle_label-$cycle_index"
    mnt="$work_dir/mnt"
    log="$TEST_ROOT/$cycle_label-$cycle_index.log"
    id="panic-census-$cycle_label-$cycle_index-$$"
    mkdir -p "$home_dir/.cache" "$home_dir/.config" "$work_dir" "$mnt"

    (
        cd "$work_dir"
        env HOME="$home_dir" \
            XDG_CACHE_HOME="$home_dir/.cache" \
            XDG_CONFIG_HOME="$home_dir/.config" \
            "$AGENTFS_BIN" init "$id"
    ) >/dev/null 2>&1 || fail "$cycle_label cycle $cycle_index init failed"

    (
        cd "$work_dir"
        env HOME="$home_dir" \
            XDG_CACHE_HOME="$home_dir/.cache" \
            XDG_CONFIG_HOME="$home_dir/.config" \
            CARGO_HOME="$CARGO_HOME_FOR_TEST" \
            RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
            RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
            AGENTFS_FUSE_URING="$cycle_leg" \
            RUST_LOG=agentfs=info \
            "$AGENTFS_BIN" mount "$id" "$mnt" --backend fuse --foreground
    ) >"$log" 2>&1 &
    MOUNT_PID=$!
    CURRENT_MNT="$mnt"

    if ! wait_for_mount "$mnt"; then
        echo
        echo "mount log for $cycle_label cycle $cycle_index:"
        sed 's/^/  /' "$log"
        fail "$cycle_label cycle $cycle_index mount did not become ready"
    fi

    start_writer "$mnt"
    sleep "$WORKLOAD_SECONDS"
    kill -INT "$MOUNT_PID" 2>/dev/null || true
    if ! wait_for_mount_exit; then
        echo
        echo "mount log for $cycle_label cycle $cycle_index:"
        sed 's/^/  /' "$log"
        fail "$cycle_label cycle $cycle_index did not exit within ${TEARDOWN_TIMEOUT}s after SIGINT"
    fi

    if [ -n "$WRITER_PID" ] && kill -0 "$WRITER_PID" 2>/dev/null; then
        kill "$WRITER_PID" 2>/dev/null || true
        wait "$WRITER_PID" 2>/dev/null || true
    fi
    WRITER_PID=""

    unmount_current
    CURRENT_MNT=""

    if [ "$cycle_leg" = "1" ] &&
        [ -r /sys/module/fuse/parameters/enable_uring ] &&
        [ "$(cat /sys/module/fuse/parameters/enable_uring)" = "Y" ] &&
        ! awk '/advertising FUSE_OVER_IO_URING/ { found = 1 } END { exit found ? 0 : 1 }' "$log"; then
        echo
        echo "mount log for $cycle_label cycle $cycle_index:"
        sed 's/^/  /' "$log"
        fail "$cycle_label cycle $cycle_index did not advertise FUSE_OVER_IO_URING"
    fi

    if [ "$cycle_leg" = "0" ] &&
        awk '/advertising FUSE_OVER_IO_URING/ { found = 1 } END { exit found ? 0 : 1 }' "$log"; then
        echo
        echo "mount log for $cycle_label cycle $cycle_index:"
        sed 's/^/  /' "$log"
        fail "$cycle_label cycle $cycle_index unexpectedly advertised FUSE_OVER_IO_URING"
    fi

    scan_panic_lines "$cycle_label-cycle-$cycle_index" "$log" || true
    assert_no_session_residue "$cycle_label cycle $cycle_index" "$id" "$mnt"
}

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-panic-census.XXXXXX")"

if [ -r /sys/module/fuse/parameters/enable_uring ] &&
    [ "$(cat /sys/module/fuse/parameters/enable_uring)" != "Y" ]; then
    skip "FUSE-over-io_uring disabled by kernel parameter"
fi

for run_leg in 1 0; do
    case "$run_leg" in
        1) run_label="uring" ;;
        0) run_label="legacy" ;;
    esac
    run_i=1
    while [ "$run_i" -le "$CYCLES" ]; do
        run_cycle "$run_leg" "$run_label" "$run_i"
        run_i=$((run_i + 1))
    done
done

if [ -s "$PANIC_LINES" ]; then
    echo
    echo "panic census matched stderr lines:"
    cat "$PANIC_LINES"
    fail "panic census found Rust panic output"
fi

printf "OK (cycles_per_leg=%s panic_lines=0)\n" "$CYCLES"
