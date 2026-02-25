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

## Profiling

Hotpath profile results and analysis: `docs/hotpath-profile.md`

## Performance: Allocation Pressure

Several per-call allocations were investigated and found unavoidable — ownership required by
the sort pipeline, or lifetime constraints prevent hoisting. See code comments at each site:
`SortRecord.data` (`sort.rs`), `tags_vec` (`pipeline.rs:314`), `coords_e7` (`pipeline.rs:598`),
`encode_feature_data_with_attrs` (`wire_format.rs:63`), k-way merge `read_record` (`sort.rs:208`).

- [ ] **`add_feature_to_layer` allocates geom_cmds Vec per feature** — Investigated: hard to fix.
  Each Feature must own its `Vec<u32>` because `merge_same_attr_geometries` needs random access
  across all Features simultaneously. Eliminating the allocation requires either lifetimes on
  Feature/LayerBuilder (borrow from sort record data) or an arena approach (flat Vec per tile,
  Features store offset+length). Both refactor Feature, LayerBuilder, merger, and encoder.
  Impact: 1.8 GB / 3.2% of alloc, in the assemble phase (7% of wall time). Not worth the
  complexity. (`wire_format.rs:131, mvt.rs:54-58`)

- [x] **Sort chunk write buffer too small** — Was default 8 KB BufWriter; now 1 MB. (`sort.rs:166`)

## Performance: Algorithms & Data Structures (Medium-High Impact)

- [ ] **Simplification P99 tail dominates CPU** — **Investigated.** `for_each_zoom_simplified`
  is the #1 CPU consumer (72.8s total, 182% wall). P50=800ns, P99=43µs (54× ratio). Complex
  coastlines/boundaries with 500-1000+ vertices run Douglas-Peucker O(n²) across 9-10 zoom
  levels. Cascading simplification is already implemented (z14→z13→...→z_lo), buffers hoisted.

  **What's already optimized:**
  - Cascading: each zoom simplifies previous zoom's output (halves input each level)
  - Buffer reuse: `keep_buf`/`simp_buf` hoisted, `mem::swap` avoids copies
  - Z14 skips DP entirely (`if z < 14`)
  - Min-points early exit (loop breaks when geometry collapses below 2/4 points)

  **Optimization options (ordered by bang-for-buck):**

  **Option A: Pre-simplification bbox subpixel check (recommended).** Before running DP at each
  zoom level, compute the cascade's bounding box in tile coordinates. If the bbox diagonal is
  smaller than ~1 pixel, skip that zoom and all coarser zooms (break the loop). Currently
  subpixel filtering only happens post-clipping in the emit functions (too late — DP already
  ran). This eliminates entire DP calls for features that are invisible at coarse zooms. Simple,
  zero risk, directly targets the tail. (`geometry.rs:305-327`)

  **Option B: Visvalingam-Whyatt instead of Douglas-Peucker.** VW computes a per-vertex
  "importance" (effective area) once in O(n log n) using a priority queue, then each zoom level
  just filters vertices by importance threshold — no re-scanning. Multi-zoom cost drops from
  ~1.33×N² to O(n log n) + O(n × zoom_levels). Larger change, different simplification
  behavior (VW preserves shape topology better than DP for some geometries). Would require
  new algorithm implementation, tolerance recalibration, and visual verification.

  **Option C: Early termination in `find_farthest()`.** The inner loop always scans all
  intermediate points. Could maintain a running "minimum possible max distance" from the
  recursion tree to prune branches. Limited benefit — the first DP call (z13, full N points)
  dominates, and that call can't be pruned much. (`geometry.rs:265-289`)

- [ ] **POI `contains()` linear scan on 50-entry arrays** — `AMENITY_VALUES` (51 entries), `SHOP_VALUES` (37 entries) searched linearly. These are already sorted; use `binary_search()` or `phf` perfect hash set. (`pois.rs:93-157, 219-233`)

