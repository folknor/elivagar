# elivagar TODO

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

## Planet scale — blocked on hardware

Waiting for a dedicated NVMe server with ≥64 GB RAM. Denmark-only performance work is
exhausted at 14s (18× from 242s). Geographic profiling (Germany, Norway, Japan) revealed
ocean polygon clipping as a major new bottleneck — see performance section below.

### What needs to happen on the new hardware

1. **Download planet PBF** (~73 GB) and ocean shapefiles
2. **First run** — see what breaks. Expect: SortedNodeStore OOM (68 GB > 64 GB RAM).
   way_index offset file will be ~144 GB (12B max way_id × 12 bytes) and sparse.
   Sort chunks will be large — may need to tune SORT_CHUNK_SIZE.
3. **Bitpacked coordinate compression** — needed to fit SortedNodeStore in RAM.
   See investigation notes below. **Validate bit-width distribution first** before
   implementing compression in the pipeline.
4. **Profile at planet scale** — bottlenecks will shift. Drain thread may become
   critical again (828 blocks for Denmark → ~125K blocks for planet). Sort phase
   and assemble phase will be much larger.

### Bitpacked coordinate compression for SortedNodeStore

8.5B nodes × 8 bytes = 68 GB uncompressed, target 64 GB RAM. Denmark doesn't need it.

Node IDs are chronological, NOT geographic — consecutive IDs can be anywhere on the globe,
so naive delta encoding between consecutive nodes is unreliable. Best approach: FOR
(Frame of Reference) encoding per 128-value block using `bitpacking` crate (v0.9, stable
Rust, SSE3 SIMD). Per block: compute min_lat/min_lon, store offsets as u32, bitpack at
the block's max bit-width. Estimated compression: ~24 bits avg (vs 32 raw) → 68 GB → ~51 GB.
Lookup overhead: ~150ns to decompress a 128-value block (+13% on `process_raw_way`).

**Validation step**: write a standalone tool that reads a planet PBF, builds the
SortedNodeStore, and measures the actual bit-width distribution across all 128-value
blocks. This gives an exact compression ratio without modifying the main pipeline.
If actual average is closer to 28-30 bits, savings drop to 59-63 GB — too tight.
A fallback path (dense packed mmap file with in-RAM bitmask index) should be designed.

## Performance — geographic profiling results

Profiled Germany (4.4 GB), Norway (1.3 GB), Japan (2.3 GB) on dm6.
Full data: `notes/geographic-profiles.md`.

**Key finding: ocean polygon clipping is the #1 alloc hotspot globally.**
Norway's fjords push `clip_polygon_into` to 197 GB alloc (1729% of main!) and
`for_each_zoom_simplified_multi` to 208 GB (1830%). These are massive multi-thousand-vertex
ocean polygons clipped across hundreds of tiles at every zoom level. The single-tile fast path
doesn't help — ocean polygons span many tiles by nature. At planet scale with all the world's
coastlines, this will dwarf everything else in the pipeline.

Profile shapes are geography-dependent, not size-dependent:
- **Germany** (inland/urban): `process_raw_way` dominates, assemble 24%, ocean negligible
- **Norway** (fjords): ocean/clipping dominates (28% wall), `clip_polygon_into` 197 GB alloc
- **Japan** (hybrid): PBF + ocean both significant, `emit_ocean_polygon` 733 µs/call avg

### Priority 1: Ocean polygon clipping alloc reduction

`clip_polygon_into` allocates a fresh `Vec<Point>` per clip call. For ocean polygons spanning
many tiles (fjords, archipelagos), this means millions of allocations of large vectors.
Norway: 31.8M calls × 6.5 KB avg = 197 GB. Japan: 6.0M calls × 8.1 KB avg = 46 GB.

