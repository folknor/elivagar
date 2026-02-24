# elivagar TODO

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## GitHub

- [ ] Write GitHub repo description and tags (vector-tiles, openstreetmap, pmtiles, shortbread, rust)
- [ ] Add GitHub Actions CI — clippy, tests, `cargo build --release` on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Bugs

- [ ] **[P2]** `area_sq_meters` cos²(lat) approximation — **investigated, moderate risk.**
  Uses single centroid latitude for entire polygon (`geometry.rs:485-500`). Affects
  `enrich_polygon_matches` thresholds (2M/700K/100K km²) in `pipeline.rs:726-750`.

  **Error by latitude span:**
  | Feature | True area | Lat span | Centroid | Est. error | Threshold | Risk |
  |---------|----------|----------|----------|-----------|-----------|------|
  | Russia | 17.1M km² | 41°–82°N | 60°N | 20-30% under | 2M km² | Safe (>>2M) |
  | Canada | 10M km² | 42°–83°N | 62°N | 20-30% under | 2M km² | Safe (>>2M) |
  | Norway | 385K km² | 58°–71°N | 64°N | 15-25% under | 100K km² | Safe (>>100K) |

  **Real risk:** moderately-sized high-latitude regions near thresholds. A 700K km² region
  at 70°N (cos²≈0.12) could be underestimated to ~490K km², misclassifying from z3→z4.
  Unlikely for well-known countries but possible for sub-national boundaries.

  **Fix options:** per-edge latitude weighting (~2× cost), or latitude-range-aware cos²
  averaging (moderate cost, good tradeoff).

## Output size

### Three-way comparison on Denmark (2026-02-23)

Gzip level 6 throughout. Visual output verified identical across all three (nidhogg).

**With ocean:**

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| File size | 457 MB | 388 MB | 293 MB |
| Addressed tiles | 1,328,874 | 104,394 | 113,476 |
| Unique tiles | 162,926 | 50,083 | 51,250 |
| Time | ~32s | ~12-15s | ~30s |

**Without ocean** (elivagar run with no `--ocean` flag):

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| File size | 347 MB | 388 MB | 293 MB |
| Unique tiles | 53,861 | 50,083 | 51,250 |

Without ocean, elivagar is **smaller than Planetiler** and has a comparable unique
tile count to both. The ocean phase is the dominant source of the size gap.

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

Without ocean, tile counts match closely at z0-z9. At z10-z14, elivagar has
slightly more tiles (e.g. z14: 36K vs 35K) — likely from minor clipping or
simplification differences. Not a significant concern.

### Per-zoom unique tile sizes (elivagar with ocean, gzip 6)

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

z14 alone is 222 MB (49% of output). z13+z14 = 303 MB (66%).

### Problem 1: Ocean bbox flooding

**Root cause:** Ocean processing emits tiles for the entire PBF data bounds.
The Denmark PBF includes the Faroe Islands (~62°N, 7°W), which stretches the
bounding box to cover the entire North Sea. Ocean polygons fill this vast area
with boundary tiles (each unique due to clipped polygon geometry) and fill tiles
(deduplicated, but generating 1.2M directory entries).

This affects **any regional extract with outlier territory** — not just Denmark.
European country extracts commonly have overseas territories or distant islands.

At planet scale the bbox is the whole world anyway, so no "outlier" problem —
but the architectural issue remains: we generate ocean tiles where there are
no land features, producing tiles that Planetiler and Tilemaker never emit.

**How Planetiler/Tilemaker avoid this:** They process ocean per-tile during
assembly, not as a separate global phase. A tile only gets ocean if it also has
(or is adjacent to) land features. They never generate standalone ocean tiles
in empty ocean.

**Potential solutions:**

- [ ] **Per-tile ocean injection at assembly time** — Instead of emitting ocean
  as sort records in a separate phase, inject ocean geometry into tiles during
  the assemble phase when encoding each tile. For each tile that has land features,
  also clip the ocean shapefile to that tile. Conceptually simpler but requires
  the ocean shapefile to be available during assembly (currently assembly only
  reads sorted records).

- [ ] **Ocean tile mask** — After PBF processing, record which coarse-grid cells
  (e.g. z8) have land features. During ocean processing, only emit tiles within
  a radius of populated cells. Preserves the current architecture but adds
  coupling between phases.

**Open questions:**
- At planet scale, how much data do standalone ocean tiles (tiles with ONLY ocean,
  no land features) contribute? If it's significant, the architecture matters even
  at planet scale.
- Would per-tile ocean injection be too slow? Each tile would need a spatial lookup
  into the ocean shapefile. An R-tree or grid index over ocean polygons could make
  this fast.
- Can we keep the separate ocean phase but restrict it to only tiles that exist in
  the land feature set? This would require a two-pass approach (PBF first, then
  ocean guided by the PBF results).

### Problem 2: Per-tile size gap vs Tilemaker

Without ocean, elivagar produces 347 MB vs Tilemaker's 293 MB for ~54K unique
tiles each. Average tile size: 6.4 KB (elivagar) vs 5.7 KB (Tilemaker) — 12% larger.

