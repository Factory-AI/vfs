#!/bin/sh
set -e

echo -n "TEST profile summary on CLI error... "

output_file="$(mktemp "${TMPDIR:-/tmp}/agentfs-profile-error.XXXXXX")"
trap 'rm -f "$output_file"' EXIT

assert_one_summary_on_failure() {
    description="$1"
    shift

    : >"$output_file"
    set +e
    AGENTFS_PROFILE=1 "$@" >"$output_file" 2>&1
    status=$?
    set -e

    if [ "$status" -eq 0 ]; then
        echo "FAILED: $description should fail"
        cat "$output_file"
        exit 1
    fi

    summary_count="$(grep -c '"event":"agentfs_profile_summary"' "$output_file" || true)"
    if [ "$summary_count" -ne 1 ]; then
        echo "FAILED: expected exactly one profile summary for $description, saw $summary_count"
        cat "$output_file"
        exit 1
    fi
}

assert_one_summary_on_failure \
    "invalid clap arguments" \
    cargo run --quiet -- --definitely-not-an-agentfs-option

assert_one_summary_on_failure \
    "invalid encryption options" \
    cargo run --quiet -- fs --key deadbeef /tmp/agentfs-profile-error.db ls /

set +e
AGENTFS_PROFILE=1 cargo run --quiet -- completions show >"$output_file" 2>&1
status=$?
set -e

if [ "$status" -ne 0 ]; then
    echo "FAILED: completions show should succeed"
    cat "$output_file"
    exit 1
fi

summary_count="$(grep -c '"event":"agentfs_profile_summary"' "$output_file" || true)"
if [ "$summary_count" -ne 1 ]; then
    echo "FAILED: expected exactly one profile summary on success, saw $summary_count"
    cat "$output_file"
    exit 1
fi

echo "OK"
