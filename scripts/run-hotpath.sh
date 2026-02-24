#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

PBF="${1:-$PROJECT_DIR/data/denmark-latest.osm.pbf}"
OUT="${2:-$PROJECT_DIR/data/hotpath-output.pmtiles}"

cargo build --release --features hotpath --manifest-path "$PROJECT_DIR/Cargo.toml"

HOTPATH_METRICS_SERVER_OFF=true "$PROJECT_DIR/target/release/elivagar" \
    "$PBF" "$OUT" \
    --ocean "$PROJECT_DIR/data/water-polygons-split-4326/water_polygons.shp"
