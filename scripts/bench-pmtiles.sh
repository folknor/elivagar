#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

TILES="${1:-500000}"
RUNS="${2:-5}"

echo "Building bench_pmtiles (release)..."
cargo build --release --example bench_pmtiles 2>&1

echo ""
./target/release/examples/bench_pmtiles --tiles "$TILES" --runs "$RUNS"
