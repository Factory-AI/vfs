#!/bin/sh
#
# `vfs branch` end to end: fork an inactive session, fork a live session
# through the control socket, resume branches over the frozen parent
# artifact, isolate branch writes from the parent, share one artifact
# between same-state branches, and refuse a drifted artifact with the
# invalid-session exit code.
#
set -eu

echo -n "TEST branch lifecycle... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
VFS_BIN="${VFS_BIN:-}"
TEST_ROOT=""
LIVE_PID=""

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
        if [ "$(date +%s)" -ge "$deadline" ]; then
            return 1
        fi
        sleep 0.1
    done
}

case "$(uname -s)" in
    Linux) ;;
    *) skip "requires Linux namespaces and FUSE" ;;
esac

[ -n "${VFS_BIN:-}" ] || command -v cargo >/dev/null 2>&1 ||
    skip "cargo is unavailable and VFS_BIN is unset"
command -v python3 >/dev/null 2>&1 || skip "python3 is unavailable"
command -v git >/dev/null 2>&1 || skip "git is unavailable"
[ -x /bin/bash ] || skip "/bin/bash is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"

if [ -r /proc/sys/kernel/unprivileged_userns_clone ] &&
    [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
    skip "unprivileged user namespaces are disabled"
fi

if [ -z "$VFS_BIN" ]; then
    cargo build --quiet --manifest-path "$CLI_DIR/Cargo.toml" >/dev/null 2>&1 ||
        fail "failed to build vfs CLI"
    VFS_BIN="$REPO_ROOT/target/debug/vfs"
fi
[ -x "$VFS_BIN" ] || fail "VFS_BIN is not executable: $VFS_BIN"

cleanup() {
    set +e
    if [ -n "$LIVE_PID" ] && kill -0 "$LIVE_PID" 2>/dev/null; then
        kill -TERM "$LIVE_PID" 2>/dev/null || true
        wait_pid_exit "$LIVE_PID" || kill -KILL "$LIVE_PID" 2>/dev/null || true
        wait "$LIVE_PID" 2>/dev/null || true
    fi
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/vfs-branch.*)
                # Published artifacts are chmod 0444; restore write bits so
                # the temp root removal cannot leave litter behind.
                chmod -R u+w "$TEST_ROOT" 2>/dev/null || true
                rm -rf "$TEST_ROOT"
                ;;
            *) echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT" ;;
        esac
    fi
}
trap cleanup EXIT INT TERM

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vfs-branch.XXXXXX")"
TEST_HOME="$TEST_ROOT/home"
BASE="$TEST_ROOT/base"
ARTIFACTS="$TEST_HOME/.vfs/artifacts"
mkdir -p "$TEST_HOME/.cache" "$TEST_HOME/.config" "$BASE"
printf "host payload\n" >"$BASE/host.txt"
printf "delete me\n" >"$BASE/host-del.txt"
# Adopt verifies the receiving checkout, so the shared base must be a git repo.
(
    cd "$BASE"
    git init -q
    git -c user.email=t@t -c user.name=t add .
    git -c user.email=t@t -c user.name=t commit -qm base
) || fail "failed to initialize the base git repository"

vfs_home() {
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" "$@"
    )
}

BASE_PIN="$(git -C "$BASE" rev-parse HEAD)" || fail "failed to read base HEAD"

# --- Parent session state: new file, host edit, host delete (whiteout) ---
vfs_home run --session branch-parent --seed-pin "$BASE_PIN" /bin/bash -c '
set -e
printf "parent bytes\n" > parent.txt
printf "edited by parent\n" > host.txt
rm host-del.txt
' || fail "parent session run failed"

# --- Fork the inactive parent ---
MANIFEST_B1="$TEST_ROOT/b1.json"
vfs_home branch branch-parent --session branch-b1 >"$MANIFEST_B1" ||
    fail "branch of inactive parent failed"