- [x] **Projection transcendentals called billions of times** — Fixed: 18-bit LUT (262K entries,
  2 MB) with linear interpolation replaces tan/cos/ln in `project_e7`. Error: 0.03 pixels at z14.
  `project()` (exact transcendentals) kept for tests and one-off calls. (`geometry.rs:103-155`)

- [ ] **`merge_same_attr_geometries` per-tile HashMap** — Clones and sorts tags for every feature per tile, allocates Vec for hash key. Hash tags in-place or pre-sort during insertion. (`mvt.rs:454-517`)

## Performance: Parallelism & I/O (Medium Impact)

- [ ] **Ocean processing: parallel collect then serial push** — `par_iter` collects into `Vec<Vec<SortRecord>>`, then pushes serially. Each rayon worker could flush to a thread-local sort chunk file directly. (`ocean.rs:180-203`)

- [ ] **Compression level tradeoff [pre-release]** — Level 6 is used; level 3-4 would be
  noticeably faster with ~5% larger output. Make configurable. Final tuning item — do this
  right before 0.1 release after all other optimizations are locked in. (`pipeline.rs:1239`)

- [ ] **Relation tag String cloning** — Every relation's tags are cloned from `&str` to `String` because PBF borrows don't survive the batch boundary. Use string interning or buffer raw PBF bytes. ~14M relations * ~10 tags * ~30 bytes = ~4 GB. (`pipeline.rs:666-668`)

- [ ] **`boundary_way_coords.push(merc.clone())` duplicates geometry** — Clones full projected geometry for boundary relations. Store indices into `member_ways` instead. (`pipeline.rs:654`)

- [ ] **MVT value interning uses SipHash** — `DefaultHasher` is slower than needed for non-adversarial input. Use `FxHasher` or `ahash`. (`mvt.rs:366-379`)

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  the landscape. Key options:
  - **paralight** (v0.0.8) — lightweight, targets slice/mut-slice parallelism. Can run on top of
    rayon's thread pool via `RayonThreadPool::new_global` (no extra threads). Has proper
    `try_for_each_init` that inits once per thread (rayon inits once per work item). Only needs
    `&` not `&mut` for the rayon backend. Limitation: no scopes, no graph algorithms, no recursive
    parallelism. Max `u32::MAX` elements.
  - **orx-parallel** — has `using()` API for guaranteed per-thread init. No thread pool yet
    (spawns threads per pipeline), on roadmap. No scopes/graph support.
  - **chili** — low-level, only provides `join`. A rayon fork (`par-iter`) builds par_iter on top
    of it. Uses lazy scheduling (less overhead for fine-grained work).
  - **forte** — experimental, rayon-like API with lazy scheduling. Supports spawn, join, scopes,
    scoped spawns. No par_iter or par_bridge yet.
  - **spindle** — built on rayon, optimised for small tasks. Very early.

  Wild's `thread_local` crate trick is also relevant: wrap per-thread state in
  `thread_local::ThreadLocal` and `.get_or()` inside rayon closures to guarantee one init per
  thread. Simple and works today without switching libraries.

  Not a current bottleneck — hotpath shows workers were starved by serial I/O, now fixed
  (node lookups moved to rayon). Worth evaluating if further parallelism changes are needed.

- [ ] **NodeIndex madvise for planet scale** — Node lookups are now parallel (rayon), but
  madvise may still help *in combination* at planet scale (MADV_RANDOM to avoid wasted
  readahead when locality breaks down across continents). `NodeIndexReader::advise_random()`
  exists but is not called. WayIndex MADV_RANDOM in `finish_writing()` is fine (relation
  lookups are truly non-sequential). Needs planet-scale testing.
  Full investigation: `docs/madvise-investigation.md`.

## Test Coverage Gaps

- [ ] **No integration test for PMTiles output validity** — No test verifies generated PMTiles can be read back and tiles decoded correctly (beyond header check).
