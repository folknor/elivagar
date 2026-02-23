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

- [ ] Railway zoom inverted — `railway=rail` with `service` tag gets zoom 8 (more prominent)
  while mainline rail (no service tag) gets zoom 10. **Verify against Planetiler's Shortbread
  profile before changing.** (`shortbread.rs:884`)
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

## Code quality

- [x] `LAYER_NAMES` in pipeline.rs duplicates `Layer::name()` in shortbread.rs — sync hazard.
  Fixed: removed `LAYER_NAMES`, pipeline uses `Layer::ALL[idx].name()` directly.
- [x] `Layer::count()` returns magic number 26 — derive from enum.
  Fixed: `Layer::ALL` const array, `count()` returns `ALL.len()`.
- [ ] Remove dead code: `_attr_float` (shortbread.rs:258), `encode_multi_point` (mvt.rs:179),
  `LayerBuilder::clear` (mvt.rs:68)
- [ ] Test-only functions exposed as `pub` — `tiles_for_bbox`, `project_bbox`, `reverse_ring`,
  `is_ccw`, `is_cw` in geometry.rs should be `#[cfg(test)]`
- [ ] Over-exposed modules in lib.rs — 11 modules are `pub` but only `run()` + `TilegenConfig`
  are the public API. Make internals `pub(crate)`.
- [ ] Extract ring close+orient helper — same pattern repeated 4 times in pipeline.rs and
  ocean.rs (close ring, ensure CW/CCW)
- [ ] `run()` should return `Result` — currently panics on I/O errors mid-run
- [ ] `TilegenConfig.skip_to: Option<String>` → `Option<SkipTo>` enum for type safety
- [ ] `MemberWay.role: String` → enum `{ Outer, Inner, Other }` to avoid string allocs
- [ ] `assert!` in `load_checkpoint` — use proper error handling (`pipeline.rs:169`)

## Performance

### High impact, low effort

- [ ] Sort chunk `BufReader` — default 8KB buffer, increase to 256KB+. Sort read is the
  serial bottleneck in Phase 4. (`sort.rs:193`)
- [ ] `new_layer_slots()` — `Vec<Option<LayerBuilder>>` → `[Option<LayerBuilder>; 26]`,
  avoids heap alloc per tile (`pipeline.rs:1032`)
- [ ] Per-geometry `Vec<u32>` allocs in `mvt::encode_point/linestring/polygon` — millions of
  unnecessary heap allocs. Return arrays for points, take `&mut Vec<u32>` for lines/polygons.

### High impact, more effort

- [ ] Double-buffer Phase 4 — overlap sort read, rayon encode, and PMTiles write. Currently
  the sort reader is serial, rayon waits for previous batch writes before starting next batch.
- [ ] Reuse encode buffers — `encode_attrs_bytes` allocates per feature, MVT protobuf
  `encode()` creates nested scratch Vecs (`layer_buf`, `feat_buf`, `val_buf`, `packed`).
  Use thread-local or passed-in buffers.
- [ ] SmallVec for `LayerMatch` vec and `attrs` vec — most elements match 1-3 layers with
  1-8 attrs, avoids heap alloc for common case (`shortbread.rs:147,155`)
- [ ] SmallVec for `tags_vec` per PBF element — typically 2-10 tags (`pipeline.rs:234`)
- [ ] `WayIndex::get` allocates Vec per call — return slice over mmap instead (`way_index.rs:124`)

### Medium impact

- [ ] Gzip level — `Compression::fast()` (level 1) may be too aggressive. Level 2-3 could
  give 10-20% smaller tiles at minimal extra CPU cost. Benchmark.
- [ ] Batch relations for parallel processing like ways — currently fully serial
  (`pipeline.rs:318`)
- [ ] `madvise` hints on mmap — `MADV_SEQUENTIAL` for write phase, `MADV_RANDOM` for
  relation read phase (`node_index.rs`, `way_index.rs`)

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
