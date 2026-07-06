#!/usr/bin/env python3
"""Run the earcut oracle over all polygon layers and compare feature counts.

Usage: oracle_sweep.py <pmtiles> <suffix> [baseline-suffix]
Writes notes/qa/oracle-<layer>-<suffix>.txt per layer; if a baseline suffix
is given, prints per-layer max |feature-count delta| vs the baseline files
and fails (exit 1) if any layer/zoom deviates more than 2% (counts >= 50).
"""
import re
import subprocess
import sys
from pathlib import Path

LAYERS = ["ocean", "water_polygons", "land", "buildings", "sites",
          "dam_polygons", "pier_polygons", "street_polygons", "bridges"]

def parse(path):
    rows = {}
    for line in Path(path).read_text().splitlines():
        m = re.match(r"\s*(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)", line)
        if m:
            rows[int(m.group(1))] = int(m.group(2))
    text = Path(path).read_text()
    clean = "No polygon exceeded" in text
    return rows, clean

def main():
    pmtiles, suffix = sys.argv[1], sys.argv[2]
    base = sys.argv[3] if len(sys.argv) > 3 else None
    qa = Path("notes/qa")
    qa.mkdir(parents=True, exist_ok=True)
    failed = False
    for layer in LAYERS:
        out = qa / f"oracle-{layer}-{suffix}.txt"
        with out.open("w") as f:
            subprocess.run(["node", "scripts/validate/earcut-oracle.mjs",
                            pmtiles, layer, "0.01"], stdout=f, check=True)
        rows, clean = parse(out)
        status = "clean" if clean else "DEVIANT"
        if not clean:
            failed = True
        delta_txt = ""
        if base:
            brows, _ = parse(qa / f"oracle-{layer}-{base}.txt")
            worst = 0.0
            for z in set(rows) | set(brows):
                a, b = rows.get(z, 0), brows.get(z, 0)
                if max(a, b) >= 50:
                    d = abs(a - b) / max(b, 1) * 100
                    worst = max(worst, d)
                    if d > 2.0:
                        failed = True
            delta_txt = f"  max-delta {worst:.2f}%"
        print(f"{layer:16} {status}{delta_txt}")
    sys.exit(1 if failed else 0)

if __name__ == "__main__":
    main()
