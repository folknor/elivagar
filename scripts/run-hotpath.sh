#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="${1:-data/denmark-latest.osm.pbf}"
OUT="${2:-data/hotpath-output.pmtiles}"

cargo build --release --features hotpath
detect_ocean

HOTPATH_METRICS_SERVER_OFF=true "$ELIVAGAR_BIN" \
    "$PBF" "$OUT" \
    --tmp-dir data/tilegen_tmp $OCEAN_FLAG
