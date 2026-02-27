# Hotpath Profile — Denmark

Dataset: `denmark-latest.osm.pbf` (483 MB), 52.5M nodes, 6.6M ways, 46K relations.
Machine: plantasjen, 30 GB DDR4, NVMe, Ryzen 9 5900X (12c/24t).
SortedNodeStore: ~420 MB in-RAM (bitmask+popcount).

## Current Baseline (2026-02-26, commit `d22b507`, multi-block overlap)

Wall time: **12.4s** (hotpath), **13.8s** (bench-self best of 3). phase12=9.5s, ocean=16ms, sort=0.3s, assemble=2.0s.
14.8M features, 56K tiles (54K unique), 283 MB output. RSS: 795 MB.

Architecture: multi-block overlapping pipeline during way phase:
1. **pbfhogg I/O thread** — reads + decodes PBF blocks, delivers via `into_blocks_pipelined`
2. **Main thread** — classifies blocks by `block_type()`, processes nodes inline, forwards
   way blocks to worker, processes relations after worker+drain join
3. **Worker thread** — receives owned PrimitiveBlocks, extracts RawWay, spawns rayon tasks
   via `rayon::in_place_scope` + `s.spawn()`. Up to MAX_INFLIGHT (4) blocks in rayon pool
   simultaneously — eliminates inter-block idle gaps and improves load balancing.
4. **Drain thread** — owns way_index + sort_writer, receives `Vec<ProcessedWay>` from worker,
   writes way_index entries + sort records concurrently with worker processing

Token-based semaphore (sync_channel) limits in-flight blocks to 4, bounding memory.
`land_mask.mark_bbox()` runs on rayon threads (AtomicU8-based, `&self`).
pbfhogg decode pool set to `threads/3` (reduced from `avail_parallelism-2`) to avoid
oversubscription with elivagar's rayon pool.

### Function Timing (top 10 of 26 measured)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % Wall |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 7.74µs | 2.32µs | 18.30µs | 113.28µs | 51.2s | 414% |
| `for_each_zoom_simplified` | 6.6M | 3.35µs | 850ns | 5.36µs | 50.88µs | 22.1s | 178% |
| `emit_polygon_feature` | 4.5M | 4.01µs | 920ns | 7.26µs | 71.30µs | 18.2s | 147% |
| `emit_line_feature` | 2.0M | 5.20µs | 1.13µs | 6.70µs | 78.14µs | 10.6s | 85% |
| `drain_processed_ways` | 828 | 6.02ms | 4.60ms | 8.54ms | 13.98ms | 4.99s | 40% |
| `match_element` | 10.1M | 322ns | 300ns | 690ns | 1.19µs | 3.26s | 26% |
| `add_feature_to_layer` | 14.8M | 195ns | 130ns | 470ns | 910ns | 2.89s | 23% |

>100% totals = parallel work on rayon threads. % is CPU-time / wall-time.

### Multi-block Overlap Analysis

The multi-block overlap reduced `process_raw_way` P99 from 156µs → 113µs (−28%). Fat-tail
ways no longer stall entire block completion — rayon steals work from other in-flight blocks.
Total rayon CPU dropped from 58.2s → 51.2s (−12%), indicating less scheduling overhead and
better cache utilization with multiple blocks interleaved.

### Drain Analysis

828 way blocks from the PBF. `drain_processed_ways` runs on the dedicated drain thread,
receiving results via `sync_channel(4)`. The drain takes 4.99s (slightly more than before
due to out-of-order results), but still under the ~7s worker time — **not on the critical
path**.

### Thread Utilization

| Thread | CPU% | User | Sys | Total |
|---|---|---|---|---|
| Main | 17–101% | 4.65s | 0.89s | 5.54s |
| Rayon ×3 | 71–73% | ~2.9s | ~0.1s | ~2.9s each |

Rayon thread utilization improved from 37-47% → 71-73% due to multi-block overlap keeping
more work available in the pool.

### Allocation Profile (system allocator, no mimalloc)

Total allocated: **5.0 GB**. Global throughput: 35.6 GB alloc, 37.0 GB dealloc. RSS: 2.0 GB.

