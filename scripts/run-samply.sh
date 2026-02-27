#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

require_cmd samply "cargo install samply"

# samply uses perf_event_open which requires perf_event_paranoid <= 1
PARANOID=$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo "0")
if [ "$PARANOID" -gt 1 ]; then
    echo "ERROR: /proc/sys/kernel/perf_event_paranoid is $PARANOID (needs <= 1)"
    echo ""
    echo "  Run:  echo 1 | sudo tee /proc/sys/kernel/perf_event_paranoid"
    echo ""
    echo "  This is a runtime-only change that resets on reboot."
    exit 1
fi

PBF="${1:-data/denmark-latest.osm.pbf}"
OUT="${2:-data/samply-output.pmtiles}"

shift 2 2>/dev/null || true

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/run-samply.sh [pbf] [out.pmtiles] [extra args...]"
    exit 1
fi

COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
HOST=$(hostname 2>/dev/null || echo "unknown")
PROFILE_OUT="data/samply-${HOST}-${COMMIT}.json.gz"

echo "=== samply profile ==="
echo "  file: $PBF"
echo "  host: $HOST"
echo "  commit: $COMMIT"

echo ""
echo "Building (profiling profile: release + debug symbols)..."
cargo build --profile profiling

PROFILING_BIN="${CARGO_TARGET_DIR}/profiling/elivagar"

detect_ocean

echo ""
echo "Recording profile..."
samply record --save-only -o "$PROFILE_OUT" \
    "$PROFILING_BIN" "$PBF" "$OUT" --tmp-dir data/tilegen_tmp $OCEAN_FLAG "$@"

echo ""
echo "Profile saved to $PROFILE_OUT"
echo "View with: samply load $PROFILE_OUT"
