#!/bin/sh
#
# Signal teardown regressions for mount-owning CLI commands.
#
# These cases deliver real SIGINT/SIGTERM to commands with live FUSE mounts and
# assert the command-owned cleanup path leaves no mount table entries,
# command processes, or temp mount directories behind.
#
set -eu

echo -n "TEST signal teardown for clone/init/exec/mount... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
CARGO_MANIFEST="$CLI_DIR/Cargo.toml"
AGENTFS_BIN="${AGENTFS_BIN:-}"
SIGNAL_DELAY="${SIGNAL_TEARDOWN_SIGNAL_DELAY:-3s}"
HOST_HOME="${HOME:-}"
CARGO_HOME_FOR_TEST="${CARGO_HOME:-$HOST_HOME/.cargo}"
RUSTUP_HOME_FOR_TEST="${RUSTUP_HOME:-$HOST_HOME/.rustup}"
RUSTUP_TOOLCHAIN_FOR_TEST="${RUSTUP_TOOLCHAIN:-nightly}"

TEST_ROOT=""
TEST_HOME=""
BEFORE_INIT_DIRS=""
BEFORE_CLONE_DIRS=""
BEFORE_EXEC_DIRS=""
REAL_GIT=""

skip() {
    echo "SKIP: $*"
    exit 0
}

case "$(uname -s)" in
    Linux)
        ;;
    *)
        skip "requires Linux FUSE"
        ;;
esac

command -v cargo >/dev/null 2>&1 || skip "cargo is unavailable"
command -v git >/dev/null 2>&1 || skip "git is unavailable"
command -v timeout >/dev/null 2>&1 || skip "timeout is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"

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

snapshot_tmp_dirs() {
    prefix="$1"
    output="$2"
    python3 - "$prefix" "$output" <<'PY'
import pathlib
import sys

prefix, output = sys.argv[1:]
paths = sorted(str(path) for path in pathlib.Path("/tmp").glob(prefix + "*"))
pathlib.Path(output).write_text("\n".join(paths) + ("\n" if paths else ""))
PY
}

assert_no_new_tmp_dirs() {
    label="$1"
    prefix="$2"
    before="$3"
    python3 - "$label" "$prefix" "$before" <<'PY'
import pathlib
import sys

label, prefix, before_path = sys.argv[1:]
before = set(pathlib.Path(before_path).read_text().splitlines())
after = {str(path) for path in pathlib.Path("/tmp").glob(prefix + "*")}
new = sorted(after - before)
if new:
    print(f"FAILED: {label} left temp mount directories:")
    for path in new:
        print(f"  {path}")
    sys.exit(1)
PY
}

cleanup_mounts_matching() {
    pattern="$1"
    mount | while IFS= read -r line; do
        case "$line" in
            *"$pattern"*)
                mountpoint_path=$(printf '%s\n' "$line" | awk '{print $3}')
                if [ -n "$mountpoint_path" ]; then
                    if command -v fusermount3 >/dev/null 2>&1; then
                        fusermount3 -u "$mountpoint_path" 2>/dev/null || fusermount3 -uz "$mountpoint_path" 2>/dev/null || true
                    elif command -v fusermount >/dev/null 2>&1; then
                        fusermount -u "$mountpoint_path" 2>/dev/null || true
                    else
                        umount "$mountpoint_path" 2>/dev/null || true
                    fi
                fi
                ;;
        esac
    done
}

cleanup_new_tmp_dirs() {
    prefix="$1"
    before="$2"
    if [ -z "$before" ] || [ ! -f "$before" ]; then
        return
    fi
    python3 - "$prefix" "$before" <<'PY' | while IFS= read -r path; do
import pathlib
import sys

prefix, before_path = sys.argv[1:]
before = set(pathlib.Path(before_path).read_text().splitlines())
after = {str(path) for path in pathlib.Path("/tmp").glob(prefix + "*")}
for path in sorted(after - before):
    print(path)
PY
        case "$path" in
            /tmp/agentfs-init-*|/tmp/agentfs-clone-*|/tmp/agentfs-exec-*)
                if command -v fusermount3 >/dev/null 2>&1; then
                    fusermount3 -u "$path" 2>/dev/null || fusermount3 -uz "$path" 2>/dev/null || true
                elif command -v fusermount >/dev/null 2>&1; then
                    fusermount -u "$path" 2>/dev/null || true
                else
                    umount "$path" 2>/dev/null || true
                fi
                rm -rf "$path" 2>/dev/null || true
                ;;
        esac
    done
}

