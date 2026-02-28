*Note: Script references below predate the dev tool. Use dev for current equivalents.*

# Tile comparison: elivagar vs Planetiler vs Tilemaker (Denmark, 2026-02-23)

Comparison tool: `examples/compare_tiles.rs` / `scripts/compare-tiles.sh`

## Files

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| File | `denmark-latest.pmtiles` | `planetiler-bench.pmtiles` | `tilemaker-bench.pmtiles` |
| Size | 630 MB | 388 MB | 308 MB |
| Tiles | 1,328,874 | 104,394 | 113,476 |
| Zoom | 0–14 | 0–14 | 0–14 |
| Time (Denmark) | ~5.5s | ~12-15s | ~30s |

elivagar produces the **largest file** (+62% vs Planetiler, +105% vs Tilemaker) with
**12.7x more tiles** than Planetiler and **11.7x more** than Tilemaker.

## Method

For each zoom level, find tiles that exist in both archives (matched by Hilbert
tile ID), sample up to 200, decode the MVT protobuf, and count features per
layer. Some tiles at z9-z14 had read errors from the Planetiler file (PMTiles
directory decoding edge case) — those were skipped.

## Grand totals (all sampled tiles across all zoom levels)

```
layer                    elivagar   planetiler   diff      cmds_A     cmds_B
------------------------------------------------------------------------------
addresses                    6605       7017      +6%      19815      21051
boundaries                   8500        765     -91%     603076      16166
boundary_labels              4640         32     -99%      13920         96
bridges                       108         61     -44%       1449        763
buildings                    9518      10152      +7%     170893     178483
dam_lines                       1          0   only A          8          0
ferries                       585        603      +3%       6824       6366
land                       651318     255942     -61%   18348193    8082674
ocean                      109809       1060     -99%    3895267     651012
pier_lines                    779        377     -52%       8016       2928
pier_polygons                 198         39     -80%       3919        991
place_labels                 9916      10486      +6%      29748      31458
pois                          709        512     -28%       2127       1536
public_transport              135        142      +5%        405        426
sites                         140        144      +3%       2181       2118
street_labels               10836       9101     -16%     136200      92996
street_labels_points           13         15     +15%         39         45
street_polygons                24         24      +0%        662        622
streets                    137894     154395     +12%    1417504    1234626
streets_polygons_labels         2          2      +0%          6          6
water_lines                  3708       2524     -32%     166218      92760
water_lines_labels            274        239     -13%      19962      16932
water_polygons             285135      13397     -95%    4980145     479843
water_polygons_labels          6         13    +117%         18         39
------------------------------------------------------------------------------
TOTAL                    1240853     467042     -62%   29826595   10913937
```

elivagar emits **2.66x more features** overall and **2.73x more geometry
commands** than Planetiler for the same tiles.

## Grand totals: elivagar vs Tilemaker

```
layer                    elivagar   tilemaker    diff      cmds_A     cmds_B
------------------------------------------------------------------------------
addresses                    8282       2776     -66%      24846      19340
aerialways                      1          1      +0%          6          6
boundaries                  11189        598     -95%     857816     101468
boundary_labels              5565          0   only A      16695          0
bridges                       117         96     -18%       1556       1259
buildings                   11857      11850      -0%     216027     217271
dam_lines                       2          2      +0%         20         14
ferries                       539        510      -5%       6520       7154
land                       794588     469614     -41%   22494418    8656371
ocean                      111655       1522     -99%    4515399     376405
pier_lines                    825        110     -87%       8030       3850
pier_polygons                 191        114     -40%       3838       1689
place_labels                10160      10133      -0%      30480      30453
pois                         1017        436     -57%       3051       1928
public_transport              116       1874   +1516%        348       9752
sites                         259        257      -1%       4095       4259
street_labels               10874       2266     -79%     137260      39890
street_labels_points           30         22     -27%         90         82
street_polygons                19          5     -74%        399         77
streets                    183834       9161     -95%    1782106     601160
streets_polygons_labels         6          1     -83%         18          7
water_lines                  7659        962     -87%     335598      69546
water_lines_labels            305        136     -55%      20436       4010
water_polygons             398255      58193     -85%    7171145    1360781
water_polygons_labels          10         17     +70%         30         51
------------------------------------------------------------------------------
TOTAL                     1557355     570656     -63%   37630227   11506823
```

