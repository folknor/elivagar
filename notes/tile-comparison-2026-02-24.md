# Output size analysis: elivagar vs Planetiler vs Tilemaker

Dataset: Denmark (`denmark-latest.osm.pbf`, 483 MB). All runs at gzip level 6,
z0-14, Shortbread schema. Ocean shapefile: `water-polygons-split-3857`.

Supersedes `tile-comparison-2026-02-23.md` (raw feature count data).

## Final results (after all fixes)

**With ocean:**

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| File size | **380 MB** | 406 MB | 308 MB |
| Addressed tiles | 667,547 | 104,394 | 113,476 |
| Unique tiles | ~54K | 50,083 | 51,250 |
| Time (Denmark) | **27s** | 41s | 29s |

**Without ocean:**

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| File size | **317 MB** | 406 MB | 308 MB |
| Unique tiles | ~54K | 50,083 | 51,250 |

elivagar is now **smaller than Planetiler** in both configurations.

## Progress summary

| Milestone | With ocean | No ocean | vs Planetiler | vs Tilemaker |
|---|---|---|---|---|
| Original baseline | 630 MB | — | +62% | +115% |
| After feature merging | 457 MB | 347 MB | +18% | +56% |
| After Causes 1-4 | 412 MB | 317 MB | +6% | +41% |
| After land mask | **380 MB** | **317 MB** | **-2%** | +30% |

Total reduction: 630 → 380 MB (**-40%** with ocean), gap vs Planetiler eliminated.

## What was fixed

### Feature merging (630 → 550 → 457 MB)

Multi-geometry merging in `mvt.rs:merge_same_attr_geometries()`. Groups features
by `(geom_type, sorted tags)` and concatenates geometry commands. Sampled feature
count reduced 97% (1.2M → 35K). Combined with gzip level 6 (from level 1) and
canonical `osm_id=0` for ocean fill tiles, brought output from 630 to 457 MB.

Also fixed: PMTiles run-length encoding (was producing corrupt runs), madvise
regression (Sequential hints on random-access mmaps caused 2.3x slowdown),
simplified ocean shapefile support (`--ocean-simplified` for z0-7).

### Cause 1: Sub-pixel feature filtering (347 → 326 MB no-ocean)

Planetiler drops polygons < 1 sq pixel and lines < 1 pixel diagonal at all zooms
below max. Tilemaker has per-layer area-based filtering with exponential zoom scaling.

**Fix:** Drop sub-pixel features at z0-z13, exempt boundaries and streets (matching
Planetiler). At z14: no filtering (preserve for overzooming).

Impact: -21 MB no-ocean, -1.4M features, -31% sort time.

### Cause 2: Simplification tolerance (326 → 317 MB no-ocean)

| | elivagar | Planetiler | Tilemaker (streets, z10) |
|---|---|---|---|
| Tolerance | **1.0 px** (DP) | 0.1 px (DP) | ~14 px (DP, degree-based) |

`PIXEL_FACTOR` increased from 0.375 to 1.0. Still conservative vs Tilemaker
(10-40x more aggressive at mid zooms). Further gains would require per-layer
tuning or Visvalingam algorithm.

Impact: -9 MB no-ocean.

### Cause 3: Redundant boolean attributes (–2.75 MB)

Was emitting `rail=false`, `tunnel=false`, `bridge=false`, etc. on every street
feature. Fixed to only emit when true.

### Cause 4: Duplicate vertex removal (–0.6 MB)

Skip consecutive duplicate points after integer coordinate quantization. Drop
degenerate geometries (collapsed lines/rings). 36K degenerate features eliminated.

### Land tile mask (412 → 380 MB with-ocean)

Ocean processing emitted tiles for the entire bounding box of each ocean polygon
intersecting `data_bounds`. Denmark's PBF includes the Faroe Islands, stretching
bounds across the North Sea → ~109K extra ocean tiles in empty ocean.

**Fix:** Z8 land tile mask — 256×256 atomic bitset (8 KB) populated during PBF
processing, filtering ocean tile emission. Tiles whose z8 ancestor has no land
features are skipped.

Impact: -32 MB (-7.8%), addressed tiles cut 50%, ocean processing 25% faster.

## Three-way encoding comparison

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| Gzip | level 6 (flate2/zlib-ng) | level 6 (Java deflate) | level 6 (libdeflate) |
| Simplification | 1.0 px DP, all layers | 0.1 px DP, all layers | degree-based, per-layer, exponential zoom scaling |
| Min polygon size | 1 sq pixel (z0-z13) | 1 sq pixel | per-layer area-based zoom filtering |
| Feature merging | multi-geom concat | none (Shortbread YAML) | `combine_below` (line/poly union) |
| Dup vertex removal | yes | yes | yes |
| Boolean attrs | only when true | zoom-gated | only when true |
| Tile extent | 4096 | 4096 | 4096 |

## Remaining gap vs Tilemaker (24 MB)

The 317 vs 293 MB gap (no-ocean) comes from Tilemaker being more aggressive on
every axis:

1. **Simplification** — 10-40x more aggressive at mid-zooms (per-layer degree-based
   with exponential zoom scaling vs our constant 1.0 px)
