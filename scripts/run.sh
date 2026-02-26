#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="${1:-data/denmark-latest.osm.pbf}"
OUT="${2:-data/out.pmtiles}"

shift 2 2>/dev/null || true

scripts/build.sh
detect_ocean

"$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir .tilegen_tmp $OCEAN_FLAG "$@"