Without ocean, elivagar (347 MB) is already **smaller than Planetiler** (388 MB).
The size gap is specifically vs Tilemaker.

**Root cause analysis** (verified against Tilemaker + Planetiler source, 2026-02-24):

#### Three-way encoding comparison

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| Gzip | level 6 (flate2/zlib-ng) | level 6 (Java deflate) | level 6 (libdeflate) |
| Simplification | **1.0 px** DP, all layers | **0.1 px** DP, all layers | **degree-based**, per-layer, exponential zoom scaling |
| Min polygon size | **1 sq pixel** (z0-z13) | **1 sq pixel** | per-layer area-based zoom filtering |
| Feature merging | multi-geom concat | **none** (Shortbread YAML) | `combine_below` (line/poly union) |
| Dup vertex removal | yes | yes | yes |
| Boolean attrs | only when true | zoom-gated | only when true |
| Tile extent | 4096 | 4096 | 4096 |

Key insight: **Planetiler is MORE conservative on simplification** (0.1 px vs our
0.375 px) yet produces a comparable-sized output. Planetiler compensates with its
**1 sq pixel minimum polygon size** — any polygon covering less than 1 tile pixel
is dropped at zooms below max. Also notable: Planetiler's Shortbread YAML has
**no post-processing at all** — no line or polygon merging. Our
`merge_same_attr_geometries` already does more than Planetiler.

The size gap vs Tilemaker comes from Tilemaker's more aggressive approach on
every axis: much more aggressive simplification, polygon area filtering, proper
geometric union (not just multi-geom concat), and no wasted boolean attributes.

#### Ruled out: compression

All three use gzip level 6. Tilemaker uses libdeflate 1.22, Planetiler uses
Java's built-in deflater, we use flate2/zlib-ng. libdeflate may produce slightly
better ratios (1-3%), but this is not the dominant factor.

#### Cause 1: No minimum polygon/line size filtering — DONE

**Planetiler:** Drops polygons smaller than 1 sq tile pixel and lines shorter
than 1 tile pixel at all zoom levels below max. At max zoom (z14), threshold
drops to 0.0625 px to allow overzooming. Boundaries and streets override to 0
(never filtered). This is a global default — the Shortbread YAML adds no
per-layer customization.

**Tilemaker:** Per-layer `filter_below` and `filter_area` config that removes
small polygon rings below a zoom threshold. The filter area scales exponentially
with zoom: `filter_area_degrees * pow(2.0, (filter_below - 1) - zoom)`.
Used on ocean (`filter_below: 12, filter_area: 0.5`), water, landuse, landcover.
Additionally, the Lua script computes per-feature min zoom based on area via
`zmin_for_area()` — water polygons are placed at the zoom where they first
cover a minimum number of pixels.

