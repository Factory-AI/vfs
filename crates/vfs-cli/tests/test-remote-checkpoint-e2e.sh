#!/bin/sh
#
# Remote checkpoint end to end: offline and live publication, idempotence,
# streaming, hollow-artifact containment, branch materialization, failure
# recovery, and reconstruction from the file-backed wire representation.
#
set -eu

echo -n "TEST remote checkpoint e2e... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
VFS_BIN="${VFS_BIN:-}"
TEST_ROOT=""
LIVE_PID=""
LIVE_SESSION=""

OFFLINE_ID="remote-offline-$$"
LIVE_ID="remote-live-$$"
STREAM_ID="remote-stream-$$"
NO_JOURNAL_ID="remote-no-journal-$$"
BRANCH_PARENT_ID="remote-branch-parent-$$"
BRANCH_CHILD_ID="remote-branch-child-$$"
FAILURE_ID="remote-failure-$$"

skip() {
    echo "SKIP: $*"
    exit 0
}

fail() {
    echo "FAILED: $*"
    exit 1
}

wait_pid_exit() {
    pid="$1"
    deadline=$(( $(date +%s) + 10 ))
    while kill -0 "$pid" 2>/dev/null; do
        [ "$(date +%s)" -lt "$deadline" ] || return 1
        sleep 0.1
    done
}

unmount_path() {
    path="$1"
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
    LIVE_SESSION=""
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
        for session_id in \
            "$OFFLINE_ID" "$LIVE_ID" "$STREAM_ID" "$NO_JOURNAL_ID" \
            "$BRANCH_PARENT_ID" "$BRANCH_CHILD_ID" "$FAILURE_ID"; do
            unmount_path "$TEST_ROOT/home/.vfs/run/$session_id/mnt"
        done
    fi
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/vfs-remote-checkpoint.*)
                chmod -R u+w "$TEST_ROOT" 2>/dev/null || true
                rm -rf "$TEST_ROOT"
                ;;
            *) echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT" ;;
        esac
    fi
}
trap cleanup EXIT INT TERM

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vfs-remote-checkpoint.XXXXXX")"
TEST_HOME="$TEST_ROOT/home"
BASE="$TEST_ROOT/base"
mkdir -p "$TEST_HOME/.cache" "$TEST_HOME/.config" "$BASE"

printf "base\n" >"$BASE/base.txt"
printf "delete through overlay\n" >"$BASE/delete-me.txt"
(
    cd "$BASE"
    git init -q
    git -c user.email=remote@example.invalid \
        -c user.name="Vfs Remote" add .
    git -c user.email=remote@example.invalid \
        -c user.name="Vfs Remote" commit -qm base
) || fail "failed to initialize base git checkout"
BASE_PIN="$(git -C "$BASE" rev-parse HEAD)" ||
    fail "failed to resolve base pin"

run_vfs() {
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" "$@"
    )
}

run_vfs_remote() {
    remote="$1"
    shift
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        VFS_REMOTE_URL="file://$remote" \
        "$VFS_BIN" "$@"
    )
}

run_vfs_remote_without_journal() {
    remote="$1"
    shift
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        VFS_JOURNAL=0 \
        VFS_REMOTE_URL="file://$remote" \
        "$VFS_BIN" "$@"
    )
}

wait_live() {
    session_id="$1"
    deadline=$(( $(date +%s) + 15 ))
    until run_vfs status "$session_id" --json 2>/dev/null |
        grep -q '"state":"live"'; do
        [ "$(date +%s)" -lt "$deadline" ] ||
            fail "$session_id never reported live"
        kill -0 "$LIVE_PID" 2>/dev/null ||
            fail "$session_id live owner exited prematurely"
        sleep 0.2
    done
}