python3 - "$MANIFEST_B1" "$ARTIFACTS" "$BASE_PIN" <<'PY' || fail "b1 manifest malformed"
import json, os, sys
m = json.load(open(sys.argv[1]))
assert m["sessionId"] == "branch-b1", m
assert m["parentSessionId"] == "branch-parent", m
assert m["parentLive"] is False, m
assert m["seedPin"] == sys.argv[3], "branch must inherit the parent seed pin"
digest = m["parentArtifactSha256"]
assert len(digest) == 64, m
path = m["artifactPath"]
assert os.path.isfile(path), path
assert path == os.path.join(sys.argv[2], digest + ".db"), path
assert not os.access(path, os.W_OK), "artifact must be write-protected"
PY

# --- Same-state fork shares one artifact (before any further parent run:
# --- every session run appends an audit row, which is a new parent state) ---
MANIFEST_B2="$TEST_ROOT/b2.json"
vfs_home branch branch-parent --session branch-b2 >"$MANIFEST_B2" ||
    fail "second branch of the same parent failed"
python3 - "$MANIFEST_B1" "$MANIFEST_B2" "$ARTIFACTS" <<'PY' || fail "artifact dedupe broken"
import json, os, sys
a = json.load(open(sys.argv[1]))
b = json.load(open(sys.argv[2]))
assert a["parentArtifactSha256"] == b["parentArtifactSha256"], (a, b)
count = len([n for n in os.listdir(sys.argv[3]) if n.endswith(".db")])
assert count == 1, f"expected one shared artifact, found {count}"
PY

# --- The branch sees parent state and its writes stay out of the parent ---
vfs_home run --session branch-b1 /bin/bash -c '
set -e
test "$(cat parent.txt)" = "parent bytes"
test "$(cat host.txt)" = "edited by parent"
test ! -e host-del.txt
printf "branch bytes\n" > branch-only.txt
' || fail "branch resume did not serve the parent state"

vfs_home run --session branch-parent /bin/bash -c '
set -e
test ! -e branch-only.txt
test "$(cat parent.txt)" = "parent bytes"
' || fail "branch write leaked into the parent session"

# --- Fork a live parent through the control socket ---
# exec so LIVE_PID is the vfs binary itself (not a subshell TERM would not
# reach), and detach stdout/stderr so the orphaned child cannot hold this
# script's output pipe open past the test.
(
    cd "$BASE" || exit 1
    exec env \
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" run --session branch-parent /bin/bash -c '
printf "pre-live-branch\n" > live-marker.txt
sync
while :; do sleep 0.1; done
'
) >"$TEST_ROOT/live.log" 2>&1 &
LIVE_PID=$!
deadline=$(( $(date +%s) + 15 ))
until vfs_home status branch-parent --json 2>/dev/null | grep -q '"state":"live"'; do
    [ "$(date +%s)" -lt "$deadline" ] || fail "parent session never reported live"
    kill -0 "$LIVE_PID" 2>/dev/null || fail "live parent exited prematurely"
    sleep 0.2
done

MANIFEST_B3="$TEST_ROOT/b3.json"
vfs_home branch branch-parent --session branch-b3 >"$MANIFEST_B3" ||
    fail "branch of live parent failed"
grep -q '"parentLive":true' "$MANIFEST_B3" || fail "live branch not marked parentLive"

# A write made to the still-live parent AFTER the fork must be invisible to
# the branch: the snapshot point is the fork's drain, nothing later.
vfs_home run --session branch-parent /bin/bash -c '
printf "after the fork\n" > post-marker.txt
' || fail "joiner write to the live parent failed"

# Stop the live parent (signal teardown is a supported exit path) before
# resuming the branch.
kill -TERM "$LIVE_PID" 2>/dev/null || true
wait_pid_exit "$LIVE_PID" || kill -KILL "$LIVE_PID" 2>/dev/null || true
wait "$LIVE_PID" 2>/dev/null || true
LIVE_PID=""

