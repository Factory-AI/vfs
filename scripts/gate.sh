#!/usr/bin/env bash
#
# Honest local milestone gate.
#
# This is the single developer and CI entrypoint for the M1 gate. It fails on
# every command failure, runs the shell suite in strict mode so SKIP is red on
# the designated runner, and keeps the codex benchmark out of CI.
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

AGENTFS_BIN="${AGENTFS_BIN:-$REPO_ROOT/target/release/agentfs}"
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-nightly}"
SHELL_TIMEOUT="${AGENTFS_GATE_SHELL_TIMEOUT:-900}"
PHASE8_TIMEOUT="${AGENTFS_GATE_PHASE8_TIMEOUT:-20}"

run() {
    printf '\n==> %s\n' "$*"
    "$@"
}

run_cargo() {
    printf '\n==> cargo +%s %s\n' "$RUST_TOOLCHAIN" "$*"
    cargo "+$RUST_TOOLCHAIN" "$@"
}

run_cargo fmt --all -- --check
run_cargo clippy --workspace --all-targets -- -D warnings
run_cargo test --workspace
run_cargo build --release --workspace --bins

printf '\n==> cli/tests/all.sh\n'
AGENTFS_GATE_STRICT=1 \
CORRUPTION_TORTURE_WORKERS="${CORRUPTION_TORTURE_WORKERS:-4}" \
CORRUPTION_TORTURE_ITERATIONS="${CORRUPTION_TORTURE_ITERATIONS:-3}" \
CORRUPTION_TORTURE_TIMEOUT="${CORRUPTION_TORTURE_TIMEOUT:-120}" \
CORRUPTION_TORTURE_TEARDOWN_TIMEOUT="${CORRUPTION_TORTURE_TEARDOWN_TIMEOUT:-10}" \
timeout "$SHELL_TIMEOUT" cli/tests/all.sh

run python3 scripts/validation/phase8-validation.py \
    --smoke \
    --timeout "$PHASE8_TIMEOUT" \
    --agentfs-bin "$AGENTFS_BIN" \
    --output /tmp/vfs-val/phase8.json
run python3 scripts/validation/noopen-coherence.py --agentfs-bin "$AGENTFS_BIN"
run python3 scripts/validation/flush-coherence.py --agentfs-bin "$AGENTFS_BIN"

printf '\nHonest gate passed with AGENTFS_BIN=%s\n' "$AGENTFS_BIN"
