#!/usr/bin/env sh
#
# Migrate consolidation regression (D6):
#   - one `agentfs migrate <id-or-path>` lands every supported old schema at
#     CURRENT in a single invocation and is idempotent
#   - open paths (fs ls) reject old schemas with guidance naming that command
#   - encrypted old databases migrate in place with --key/--cipher, leaving no
#     plaintext copies behind
#   - the split `migrate-v0-5` command is gone
#   - cross-flow: a migrated old database survives a mounted git workload and
#     a clean integrity check
set -eu

DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$DIR/.." && pwd)"
FIXTURES="$DIR/fixtures/migrate"

# Kept in step with tests/migrate_fixtures.rs.
ENC_KEY="00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
ENC_CIPHER="aes256gcm"

echo -n "TEST migrate consolidation... "

ROOT="$(mktemp -d "${TMPDIR:-/tmp}/agentfs-migrate-consolidation.XXXXXX")"
MNT="$ROOT/mnt"
PINNED_TMP="$ROOT/pinned-tmp"
mkdir -p "$MNT" "$PINNED_TMP" "$ROOT/home"
MOUNT_PID=""

cleanup() {
    if [ -n "$MOUNT_PID" ]; then
        kill "$MOUNT_PID" 2>/dev/null || true
    fi
    if mountpoint -q "$MNT" 2>/dev/null; then
        fusermount3 -u "$MNT" >/dev/null 2>&1 || fusermount -u "$MNT" >/dev/null 2>&1 || true
    fi
    rm -rf "$ROOT"
}
trap cleanup EXIT INT TERM

fail() {
    echo "FAILED: $*"
    exit 1
}

run_agentfs() {
    if [ -n "${AGENTFS_BIN:-}" ]; then
        "$AGENTFS_BIN" "$@"
    else
        cargo +nightly run --quiet --manifest-path "$CLI_DIR/Cargo.toml" -- "$@"
    fi
}

user_version_of() {
    python3 - "$1" <<'EOF'
import sqlite3, sys
conn = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
print(conn.execute("PRAGMA user_version").fetchone()[0])
conn.close()
EOF
}

[ -f "$FIXTURES/v0_0.db" ] || fail "missing committed fixtures under $FIXTURES"

# --- Obsolete split command is gone -----------------------------------------
if run_agentfs migrate-v0-5 --help >"$ROOT/mv5.out" 2>&1; then
    fail "migrate-v0-5 still exists as a command"
fi
if run_agentfs --help 2>/dev/null | grep -q 'migrate-v0-5'; then
    fail "--help still advertises migrate-v0-5"
fi

# --- One migrate per supported old schema -----------------------------------
for name in v0_0 v0_2 v0_4; do
    DB="$ROOT/$name.db"
    cp "$FIXTURES/$name.db" "$DB"

    # Open paths must refuse the old schema and name the command that fixes it.
    if run_agentfs fs "$DB" ls / >"$ROOT/$name-ls-before.out" 2>"$ROOT/$name-ls-before.err"; then
        fail "$name: fs ls opened an old-schema database without migration"
    fi
    grep -q "agentfs migrate $DB" "$ROOT/$name-ls-before.err" \
        || fail "$name: schema-mismatch guidance does not name 'agentfs migrate $DB' (stderr: $(cat "$ROOT/$name-ls-before.err"))"
    grep -q 'migrate-v0-5' "$ROOT/$name-ls-before.err" \
        && fail "$name: guidance mentions the deleted migrate-v0-5 command"

    run_agentfs migrate "$DB" >"$ROOT/$name-migrate.out" 2>&1 \
        || fail "$name: migrate failed: $(cat "$ROOT/$name-migrate.out")"
    grep -q 'Migration completed successfully.' "$ROOT/$name-migrate.out" \
        || fail "$name: migrate output missing completion line"
    grep -q 'Target schema version: 0.5 (CURRENT)' "$ROOT/$name-migrate.out" \
        || fail "$name: migrate output missing CURRENT target line"

    UV="$(user_version_of "$DB")"
    [ "$UV" = "5" ] || fail "$name: user_version after migrate is $UV, expected 5"

    # Idempotent second run.
    run_agentfs migrate "$DB" >"$ROOT/$name-migrate2.out" 2>&1 \
        || fail "$name: second migrate failed"
    grep -q 'Database is already at schema 0.5.' "$ROOT/$name-migrate2.out" \
        || fail "$name: second migrate is not idempotent: $(cat "$ROOT/$name-migrate2.out")"

    run_agentfs integrity "$DB" >"$ROOT/$name-integrity.out" 2>&1 \
        || fail "$name: integrity failed after migrate: $(cat "$ROOT/$name-integrity.out")"
    run_agentfs fs "$DB" ls / >"$ROOT/$name-ls-after.out" 2>&1 \
        || fail "$name: fs ls failed after migrate"
    grep -q 'small.txt' "$ROOT/$name-ls-after.out" \
        || fail "$name: migrated filesystem lost its contents"