elivagar emits **2.73x more features** and **3.27x more geometry commands**
than Tilemaker for the same tiles.

## Three-way summary

| Layer | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| ocean | 111,655 | 1,060 | 1,522 |
| water_polygons | 398,255 | 13,397 | 58,193 |
| land | 794,588 | 255,942 | 469,614 |
| streets | 183,834 | 154,395 | 9,161 |
| boundaries | 11,189 | 765 | 598 |
| **TOTAL** | **1,557,355** | **467,042** | **570,656** |

**Key observations:**
- Tilemaker has the **smallest output** (308 MB) — even smaller than Planetiler (388 MB)
- Tilemaker's **simplified ocean shapefile at z0-7** is very effective (1,522 ocean
  features vs our 111,655)
- Tilemaker has **far fewer streets** at low-mid zooms (9K vs our 184K) — stricter
  zoom thresholds in the Lua config
- **public_transport**: Tilemaker includes 16x more (+1516%) — broader inclusion rules
- **z14 convergence**: at z14, most layers match closely (buildings -0%, land -0%,
  streets -1%, water_polygons +0%)

## The big three: ocean, water_polygons, land

Three polygon layers account for the vast majority of the difference:

| Layer | elivagar | Planetiler | Ratio | Geom cmds ratio |
|---|---|---|---|---|
| ocean | 109,809 | 1,060 | **104x** | 6.0x |
| water_polygons | 285,135 | 13,397 | **21x** | 10.4x |
| land | 651,318 | 255,942 | **2.5x** | 2.3x |

These three layers alone account for **1,046,262 features** in elivagar vs
**270,399** in Planetiler — a gap of **775,863 features**.

### Per-zoom breakdown

#### Ocean

| Zoom | elivagar | Planetiler | Tilemaker | vs Planetiler | vs Tilemaker |
|------|----------|------------|-----------|---------------|--------------|
| z0 | 644 | 10 | 13 | 64x | 50x |
| z1 | 1,199 | 11 | 14 | 109x | 86x |
| z2 | 2,937 | 14 | 16 | 210x | 184x |
| z3 | 7,571 | 14 | 18 | 541x | 421x |
| z4 | 16,036 | 27 | 29 | 594x | 553x |
| z5 | 28,267 | 40 | 46 | 707x | 614x |
| z6 | 10,580 | 44 | 46 | 240x | 230x |
| z7 | 13,577 | 75 | 74 | 181x | 183x |
| z8 | 8,900 | 113 | 152 | 79x | 59x |
| z10 | 8,360 | 226 | 318 | 37x | 26x |
| z11 | 2,220 | 194 | 215 | 11x | 10x |
| z13 | 192 | 81 | 89 | 2x | 2x |
| z14 | 139 | 65 | 74 | 2x | 2x |

The ratio is worst at low zooms (z3-z5: 400-600x). At z14 it's only 2x.
Tilemaker uses a **simplified ocean shapefile** at z0-7, giving it feature
counts very close to Planetiler. Both emit ~10-75 ocean features where we
emit hundreds to thousands.

#### Water polygons

| Zoom | elivagar | Planetiler | Tilemaker |
|------|----------|------------|-----------|
| z4 | 2,040 | 5 | 36 |
| z7 | 65,669 | 185 | 1,114 |
| z10 | 58,963 | 3,618 | 19,082 |
| z14 | 305 | 362 | 305 |

At high zooms the feature counts converge. Tilemaker has more water polygon
features than Planetiler at mid-zooms but still far fewer than us. At z14
all three tools produce ~300-360 features.

#### Land

| Zoom | elivagar | Planetiler | Tilemaker |
|------|----------|------------|-----------|
| z7 | 133,069 | 2,464 | 17,293 |
| z9 | 146,606 | — | 90,687 |
| z12 | 21,434 | 21,890 | 21,248 |
| z14 | 2,283 | — | 2,281 |

At z12+ all three tools converge. Tilemaker has more land features at
low zooms than Planetiler but still far fewer than us at z7 (17K vs 133K).

## Layers where all three tools converge (z14)

