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

- [x] **`intern_value` allocates String for every cache hit** — Fixed: added
  `intern_string_value(&str) -> u16` with a separate `string_value_map: HashMap<String, u16>`
  that looks up by borrowed `&str` (zero allocation on cache hit). `intern_value` routes
  `Value::String` through it for consistency. Call site in `wire_format.rs` now calls
  `intern_string_value(s)` directly. (`mvt.rs:123-148, wire_format.rs:186`)

- [x] **Sort chunk write buffer too small** — Was default 8 KB BufWriter; now 1 MB. (`sort.rs:166`)

## Performance: Algorithms & Data Structures (Medium-High Impact)

- [ ] **Simplification P99 tail dominates CPU** — **Investigated, partially addressed.**
  `for_each_zoom_simplified` was the #1 CPU consumer. Complex coastlines/boundaries with
  500-1000+ vertices run Douglas-Peucker O(n²) across 9-10 zoom levels.

  **What's already optimized:**
  - Cascading: each zoom simplifies previous zoom's output (halves input each level)
  - Buffer reuse: `keep_buf`/`simp_buf` hoisted, `mem::swap` avoids copies
  - Z14 skips DP entirely (`if z < 14`)
  - Min-points early exit (loop breaks when geometry collapses below 2/4 points)
  - Pre-DP subpixel bbox check (breaks loop when geometry is < 1 pixel — see below)

  **Done — Option A: Pre-simplification bbox subpixel check.** Before DP at each zoom,
  `merc_bbox_is_subpixel` checks if the cascade's bbox diagonal is < 1 pixel in Mercator space.
  If so, skips DP and all coarser zooms. Results: `for_each_zoom_simplified` total CPU −35%
  (31.5s→20.4s), P99 −63% (89µs→33µs). `emit_polygon_feature` −37%, `emit_line_feature` −45%.
  630K fewer feature-zoom combinations (−3.7%). (`geometry.rs:38-56,341-346`)

  **Remaining options:**

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

  **Option D: DP max-deviation tracking for cascade skip.** If `simplify_into` returned the
  maximum perpendicular deviation it found, the cascade could check: "is max deviation <
  next zoom's tolerance?" If yes, skip DP at that zoom and all coarser zooms — the cascade
  is already optimal. Currently there's no way to know without running DP. Requires threading
  the max deviation out of `find_farthest` → `dp_recurse` → `simplify_into`. Medium effort,
  medium impact — eliminates DP invocations for features that have already converged.
  (`geometry.rs:262-320`)

  **Option E: Vertex count pre-check before DP.** If `cascade.len() <= min_points` before
  calling `simplify_into`, DP can't reduce further — skip it. Currently checked *after* DP
  at `geometry.rs:350`. Moving before saves a full DP invocation for already-collapsed
  features. Low effort, low impact. (`geometry.rs:347-351`)

- [ ] **Recompute bbox from simplified cascade per zoom** — **Investigated, promising.**
  The bbox passed to `emit_polygon_feature` / `emit_line_feature` is computed once from
  full-resolution coords (`pipeline.rs:630`), never recomputed after simplification. At low
  zooms where DP aggressively reduces vertices, the simplified geometry may span far fewer
  tiles than the original bbox suggests. This causes `clip_polygon_into` to be called on
  tiles where the geometry can't possibly intersect, producing empty results.

  Recomputing bbox from the simplified cascade inside the `for_each_zoom_simplified` callback
  is O(n) where n is the already-small simplified vertex count. Eliminates wasted tile
  iterations + S-H clipping calls. For a feature whose simplified form at z6 fits in 1 tile
  but whose original bbox spans 4 tiles, this eliminates 3 full S-H clip passes.

  Medium effort (callback currently receives `&[Point]` simplified coords; need to compute
  bbox and pass to tile iteration). High impact at low zooms.
  (`pipeline.rs:630,990-999, geometry.rs:853-869`)

- [ ] **Outcode pre-test before Sutherland-Hodgman clipping** — Before running
  `clip_polygon_into`, compute bitwise AND of all vertex outcodes against the tile rect.
  If all vertices share a common outside bit (all left, all right, etc.), the clip must
  produce empty — skip the full 4-edge S-H pass. One O(n) pass with 4 comparisons per
  vertex vs S-H's 4×O(n) with intersection math. Low effort, useful when the simplified
  bbox is still larger than the actual geometry extent (e.g. L-shaped features).
  (`geometry.rs:575-598`)

