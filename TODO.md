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

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

---

- [ ] Generally check for updates to all dependencies.

## Planet-Scale Blockers

- [x] **PMTiles writer unbounded memory growth** — replaced `tiles: Vec<(u64, StoredTile)>` with
  incremental `DirStore` (memory or streaming to disk). Run-length encoded `DirEntry` built on the
  fly in `push_dir_entry()`. Dedup HashMap capped at 1M entries (`MAX_DEDUP_ENTRIES`). Tile data
  optionally streamed via `TileBlob::File`. Saves ~12 GB at planet scale.

- [ ] **Node index virtual memory at planet scale** — OSM planet has ~8.5B nodes with IDs up to ~12B, so the index file grows to ~96 GB. On a 64GB machine, page cache will thrash. Consider a two-level index (blocks of 4096 nodes with top-level pointer array), or call `madvise(MADV_SEQUENTIAL)` during write phase and `MADV_RANDOM` for lookups. (`node_index.rs`)

## Performance: Allocation Pressure (High Impact)

- [ ] **SortRecord `data: Vec<u8>` — billions of small heap allocs** — Every sort record owns a separate `Vec<u8>`. Use an arena allocator or a single large buffer per chunk, storing offset+length pairs in SortRecord instead of individual Vecs. (`sort.rs:47`)

- [ ] **Per-feature Vec allocations in PBF callback** — `node_records`, `coords_e7`, and `merc` projection Vecs are allocated per-element (billions of times). Hoist outside the closure and reuse via clear-between-iterations, or use thread-local reusable buffers. (`pipeline.rs:320-567`)

- [ ] **`encode_feature_data_with_attrs` allocates per call** — Creates a new `Vec<u8>` per feature per zoom level (billions of allocations). Accept a `&mut Vec<u8>` parameter and reuse the buffer. (`wire_format.rs:61-78`)

- [ ] **k-way merge allocates `Vec<u8>` per record read** — During merge phase, every record read allocates a new `Vec<u8>`. Use a reusable buffer pool; since the heap has k entries, only k buffers are needed. (`sort.rs:202-221`)

- [ ] **`add_feature_to_layer` allocates geom_cmds Vec per feature** — During tile assembly, every decoded feature creates a `Vec<u32>`. Pass a `&mut Vec<u32>` in, or reference geometry as a byte slice into the sort record data. (`wire_format.rs:120-131`)

- [ ] **Sort chunk buffer doesn't free after flush** — `self.buffer.clear()` keeps allocated capacity. Use `std::mem::take` or `shrink_to_fit()` after flush to release the pointer array. (`sort.rs:154-178`)

## Performance: Algorithms & Data Structures (Medium-High Impact)

- [ ] **Tags linear scan called billions of times** — `Tags::get()` is O(n) over 3-15 tags, called ~100x per way across 20 matchers. Pre-sort tags and use binary search, or check the most discriminating tag first and short-circuit remaining matchers. (`shortbread.rs:139-160`)

- [x] **Polygon clipping creates 4 intermediate Vecs** — replaced with double-buffer swap in
  `clip_polygon()`. Two Vecs (`input`/`output`) swap roles via `std::mem::swap` across the four
  Sutherland-Hodgman edge passes. New `clip_polygon_edge_into()` appends to an existing buffer.

- [ ] **Simplify allocates two Vecs per call** — `simplify()` allocates a `vec![bool]` keep array and a result Vec per invocation. Accept an output buffer, use a bitset for keep array. (`geometry.rs:167-180`)

- [x] **`to_tile_coords` allocates a new Vec per call** — added `to_tile_coords_into()` buffer-reuse
  variant. Used in `emit_line_feature` and `emit_polygon_feature` hot paths. Original function
  retained for tests and non-hot paths.

- [x] **`clip_linestring` returns `Vec<Vec<Point>>`** — changed to `SmallVec<[Vec<Point>; 1]>`.
  Most clips produce exactly one segment, so the outer container stays inline.

- [x] **`match_element` always allocates `Vec<LayerMatch>`** — changed to `SmallVec<[LayerMatch; 4]>`.
  Most elements match 1-3 layers.

- [x] **`LayerMatch.attrs: Vec<Attr>` allocates per match** — changed to `SmallVec<[Attr; 8]>`.
  Most matches have 1-6 attributes.