- [x] **Research how Tilemaker and Planetiler handle ocean polygon clipping.**
  - **Planetiler**: Uses stripe clipping (from geojson-vt), not per-tile Sutherland-Hodgman.
    Slices geometry once through vertical then horizontal stripes to produce all tiles in one
    pass. Tracks filled interior tiles via parity algorithm + RoaringBitmap, emits pre-computed
    fill rectangles. No buffer reuse across clips (fresh allocs per tile). No pre-splitting.
    Source: `data/planetiler/` `TiledGeometry.java`, `FeatureRenderer.java`.
  - **Tilemaker**: Uses Sutherland-Hodgman (same as us) with Cohen-Sutherland bit codes.
    Key difference: **clip cache** — caches clipped geometry at parent zoom levels. Clip at z-1
    is reused for z children, reducing clip input size. Two-level spatial index (RTree + bitmap)
    for fast tile-skip. Bbox pre-check at shapefile load time.
    Source: `data/tilemaker/` `geom.cpp`, `clip_cache.h`, `shp_mem_tiles.cpp`.
- [x] **Hoist clip buffers in `emit_boundary_tile`** — `ocean.rs:458,468` was using `clip_polygon`
  (the allocating convenience wrapper) instead of `clip_polygon_into` with reusable buffers.
  Added `clip_a`/`clip_b` params to `emit_boundary_tile`, hoisted from `emit_ocean_polygon`.
  Results: `clip_polygon_into` alloc dropped from **197 GB → 0** (out of top 10). Thread alloc
  **296 GB → 110 GB** (−63%). Norway ocean phase **16.6s → 11.4s** (−31%). Norway wall
  **55.7s → 50.7s** (−9%). Japan ocean **8.4s → 7.6s** (−10%). Denmark unchanged (ocean negligible).
- [ ] **Tilemaker-style clip cache** — clip at parent zoom, reuse clipped result for child
  tiles at the next zoom. `clip_cache.h` shows the pattern: keyed by (zoom, tile_xy, object_id),
  FIFO eviction at 1024 entries/shard. For ocean polygons that span many tiles across many zooms,
  clipping a z13 result is much cheaper than re-clipping the full simplified polygon. More
  complex than buffer hoisting — defer unless #1 doesn't close the gap.
- [ ] **Investigate**: are we clipping ocean polygons against tiles they don't intersect?
  The outcode pre-test in `clip_polygon_into` rejects trivially-outside rings, but a coarser
  bbox pre-check before even calling clip could skip more. Tilemaker uses a two-level spatial
  index (RTree + bitmap at z15) for this.

### Priority 2: Multi-polygon simplification cost

`for_each_zoom_simplified_multi` averages 249 µs/call on Norway (vs 1.5 µs for single-way
`_simplified`). 788K calls × 277 KB avg = 208 GB alloc. This processes multi-ring polygons
(ocean, relations) through simplification at each zoom.

- [x] **Research how Tilemaker and Planetiler handle multi-ring polygon simplification.**
  - **Planetiler**: Simplifies per-ring using selectable algorithms (Douglas-Peucker default,
    Visvalingam-Whyatt optional). No cross-zoom caching — recomputed fresh each zoom level.
    No large-polygon-specific algorithm. Per-ring minimum 4 points enforced.
    Source: `data/planetiler/` `DouglasPeuckerSimplifier.java`, `VWSimplifier.java`.
  - **Tilemaker**: Per-ring simplification with RTree-backed self-intersection detection.
    Outer ring simplified against inners' RTree, inner rings against outer's RTree. Overlapping
    simplified rings merged via `simplify_combine()`. Also offers Visvalingam-Whyatt.
    No cross-zoom caching either.
    Source: `data/tilemaker/` `geom.cpp` (lines 16-136), `visvalingam.cpp`.
  - **Neither caches simplified geometry across zoom levels.** Both recompute per zoom.
- [ ] **Avoid `.to_vec()` in `for_each_zoom_simplified_multi`** — `geometry.rs:405-406` clones
  the entire outer ring and all inner rings upfront (`outer.to_vec()`, `inners.to_vec()`). For a
  10K-vertex fjord polygon with holes, this is ~160 KB per call. The cascading design needs owned
  data to mutate, but the ocean caller (`emit_ocean_polygon`) already has owned data — could
  accept owned vecs to avoid the copy, or take `&mut` slices.
- [ ] **Investigate**: why is `_multi` 100-300× more expensive per call? Is it the ring count,
  vertex count per ring, or per-ring alloc overhead? Profile the inner loop. The `.to_vec()`
  clone is one factor, but 249 µs avg suggests the actual simplification of large rings dominates.
