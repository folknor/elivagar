#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-1}"
SKIP_TO=""
NO_OCEAN=false
COMPRESSION_LEVEL=""

shift 2 2>/dev/null || true
while [ $# -gt 0 ]; do
    case "$1" in
        --skip-to) SKIP_TO="$2"; shift 2 ;;
        --no-ocean) NO_OCEAN=true; shift ;;
        --compression-level) COMPRESSION_LEVEL="$2"; shift 2 ;;
        *) echo "Unknown flag: $1"; exit 1 ;;
    esac
done

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench-self.sh [pbf] [runs] [--skip-to ocean|sort] [--no-ocean] [--compression-level 0-10]"
    exit 1
fi

NAME="$(basename "${PBF%.osm.pbf}")"
FILE_MB=$(file_size_mb "$PBF")
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
HOST=$(hostname 2>/dev/null || echo "unknown")

echo "=== bench-self ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo "  host: $HOST"
if [ -n "$COMPRESSION_LEVEL" ]; then
    echo "  compression: level $COMPRESSION_LEVEL"
fi
echo "  commit: $COMMIT"

# Build
echo ""
echo "Building (release)..."
scripts/build.sh

# Ocean shapefile detection
OCEAN_FLAG=""
if [ "$NO_OCEAN" = false ]; then
    detect_ocean
fi

SKIP_FLAG=""
if [ -n "$SKIP_TO" ]; then
    SKIP_FLAG="--skip-to $SKIP_TO"
fi

COMPRESS_FLAG=""
if [ -n "$COMPRESSION_LEVEL" ]; then
    COMPRESS_FLAG="--compression-level $COMPRESSION_LEVEL"
fi

OUT="data/${NAME}.pmtiles"
STDERR_FILE=$(mktemp "$CARGO_TARGET_DIR/.bench_stderr.XXXXXX")
trap 'rm -f "$STDERR_FILE"' EXIT

echo ""
BEST_TOTAL=999999999
BEST_PHASE12="-" BEST_OCEAN="-" BEST_PHASE3="-" BEST_PHASE4="-"
BEST_FEATURES="-" BEST_TILES="-" BEST_BYTES="-"

RUN_TIMEOUT=240

for i in $(seq 1 "$RUNS"); do
    echo "  run $i/$RUNS..."
    timeout "$RUN_TIMEOUT" "$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir data/tilegen_tmp $OCEAN_FLAG $SKIP_FLAG $COMPRESS_FLAG 2> "$STDERR_FILE"
    EXIT_CODE=$?
    if [ "$EXIT_CODE" -eq 124 ]; then
        echo "  KILLED: run exceeded ${RUN_TIMEOUT}s timeout"
        continue
    elif [ "$EXIT_CODE" -ne 0 ]; then
        echo "  FAILED: exit code $EXIT_CODE"
        continue
    fi

    THIS_TOTAL=$(parse_kv total_ms "$STDERR_FILE")
    if [ "$THIS_TOTAL" != "-" ] && [ "$THIS_TOTAL" -lt "$BEST_TOTAL" ]; then
        BEST_TOTAL=$THIS_TOTAL
        BEST_PHASE12=$(parse_kv phase12_ms "$STDERR_FILE")
        BEST_OCEAN=$(parse_kv ocean_ms "$STDERR_FILE")
        BEST_PHASE3=$(parse_kv phase3_ms "$STDERR_FILE")
        BEST_PHASE4=$(parse_kv phase4_ms "$STDERR_FILE")
        BEST_FEATURES=$(parse_kv features "$STDERR_FILE")
        BEST_TILES=$(parse_kv tiles "$STDERR_FILE")
        BEST_BYTES=$(parse_kv output_bytes "$STDERR_FILE")
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