vfs_home run --session branch-b3 /bin/bash -c '
set -e
test "$(cat live-marker.txt)" = "pre-live-branch"
test "$(cat parent.txt)" = "parent bytes"
test ! -e post-marker.txt
' || fail "live-forked branch does not serve exactly the fork-time state"

vfs_home run --session branch-parent /bin/bash -c '
set -e
test "$(cat post-marker.txt)" = "after the fork"
' || fail "post-fork joiner write lost from the parent"

# --- Pack materializes the branch; a receiver with no artifact store
# --- reconstructs the full view (self-containment of the wire contract) ---
PACKED="$TEST_ROOT/b2-packed.db"
vfs_home pack branch-b2 --output "$PACKED" >"$TEST_ROOT/pack.json" ||
    fail "pack of a branch session failed"
ADOPT_HOME="$TEST_ROOT/home2"
mkdir -p "$ADOPT_HOME/.cache" "$ADOPT_HOME/.config"

vfs_home2() {
    (
        cd "$BASE"
        HOME="$ADOPT_HOME" \
        XDG_CACHE_HOME="$ADOPT_HOME/.cache" \
        XDG_CONFIG_HOME="$ADOPT_HOME/.config" \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" "$@"
    )
}

vfs_home2 adopt branch-b2 --db "$PACKED" --base "$BASE" >"$TEST_ROOT/adopt.json" ||
    fail "adopt of a packed branch failed"
[ ! -d "$ADOPT_HOME/.vfs/artifacts" ] ||
    fail "adopt must not need or create an artifact store"
vfs_home2 run --session branch-b2 /bin/bash -c '
set -e
test "$(cat parent.txt)" = "parent bytes"
test "$(cat host.txt)" = "edited by parent"
test ! -e host-del.txt
' || fail "adopted packed branch does not reproduce the branched view"

# --- Branch of a branch: the chain serves every layer through a real run ---
vfs_home run --session branch-b3 /bin/bash -c '
printf "b3 owned\n" > from-b3.txt
' || fail "write into branch b3 failed"
vfs_home branch branch-b3 --session branch-b4 >"$TEST_ROOT/b4.json" ||
    fail "branch of a branch failed"
vfs_home run --session branch-b4 /bin/bash -c '
set -e
test "$(cat from-b3.txt)" = "b3 owned"
test "$(cat live-marker.txt)" = "pre-live-branch"
test "$(cat parent.txt)" = "parent bytes"
test "$(cat host.txt)" = "edited by parent"
test ! -e host-del.txt
test ! -e post-marker.txt
printf "b4 owned\n" > from-b4.txt
' || fail "branch-of-branch does not serve the whole chain"
vfs_home run --session branch-b3 /bin/bash -c '
test ! -e from-b4.txt
' || fail "grandchild write leaked into its parent branch"

# --- The exec surface serves the same stack ---
EXEC_OUT="$TEST_ROOT/exec-out.txt"
(
    HOME="$TEST_HOME" \
    XDG_CACHE_HOME="$TEST_HOME/.cache" \
    XDG_CONFIG_HOME="$TEST_HOME/.config" \
    VFS_FUSE_URING=0 \
    "$VFS_BIN" exec "$TEST_HOME/.vfs/run/branch-b4/delta.db" \
        /bin/bash -c 'cat from-b3.txt parent.txt from-b4.txt'
) >"$EXEC_OUT" 2>"$TEST_ROOT/exec-err.txt" ||
    fail "vfs exec on a branch delta failed: $(cat "$TEST_ROOT/exec-err.txt")"
printf 'b3 owned\nparent bytes\nb4 owned\n' | cmp -s - "$EXEC_OUT" ||
    fail "exec surface served the wrong stack: $(cat "$EXEC_OUT")"

# --- diff/status report only the branch's own delta ---
vfs_home status branch-b4 --json >"$TEST_ROOT/b4-status.json" ||
    fail "status on a branch session failed"
grep -q '"state":"stopped"' "$TEST_ROOT/b4-status.json" ||
    fail "branch status not stopped: $(cat "$TEST_ROOT/b4-status.json")"