start_live() {
    session_id="$1"
    remote="$2"
    interval_ms="$3"
    log="$4"
    (
        cd "$BASE" || exit 1
        exec env \
            HOME="$TEST_HOME" \
            XDG_CACHE_HOME="$TEST_HOME/.cache" \
            XDG_CONFIG_HOME="$TEST_HOME/.config" \
            GIT_CONFIG_GLOBAL=/dev/null \
            GIT_CONFIG_SYSTEM=/dev/null \
            VFS_FUSE_URING=0 \
            VFS_REMOTE_URL="file://$remote" \
            VFS_REMOTE_STREAM_INTERVAL_MS="$interval_ms" \
            "$VFS_BIN" run --session "$session_id" --seed-pin "$BASE_PIN" -- \
            /bin/bash -c 'while :; do sleep 0.1; done'
    ) >"$log" 2>&1 &
    LIVE_PID=$!
    LIVE_SESSION="$session_id"
    wait_live "$session_id"
}

assert_path_present() {
    database="$1"
    path="$2"
    python3 - "$database" "$path" <<'PY'
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
ino = 1
for component in [part for part in sys.argv[2].split("/") if part]:
    row = conn.execute(
        "SELECT ino FROM fs_dentry WHERE parent_ino = ? AND name = ?",
        (ino, component),
    ).fetchone()
    assert row is not None, f"missing path component {component!r} in {sys.argv[2]!r}"
    ino = row[0]
assert conn.execute("SELECT 1 FROM fs_inode WHERE ino = ?", (ino,)).fetchone()
conn.close()
PY
}

# --- Offline checkpoint round trip -----------------------------------------
OFFLINE_REMOTE="$TEST_ROOT/remote-offline"
mkdir -p "$OFFLINE_REMOTE"
run_vfs run --session "$OFFLINE_ID" --seed-pin "$BASE_PIN" -- /bin/bash -c '
set -e
head -c 200000 /dev/urandom > random.bin
printf "small inline payload\n" > small.txt
/bin/mkdir -p nested/dir
printf "nested payload\n" > nested/dir/file.txt
/bin/rm delete-me.txt
' >"$TEST_ROOT/offline-run.log" 2>&1 ||
    fail "offline checkpoint workload failed"

FIRST_TOKEN="$TEST_ROOT/offline-first.json"
run_vfs_remote "$OFFLINE_REMOTE" checkpoint "$OFFLINE_ID" --json >"$FIRST_TOKEN" ||
    fail "offline checkpoint failed"
FIRST_META_SHA="$(python3 - "$FIRST_TOKEN" <<'PY'
import json
import sys

token = json.load(open(sys.argv[1]))
assert token["sessionId"], token
assert token["seq"] > 0, token
assert token["historyEpoch"] >= 1, token
assert token["historyValid"] is True, token
assert token["uploadedChunks"] == token["chunkCount"] > 0, token
assert token["reusedChunks"] == 0, token
assert token["uploadedBytes"] == token["chunkBytes"] > 0, token
assert token["manifestKey"] == f"sessions/{token['sessionId']}/manifest.json", token
assert len(token["metadataSha256"]) == 64, token
assert token["vfsVersion"], token
print(token["metadataSha256"])
PY
)" || fail "offline checkpoint token was malformed"
FIRST_META="$OFFLINE_REMOTE/sessions/$OFFLINE_ID/meta/$FIRST_META_SHA.db"
FIRST_MANIFEST="$OFFLINE_REMOTE/sessions/$OFFLINE_ID/manifest.json"
[ -f "$FIRST_META" ] || fail "offline metadata object is missing"
[ -f "$FIRST_MANIFEST" ] || fail "offline manifest is missing"
ACTUAL_META_SHA="$(sha256sum "$FIRST_META")"
ACTUAL_META_SHA="${ACTUAL_META_SHA%% *}"
[ "$ACTUAL_META_SHA" = "$FIRST_META_SHA" ] ||
    fail "metadata object sha256 does not match its key"

python3 - "$OFFLINE_REMOTE" "$OFFLINE_ID" "$FIRST_TOKEN" "$FIRST_MANIFEST" "$FIRST_META" <<'PY' || fail "offline remote layout or metadata assertions failed"
import hashlib
import json
import os
import sqlite3
import sys

