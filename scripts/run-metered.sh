#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# -------------------------------------------------------------------------
# run-metered.sh — build + run elivagar, capture peak RSS via /usr/bin/time.
# No cgroup memory cap — relies on kernel to manage mmap pages.
#
# Usage: scripts/run-metered.sh [pbf] [out.pmtiles] [extra args...]
# -------------------------------------------------------------------------

PBF="${1:-data/denmark-latest.osm.pbf}"
OUT="${2:-data/out.pmtiles}"

shift 2 2>/dev/null || true

scripts/build.sh
detect_ocean

mkdir -p data/tilegen_tmp

echo "PBF:    $PBF"
echo "Output: $OUT"
echo ""

/usr/bin/time -v "$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir data/tilegen_tmp $OCEAN_FLAG "$@" 2>&1 | tee data/tilegen_tmp/time-output.txt

echo ""
echo "=== Peak memory ==="
grep "Maximum resident" data/tilegen_tmp/time-output.txt
