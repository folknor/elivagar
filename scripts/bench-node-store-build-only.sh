#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

NODES_M="${1:-5}"
RUNS="${2:-3}"
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
HOST=$(hostname 2>/dev/null || echo "unknown")

echo "Building bench_node_store (release + hotpath)..."
cargo build --release --features hotpath --example bench_node_store 2>&1

echo ""
echo "commit: $COMMIT  host: $HOST"
echo ""
HOTPATH_METRICS_SERVER_OFF=true "${CARGO_TARGET_DIR}/release/examples/bench_node_store" \
    --nodes "$NODES_M" \
    --runs "$RUNS" \
    --build-only