cleanup() {
    set +e
    if [ -n "$TEST_ROOT" ]; then
        cleanup_mounts_matching "$TEST_ROOT"
        cleanup_mounts_matching "signal-init-$$"
        cleanup_mounts_matching "signal-exec-$$"
        cleanup_mounts_matching "signal-mount-$$"
    fi
    if [ -n "$TEST_HOME" ]; then
        cleanup_mounts_matching "$TEST_HOME"
    fi
    cleanup_new_tmp_dirs "agentfs-init-" "$BEFORE_INIT_DIRS"
    cleanup_new_tmp_dirs "agentfs-clone-" "$BEFORE_CLONE_DIRS"
    cleanup_new_tmp_dirs "agentfs-exec-" "$BEFORE_EXEC_DIRS"
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/agentfs-signal-teardown.*)
                rm -rf "$TEST_ROOT"
                ;;
            *)
                echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT"
                ;;
        esac
    fi
    set -e
}

trap cleanup EXIT

fail() {
    echo "FAILED: $*"
    exit 1
}

expected_status_for_signal() {
    case "$1" in
        INT) echo 130 ;;
        TERM) echo 143 ;;
        *) fail "unknown signal $1" ;;
    esac
}

run_timeout_expect() {
    label="$1"
    expected="$2"
    shift 2
    set +e
    "$@"
    status=$?
    set -e
    if [ "$status" -ne "$expected" ]; then
        echo
        echo "FAILED: $label exited $status, expected $expected"
        return 1
    fi
}

assert_no_mount() {
    label="$1"
    pattern="$2"
    if mount | grep "$pattern" >/dev/null 2>&1; then
        echo
        echo "FAILED: $label left a mount table entry for $pattern"
        mount | grep "$pattern" || true
        return 1
    fi
}

assert_no_processes() {
    label="$1"
    pattern="$2"
    if ps -e -o pid= -o args= | grep "$pattern" | grep -v grep >/dev/null 2>&1; then
        echo
        echo "FAILED: $label left process residue for $pattern"
        ps -e -o pid= -o args= | grep "$pattern" | grep -v grep || true
        return 1
    fi
}

new_case_home() {
    name="$1"
    TEST_HOME="$TEST_ROOT/home-$name"
    mkdir -p "$TEST_HOME/.cache" "$TEST_HOME/.config"
}

run_init_signal_case() {
    signal="$1"
    expected="$(expected_status_for_signal "$signal")"
    new_case_home "init-$signal"
    id="signal-init-$$-$signal"

    run_timeout_expect "init -c $signal" "$expected" \
        env HOME="$TEST_HOME" \
            XDG_CACHE_HOME="$TEST_HOME/.cache" \
            XDG_CONFIG_HOME="$TEST_HOME/.config" \
            CARGO_HOME="$CARGO_HOME_FOR_TEST" \
            RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
            RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
            AGENTFS_FUSE_URING=0 \
            timeout --preserve-status --foreground -s "$signal" "$SIGNAL_DELAY" \
            "$AGENTFS_BIN" init "$id" --backend fuse -c 'exec sleep 30'

    assert_no_mount "init -c $signal" "agentfs:$id"
    assert_no_processes "init -c $signal" "$TEST_HOME"
    assert_no_new_tmp_dirs "init -c $signal" "agentfs-init-" "$BEFORE_INIT_DIRS"
}

write_git_wrapper() {
    wrapper_dir="$TEST_ROOT/git-wrapper"
    mkdir -p "$wrapper_dir"
    cat >"$wrapper_dir/git" <<'EOF'
#!/bin/sh
if [ "$1" = "clone" ]; then
    for last do :; done
    mkdir -p "$last/.git/objects"
    printf 'clone wrapper touched the mounted filesystem\n' > "$last/probe.txt"
    exec sleep 30
fi
exec "$REAL_GIT" "$@"
EOF
    chmod +x "$wrapper_dir/git"
    echo "$wrapper_dir"
}

