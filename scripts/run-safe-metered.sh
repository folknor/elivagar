#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# -------------------------------------------------------------------------
# run-safe-metered.sh — like run-safe.sh but captures peak RSS via /usr/bin/time.
#
# Usage: scripts/run-safe-metered.sh [--mem 25G] [pbf] [out.pmtiles] [extra args...]
# Default memory cap: 28G
# -------------------------------------------------------------------------

MEM_LIMIT="28G"

if [[ "${1:-}" == "--mem" ]]; then
    MEM_LIMIT="$2"
    shift 2
fi

PBF="${1:-data/denmark-latest.osm.pbf}"
OUT="${2:-data/out.pmtiles}"

shift 2 2>/dev/null || true

scripts/build.sh
detect_ocean

mkdir -p .tilegen_tmp

echo "Memory cap: $MEM_LIMIT"
echo "PBF:        $PBF"
echo "Output:     $OUT"
echo ""

/usr/bin/time -v systemd-run --scope -p MemoryMax="$MEM_LIMIT" -p MemorySwapMax=0 \
    "$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir .tilegen_tmp $OCEAN_FLAG "$@" 2>&1 | tee .tilegen_tmp/time-output.txt

echo ""
echo "=== Peak memory ==="
grep "Maximum resident" .tilegen_tmp/time-output.txt
