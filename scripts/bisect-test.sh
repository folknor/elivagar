#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="data/denmark-latest.osm.pbf"
THRESHOLD=40000
RUN_TIMEOUT=240

echo "=== bisect test at $(git rev-parse --short HEAD) ==="

git checkout -- Cargo.lock 2>/dev/null || true

if ! cargo build --release 2>&1; then
    echo "  build failed — SKIP"
    exit 125
fi

detect_ocean

STDERR_FILE=$(mktemp "$CARGO_TARGET_DIR/.bisect_stderr.XXXXXX")
trap 'rm -f "$STDERR_FILE"; git checkout -- Cargo.lock 2>/dev/null || true' EXIT

EXIT_CODE=0
timeout "$RUN_TIMEOUT" "$ELIVAGAR_BIN" "$PBF" "data/bisect-test.pmtiles" --tmp-dir .tilegen_tmp $OCEAN_FLAG 2> "$STDERR_FILE" || EXIT_CODE=$?
if [ "$EXIT_CODE" -eq 124 ]; then
    echo "  KILLED after ${RUN_TIMEOUT}s — BAD"
    exit 1
elif [ "$EXIT_CODE" -ne 0 ]; then
    echo "  elivagar run failed (exit $EXIT_CODE) — SKIP"
    exit 125
fi

TOTAL=$(parse_kv total_ms "$STDERR_FILE")
if [ "$TOTAL" = "-" ]; then TOTAL=999999; fi
PBF_MS=$(parse_kv phase12_ms "$STDERR_FILE")
if [ "$PBF_MS" = "-" ]; then PBF_MS=999999; fi

echo "  total=${TOTAL}ms  pbf=${PBF_MS}ms  threshold=${THRESHOLD}ms"

if [ "$TOTAL" -gt "$THRESHOLD" ]; then
    echo "  BAD (above threshold)"
    exit 1
else
    echo "  GOOD (below threshold)"
    exit 0
fi
