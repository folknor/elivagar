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

Three-way comparison on Denmark (2026-02-23):

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| File size | 550 MB | 388 MB | 308 MB |
| Tiles | 1,328,874 | 104,394 | 113,476 |
| Time | ~5.5s | ~12-15s | ~30s |

Visual output verified identical across all three (nidhogg test suite).

### Done: Feature merging (Phase 1)

Simple multi-geometry merging implemented in `mvt.rs:merge_same_attr_geometries()`.
Groups features by `(geom_type, sorted tags)` and concatenates geometry commands
with delta-encoding adjustment. Called in `encode_tile_batch` before MVT encoding.

**Results:** Feature count reduced 97% (1.2M → 35K sampled features). File size
reduced 13% (630 MB → 550 MB). Geometry commands unchanged (same vertices).

### Remaining size gap: 550 MB vs 308-388 MB

The remaining gap is **geometry command volume** — we have ~3.3x more geometry
commands than Planetiler/Tilemaker. Root causes:

**1. Tile count (1.3M vs ~100-113K) — biggest contributor**

We emit ~1.2M more tiles than competitors, mostly ocean fill tiles at z7-z14.
The scanline fill in `ocean.rs` emits a fill tile for every tile inside an ocean
polygon at every zoom level. Each ocean polygon gets a different `feature_id`
(line 181: `idx as u64`), so even though fill tile geometry is identical (4096×4096
rectangle), the wire format includes different `osm_id` bytes, producing different
compressed data, preventing PMTiles content-hash dedup.

After merging, multiple ocean polygons' fills in the same tile merge to one feature
(since attrs are identical), so the encoded MVT should now be identical across fill
tiles. But fill_data still differs at the sort record level because of per-polygon
osm_id in the wire format.

**Fix options:**
- [ ] **Use a canonical fill_data for all ocean fills** — compute fill_data once
  with `osm_id=0` and reuse for every fill tile, regardless of source polygon.
  This ensures PMTiles dedup catches all identical ocean fills. The osm_id is
  irrelevant for fill tiles (no visible feature identity).
- [ ] **Use simplified ocean shapefile at z0-7** — like Tilemaker, use
  `simplified-water-polygons-split-3857` at z0-7 for far fewer source polygons
  and vertices. Reduces both tile count and geometry commands at low zooms.
  Requires downloading a second shapefile (~100 MB).

**2. Geometry commands at low zooms (ocean/water/land)**

Even after merging, ocean boundary tiles at z0-z7 have massive geometry command
counts because we clip the full-resolution shapefile at all zooms. Tilemaker uses
a simplified shapefile at z0-z7 with far fewer vertices.

Per-tile MVT sizes: z0 elivagar=19KB vs Tilemaker=888B (22x), z3 elivagar=190KB
vs Tilemaker=2.3KB (80x). By z14 they converge.

**Fix:** Use simplified shapefile at low zooms (see above).

**3. Extra tiles at z12-z14**

z14 alone: 990K tiles (elivagar) vs 85K (Tilemaker). The extra ~905K tiles are
pure ocean fill tiles. If dedup works correctly (fix #1 above), these should all
collapse to references to one shared tile blob, adding only ~10 bytes of PMTiles
directory overhead each instead of ~150 bytes of compressed tile data.

### Priority order

1. **Canonical ocean fill_data** — cheapest fix, ~10 lines in `ocean.rs`. Makes
   PMTiles dedup catch all fill tiles. Could save ~135 MB (905K tiles × ~150B each).
2. **Simplified ocean shapefile at z0-7** — reduces geometry commands dramatically
   at low zooms. Requires data pipeline change and second shapefile download.
3. **Gzip level tuning** — `Compression::fast()` (level 1) may leave 10-20% on
   the table. Benchmark levels 2-3.

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server