remote, session_id, token_path, manifest_path, metadata_path = sys.argv[1:]
token = json.load(open(token_path))
manifest = json.load(open(manifest_path))
assert set(manifest) == {
    "sessionId", "headSeq", "historyEpoch", "historyValid", "generation",
    "artifactVersion", "seedPin", "metadata", "chunkCount", "chunkBytes",
    "createdAtMs", "vfsVersion",
}, manifest
assert manifest["sessionId"] == session_id, manifest
assert manifest["headSeq"] == token["seq"], (manifest, token)
assert manifest["historyEpoch"] == token["historyEpoch"], (manifest, token)
assert manifest["historyValid"] is True, manifest
assert manifest["generation"] == token["generation"], (manifest, token)
assert manifest["seedPin"], manifest
assert manifest["metadata"]["sha256"] == token["metadataSha256"], manifest
assert manifest["metadata"]["key"] == (
    f"sessions/{session_id}/meta/{token['metadataSha256']}.db"
), manifest
assert manifest["metadata"]["bytes"] == os.path.getsize(metadata_path), manifest
assert hashlib.sha256(open(metadata_path, "rb").read()).hexdigest() == (
    manifest["metadata"]["sha256"]
), manifest
assert manifest["chunkCount"] == token["chunkCount"], (manifest, token)
assert manifest["chunkBytes"] == token["chunkBytes"], (manifest, token)
assert manifest["createdAtMs"] > 0 and manifest["vfsVersion"], manifest

actual_files = set()
for root, _, files in os.walk(remote):
    for name in files:
        actual_files.add(os.path.relpath(os.path.join(root, name), remote))
chunk_files = {path for path in actual_files if path.startswith("chunks/")}
expected_fixed = {
    f"sessions/{session_id}/manifest.json",
    f"sessions/{session_id}/meta/{token['metadataSha256']}.db",
}
assert actual_files == chunk_files | expected_fixed, sorted(actual_files)
assert len(chunk_files) == token["chunkCount"], (len(chunk_files), token)
assert sum(
    os.path.getsize(os.path.join(remote, path)) for path in chunk_files
) == token["chunkBytes"], token

conn = sqlite3.connect(f"file:{metadata_path}?mode=ro", uri=True)
assert conn.execute(
    "SELECT value FROM fs_config WHERE key = 'chunks_hollow'"
).fetchone() == ("1",)
assert conn.execute(
    "SELECT COUNT(*) FROM fs_chunk WHERE length(data) != 0"
).fetchone()[0] == 0
rows = conn.execute(
    "SELECT lower(hex(digest)), length(data) FROM fs_chunk"
).fetchall()
assert len(rows) == token["chunkCount"], (len(rows), token)
for digest, data_len in rows:
    assert data_len == 0
    object_path = os.path.join(remote, "chunks", digest)
    assert os.path.isfile(object_path), object_path
assert conn.execute(
    """SELECT COUNT(*)
       FROM fs_dentry d JOIN fs_inode i ON i.ino = d.ino
       WHERE d.parent_ino = 1 AND d.name = 'delete-me.txt'"""
).fetchone()[0] == 0
conn.close()
PY

# --- Idempotent re-checkpoint ----------------------------------------------
SECOND_TOKEN="$TEST_ROOT/offline-second.json"
run_vfs_remote "$OFFLINE_REMOTE" checkpoint "$OFFLINE_ID" --json >"$SECOND_TOKEN" ||
    fail "idempotent checkpoint failed"
python3 - "$FIRST_TOKEN" "$SECOND_TOKEN" "$FIRST_MANIFEST" <<'PY' || fail "idempotent checkpoint changed its published state"
import json
import sys

first, second, manifest = (json.load(open(path)) for path in sys.argv[1:])
assert second["uploadedChunks"] == 0, second
assert second["reusedChunks"] == second["chunkCount"] > 0, second
assert second["uploadedBytes"] == 0, second
assert second["metadataSha256"] == first["metadataSha256"], (first, second)
assert second["seq"] == first["seq"], (first, second)
assert manifest["headSeq"] == second["seq"], (manifest, second)
assert manifest["metadata"]["sha256"] == second["metadataSha256"], (manifest, second)
assert manifest["chunkCount"] == second["chunkCount"], (manifest, second)
PY

