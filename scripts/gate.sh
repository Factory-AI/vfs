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

# Phases exist so CI can spread one gate across runners without CI growing its
# own idea of what the gate is. Running no --phases is still the whole gate,
# which is what a developer gets and what the contract means locally.
ALL_PHASES="cargo shell python canon"
PHASES="$ALL_PHASES"
while [ $# -gt 0 ]; do
    case "$1" in
        --phases)
            PHASES="$(printf '%s' "${2:?--phases needs a value}" | tr ',' ' ')"
            shift 2
            ;;
        --phases=*)
            PHASES="$(printf '%s' "${1#--phases=}" | tr ',' ' ')"
            shift
            ;;
        -h | --help)
            printf 'usage: gate.sh [--phases %s]\n' "$(printf '%s' "$ALL_PHASES" | tr ' ' ',')"
            exit 0
            ;;
        *)
            printf 'gate.sh: unknown argument %s\n' "$1" >&2
            exit 2
            ;;
    esac
done
for phase in $PHASES; do
    case " $ALL_PHASES " in
        *" $phase "*) ;;
        *)
            printf 'gate.sh: unknown phase %s (known: %s)\n' "$phase" "$ALL_PHASES" >&2
            exit 2
            ;;
    esac
done

want() {
    case " $PHASES " in
        *" $1 "*) return 0 ;;
        *) return 1 ;;
    esac
}

VFS_BIN="${VFS_BIN:-$REPO_ROOT/target/release/vfs}"
# Default to the channel rust-toolchain.toml pins. A bare `+nightly` overrides
# that pin, so the gate would silently lint against a different compiler than
# CI and than every plain `cargo` invocation in the tree.
PINNED_TOOLCHAIN="$(sed -n 's/^channel = "\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"
RUST_TOOLCHAIN="${RUST_TOOLCHAIN:-${PINNED_TOOLCHAIN:-nightly}}"
SHELL_TIMEOUT="${VFS_GATE_SHELL_TIMEOUT:-900}"
PHASE8_TIMEOUT="${VFS_GATE_PHASE8_TIMEOUT:-20}"

# Pin TMPDIR to a per-run scratch dir cleaned on exit: gate legs and their
# dependencies write temp state, and any litter one leg leaves (or a
# dependency regression starts leaving) must die with the run instead of
# accumulating on the host.
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

if want cargo; then
    run_cargo fmt --all -- --check
    run_cargo clippy --workspace --all-targets -- -D warnings
    run_cargo test --workspace
    # macOS reaches the default temp dir through the /private symlink, so a
    # test that canonicalizes one side of a path comparison passes on Linux
    # and fails only on macOS CI. Re-running the unit tests with TMPDIR
    # routed through a symlink surfaces that class here.
    mkdir -p "$GATE_TMPDIR/real-tmp"
    ln -sfn real-tmp "$GATE_TMPDIR/link-tmp"
    TMPDIR="$GATE_TMPDIR/link-tmp" TMP="$GATE_TMPDIR/link-tmp" TEMP="$GATE_TMPDIR/link-tmp" \
        run_cargo test --workspace --lib
    run_cargo build --release --workspace --bins
fi

if [ ! -x "$VFS_BIN" ]; then
    printf 'gate.sh: no vfs binary at %s (build it, or run the cargo phase)\n' "$VFS_BIN" >&2
    exit 1
fi

if want shell; then
    printf '\n==> crates/vfs-cli/tests/all.sh\n'
    VFS_GATE_STRICT=1 \
    VFS_GATE_ALLOWED_SKIPS="${VFS_GATE_ALLOWED_SKIPS:-}" \
    VFS_GATE_SHARD="${VFS_GATE_SHARD:-}" \
    VFS_BIN="$VFS_BIN" \
    CORRUPTION_TORTURE_WORKERS="${CORRUPTION_TORTURE_WORKERS:-4}" \
    CORRUPTION_TORTURE_ITERATIONS="${CORRUPTION_TORTURE_ITERATIONS:-3}" \
    CORRUPTION_TORTURE_TIMEOUT="${CORRUPTION_TORTURE_TIMEOUT:-120}" \
    CORRUPTION_TORTURE_TEARDOWN_TIMEOUT="${CORRUPTION_TORTURE_TEARDOWN_TIMEOUT:-10}" \
    timeout "$SHELL_TIMEOUT" crates/vfs-cli/tests/all.sh
fi

# Phase 8 smoke is the top-level python gate; the noopen/flush/base-drift
# coherence harnesses run inside it (M7 scripts consolidation).
if want python; then
    run python3 scripts/validation/phase8-validation.py \
        --smoke \
        --timeout "$PHASE8_TIMEOUT" \
        --vfs-bin "$VFS_BIN" \
        --output /tmp/vfs-val/phase8.json
fi

if want canon; then
    run scripts/validation/consistency-canon.sh
fi

printf '\nHonest gate passed (phases:%s) with VFS_BIN=%s\n' \
    "$(printf '%s' " $PHASES" | tr -s ' ')" "$VFS_BIN"
