#!/bin/bash
#
# Lazy remote adopt end to end: hollow installation, lazy reads and writes,
# containment, hydration, portable copies, transfer closure, refusal paths,
# and the hollow-session streamer guard.
#
set -euo pipefail

echo -n "TEST remote adopt e2e... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
VFS_BIN="${VFS_BIN:-}"
TEST_ROOT=""
LIVE_PID=""
LIVE_LOG=""

ORIGIN_ID="remote-adopt-origin-$$"
MAIN_ID="remote-adopt-main-$$"
RMW_ID="remote-adopt-rmw-$$"
TRUNCATE_ID="remote-adopt-truncate-$$"
RETRY_ID="remote-adopt-retry-$$"
BACKUP_ID="remote-adopt-backup-$$"
OUTPUT_ID="remote-adopt-output-$$"
STREAM_ID="remote-adopt-stream-$$"
PACKED_ID="remote-adopt-packed-$$"
BRANCH_ID="remote-adopt-branch-$$"
MISSING_ID="remote-adopt-missing-$$"
ABSENT_ID="remote-adopt-absent-$$"
WRONG_ID="remote-adopt-wrong-$$"
CORRUPT_ID="remote-adopt-corrupt-$$"
FUTURE_ID="remote-adopt-future-$$"

skip() {
    echo "SKIP: $*"
    exit 0
}

fail() {
    echo "FAILED: $*"
    exit 1
}

wait_pid_exit() {
    local pid="$1"
    local deadline=$(( $(date +%s) + 10 ))
    while kill -0 "$pid" 2>/dev/null; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.1
    done
}

unmount_path() {
    local path="$1"
    [ -n "$path" ] || return
    if command -v fusermount3 >/dev/null 2>&1; then
        fusermount3 -uz "$path" 2>/dev/null || true
    elif command -v fusermount >/dev/null 2>&1; then
        fusermount -u "$path" 2>/dev/null || true
    else
        umount -l "$path" 2>/dev/null || true
    fi
}

stop_live() {
    if [ -n "$LIVE_PID" ] && kill -0 "$LIVE_PID" 2>/dev/null; then
        kill -TERM "$LIVE_PID" 2>/dev/null || true
        wait_pid_exit "$LIVE_PID" || kill -KILL "$LIVE_PID" 2>/dev/null || true
        wait "$LIVE_PID" 2>/dev/null || true
    fi
    LIVE_PID=""
    LIVE_LOG=""
}

case "$(uname -s)" in
    Linux) ;;
    *) skip "requires Linux namespaces and FUSE" ;;
esac

[ -n "$VFS_BIN" ] || command -v cargo >/dev/null 2>&1 ||
    skip "cargo is unavailable and VFS_BIN is unset"
command -v python3 >/dev/null 2>&1 || skip "python3 is unavailable"
command -v git >/dev/null 2>&1 || skip "git is unavailable"
command -v sha256sum >/dev/null 2>&1 || skip "sha256sum is unavailable"
command -v truncate >/dev/null 2>&1 || skip "truncate is unavailable"
[ -x /bin/bash ] || skip "/bin/bash is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"

