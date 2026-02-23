#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-3}"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench-planetiler.sh [path/to/file.osm.pbf] [runs]"
    exit 1
fi

for cmd in jq curl; do
    if ! command -v "$cmd" &>/dev/null; then
        echo "Error: $cmd is required but not found"
        exit 1
    fi
done

FILE_MB=$(( $(stat -c%s "$PBF") / 1000000 ))
OUT="data/planetiler-bench.pmtiles"

# --- Temurin JDK setup ---
JDK_MAJOR=21
JDK_DIR="data/jdk"
JDK_VERSION_FILE="data/.jdk-version"
JAVA="$JDK_DIR/bin/java"

ensure_jdk() {
    echo "Checking Temurin JDK ${JDK_MAJOR}..."
    local api_url="https://api.adoptium.net/v3/assets/latest/${JDK_MAJOR}/hotspot?architecture=x64&image_type=jdk&os=linux&vendor=eclipse"
    local api_json
    api_json=$(curl -sfL "$api_url") || { echo "Error: failed to query Adoptium API"; exit 1; }

    local release_name download_url
    release_name=$(echo "$api_json" | jq -r '.[0].release_name')
    download_url=$(echo "$api_json" | jq -r '.[0].binary.package.link')

    if [ -f "$JDK_VERSION_FILE" ] && [ "$(cat "$JDK_VERSION_FILE")" = "$release_name" ] && [ -x "$JAVA" ]; then
        echo "  JDK up to date: $release_name"
        return
    fi

    echo "  Downloading Temurin JDK $release_name..."
    local tarball="data/jdk-download.tar.gz"
    curl -fsSL -o "$tarball" "$download_url"
    rm -rf "$JDK_DIR"
    mkdir -p "$JDK_DIR"
    tar xzf "$tarball" -C "$JDK_DIR" --strip-components=1
    rm -f "$tarball"
    echo "$release_name" > "$JDK_VERSION_FILE"
    echo "  Installed: $("$JAVA" -version 2>&1 | head -1)"
}

# --- Planetiler JAR setup ---
PLANETILER_JAR="data/planetiler.jar"
PLANETILER_VERSION_FILE="data/.planetiler-version"

ensure_planetiler() {
    echo "Checking Planetiler..."
    local api_json
    api_json=$(curl -sfL "https://api.github.com/repos/onthegomap/planetiler/releases/latest") || {
        echo "Error: failed to query GitHub API"; exit 1;
    }

    local tag_name download_url
    tag_name=$(echo "$api_json" | jq -r '.tag_name')
    download_url=$(echo "$api_json" | jq -r '.assets[] | select(.name == "planetiler.jar") | .browser_download_url')

    if [ -f "$PLANETILER_VERSION_FILE" ] && [ "$(cat "$PLANETILER_VERSION_FILE")" = "$tag_name" ] && [ -f "$PLANETILER_JAR" ]; then
        echo "  Planetiler up to date: $tag_name"
        return
    fi

    echo "  Downloading Planetiler $tag_name..."
    curl -fsSL -o "$PLANETILER_JAR" "$download_url"
    echo "$tag_name" > "$PLANETILER_VERSION_FILE"
    echo "  Installed: $tag_name ($(du -h "$PLANETILER_JAR" | cut -f1))"
}

# --- Ensure Planetiler source data is cached ---
prime_planetiler_data() {
    # Planetiler with --download fetches ocean + natural earth data on first run.
    # Check if the data directory exists; if not, do a priming run.
    if [ -d "data/sources" ]; then
        echo "  Planetiler source data cached"
        return
    fi

    echo "  Priming Planetiler data (first-time download of ocean + natural earth)..."
    echo "  This may take a few minutes."
    "$JAVA" "-Xmx${HEAP_MB}m" -jar "$PLANETILER_JAR" shortbread \
        --osm-path="$PBF" \
        --output="$OUT" \
        --force \
        --nodemap-type=sparsearray \
        --tmpdir=.planetiler_tmp \
        --download \
        2>&1 | tail -5
    echo "  Data primed."
}

# --- Main ---
mkdir -p data

HEAP_MB=$(( FILE_MB * 2 ))
if [ "$HEAP_MB" -lt 2048 ]; then
    HEAP_MB=2048
fi

ensure_jdk
ensure_planetiler
prime_planetiler_data
echo ""

echo "=== Planetiler Shortbread benchmark ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo "  heap: ${HEAP_MB}m"
echo ""

BEST_MS=999999999
BEST_BYTES=0

for i in $(seq 1 "$RUNS"); do
    echo "  run $i/$RUNS..."
    rm -f "$OUT"
    START=$(date +%s%N)
    "$JAVA" "-Xmx${HEAP_MB}m" -jar "$PLANETILER_JAR" shortbread \
        --osm-path="$PBF" \
        --output="$OUT" \
        --force \
        --nodemap-type=sparsearray \
        --tmpdir=.planetiler_tmp \
        --download \
        &>/dev/null
    END=$(date +%s%N)
    MS=$(( (END - START) / 1000000 ))

    OUTPUT_BYTES=0
    if [ -f "$OUT" ]; then
        OUTPUT_BYTES=$(stat -c%s "$OUT")
    fi

    echo "    ${MS}ms ($(( OUTPUT_BYTES / 1000000 )) MB output)"
    if [ "$MS" -lt "$BEST_MS" ]; then
        BEST_MS=$MS
        BEST_BYTES=$OUTPUT_BYTES
    fi
done

printf "  %-12s %6s ms\n" "planetiler" "$BEST_MS"

# Emit in standard format to stderr
>&2 echo "---"
>&2 echo "tool=planetiler"
>&2 echo "total_ms=$BEST_MS"
>&2 echo "output_bytes=$BEST_BYTES"
