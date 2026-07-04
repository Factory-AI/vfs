#!/usr/bin/env sh
set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"

echo -n "TEST ephemeral sidecar cleanup... "

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-sidecar-cleanup.XXXXXX")"
PINNED_TMP="$ROOT/tmp"
WORK="$ROOT/work"
MNT="$ROOT/mnt"
mkdir -p "$PINNED_TMP" "$WORK" "$MNT" "$ROOT/home"

cleanup() {
    if mountpoint -q "$MNT" 2>/dev/null; then
        fusermount3 -u "$MNT" >/dev/null 2>&1 || true
    fi
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

run_agentfs() {
    if [ -n "${AGENTFS_BIN:-}" ]; then
        "$AGENTFS_BIN" "$@"
    else
        cargo +nightly run --quiet --manifest-path "$CLI_DIR/Cargo.toml" -- "$@"
    fi
}

sidecar_count() {
    find "$PINNED_TMP" -maxdepth 1 -type f -name 'tursodb-ephemeral-*' -print | wc -l | tr -d ' '
}

assert_no_sidecars() {
    phase="$1"
    count="$(sidecar_count)"
    printf '\n%s sidecar_count=%s\n' "$phase" "$count"
    if [ "$count" -ne 0 ]; then
        echo "FAILED: residual tursodb ephemeral sidecars after $phase"
        find "$PINNED_TMP" -maxdepth 1 -type f -name 'tursodb-ephemeral-*' -printf '%f %s\n' | sort
        exit 1
    fi
}

export TMPDIR="$PINNED_TMP"
export TMP="$PINNED_TMP"
export TEMP="$PINNED_TMP"
export HOME="$ROOT/home"

cd "$WORK"

assert_no_sidecars "before"
run_agentfs init sidecar
assert_no_sidecars "init"
run_agentfs fs sidecar write /hello.txt "hello sidecar"
assert_no_sidecars "fs-write"
run_agentfs fs sidecar cat /hello.txt >/dev/null
assert_no_sidecars "fs-cat"
run_agentfs run --session sidecar-run -- sh -c 'printf run-data > run.txt && cat run.txt' >/dev/null
assert_no_sidecars "run"
GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null run_agentfs run --session sidecar-git -- sh -c '
    set -eu
    git init repo >/dev/null
    cd repo
    git config user.email agentfs@example.invalid
    git config user.name AgentFS
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
run_agentfs mount sidecar "$MNT"

mounted=0
i=0
while [ "$i" -lt 100 ]; do
    if mountpoint -q "$MNT"; then
        mounted=1
        break
    fi
    i=$((i + 1))
    sleep 0.1
done

if [ "$mounted" -ne 1 ]; then
    echo "FAILED: mount did not become live"
    exit 1
fi

printf mounted-data > "$MNT/mounted.txt"
cat "$MNT/mounted.txt" >/dev/null
fusermount3 -u "$MNT"
assert_no_sidecars "mount-unmount"

echo "OK"
