#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
OUT="${2:-data/out.pmtiles}"

shift 2 2>/dev/null || true

# Build
cargo build --release 2>&1
ELIVAGAR_BIN=$(cargo build --release --message-format=json 2>/dev/null \
    | grep '"executable"' | grep -oP '"executable":"\K[^"]+')

# Ocean shapefile detection
OCEAN_SHP="data/water-polygons-split-3857/water_polygons.shp"
OCEAN_SIMPLIFIED_SHP="data/simplified-water-polygons-split-3857/simplified_water_polygons.shp"
OCEAN_FLAG=""
if [ -f "$OCEAN_SHP" ]; then
    OCEAN_FLAG="--ocean $OCEAN_SHP"
    if [ -f "$OCEAN_SIMPLIFIED_SHP" ]; then
        OCEAN_FLAG="$OCEAN_FLAG --ocean-simplified $OCEAN_SIMPLIFIED_SHP"
    fi
fi

"$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir .tilegen_tmp $OCEAN_FLAG "$@"