done

# --- Encrypted old database migrates with --key/--cipher --------------------
ENC_DB="$ROOT/v0_4-encrypted.db"
cp "$FIXTURES/v0_4-encrypted.db" "$ENC_DB"
(
    export TMPDIR="$PINNED_TMP" TMP="$PINNED_TMP" TEMP="$PINNED_TMP"
    run_agentfs migrate "$ENC_DB" --key "$ENC_KEY" --cipher "$ENC_CIPHER"
) >"$ROOT/enc-migrate.out" 2>&1 \
    || fail "encrypted migrate failed: $(cat "$ROOT/enc-migrate.out")"
grep -q 'Migration completed successfully.' "$ROOT/enc-migrate.out" \
    || fail "encrypted migrate output missing completion line"

LEFTOVER="$(find "$PINNED_TMP" -mindepth 1 | head -5)"
[ -z "$LEFTOVER" ] || fail "encrypted migrate left temp artifacts: $LEFTOVER"
STRAYS="$(find "$ROOT" -maxdepth 1 -name 'v0_4-encrypted.db*' ! -name 'v0_4-encrypted.db' | head -5)"
[ -z "$STRAYS" ] || fail "encrypted migrate left sidecar/copy artifacts: $STRAYS"

run_agentfs fs --key "$ENC_KEY" --cipher "$ENC_CIPHER" "$ENC_DB" ls / \
    >"$ROOT/enc-ls.out" 2>&1 || fail "encrypted fs ls with key failed"
grep -q 'small.txt' "$ROOT/enc-ls.out" || fail "encrypted migrated fs lost its contents"
if run_agentfs fs "$ENC_DB" ls / >/dev/null 2>&1; then
    fail "encrypted database opened without the key"
fi
run_agentfs integrity "$ENC_DB" --key "$ENC_KEY" --cipher "$ENC_CIPHER" >/dev/null 2>&1 \
    || fail "encrypted integrity with key failed"

# --- Cross-flow: migrated old DB survives a mounted git workload ------------
case "$(uname -s)" in
    Linux)
        if [ ! -e /dev/fuse ]; then
            echo "OK (mount leg skipped: no /dev/fuse)"
            exit 0
        fi
        ;;
    *)
        echo "OK (mount leg skipped: requires Linux FUSE)"
        exit 0
        ;;
esac

WORK_DB="$ROOT/v0_4.db"   # migrated above
run_agentfs mount "$WORK_DB" "$MNT" --foreground >"$ROOT/mount.log" 2>&1 &
MOUNT_PID=$!

WAITED=0
while [ $WAITED -lt 40 ]; do
    if mountpoint -q "$MNT" 2>/dev/null; then
        break
    fi
    sleep 0.25
    WAITED=$((WAITED + 1))
done
mountpoint -q "$MNT" || fail "migrated database did not mount: $(tail -5 "$ROOT/mount.log")"

[ "$(cat "$MNT/dir/small.txt")" = "hello fixture" ] \
    || fail "migrated data not readable through the mount"

(
    set -e
    cd "$MNT"
    mkdir -p repo && cd repo
    export HOME="$ROOT/home" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
    git init -q .
    git config user.email agentfs@example.invalid
    git config user.name AgentFS
    printf 'one\n' > a.txt
    printf 'two\n' > b.txt
    git add .
    git commit -q -m 'migrated workload'
    [ -z "$(git status --porcelain)" ]
    git fsck --strict --no-progress
) >"$ROOT/git.log" 2>&1 || fail "git workload on migrated mount failed: $(tail -10 "$ROOT/git.log")"

fusermount3 -u "$MNT" 2>/dev/null || fusermount -u "$MNT" 2>/dev/null \
    || fail "unmount failed"
WAITED=0
while kill -0 "$MOUNT_PID" 2>/dev/null && [ $WAITED -lt 40 ]; do
    sleep 0.25
    WAITED=$((WAITED + 1))
done
kill -0 "$MOUNT_PID" 2>/dev/null && fail "mount owner did not exit after unmount"
MOUNT_PID=""

run_agentfs integrity "$WORK_DB" --json >"$ROOT/final-integrity.json" 2>"$ROOT/final-integrity.err" \
    || fail "integrity after mounted workload failed: $(cat "$ROOT/final-integrity.err")"
python3 - "$ROOT/final-integrity.json" <<'EOF' || fail "integrity --json did not report ok:true"
import json, sys
report = json.load(open(sys.argv[1]))
assert report["ok"] is True, report
EOF

echo "OK"