2. **Feature merging** — proper geometric union (`combine_below`) vs our multi-geom
   concatenation (same feature count, but Tilemaker can eliminate shared edges)
3. **Area filtering** — per-layer `zmin_for_area()` in Lua filters small features
   dynamically based on pixel coverage at each zoom
4. **Compression** — libdeflate may produce 1-3% better ratios than zlib-ng

Closing this gap further would require per-layer simplification tuning or switching
to Visvalingam. The cost/benefit is diminishing — 24 MB is an 8% gap.

## Raw feature count data

### Grand totals: elivagar vs Planetiler (pre-fix, sampled tiles)

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

### Grand totals: elivagar vs Tilemaker (pre-fix, sampled tiles)

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

### Per-zoom ocean features (pre-fix)

| Zoom | elivagar | Planetiler | Tilemaker |
|------|----------|------------|-----------|
| z0 | 644 | 10 | 13 |
| z1 | 1,199 | 11 | 14 |
| z2 | 2,937 | 14 | 16 |
| z3 | 7,571 | 14 | 18 |
| z4 | 16,036 | 27 | 29 |
| z5 | 28,267 | 40 | 46 |
| z6 | 10,580 | 44 | 46 |
| z7 | 13,577 | 75 | 74 |
| z8 | 8,900 | 113 | 152 |
| z10 | 8,360 | 226 | 318 |
| z11 | 2,220 | 194 | 215 |
| z13 | 192 | 81 | 89 |
| z14 | 139 | 65 | 74 |

### Per-zoom unique tile breakdown

| Zoom | elivagar (ocean) | elivagar (no ocean) | Planetiler | Tilemaker |
|------|-----------------|-------------------|------------|-----------|
| z0   | 1               | 1                 | 1          | 1         |
| z1   | 2               | 1                 | 1          | 1         |
| z2   | 2               | 1                 | 1          | 1         |
| z3   | 2               | 1                 | 1          | 1         |
| z4   | 5               | 2                 | 2          | 2         |
| z5   | 11              | 4                 | 4          | 4         |
| z6   | 30              | 4                 | 4          | 4         |
| z7   | 102             | 9                 | 12         | 11        |
| z8   | 337             | 26                | 32         | 31        |
| z9   | 763             | 75                | 106        | 105       |
| z10  | 1,957           | 499               | 339        | 338       |
| z11  | 5,042           | 1,374             | 1,100      | 1,094     |
| z12  | 13,635          | 3,947             | 3,389      | 3,355     |
| z13  | 37,339          | 11,550            | 10,567     | 10,427    |
| z14  | 103,755         | 36,367            | 34,591     | 35,887    |
| **Total** | **162,926** | **53,861**    | **50,083** | **51,250** |

### Per-zoom unique tile sizes (elivagar, pre-fix, gzip 6)

| Zoom | Unique tiles | Unique MB | Avg size |
|------|-------------|-----------|----------|
| z0-z6 | 55        | 0.7 MB    | 13 KB    |
| z7   | 102         | 2.4 MB    | 24 KB    |
| z8   | 337         | 6.8 MB    | 21 KB    |
| z9   | 763         | 10.7 MB   | 14 KB    |
| z10  | 1,957       | 25.2 MB   | 13 KB    |
| z11  | 5,042       | 35.9 MB   | 7.3 KB   |
| z12  | 13,635      | 49.7 MB   | 3.7 KB   |
| z13  | 37,339      | 81.0 MB   | 2.2 KB   |
| z14  | 103,755     | 222.4 MB  | 2.2 KB   |

## How other tools handle ocean

### Data sources

All tools consume data from osmdata.openstreetmap.de (produced by OSMCoastline):

| Dataset | Description | Typical use |
|---------|-------------|-------------|
| `water-polygons-split-3857` | Full-resolution, split on 1x1 degree grid | z6-14 |
| `simplified-water-polygons-split-3857` | Simplified, split | z0-5 |
| Natural Earth (`ne_110m/50m/10m_ocean`) | Very coarse global outlines | z0-5 |

### Per-tool approaches

**Planetiler (Shortbread profile):**
- Uses `water-polygons-split-3857` for all zoom levels (no Natural Earth)
- Stripe clipping algorithm (geojson-vt derived): `sliceX` then `sliceY`
- `IntRangeSet` tracks fully-covered tile ranges
- Duplicate tile detection: skip re-encoding identical ocean-only tiles

**Tilemaker (Shortbread config):**
- z0-7: `simplified-water-polygons-split-4326` (separate simplified shapefile)
- z8-14: `water-polygons-split-4326` (full resolution)
- Exponential simplification: tolerance increases at lower zooms

**elivagar:**
- Uses `water-polygons-split-3857` for all zoom levels (+ optional simplified for z0-7)
- Scanline fill: rasterize polygon edges → identify boundary tiles → fill gaps
- Cascading simplification: z14 down, each zoom simplified from previous
- Pre-computed fill tiles: one MVT encoding reused for all fully-covered tiles
- Z8 land tile mask: skip ocean tiles where no land features exist