- [ ] **Reusable simplification buffers for multi variant** — the `_simplified` single-way
  path was deferred because mimalloc handles small allocs well, but `_multi` allocs are huge
  (277 KB avg). Worth revisiting buffer reuse here specifically.

### Priority 3: Ocean emission pipeline

`emit_ocean_polygon` averages 521-733 µs/call. Norway: 317K calls, 165s cumulative.
Japan: 118K calls, 87s. These are individual ocean shapefile polygons being projected,
simplified, and clipped across all zoom levels.

- [x] **Research how Tilemaker and Planetiler structure their ocean pipeline.**
  - **Planetiler**: No ocean-specific pipeline. Ocean polygons processed like any other polygon
    through the general `FeatureRenderer` → `TiledGeometry` stripe clipping path. The filled tile
    optimization (parity tracking + pre-computed fill rectangles) is the main ocean optimization,
    applied generically to all large polygons.
    Source: `data/planetiler/` `FeatureRenderer.java` (lines 283-315).
  - **Tilemaker**: Uses water-polygons-split shapefiles (same as us), not osmcoastline. Clips
    shapefiles to dataset bbox once at load time, then indexes with RTree + bitmap. Per-tile
    processing queries the spatial index, clips from cache or parent zoom, simplifies per-tile.
    Source: `data/tilemaker/` `shp_processor.cpp`, `shp_mem_tiles.cpp`, `tile_data.cpp`.
  - **Our approach** (scanline fill with edge rasterization + gap PIP) is architecturally sound
    and similar to Planetiler's filled tile concept. The bottleneck is alloc overhead, not the
    algorithm itself.
- [ ] **Investigate**: is there redundant work across zoom levels? Could we simplify once
  and clip the simplified versions, rather than clipping first then simplifying? Currently
  `for_each_zoom_simplified_multi` re-simplifies at each zoom (cascading), then boundary tiles
  are clipped from the simplified geometry. This is the right order (simplify then clip).
- [ ] **Pre-split large ocean polygons** — a 10K-vertex fjord polygon that spans 200 tiles
  at z14 could be split at a coarser level first to reduce per-tile clip input size. Planetiler
  doesn't do this (relies on stripe clipping). Tilemaker doesn't either (relies on clip cache).
  May not be needed if clip buffer hoisting + clip cache close the alloc gap.

## Performance — Denmark squeeze opportunities (completed)

Denmark 13.7s wall (bench-self), 12.2s hotpath. Profile: `notes/hotpath-profile.md`.

### ~~Priority 1: More rayon concurrency~~ — Done

- [x] Multi-block in-flight via `rayon::in_place_scope` + `s.spawn()`. Up to 4 blocks
  in rayon pool simultaneously. Token semaphore bounds memory. Rayon thread utilization
  improved from 37-47% → 71-73%. PBF phase: 9.0s → 8.1s (−10%). `process_raw_way` P99
  dropped 156µs → 113µs (−28%).
- [x] `-j` / `--threads` CLI flag controlling rayon global pool + pbfhogg decode threads.
- [x] pbfhogg `decode_threads()` API to control decode pool size (set to threads/3).

### ~~Priority 2: Single-tile fast path~~ — Done

- [x] Skip clipping for features whose bbox fits entirely within one tile at a given zoom.
  `is_single_tile()` check before clip — when true, geometry is already inside the clip
  rect, so clipping is a no-op. Rayon CPU −30% (`emit_polygon_feature` −47%,
  `emit_line_feature` −43%). Wall time modest at Denmark (−0.3s) because cores were
  already saturated. Alloc throughput −3.3 GB (clip_polygon_into eliminated for single-tile).

### ~~Priority 3: Assemble merge alloc reduction~~ — Done

- [x] Replaced FxHashMap (tag Vec cloning on every `.entry()`) with sort+scan in
  `merge_same_attr_geometries`. Eliminates all tag cloning and HashMap overhead.
  Assemble phase −70ms, −100 MB alloc. Modest win — tag clones were smaller than
  estimated (~32 bytes/clone).

### Not worth pursuing — investigated and rejected

