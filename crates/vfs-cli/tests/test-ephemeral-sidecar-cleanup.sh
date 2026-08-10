#!/usr/bin/env sh
set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

echo -n "TEST ephemeral sidecar cleanup... "

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vfs-sidecar-cleanup.XXXXXX")"
PINNED_TMP="$ROOT/tmp"
WORK="$ROOT/work"
MNT="$ROOT/mnt"
mkdir -p "$PINNED_TMP" "$WORK" "$MNT" "$ROOT/home"

MOUNT_PID=""

cleanup() {
    if mountpoint -q "$MNT" 2>/dev/null; then
        fusermount3 -u "$MNT" >/dev/null 2>&1 || true
    fi
    if [ -n "$MOUNT_PID" ] && kill -0 "$MOUNT_PID" 2>/dev/null; then
        kill "$MOUNT_PID" 2>/dev/null || true
        wait "$MOUNT_PID" 2>/dev/null || true
    fi
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

# Resolve the binary once: the mount leg must track the vfs PID itself
# and reap it before the test exits (a lingering FUSE connection draining in
# the kernel wedges the next fuse-over-io_uring mount attempt forever).
if [ -n "${VFS_BIN:-}" ]; then
    BIN="$VFS_BIN"
else
    cargo +nightly build --quiet --manifest-path "$CLI_DIR/Cargo.toml" || {
        echo "FAILED: could not build vfs"
        exit 1
    }
    BIN="$CLI_DIR/../../target/debug/vfs"
fi
if [ ! -x "$BIN" ]; then
    echo "FAILED: vfs binary not found at $BIN"
    exit 1
fi

run_vfs() {
    "$BIN" "$@"
}

# The pinned TMPDIR is dedicated to this test, so it must drain to empty
# whenever no vfs process is running: turso >= 0.6 removes its sort-spill
# temp dirs on drop (tempfile-style `.tmp*` names, not the old
# `tursodb-ephemeral-*` files), and asserting emptiness catches any litter
# pattern the engine or the CLI grows next.
residual_count() {
    find "$PINNED_TMP" -mindepth 1 -maxdepth 1 -print | wc -l | tr -d ' '
}

assert_no_sidecars() {
    phase="$1"
    count="$(residual_count)"
    printf '\n%s residual_count=%s\n' "$phase" "$count"
    if [ "$count" -ne 0 ]; then
        echo "FAILED: residual temp entries after $phase"
        find "$PINNED_TMP" -mindepth 1 -maxdepth 1 -print | sort
        exit 1
    fi
}

export TMPDIR="$PINNED_TMP"
export TMP="$PINNED_TMP"
export TEMP="$PINNED_TMP"
export HOME="$ROOT/home"

# The user's PATH may route `git` through a hook-manager shim that daemonizes
# out of test repos (library/environment.md); pin the distro binary and give
# the temp HOME a hookless git config.
mkdir -p "$HOME/bin" "$HOME/git-hooks-none"
GIT_REAL=""
for candidate in /usr/bin/git /bin/git; do
    if [ -x "$candidate" ]; then
        GIT_REAL="$candidate"
        break
    fi
done
[ -n "$GIT_REAL" ] || GIT_REAL="$(command -v git)"
ln -sf "$GIT_REAL" "$HOME/bin/git"
printf '[core]\n\thooksPath = %s\n' "$HOME/git-hooks-none" >"$HOME/.gitconfig"
PATH="$HOME/bin:$PATH"
export PATH

cd "$WORK"

assert_no_sidecars "before"
run_vfs init sidecar
assert_no_sidecars "init"
run_vfs fs sidecar write /hello.txt "hello sidecar"
assert_no_sidecars "fs-write"
run_vfs fs sidecar cat /hello.txt >/dev/null
assert_no_sidecars "fs-cat"
run_vfs run --session sidecar-run -- sh -c 'printf run-data > run.txt && cat run.txt' >/dev/null
assert_no_sidecars "run"

# The CLI must not touch TMPDIR: children of run/exec see the ambient value.
# stdout carries only the wrapped child's output, so it is compared verbatim:
# a log line appearing here means diagnostics have leaked back onto the data
# channel, which is the bug this asserts against.
CHILD_TMPDIR="$(run_vfs run --session sidecar-tmpdir -- sh -c 'printf %s "${TMPDIR:-}"' 2>/dev/null)"
if [ "$CHILD_TMPDIR" != "$PINNED_TMP" ]; then
    echo "FAILED: run child saw TMPDIR='$CHILD_TMPDIR', expected '$PINNED_TMP'"
    exit 1
fi
assert_no_sidecars "run-child-tmpdir"
CHILD_TMPDIR="$(run_vfs exec sidecar sh -c 'printf %s "${TMPDIR:-}"' 2>/dev/null)"
if [ "$CHILD_TMPDIR" != "$PINNED_TMP" ]; then
    echo "FAILED: exec child saw TMPDIR='$CHILD_TMPDIR', expected '$PINNED_TMP'"
    exit 1
fi
assert_no_sidecars "exec-child-tmpdir"
GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null run_vfs run --session sidecar-git -- sh -c '
    set -eu
    git init repo >/dev/null
    cd repo
    git config user.email vfs@example.invalid
    git config user.name Vfs
    mkdir -p src
    for i in 1 2 3 4; do
        printf "token %s\n" "$i" > "src/file$i.txt"
    done
    git add src
    git commit -m initial >/dev/null
    git status --short >/dev/null
    git grep token >/dev/null
    printf change >> src/file1.txt
    git diff -- src/file1.txt >/dev/null
    git checkout -- src/file1.txt
    git fsck --no-dangling >/dev/null
' >/dev/null
assert_no_sidecars "run-git"
"$BIN" mount sidecar "$MNT" --foreground >"$ROOT/mount.log" 2>&1 &
MOUNT_PID=$!

mounted=0
i=0
while [ "$i" -lt 100 ]; do
    if mountpoint -q "$MNT"; then
        mounted=1
        break
    fi
    if ! kill -0 "$MOUNT_PID" 2>/dev/null; then
        break
    fi
    i=$((i + 1))
    sleep 0.1
done

if [ "$mounted" -ne 1 ]; then
    echo "FAILED: mount did not become live"
    sed 's/^/  /' "$ROOT/mount.log" || true
    exit 1
fi

printf mounted-data > "$MNT/mounted.txt"
cat "$MNT/mounted.txt" >/dev/null
fusermount3 -u "$MNT"

i=0
while kill -0 "$MOUNT_PID" 2>/dev/null && [ "$i" -lt 100 ]; do
    i=$((i + 1))
    sleep 0.1
done
if kill -0 "$MOUNT_PID" 2>/dev/null; then
    echo "FAILED: mount process did not exit after unmount"
    exit 1
fi
wait "$MOUNT_PID" 2>/dev/null || true
MOUNT_PID=""

assert_no_sidecars "mount-unmount"

echo "OK"
