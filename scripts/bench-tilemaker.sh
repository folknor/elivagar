#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

PBF="${1:-data/denmark-latest.osm.pbf}"
RUNS="${2:-3}"

if [ ! -f "$PBF" ]; then
    echo "PBF not found: $PBF"
    echo "Usage: scripts/bench-tilemaker.sh [path/to/file.osm.pbf] [runs]"
    exit 1
fi

FILE_MB=$(( $(stat -c%s "$PBF") / 1000000 ))
OUT="data/tilemaker-bench.pmtiles"

# --- Tilemaker build from source ---
TILEMAKER_DIR="data/tilemaker"
TILEMAKER_BIN="$TILEMAKER_DIR/build/tilemaker"
TILEMAKER_VERSION_FILE="data/.tilemaker-version"

ensure_tilemaker() {
    echo "Checking Tilemaker..."

    # Check build dependencies
    for pkg in cmake g++ make; do
        if ! command -v "$pkg" &>/dev/null; then
            echo "Error: $pkg is required. Install build dependencies:"
            echo "  sudo apt install build-essential cmake"
            exit 1
        fi
    done

    # Clone or update
    if [ ! -d "$TILEMAKER_DIR/.git" ]; then
        echo "  Cloning tilemaker..."
        git clone --depth 1 https://github.com/systemed/tilemaker.git "$TILEMAKER_DIR"
    else
        echo "  Updating tilemaker..."
        git -C "$TILEMAKER_DIR" fetch --depth 1 origin
        git -C "$TILEMAKER_DIR" reset --hard origin/master
    fi

    local current_commit
    current_commit=$(git -C "$TILEMAKER_DIR" rev-parse --short HEAD)

    if [ -f "$TILEMAKER_VERSION_FILE" ] && [ "$(cat "$TILEMAKER_VERSION_FILE")" = "$current_commit" ] && [ -x "$TILEMAKER_BIN" ]; then
        echo "  Tilemaker up to date: $current_commit"
        return
    fi

    echo "  Building tilemaker ($current_commit)..."
    mkdir -p "$TILEMAKER_DIR/build"
    cmake -S "$TILEMAKER_DIR" -B "$TILEMAKER_DIR/build" -DCMAKE_BUILD_TYPE=Release 2>&1 | tail -3
    cmake --build "$TILEMAKER_DIR/build" -j "$(nproc)" 2>&1 | tail -3
    echo "$current_commit" > "$TILEMAKER_VERSION_FILE"
    echo "  Built: $current_commit"
}

# --- Shortbread config ---
SHORTBREAD_DIR="data/shortbread-tilemaker"
SHORTBREAD_CONFIG="$SHORTBREAD_DIR/config.json"
SHORTBREAD_PROCESS="$SHORTBREAD_DIR/process.lua"

ensure_shortbread_config() {
    echo "Checking Shortbread config..."
    if [ ! -d "$SHORTBREAD_DIR/.git" ]; then
        echo "  Cloning shortbread-tilemaker..."
        git clone --depth 1 https://github.com/shortbread-tiles/shortbread-tilemaker.git "$SHORTBREAD_DIR"
    else
        echo "  Shortbread config present"
        git -C "$SHORTBREAD_DIR" pull --ff-only 2>/dev/null || true
    fi

    if [ ! -f "$SHORTBREAD_CONFIG" ]; then
        echo "Error: config.json not found in $SHORTBREAD_DIR"
        exit 1
    fi
}

# --- Ocean shapefiles for Tilemaker ---
# Tilemaker's Shortbread config needs:
#   1. water-polygons-split-4326 (z8-14) — full resolution, EPSG:4326
#   2. simplified-water-polygons-split-4326 (z0-7) — simplified, reprojected from 3857
OCEAN_4326_DIR="data/water-polygons-split-4326"
OCEAN_4326_SHP="$OCEAN_4326_DIR/water_polygons.shp"
SIMPLIFIED_4326_DIR="data/simplified-water-polygons-split-4326"
SIMPLIFIED_4326_SHP="$SIMPLIFIED_4326_DIR/simplified_water_polygons.shp"