| Function | Calls | Avg | P50 | P95 | P99 | Total | % |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 1.7 KB | 1.0 KB | 4.1 KB | 9.2 KB | 10.5 GB | 210% |
| `for_each_zoom_simplified` | 6.6M | 1.3 KB | 800 B | 3.1 KB | 7.0 KB | 8.2 GB | 164% |
| `emit_polygon_feature` | 4.5M | 1.4 KB | 794 B | 3.2 KB | 7.4 KB | 6.0 GB | 119% |
| `add_feature_to_layer` | 14.8M | 302 B | 60 B | 902 B | 4.5 KB | 4.2 GB | 83% |
| `merge_same_attr_geometries` | 306K | 11.5 KB | 3.4 KB | 63.9 KB | 282.0 KB | 3.3 GB | 67% |
| `clip_polygon_into` | 8.1M | 327 B | 304 B | 1.3 KB | 2.7 KB | 2.5 GB | 49% |
| `emit_line_feature` | 2.0M | 1.2 KB | 808 B | 3.0 KB | 6.0 KB | 2.3 GB | 46% |

Per-thread:

| Thread | Alloc | Dealloc | Diff |
|---|---|---|---|
| Main | 5.9 GB | 7.1 GB | −1.2 GB |
| Rayon ×3 | ~2.1 GB each | ~1.5 GB each | ~0.6 GB each |

## Key Observations

### 1. CPU-dominated — no I/O stalls

SortedNodeStore eliminated the 96 GB sparse mmap. Rayon workers now spend ~98% of time
in user mode (was ~97% sys with the mmap). `process_raw_way` avg dropped from 750µs
(dm6, mmap) to 9.0µs — 83× faster per call. Main thread sys time is now just 1.0s (was
3.7s when it owned the drain).

### 2. Simplification is the #1 CPU consumer

`for_each_zoom_simplified` (25.1s total CPU, 184% wall) dominates feature processing.
Douglas-Peucker runs at each zoom level from z14 down to z_lo. Pre-DP subpixel bbox
check and DP convergence tracking reduce unnecessary work, but DP itself is inherently
O(n²) per level. No further algorithmic improvements available (VW tried and reverted).

### 3. Polygons dominate the workload

4.5M polygon features vs 2.0M line features. `emit_polygon_feature` (22.2s) is 1.9×
`emit_line_feature` (11.7s). Each polygon also calls `clip_polygon_into` (~2 clips per
polygon across tiles).

### 4. Drain is no longer the bottleneck

With the dedicated drain thread, `drain_processed_ways` (4.54s) runs concurrently with
worker processing (~7s). Since drain < worker, the drain completes before the worker
finishes each cycle. The critical path is now purely worker + rayon processing time.
Approach 3 from the investigation (concurrent way_index offset writes) is not worthwhile.

### 5. Parallelism utilization improved

Total CPU ~23s on 13.7s wall = **1.7× utilization** (was 1.5× with main-thread drain,
1.3× before double-buffering). Main thread is now mostly idle during way phase — it just
forwards blocks (~0.3s of work for 828 blocks).

### 6. Planet-scale projection

Denmark is ~1/150th of planet by PBF size. Extrapolating: ~6.2 TB of allocator throughput
at planet scale. SortedNodeStore for planet (8.5B nodes × 8 bytes = 68 GB uncompressed)
won't fit in 64 GB RAM — needs bitpacked coordinate compression (~51 GB estimate).

## Performance History

| Commit | Change | PBF Phase | Total | Host |
|---|---|---|---|---|
| 2ba4c0f | 96 GB sparse mmap (baseline) | — | 242s | dm6 |
| | *(host switch — all rows below are plantasjen)* | | | |
| d65ee36 | SortedNodeStore | 13.3s | 17.2s | plantasjen |
| 647a360 | Double-buffer way batches | 10.1s | 15.8s | plantasjen |
| ca87a20 | Block-level dispatch | 9.3s | 15.1s | plantasjen |
| 31f7c14 | Iterator API (no perf change) | 9.3s | 15.1s | plantasjen |
| 15be3bf | Dedicated drain thread + land_mask to rayon | 8.6s | 14.4s | plantasjen |
| 5c45361 | BlockType API (no perf change) | 8.6s | 14.4s | plantasjen |
| d22b507 | Multi-block overlap + `-j` + decode thread control | 8.1s | 13.8s | plantasjen |