- [ ] **POI `contains()` linear scan on 50-entry arrays** — **Investigated, not worth it.**
  7 arrays (5-51 entries) searched via `.contains()`. All sorted except `emergency` (7 entries).
  But `contains()` is only reached when the element has the relevant key (`amenity`, `shop`,
  etc.) — a tiny fraction of all elements. Hotpath profile confirms tag matching is NOT a
  bottleneck (`match_element` not in top 10). The 51-entry `AMENITY_VALUES` is ~400 bytes —
  fits in L1 cache, sequential scan with first-byte short-circuit. `binary_search` saves ~19
  comparisons per hit but hits are rare. `phf` adds a dependency for zero measurable gain.
  (`pois.rs:93-157, 219-233`)

- [x] **Projection transcendentals called billions of times** — Fixed: 18-bit LUT (262K entries,
  2 MB) with linear interpolation replaces tan/cos/ln in `project_e7`. Error: 0.03 pixels at z14.
  `project()` (exact transcendentals) kept for tests and one-off calls. (`geometry.rs:103-155`)

- [ ] **`merge_same_attr_geometries` per-tile HashMap** — **Investigated, not worth it.**
  Clones and sorts tags for every feature per tile. Tried two alternatives:
  (1) Sort features directly by (geom_type, tags), merge adjacent runs — regressed assemble
  3.5s→5.8s (moving Feature structs with 3 Vecs each is expensive).
  (2) Sort indices by (geom_type, tags), merge runs — still regressed 3.5s→4.7s (O(n log n)
  tag slice comparisons worse than HashMap's O(n) clone+hash). Pre-sorting tags during
  insertion adds 16M sort calls per Denmark run. Assemble is only 7% of wall time;
  the clone+sort HashMap is already the right tradeoff. (`mvt.rs:454-517`)

## Performance: Parallelism & I/O (Medium Impact)

- [ ] **Ocean processing: parallel collect then serial push** — `par_iter` collects into `Vec<Vec<SortRecord>>`, then pushes serially. Each rayon worker could flush to a thread-local sort chunk file directly. (`ocean.rs:180-203`)

- [ ] **Compression level tradeoff [pre-release]** — Level 6 is used; level 3-4 would be
  noticeably faster with ~5% larger output. Make configurable. Final tuning item — do this
  right before 0.1 release after all other optimizations are locked in. (`pipeline.rs:1239`)

- [x] **Relation tag String cloning** — Fixed: moved `match_element` into `prepare_relation`
  while PBF borrows are alive. `PreparedRelation` stores match results (`SmallVec<[LayerMatch; 4]>`)
  instead of cloned tags. Also skips member way resolution for unmatched relations.
  (`pipeline.rs:669-735`)

- [x] **`boundary_way_coords.push(merc.clone())` duplicates geometry** — Fixed: replaced
  `boundary_way_coords: Vec<Vec<Point>>` with `is_boundary: bool`. Line emission iterates
  `member_ways` directly — `multipolygon::assemble()` only borrows, so coords are still
  available. Eliminates one `Vec<Point>` clone per member way per boundary relation.
  (`pipeline.rs:675-740,825-836`)

- [ ] **MVT value interning uses SipHash** — **Investigated, low priority.**
  `key_map` and `value_map` HashMaps in `LayerBuilder` use `DefaultHasher` (SipHash).
  Called from `add_feature_to_layer` (14.9M calls, 274ns avg). Each call does ~3-5 hash
  lookups (1 key + 2-4 values). SipHash costs ~15-25ns/hash → ~60-125ns per feature,
  potentially 20-45% of the 274ns avg. FxHash at ~3-5ns/hash would save ~50-100ns per
  feature (~1-1.5s CPU across threads, ~0.3s wall on a 29s run). But assemble is only
  6% of wall time, so wall impact is ~1%. HashMaps are small (few dozen to few hundred
  entries per tile) — collision resistance doesn't matter. Adds a dependency (`rustc-hash`
  or `ahash`) for minor gain. Worth doing if already pulling in FxHash for another reason.
  (`mvt.rs:86-88,112-131`)

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
