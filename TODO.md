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

- [ ] Railway zoom inverted — **confirmed bug, also present in Planetiler's Shortbread YAML.**
  The Shortbread spec 1.0 (https://shortbread-tiles.org/schema/1.0/) says for `rail` and
  `narrow_gauge` in the streets layer: "ways with `service=*` on zoom level 10+, other ways
  on zoom level 8+." Mainline rail (no service tag) should be MORE prominent (z8), service
  tracks (sidings, yards) LESS prominent (z10). Both elivagar and Planetiler have it backwards.

  **Current elivagar code** (`shortbread.rs`, `railway_zoom` function):
  ```
  "rail" => {
      if tags.has("service") { Some(8) }   // BUG: service rail gets z8
      else                   { Some(10) }  // BUG: mainline rail gets z10
  }
  "narrow_gauge" => Some(10),  // BUG: should also split on service tag
  ```

  **Planetiler's shortbread.yml** (verified 2026-02-23, source:
  https://github.com/versatiles-org/planetiler-shortbread/blob/main/resources/config/shortbread.yml)
  has the same inversion — the min_zoom override blocks assign `service: __any__`
  (service tag present) to the z8 block and `service: ''` (service tag absent) to
  the z10 block:
  ```yaml
  # Planetiler's YAML (buggy):
  8:                          # ← service tracks get z8 (should be z10)
    __all__:
      railway: [ rail, narrow_gauge ]
      service: __any__
  10:                         # ← mainline gets z10 (should be z8)
    __all__:
      railway: [ rail, narrow_gauge ]
      service: ''
  ```

  **Correct values per spec:**
  | Feature                          | Spec min_zoom | elivagar | Planetiler |
  |----------------------------------|---------------|----------|------------|
  | `railway=rail` (mainline)        | **8**         | 10 (bug) | 10 (bug)   |
  | `railway=rail` + `service=*`     | **10**        | 8 (bug)  | 8 (bug)    |
  | `narrow_gauge` (mainline)        | **8**         | 10 (bug) | 10 (bug)   |
  | `narrow_gauge` + `service=*`     | **10**        | 10 (ok)  | 8 (bug)    |

  **Fix:** In `railway_zoom`, swap the zoom values for `rail` (service→10, mainline→8)
  and extend `narrow_gauge` with the same service-tag split. Note: this intentionally
  diverges from Planetiler output, matching the spec instead.
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
| Simplification | **0.375 px** DP, all layers | **0.1 px** DP, all layers | **degree-based**, per-layer, exponential zoom scaling |
| Min polygon size | **none** | **1 sq pixel** | per-layer area-based zoom filtering |
| Feature merging | multi-geom concat | **none** (Shortbread YAML) | `combine_below` (line/poly union) |
| Dup vertex removal | **no** | yes | yes |
| Boolean attrs | emits false values | zoom-gated | only when true |
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

#### Cause 1: No minimum polygon/line size filtering (dominant factor)

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

**Elivagar:** No equivalent. Every polygon fragment and line segment, no matter
how small, gets encoded into the MVT tile. This is likely the single largest
contributor — both Planetiler and Tilemaker filter aggressively, just with
different mechanisms.

- [ ] Add minimum-size filtering: drop polygon rings below 1 sq pixel and
  line segments below 1 pixel at zooms below max. This matches Planetiler's
  approach and is simpler to implement than Tilemaker's per-layer config.

#### Cause 2: Simplification tolerance vs Tilemaker

**Elivagar:** `PIXEL_FACTOR = 0.375` — tolerance is 0.375 sub-pixels in MVT
coordinate space, constant across all layers. Formula: `0.375 / (4096 * 2^zoom)`.

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
- 0.0012 / 8.5e-5 ≈ **14 pixels** — vs our **0.375 pixels** (37x more aggressive)

At z14, all three tools skip or minimize simplification, matching the tile
comparison showing convergence at z14.

**Impact:** Tilemaker's aggressive simplification at z7-z12 (10-40x more than us)
produces dramatically fewer geometry commands per feature. Combined with min-size
filtering, this accounts for the bulk of the gap. However, since Planetiler
achieves comparable size with even less simplification, min-size filtering alone
may close most of the gap.

- [ ] After implementing min-size filtering, re-measure the gap. If still
  significant, consider increasing `PIXEL_FACTOR` to 1.0 (still conservative
  vs Tilemaker, and 10x more aggressive than Planetiler's 0.1 px).

#### Cause 3: Redundant false-valued boolean attributes

Elivagar emits boolean attributes even when false: `rail=false`, `tunnel=false`,
`bridge=false`, `oneway=false`, `oneway_reverse=false` on streets; `rail=false`
on street_polygons. Each costs a key+value index pair in the protobuf tag array.
Tilemaker's Lua config only sets these when true — `Attribute("tunnel", "yes")`
is conditional, so the attribute is absent on non-tunnel features. Planetiler's
Shortbread YAML also zoom-gates these attributes (tunnel/bridge/link at z11+,
oneway at z14+).

On a layer like streets with many features, this adds up: 3-6 unnecessary boolean
tags per feature × thousands of features per tile.

- [ ] Only emit boolean attributes when true. Skip `rail`, `tunnel`, `bridge`,
  `oneway`, `oneway_reverse`, `surface`, `service` etc. when the value is false
  or empty.

#### Cause 4: No collinear vertex removal after clipping

Both Tilemaker and Planetiler filter consecutive duplicate points after scaling
to integer tile coordinates. Planetiler's `CommandEncoder` skips points where
`_x == x && _y == y`. Our Sutherland-Hodgman polygon clipper can produce
collinear vertices along clip edges that survive into the MVT encoding. Minor
but contributes extra geometry commands.

- [ ] Deduplicate consecutive identical points after coordinate scaling in MVT
  encoding. Optionally also remove collinear points (three consecutive points
  on the same line).

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