DIFF_OUT="$TEST_ROOT/b4-diff.txt"
vfs_home diff branch-b4 >"$DIFF_OUT" 2>/dev/null || fail "diff on a branch session failed"
grep -q 'from-b4.txt' "$DIFF_OUT" || fail "diff must list the branch's own write"
if grep -q 'from-b3.txt' "$DIFF_OUT"; then
    fail "diff must not attribute parent-layer content to the branch"
fi

# --- prune artifacts collects exactly the orphaned chain member ---
vfs_home branch branch-b4 --session branch-b5 >"$TEST_ROOT/b5.json" ||
    fail "throwaway branch failed"
B5_DIGEST="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["parentArtifactSha256"])' "$TEST_ROOT/b5.json")"
rm -rf "$TEST_HOME/.vfs/run/branch-b5"
vfs_home prune artifacts --dry-run >"$TEST_ROOT/prune-dry.json" ||
    fail "prune artifacts dry-run failed"
python3 - "$TEST_ROOT/prune-dry.json" "$B5_DIGEST" "$ARTIFACTS" <<'PY' || fail "dry-run report wrong"
import json, os, sys
r = json.load(open(sys.argv[1]))
assert r["dryRun"] is True, r
assert r["removed"] == [sys.argv[2]], r
assert os.path.isfile(os.path.join(sys.argv[3], sys.argv[2] + ".db")), "dry-run must not delete"
PY
vfs_home prune artifacts >"$TEST_ROOT/prune.json" || fail "prune artifacts failed"
python3 - "$TEST_ROOT/prune.json" "$B5_DIGEST" "$ARTIFACTS" <<'PY' || fail "prune report wrong"
import json, os, sys
r = json.load(open(sys.argv[1]))
assert r["dryRun"] is False, r
assert r["removed"] == [sys.argv[2]], r
assert r["reclaimedBytes"] > 0, r
assert not os.path.exists(os.path.join(sys.argv[3], sys.argv[2] + ".db")), "artifact must be gone"
PY
vfs_home run --session branch-b4 /bin/bash -c '
test "$(cat from-b3.txt)" = "b3 owned"
' || fail "prune broke a surviving branch"

# --- A packed grandchild folds the whole chain for a store-less receiver ---
PACKED_B4="$TEST_ROOT/b4-packed.db"
vfs_home pack branch-b4 --output "$PACKED_B4" >"$TEST_ROOT/b4-pack.json" ||
    fail "pack of a branch-of-branch failed"
vfs_home2 adopt branch-b4 --db "$PACKED_B4" --base "$BASE" >/dev/null ||
    fail "adopt of a packed branch-of-branch failed"
vfs_home2 run --session branch-b4 /bin/bash -c '
set -e
test "$(cat from-b3.txt)" = "b3 owned"
test "$(cat from-b4.txt)" = "b4 owned"
test "$(cat live-marker.txt)" = "pre-live-branch"
test "$(cat parent.txt)" = "parent bytes"
test ! -e host-del.txt
test ! -e post-marker.txt
' || fail "adopted packed chain does not reproduce the grandchild view"

# --- Drifted artifact refuses the mount with the invalid-session code ---
ARTIFACT_FILE="$ARTIFACTS/$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["parentArtifactSha256"])' "$MANIFEST_B1").db"
chmod u+w "$ARTIFACT_FILE"
printf "drift" >>"$ARTIFACT_FILE"
set +e
vfs_home run --session branch-b1 /bin/true >"$TEST_ROOT/refusal.log" 2>&1
REFUSAL_CODE=$?
set -e
[ "$REFUSAL_CODE" -eq 5 ] ||
    fail "drifted artifact must exit 5 (invalid session), got $REFUSAL_CODE: $(cat "$TEST_ROOT/refusal.log")"
grep -qi "drifted" "$TEST_ROOT/refusal.log" ||
    fail "refusal message must name the drifted parent artifact"

echo "PASS"