- [ ] **POI `contains()` linear scan on 50-entry arrays** — `AMENITY_VALUES` (51 entries), `SHOP_VALUES` (37 entries) searched linearly. These are already sorted; use `binary_search()` or `phf` perfect hash set. (`pois.rs:93-157, 219-233`)

- [ ] **Projection transcendentals called billions of times** — `project_e7` calls tan, cos, ln per node. Use a lookup table for latitude projection (180K entries for 0.001-degree steps) with linear interpolation, or polynomial approximation. (`geometry.rs:103-118`)

- [ ] **`merge_same_attr_geometries` per-tile HashMap** — Clones and sorts tags for every feature per tile, allocates Vec for hash key. Hash tags in-place or pre-sort during insertion. (`mvt.rs:454-517`)

## Performance: Parallelism & I/O (Medium Impact)

- [ ] **PBF callback is single-threaded** — Tag matching and tag collection happen in the serial `for_each_pipelined` callback. Move tag matching and collection into rayon batch processing. (`pipeline.rs:305`)

- [ ] **Ocean processing: parallel collect then serial push** — `par_iter` collects into `Vec<Vec<SortRecord>>`, then pushes serially. Each rayon worker could flush to a thread-local sort chunk file directly. (`ocean.rs:180-203`)

- [ ] **Compression level tradeoff** — Level 6 is used; level 3-4 would be noticeably faster with ~5% larger output. Make configurable. (`pipeline.rs:1239`)

- [ ] **Relation tag String cloning** — Every relation's tags are cloned from `&str` to `String` because PBF borrows don't survive the batch boundary. Use string interning or buffer raw PBF bytes. ~14M relations * ~10 tags * ~30 bytes = ~4 GB. (`pipeline.rs:666-668`)

- [ ] **`boundary_way_coords.push(merc.clone())` duplicates geometry** — Clones full projected geometry for boundary relations. Store indices into `member_ways` instead. (`pipeline.rs:654`)

- [ ] **Sort chunk write buffer too small** — Default BufWriter (8 KB) for chunk writes. Use `BufWriter::with_capacity(1 << 20, file)` (1 MB). (`sort.rs:158-159`)

- [ ] **MVT value interning uses SipHash** — `DefaultHasher` is slower than needed for non-adversarial input. Use `FxHasher` or `ahash`. (`mvt.rs:366-379`)

- [ ] **`madvise` hints not set during write phase** — Call `MADV_SEQUENTIAL` on node/way mmaps during PBF write phase, switch to `MADV_RANDOM` for relation lookups. (`way_index.rs:117-128`, `node_index.rs`)

## Correctness Bugs

- [x] **MVT value hash collision bug** — `value_map` now uses `HashMap<Value, u16>` with manual
  `Hash`+`Eq` impls on `Value` (floats hashed via `to_bits()`). Eliminates silent wrong tile data
  from 64-bit hash collisions. Old `value_hash` helper removed.

- [ ] **NodeIndex sentinel value `(0, 0)` is a valid coordinate** — A node at exactly lat=0, lon=0 (Gulf of Guinea) appears as "not found". Use a separate bit or different sentinel. Document at minimum. (`node_index.rs:63-81`)

- [ ] **`compare_tiles.rs` `expand_single` wrong for runs** — Assumes each tile in a run has data at `offset + length * r`, but PMTiles run-length means all tiles in a run share the SAME blob. (`examples/compare_tiles.rs:98-106`)

## Code Quality: Duplication

- [x] **Node/DenseNode processing duplicated** — extracted `handle_node!` macro in `pipeline.rs`.
  Both `Node` and `DenseNode` have different types but identical processing logic; a macro
  avoids the duplication without requiring a shared trait.

- [x] **`emit_point_feature` / `emit_centroid_feature` nearly identical** — merged into single
  `emit_point_or_centroid(osm_id, point, ...)` in `pipeline.rs`.

- [x] **Cascading simplification loop duplicated 3-4x** — extracted `for_each_zoom_simplified`
  (single geometry) and `for_each_zoom_simplified_multi` (outer+inners) helpers in `geometry.rs`.
  Used in `emit_line_feature`, `emit_polygon_feature`, `emit_multipolygon_feature`, and
  `emit_ocean_polygon`.

