#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
NAME="$(basename "${PBF%.osm.pbf}")"
OUT="data/${NAME}.pmtiles"

scripts/build.sh
echo ""
echo "=== Running elivagar ==="

OCEAN_SHP="data/water-polygons-split-3857/water_polygons.shp"
OCEAN_FLAG=""
if [ -f "$OCEAN_SHP" ]; then
    OCEAN_FLAG="--ocean $OCEAN_SHP"
fi

time ./target/release/elivagar "$PBF" "$OUT" --tmp-dir .tilegen_tmp $OCEAN_FLAG

echo ""
ls -lh "$OUT"
