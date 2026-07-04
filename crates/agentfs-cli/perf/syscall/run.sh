#!/bin/bash
#
# Benchmark syscall performance across different scenarios:
#   1. Native filesystem
#   2. AgentFS (file in base layer)
#   3. AgentFS (file copied up to delta layer)
#

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CLI_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
REPO_ROOT="$(cd "$CLI_DIR/../.." && pwd)"
TEST_FILE="$SCRIPT_DIR/hello.txt"
ITERATIONS="${1:-100000}"
AGENTFS="$REPO_ROOT/target/release/agentfs"

# Build benchmarks if needed
make -C "$SCRIPT_DIR" -s

# Check agentfs binary
if [ ! -x "$AGENTFS" ]; then
    echo "Error: agentfs binary not found at $AGENTFS"
    echo "Run: cargo build --release"
    exit 1
fi

# Extract metrics from benchmark output
extract_latency() {
    grep "Avg latency:" | awk '{print $3}'
}

extract_throughput() {
    grep "Throughput:" | awk '{print $2}'
}

# Run a benchmark for all three scenarios
run_benchmark() {
    local name="$1"
    local benchmark="$2"

    echo "=============================================="
    echo "$name"
    echo "=============================================="
    echo "Iterations: $ITERATIONS"
    echo ""

    # Test 1: Native filesystem
    echo "[1/3] Native filesystem..."
    NATIVE_OUTPUT=$("$benchmark" "$TEST_FILE" "$ITERATIONS" 2>&1)
    NATIVE_LATENCY=$(echo "$NATIVE_OUTPUT" | extract_latency)
    NATIVE_THROUGHPUT=$(echo "$NATIVE_OUTPUT" | extract_throughput)

    # Test 2: AgentFS (file in base layer)
    echo "[2/3] AgentFS (base layer)..."
    AGENTFS_BASE_OUTPUT=$("$AGENTFS" run "$benchmark" "$TEST_FILE" "$ITERATIONS" 2>&1)
    AGENTFS_BASE_LATENCY=$(echo "$AGENTFS_BASE_OUTPUT" | extract_latency)
    AGENTFS_BASE_THROUGHPUT=$(echo "$AGENTFS_BASE_OUTPUT" | extract_throughput)

    # Test 3: AgentFS (file copied up to delta)
    echo "[3/3] AgentFS (delta layer)..."
    AGENTFS_DELTA_OUTPUT=$("$AGENTFS" run sh -c "
        touch '$TEST_FILE'
        '$benchmark' '$TEST_FILE' '$ITERATIONS'
    " 2>&1)
    AGENTFS_DELTA_LATENCY=$(echo "$AGENTFS_DELTA_OUTPUT" | extract_latency)
    AGENTFS_DELTA_THROUGHPUT=$(echo "$AGENTFS_DELTA_OUTPUT" | extract_throughput)

    # Calculate overhead percentages
    AGENTFS_BASE_OVERHEAD=$(echo "scale=1; (($AGENTFS_BASE_LATENCY / $NATIVE_LATENCY) - 1) * 100" | bc)
    AGENTFS_DELTA_OVERHEAD=$(echo "scale=1; (($AGENTFS_DELTA_LATENCY / $NATIVE_LATENCY) - 1) * 100" | bc)

    # Results
    echo ""
    echo "Results:"
    echo "----------------------------------------------"
    printf "%-25s %10s %12s %10s\n" "Scenario" "Latency" "Throughput" "Overhead"
    printf "%-25s %10s %12s %10s\n" "--------" "-------" "----------" "--------"
    printf "%-25s %8s ns %10s/s %10s\n" "Native" "$NATIVE_LATENCY" "$NATIVE_THROUGHPUT" "-"
    printf "%-25s %8s ns %10s/s %8s %%\n" "AgentFS (base)" "$AGENTFS_BASE_LATENCY" "$AGENTFS_BASE_THROUGHPUT" "$AGENTFS_BASE_OVERHEAD"
    printf "%-25s %8s ns %10s/s %8s %%\n" "AgentFS (delta)" "$AGENTFS_DELTA_LATENCY" "$AGENTFS_DELTA_THROUGHPUT" "$AGENTFS_DELTA_OVERHEAD"
    echo "----------------------------------------------"
    echo ""
}

# Run all benchmarks
run_benchmark "open()+close() Micro-Benchmark" "$SCRIPT_DIR/perf-open-close"
run_benchmark "statx() Micro-Benchmark" "$SCRIPT_DIR/perf-statx"