# --- Mutate and re-checkpoint ----------------------------------------------
run_vfs run --session "$OFFLINE_ID" -- /bin/bash -c '
set -e
head -c 100000 /dev/urandom >> random.bin
printf "after first checkpoint\n" > added-later.txt
' >"$TEST_ROOT/offline-mutate.log" 2>&1 ||
    fail "post-checkpoint mutation failed"
THIRD_TOKEN="$TEST_ROOT/offline-third.json"
run_vfs_remote "$OFFLINE_REMOTE" checkpoint "$OFFLINE_ID" --json >"$THIRD_TOKEN" ||
    fail "checkpoint after mutation failed"
python3 - "$SECOND_TOKEN" "$THIRD_TOKEN" <<'PY' || fail "mutated checkpoint did not upload only its delta"
import json
import sys

before, after = (json.load(open(path)) for path in sys.argv[1:])
assert 0 < after["uploadedChunks"] < after["chunkCount"], after
assert after["reusedChunks"] > 0, after
assert after["uploadedChunks"] + after["reusedChunks"] == after["chunkCount"], after
assert after["seq"] > before["seq"], (before, after)
PY

# --- Hollow containment ----------------------------------------------------
HOLLOW_COPY="$TEST_ROOT/hollow-copy.db"
cp "$FIRST_META" "$HOLLOW_COPY"
chmod 600 "$HOLLOW_COPY"
run_vfs integrity "$HOLLOW_COPY" --json >"$TEST_ROOT/hollow-integrity.json" ||
    fail "plain integrity rejected a hollow metadata artifact"
python3 - "$TEST_ROOT/hollow-integrity.json" <<'PY' || fail "plain integrity omitted the hollow storage finding"
import json
import sys

report = json.load(open(sys.argv[1]))
assert report["ok"] is True, report
assert report["portable"] is False, report
finding = next(
    check for check in report["checks"]
    if check["name"] == "storage.chunks_hollow"
)
assert finding["ok"] is True, finding
assert "remote metadata artifact" in finding["detail"], finding
PY

if run_vfs integrity "$HOLLOW_COPY" --json --require-portable \
    >"$TEST_ROOT/hollow-portable.out" 2>"$TEST_ROOT/hollow-portable.err"; then
    fail "portable integrity unexpectedly accepted hollow metadata"
fi
if run_vfs backup "$HOLLOW_COPY" "$TEST_ROOT/hollow-backup.db" \
    >"$TEST_ROOT/hollow-backup.out" 2>"$TEST_ROOT/hollow-backup.err"; then
    fail "backup unexpectedly accepted hollow metadata"
fi
grep -qi "remote metadata artifact.*chunk bytes are not present" \
    "$TEST_ROOT/hollow-backup.err" ||
    fail "backup refusal did not identify hollow remote metadata"
if run_vfs fs "$HOLLOW_COPY" write forbidden.txt forbidden \
    >"$TEST_ROOT/hollow-write.out" 2>"$TEST_ROOT/hollow-write.err"; then
    fail "writable open unexpectedly accepted hollow metadata"
fi
grep -qi "remote metadata artifact.*chunk bytes are not present" \
    "$TEST_ROOT/hollow-write.err" ||
    fail "writable-open refusal did not identify hollow remote metadata"

# --- Live checkpoint through the control socket ----------------------------
LIVE_REMOTE="$TEST_ROOT/remote-live"
mkdir -p "$LIVE_REMOTE"
start_live "$LIVE_ID" "$LIVE_REMOTE" 60000 "$TEST_ROOT/live-owner.log"
run_vfs_remote "$LIVE_REMOTE" run --session "$LIVE_ID" -- /bin/bash -c '
set -e
head -c 70000 /dev/urandom > live-sentinel.bin
sync
' >"$TEST_ROOT/live-writer.log" 2>&1 ||
    fail "live sentinel write failed"
