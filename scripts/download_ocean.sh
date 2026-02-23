#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

URL="https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip"
ZIP="data/water-polygons-split-3857.zip"
DIR="data/water-polygons-split-3857"
SHP="$DIR/water_polygons.shp"

if [ -f "$SHP" ]; then
    echo "Ocean shapefile already exists: $SHP"
    exit 0
fi

echo "Downloading ocean polygons (~765 MB)..."
curl -L -o "$ZIP" "$URL"

echo "Extracting..."
unzip -o "$ZIP" -d data/

echo "Cleaning up zip..."
rm "$ZIP"

echo "Done: $SHP"
ls -lh "$SHP"
