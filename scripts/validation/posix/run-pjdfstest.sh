#!/usr/bin/env bash
#
# Run pjdfstest against an AgentFS FUSE mount.
#
# Usage:
#   run-pjdfstest.sh [--pjdfstest-dir DIR] [--agentfs-bin PATH] [--profile NAME]
#                    [--manifest FILE] [--report-dir DIR] [--partial-origin]
#                    [--no-partial-origin] [--keep-work]
#
# Environment:
#   PJDFSTEST_DIR  pjdfstest checkout root or tests directory.
#   AGENTFS_BIN    agentfs executable to invoke (default: agentfs).
#   PJDFSTEST_PROFILE   test profile to run (default: full).
#   PJDFSTEST_MANIFEST  explicit manifest overriding --profile.
#   REPORT_DIR     directory where logs should be written.
#   SKIP_CODE      exit code for missing prerequisites (default: 77).
#
set -Eeuo pipefail

SKIP_CODE="${SKIP_CODE:-77}"
AGENTFS_BIN="${AGENTFS_BIN:-agentfs}"
PJDFSTEST_DIR="${PJDFSTEST_DIR:-}"
PJDFSTEST_PROFILE="${PJDFSTEST_PROFILE:-full}"
PJDFSTEST_MANIFEST="${PJDFSTEST_MANIFEST:-}"
PJDFSTEST_KNOWN_UNSUPPORTED="${PJDFSTEST_KNOWN_UNSUPPORTED:-}"
PARTIAL_ORIGIN=""
REPORT_DIR="${REPORT_DIR:-}"
KEEP_WORK=0

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

WORK_DIR=""
MOUNT_DIR=""
MOUNT_PID=""
AGENTFS_RESOLVED=""
PJDFSTEST_RESOLVED=""
PJDFSTEST_TESTS=""
PJDFSTEST_RESOLVED_MANIFEST=""
PROVE_TARGETS=()

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