LIVE_FIRST_TOKEN="$TEST_ROOT/live-first.json"
run_vfs_remote "$LIVE_REMOTE" checkpoint "$LIVE_ID" --json >"$LIVE_FIRST_TOKEN" ||
    fail "live checkpoint failed"
LIVE_FIRST_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["metadataSha256"])' "$LIVE_FIRST_TOKEN")"
LIVE_FIRST_META="$LIVE_REMOTE/sessions/$LIVE_ID/meta/$LIVE_FIRST_SHA.db"
assert_path_present "$LIVE_FIRST_META" "live-sentinel.bin" ||
    fail "live checkpoint omitted the acknowledged sentinel"
python3 - "$LIVE_FIRST_META" "$LIVE_REMOTE" <<'PY' || fail "live sentinel chunks were not reachable from the checkpoint"
import os
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
ino = conn.execute(
    "SELECT ino FROM fs_dentry WHERE parent_ino = 1 AND name = 'live-sentinel.bin'"
).fetchone()[0]
digests = [
    row[0].lower()
    for row in conn.execute(
        "SELECT hex(digest) FROM fs_data WHERE ino = ? ORDER BY chunk_index", (ino,)
    )
]
assert digests, "sentinel must use chunk storage"
for digest in digests:
    assert os.path.isfile(os.path.join(sys.argv[2], "chunks", digest)), digest
conn.close()
PY
cp "$LIVE_REMOTE/sessions/$LIVE_ID/manifest.json" "$TEST_ROOT/live-manifest-before-post.json"
run_vfs_remote "$LIVE_REMOTE" run --session "$LIVE_ID" -- /bin/bash -c '
set -e
head -c 70000 /dev/urandom > post-checkpoint.bin
sync
' >"$TEST_ROOT/live-post-writer.log" 2>&1 ||
    fail "post-checkpoint live write failed"
cmp -s "$TEST_ROOT/live-manifest-before-post.json" \
    "$LIVE_REMOTE/sessions/$LIVE_ID/manifest.json" ||
    fail "manifest changed without a second checkpoint"
if assert_path_present "$LIVE_FIRST_META" "post-checkpoint.bin" 2>/dev/null; then
    fail "first live checkpoint included a later write"
fi
LIVE_SECOND_TOKEN="$TEST_ROOT/live-second.json"
run_vfs_remote "$LIVE_REMOTE" checkpoint "$LIVE_ID" --json >"$LIVE_SECOND_TOKEN" ||
    fail "second live checkpoint failed"
LIVE_SECOND_SHA="$(python3 - "$LIVE_FIRST_TOKEN" "$LIVE_SECOND_TOKEN" <<'PY'
import json
import sys

first, second = (json.load(open(path)) for path in sys.argv[1:])
assert second["seq"] > first["seq"], (first, second)
print(second["metadataSha256"])
PY
)" || fail "second live checkpoint did not advance"
assert_path_present \
    "$LIVE_REMOTE/sessions/$LIVE_ID/meta/$LIVE_SECOND_SHA.db" \
    "post-checkpoint.bin" ||
    fail "second live checkpoint omitted the later write"
stop_live

# --- Background chunk streamer --------------------------------------------
STREAM_REMOTE="$TEST_ROOT/remote-stream"
mkdir -p "$STREAM_REMOTE"
start_live "$STREAM_ID" "$STREAM_REMOTE" 200 "$TEST_ROOT/stream-owner.log"
sleep 1
STREAM_BASELINE="$(find "$STREAM_REMOTE/chunks" -type f 2>/dev/null | wc -l)"
run_vfs_remote "$STREAM_REMOTE" run --session "$STREAM_ID" -- /bin/bash -c '
set -e
head -c 196608 /dev/urandom > streamed.bin
sync
' >"$TEST_ROOT/stream-writer.log" 2>&1 ||
    fail "streamer workload failed"
attempt=0
STREAM_COUNT="$STREAM_BASELINE"
while [ "$attempt" -lt 20 ]; do
    STREAM_COUNT="$(find "$STREAM_REMOTE/chunks" -type f 2>/dev/null | wc -l)"
    [ "$STREAM_COUNT" -ge "$((STREAM_BASELINE + 3))" ] && break
    attempt=$((attempt + 1))
    sleep 0.5
