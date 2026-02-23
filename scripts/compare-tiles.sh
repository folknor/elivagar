#!/usr/bin/env bash
set -euo pipefail

# Compare feature counts between two PMTiles archives.
# Usage: scripts/compare-tiles.sh [file_a] [file_b] [--sample N]
#
# Defaults:
#   file_a = data/denmark-latest.pmtiles (elivagar)
#   file_b = data/planetiler-bench.pmtiles (Planetiler)
#   sample = 200 tiles per zoom

FILE_A="${1:-data/denmark-latest.pmtiles}"
FILE_B="${2:-data/planetiler-bench.pmtiles}"
shift 2 2>/dev/null || true

echo "Building (release)..."
cargo build --release --example compare_tiles 2>&1 | tail -1

echo ""
exec ./target/release/examples/compare_tiles "$FILE_A" "$FILE_B" "$@"
