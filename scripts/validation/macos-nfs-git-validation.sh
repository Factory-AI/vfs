#!/usr/bin/env bash
#
# Validate the macOS NFS path for git loose-object writes (#333) and the
# `vfs run` seatbelt read-scoping posture (secret outside the allow
# list must be unreadable; `--allow` must make it readable).
#
# Usage:
#   macos-nfs-git-validation.sh [--vfs-bin PATH] [--report-dir DIR] [--keep-work]
#
# Environment:
#   VFS_BIN  vfs executable to invoke (default: vfs)
#   REPORT_DIR   directory where logs should be written
#   SKIP_CODE    exit code for unsupported platform/prerequisites (default: 77)
#
set -Eeuo pipefail

SKIP_CODE="${SKIP_CODE:-77}"
VFS_BIN="${VFS_BIN:-vfs}"
REPORT_DIR="${REPORT_DIR:-}"
KEEP_WORK=0

WORK_DIR=""
MOUNT_DIR=""
MOUNT_PID=""
VFS_RESOLVED=""
RUN_WORK_DIR=""
SECRET_DIR=""
RUN_SESSION_DENY=""
RUN_SESSION_ALLOW=""
RUN_SESSION_MISSING=""
RUN_SESSION_NOEXEC=""

usage() {
    sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
}

skip() {
    printf 'SKIP: %s\n' "$*" >&2
    exit "$SKIP_CODE"
}

resolve_vfs() {
    if [[ "$VFS_BIN" == */* ]]; then
        [[ -x "$VFS_BIN" ]] || return 1
        VFS_RESOLVED="$VFS_BIN"
    else
        VFS_RESOLVED="$(command -v "$VFS_BIN" 2>/dev/null)" || return 1
    fi
}

safe_rm_tmp() {
    local path="$1"
    [[ -n "$path" ]] || return 0
    case "$path" in
        /tmp/vfs-macos-nfs-git-work.*|/tmp/vfs-macos-nfs-git-mnt.*|/private/tmp/vfs-macos-nfs-git-work.*|/private/tmp/vfs-macos-nfs-git-mnt.*|/tmp/vfs-macos-read-scope-work.*|/private/tmp/vfs-macos-read-scope-work.*)
            rm -rf -- "$path"
            ;;
        *)
            printf 'Refusing to remove non-harness temp path: %s\n' "$path" >&2
            ;;
    esac
}

safe_rm_secret_dir() {
    local path="$1"
    [[ -n "$path" ]] || return 0
    case "$path" in
        */.vfs-macos-read-scope.*)
            rm -rf -- "$path"
            ;;
        *)
            printf 'Refusing to remove non-harness secret path: %s\n' "$path" >&2
            ;;
    esac
}

safe_rm_run_session() {
    local session="$1"
    [[ -n "$session" ]] || return 0
    case "$session" in
        macos-read-scope-*)
            rm -rf -- "${HOME:?}/.vfs/run/$session"
            ;;
        *)
            printf 'Refusing to remove non-harness run session: %s\n' "$session" >&2
            ;;
    esac
}

canonical_dir() {
    local path="$1"
    (cd "$path" && pwd -P)
}

is_mounted() {
    local dir="$1"
    mount | awk -v dir="$dir" 'index($0, " on " dir " ") { found = 1 } END { exit found ? 0 : 1 }'
}

unmount_dir() {
    local dir="$1"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        /sbin/umount "$dir" || /sbin/umount -f "$dir"
    else
        umount "$dir"
    fi
}