## Remaining Opportunities

1. ~~**Visvalingam-Whyatt**~~ — Tried and reverted. VW's allocation overhead (5 Vecs +
   BinaryHeap per call) exceeds DP savings for small geometries (avg ~10 vertices).
   Non-cascading VW also produces +7 MB output at low zooms. See `notes/vw-simplification-experiment.md`.
2. **Polygon-focused optimization** — polygons are 1.9× the total CPU of lines, but
   per-feature cost is similar. The ratio is mostly feature count (2.25×).
3. ~~**Node storage redesign**~~ — Done. SortedNodeStore replaces 96 GB sparse mmap with
   ~420 MB in-RAM hierarchical store (bitmask+popcount). PBF phase −5.2s.
4. **Bitpacked coordinate compression** — needed for planet scale (68 GB uncompressed SortedNodeStore
   won't fit in 64 GB RAM). FOR encoding estimate: ~51 GB. Not needed for extracts.
5. ~~**PBF callback parallelism**~~ — Done. Block-level dispatch via `into_blocks_pipelined`.
   Way blocks sent to worker thread, main thread drains results in parallel. PBF phase: 13.3s → 9.3s.
6. ~~**Reduce serial drain cost**~~ — Done. Dedicated drain thread runs concurrently with
   worker. land_mask.mark_bbox() moved to rayon. Drain is no longer on the critical path
   (4.54s drain < ~7s worker). PBF phase: 9.3s → 8.6s. Total: 15s → 14s.

## Optimization History

All completed. Documented here for reference.

1. **clip_polygon buffer reuse** — `clip_polygon_into()` with double-buffers. 4.4→2.5 GB.
2. **simplify buffer reuse** — `simplify_into()` with reusable keep/result Vecs. 10.3→9.1 GB.
3. **Parallel node lookups** — node resolution + tag matching moved to rayon. −12% alloc throughput.
4. **pbfhogg wire parser** — varint on-the-fly, eliminated ~9 GB protobuf Vec allocs. RSS −20%.
5. **Projection LUT** — 18-bit LUT replaces tan/cos/ln in `project_e7`. `process_raw_way` P99 −55%.
6. **Pre-DP subpixel bbox check** — `merc_bbox_is_subpixel` breaks zoom loop early. DP CPU −35%.
7. **Relation tag cloning** — match during prepare, store results. `prepare_relation` dropped from top 10.
8. **Bbox recompute from simplified cascade** — eliminates wasted tile iterations at low zooms.
9. **DP convergence tracking** — skip DP when deviation < next tolerance. Skip when ≤ min_points.
10. **intern_string_value** — zero-alloc lookup by `&str`. ~90% cache hit rate.
11. **Assemble-phase Vec pools** — `add_feature_to_layer` 4.4→4.2 GB (−5%), `merge_same_attr_geometries` 3.5→3.4 GB (−3%).
12. **Buffer hoisting (batch 2)** — gaps Vec, emit_boundary_tile buffers, emit_multipolygon_feature all_rings.
13. **Compiler opts (batch 2)** — `codegen-units=1`, `panic=abort`, `#[inline]` on hot small fns.
14. **libdeflate** — replaced flate2 (zlib-ng) with libdeflater. Thread-local compressor reuse. Assemble −0.2s.
15. **SortedNodeStore** — replaced 96 GB sparse mmap with ~420 MB in-RAM hierarchical store (bitmask+popcount). PBF phase −5.2s. Total 24s → 17s.
16. **Double-buffer + block dispatch** — block-level way dispatch via `into_blocks_pipelined`. Worker thread receives owned PrimitiveBlocks, extracts + rayon processes. Main thread drains results between blocks. PBF phase: 13.3s → 9.3s. Total: 17s → 15s.
17. **Dedicated drain thread** — drain_processed_ways moved to own thread, land_mask.mark_bbox() moved to rayon. Drain fully overlaps with worker processing. PBF phase: 9.3s → 8.6s. Total: 15s → 14s.
