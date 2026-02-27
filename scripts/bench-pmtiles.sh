#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

TILES="${1:-500000}"
RUNS="${2:-5}"
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
HOST=$(hostname 2>/dev/null || echo "unknown")

echo "Building bench_pmtiles (release)..."
cargo build --release --example bench_pmtiles 2>&1

echo ""
echo "commit: $COMMIT  host: $HOST"
echo ""
"$CARGO_TARGET_DIR/release/examples/bench_pmtiles" --tiles "$TILES" --runs "$RUNS"
