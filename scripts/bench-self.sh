#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-1}"
SKIP_TO=""
NO_OCEAN=false

shift 2 2>/dev/null || true
while [ $# -gt 0 ]; do
    case "$1" in
        --skip-to) SKIP_TO="$2"; shift 2 ;;
        --no-ocean) NO_OCEAN=true; shift ;;
        *) echo "Unknown flag: $1"; exit 1 ;;
    esac
done

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench-self.sh [pbf] [runs] [--skip-to ocean|sort] [--no-ocean]"
    exit 1
fi

NAME="$(basename "${PBF%.osm.pbf}")"
FILE_MB=$(( $(stat -c%s "$PBF") / 1000000 ))
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")

echo "=== bench-self ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo "  commit: $COMMIT"

# Build
echo ""
echo "Building (release)..."
cargo build --release 2>&1 | tail -1
ELIVAGAR_BIN=$(cargo build --release --message-format=json 2>/dev/null \
    | grep '"executable"' | grep -oP '"executable":"\K[^"]+')

# Ocean shapefile detection
OCEAN_SHP="data/water-polygons-split-3857/water_polygons.shp"
OCEAN_SIMPLIFIED_SHP="data/simplified-water-polygons-split-3857/simplified_water_polygons.shp"
OCEAN_FLAG=""
if [ "$NO_OCEAN" = false ] && [ -f "$OCEAN_SHP" ]; then
    OCEAN_FLAG="--ocean $OCEAN_SHP"
    if [ -f "$OCEAN_SIMPLIFIED_SHP" ]; then
        OCEAN_FLAG="$OCEAN_FLAG --ocean-simplified $OCEAN_SIMPLIFIED_SHP"
    fi
fi

SKIP_FLAG=""
if [ -n "$SKIP_TO" ]; then
    SKIP_FLAG="--skip-to $SKIP_TO"
fi

OUT="data/${NAME}.pmtiles"
STDERR_FILE=$(mktemp .bench_stderr.XXXXXX)
trap 'rm -f "$STDERR_FILE"' EXIT

parse() { grep -oP "^${1}=\\K.*" "$STDERR_FILE" || echo "-"; }

echo ""
BEST_TOTAL=999999999

for i in $(seq 1 "$RUNS"); do
    echo "  run $i/$RUNS..."
    "$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir .tilegen_tmp $OCEAN_FLAG $SKIP_FLAG 2> "$STDERR_FILE"

    THIS_TOTAL=$(parse total_ms)
    if [ "$THIS_TOTAL" != "-" ] && [ "$THIS_TOTAL" -lt "$BEST_TOTAL" ]; then
        BEST_TOTAL=$THIS_TOTAL
        BEST_PHASE12=$(parse phase12_ms)
        BEST_OCEAN=$(parse ocean_ms)
        BEST_PHASE3=$(parse phase3_ms)
        BEST_PHASE4=$(parse phase4_ms)
        BEST_FEATURES=$(parse features)
        BEST_TILES=$(parse tiles)
        BEST_BYTES=$(parse output_bytes)
    fi
done

echo ""
echo "  total:    ${BEST_TOTAL} ms"
echo "  pbf:      ${BEST_PHASE12} ms"
echo "  ocean:    ${BEST_OCEAN} ms"
echo "  sort:     ${BEST_PHASE3} ms"
echo "  assemble: ${BEST_PHASE4} ms"
echo "  features: ${BEST_FEATURES}"
echo "  tiles:    ${BEST_TILES}"
echo "  output:   ${BEST_BYTES} bytes"
