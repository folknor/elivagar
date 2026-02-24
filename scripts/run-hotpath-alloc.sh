#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

PBF="${1:-$PROJECT_DIR/data/denmark-latest.osm.pbf}"
OUT="${2:-$PROJECT_DIR/data/hotpath-alloc-output.pmtiles}"

echo "NOTE: mimalloc is disabled for alloc profiling — wall-clock times are not meaningful."
cargo build --release --features hotpath-alloc --manifest-path "$PROJECT_DIR/Cargo.toml"

HOTPATH_METRICS_SERVER_OFF=true "$PROJECT_DIR/target/release/elivagar" \
    "$PBF" "$OUT" \
    --ocean "$PROJECT_DIR/data/water-polygons-split-4326/water_polygons.shp"