print_testing_guidance() {
    cat >&2 <<'EOF'

Relevant setup guidance from TESTING.md:

## pjdfstest

```bash
git clone https://github.com/pjd/pjdfstest.git
cd pjdfstest
autoreconf -ifs
./configure --prefix="$HOME/.local"
make pjdfstest
install -m 0755 pjdfstest "$HOME/.local/bin/pjdfstest"
command -v prove
command -v pjdfstest
```

AgentFS harness command from TESTING.md:

```bash
scripts/validation/posix/run-pjdfstest.sh \
  --agentfs-bin "$PWD/cli/target/debug/agentfs" \
  --pjdfstest-dir /path/to/pjdfstest \
  --profile phase45-ci
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

resolve_pjdfstest_binary() {
    if PJDFSTEST_RESOLVED="$(command -v pjdfstest 2>/dev/null)"; then
        return 0
    fi

    local checkout_bin
    checkout_bin="$(cd "$PJDFSTEST_TESTS/.." && pwd)/pjdfstest"
    if [[ -x "$checkout_bin" ]]; then
        PJDFSTEST_RESOLVED="$checkout_bin"
        export PATH="$(dirname "$checkout_bin"):$PATH"
        return 0
    fi

    return 1
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

trim_line() {
    local value="$1"
    value="${value%%#*}"
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
    printf '%s' "$value"
}

manifest_for_profile() {
    case "$PJDFSTEST_PROFILE" in
        full)
            printf ''
            ;;
        phase45-ci)
            printf '%s\n' "$SCRIPT_DIR/pjdfstest/phase45-ci.txt"
            ;;
        *)
            printf '%s\n' "$SCRIPT_DIR/pjdfstest/$PJDFSTEST_PROFILE.txt"
            ;;
    esac
}

list_profiles() {
    cat <<EOF
full
phase45-ci
phase5-ci
EOF
}

env_flag_enabled() {
    case "${1,,}" in
        1|true|yes|on)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

resolve_prove_targets() {
    local manifest="${PJDFSTEST_MANIFEST:-}"
    local entry target
    declare -A seen=()

    if [[ -z "$manifest" ]]; then
        manifest="$(manifest_for_profile)"
    fi

    if [[ -z "$manifest" ]]; then
        PROVE_TARGETS=("$PJDFSTEST_TESTS")
        PJDFSTEST_RESOLVED_MANIFEST=""
        return 0
    fi

    if [[ ! -f "$manifest" ]]; then
        printf 'pjdfstest manifest not found for profile %s: %s\n' "$PJDFSTEST_PROFILE" "$manifest" >&2
        exit 2
    fi
    PJDFSTEST_RESOLVED_MANIFEST="$(cd "$(dirname "$manifest")" && pwd)/$(basename "$manifest")"

    while IFS= read -r line || [[ -n "$line" ]]; do
        entry="$(trim_line "$line")"
        [[ -n "$entry" ]] || continue

        case "$entry" in
            /*|*..*|-*)
                printf 'invalid pjdfstest manifest entry: %s\n' "$entry" >&2
                exit 2
                ;;
        esac

        target="$PJDFSTEST_TESTS/$entry"
        if [[ ! -f "$target" && ! -d "$target" ]]; then
            printf 'pjdfstest manifest entry not found: %s\n' "$entry" >&2
            exit 2
        fi
        if [[ -z "${seen[$target]:-}" ]]; then
            PROVE_TARGETS+=("$target")
            seen["$target"]=1
        fi
    done <"$manifest"

    if [[ ${#PROVE_TARGETS[@]} -eq 0 ]]; then
        printf 'pjdfstest manifest selected no tests: %s\n' "$manifest" >&2
        exit 2
    fi
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
        --profile)
            [[ $# -ge 2 ]] || { echo "missing value for --profile" >&2; exit 2; }
            PJDFSTEST_PROFILE="$2"
            shift 2
            ;;
        --manifest)
            [[ $# -ge 2 ]] || { echo "missing value for --manifest" >&2; exit 2; }
            PJDFSTEST_MANIFEST="$2"
            shift 2
            ;;
        --known-unsupported)
            [[ $# -ge 2 ]] || { echo "missing value for --known-unsupported" >&2; exit 2; }
            PJDFSTEST_KNOWN_UNSUPPORTED="$2"
            shift 2
            ;;
        --partial-origin)
            PARTIAL_ORIGIN=1
            shift
            ;;
        --no-partial-origin)
            PARTIAL_ORIGIN=
            shift
            ;;
        --list-profiles)
            list_profiles
            exit 0
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
resolve_agentfs || missing+=("agentfs")
resolve_pjdfstest_tests || missing+=("pjdfstest tests")
resolve_pjdfstest_binary || missing+=("pjdfstest executable")

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

resolve_prove_targets

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
printf 'pjdfstest binary: %s\n' "$PJDFSTEST_RESOLVED"
printf 'pjdfstest tests: %s\n' "$PJDFSTEST_TESTS"
printf 'pjdfstest profile: %s\n' "$PJDFSTEST_PROFILE"
if env_flag_enabled "$PARTIAL_ORIGIN"; then
    printf 'partial-origin overlay: enabled via --partial-origin on\n'
else
    printf 'partial-origin overlay: disabled\n'
fi
printf 'Report directory: %s\n' "$REPORT_DIR"

printf '%s\n' "$PJDFSTEST_PROFILE" >"$REPORT_DIR/selected-profile.txt"
printf '%s\n' "$(env_flag_enabled "$PARTIAL_ORIGIN" && printf 'on' || printf 'off')" >"$REPORT_DIR/partial-origin-cli.txt"
if [[ -n "$PJDFSTEST_RESOLVED_MANIFEST" ]]; then
    {
        printf 'path\t%s\n' "$PJDFSTEST_RESOLVED_MANIFEST"
        if command -v sha256sum >/dev/null 2>&1; then
            printf 'sha256\t%s\n' "$(sha256sum "$PJDFSTEST_RESOLVED_MANIFEST" | awk '{print $1}')"
        fi
    } >"$REPORT_DIR/selected-manifest.tsv"
fi
for target in "${PROVE_TARGETS[@]}"; do
    if [[ "$target" == "$PJDFSTEST_TESTS" ]]; then
        printf '.\n'
    else
        printf '%s\n' "${target#"$PJDFSTEST_TESTS"/}"
    fi
done >"$REPORT_DIR/selected-tests.txt"

if [[ -z "$PJDFSTEST_KNOWN_UNSUPPORTED" ]]; then
    PJDFSTEST_KNOWN_UNSUPPORTED="$SCRIPT_DIR/pjdfstest/known-gaps.tsv"
fi
if [[ -f "$PJDFSTEST_KNOWN_UNSUPPORTED" ]]; then
    cp "$PJDFSTEST_KNOWN_UNSUPPORTED" "$REPORT_DIR/known-unsupported.tsv"
fi

(
    cd "$WORK_DIR"
    "$AGENTFS_RESOLVED" init "$AGENT_ID"
) >"$REPORT_DIR/init.log" 2>&1

if [[ ! -f "$DB_PATH" ]]; then
    printf 'FAILED: expected AgentFS database was not created at %s\n' "$DB_PATH" >&2
    printf 'See %s/init.log\n' "$REPORT_DIR" >&2
    exit 1
fi

MOUNT_CMD=("$AGENTFS_RESOLVED" mount "$DB_PATH" "$MOUNT_DIR" --foreground)
if env_flag_enabled "$PARTIAL_ORIGIN"; then
    MOUNT_CMD+=(--partial-origin on)
fi
"${MOUNT_CMD[@]}" >"$REPORT_DIR/mount.log" 2>&1 &
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
    prove -rv "${PROVE_TARGETS[@]}"
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