cleanup() {
    local status=$?
    set +e

    if [[ -n "$MOUNT_DIR" ]] && is_mounted "$MOUNT_DIR"; then
        if [[ -n "$REPORT_DIR" && -d "$REPORT_DIR" ]]; then
            unmount_dir "$MOUNT_DIR" >>"$REPORT_DIR/cleanup.log" 2>&1
        else
            unmount_dir "$MOUNT_DIR" >/dev/null 2>&1
        fi
    fi

    if [[ -n "$MOUNT_PID" ]]; then
        kill "$MOUNT_PID" >/dev/null 2>&1 || true
        wait "$MOUNT_PID" >/dev/null 2>&1 || true
    fi

    if [[ "$KEEP_WORK" -eq 0 ]]; then
        safe_rm_tmp "$WORK_DIR"
        safe_rm_tmp "$MOUNT_DIR"
        safe_rm_tmp "$RUN_WORK_DIR"
        safe_rm_secret_dir "$SECRET_DIR"
        safe_rm_run_session "$RUN_SESSION_DENY"
        safe_rm_run_session "$RUN_SESSION_ALLOW"
        safe_rm_run_session "$RUN_SESSION_MISSING"
        safe_rm_run_session "$RUN_SESSION_NOEXEC"
    elif [[ -n "$WORK_DIR" || -n "$MOUNT_DIR" ]]; then
        printf 'Kept work directory: %s\n' "$WORK_DIR" >&2
        printf 'Kept mount directory: %s\n' "$MOUNT_DIR" >&2
        [[ -n "$RUN_WORK_DIR" ]] && printf 'Kept run work directory: %s\n' "$RUN_WORK_DIR" >&2
        [[ -n "$SECRET_DIR" ]] && printf 'Kept secret directory: %s\n' "$SECRET_DIR" >&2
        [[ -n "$RUN_SESSION_DENY" ]] && printf 'Kept run session: %s\n' "$HOME/.vfs/run/$RUN_SESSION_DENY" >&2
        [[ -n "$RUN_SESSION_ALLOW" ]] && printf 'Kept run session: %s\n' "$HOME/.vfs/run/$RUN_SESSION_ALLOW" >&2
    fi

    exit "$status"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --vfs-bin)
            [[ $# -ge 2 ]] || { echo "missing value for --vfs-bin" >&2; exit 2; }
            VFS_BIN="$2"
            shift 2
            ;;
        --report-dir)
            [[ $# -ge 2 ]] || { echo "missing value for --report-dir" >&2; exit 2; }
            REPORT_DIR="$2"
            shift 2
            ;;
        --keep-work)
            KEEP_WORK=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [[ "$(uname -s)" != "Darwin" ]]; then
    skip "macOS NFS validation requires Darwin; got $(uname -s)"
fi

missing=()
resolve_vfs || missing+=("vfs")
command -v git >/dev/null 2>&1 || missing+=("git")
[[ -x /sbin/mount_nfs ]] || missing+=("/sbin/mount_nfs")
[[ -x /sbin/umount ]] || missing+=("/sbin/umount")
command -v mount >/dev/null 2>&1 || missing+=("mount")
command -v awk >/dev/null 2>&1 || missing+=("awk")
command -v find >/dev/null 2>&1 || missing+=("find")

if [[ ${#missing[@]} -gt 0 ]]; then
    skip "missing prerequisite(s): ${missing[*]}"
fi

if [[ -z "$REPORT_DIR" ]]; then
    REPORT_DIR="$(mktemp -d /tmp/vfs-macos-nfs-git-report.XXXXXX)"
else
    mkdir -p "$REPORT_DIR"
    REPORT_DIR="$(cd "$REPORT_DIR" && pwd)"
fi

WORK_DIR="$(canonical_dir "$(mktemp -d /tmp/vfs-macos-nfs-git-work.XXXXXX)")"
MOUNT_DIR="$(canonical_dir "$(mktemp -d /tmp/vfs-macos-nfs-git-mnt.XXXXXX)")"
trap cleanup EXIT INT TERM

AGENT_ID="macos-nfs-git-$$-$(date +%s)"
DB_PATH="$WORK_DIR/.vfs/$AGENT_ID.db"

printf 'Vfs binary: %s\n' "$VFS_RESOLVED"
printf 'Report directory: %s\n' "$REPORT_DIR"
printf 'Work directory: %s\n' "$WORK_DIR"
printf 'Mount directory: %s\n' "$MOUNT_DIR"

(
    cd "$WORK_DIR"
    "$VFS_RESOLVED" init "$AGENT_ID"
) >"$REPORT_DIR/init.log" 2>&1

if [[ ! -f "$DB_PATH" ]]; then
    printf 'FAILED: expected Vfs database was not created at %s\n' "$DB_PATH" >&2
    printf 'See %s/init.log\n' "$REPORT_DIR" >&2
    exit 1
fi

"$VFS_RESOLVED" mount --backend nfs "$DB_PATH" "$MOUNT_DIR" --foreground >"$REPORT_DIR/mount.log" 2>&1 &
MOUNT_PID=$!

mounted=0
for ((attempt = 0; attempt < 200; attempt++)); do
    if is_mounted "$MOUNT_DIR"; then
        mounted=1
        break
    fi
    if ! kill -0 "$MOUNT_PID" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

if [[ "$mounted" -ne 1 ]]; then
    if grep -Eqi 'operation not permitted|permission denied|not permitted|must be root|requires.*root' "$REPORT_DIR/mount.log"; then
        skip "mount_nfs is unavailable to this user; see $REPORT_DIR/mount.log"
    fi
    printf 'FAILED: Vfs NFS mount did not become ready at %s\n' "$MOUNT_DIR" >&2
    printf 'See %s/mount.log\n' "$REPORT_DIR" >&2
    exit 1
fi

set +e
(
    set -Eeuo pipefail
    cd "$MOUNT_DIR"
    git init
    git config user.name "Vfs macOS NFS validation"
    git config user.email "vfs-validation@example.invalid"
    printf 'hello from Vfs macOS NFS validation\n' >README.txt
    git add README.txt
    git commit -m "validate macos nfs git loose objects"
    git fsck --strict
    loose_count="$(find .git/objects -type f -path '.git/objects/[0-9a-f][0-9a-f]/*' | wc -l | tr -d '[:space:]')"
    if [[ "$loose_count" -lt 1 ]]; then
        printf 'expected at least one git loose object, found %s\n' "$loose_count" >&2
        exit 1
    fi
    printf 'Loose object count: %s\n' "$loose_count"
) >"$REPORT_DIR/git.log" 2>&1
git_status=$?
set -e

cat "$REPORT_DIR/git.log"

if [[ "$git_status" -ne 0 ]]; then
    printf 'FAILED: git validation failed with status %s. See %s/git.log\n' "$git_status" "$REPORT_DIR" >&2
    exit "$git_status"
fi

# --- vfs run read-scoping leg -----------------------------------------
# The darwin seatbelt profile is default-deny for reads: a secret outside the
# session/allow/platform roots must be unreadable, and `--allow` must make it
# readable (and writable, as before).

printf 'Running vfs run read-scoping checks...\n'

RUN_WORK_DIR="$(canonical_dir "$(mktemp -d /tmp/vfs-macos-read-scope-work.XXXXXX)")"
SECRET_DIR="$(canonical_dir "$(mktemp -d "$HOME/.vfs-macos-read-scope.XXXXXX")")"
printf 'vfs-read-scope-secret\n' >"$SECRET_DIR/secret.txt"

RUN_SESSION_DENY="macos-read-scope-deny-$$-$(date +%s)"
RUN_SESSION_ALLOW="macos-read-scope-allow-$$-$(date +%s)"

set +e
(
    cd "$RUN_WORK_DIR"
    "$VFS_RESOLVED" run --session "$RUN_SESSION_DENY" \
        /bin/cat "$SECRET_DIR/secret.txt"
) >"$REPORT_DIR/read-scope-deny.log" 2>&1
deny_status=$?
set -e

if ! grep -q 'Welcome to Vfs' "$REPORT_DIR/read-scope-deny.log"; then
    printf 'FAILED: vfs run never reached the sandbox (mount failure?). See %s/read-scope-deny.log\n' "$REPORT_DIR" >&2
    exit 1
fi
if [[ "$deny_status" -eq 0 ]]; then
    printf 'FAILED: read of %s/secret.txt outside the allow list unexpectedly succeeded. See %s/read-scope-deny.log\n' "$SECRET_DIR" "$REPORT_DIR" >&2
    exit 1
fi
if grep -q 'vfs-read-scope-secret' "$REPORT_DIR/read-scope-deny.log"; then
    printf 'FAILED: secret content leaked despite exit status %s. See %s/read-scope-deny.log\n' "$deny_status" "$REPORT_DIR" >&2
    exit 1
fi
if ! grep -Eqi 'operation not permitted|permission denied' "$REPORT_DIR/read-scope-deny.log"; then
    printf 'FAILED: expected a permission error from the sandboxed read; got exit %s without one. See %s/read-scope-deny.log\n' "$deny_status" "$REPORT_DIR" >&2
    exit 1
fi
printf 'Read of unallowed path denied as expected (exit %s).\n' "$deny_status"

set +e
(
    cd "$RUN_WORK_DIR"
    "$VFS_RESOLVED" run --session "$RUN_SESSION_ALLOW" --allow "$SECRET_DIR" \
        /bin/cat "$SECRET_DIR/secret.txt"
) >"$REPORT_DIR/read-scope-allow.log" 2>&1
allow_status=$?
set -e

if [[ "$allow_status" -ne 0 ]] || ! grep -q 'vfs-read-scope-secret' "$REPORT_DIR/read-scope-allow.log"; then
    printf 'FAILED: --allow %s did not make the secret readable (exit %s). See %s/read-scope-allow.log\n' "$SECRET_DIR" "$allow_status" "$REPORT_DIR" >&2
    exit 1
fi
printf 'Read of --allow path succeeded as expected.\n'

# --- seatbelt profile paths with spaces and quotes --------------------------
# Dynamic profile paths travel as Seatbelt `(param "NAME")` references with
# -D NAME=value definitions on the sandbox-exec command line. Homing the
# session store and the base directory under names containing spaces and a
# double quote exercises every parameterized path; a regression back to
# string interpolation fails to mount or run.

printf 'Running seatbelt quoted-path checks...\n'

QUOTE_HOME="$RUN_WORK_DIR/home with \"quote\""
QUOTE_BASE="$RUN_WORK_DIR/base with \"quote\" dir"
mkdir -p "$QUOTE_HOME" "$QUOTE_BASE"
RUN_SESSION_QUOTES="macos-read-scope-quotes-$$-$(date +%s)"

set +e
(
    cd "$QUOTE_BASE"
    HOME="$QUOTE_HOME" \
    XDG_CACHE_HOME="$QUOTE_HOME/.cache" \
    XDG_CONFIG_HOME="$QUOTE_HOME/.config" \
        "$VFS_RESOLVED" run --session "$RUN_SESSION_QUOTES" \
        /bin/sh -c 'printf quoted-path-ok >probe.txt && /bin/cat probe.txt'
) >"$REPORT_DIR/quoted-paths.log" 2>&1
quotes_status=$?
set -e

if ! grep -q 'Welcome to Vfs' "$REPORT_DIR/quoted-paths.log"; then
    printf 'FAILED: vfs run never reached the sandbox under a quoted path. See %s/quoted-paths.log\n' "$REPORT_DIR" >&2
    exit 1
fi
if [[ "$quotes_status" -ne 0 ]] || ! grep -q 'quoted-path-ok' "$REPORT_DIR/quoted-paths.log"; then
    printf 'FAILED: session under a directory with spaces/quotes did not mount and run (exit %s). See %s/quoted-paths.log\n' "$quotes_status" "$REPORT_DIR" >&2
    exit 1
fi
printf 'Session under spaced/quoted directories mounted and ran.\n'

# --- run exit-status parity --------------------------------------------------
# The reserved startup exit statuses are a daemon contract: 127 for a missing
# command and 126 for a present but non-executable one, matching vfs exec and
# the Linux run path.

printf 'Running exit-status parity checks...\n'

RUN_SESSION_MISSING="macos-read-scope-missing-$$-$(date +%s)"
RUN_SESSION_NOEXEC="macos-read-scope-noexec-$$-$(date +%s)"

set +e
(
    cd "$RUN_WORK_DIR"
    "$VFS_RESOLVED" run --session "$RUN_SESSION_MISSING" \
        /nonexistent-vfs-validation-command
) >"$REPORT_DIR/exit-missing.log" 2>&1
missing_status=$?
set -e

if [[ "$missing_status" -ne 127 ]]; then
    printf 'FAILED: run of a missing command exited %s, expected 127. See %s/exit-missing.log\n' "$missing_status" "$REPORT_DIR" >&2
    exit 1
fi

printf '#!/bin/sh\n' >"$RUN_WORK_DIR/not-executable"
chmod 0644 "$RUN_WORK_DIR/not-executable"

set +e
(
    cd "$RUN_WORK_DIR"
    "$VFS_RESOLVED" run --session "$RUN_SESSION_NOEXEC" \
        "$RUN_WORK_DIR/not-executable"
) >"$REPORT_DIR/exit-noexec.log" 2>&1
noexec_status=$?
set -e

if [[ "$noexec_status" -ne 126 ]]; then
    printf 'FAILED: run of a non-executable file exited %s, expected 126. See %s/exit-noexec.log\n' "$noexec_status" "$REPORT_DIR" >&2
    exit 1
fi
printf 'Exit-status parity held (127 missing, 126 non-executable).\n'

printf 'macOS NFS git + run read-scoping validation passed. Logs: %s\n' "$REPORT_DIR"