run_clone_signal_case() {
    signal="$1"
    expected="$(expected_status_for_signal "$signal")"
    new_case_home "clone-$signal"
    db="$TEST_ROOT/clone-$signal.db"
    source="$TEST_ROOT/source-$signal"
    mkdir -p "$source"
    wrapper_dir="$(write_git_wrapper)"

    run_timeout_expect "clone $signal" "$expected" \
        env HOME="$TEST_HOME" \
            XDG_CACHE_HOME="$TEST_HOME/.cache" \
            XDG_CONFIG_HOME="$TEST_HOME/.config" \
            CARGO_HOME="$CARGO_HOME_FOR_TEST" \
            RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
            RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
            AGENTFS_FUSE_URING=0 \
            REAL_GIT="$REAL_GIT" \
            PATH="$wrapper_dir:$PATH" \
            timeout --preserve-status --foreground -s "$signal" "$SIGNAL_DELAY" \
            "$AGENTFS_BIN" clone "$db" "$source" repo --backend fuse

    assert_no_mount "clone $signal" "$db"
    assert_no_processes "clone $signal" "$TEST_ROOT"
    assert_no_new_tmp_dirs "clone $signal" "agentfs-clone-" "$BEFORE_CLONE_DIRS"
}

run_exec_signal_case() {
    signal="$1"
    expected="$(expected_status_for_signal "$signal")"
    new_case_home "exec-$signal"
    id="signal-exec-$$-$signal"

    env HOME="$TEST_HOME" "$AGENTFS_BIN" init "$id" >/dev/null 2>&1
    run_timeout_expect "exec $signal" "$expected" \
        env HOME="$TEST_HOME" \
            XDG_CACHE_HOME="$TEST_HOME/.cache" \
            XDG_CONFIG_HOME="$TEST_HOME/.config" \
            CARGO_HOME="$CARGO_HOME_FOR_TEST" \
            RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
            RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
            AGENTFS_FUSE_URING=0 \
            timeout --preserve-status --foreground -s "$signal" "$SIGNAL_DELAY" \
            "$AGENTFS_BIN" exec "$id" --backend fuse sh -c 'exec sleep 30'

    assert_no_mount "exec $signal" "agentfs:.*$id"
    assert_no_processes "exec $signal" "$TEST_HOME"
    assert_no_new_tmp_dirs "exec $signal" "agentfs-exec-" "$BEFORE_EXEC_DIRS"
}

run_mount_foreground_signal_case() {
    signal="$1"
    new_case_home "mount-$signal"
    id="signal-mount-$$-$signal"
    mountpoint="$TEST_ROOT/mount-$signal"
    mkdir -p "$mountpoint"

    env HOME="$TEST_HOME" "$AGENTFS_BIN" init "$id" >/dev/null 2>&1
    run_timeout_expect "mount --foreground $signal" 0 \
        env HOME="$TEST_HOME" \
            XDG_CACHE_HOME="$TEST_HOME/.cache" \
            XDG_CONFIG_HOME="$TEST_HOME/.config" \
            CARGO_HOME="$CARGO_HOME_FOR_TEST" \
            RUSTUP_HOME="$RUSTUP_HOME_FOR_TEST" \
            RUSTUP_TOOLCHAIN="$RUSTUP_TOOLCHAIN_FOR_TEST" \
            AGENTFS_FUSE_URING=0 \
            timeout --preserve-status --foreground -s "$signal" "$SIGNAL_DELAY" \
            "$AGENTFS_BIN" mount "$id" "$mountpoint" --backend fuse --foreground

    assert_no_mount "mount --foreground $signal" "$mountpoint"
    assert_no_processes "mount --foreground $signal" "$TEST_HOME"
}

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-signal-teardown.XXXXXX")"
BEFORE_INIT_DIRS="$TEST_ROOT/before-init-dirs.txt"
BEFORE_CLONE_DIRS="$TEST_ROOT/before-clone-dirs.txt"
BEFORE_EXEC_DIRS="$TEST_ROOT/before-exec-dirs.txt"
REAL_GIT="$(command -v git)"
snapshot_tmp_dirs "agentfs-init-" "$BEFORE_INIT_DIRS"
snapshot_tmp_dirs "agentfs-clone-" "$BEFORE_CLONE_DIRS"
snapshot_tmp_dirs "agentfs-exec-" "$BEFORE_EXEC_DIRS"

for signal in INT TERM; do
    run_init_signal_case "$signal"
    run_clone_signal_case "$signal"
    run_exec_signal_case "$signal"
    run_mount_foreground_signal_case "$signal"
done

echo "OK"
