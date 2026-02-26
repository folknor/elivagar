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

Waiting for a dedicated NVMe server with ≥64 GB RAM. All remaining performance work is
planet-scale only — Denmark (483 MB) is fully optimized at 14s (17× from initial 242s).

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

## Performance — squeeze opportunities

Denmark 13.8s wall (bench-self best of 3). Profile: `notes/hotpath-profile.md`.

### ~~Priority 1: More rayon concurrency~~ — Done

- [x] Multi-block in-flight via `rayon::in_place_scope` + `s.spawn()`. Up to 4 blocks
  in rayon pool simultaneously. Token semaphore bounds memory. Rayon thread utilization
  improved from 37-47% → 71-73%. PBF phase: 9.0s → 8.1s (−10%). `process_raw_way` P99
  dropped 156µs → 113µs (−28%).
- [x] `-j` / `--threads` CLI flag controlling rayon global pool + pbfhogg decode threads.
- [x] pbfhogg `decode_threads()` API to control decode pool size (set to threads/3).

### Priority 2: Single-tile fast path (est. 0.5-1s)

- [ ] Skip clipping for features whose bbox fits entirely within one tile at a given zoom.
  `emit_polygon_feature` (21.5s CPU) and `emit_line_feature` (11.6s CPU) both call into
  clip routines. Small features (majority of calls) that land in a single tile can bypass
  clip entirely.

### Priority 3: Assemble merge alloc reduction (est. 0.3-0.5s)

- [ ] `merge_same_attr_geometries` — 306K calls, 11.5 KB avg, 3.3 GB total alloc.
  Pre-sized buffers or merge-in-place could cut the Vec churn. Fat P99 (282 KB) suggests
  a few large tiles dominate the allocation.

### Priority 4: Thread-local arenas (est. 0.2-0.5s, bigger at planet)

- [ ] Per-feature allocation is 1.7 KB avg × 6.6M calls = 10.5 GB in `process_raw_way`.
  Resolved coordinate Vecs, clipped geometry Vecs, wire format buffers. Thread-local bump
  allocator or arena per rayon task would turn millions of small allocs into pointer bumps.
  Mimalloc already hides most of this at Denmark scale, but planet-scale (150× more features)
  will feel the pressure.

### Priority 5: Pre-size wire format buffers (est. 0.1-0.2s)

- [ ] `add_feature_to_layer` — 14.8M calls, 302 B avg, 4.2 GB total. Sort record buffers
  grow via `Vec::push`. Pre-sizing based on vertex count could cut realloc churn.

### Not worth pursuing

- **Simplification algorithm** — already exhausted (DP + convergence + subpixel bbox, VW tried/reverted)
- **Sort phase** — 0.3s, negligible
- **match_element** — 296ns/call, very tight
- **Ocean** — 16ms

## Performance — Denmark history

Denmark extract: 242s → 14s (17×). PBF phase is CPU-bound in rayon
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

### Not pursued

- **Rayon alternatives for slice-based parallelism** — Research notes in previous git
  history. Key options: paralight, orx-parallel, chili, forte. Not a current bottleneck.

## Performance: Linux kernel features — all reverted

All madvise/fadvise hints were tried and removed. Every hint caused regressions because the
node index was a sparse file. Now that SortedNodeStore is in-RAM, the madvise concern is moot
for the node index. See CLAUDE.md for full history.
