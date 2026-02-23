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

  **Planetiler's shortbread.yml** has the same inversion — the min_zoom override blocks
  assign `service: __any__` (service tag present) to the z8 block and `service: ''`
  (service tag absent) to the z10 block.

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
- [ ] PMTiles dedup hash collision — 64-bit content hash with no collision verification.
  ~1:50,000 chance of wrong tile content on planet-scale data. **Check how pmtiles-rs and
  Planetiler handle dedup.** (`pmtiles_writer.rs:111`)
- [ ] Geometry command count truncated to u16 — overflows at ~21K vertices, plausible for
  complex country boundaries. **Check actual max vertex counts in Planetiler Denmark output
  to see if this is hit in practice.** Either widen to u32 or detect and split.
  (`wire_format.rs:72`)
- [x] Ocean layer missing from PMTiles metadata — `build_metadata` lists 25 layers, but
  tiles contain 26 including ocean. Confirmed Planetiler includes ocean z0-14.
  Fixed: `build_metadata` now derives from `Layer::ALL`.
- [ ] `area_sq_meters` uses cos²(lat) approximation — 20-30% error for features spanning
  large latitude ranges (e.g. Norway). Affects `enrich_polygon_matches` boundary label zoom
  thresholds. **Compare boundary_labels min_zoom values against Planetiler for
  Scandinavia/Russia.** (`geometry.rs:482`)

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

- [ ] `madvise` hints on mmap — critical at planet scale (75GB PBF, 64GB RAM). Without
  hints, the kernel readaheads pages during random `way_index.get()` lookups in the
  relation phase, wasting I/O bandwidth on pages that immediately get evicted.

  **Where to add hints** (via `memmap2`'s `.advise()` method):
  | Phase | Index | Access | Hint |
  |-------|-------|--------|------|
  | PBF write | `node_index` mmap | Sequential (IDs increasing) | `MADV_SEQUENTIAL` |
  | PBF write | `way_index` offsets mmap | Sequential (IDs increasing) | `MADV_SEQUENTIAL` |
  | Way reads | `node_index.get()` | Roughly sequential (refs within ways) | `MADV_SEQUENTIAL` |
  | Relation reads | `way_index` offsets + data | Random (member way IDs arbitrary) | `MADV_RANDOM` |

  **Implementation:** `NodeIndex::create()` → `advise(Sequential)`. `WayIndex::create()`
  → `advise(Sequential)` on offsets. `WayIndex::finish_writing()` → `advise(Random)` on
  both offsets_mmap and data_mmap. (~5 lines of code total.)

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