| Layer | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| buildings | 11,857 | 10,152 | 11,850 |
| place_labels | 10,160 | 10,486 | 10,133 |
| land (z12+) | ~21,400 | ~21,900 | ~21,250 |
| streets (z14) | 5,297 | — | 5,264 |
| sites | 259 | 144 | 257 |
| water_polygons (z14) | 305 | 362 | 305 |

At z14 where all tools process the same high-resolution data, feature counts
are nearly identical. Differences are from tag matching rules and zoom thresholds.

## Notable Tilemaker differences

| Layer | elivagar | Tilemaker | Notes |
|---|---|---|---|
| streets | 183,834 | 9,161 | -95% — Tilemaker has much stricter low-zoom thresholds |
| street_labels | 10,874 | 2,266 | -79% — same pattern |
| public_transport | 116 | 1,874 | +1516% — Tilemaker includes far more transit features |
| addresses | 8,282 | 2,776 | -66% — Tilemaker only emits at z14 |
| boundary_labels | 5,565 | 0 | Tilemaker doesn't emit this layer in sampled tiles |
| pier_lines | 825 | 110 | -87% — different zoom ranges |

## Root cause analysis

### Not a merging problem

Planetiler's Shortbread profile does **no feature merging** (no
`tile_post_process` in the YAML). The feature count difference is NOT
because Planetiler merges and we don't.

### Polygon-per-tile emission at low zooms

The core issue: when a large polygon (ocean, lake, land mass) spans many
tiles, elivagar clips it to each tile and emits one feature per tile per
polygon fragment. At z0, the world ocean gets clipped into 644 separate
polygon features across the single tile. Planetiler somehow keeps this to
just 10 features.

**Likely Planetiler approach:** Planetiler probably generates ocean/water
polygons as large multi-polygon features covering many tiles, then clips
them during tile encoding. Or it uses polygon simplification and merging
during the ocean generation phase to produce far fewer, coarser polygons
at low zooms.

**Our approach** (in `ocean.rs`): the ocean shapefile is processed with a
scanline fill algorithm that generates individual polygon features per
tile. Each tile gets its own set of ocean polygon fragments.

### Boundary feature duplication

boundaries: 8,500 vs 765 (11x). This suggests we're emitting the same
boundary linestring into every tile it crosses, while Planetiler may be
deduplicating or only emitting to the primary tile.

### Impact on file size

The 630 MB vs 388 MB gap (242 MB, +62%) is likely dominated by the
polygon layers at low zooms. Even though gzip compresses well, having
100x more features means 100x more protobuf feature headers, tag arrays,
and geometry command sequences — most of which compress poorly because
they contain unique coordinate data.

## Recommendations

### Priority 1: Multi-polygon merging at tile assembly

A single change that fixes ocean, water_polygons, and land simultaneously:
group same-attribute polygon features per tile into a single multi-polygon.
This is purely a tile-assembly optimization — no new data sources needed.

**Expected impact:**
- Ocean: 111K → ~1.3K features (matching Planetiler/Tilemaker)
- Water polygons: 398K → ~58K features (matching Tilemaker)
- Land: 795K → ~470K features (matching Tilemaker)
- Total feature reduction: ~40-50%

This won't reduce geometry commands (same vertices), but eliminates
per-feature protobuf overhead and should significantly reduce file size.

### Priority 2: Use simplified ocean shapefile at z0-7

Like Tilemaker, use `simplified-water-polygons-split-3857` for z0-7 and
the full shapefile for z8-14. This reduces geometry commands as well as
features — the simplified polygons have far fewer vertices.

**Expected impact:** Combined with Priority 1, should bring ocean geometry
commands from 4.5M down to ~376K (matching Tilemaker).

### Priority 3: Fix boundary duplication

11K vs 598 (Tilemaker) / 765 (Planetiler) — 15-19x more boundary features.

### Priority 4: Review street zoom thresholds

Our 184K streets vs Tilemaker's 9K suggests we emit streets at much lower
zooms. Tilemaker's stricter thresholds contribute to its smaller file size.
Review whether our low-zoom street inclusion matches the Shortbread spec.

---

## How other tools handle ocean polygons

### Data sources

All tools consume data from osmdata.openstreetmap.de, produced by
OSMCoastline. Three key datasets:

