#!/usr/bin/env bash
#
# Validate the macOS NFS path for git loose-object writes (#333).
#
# Usage:
#   macos-nfs-git-validation.sh [--agentfs-bin PATH] [--report-dir DIR] [--keep-work]
#
# Environment:
#   AGENTFS_BIN  agentfs executable to invoke (default: agentfs)
#   REPORT_DIR   directory where logs should be written
#   SKIP_CODE    exit code for unsupported platform/prerequisites (default: 77)
#
set -Eeuo pipefail

SKIP_CODE="${SKIP_CODE:-77}"
AGENTFS_BIN="${AGENTFS_BIN:-agentfs}"
REPORT_DIR="${REPORT_DIR:-}"
KEEP_WORK=0

WORK_DIR=""
MOUNT_DIR=""
MOUNT_PID=""
AGENTFS_RESOLVED=""

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
}

skip() {
    printf 'SKIP: %s\n' "$*" >&2
    exit "$SKIP_CODE"
}

resolve_agentfs() {
    if [[ "$AGENTFS_BIN" == */* ]]; then
        [[ -x "$AGENTFS_BIN" ]] || return 1
        AGENTFS_RESOLVED="$AGENTFS_BIN"
    else
        AGENTFS_RESOLVED="$(command -v "$AGENTFS_BIN" 2>/dev/null)" || return 1
    fi
}

safe_rm_tmp() {
    local path="$1"
    [[ -n "$path" ]] || return 0
    case "$path" in
        /tmp/agentfs-macos-nfs-git-work.*|/tmp/agentfs-macos-nfs-git-mnt.*)
            rm -rf -- "$path"
            ;;
        *)
            printf 'Refusing to remove non-harness temp path: %s\n' "$path" >&2
            ;;
    esac
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
    elif [[ -n "$WORK_DIR" || -n "$MOUNT_DIR" ]]; then
        printf 'Kept work directory: %s\n' "$WORK_DIR" >&2
        printf 'Kept mount directory: %s\n' "$MOUNT_DIR" >&2
    fi

    exit "$status"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --agentfs-bin)
            [[ $# -ge 2 ]] || { echo "missing value for --agentfs-bin" >&2; exit 2; }
            AGENTFS_BIN="$2"
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
resolve_agentfs || missing+=("agentfs")
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
    REPORT_DIR="$(mktemp -d /tmp/agentfs-macos-nfs-git-report.XXXXXX)"
else
    mkdir -p "$REPORT_DIR"
    REPORT_DIR="$(cd "$REPORT_DIR" && pwd)"
fi

WORK_DIR="$(mktemp -d /tmp/agentfs-macos-nfs-git-work.XXXXXX)"
MOUNT_DIR="$(mktemp -d /tmp/agentfs-macos-nfs-git-mnt.XXXXXX)"
trap cleanup EXIT INT TERM

AGENT_ID="macos-nfs-git-$$-$(date +%s)"
DB_PATH="$WORK_DIR/.agentfs/$AGENT_ID.db"

printf 'AgentFS binary: %s\n' "$AGENTFS_RESOLVED"
printf 'Report directory: %s\n' "$REPORT_DIR"
printf 'Work directory: %s\n' "$WORK_DIR"
printf 'Mount directory: %s\n' "$MOUNT_DIR"

(
    cd "$WORK_DIR"
    "$AGENTFS_RESOLVED" init "$AGENT_ID"
) >"$REPORT_DIR/init.log" 2>&1

if [[ ! -f "$DB_PATH" ]]; then
    printf 'FAILED: expected AgentFS database was not created at %s\n' "$DB_PATH" >&2
    printf 'See %s/init.log\n' "$REPORT_DIR" >&2
    exit 1
fi

"$AGENTFS_RESOLVED" mount --backend nfs "$DB_PATH" "$MOUNT_DIR" --foreground >"$REPORT_DIR/mount.log" 2>&1 &
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
    printf 'FAILED: AgentFS NFS mount did not become ready at %s\n' "$MOUNT_DIR" >&2
    printf 'See %s/mount.log\n' "$REPORT_DIR" >&2
    exit 1
fi

set +e
(
    set -Eeuo pipefail
    cd "$MOUNT_DIR"
    git init
    git config user.name "AgentFS macOS NFS validation"
    git config user.email "agentfs-validation@example.invalid"
    printf 'hello from AgentFS macOS NFS validation\n' >README.txt
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

printf 'macOS NFS git validation passed. Logs: %s\n' "$REPORT_DIR"
