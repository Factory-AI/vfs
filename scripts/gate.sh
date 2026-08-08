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

VFS_BIN="${VFS_BIN:-$REPO_ROOT/target/release/vfs}"
# Default to the channel rust-toolchain.toml pins. A bare `+nightly` overrides
# that pin, so the gate would silently lint against a different compiler than
# CI and than every plain `cargo` invocation in the tree.
PINNED_TOOLCHAIN="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-${PINNED_TOOLCHAIN:-nightly}}"
SHELL_TIMEOUT="${VFS_GATE_SHELL_TIMEOUT:-900}"
PHASE8_TIMEOUT="${VFS_GATE_PHASE8_TIMEOUT:-20}"

# Pin TMPDIR to a per-run scratch dir cleaned on exit: turso_core 0.5.3 leaks
# /tmp/tursodb-ephemeral-* sort-spill files (vdbe/execute.rs:10096 never
# unlinks them), so dependency litter must not accumulate on the host.
GATE_TMPDIR="$(mktemp -d "${TMPDIR:-/tmp}/vfs-gate.XXXXXX")"
trap 'rm -rf "$GATE_TMPDIR"' EXIT
export TMPDIR="$GATE_TMPDIR" TMP="$GATE_TMPDIR" TEMP="$GATE_TMPDIR"
export PYTHONDONTWRITEBYTECODE=1

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

printf '\n==> crates/vfs-cli/tests/all.sh\n'
VFS_GATE_STRICT=1 \
VFS_GATE_ALLOWED_SKIPS="${VFS_GATE_ALLOWED_SKIPS:-}" \
VFS_BIN="$VFS_BIN" \
CORRUPTION_TORTURE_WORKERS="${CORRUPTION_TORTURE_WORKERS:-4}" \
CORRUPTION_TORTURE_ITERATIONS="${CORRUPTION_TORTURE_ITERATIONS:-3}" \
CORRUPTION_TORTURE_TIMEOUT="${CORRUPTION_TORTURE_TIMEOUT:-120}" \
CORRUPTION_TORTURE_TEARDOWN_TIMEOUT="${CORRUPTION_TORTURE_TEARDOWN_TIMEOUT:-10}" \
timeout "$SHELL_TIMEOUT" crates/vfs-cli/tests/all.sh

# Phase 8 smoke is the top-level python gate; the noopen/flush/base-drift
# coherence harnesses run inside it (M7 scripts consolidation).
run python3 scripts/validation/phase8-validation.py \
    --smoke \
    --timeout "$PHASE8_TIMEOUT" \
    --vfs-bin "$VFS_BIN" \
    --output /tmp/vfs-val/phase8.json

run scripts/validation/consistency-canon.sh

printf '\nHonest gate passed with VFS_BIN=%s\n' "$VFS_BIN"
