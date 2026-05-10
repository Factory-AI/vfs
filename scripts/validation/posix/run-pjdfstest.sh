#!/usr/bin/env bash
#
# Run pjdfstest against an AgentFS FUSE mount.
#
# Usage:
#   run-pjdfstest.sh [--pjdfstest-dir DIR] [--agentfs-bin PATH] [--report-dir DIR] [--keep-work]
#
# Environment:
#   PJDFSTEST_DIR  pjdfstest checkout root or tests directory.
#   AGENTFS_BIN    agentfs executable to invoke (default: agentfs).
#   REPORT_DIR     directory where logs should be written.
#   SKIP_CODE      exit code for missing prerequisites (default: 77).
#
set -Eeuo pipefail

SKIP_CODE="${SKIP_CODE:-77}"
AGENTFS_BIN="${AGENTFS_BIN:-agentfs}"
PJDFSTEST_DIR="${PJDFSTEST_DIR:-}"
REPORT_DIR="${REPORT_DIR:-}"
KEEP_WORK=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

WORK_DIR=""
MOUNT_DIR=""
MOUNT_PID=""
AGENTFS_RESOLVED=""
PJDFSTEST_TESTS=""

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

print_testing_guidance() {
    cat >&2 <<'EOF'

Relevant setup guidance from TESTING.md:

## pjdfstest

```bash
git clone git@github.com:pjd/pjdfstest.git
cd pjdfstest
autoreconf -ifs
./configure
make pjdfstest
sudo make install
sudo dnf install perl-Test-Harness
mkdir -p ../agentfs-testing
cd ../agentfs-testing
agentfs init testing
mkdir mnt
sudo su
agentfs mount testing ./mnt
cd mnt
prove -rv ../../pjdfstest/tests/ 2>&1 | tee /tmp/pjdfstest.log
```

AgentFS executable setup from TESTING.md:

```bash
cd cli
cargo build --release
cp target/release/agentfs /usr/local/bin
cp scripts/mount.fuse.agentfs /sbin
```
EOF
}

skip_missing() {
    printf 'SKIP: missing prerequisite(s): %s\n' "$*" >&2
    print_testing_guidance
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

resolve_pjdfstest_tests() {
    local candidate
    local candidates=()

    if [[ -n "$PJDFSTEST_DIR" ]]; then
        candidates+=("$PJDFSTEST_DIR")
    else
        candidates+=(
            "$PWD/pjdfstest"
            "$PWD/../pjdfstest"
            "$REPO_ROOT/pjdfstest"
            "$REPO_ROOT/../pjdfstest"
        )
    fi

    for candidate in "${candidates[@]}"; do
        if [[ -d "$candidate/tests" ]]; then
            PJDFSTEST_TESTS="$(cd "$candidate/tests" && pwd)"
            return 0
        fi
        if [[ -d "$candidate" && "$(basename "$candidate")" == "tests" ]]; then
            PJDFSTEST_TESTS="$(cd "$candidate" && pwd)"
            return 0
        fi
    done

    return 1
}

safe_rm_tmp() {
    local path="$1"
    [[ -n "$path" ]] || return 0
    case "$path" in
        /tmp/agentfs-pjdfstest-work.*|/tmp/agentfs-pjdfstest-mnt.*)
            rm -rf -- "$path"
            ;;
        *)
            printf 'Refusing to remove non-harness temp path: %s\n' "$path" >&2
            ;;
    esac
}

unmount_dir() {
    local dir="$1"
    if command -v fusermount3 >/dev/null 2>&1; then
        fusermount3 -u "$dir"
    elif command -v fusermount >/dev/null 2>&1; then
        fusermount -u "$dir"
    else
        umount "$dir"
    fi
}