if [ -r /proc/sys/kernel/unprivileged_userns_clone ] &&
    [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
    skip "unprivileged user namespaces are disabled"
fi

if [ -z "$VFS_BIN" ]; then
    cargo +nightly build --quiet --manifest-path "$CLI_DIR/Cargo.toml" >/dev/null 2>&1 ||
        fail "failed to build vfs CLI"
    VFS_BIN="$REPO_ROOT/target/debug/vfs"
fi
case "$VFS_BIN" in
    /*) ;;
    *) VFS_BIN="$(cd "$(dirname "$VFS_BIN")" && pwd)/$(basename "$VFS_BIN")" ;;
esac
[ -x "$VFS_BIN" ] || fail "VFS_BIN is not executable: $VFS_BIN"

cleanup() {
    set +e
    stop_live
    if [ -n "$TEST_ROOT" ]; then
        for home_name in home-origin home-receiver home-third; do
            for session_id in \
                "$ORIGIN_ID" "$MAIN_ID" "$RMW_ID" "$TRUNCATE_ID" "$RETRY_ID" \
                "$BACKUP_ID" "$OUTPUT_ID" "$STREAM_ID" "$PACKED_ID" "$BRANCH_ID" \
                "$MISSING_ID" "$ABSENT_ID" "$WRONG_ID" "$CORRUPT_ID" "$FUTURE_ID"; do
                unmount_path "$TEST_ROOT/$home_name/.vfs/run/$session_id/mnt"
            done
        done
    fi
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/vfs-remote-adopt.*)
                chmod -R u+w "$TEST_ROOT" 2>/dev/null || true
                rm -rf "$TEST_ROOT"
                ;;
            *) echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT" ;;
        esac
    fi
}
trap cleanup EXIT INT TERM

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vfs-remote-adopt.XXXXXX")"
HOME_ORIGIN="$TEST_ROOT/home-origin"
HOME_RECEIVER="$TEST_ROOT/home-receiver"
HOME_THIRD="$TEST_ROOT/home-third"
SENDER="$TEST_ROOT/sender"
RECEIVER="$TEST_ROOT/receiver"
THIRD="$TEST_ROOT/third"
REMOTE="$TEST_ROOT/remote"
mkdir -p \
    "$HOME_ORIGIN/.cache" "$HOME_ORIGIN/.config" \
    "$HOME_RECEIVER/.cache" "$HOME_RECEIVER/.config" \
    "$HOME_THIRD/.cache" "$HOME_THIRD/.config" \
    "$SENDER" "$REMOTE"

run_vfs() {
    local home="$1"
    local base="$2"
    shift 2
    (
        cd "$base"
        env -u VFS_REMOTE_URL -u VFS_REMOTE_STREAM_INTERVAL_MS \
            HOME="$home" \
            XDG_CACHE_HOME="$home/.cache" \
            XDG_CONFIG_HOME="$home/.config" \
            GIT_CONFIG_GLOBAL=/dev/null \
            GIT_CONFIG_SYSTEM=/dev/null \
            VFS_FUSE_URING=0 \
            "$VFS_BIN" "$@"
    )
}

run_vfs_remote() {
    local home="$1"
    local base="$2"
    local remote="$3"
    shift 3
    (
        cd "$base"
        env -u VFS_REMOTE_STREAM_INTERVAL_MS \
            HOME="$home" \
            XDG_CACHE_HOME="$home/.cache" \
            XDG_CONFIG_HOME="$home/.config" \
            GIT_CONFIG_GLOBAL=/dev/null \
            GIT_CONFIG_SYSTEM=/dev/null \
            VFS_FUSE_URING=0 \
            VFS_REMOTE_URL="file://$remote" \
            "$VFS_BIN" "$@"
    )
}

adopt_receiver() {
    local session_id="$1"
    local output="$2"
    mkdir -p "$REMOTE/sessions/$session_id"
    python3 - \
        "$REMOTE/sessions/$ORIGIN_ID/manifest.json" \
        "$REMOTE/sessions/$session_id/manifest.json" \
        "$session_id" <<'PY'
import json
import sys

source, target, session_id = sys.argv[1:]
manifest = json.load(open(source))
manifest["sessionId"] = session_id
with open(target, "w") as output:
    json.dump(manifest, output, separators=(",", ":"))
PY
    run_vfs_remote "$HOME_RECEIVER" "$RECEIVER" "$REMOTE" \
        adopt "$session_id" --remote --base "$RECEIVER" >"$output"
}

extract_hash() {
    python3 - "$1" <<'PY'
import re
import sys

matches = re.findall(r"(?m)^([0-9a-f]{64})(?:\s|$)", open(sys.argv[1]).read())
assert matches, f"no sha256 found in {sys.argv[1]}"
print(matches[-1])
PY
}

assert_hollow_database() {
    python3 - "$1" <<'PY'
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
assert conn.execute(
    "SELECT value FROM fs_config WHERE key = 'chunks_hollow'"
).fetchone() == ("1",)
total, present = conn.execute(
    "SELECT COUNT(*), SUM(CASE WHEN length(data) != 0 THEN 1 ELSE 0 END) FROM fs_chunk"
).fetchone()
assert total > 0, total
assert present == 0, (present, total)
conn.close()
PY
}

wait_live() {
    local session_id="$1"
    local deadline=$(( $(date +%s) + 15 ))
    until run_vfs "$HOME_RECEIVER" "$RECEIVER" status "$session_id" --json \
        2>/dev/null | grep -q '"state":"live"'; do
        [ "$(date +%s)" -lt "$deadline" ] ||
            fail "$session_id never reported live"
        kill -0 "$LIVE_PID" 2>/dev/null || {
            [ -z "$LIVE_LOG" ] || cat "$LIVE_LOG" >&2
            fail "$session_id live owner exited prematurely"
        }
        sleep 0.2
    done
}

start_hollow_streamer() {
    local session_id="$1"
    local stream_remote="$2"
    local log="$3"
    (
        cd "$RECEIVER" || exit 1
        exec env \
            HOME="$HOME_RECEIVER" \
            XDG_CACHE_HOME="$HOME_RECEIVER/.cache" \
            XDG_CONFIG_HOME="$HOME_RECEIVER/.config" \
            GIT_CONFIG_GLOBAL=/dev/null \
            GIT_CONFIG_SYSTEM=/dev/null \
            VFS_FUSE_URING=0 \
            VFS_REMOTE_URL="file://$stream_remote" \
            VFS_REMOTE_STREAM_INTERVAL_MS=200 \
            "$VFS_BIN" run --session "$session_id" -- \
            /bin/bash -c 'while :; do sleep 0.1; done'
    ) >"$log" 2>&1 &
    LIVE_PID=$!
    LIVE_LOG="$log"
    wait_live "$session_id"
}

# --- Origin checkpoint -----------------------------------------------------
head -c 200000 /dev/urandom >"$SENDER/payload-source.bin"
head -c 150000 /dev/urandom >"$SENDER/tracked-base.bin"
printf "tracked checkout content\n" >"$SENDER/tracked.txt"
(
    cd "$SENDER"
    git init -q -b main
    git -c user.email=remote-adopt@example.invalid \
        -c user.name="Vfs Remote Adopt" add .
    git -c user.email=remote-adopt@example.invalid \
        -c user.name="Vfs Remote Adopt" commit -qm base
) || fail "failed to initialize sender git checkout"
BASE_PIN="$(git -C "$SENDER" rev-parse HEAD)" ||
    fail "failed to resolve sender pin"
ORIGIN_SHA="$(sha256sum "$SENDER/payload-source.bin")"
ORIGIN_SHA="${ORIGIN_SHA%% *}"

run_vfs "$HOME_ORIGIN" "$SENDER" run \
    --session "$ORIGIN_ID" --seed-pin "$BASE_PIN" -- /bin/bash -c '
set -e
/bin/cp payload-source.bin big.bin
printf "small remote payload\n" > small.txt
/bin/mkdir -p nested/dir
printf "nested remote payload\n" > nested/dir/file.txt
printf "partial-origin-edit" |
    /bin/dd of=tracked-base.bin bs=1 seek=70000 conv=notrunc status=none
' >"$TEST_ROOT/origin-run.log" 2>&1 ||
    fail "origin seeded workload failed"

ORIGIN_DB="$HOME_ORIGIN/.vfs/run/$ORIGIN_ID/delta.db"
[ -f "$ORIGIN_DB" ] || fail "origin workload did not install its session database"

CHECKPOINT_JSON="$TEST_ROOT/checkpoint.json"
run_vfs_remote "$HOME_ORIGIN" "$SENDER" "$REMOTE" \
    checkpoint "$ORIGIN_ID" --json >"$CHECKPOINT_JSON" ||
    fail "origin checkpoint failed"
MANIFEST="$REMOTE/sessions/$ORIGIN_ID/manifest.json"
[ -f "$MANIFEST" ] || fail "checkpoint manifest is missing"
python3 - "$CHECKPOINT_JSON" "$MANIFEST" <<'PY' || fail "checkpoint token or manifest was malformed"
import json
import sys

token, manifest = (json.load(open(path)) for path in sys.argv[1:])
assert token["sessionId"] == manifest["sessionId"], (token, manifest)
assert token["chunkCount"] == manifest["chunkCount"] >= 3, (token, manifest)
assert token["chunkBytes"] == manifest["chunkBytes"] >= 200000, (token, manifest)
assert token["metadataSha256"] == manifest["metadata"]["sha256"], (token, manifest)
assert manifest["seedPin"], manifest
PY

git clone -q "$SENDER" "$RECEIVER" ||
    fail "failed to clone receiver checkout"
git clone -q "$SENDER" "$THIRD" ||
    fail "failed to clone third checkout"
[ "$(git -C "$RECEIVER" rev-parse HEAD)" = "$BASE_PIN" ] ||
    fail "receiver checkout is not at the sender pin"
[ "$(git -C "$THIRD" rev-parse HEAD)" = "$BASE_PIN" ] ||
    fail "third checkout is not at the sender pin"

# --- Hollow adopt and integrity distinction -------------------------------
MAIN_ADOPT_JSON="$TEST_ROOT/main-adopt.json"
adopt_receiver "$MAIN_ID" "$MAIN_ADOPT_JSON" ||
    fail "remote adopt failed"
MAIN_DIR="$HOME_RECEIVER/.vfs/run/$MAIN_ID"
MAIN_DB="$MAIN_DIR/delta.db"
[ -f "$MAIN_DB" ] || fail "remote adopt did not install delta.db"
[ -f "$MAIN_DIR/remote" ] || fail "remote adopt did not persist its sidecar"
[ "$(cat "$MAIN_DIR/remote")" = "file://$REMOTE" ] ||
    fail "remote sidecar does not contain the adopted URL"
python3 - "$MAIN_ADOPT_JSON" "$MAIN_DIR/remote" "$MAIN_DB" <<'PY' || fail "remote adopt manifest or installed layout was malformed"
import json
import os
import sys

manifest = json.load(open(sys.argv[1]))
assert manifest["manifestVersion"] == 1, manifest
assert manifest["remote"] is True, manifest
assert manifest["sessionId"], manifest
assert manifest["basePin"], manifest
assert os.path.isfile(sys.argv[2]), sys.argv[2]
assert os.path.isfile(sys.argv[3]), sys.argv[3]
PY
assert_hollow_database "$MAIN_DB" ||
    fail "adopted database was not fully hollow"

run_vfs "$HOME_RECEIVER" "$RECEIVER" integrity "$MAIN_DB" --json \
    >"$TEST_ROOT/main-integrity.json" ||
    fail "plain integrity rejected the hollow session"
python3 - "$TEST_ROOT/main-integrity.json" <<'PY' || fail "plain hollow integrity did not report the expected distinction"
import json
import sys

report = json.load(open(sys.argv[1]))
assert report["ok"] is True, report
assert report["portable"] is False, report
checks = {check["name"]: check for check in report["checks"]}
hollow = checks["storage.chunks_hollow"]
assert hollow["ok"] is True, hollow
assert "remote metadata artifact" in hollow["detail"], hollow
digest = checks["storage.chunk_bytes_match_digest"]
assert digest["ok"] is True and "intentionally absent" in digest["detail"], digest
PY
if run_vfs "$HOME_RECEIVER" "$RECEIVER" \
    integrity "$MAIN_DB" --json --require-portable \
    >"$TEST_ROOT/main-portable.json" 2>"$TEST_ROOT/main-portable.err"; then
    fail "portable integrity unexpectedly accepted the hollow session"
fi
python3 - "$TEST_ROOT/main-portable.json" <<'PY' || fail "portable hollow integrity did not identify both dependencies"
import json
import sys

report = json.load(open(sys.argv[1]))
assert report["ok"] is False, report
assert report["portable"] is False, report
checks = {check["name"]: check for check in report["checks"]}
assert checks["storage.chunks_hollow"]["ok"] is False, checks
assert checks["overlay.require_portable"]["ok"] is False, checks
detail = checks["overlay.require_portable"]["detail"]
assert "chunk bytes are not present" in detail, detail
if report["origin_backed"]:
    assert report["partial_origin_rows"] > 0 and "partial-origin" in detail, report
PY

# --- Lazy read without ambient remote config -------------------------------
run_vfs "$HOME_RECEIVER" "$RECEIVER" run --session "$MAIN_ID" -- \
    /bin/bash -c '
set -e
test "$(/bin/cat small.txt)" = "small remote payload"
test "$(/bin/cat nested/dir/file.txt)" = "nested remote payload"
sha256sum big.bin
' >"$TEST_ROOT/main-lazy-read.log" 2>&1 ||
    fail "lazy read through the persisted sidecar failed"
MAIN_LAZY_SHA="$(extract_hash "$TEST_ROOT/main-lazy-read.log")"
[ "$MAIN_LAZY_SHA" = "$ORIGIN_SHA" ] ||
    fail "lazy read returned bytes different from the origin"
python3 - "$MAIN_DB" <<'PY' || fail "lazy read did not backfill chunks while preserving the hollow marker"
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
assert conn.execute(
    "SELECT value FROM fs_config WHERE key = 'chunks_hollow'"
).fetchone() == ("1",)
total, present = conn.execute(
    "SELECT COUNT(*), SUM(CASE WHEN length(data) != 0 THEN 1 ELSE 0 END) FROM fs_chunk"
).fetchone()
assert total > 0 and 0 < present <= total, (present, total)
conn.close()
PY

# --- Lazy read-modify-write on a fresh hollow adopt ------------------------
adopt_receiver "$RMW_ID" "$TEST_ROOT/rmw-adopt.json" ||
    fail "fresh adopt for lazy RMW failed"
assert_hollow_database "$HOME_RECEIVER/.vfs/run/$RMW_ID/delta.db" ||
    fail "lazy RMW fixture was not fresh and hollow"
cp "$SENDER/payload-source.bin" "$TEST_ROOT/rmw-expected.bin"
printf "lazy-rmw-edit" |
    dd of="$TEST_ROOT/rmw-expected.bin" bs=1 seek=90000 conv=notrunc status=none
RMW_EXPECTED_SHA="$(sha256sum "$TEST_ROOT/rmw-expected.bin")"
RMW_EXPECTED_SHA="${RMW_EXPECTED_SHA%% *}"
run_vfs "$HOME_RECEIVER" "$RECEIVER" run --session "$RMW_ID" -- \
    /bin/bash -c '
set -e
printf "lazy-rmw-edit" |
    /bin/dd of=big.bin bs=1 seek=90000 conv=notrunc status=none
sha256sum big.bin
' >"$TEST_ROOT/rmw-run.log" 2>&1 ||
    fail "lazy RMW workload failed"
RMW_ACTUAL_SHA="$(extract_hash "$TEST_ROOT/rmw-run.log")"
[ "$RMW_ACTUAL_SHA" = "$RMW_EXPECTED_SHA" ] ||
    fail "lazy RMW did not preserve untouched remote bytes"

# --- Lazy truncate boundary on a fresh hollow adopt ------------------------
adopt_receiver "$TRUNCATE_ID" "$TEST_ROOT/truncate-adopt.json" ||
    fail "fresh adopt for lazy truncate failed"
head -c 150000 "$SENDER/payload-source.bin" >"$TEST_ROOT/truncate-expected.bin"
TRUNCATE_EXPECTED_SHA="$(sha256sum "$TEST_ROOT/truncate-expected.bin")"
TRUNCATE_EXPECTED_SHA="${TRUNCATE_EXPECTED_SHA%% *}"
TRUNCATE_BIN="$(command -v truncate)"
run_vfs "$HOME_RECEIVER" "$RECEIVER" run --session "$TRUNCATE_ID" -- \
    /bin/bash -c '
set -e
"$1" -s 150000 big.bin
sha256sum big.bin
' _ "$TRUNCATE_BIN" >"$TEST_ROOT/truncate-run.log" 2>&1 ||
    fail "lazy truncate workload failed"
TRUNCATE_ACTUAL_SHA="$(extract_hash "$TEST_ROOT/truncate-run.log")"
[ "$TRUNCATE_ACTUAL_SHA" = "$TRUNCATE_EXPECTED_SHA" ] ||
    fail "lazy truncate did not preserve the retained remote prefix"

# --- Remote failure is explicit and retryable ------------------------------
adopt_receiver "$RETRY_ID" "$TEST_ROOT/retry-adopt.json" ||
    fail "fresh adopt for remote retry failed"
mv "$REMOTE/chunks" "$REMOTE/chunks.offline"
mkdir "$REMOTE/chunks"
if run_vfs "$HOME_RECEIVER" "$RECEIVER" run --session "$RETRY_ID" -- \
    /bin/bash -c '/bin/cat big.bin >/dev/null' \
    >"$TEST_ROOT/retry-failure.out" 2>"$TEST_ROOT/retry-failure.err"; then
    rmdir "$REMOTE/chunks"
    mv "$REMOTE/chunks.offline" "$REMOTE/chunks"
    fail "remote-unreachable read silently succeeded"
fi
rmdir "$REMOTE/chunks"
mv "$REMOTE/chunks.offline" "$REMOTE/chunks"
grep -Eqi 'input/output error|I/O error|remote chunk|chunk source' \
    "$TEST_ROOT/retry-failure.out" "$TEST_ROOT/retry-failure.err" ||
    fail "remote-unreachable read did not surface an explicit read error"
run_vfs "$HOME_RECEIVER" "$RECEIVER" run --session "$RETRY_ID" -- \
    sha256sum big.bin >"$TEST_ROOT/retry-success.log" 2>&1 ||
    fail "same-session lazy read did not recover after the remote returned"
RETRY_SHA="$(extract_hash "$TEST_ROOT/retry-success.log")"
[ "$RETRY_SHA" = "$ORIGIN_SHA" ] ||
    fail "retry after remote recovery returned incorrect bytes"

# --- Default-deny containment while hollow --------------------------------
if run_vfs "$HOME_RECEIVER" "$RECEIVER" pack "$MAIN_ID" \
    --output "$TEST_ROOT/hollow-pack.db" \
    >"$TEST_ROOT/hollow-pack.out" 2>"$TEST_ROOT/hollow-pack.err"; then
    fail "pack unexpectedly accepted a hollow session"
fi
grep -q "hydrate it before opening writable" "$TEST_ROOT/hollow-pack.err" ||
    fail "pack refusal omitted hydration guidance"

if run_vfs "$HOME_RECEIVER" "$RECEIVER" branch "$MAIN_ID" \
    --session "$BRANCH_ID" \
    >"$TEST_ROOT/hollow-branch.out" 2>"$TEST_ROOT/hollow-branch.err"; then
    fail "branch unexpectedly accepted a hollow session"
fi
grep -q "hydrate it before opening writable" "$TEST_ROOT/hollow-branch.err" ||
    fail "branch refusal omitted hydration guidance"

if run_vfs "$HOME_RECEIVER" "$RECEIVER" revert "$MAIN_ID" --to 0 \
    >"$TEST_ROOT/hollow-revert.out" 2>"$TEST_ROOT/hollow-revert.err"; then
    fail "revert unexpectedly accepted a hollow session"
fi
grep -q "hydrate it before opening writable" "$TEST_ROOT/hollow-revert.err" ||
    fail "revert refusal omitted hydration guidance"

if run_vfs_remote "$HOME_RECEIVER" "$RECEIVER" "$REMOTE" \
    checkpoint "$MAIN_ID" --json \
    >"$TEST_ROOT/hollow-checkpoint.out" 2>"$TEST_ROOT/hollow-checkpoint.err"; then
    fail "checkpoint unexpectedly accepted a hollow session"
fi
grep -q "vfs checkpoint refuses hollow sessions; materialize the session first" \
    "$TEST_ROOT/hollow-checkpoint.err" ||
    fail "checkpoint refusal omitted materialization guidance"

if run_vfs "$HOME_RECEIVER" "$RECEIVER" backup "$MAIN_ID" \
    "$TEST_ROOT/hollow-backup.db" \
    >"$TEST_ROOT/hollow-backup.out" 2>"$TEST_ROOT/hollow-backup.err"; then
    fail "plain backup unexpectedly accepted a hollow session"
fi
grep -q "remote metadata artifact whose chunk bytes are not present" \
    "$TEST_ROOT/hollow-backup.err" ||
    fail "plain backup refusal did not identify hollow storage"
grep -q "backup --materialize" "$TEST_ROOT/hollow-backup.err" ||
    fail "plain backup refusal omitted the materializing alternative"

# --- Portable materializing copies ----------------------------------------
adopt_receiver "$BACKUP_ID" "$TEST_ROOT/backup-adopt.json" ||
    fail "fresh adopt for backup materialization failed"
PORTABLE_BACKUP="$TEST_ROOT/portable-backup.db"
run_vfs "$HOME_RECEIVER" "$RECEIVER" backup "$BACKUP_ID" \
    "$PORTABLE_BACKUP" --materialize --verify \
    >"$TEST_ROOT/portable-backup.log" ||
    fail "backup --materialize failed on a hollow session"
grep -Eq '^Hydrated chunks: [1-9][0-9]*$' "$TEST_ROOT/portable-backup.log" ||
    fail "backup --materialize did not report hydrated chunks"
run_vfs "$HOME_RECEIVER" "$RECEIVER" \
    integrity "$PORTABLE_BACKUP" --json --require-portable \
    >"$TEST_ROOT/portable-backup-integrity.json" ||
    fail "materializing backup was not portable"
python3 - "$TEST_ROOT/portable-backup-integrity.json" <<'PY' || fail "materializing backup retained a remote or origin dependency"
import json
import sys

report = json.load(open(sys.argv[1]))
assert report["ok"] is True and report["portable"] is True, report
assert report["origin_backed"] is False and report["partial_origin_rows"] == 0, report
checks = {check["name"]: check for check in report["checks"]}
assert checks["storage.chunks_hollow"]["ok"] is True, checks
assert checks["storage.chunks_hollow"]["detail"] == "chunk bytes are present", checks
PY

adopt_receiver "$OUTPUT_ID" "$TEST_ROOT/output-adopt.json" ||
    fail "fresh adopt for materialize --output failed"
PORTABLE_OUTPUT="$TEST_ROOT/portable-output.db"
run_vfs "$HOME_RECEIVER" "$RECEIVER" materialize "$OUTPUT_ID" \
    --output "$PORTABLE_OUTPUT" --verify >"$TEST_ROOT/portable-output.log" ||
    fail "materialize --output failed on a hollow session"
grep -Eq '^Hydrated chunks: [1-9][0-9]*$' "$TEST_ROOT/portable-output.log" ||
    fail "materialize --output did not report hydrated chunks"
run_vfs "$HOME_RECEIVER" "$RECEIVER" \
    integrity "$PORTABLE_OUTPUT" --json --require-portable \
    >"$TEST_ROOT/portable-output-integrity.json" ||
    fail "materialize --output did not create a portable database"

# --- Hollow streamer never publishes empty chunk objects ------------------
adopt_receiver "$STREAM_ID" "$TEST_ROOT/stream-adopt.json" ||
    fail "fresh adopt for hollow streamer failed"
assert_hollow_database "$HOME_RECEIVER/.vfs/run/$STREAM_ID/delta.db" ||
    fail "streamer fixture was not fresh and hollow"
STREAM_REMOTE="$TEST_ROOT/stream-remote"
mkdir -p "$STREAM_REMOTE/chunks"
start_hollow_streamer "$STREAM_ID" "$STREAM_REMOTE" "$TEST_ROOT/stream-owner.log"
sleep 1
STREAM_OBJECTS="$(find "$STREAM_REMOTE/chunks" -type f 2>/dev/null | wc -l)"
[ "$STREAM_OBJECTS" -eq 0 ] ||
    fail "hollow streamer published chunk objects from absent local bytes"
stop_live

# --- In-place hydration and transfer closure -------------------------------
run_vfs "$HOME_RECEIVER" "$RECEIVER" materialize "$MAIN_ID" \
    --in-place --verify >"$TEST_ROOT/main-materialize.log" ||
    fail "materialize --in-place failed"
grep -Eq '^Hydrated chunks: [1-9][0-9]*$' "$TEST_ROOT/main-materialize.log" ||
    fail "in-place materialization did not report hydrated chunks"
[ ! -e "$MAIN_DIR/remote" ] ||
    fail "in-place materialization retained the remote sidecar"
run_vfs "$HOME_RECEIVER" "$RECEIVER" integrity "$MAIN_DB" --json \
    >"$TEST_ROOT/main-hydrated-integrity.json" ||
    fail "integrity rejected the in-place hydrated session"
python3 - "$TEST_ROOT/main-hydrated-integrity.json" "$MAIN_DB" <<'PY' || fail "in-place hydration did not clear only the remote dependency"
import json
import sqlite3
import sys

report = json.load(open(sys.argv[1]))
assert report["ok"] is True, report
assert report["portable"] is (not report["origin_backed"]), report
assert (report["partial_origin_rows"] > 0) is report["origin_backed"], report
checks = {check["name"]: check for check in report["checks"]}
hollow = checks["storage.chunks_hollow"]
assert hollow["ok"] is True and hollow["detail"] == "chunk bytes are present", hollow

conn = sqlite3.connect(f"file:{sys.argv[2]}?mode=ro", uri=True)
assert conn.execute(
    "SELECT value FROM fs_config WHERE key = 'chunks_hollow'"
).fetchone() is None
conn.close()
PY

run_vfs "$HOME_RECEIVER" "$RECEIVER" materialize "$MAIN_ID" --in-place \
    >"$TEST_ROOT/main-materialize-again.log" ||
    fail "second in-place materialization was not idempotent"
grep -q '^Hydrated chunks: 0$' "$TEST_ROOT/main-materialize-again.log" ||
    fail "second in-place materialization was not a no-op"

mv "$REMOTE" "$REMOTE.offline"
if ! run_vfs "$HOME_RECEIVER" "$RECEIVER" run --session "$MAIN_ID" -- \
    /bin/bash -c '
set -e
test "$(/bin/cat small.txt)" = "small remote payload"
test "$(/bin/cat nested/dir/file.txt)" = "nested remote payload"
sha256sum big.bin
' >"$TEST_ROOT/main-no-remote.log" 2>&1; then
    mv "$REMOTE.offline" "$REMOTE"
    fail "hydrated session still depended on the remote"
fi
mv "$REMOTE.offline" "$REMOTE"
MAIN_HYDRATED_SHA="$(extract_hash "$TEST_ROOT/main-no-remote.log")"
[ "$MAIN_HYDRATED_SHA" = "$ORIGIN_SHA" ] ||
    fail "hydrated session bytes changed after removing the remote"

PACKED_DB="$TEST_ROOT/materialized-pack.db"
run_vfs "$HOME_RECEIVER" "$RECEIVER" pack "$MAIN_ID" \
    --output "$PACKED_DB" --json >"$TEST_ROOT/materialized-pack.json" ||
    fail "pack did not succeed after in-place materialization"
[ -f "$PACKED_DB" ] || fail "pack did not create its artifact"
run_vfs "$HOME_THIRD" "$THIRD" adopt "$PACKED_ID" \
    --db "$PACKED_DB" --base "$THIRD" >"$TEST_ROOT/packed-adopt.json" ||
    fail "third-home adopt of the packed hydrated session failed"
run_vfs "$HOME_THIRD" "$THIRD" run --session "$PACKED_ID" -- \
    /bin/bash -c '
set -e
test "$(/bin/cat small.txt)" = "small remote payload"
test "$(/bin/cat nested/dir/file.txt)" = "nested remote payload"
sha256sum big.bin
' >"$TEST_ROOT/packed-run.log" 2>&1 ||
    fail "packed round-trip content read failed"
PACKED_SHA="$(extract_hash "$TEST_ROOT/packed-run.log")"
[ "$PACKED_SHA" = "$ORIGIN_SHA" ] ||
    fail "packed round-trip changed the origin content"

# --- Remote adopt refusal paths -------------------------------------------
if run_vfs "$HOME_RECEIVER" "$RECEIVER" adopt "$MISSING_ID" \
    --remote --base "$RECEIVER" \
    >"$TEST_ROOT/no-url.out" 2>"$TEST_ROOT/no-url.err"; then
    fail "remote adopt unexpectedly succeeded without VFS_REMOTE_URL"
fi
grep -q "vfs adopt --remote requires VFS_REMOTE_URL to be configured" \
    "$TEST_ROOT/no-url.err" ||
    fail "missing remote URL refusal omitted VFS_REMOTE_URL"
[ ! -e "$HOME_RECEIVER/.vfs/run/$MISSING_ID" ] ||
    fail "missing-URL adopt left a run directory"

if run_vfs_remote "$HOME_RECEIVER" "$RECEIVER" "$REMOTE" \
    adopt "$ABSENT_ID" --remote --base "$RECEIVER" \
    >"$TEST_ROOT/absent.out" 2>"$TEST_ROOT/absent.err"; then
    fail "remote adopt unexpectedly found a nonexistent session"
fi
grep -q "Failed to fetch remote manifest" "$TEST_ROOT/absent.err" ||
    fail "missing-manifest refusal did not identify the manifest fetch"
[ ! -e "$HOME_RECEIVER/.vfs/run/$ABSENT_ID" ] ||
    fail "missing-manifest adopt left a run directory"

WRONG_REMOTE="$TEST_ROOT/wrong-remote"
cp -a "$REMOTE" "$WRONG_REMOTE"
python3 - "$WRONG_REMOTE/sessions/$ORIGIN_ID/manifest.json" <<'PY'
import json
import sys

path = sys.argv[1]
manifest = json.load(open(path))
manifest["sessionId"] = "different-session"
with open(path, "w") as output:
    json.dump(manifest, output, separators=(",", ":"))
PY
mkdir -p "$WRONG_REMOTE/sessions/$WRONG_ID"
cp "$WRONG_REMOTE/sessions/$ORIGIN_ID/manifest.json" \
    "$WRONG_REMOTE/sessions/$WRONG_ID/manifest.json"
if run_vfs_remote "$HOME_RECEIVER" "$RECEIVER" "$WRONG_REMOTE" \
    adopt "$WRONG_ID" --remote --base "$RECEIVER" \
    >"$TEST_ROOT/wrong.out" 2>"$TEST_ROOT/wrong.err"; then
    fail "remote adopt accepted a manifest for another session"
fi
grep -q "does not match requested session" "$TEST_ROOT/wrong.err" ||
    fail "wrong-session refusal omitted the session mismatch"
[ ! -e "$HOME_RECEIVER/.vfs/run/$WRONG_ID" ] ||
    fail "wrong-session adopt left a run directory"

CORRUPT_REMOTE="$TEST_ROOT/corrupt-remote"
cp -a "$REMOTE" "$CORRUPT_REMOTE"
mkdir -p "$CORRUPT_REMOTE/sessions/$CORRUPT_ID"
cp "$CORRUPT_REMOTE/sessions/$ORIGIN_ID/manifest.json" \
    "$CORRUPT_REMOTE/sessions/$CORRUPT_ID/manifest.json"
python3 - \
    "$CORRUPT_REMOTE/sessions/$CORRUPT_ID/manifest.json" \
    "$CORRUPT_REMOTE" "$CORRUPT_ID" <<'PY'
import json
import os
import sys

manifest_path, remote, session_id = sys.argv[1:]
manifest = json.load(open(manifest_path))
manifest["sessionId"] = session_id
with open(manifest_path, "w") as output:
    json.dump(manifest, output, separators=(",", ":"))
metadata = os.path.join(remote, manifest["metadata"]["key"])
data = bytearray(open(metadata, "rb").read())
assert data
data[min(4096, len(data) - 1)] ^= 0x01
with open(metadata, "wb") as output:
    output.write(data)
PY
if run_vfs_remote "$HOME_RECEIVER" "$RECEIVER" "$CORRUPT_REMOTE" \
    adopt "$CORRUPT_ID" --remote --base "$RECEIVER" \
    >"$TEST_ROOT/corrupt.out" 2>"$TEST_ROOT/corrupt.err"; then
    fail "remote adopt accepted corrupt metadata"
fi
grep -q "remote session metadata SHA-256 mismatch" "$TEST_ROOT/corrupt.err" ||
    fail "corrupt metadata refusal omitted the SHA-256 mismatch"
[ ! -e "$HOME_RECEIVER/.vfs/run/$CORRUPT_ID" ] ||
    fail "corrupt metadata adopt left a run directory"

FUTURE_REMOTE="$TEST_ROOT/future-remote"
cp -a "$REMOTE" "$FUTURE_REMOTE"
mkdir -p "$FUTURE_REMOTE/sessions/$FUTURE_ID"
cp "$FUTURE_REMOTE/sessions/$ORIGIN_ID/manifest.json" \
    "$FUTURE_REMOTE/sessions/$FUTURE_ID/manifest.json"
python3 - "$FUTURE_REMOTE/sessions/$FUTURE_ID/manifest.json" "$FUTURE_ID" <<'PY'
import json
import sys

path, session_id = sys.argv[1:]
manifest = json.load(open(path))
manifest["sessionId"] = session_id
manifest["artifactVersion"] = "99.0"
with open(path, "w") as output:
    json.dump(manifest, output, separators=(",", ":"))
PY
if run_vfs_remote "$HOME_RECEIVER" "$RECEIVER" "$FUTURE_REMOTE" \
    adopt "$FUTURE_ID" --remote --base "$RECEIVER" \
    >"$TEST_ROOT/future.out" 2>"$TEST_ROOT/future.err"; then
    fail "remote adopt accepted a future artifact version"
fi
grep -q "artifact version 99.0" "$TEST_ROOT/future.err" ||
    fail "future-version refusal omitted the unsupported version"
grep -q "upgrade vfs" "$TEST_ROOT/future.err" ||
    fail "future-version refusal omitted upgrade guidance"
[ ! -e "$HOME_RECEIVER/.vfs/run/$FUTURE_ID" ] ||
    fail "future-version adopt left a run directory"

echo "OK"