| Dataset | Description | Typical use |
|---------|-------------|-------------|
| `water-polygons-split-3857` | Full-resolution, split on 1x1° grid | z6-14 |
| `simplified-water-polygons-split-3857` | Simplified, split | z0-5 |
| Natural Earth (`ne_110m/50m/10m_ocean`) | Very coarse global outlines | z0-5 |

The "split" variants contain thousands of small polygons (pre-split on a
1x1 degree grid) rather than continent-sized ocean polygons.

### Per-tool approaches

**Planetiler (OpenMapTiles profile):**
- z0-1: Natural Earth 110m (~1 ocean polygon per tile)
- z2-4: Natural Earth 50m
- z5: Natural Earth 10m
- z6+: OSM water polygons shapefile
- Stripe clipping algorithm (geojson-vt derived): `sliceX` then `sliceY`
  instead of per-tile clipping
- `IntRangeSet` tracks fully-covered tile ranges for large filled areas
- Duplicate tile detection: skip re-encoding identical ocean-only tiles
- Post-process polygon merging: `mergeOverlappingPolygons()` on water layer

**Planetiler (Shortbread profile):**
- Uses `water-polygons-split-3857` for ALL zoom levels (no Natural Earth)
- Still benefits from stripe clipping and IntRangeSet optimizations

**Tilemaker (Shortbread config):**
- z0-7: `simplified-water-polygons-split-4326` (separate simplified shapefile)
- z8-14: `water-polygons-split-4326` (full resolution)
- Both write to the same `ocean` output layer
- Exponential simplification: tolerance increases at lower zooms
  (`simplify_level * pow(simplify_ratio, max_zoom - current_zoom)`)

**OpenMapTiles (PostGIS):**
- Same Natural Earth tiering as Planetiler
- Creates 4 generalized tables with `ST_Simplify` (20/40/80/160m tolerance)
- Lower zooms query coarser tables

**Tilezen:**
- Natural Earth at z0-7, OSM at z8+
- Progressive area filtering (small water bodies hidden at low zooms)
- Polygon merging for overlapping features

### What elivagar does

- Uses `water-polygons-split-3857` for ALL zoom levels (z0-14)
- No Natural Earth, no simplified shapefile
- Scanline fill: rasterize polygon edges → identify boundary tiles →
  fill gaps with pre-computed full-tile rectangles
- Cascading simplification: z14 down, each zoom simplified from previous
- Pre-computed fill tiles: one MVT encoding reused for all fully-covered tiles

### Why elivagar has 100x more ocean features

The scanline fill correctly identifies that deep-ocean tiles need a single
fill feature. The problem is **boundary tiles** (tiles where the coastline
crosses): each polygon fragment from the pre-split shapefile that overlaps
a tile gets clipped and emitted as a **separate feature**. At z0, many of
the ~23,000 pre-split polygons overlap the single tile, each producing a
clipped fragment.

At z14, most tiles are either fully ocean (1 fill feature) or touched by
at most 1-2 coastline polygons, so the counts converge.

### Key insight: what to fix

The problem is NOT the fill tiles (those work great). The problem is
boundary tiles at low zooms where many pre-split shapefile polygons
overlap and each produces a separate feature. Options:

1. **Multi-polygon merging at tile assembly** — group all ocean polygon
   features in a tile into a single multi-polygon feature. Cheapest fix,
   reduces feature count to 1 per tile regardless of input polygon count.
   Does not reduce geometry command count (same vertices), but eliminates
   per-feature protobuf overhead (headers, tags).

2. **Use simplified shapefile at low zooms** — like Tilemaker, use
   `simplified-water-polygons-split-3857` for z0-7 and the full shapefile
   for z8-14. Fewer source polygons = fewer clipped fragments. Requires
   downloading a second shapefile.

3. **Use Natural Earth at z0-5** — like Planetiler/OpenMapTiles. Single
   coarse ocean polygon at low zooms. Requires downloading Natural Earth
   shapefiles.

4. **Merge source polygons before clipping** — pre-process the shapefile
   to union overlapping polygons per tile before clipping. Expensive
   geometry operation.

Option 1 (multi-polygon merging) is the best cost/benefit: it's entirely
a tile-assembly optimization, needs no new data sources, and reduces
ocean feature count from hundreds to 1 per tile.
