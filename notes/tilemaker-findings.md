# Tilemaker Investigation — Key Findings (2026-02-26)

## Tilemaker Phase Timing (Denmark, plantasjen)

| Phase | Tilemaker | Elivagar | Winner |
|-------|-----------|----------|--------|
| PBF reading | 14.6s | 17.0s | TM 2.4s faster |
| Shapefile/Ocean | 5.2s | 3.3s | Us 1.9s faster |
| Collection/Sort | 0.9s | 0.7s | ~same |
| Tile output/Assemble | 8.8s | 4.6s | Us 4.2s faster |
| **Total** | **29.6s** | **28.0s** | |

Tilemaker RSS: 9.3 GB. Tiles: 113,476.
Elivagar tiles: 667,547. **We produce 5.9x more tiles.**

## SMOKING GUN: Geometry Over-Detail

compare-tiles.sh output (sampled tiles, all zooms):

| Layer | Our cmds | TM cmds | Ratio | Impact |
|-------|----------|---------|-------|--------|
| ocean | 3.8M | 381K | **10x** | Huge |
| boundaries | 1.5M | 101K | **15x** | Huge |
| water_lines | 458K | 68K | **6.7x** | Large |
| streets | 1.6M | 597K | **2.7x** | Large |
| land | 14.2M | 8.7M | **1.6x** | Medium |
| boundary_labels | 16K | 0 | only us | TM doesn't emit |

Total vertex commands: Us 23.6M, TM 11.5M = **2x more geometry data**.

## Root Cause Analysis

### 1. Our simplification tolerance is 1 pixel (PIXEL_FACTOR=1.0)
In geometry.rs:20-21:
```rust
const PIXEL_FACTOR: f64 = 1.0;
```
`simplify_tolerance(z) = 1.0 / (4096.0 * 2^z)` = 1 pixel at each zoom.

Tilemaker's config uses `simplify_level: 0.0001` with `simplify_ratio: 2.0`.
At z0, Tilemaker tolerance = 0.0001 * 2^13 = 0.8192 degrees.
Our tolerance at z0 = 1/(4096*1) = 0.000244 Mercator units ≈ much finer.

**We are simplifying ~3000x less aggressively than Tilemaker at low zooms.**

### 2. Ocean polygons are not simplified enough at low zoom
At z0-z3, our ocean has 25K-107K vertex commands per tile.
Tilemaker has 500-1700. That's 20-60x over-detailed.

### 3. We generate 5.9x more tiles
667,547 vs 113,476. Most extra tiles are ocean-only tiles covering sea areas.

### 4. boundary_labels layer
We emit this layer, Tilemaker doesn't. 5,508 features across samples.

## Simplification System Comparison

### Elivagar (geometry.rs)
- Douglas-Peucker with tolerance = PIXEL_FACTOR / (EXTENT * 2^z)
- PIXEL_FACTOR = 1.0, EXTENT = 4096 → tolerance = 1 pixel
- Cascading: z_hi down to z_lo, each zoom uses previous result
- Pre-DP subpixel bbox check skips if whole geometry is subpixel
- DP convergence tracking skips when deviation < next tolerance

### Tilemaker (config.json + geom.cpp)
- Douglas-Peucker via boost::geometry::simplify
- Per-layer configurable: simplify_level * pow(simplify_ratio, (simplify_below-1) - zoom)
- Example: boundaries: level=0.0001, ratio=2, below=14
  - z13: 0.0001, z12: 0.0002, z11: 0.0004, z10: 0.0008, ...
  - z0: 0.0001 * 2^13 = 0.8192 (in lat/lon degrees!)
- This is in DEGREES, not Mercator units → much more aggressive at low zoom
- Also: combine_below merges compatible adjacent features → fewer features

## Tilemaker Architecture Details

### SortedNodeStore (from instrumented run)
- 52,489,653 nodes in 325,900,868 bytes (311 MB, 14.6% wasted)
- 144,979 groups, 1,162,386 chunks
- StreamVByte delta compression, O(1) lookup via popcount

### SortedWayStore
- 83,881 ways (only relations' ways), 2,562,283 nodes, 10.3 MB
- Most ways NOT stored (--fast materializes geometry directly)

### Feature counts (from Tilemaker stdout)
- Points: 2,820,562
- Lines: 1,583,201
- Polygons: 4,570,906
- Total: 8,974,669

Our feature count: 14.9M (from hotpath profile). We emit 66% more features.

## Compression

Tilemaker: libdeflate level 6, thread-local reusable compressor.
Us: flate2 (zlib-ng) level 6, new GzEncoder per tile.
Gap: ~10-20% throughput difference. Small win (~0.3-0.5s).

## Instrumentation Added to Tilemaker

Modified `data/tilemaker/src/tilemaker.cpp`:
- Added monotonic_ms() timer function
- Phase timing around: shp, pbf, collect, output, finalize
- Emits key=value pairs on stderr
- Removed 2s sleep on file overwrite
- Prints RSS via getrusage

## Fixes Applied

### 1. Simplification tolerance (DONE — 28.0s → 25.4s)
PIXEL_FACTOR=1.0 was divided by EXTENT (4096) but a rendered tile is 256 px wide.
Actual tolerance was 1/16 pixel, not 1 pixel. Renamed to SIMPLIFY_PIXELS, divide
by 256 instead. Vertex commands: 23.6M → 12.5M (now within 8% of TM's 11.5M).

### 2. Tile count reduction (DONE — 667K → 56K tiles, 25.4s → 24.0s)
Two changes:
- LandMask upgraded from z8 (8 KB) to z14 (32 MB) — per-tile precision
- Ocean-only tiles skipped in assemble phase reader thread
Output: 328 MB → 288 MB. Assemble: 3.7s → 2.5s.

### Final results (plantasjen, best of 3)
| Phase | Before | After fixes | + SortedNodeStore | Tilemaker |
|-------|--------|-------------|-------------------|-----------|
| PBF | 17.0s | 16.3s | **10.8s** | 14.6s |
| Ocean | 3.3s | 3.0s | **2.5s** | 5.2s |
| Sort | 0.7s | 0.4s | **0.4s** | 0.9s |
| Assemble | 4.6s | 2.5s | **2.3s** | 8.8s |
| **Total** | **28.0s** | **24.0s** | **16.6s** | **29.6s** |

Tiles: 56K (was 667K, TM 113K). Output: 286 MB (was ~380 MB, TM 308 MB).
**Now 1.8x faster than Tilemaker.**

## Completed Optimizations

### 1. Switch to libdeflate (DONE — 24.0s → 23.6s)
Replaced flate2 (zlib-ng) with libdeflater. Thread-local compressor reuse via `map_init`.

### 2. SortedNodeStore (DONE — 23.6s → 16.6s)
Replaced 96 GB sparse mmap with compact in-RAM hierarchical node store (~420 MB for
Denmark). 3-level bitmask+popcount lookup, same as Tilemaker's architecture. PBF phase
dropped from 16s to 10.8s — eliminated page faults entirely.

### ~~3. Visvalingam simplification~~ (tried and reverted)
VW's allocation overhead exceeds DP savings for small geometries (avg ~10 vertices).
Non-cascading produces +7 MB output. See `notes/vw-simplification-experiment.md`.

## Remaining Opportunities

### 1. StreamVByte delta compression for SortedNodeStore
Planet scale: 8.5B nodes × 8 bytes = 68 GB uncompressed. Won't fit in 64 GB RAM target.
Delta compression could bring it to ~40 GB. Not needed for extracts.
