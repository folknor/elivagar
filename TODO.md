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

## Performance

- [ ] Gzip level — `Compression::fast()` (level 1) may be too aggressive. Level 2-3 could
  give 10-20% smaller tiles at minimal extra CPU cost. Benchmark.

## Quality

- [ ] Feature merging (see below)
- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

## Feature merging

Post-decode merge pass in `encode_tile_batch`, per layer, before MVT encoding.
Reduces tile size by combining geometries that share identical attributes.

### Planetiler investigation (2026-02-23)

Planetiler's `FeatureMerge` class (`planetiler-core/.../FeatureMerge.java`) provides
several strategies, but **the Shortbread YAML profile uses none of them** — no
`tile_post_process` sections are defined. Only the OpenMapTiles Java profile does
merging. This means Planetiler's Shortbread output has the same unmerged features
as ours.

**Planetiler's merging strategies (for reference):**

1. **Simple multi-geometry** (`mergeMultiPoint/LineString/Polygon`): groups features
   by identical attributes and concatenates geometry commands. Cheapest option — no
   geometric computation, just command array concatenation. This is what we should
   start with.

2. **Linestring merging** (`mergeLineStrings` via `LoopLineMerger`): snap-rounds
   coordinates, splits intersecting lines, joins endpoints, removes stubs below a
   threshold, re-simplifies with Douglas-Peucker. Expensive — requires full JTS
   geometry decode/encode cycle.

3. **Polygon overlap resolution** (`mergeOverlappingPolygons`): unions
   overlapping/touching polygons via JTS. Filters by `minArea`.

4. **Polygon proximity merging** (`mergeNearbyPolygons`): clusters polygons within
   `minDist` pixels via STR-tree spatial index, buffers/unions/unbuffers to close
   gaps. Most expensive strategy.

**Typical parameters** (from OpenMapTiles profile):
- `minLength`: 0–0.5 px (linestrings shorter than this dropped after merge)
- `tolerance`: ~0.1 px (Douglas-Peucker re-simplification)
- `buffer`: 4.0 px (retain detail outside tile boundary)
- `minArea`: 4 sq px (drop tiny polygons after merge)

**`VectorTile.VectorGeometryMerger`** (inner class): concatenates MVT command arrays
into multi-geometries, adjusting delta-encoded coordinates. Used by simple
multi-geometry merging — this is the closest analog to what we'd implement.

### Priority assessment

Since Planetiler's Shortbread profile doesn't merge, our tile sizes should be
comparable. Feature merging would still reduce tile sizes (fewer feature headers,
better tag dedup), but it's an improvement over the baseline, not catching up.

**Before implementing:** write a comparison tool to decode sample tiles from both
outputs and compare feature counts per layer per zoom. This quantifies the actual
gap and identifies which layers have the most mergeable features.

### Implementation plan

**Phase 1: Simple multi-geometry merging (cheap, no geometry library)**

Group features by `(geom_type, sorted tags)` within each layer. Concatenate
geometry commands for features in the same group into a single multi-geometry
feature. Drop the `id` field (no single OSM ID applies to merged features).

- **Polygon merging**: each polygon ring is self-contained
  (MoveTo+LineTo+ClosePath), so rings from different features can be concatenated
  directly into one geometry command buffer.
- **Linestring merging**: each linestring is MoveTo+LineTo, so multiple
  linestrings concatenate directly into a multi-linestring (multiple MoveTo
  segments in one geometry).
- **Point merging**: multiple points concatenate into a multi-point (single
  MoveTo command with count > 1, but needs coordinate re-delta-encoding).

MVT delta encoding caveat: each feature's geometry starts with absolute
coordinates (cursor resets per feature). When concatenating into a single feature,
only the first geometry starts absolute — subsequent geometries need their
initial MoveTo adjusted to be relative to the running cursor position.

Layers to merge: `water_polygons`, `land`, `sites`, `buildings`, `streets`,
`water_lines`, `ferries`, `bridges`, `tunnels`.

Skip: `pois`, `places`, `addresses`, `boundary_labels`, `street_labels_points`,
`public_transport` — per-feature identity matters.

**Phase 2: Endpoint-joining linestring merge (moderate effort)**

Within same-attribute linestring groups, build adjacency graph on endpoints.
Join chains where one linestring's last point matches another's first point.
Saves one MoveTo+coordinates per join. Requires tracking absolute cursor
position through delta-encoded commands.

**Phase 3: Advanced merging (future, may not be needed)**

Snap-rounding, intersection splitting, polygon union. Would need a geometry
library (equivalent to JTS). Only worth it if Phase 1-2 leave significant gaps.

### Where it fits in the pipeline

In `encode_tile_batch`, after `add_feature_to_layer` populates the `LayerBuilder`
and before `mvt::encode_tile`. New function `merge_features(layer: &mut LayerBuilder)`
operates on the `features: Vec<Feature>` directly. This keeps the sort and wire
format untouched — merging is purely a tile-assembly optimization.

Sort key already orders features by `(tile_id, layer, priority)`, so same-layer
features are adjacent in the input stream and land in the same `LayerBuilder`.

### What NOT to merge

- Features across different layers
- Features with different `geom_type`
- Features where per-feature `id` matters (POIs, places, addresses, labels)
- Polygons with inner rings that belong to different outer rings (would
  need topology-aware merging, not worth the complexity)