done
[ "$STREAM_COUNT" -ge "$((STREAM_BASELINE + 3))" ] ||
    fail "streamer did not publish workload chunks within 10 seconds"
[ ! -e "$STREAM_REMOTE/sessions/$STREAM_ID/manifest.json" ] ||
    fail "streamer published a manifest without a checkpoint"
STREAM_TOKEN="$TEST_ROOT/stream-checkpoint.json"
run_vfs_remote "$STREAM_REMOTE" checkpoint "$STREAM_ID" --json >"$STREAM_TOKEN" ||
    fail "checkpoint after streaming failed"
python3 - "$STREAM_TOKEN" <<'PY' || fail "checkpoint did not reuse streamed chunks"
import json
import sys

token = json.load(open(sys.argv[1]))
assert token["reusedChunks"] > 0, token
assert token["uploadedChunks"] + token["reusedChunks"] == token["chunkCount"], token
PY
stop_live

# --- Journal disabled ------------------------------------------------------
NO_JOURNAL_REMOTE="$TEST_ROOT/remote-no-journal"
mkdir -p "$NO_JOURNAL_REMOTE"
run_vfs_remote_without_journal "$NO_JOURNAL_REMOTE" run \
    --session "$NO_JOURNAL_ID" --seed-pin "$BASE_PIN" -- \
    /bin/bash -c 'printf "journal disabled\n" > no-journal.txt' \
    >"$TEST_ROOT/no-journal-run.log" 2>&1 ||
    fail "journal-disabled workload failed"
NO_JOURNAL_TOKEN="$TEST_ROOT/no-journal-token.json"
run_vfs_remote_without_journal "$NO_JOURNAL_REMOTE" checkpoint \
    "$NO_JOURNAL_ID" --json \
    >"$NO_JOURNAL_TOKEN" ||
    fail "journal-disabled checkpoint failed"
python3 - "$NO_JOURNAL_TOKEN" \
    "$NO_JOURNAL_REMOTE/sessions/$NO_JOURNAL_ID/manifest.json" <<'PY' || fail "journal-disabled checkpoint claimed replayable history"
import json
import sys

token, manifest = (json.load(open(path)) for path in sys.argv[1:])
assert token["historyValid"] is False, token
assert manifest["historyValid"] is False, manifest
assert manifest["headSeq"] == token["seq"], (manifest, token)
PY

# --- Branch materialization ------------------------------------------------
BRANCH_REMOTE="$TEST_ROOT/remote-branch"
mkdir -p "$BRANCH_REMOTE"
run_vfs run --session "$BRANCH_PARENT_ID" --seed-pin "$BASE_PIN" -- \
    /bin/bash -c '
set -e
head -c 70000 /dev/urandom > inherited-parent.bin
' >"$TEST_ROOT/branch-parent-run.log" 2>&1 ||
    fail "branch parent workload failed"
run_vfs branch "$BRANCH_PARENT_ID" --session "$BRANCH_CHILD_ID" --json \
    >"$TEST_ROOT/branch.json" ||
    fail "branch creation failed"
run_vfs run --session "$BRANCH_CHILD_ID" -- /bin/bash -c '
set -e
test -f inherited-parent.bin
head -c 70000 /dev/urandom > child-owned.bin
' >"$TEST_ROOT/branch-child-run.log" 2>&1 ||
    fail "branch child workload failed"
BRANCH_TOKEN="$TEST_ROOT/branch-token.json"
run_vfs_remote "$BRANCH_REMOTE" checkpoint "$BRANCH_CHILD_ID" --json \
    >"$BRANCH_TOKEN" ||
    fail "branch checkpoint failed"
BRANCH_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["metadataSha256"])' "$BRANCH_TOKEN")"
BRANCH_META="$BRANCH_REMOTE/sessions/$BRANCH_CHILD_ID/meta/$BRANCH_SHA.db"
assert_path_present "$BRANCH_META" "inherited-parent.bin" ||
    fail "materialized branch omitted parent content"
