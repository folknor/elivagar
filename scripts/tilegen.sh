#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="${1:-data/denmark-latest.osm.pbf}"
NAME="$(basename "${PBF%.osm.pbf}")"
OUT="data/${NAME}.pmtiles"

scripts/build.sh
echo ""
echo "=== Running elivagar ==="

detect_ocean

time "$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir data/tilegen_tmp $OCEAN_FLAG

echo ""
ls -lh "$OUT"
