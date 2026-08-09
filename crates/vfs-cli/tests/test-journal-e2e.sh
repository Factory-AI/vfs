#!/bin/sh
#
# Operation journal and content-addressed chunk storage end to end:
# mounted mutations, live-chunk dedupe, journal kill switch, pack floor,
# and forward migration of an old artifact through adopt.
#
set -eu

echo -n "TEST journal e2e... "

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
FIXTURES="$DIR/fixtures/migrate"
VFS_BIN="${VFS_BIN:-}"
TEST_ROOT=""

JOURNAL_ID="journal-mounted-$$"
DISABLED_ID="journal-disabled-$$"
RETENTION_ID="journal-retention-$$"
ADOPT_ID="journal-adopt-old-$$"

skip() {
    echo "SKIP: $*"
    exit 0
}

fail() {
    echo "FAILED: $*"
    exit 1
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

case "$(uname -s)" in
    Linux) ;;
    *) skip "requires Linux namespaces and FUSE" ;;
esac

[ -n "$VFS_BIN" ] || command -v cargo >/dev/null 2>&1 ||
    skip "cargo is unavailable and VFS_BIN is unset"
command -v python3 >/dev/null 2>&1 || skip "python3 is unavailable"
command -v git >/dev/null 2>&1 || skip "git is unavailable"
[ -x /bin/bash ] || skip "/bin/bash is unavailable"
[ -x /bin/rm ] || skip "/bin/rm is unavailable"
[ -e /dev/fuse ] || skip "requires /dev/fuse for FUSE mounts"
[ -f "$FIXTURES/v0_4.db" ] || fail "missing committed v0_4 migration fixture"

if [ -r /proc/sys/kernel/unprivileged_userns_clone ] &&
    [ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" = "0" ]; then
    skip "unprivileged user namespaces are disabled"
fi

if [ -z "$VFS_BIN" ]; then
    cargo +nightly build --quiet --manifest-path "$CLI_DIR/Cargo.toml" >/dev/null 2>&1 ||
        fail "failed to build vfs CLI"
    VFS_BIN="$REPO_ROOT/target/debug/vfs"
fi
[ -x "$VFS_BIN" ] || fail "VFS_BIN is not executable: $VFS_BIN"

cleanup() {
    set +e
    if [ -n "$TEST_ROOT" ]; then
        for session_id in \
            "$JOURNAL_ID" "$DISABLED_ID" "$RETENTION_ID" "$ADOPT_ID"; do
            unmount_path "$TEST_ROOT/home/.vfs/run/$session_id/mnt"
        done
    fi
    if [ -n "$TEST_ROOT" ] && [ -d "$TEST_ROOT" ]; then
        case "$TEST_ROOT" in
            "${TMPDIR:-/tmp}"/vfs-journal-e2e.*)
                chmod -R u+w "$TEST_ROOT" 2>/dev/null || true
                rm -rf "$TEST_ROOT"
                ;;
            *) echo "WARNING: refusing to remove unexpected temp root: $TEST_ROOT" ;;
        esac
    fi
}
trap cleanup EXIT INT TERM

TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/vfs-journal-e2e.XXXXXX")"
TEST_HOME="$TEST_ROOT/home"
BASE="$TEST_ROOT/base"
BIN_DIR="$TEST_ROOT/bin"
mkdir -p "$TEST_HOME/.cache" "$TEST_HOME/.config" "$BASE" "$BIN_DIR"

for candidate in /usr/bin/git /bin/git; do
    if [ -x "$candidate" ]; then
        ln -s "$candidate" "$BIN_DIR/git"
        break
    fi
done
[ -e "$BIN_DIR/git" ] || ln -s "$(command -v git)" "$BIN_DIR/git"

printf "base\n" >"$BASE/base.txt"
(
    cd "$BASE"
    "$BIN_DIR/git" init -q
    "$BIN_DIR/git" -c user.email=journal@example.invalid \
        -c user.name="Vfs Journal" add .
    "$BIN_DIR/git" -c user.email=journal@example.invalid \
        -c user.name="Vfs Journal" commit -qm base
) || fail "failed to initialize the base git repository"
BASE_PIN="$("$BIN_DIR/git" -C "$BASE" rev-parse HEAD)" ||
    fail "failed to resolve the base commit"

run_vfs() {
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        PATH="$BIN_DIR:$PATH" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        "$VFS_BIN" "$@"
    )
}