assert_path_present "$BRANCH_META" "child-owned.bin" ||
    fail "materialized branch omitted child content"
python3 - "$BRANCH_META" <<'PY' || fail "branch checkpoint retained an external parent dependency"
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
keys = {
    key for key, in conn.execute(
        "SELECT key FROM fs_overlay_config WHERE key LIKE 'parent_%'"
    )
}
assert not keys, keys
conn.close()
PY

# --- Failure injection and recovery ----------------------------------------
FAILURE_REMOTE="$TEST_ROOT/remote-read-only"
mkdir -p "$FAILURE_REMOTE"
run_vfs run --session "$FAILURE_ID" --seed-pin "$BASE_PIN" -- \
    /bin/bash -c 'head -c 70000 /dev/urandom > recoverable.bin' \
    >"$TEST_ROOT/failure-run.log" 2>&1 ||
    fail "failure-injection workload failed"
chmod 555 "$FAILURE_REMOTE"
if run_vfs_remote "$FAILURE_REMOTE" checkpoint "$FAILURE_ID" --json \
    >"$TEST_ROOT/read-only.out" 2>"$TEST_ROOT/read-only.err"; then
    fail "checkpoint unexpectedly published to a read-only remote"
fi
[ ! -e "$FAILURE_REMOTE/sessions/$FAILURE_ID/manifest.json" ] ||
    fail "failed checkpoint published its manifest commit point"
chmod 755 "$FAILURE_REMOTE"
run_vfs_remote "$FAILURE_REMOTE" checkpoint "$FAILURE_ID" --json \
    >"$TEST_ROOT/recovered-token.json" ||
    fail "checkpoint did not recover after remote permissions were fixed"
[ -f "$FAILURE_REMOTE/sessions/$FAILURE_ID/manifest.json" ] ||
    fail "recovered checkpoint did not publish a manifest"
if run_vfs checkpoint "$FAILURE_ID" --json \
    >"$TEST_ROOT/no-remote.out" 2>"$TEST_ROOT/no-remote.err"; then
    fail "checkpoint unexpectedly succeeded without VFS_REMOTE_URL"
fi
grep -q "VFS_REMOTE_URL" "$TEST_ROOT/no-remote.err" ||
    fail "missing-remote error did not name VFS_REMOTE_URL"

# --- Reconstruct a portable artifact from the wire -------------------------
HYDRATED="$TEST_ROOT/hydrated.db"
cp "$FIRST_META" "$HYDRATED"
chmod 600 "$HYDRATED"
python3 - "$HYDRATED" "$OFFLINE_REMOTE" <<'PY' || fail "failed to hydrate remote metadata from chunk objects"
import os
import sqlite3
import sys

database, remote = sys.argv[1:]
conn = sqlite3.connect(database)
rows = conn.execute("SELECT digest FROM fs_chunk").fetchall()
assert rows
for (digest,) in rows:
    path = os.path.join(remote, "chunks", digest.hex())
    with open(path, "rb") as chunk:
        data = chunk.read()
    conn.execute("UPDATE fs_chunk SET data = ? WHERE digest = ?", (data, digest))
conn.execute("DELETE FROM fs_config WHERE key = 'chunks_hollow'")
conn.commit()
conn.close()
PY
run_vfs integrity "$HYDRATED" --json --require-portable \
    >"$TEST_ROOT/hydrated-integrity.json" ||
    fail "reconstructed remote artifact was not portable"
run_vfs integrity "$HYDRATED" --json \
    >"$TEST_ROOT/hydrated-integrity-full.json" ||
    fail "full integrity rejected the reconstructed remote artifact"
python3 - \
    "$TEST_ROOT/hydrated-integrity.json" \
    "$TEST_ROOT/hydrated-integrity-full.json" <<'PY' || fail "reconstructed artifact integrity report was not clean"
import json
import sys

for path in sys.argv[1:]:
    report = json.load(open(path))
    assert report["ok"] is True, report
    assert report["portable"] is True, report
    assert all(check["ok"] for check in report["checks"]), report
PY

echo "OK"
