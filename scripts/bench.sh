#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-3}"
LOG="benchmarks.tsv"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench.sh [path/to/file.osm.pbf] [runs]"
    exit 1
fi

NAME="$(basename "${PBF%.osm.pbf}")"
FILE_MB=$(( $(stat -c%s "$PBF") / 1000000 ))
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
DATE=$(date +%Y-%m-%d)
SUBJECT=$(git log -1 --format=%s 2>/dev/null || echo "-")

echo "=== elivagar benchmark ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo "  commit: $COMMIT"
echo ""

# Build and locate binary
echo "Building elivagar (release)..."
cargo build --release 2>&1 | tail -1
ELIVAGAR_BIN=$(cargo build --release --message-format=json 2>/dev/null \
    | grep '"executable"' | grep -oP '"executable":"\K[^"]+')
echo ""

# Ocean shapefile detection
OCEAN_SHP="data/water-polygons-split-3857/water_polygons.shp"
OCEAN_FLAG=""
if [ -f "$OCEAN_SHP" ]; then
    OCEAN_FLAG="--ocean $OCEAN_SHP"
    echo "  ocean: $OCEAN_SHP"
else
    echo "  ocean: not found (run scripts/download_ocean.sh)"
fi

OUT="data/${NAME}.pmtiles"

# Create TSV header if needed
if [ ! -f "$LOG" ]; then
    printf "date\tcommit\tsubject\ttool\ttotal_ms\tphase12_ms\tocean_ms\tphase3_ms\tphase4_ms\tfeatures\ttiles\toutput_bytes\tpbf\n" > "$LOG"
fi

STDERR_FILE=$(mktemp .bench_stderr.XXXXXX)
trap 'rm -f "$STDERR_FILE"' EXIT

# Parse key=value from a stderr file
parse() { grep -oP "^${1}=\\K.*" "$STDERR_FILE" || echo "-"; }

record_result() {
    local tool="$1"
    local total phase12 ocean phase3 phase4 features tiles output_bytes
    total=$(parse total_ms)
    phase12=$(parse phase12_ms)
    ocean=$(parse ocean_ms)
    phase3=$(parse phase3_ms)
    phase4=$(parse phase4_ms)
    features=$(parse features)
    tiles=$(parse tiles)
    output_bytes=$(parse output_bytes)

    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
        "$DATE" "$COMMIT" "$SUBJECT" "$tool" \
        "$total" "$phase12" "$ocean" "$phase3" "$phase4" \
        "$features" "$tiles" "$output_bytes" "$NAME" >> "$LOG"

    printf "  %-12s %6s ms  (pbf=%s ocean=%s sort=%s asm=%s)\n" \
        "$tool" "$total" "$phase12" "$ocean" "$phase3" "$phase4"
}

# --- Elivagar benchmark ---
echo ""
echo "--- elivagar ---"
BEST_TOTAL=999999999
BEST_STDERR=""

for i in $(seq 1 "$RUNS"); do
    echo "  run $i/$RUNS..."
    "$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir .tilegen_tmp $OCEAN_FLAG 2> "$STDERR_FILE"

    THIS_TOTAL=$(parse total_ms)
    if [ "$THIS_TOTAL" != "-" ] && [ "$THIS_TOTAL" -lt "$BEST_TOTAL" ]; then
        BEST_TOTAL=$THIS_TOTAL
        BEST_STDERR=$(cat "$STDERR_FILE")
    fi
done

# Write the best run's stderr back for record_result to parse
echo "$BEST_STDERR" > "$STDERR_FILE"
record_result "elivagar"
echo ""

# --- Planetiler benchmark ---
if command -v curl &>/dev/null && command -v jq &>/dev/null; then
    echo "--- planetiler ---"
    scripts/bench-planetiler.sh "$PBF" "$RUNS" 2> "$STDERR_FILE"
    record_result "planetiler"
    echo ""
else
    echo "Skipping Planetiler (curl and jq required)"
    echo ""
fi

# --- Tilemaker benchmark ---
if [ -x "data/tilemaker/build/tilemaker" ]; then
    echo "--- tilemaker ---"
    scripts/bench-tilemaker.sh "$PBF" "$RUNS" 2> "$STDERR_FILE"
    record_result "tilemaker"
    echo ""
else
    echo "Skipping Tilemaker (run scripts/bench-tilemaker.sh once to set up)"
    echo ""
fi

echo "=== Results recorded to $LOG ==="
tail -5 "$LOG" | column -t -s$'\t'
