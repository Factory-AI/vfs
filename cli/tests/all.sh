#!/bin/sh
set -e

DIR="$(dirname "$0")"

"$DIR/test-init.sh"

# Syscall tests in two configurations:
# 1. Linux baseline - establishes expected behavior
"$DIR/test-linux-syscalls.sh"

# 2. FUSE overlay (agentfs run) - tests copy-on-write
"$DIR/test-run-syscalls.sh" || true  # Requires user namespaces (may fail in CI)

"$DIR/test-run-bash.sh" || true  # Requires user namespaces (may fail in CI)
"$DIR/test-run-git.sh" || true  # Requires user namespaces (may fail in CI)
"$DIR/test-teardown-bounded.sh"
SIGNAL_TEARDOWN_SIGNAL_DELAY="${SIGNAL_TEARDOWN_SIGNAL_DELAY:-5s}" \
    "$DIR/test-signal-teardown.sh"
# Short corruption/concurrency smoke; the test prints SKIP and exits 0 if
# Linux user namespace/FUSE prerequisites are unavailable.
CORRUPTION_TORTURE_WORKERS="${CORRUPTION_TORTURE_WORKERS:-2}" \
CORRUPTION_TORTURE_ITERATIONS="${CORRUPTION_TORTURE_ITERATIONS:-2}" \
CORRUPTION_TORTURE_TIMEOUT="${CORRUPTION_TORTURE_TIMEOUT:-60}" \
CORRUPTION_TORTURE_INTEGRITY_INTERVAL="${CORRUPTION_TORTURE_INTEGRITY_INTERVAL:-1}" \
"$DIR/test-corruption-torture.sh"
"$DIR/test-mount.sh"
"$DIR/test-overlay-whiteout.sh"
"$DIR/test-overlay-delta-in-base-dir.sh"
"$DIR/test-overlay-base-dir-rename-exdev.sh"
"$DIR/test-fuse-cache-invalidation.sh"
"$DIR/test-symlinks.sh" || true  # Requires user namespaces (may fail in CI)
