#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
source "$(dirname "$0")/lib.sh"

# -------------------------------------------------------------------------
# run-safe.sh — run elivagar under a cgroup memory cap.
# If the process exceeds the limit it gets SIGKILL; your desktop stays alive.
#
# Usage: scripts/run-safe.sh [--mem 25G] [pbf] [out.pmtiles] [extra args...]
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

# -------------------------------------------------------------------------
# Pre-flight: estimate SortedNodeStore memory from PBF node count.
# -------------------------------------------------------------------------

find_pbfhogg() {
    if command -v pbfhogg &>/dev/null; then
        echo "pbfhogg"
        return
    fi
    local sibling="../pbfhogg/target/release/pbfhogg"
    if [[ -x "$sibling" ]]; then
        echo "$sibling"
        return
    fi
    return 1
}

estimate_memory() {
    local pbf="$1" mem_limit="$2"
    local pbfhogg_bin
    pbfhogg_bin=$(find_pbfhogg) || return 0

    echo "Scanning PBF for node count..."
    mkdir -p data/tilegen_tmp
    "$pbfhogg_bin" fileinfo --extended "$pbf" > data/tilegen_tmp/fileinfo.txt

    python3 -c "
import sys

node_count = None
with open('data/tilegen_tmp/fileinfo.txt') as f:
    for line in f:
        if line.startswith('Nodes:'):
            node_count = int(line.split(':')[1].strip())

if node_count is None:
    print('  Could not determine node count, skipping estimate.')
    sys.exit(0)

# SortedNodeStore: nodes * 8 bytes coords + ~1.5 GB bitmask overhead
store_gb = (node_count * 8 + 1_500_000_000) / 1e9
# Add ~2 GB headroom for rayon, way_index, sort buffers, OS
total_gb = store_gb + 2.0

# Parse memory limit (e.g. '28G', '16384M')
limit_str = '$mem_limit'
if limit_str.endswith('G'):
    limit_gb = float(limit_str[:-1])
elif limit_str.endswith('M'):
    limit_gb = float(limit_str[:-1]) / 1024
else:
    limit_gb = float(limit_str) / 1e9

print(f'  Nodes:          {node_count:,}')
print(f'  NodeStore est:  {store_gb:.1f} GB')
print(f'  Total est:      {total_gb:.1f} GB (incl ~2 GB overhead)')
print(f'  Memory cap:     {limit_gb:.1f} GB')
print()

if total_gb > limit_gb:
    print(f'  WARNING: estimated memory ({total_gb:.1f} GB) exceeds cap ({limit_gb:.1f} GB).')
    print(f'  The process will likely be killed by the cgroup OOM.')
    print(f'  Consider raising --mem or using a smaller extract.')
    sys.exit(1)
elif total_gb > limit_gb * 0.8:
    print(f'  CAUTION: estimated memory ({total_gb:.1f} GB) is within 80% of cap ({limit_gb:.1f} GB).')
    print(f'  May get tight with large tiles or many relations.')
else:
    print(f'  OK: estimated memory well within cap.')
print()
"
    local rc=$?
    if [[ $rc -ne 0 ]]; then
        echo ""
        read -r -p "Continue anyway? [y/N] " answer
        if [[ "$answer" != "y" && "$answer" != "Y" ]]; then
            exit 1
        fi
        echo ""
    fi
}

estimate_memory "$PBF" "$MEM_LIMIT"

# -------------------------------------------------------------------------
# Build and run under cgroup memory cap.
# -------------------------------------------------------------------------

scripts/build.sh
detect_ocean

echo "Memory cap: $MEM_LIMIT"
echo "PBF:        $PBF"
echo "Output:     $OUT"
echo ""

systemd-run --scope -p MemoryMax="$MEM_LIMIT" -p MemorySwapMax=0 \
    "$ELIVAGAR_BIN" "$PBF" "$OUT" --tmp-dir data/tilegen_tmp $OCEAN_FLAG "$@"
