# elivagar TODO

## Release prep

- [x] Run clippy and fix all warnings
- [x] Extend test suite — wire format roundtrip, zoom filtering, boundary label thresholds
- [x] Add Cargo.toml metadata for crates.io (`description`, `repository`, `keywords`, `categories`, `readme`)
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
- [x] **[P0]** Geometry command count truncated to u16 — widened to u32 in wire format.
  Was `geom_cmds.len() as u16` (max 65,535 commands, overflow at 32,767 vertices).
  Coastlines/fjords at z14 routinely exceed this. Fixed: encode/decode now use u32,
  +2 bytes per sort record (negligible). Roundtrip tests verify the change.

- [ ] **[P1]** PMTiles dedup hash collision — **investigated, confirmed risk.**
  Both elivagar (`DefaultHasher`, 64-bit) and pmtiles-rs (`XxHash3_64`, 64-bit) use
  64-bit hashes with **no content verification** on match. (`pmtiles_writer.rs:111-124`)

  Birthday problem at planet scale (~300M unique tiles):
  P(collision) ≈ k²/2^65 ≈ (3×10⁸)²/(3.7×10¹⁹) ≈ **0.24%** — roughly 1-2 expected
  collisions per planet run. Failure mode: wrong tile content served, undetectable.

  go-pmtiles uses FNV-128a (128-bit), much safer. pmtiles-rs has the same 64-bit bug.

  **Fix options (cheapest first):**
  1. Verify `data.len() == dup_length` on hash hit (catches most collisions for free)
  2. Full content comparison on size match (catches all collisions, cost only on dedup hits)
  3. Upgrade to 128-bit hash (reduces collision probability by ~10⁹×)
- [x] Ocean layer missing from PMTiles metadata — `build_metadata` lists 25 layers, but
  tiles contain 26 including ocean. Confirmed Planetiler includes ocean z0-14.
  Fixed: `build_metadata` now derives from `Layer::ALL`.
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

### High impact, more effort

- [x] Double-buffer Phase 4 — 3-stage pipeline (reader thread → rayon encode → writer thread)
  using `std::thread::scope` + `sync_channel(1)`. Overlaps sort read and PMTiles write with
  CPU-bound encoding. Assemble phase 5.5s → 3.5s (skip-to-sort), 5.3s → 4.1s (full run)
  on Denmark.
- [x] Reuse encode buffers — MVT `encode()` scratch Vecs (`layer_buf`, `feat_buf`, `val_buf`,
  `packed`) now reused via `EncodeScratch` struct + rayon `map_init`. Benchmark-neutral on
  Denmark (assemble phase ~5.5s before and after — gzip dominates, mimalloc handles transient
  allocs well), but cleaner code. `encode_attrs_bytes` was already buffer-reused (P3).
- ~~SmallVec for `LayerMatch` vec, `attrs` vec, and `tags_vec`~~ — **Reverted.** Benchmarked
  on Denmark (best of 3): PBF phase 22.8s vs baseline 18.7-19.3s (~20% slower). `Attr` is
  ~40 bytes so `SmallVec<[Attr; 8]>` = ~320 bytes inline; `LayerMatch` containing that makes
  `SmallVec<[LayerMatch; 4]>` enormous. Extra stack memcpy outweighs saved heap allocs.
- [x] `WayIndex::get` returns zero-copy `&[(i32, i32)]` slice over mmap instead of allocating

### Medium impact

- [ ] Gzip level — `Compression::fast()` (level 1) may be too aggressive. Level 2-3 could
  give 10-20% smaller tiles at minimal extra CPU cost. Benchmark.
- [x] Batch relations for parallel processing like ways — split `process_relation` into
  `prepare_relation` (serial I/O: way_index lookups + projection) and
  `process_prepared_relation` (parallel CPU: multipolygon assembly + matching + clipping).
  Batched into `Vec<PreparedRelation>` (1024), flushed via rayon like ways.
  Tags owned as `Vec<(String, String)>` since PBF borrows don't survive batch boundary.
  Output byte-identical on Denmark. Impact on Denmark minimal (few relations), but
  critical at planet scale (hundreds of thousands of complex boundary/multipolygon relations).

- [x] `madvise` hints on mmap — `MADV_SEQUENTIAL` during write phase (node/way IDs
  increasing), `MADV_RANDOM` after `finish_writing()` for relation member lookups.
  Prevents wasted readahead when mmaps exceed available RAM at planet scale.

## Quality

- [ ] Feature merging (see below)
- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

## Feature merging

Post-decode merge pass in `flush_tile_batch`, per layer, before MVT encoding.
Reduces tile size by combining geometries that share identical attributes.

**Before implementing:** compare tile sizes and feature counts per layer per zoom
against Planetiler output to quantify the gap. Inspect Planetiler's `FeatureMerge`
and `VectorTile.mergeSortedFeatures` to understand their approach and which layers
they merge.

### Linestring merging

Group linestring features by their tag set (sorted `Vec<(u16, u16)>`). Within
each group, build an adjacency graph: two linestrings connect if one's endpoint
matches the other's startpoint (MVT delta-encoded coords, so compare absolute
cursor positions). Walk chains greedily to produce merged linestrings. The merged
feature drops the `id` (no single OSM ID applies) and concatenates the geometry
commands, replacing each subsequent MoveTo+LineTo with just LineTo.

Layers that benefit: `streets`, `street_labels`, `water_lines`, `ferries`,
`bridges`, `tunnels`. Skip layers where merging doesn't make sense (points,
labels with per-feature identity).

### Polygon merging

Group polygon features by tag set. Merge is simpler: combine into a single
multi-polygon by concatenating geometry commands (each polygon is already
MoveTo+LineTo+ClosePath, and MVT allows multiple rings per feature). No
adjacency detection needed — just batching same-attribute polygons into one
feature.

Layers that benefit: `water_polygons`, `land`, `sites`, `buildings` (at low
zooms if we ever add generalization).

### Where it fits in the pipeline

In `flush_tile_batch`, after `add_feature_to_layer` populates the `LayerBuilder`
and before `mvt::encode_tile`. New function `merge_features(layer: &mut LayerBuilder)`
operates on the decoded `Feature` vec. This keeps the sort and wire format
untouched — merging is purely a tile-assembly optimization.

### What NOT to merge

- Point features (no geometry to combine)
- Features with `id: Some(...)` where identity matters (POIs, places)
- Features across different layers
- Polygons with inner rings that belong to different outer rings (would
  need topology-aware merging, not worth the complexity)