run_vfs_without_journal() {
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        PATH="$BIN_DIR:$PATH" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        VFS_JOURNAL=0 \
        "$VFS_BIN" "$@"
    )
}

run_vfs_with_retention() {
    retention="$1"
    shift
    (
        cd "$BASE"
        HOME="$TEST_HOME" \
        XDG_CACHE_HOME="$TEST_HOME/.cache" \
        XDG_CONFIG_HOME="$TEST_HOME/.config" \
        PATH="$BIN_DIR:$PATH" \
        GIT_CONFIG_GLOBAL=/dev/null \
        GIT_CONFIG_SYSTEM=/dev/null \
        VFS_FUSE_URING=0 \
        VFS_JOURNAL_RETENTION_OPS="$retention" \
        "$VFS_BIN" "$@"
    )
}

# Database inspection always happens against a private copy after the owning
# foreground command has stopped. Copy sidecars too if a failure path left a
# non-single-file family so SQLite sees a coherent snapshot.
snapshot_db() {
    source="$1"
    target="$2"
    cp "$source" "$target"
    for suffix in -wal -shm; do
        if [ -e "$source$suffix" ]; then
            cp "$source$suffix" "$target$suffix"
        fi
    done
    chmod 600 "$target" "$target-wal" "$target-shm" 2>/dev/null || true
}

# --- Journal through the mount and live content dedupe ----------------------
run_vfs run --session "$JOURNAL_ID" -- /bin/bash -c '
set -e
printf "first\n" > edited.txt
printf "second\n" > edited.txt
printf "gone\n" > deleted.txt
/bin/rm deleted.txt
printf "created\n" > created.txt

# Build exactly two identical 64 KiB chunks without depending on an
# interpreter path inside the sandbox.
payload=0123456789abcdef0123456789abcdef
i=0
while [ "$i" -lt 12 ]; do
    payload=$payload$payload
    i=$((i + 1))
done
[ "${#payload}" -eq 131072 ]
printf "%s" "$payload" > dedupe-a.bin
printf "%s" "$payload" > dedupe-b.bin
' || fail "journaled mounted workload failed"

run_vfs run --session "$JOURNAL_ID" -- /bin/bash -c '
set -e
test "$(cat edited.txt)" = second
test "$(cat created.txt)" = created
test ! -e deleted.txt
test "$(wc -c < dedupe-a.bin)" -eq 131072
cmp -s dedupe-a.bin dedupe-b.bin
' || fail "journaled session did not read back its mounted mutations"

JOURNAL_DB="$TEST_HOME/.vfs/run/$JOURNAL_ID/delta.db"
JOURNAL_SNAPSHOT="$TEST_ROOT/journal-mounted.db"
snapshot_db "$JOURNAL_DB" "$JOURNAL_SNAPSHOT"
python3 - "$JOURNAL_SNAPSHOT" <<'PY' || fail "mounted journal or dedupe assertions failed"
import json
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
rows = conn.execute(
    "SELECT seq, txn_id, label, tbl, verb, row FROM fs_op_journal ORDER BY seq"
).fetchall()
assert rows, "mounted mutations produced no journal rows"

labels = {row[2] for row in rows}
assert {"create_file", "write", "unlink"} <= labels, sorted(labels)
assert all(seq > 0 and txn_id > 0 for seq, txn_id, *_ in rows), rows
for _, _, _, tbl, verb, row_json in rows:
    assert tbl in {
        "fs_inode", "fs_dentry", "fs_data", "fs_symlink", "fs_whiteout",
        "fs_origin", "fs_partial_origin", "fs_chunk_override",
        "fs_overlay_config",
    }, tbl
    assert verb in {"upsert", "delete"}, verb
    assert isinstance(json.loads(row_json), dict), row_json

# Journal rows must form non-interleaved transaction groups, and grouping the
# rows by txn_id must account for every sequence exactly once.
seen = set()
current = None
groups = {}
for seq, txn_id, label, *_ in rows:
    if txn_id != current:
        assert txn_id not in seen, f"txn_id {txn_id} is interleaved"
        seen.add(txn_id)
        current = txn_id
    groups.setdefault(txn_id, []).append((seq, label))
assert sum(len(group) for group in groups.values()) == len(rows)
for txn_id, group in groups.items():
    assert txn_id == min(seq for seq, _ in group), (txn_id, group)