- [x] **`point_in_polygon` implemented twice** — consolidated into `geometry::point_in_polygon`.
  Both `ocean.rs` and `multipolygon.rs` now import from geometry.

- [x] **POI matchers: 6 functions with identical structure** — extracted `match_tag_in_list`
  helper in `pois.rs` for the 3 simple matchers (emergency, historic, shop). 3 complex
  matchers (leisure, man_made, tourism) kept separate due to extra attribute logic.

- [x] **`match_addresses_point` / `match_addresses_centroid` duplicated** — extracted shared
  `match_addresses` helper taking `GeomExpect` as parameter in `shortbread.rs`.

- [x] **Hilbert curve code duplicated in `compare_tiles.rs`** — deleted local copies, now
  imports `elivagar::pmtiles_writer::tile_id_to_zxy`.

## Code Quality: Type Safety & API

All reviewed, none worth changing:

- **`Attr` tuple** — 3-element tuple is clear when destructured (`key, val, min_zoom`). A named struct would make ~100+ construction sites more verbose for no readability gain.
- **`PipelineError(String)`** — No code ever inspects the variant; errors are only displayed. An enum would be over-engineering.
- **Layer indices as `u8`** — `Layer` is `#[repr(u8)]`, so `as u8` is a zero-cost correct cast. A wrapper method is pure ceremony.
- **`GeomType` hardcoded integers** — 4-line match in one place, right next to "geom_type" comment. `TryFrom` would be more boilerplate than the match.
- **`SortReader::next()` fallible** — Standard `Iterator` can't express `Result<Option<T>>`. The `#[allow]` + `while let` pattern is idiomatic for fallible iteration.
- **Wire format version byte** — Internal format used in `.tilegen_tmp` within a single run. Never persisted across versions or shared.

## Code Quality: Miscellaneous

- [ ] **`expect("slice")` messages not helpful** — Multiple `.expect("slice")` calls with no context. Use descriptive messages. (`ocean.rs:49-68`)

- [ ] **Silent failures in `add_feature_to_layer`** — Silently returns on malformed data with no logging. Add `eprintln!` warning or return `Result`. (`wire_format.rs:94-204`)

- [ ] **Dead match arm in `water_polygons_labels`** — Both arms return 14. Either differentiate or simplify to `let label_zoom = 14`. (`shortbread.rs:404-407`)

- [ ] **Unused `mvt::Value` variants** — `Float(f32)`, `SInt(i64)`, `UInt(u64)` may never be constructed. Remove if confirmed unused. (`mvt.rs:21-30`)

- [ ] **`drop(std::fs::remove_dir_all(...))` is non-idiomatic** — Use `let _ =` to ignore the `Result`. (`pipeline.rs:117`)

- [ ] **PMTiles metadata JSON manually constructed** — String concatenation is fragile. Use `serde_json`. (`pmtiles_writer.rs:499-520`)

- [ ] **`main.rs` argument parsing has no bounds check** — `args[i+1]` will panic if a flag is the last argument. (`main.rs:5-57`)

- [ ] **Magic numbers: sort chunk size** — `1_073_741_824` (1 GB) appears twice. Define `const SORT_CHUNK_SIZE: usize = 1 << 30`. (`pipeline.rs:131, 272`)

- [ ] **Magic numbers: gzip level, batch sizes, area thresholds, BufWriter capacity** — Hardcoded without named constants or documentation. (`pipeline.rs:531,621,797,1098,1239`, `pmtiles_writer.rs:106`)

- [ ] **Ocean boundary tiles use `HashSet<u64>`** — A bitset indexed by packed tile coordinates would be more cache-friendly. (`ocean.rs`)

## Test Coverage Gaps

- [x] **ocean.rs** — rasterize_segment, point_in_polygon tested (10 tests)
- [x] **node_index.rs** — put/get, grow, sentinel, overwrite tested (6 tests)
- [x] **way_index.rs** — lifecycle, sentinel, overwrite tested (7 tests)
- [x] **pipeline.rs emit functions** — sort key/wire format decode roundtrips, cascading simplification, zoom-dependent attrs (7 tests)
- [x] **pmtiles_writer.rs** — end-to-end write_to header validation (1 test)
- [ ] **No integration test for PMTiles output validity** — No test verifies generated PMTiles can be read back and tiles decoded correctly (beyond header check).