cleanup() {
    local status=$?
    set +e

    if [[ -n "$MOUNT_DIR" ]] && command -v mountpoint >/dev/null 2>&1 && mountpoint -q "$MOUNT_DIR"; then
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
        --pjdfstest-dir)
            [[ $# -ge 2 ]] || { echo "missing value for --pjdfstest-dir" >&2; exit 2; }
            PJDFSTEST_DIR="$2"
            shift 2
            ;;
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

missing=()
command -v prove >/dev/null 2>&1 || missing+=("prove (perl-Test-Harness)")
command -v pjdfstest >/dev/null 2>&1 || missing+=("pjdfstest executable")
resolve_agentfs || missing+=("agentfs")
resolve_pjdfstest_tests || missing+=("pjdfstest tests")

if [[ ${#missing[@]} -gt 0 ]]; then
    skip_missing "${missing[*]}"
fi

if ! command -v mountpoint >/dev/null 2>&1; then
    skip_missing "mountpoint"
fi

if ! command -v fusermount3 >/dev/null 2>&1 && ! command -v fusermount >/dev/null 2>&1 && ! command -v umount >/dev/null 2>&1; then
    skip_missing "fusermount3/fusermount/umount"
fi

if [[ "$(uname -s)" == "Linux" && ! -e /dev/fuse ]]; then
    skip_missing "/dev/fuse"
fi

if [[ -z "$REPORT_DIR" ]]; then
    REPORT_DIR="$(mktemp -d /tmp/agentfs-pjdfstest-report.XXXXXX)"
else
    mkdir -p "$REPORT_DIR"
    REPORT_DIR="$(cd "$REPORT_DIR" && pwd)"
fi

WORK_DIR="$(mktemp -d /tmp/agentfs-pjdfstest-work.XXXXXX)"
MOUNT_DIR="$(mktemp -d /tmp/agentfs-pjdfstest-mnt.XXXXXX)"
trap cleanup EXIT INT TERM

AGENT_ID="pjdfstest-$$-$(date +%s)"
DB_PATH="$WORK_DIR/.agentfs/$AGENT_ID.db"

printf 'AgentFS binary: %s\n' "$AGENTFS_RESOLVED"
printf 'pjdfstest tests: %s\n' "$PJDFSTEST_TESTS"
printf 'Report directory: %s\n' "$REPORT_DIR"

(
    cd "$WORK_DIR"
    "$AGENTFS_RESOLVED" init "$AGENT_ID"
) >"$REPORT_DIR/init.log" 2>&1

if [[ ! -f "$DB_PATH" ]]; then
    printf 'FAILED: expected AgentFS database was not created at %s\n' "$DB_PATH" >&2
    printf 'See %s/init.log\n' "$REPORT_DIR" >&2
    exit 1
fi

"$AGENTFS_RESOLVED" mount "$DB_PATH" "$MOUNT_DIR" --foreground >"$REPORT_DIR/mount.log" 2>&1 &
MOUNT_PID=$!

mounted=0
for _ in $(seq 1 100); do
    if mountpoint -q "$MOUNT_DIR"; then
        mounted=1
        break
    fi
    if ! kill -0 "$MOUNT_PID" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

if [[ "$mounted" -ne 1 ]]; then
    printf 'FAILED: AgentFS mount did not become ready at %s\n' "$MOUNT_DIR" >&2
    printf 'See %s/mount.log\n' "$REPORT_DIR" >&2
    exit 1
fi

set +e
(
    cd "$MOUNT_DIR"
    prove -rv "$PJDFSTEST_TESTS"
) 2>&1 | tee "$REPORT_DIR/pjdfstest.log"
prove_status=${PIPESTATUS[0]}
set -e

printf '%s\n' "$prove_status" >"$REPORT_DIR/status.txt"

if [[ "$prove_status" -eq 0 ]]; then
    printf 'pjdfstest completed successfully. Logs: %s\n' "$REPORT_DIR"
else
    printf 'pjdfstest failed with status %s. Logs: %s\n' "$prove_status" "$REPORT_DIR" >&2
fi

exit "$prove_status"