def inode(name):
    row = conn.execute(
        "SELECT ino FROM fs_dentry WHERE parent_ino = 1 AND name = ?", (name,)
    ).fetchone()
    assert row is not None, name
    return row[0]

a = inode("dedupe-a.bin")
b = inode("dedupe-b.bin")
sizes = conn.execute(
    "SELECT ino, size FROM fs_inode WHERE ino IN (?, ?) ORDER BY ino", (a, b)
).fetchall()
assert [size for _, size in sizes] == [131072, 131072], sizes

a_map = conn.execute(
    "SELECT chunk_index, hex(digest) FROM fs_data WHERE ino = ? ORDER BY chunk_index",
    (a,),
).fetchall()
b_map = conn.execute(
    "SELECT chunk_index, hex(digest) FROM fs_data WHERE ino = ? ORDER BY chunk_index",
    (b,),
).fetchall()
assert len(a_map) == 2 and len(b_map) == 2, (a_map, b_map)
assert a_map == b_map, (a_map, b_map)
live_digests = {digest for _, digest in a_map + b_map}
assert len(live_digests) == 1, live_digests
digest = next(iter(live_digests))
chunk = conn.execute(
    "SELECT refcount FROM fs_chunk WHERE hex(digest) = ?", (digest,)
).fetchall()
assert chunk == [(4,)], chunk
conn.close()
PY

# --- Journal kill switch ----------------------------------------------------
run_vfs_without_journal run --session "$DISABLED_ID" -- /bin/bash -c '
set -e
printf "journal disabled but durable\n" > disabled.txt
printf "temporary\n" > disabled-delete.txt
/bin/rm disabled-delete.txt
' || fail "journal-disabled mounted workload failed"

run_vfs_without_journal run --session "$DISABLED_ID" -- /bin/bash -c '
set -e
test "$(cat disabled.txt)" = "journal disabled but durable"
test ! -e disabled-delete.txt
' || fail "journal-disabled files did not read back correctly"

DISABLED_DB="$TEST_HOME/.vfs/run/$DISABLED_ID/delta.db"
DISABLED_SNAPSHOT="$TEST_ROOT/journal-disabled.db"
snapshot_db "$DISABLED_DB" "$DISABLED_SNAPSHOT"
python3 - "$DISABLED_SNAPSHOT" <<'PY' || fail "journal kill-switch assertions failed"
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
counts = {
    table: conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
    for table in ("fs_op_journal", "fs_journal_chunk")
}
assert counts == {
    "fs_op_journal": 0,
    "fs_journal_chunk": 0,
}, counts
assert conn.execute(
    "SELECT value FROM fs_config WHERE key = 'history_valid'"
).fetchone() == ("0",)
conn.close()
PY

# --- Pack establishes a fresh history floor ---------------------------------
run_vfs run --session "$RETENTION_ID" -- /bin/bash -c '
set -e
i=0
while [ "$i" -lt 30 ]; do
    printf "mutation-%s\n" "$i" > "retention-$i.tmp"
    /bin/rm "retention-$i.tmp"
    i=$((i + 1))
done
printf "survives pack\n" > retention-keep.txt
' || fail "retention workload failed"

RETENTION_DB="$TEST_HOME/.vfs/run/$RETENTION_ID/delta.db"
RETENTION_BEFORE="$TEST_ROOT/retention-before.db"
snapshot_db "$RETENTION_DB" "$RETENTION_BEFORE"
PRE_PACK_COUNT="$(python3 - "$RETENTION_BEFORE" <<'PY'
import sqlite3
import sys
conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
print(conn.execute("SELECT COUNT(*) FROM fs_op_journal").fetchone()[0])
conn.close()
PY
)"
[ "$PRE_PACK_COUNT" -gt 50 ] ||
    fail "retention workload produced only $PRE_PACK_COUNT journal rows"

PACKED="$TEST_ROOT/retention-packed.db"
run_vfs_with_retention 5 pack "$RETENTION_ID" --output "$PACKED" --json \
    >"$TEST_ROOT/retention-pack.json" ||
    fail "pack with journal retention failed"

