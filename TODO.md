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
| File size | 545 MB | 388 MB | 308 MB |
| Tiles | 1,328,874 | 104,394 | 113,476 |
| Unique tiles | 162,926 | — | — |
| Time | ~30s | ~12-15s | ~30s |

Visual output verified identical across all three (nidhogg test suite).

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

### Remaining size gap: 545 MB vs 308-388 MB

**Gzip compression level** — currently `Compression::fast()` (level 1). Testing
level 6 reduced output from 545 → 457 MB (16% saving) with +0.7s assemble time.

| | Level 1 | Level 6 | Delta |
|---|---|---|---|
| Output | 545 MB | 457 MB | -16% |
| Assemble | 6.7s | 7.4s | +0.7s |

Still 149 MB above Tilemaker (308 MB) at level 6. The remaining gap comes from:
- 49K more unique tiles (163K vs 113K) — mostly ocean boundary tiles at z8-14
  with more geometry commands than Tilemaker's equivalent tiles
- Higher average tile size (3.3 KB vs 2.7 KB per unique tile)

### Next steps

- [ ] **Gzip level tuning** — benchmark levels 2-6, find the sweet spot for
  size vs speed. Level 6 saves 16% for +0.7s.
- [ ] Investigate remaining 49K extra unique tiles vs Tilemaker at z8-14

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server