ensure_ocean_shapefiles() {
    echo "Checking ocean shapefiles for Tilemaker..."

    # Full resolution (4326)
    if [ -f "$OCEAN_4326_SHP" ]; then
        echo "  Full-res ocean (4326): present"
    else
        echo "  Downloading water-polygons-split-4326 (~700 MB)..."
        local zip="data/water-polygons-split-4326.zip"
        curl -fSL -o "$zip" "https://osmdata.openstreetmap.de/download/water-polygons-split-4326.zip"
        unzip -o "$zip" -d data/
        rm -f "$zip"
        echo "  Extracted: $OCEAN_4326_SHP"
    fi

    # Simplified (needs ogr2ogr to reproject from 3857 to 4326)
    if [ -f "$SIMPLIFIED_4326_SHP" ]; then
        echo "  Simplified ocean (4326): present"
    else
        if ! command -v ogr2ogr &>/dev/null; then
            echo "Error: ogr2ogr is required for simplified ocean reprojection."
            echo "  sudo apt install gdal-bin"
            exit 1
        fi

        local simp_3857_dir="data/simplified-water-polygons-split-3857"
        local simp_3857_shp="$simp_3857_dir/simplified_water_polygons.shp"

        if [ ! -f "$simp_3857_shp" ]; then
            echo "  Downloading simplified-water-polygons-split-3857 (~100 MB)..."
            local zip="data/simplified-water-polygons-split-3857.zip"
            curl -fSL -o "$zip" "https://osmdata.openstreetmap.de/download/simplified-water-polygons-split-3857.zip"
            unzip -o "$zip" -d data/
            rm -f "$zip"
        fi

        echo "  Reprojecting simplified ocean to EPSG:4326..."
        mkdir -p "$SIMPLIFIED_4326_DIR"
        ogr2ogr -f "ESRI Shapefile" \
            "$SIMPLIFIED_4326_SHP" \
            "$simp_3857_shp" \
            -t_srs EPSG:4326 -lco ENCODING=utf8
        echo "  Done: $SIMPLIFIED_4326_SHP"
    fi
}

# --- Patch config.json with absolute shapefile paths ---
patch_config() {
    # Tilemaker's Shortbread config expects shapefiles relative to its own data/ dir.
    # We symlink our downloaded shapefiles there instead of patching JSON.
    local config_data="$SHORTBREAD_DIR/data"
    mkdir -p "$config_data"

    local abs_ocean abs_simplified
    abs_ocean=$(cd "$(dirname "$OCEAN_4326_SHP")" && pwd)
    abs_simplified=$(cd "$(dirname "$SIMPLIFIED_4326_SHP")" && pwd)

    # Symlink ocean shapefiles into the config's data directory
    if [ ! -L "$config_data/water-polygons-split-4326" ]; then
        ln -sfn "$abs_ocean" "$config_data/water-polygons-split-4326"
        echo "  Linked ocean shapefiles"
    fi
    if [ ! -L "$config_data/simplified-water-polygons-split-4326" ]; then
        ln -sfn "$abs_simplified" "$config_data/simplified-water-polygons-split-4326"
        echo "  Linked simplified ocean shapefiles"
    fi
}

# --- Main ---
mkdir -p data

ensure_tilemaker
ensure_shortbread_config
ensure_ocean_shapefiles
patch_config
echo ""

echo "=== Tilemaker Shortbread benchmark ==="
echo "  file: $PBF ($FILE_MB MB)"
echo "  runs: $RUNS (best of)"
echo ""

BEST_MS=999999999
BEST_BYTES=0

for i in $(seq 1 "$RUNS"); do
    echo "  run $i/$RUNS..."
    rm -f "$OUT"
    START=$(date +%s%N)
    "$TILEMAKER_BIN" \
        --input "$PBF" \
        --output "$OUT" \
        --config "$SHORTBREAD_CONFIG" \
        --process "$SHORTBREAD_PROCESS" \
        --fast \
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

printf "  %-12s %6s ms\n" "tilemaker" "$BEST_MS"

# Emit in standard format to stderr
>&2 echo "---"
>&2 echo "tool=tilemaker"
>&2 echo "total_ms=$BEST_MS"
>&2 echo "output_bytes=$BEST_BYTES"