- **Thread-local arenas / hoisted simplification buffers** — Remaining per-feature allocs
  (1.3 KB avg × 6.6M = 8.1 GB) are: coords_e7 (ownership transfer to ProcessedWay),
  merc projection (read-only after creation), cascade/keep_buf/simp_buf in
  `for_each_zoom_simplified` (~1.5 GB, hoistable via `map_init` but requires threading
  buffers through 5-6 function signatures). Mimalloc handles these at Denmark scale but
  planet (150× features → ~1.2 TB alloc throughput) will stress the allocator harder.
  Estimated wall-time gain 0.1-0.3s Denmark, potentially larger at planet. Deferred until
  planet hardware is available for profiling — implement if allocator shows up in the profile.
- **Pre-size wire format buffers** — `add_feature_to_layer` already uses `with_capacity()`
  pre-sizing. 303 B avg, no realloc churn to cut.
- **Simplification algorithm** — already exhausted (DP + convergence + subpixel bbox, VW tried/reverted)
- **Sort phase** — 0.3s, negligible
- **match_element** — 296ns/call, very tight
- **Ocean (Denmark)** — 16ms on Denmark. **BUT: 16.6s on Norway (30% of wall time).**
  The "ocean is negligible" conclusion was Denmark-specific. Reclassified as Priority 1-3
  above after geographic profiling.

## Performance — Denmark history

Denmark extract: 242s → 13.7s (18×). PBF phase is CPU-bound in rayon
(simplification, clipping, MVT encoding). Drain is off the critical path.
Hotpath profile: `notes/hotpath-profile.md`.

Investigated-and-rejected optimizations are documented in code comments at each site.

### Completed

- [x] ~~**Visvalingam-Whyatt instead of Douglas-Peucker.**~~ Tried and reverted — VW's
  allocation overhead exceeds DP savings for small geometries (avg ~10 vertices). Non-cascading
  VW also produces more output at low zooms. See `notes/vw-simplification-experiment.md`.

- [x] ~~**SortedNodeStore**~~ — replaced 96 GB sparse mmap with compact in-RAM hierarchical
  store (bitmask+popcount, ~420 MB for Denmark). PBF phase: 16s → 11s. Total: 24s → 17s.

- [x] ~~**libdeflate**~~ — replaced flate2 with libdeflater for gzip compression. Assemble
  phase: 2.5s → 2.3s.

- [x] ~~**Double-buffer + block-level way dispatch**~~ — block-level dispatch via
  `into_blocks_pipelined`. Worker thread receives owned PrimitiveBlocks, extracts + rayon
  processes. Main thread drains results between blocks. PBF phase: 13.3s → 9.3s. Total: 17s → 15s.

- [x] ~~**Reduce serial drain cost**~~ — dedicated drain thread owns way_index + sort_writer
  during way phase, runs concurrently with worker. land_mask.mark_bbox() moved to rayon
  threads (AtomicU8, already Sync). Drain (4.54s) now fully overlapped with worker (~7s) —
  no longer on the critical path. Concurrent way_index writes (approach 3) not worthwhile.
  PBF phase: 9.3s → 8.6s. Total: 15s → 14s.

- [x] ~~**Single-tile clipping fast path**~~ — `is_single_tile()` check skips clip when
  feature bbox fits in one tile (clipping is a no-op). Rayon CPU −30%
  (`emit_polygon_feature` −47%, `emit_line_feature` −43%). Global alloc −3.3 GB
  (clip_polygon_into eliminated). Wall time modest at Denmark (−0.3s hotpath) because
  rayon cores already saturated. Total: 14s → 13.7s.

- [x] ~~**Sort-based geometry merge**~~ — replaced FxHashMap (tag Vec cloning) with
  sort+scan in `merge_same_attr_geometries`. Eliminates all tag cloning and HashMap
  overhead. Assemble phase −70ms, −100 MB alloc.

### Not pursued

- **Rayon alternatives for slice-based parallelism** — Research notes in previous git
  history. Key options: paralight, orx-parallel, chili, forte. Not a current bottleneck.

## Performance: Linux kernel features — all reverted

All madvise/fadvise hints were tried and removed. Every hint caused regressions because the
node index was a sparse file. Now that SortedNodeStore is in-RAM, the madvise concern is moot
for the node index. See CLAUDE.md for full history.
