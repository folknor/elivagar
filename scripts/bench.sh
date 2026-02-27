#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-3}"
LOG="benchmarks/bench.tsv"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench.sh [path/to/file.osm.pbf] [runs]"
    exit 1
fi

NAME="$(basename "${PBF%.osm.pbf}")"
FILE_MB=$(file_size_mb "$PBF")
COMMIT=$(git rev-parse --short HEAD 2>/dev/null || echo "unknown")
DATE=$(date +%Y-%m-%d)
SUBJECT=$(git log -1 --format=%s 2>/dev/null || echo "-")
HOST=$(hostname 2>/dev/null || echo "unknown")

echo "=== elivagar benchmark ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo "  commit: $COMMIT"
echo ""

# Build and locate binary
echo "Building elivagar (release)..."
scripts/build.sh
echo ""

# Ocean shapefile detection
detect_ocean
if [ -n "$OCEAN_FLAG" ]; then
    echo "  ocean: detected"
else
    echo "  ocean: not found (run scripts/download_ocean.sh)"
fi

OUT="data/${NAME}.pmtiles"

# Create TSV header if needed
mkdir -p "$(dirname "$LOG")"
if [ ! -f "$LOG" ]; then
    printf "date\tcommit\tsubject\ttool\ttotal_ms\tphase12_ms\tocean_ms\tphase3_ms\tphase4_ms\tfeatures\ttiles\toutput_bytes\tpbf\thost\n" > "$LOG"
fi

STDERR_FILE=$(mktemp "$CARGO_TARGET_DIR/.bench_stderr.XXXXXX")
trap 'rm -f "$STDERR_FILE"' EXIT

record_result() {
    local tool="$1"
    local total phase12 ocean phase3 phase4 features tiles output_bytes
    total=$(parse_kv total_ms "$STDERR_FILE")
    phase12=$(parse_kv phase12_ms "$STDERR_FILE")
    ocean=$(parse_kv ocean_ms "$STDERR_FILE")
    phase3=$(parse_kv phase3_ms "$STDERR_FILE")
    phase4=$(parse_kv phase4_ms "$STDERR_FILE")
    features=$(parse_kv features "$STDERR_FILE")
    tiles=$(parse_kv tiles "$STDERR_FILE")
    output_bytes=$(parse_kv output_bytes "$STDERR_FILE")

    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
        "$DATE" "$COMMIT" "$SUBJECT" "$tool" \
        "$total" "$phase12" "$ocean" "$phase3" "$phase4" \
        "$features" "$tiles" "$output_bytes" "$NAME" "$HOST" >> "$LOG"

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
    "$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir data/tilegen_tmp $OCEAN_FLAG 2> "$STDERR_FILE"

    THIS_TOTAL=$(parse_kv total_ms "$STDERR_FILE")
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
if command -v cmake &>/dev/null && command -v g++ &>/dev/null; then
    echo "--- tilemaker ---"
    scripts/bench-tilemaker.sh "$PBF" "$RUNS" 2> "$STDERR_FILE"
    record_result "tilemaker"
    echo ""
else
    echo "Skipping Tilemaker (cmake and g++ required)"
    echo ""
fi

echo "=== Results recorded to $LOG ==="
tail -5 "$LOG" | column -t -s$'\t'