**Fixed:** Drop sub-pixel features at z0-z13 (matching Planetiler's approach):
- Lines: skip if bounding box diagonal < 1 pixel (16 extent units)
- Polygons: skip outer/inner rings with area < 1 sq pixel (shoelace formula)
- Exemptions: boundaries and streets (never filtered, matching Planetiler)
- At z14 (max zoom): no filtering (all features preserved for overzooming)

**Impact (Denmark no-ocean, cumulative):** -21.3 MB from original baseline
(before Cause 2 simplification change). See Cause 2 for final cumulative numbers.

| | Original baseline | After Causes 1+3+4 | After all (incl. Cause 2) |
|---|---|---|---|
| Output | 347.0 MB | 325.7 MB (-6.1%) | 316.7 MB (-8.7%) |
| Sort | 781 ms | 537 ms (-31%) | 471 ms (-40%) |
| Assemble | 2,493 ms | 2,165 ms (-13%) | 2,467 ms (-1%) |
| Features | 16,583,912 | 15,153,474 (-8.6%) | 15,149,684 (-8.6%) |

Remaining gap vs Tilemaker (293 MB): 24 MB, down from 54 MB (closed 56%).

#### Cause 2: Simplification tolerance vs Tilemaker — DONE

**Elivagar:** `PIXEL_FACTOR` increased from 0.375 to 1.0 — tolerance is now
1 pixel in MVT coordinate space, constant across all layers. Still conservative
vs Tilemaker (10-40x more aggressive at mid zooms), 10x more aggressive than
Planetiler's 0.1 px.

**Planetiler:** 0.1 px tolerance (Douglas-Peucker), also constant across all
layers. Even more conservative than us. This confirms simplification alone
doesn't explain the gap — Planetiler is more conservative yet comparable in size.

**Tilemaker:** Per-layer, degree-based tolerance with exponential zoom scaling.
Each layer has `simplify_below` (zoom threshold), `simplify_level` (base tolerance
in degrees), and `simplify_ratio` (exponential factor, default 2.0). Formula:
`simplify_level * pow(simplify_ratio, (simplify_below - 1) - zoom)`.

Tilemaker's Shortbread config values:

| Layer | simplify_below | simplify_level | algorithm |
|---|---|---|---|
| ocean | 13 | 0.0001 | visvalingam |
| water | 12 | 0.0003 | visvalingam |
| waterway | 12 | 0.0003 | douglas-peucker |
| landuse | 13 | 0.0003 | visvalingam |
| landcover | 13 | 0.0003 | visvalingam |
| boundary | 12 | 0.0003 | visvalingam |
| transportation | 13 | 0.0003 | douglas-peucker |

Example: `transportation` at z10, simplify_below=13:
- tolerance = 0.0003 * pow(2.0, 12 - 10) = 0.0012 degrees
- One MVT extent unit at z10 ≈ 8.5e-5 degrees
- 0.0012 / 8.5e-5 ≈ **14 pixels** — vs our **1.0 pixel** (14x more aggressive)

**Impact (Denmark no-ocean, cumulative with Causes 1+3+4):** -9.0 MB additional.

| | After Causes 1+3+4 | After all fixes | Delta |
|---|---|---|---|
| Output | 325.7 MB | 316.7 MB | -9.0 MB (-2.8%) |
| Features | 15,153,474 | 15,149,684 | -3,790 |
| Sort | 537 ms | 471 ms | -66 ms |
| Assemble | 2,165 ms | 2,467 ms | +302 ms |

Remaining gap vs Tilemaker (293 MB): 24 MB, down from 54 MB (closed 56%).
Tilemaker is still 14x more aggressive on simplification — further gains would
require per-layer tuning or a different algorithm (Visvalingam).

#### Cause 3: Redundant false-valued boolean attributes — DONE

Elivagar emitted boolean attributes even when false: `rail=false`, `tunnel=false`,
`bridge=false`, `oneway=false`, `oneway_reverse=false` on streets; `rail=false`
on street_polygons.

**Fixed:** Only emit boolean attrs when true on streets, street_polygons,
street_labels, water_lines, water_lines_labels. Recycling POI booleans
(semantically meaningful per spec) left unchanged.

**Impact (Denmark no-ocean):** -2.75 MB (-0.8%), sort -105 ms, assemble -69 ms.

#### Cause 4: Duplicate vertex removal after clipping — DONE

Both Tilemaker and Planetiler filter consecutive duplicate points after scaling
to integer tile coordinates.

**Fixed:** `encode_linestring` and `encode_polygon` in `mvt.rs` now skip
consecutive duplicate points. Degenerate geometries (lines collapsed to 1 point,
rings collapsed to < 3 unique points) are discarded.

**Impact (Denmark no-ocean, cumulative with Cause 3):** additional -0.6 MB,
-36,706 degenerate features dropped, -19 empty tiles eliminated.

**Combined (Cause 3 + 4):** -3.4 MB (-1.0%), 347.0 → 343.6 MB. No perf regression.

| | Baseline | After Cause 3+4 | Delta |
|---|---|---|---|
| Output | 347.0 MB | 343.6 MB | -3.4 MB (-1.0%) |
| Assemble | 2,493 ms | 2,366 ms | -127 ms (-5.1%) |
| Features | 16,583,912 | 16,547,206 | -36,706 |

### Done

**Feature merging** — multi-geometry merging in `mvt.rs:merge_same_attr_geometries()`.
Groups features by `(geom_type, sorted tags)` and concatenates geometry commands
with delta-encoding adjustment. Feature count reduced 97% (1.2M → 35K sampled),
file size reduced 13% (630 → 550 MB).

**Canonical ocean fill_data** — `osm_id=0` for all fill tiles. PMTiles dedup was
already catching them via the merge pass (162K unique either way), but this ensures
correctness regardless of merge order.

**Simplified ocean shapefile at z0-7** — `--ocean-simplified` flag uses the
30 MB simplified shapefile for z0-7 (9K polygons vs 219K full-res). Full-res
used for z8-14. Modest size impact (~5 MB on Denmark) since low-zoom tiles are
a small fraction of total data, but dramatically fewer geometry commands at z0-7.

**PMTiles run-length fix** — the `try_extend_run` function checked for consecutive
offsets, but PMTiles v3 spec says run_length means all tiles share the SAME data
(same offset). Old code produced corrupt runs for unique tiles and couldn't merge
dedup'd tiles. Fixed + corrected `num_tile_entries` header field.

**madvise regression fix** — `Advice::Sequential` on node/way index mmaps caused
2.3x PBF phase regression (20s → 46s on Denmark) by triggering aggressive readahead
during random-access ID lookups. Removed Sequential hints, kept Random hints after
`finish_writing()` for relation member lookups.

**Gzip level 6** — changed from `Compression::fast()` (level 1) to level 6.
Output reduced from 545 → 457 MB (16% saving) with +0.7s assemble time on Denmark.

| | Level 1 | Level 6 | Delta |
|---|---|---|---|
| Output | 545 MB | 457 MB | -16% |
| Assemble | 6.7s | 7.4s | +0.7s |

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server