PACKED_SNAPSHOT="$TEST_ROOT/retention-packed-copy.db"
snapshot_db "$PACKED" "$PACKED_SNAPSHOT"
python3 - "$PACKED_SNAPSHOT" "$PRE_PACK_COUNT" <<'PY' || fail "packed journal retention assertions failed"
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
pre = int(sys.argv[2])
post = conn.execute("SELECT COUNT(*) FROM fs_op_journal").fetchone()[0]
assert post == 0, (pre, post)
floor = int(conn.execute(
    "SELECT value FROM fs_config WHERE key = 'history_floor_seq'"
).fetchone()[0])
roots = conn.execute(
    """SELECT through_seq, reason, history_epoch
       FROM fs_snapshot
       ORDER BY snapshot_id"""
).fetchall()
assert roots == [(floor, "pack", 1)], roots
assert floor > 1, floor
orphans = conn.execute(
    """SELECT COUNT(*)
       FROM fs_journal_chunk c
       LEFT JOIN fs_op_journal j ON j.seq = c.seq
       WHERE j.seq IS NULL"""
).fetchone()[0]
assert orphans == 0, orphans
conn.close()
PY

run_vfs integrity --json "$PACKED" >"$TEST_ROOT/retention-integrity.json" ||
    fail "integrity rejected the retained packed artifact"
python3 - "$TEST_ROOT/retention-integrity.json" <<'PY' || fail "packed integrity report did not report ok:true"
import json
import sys
report = json.load(open(sys.argv[1]))
assert report["ok"] is True, report
PY

# --- Old artifact adopts forward -------------------------------------------
OLD_ARTIFACT="$TEST_ROOT/v0_4-artifact.db"
cp "$FIXTURES/v0_4.db" "$OLD_ARTIFACT"
chmod 600 "$OLD_ARTIFACT"
# The whole section runs under the kill switch: adopt's forward migration
# journals a root_init row when the runner's uid differs from the fixture
# creator's, which made a bare row-count assertion uid-dependent.
run_vfs_without_journal adopt "$ADOPT_ID" --db "$OLD_ARTIFACT" --base "$BASE" --pin "$BASE_PIN" \
    --json >"$TEST_ROOT/adopt-old.json" ||
    fail "adopt failed to migrate the v0.4 artifact"
python3 - "$TEST_ROOT/adopt-old.json" "$ADOPT_ID" <<'PY' || fail "old-artifact adopt manifest was malformed"
import json
import sys
manifest = json.load(open(sys.argv[1]))
assert manifest["sessionId"] == sys.argv[2], manifest
assert manifest["schemaVersion"] == "0.8", manifest
PY

run_vfs_without_journal run --session "$ADOPT_ID" -- /bin/bash -c '
test "$(cat dir/small.txt)" = "hello fixture"
' || fail "adopted v0.4 artifact did not serve its migrated contents"

ADOPTED_DB="$TEST_HOME/.vfs/run/$ADOPT_ID/delta.db"
ADOPTED_SNAPSHOT="$TEST_ROOT/adopted-old.db"
snapshot_db "$ADOPTED_DB" "$ADOPTED_SNAPSHOT"
python3 - "$ADOPTED_SNAPSHOT" <<'PY' || fail "adopted old-artifact schema assertions failed"
import sqlite3
import sys

conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
assert conn.execute("PRAGMA user_version").fetchone()[0] == 8
tables = {
    row[0]
    for row in conn.execute(
        "SELECT name FROM sqlite_master WHERE type = 'table'"
    )
}
required = {
    "fs_chunk",
    "fs_op_journal",
    "fs_journal_chunk",
    "fs_snapshot",
    "fs_snapshot_inode",
    "fs_snapshot_dentry",
    "fs_snapshot_data",
    "fs_snapshot_chunk",
}
assert required <= tables, sorted(required - tables)
columns = [row[1] for row in conn.execute("PRAGMA table_info(fs_data)")]
assert columns == ["ino", "chunk_index", "digest"], columns
assert conn.execute(
    "SELECT COUNT(*) FROM fs_data WHERE typeof(digest) != 'blob' OR length(digest) != 32"
).fetchone()[0] == 0
counts = {
    table: conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0]
    for table in ("fs_op_journal", "fs_journal_chunk")
}
assert counts == {
    "fs_op_journal": 0,
    "fs_journal_chunk": 0,
}, counts
assert conn.execute(
    "SELECT COUNT(*) FROM fs_snapshot WHERE reason = 'migrate' AND history_epoch = 1 AND through_seq = 0"
).fetchone()[0] == 1
conn.close()
PY

echo "OK"
