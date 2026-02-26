# Hotpath Profile — Denmark

Dataset: `denmark-latest.osm.pbf` (483 MB), 52.5M nodes, 6.6M ways, 46K relations.
Machine: plantasjen, 30 GB DDR4, NVMe, Ryzen 9 5900X (12c/24t).
SortedNodeStore: ~420 MB in-RAM (bitmask+popcount).

## Current Baseline (2026-02-26, commit 5c45361)

Wall time: **13.7s**. phase12=10.9s (79%), ocean=18ms, sort=0.3s, assemble=1.8s.
14.8M features, 56K tiles (54K unique), 283 MB output. RSS: 757 MB.

Architecture: 4-thread pipeline during way phase:
1. **pbfhogg I/O thread** — reads + decodes PBF blocks, delivers via `into_blocks_pipelined`
2. **Main thread** — classifies blocks by `block_type()`, processes nodes inline, forwards
   way blocks to worker, processes relations after worker+drain join
3. **Worker thread** — receives owned PrimitiveBlocks, extracts RawWay, runs rayon `par_iter`
4. **Drain thread** — owns way_index + sort_writer, receives `Vec<ProcessedWay>` from worker,
   writes way_index entries + sort records concurrently with worker processing

`land_mask.mark_bbox()` runs on rayon threads (AtomicU8-based, `&self`).

### Function Timing (top 10 of 26 measured)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % Wall |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 9.01µs | 2.18µs | 21.58µs | 167.17µs | 59.6s | 436% |
| `for_each_zoom_simplified` | 6.6M | 3.81µs | 800ns | 5.39µs | 68.22µs | 25.1s | 184% |
| `emit_polygon_feature` | 4.5M | 4.87µs | 870ns | 7.54µs | 104.38µs | 22.2s | 162% |
| `emit_line_feature` | 2.0M | 5.76µs | 1.09µs | 7.22µs | 118.14µs | 11.7s | 86% |
| `drain_processed_ways` | 828 | 5.48ms | 3.83ms | 8.68ms | 13.50ms | 4.54s | 33% |
| `match_element` | 10.1M | 291ns | 270ns | 620ns | 1.11µs | 2.95s | 22% |
| `add_feature_to_layer` | 14.8M | 197ns | 130ns | 470ns | 910ns | 2.91s | 21% |

>100% totals = parallel work on rayon threads. % is CPU-time / wall-time.

### Drain Analysis

828 way blocks from the PBF (natural batching by PBF block boundaries, ~8000 ways/block).
`drain_processed_ways` runs on the dedicated drain thread, receiving results via
`sync_channel(4)`. The drain completes in 4.54s while the worker takes ~7s — the drain
is **no longer on the critical path**. It finishes well before the worker, so further
drain optimization (e.g., concurrent way_index writes) would not improve wall time.

### Thread Utilization

| Thread | CPU% | User | Sys | Total |
|---|---|---|---|---|
| Main | 16–100% | 4.4s | 1.0s | 5.4s |
| Drain | ~97% | 6.7s | 0.1s | 6.7s |
| Rayon ×2 | 75% | ~3.5s | ~0.1s | ~3.6s each |

Main thread sys time dropped from 3.7s → 1.0s (no more way_index mmap writes on main).
The drain thread is 99% user mode — the mmap writes are cheap per-call, just many of them.
Total CPU across active threads: ~23s on 13.7s wall = **~1.7× utilization** (was 1.5×).

### Allocation Profile (system allocator, no mimalloc)

Total allocated: **9.1 GB**. Global throughput: 41.6 GB alloc, 41.5 GB dealloc. RSS: 2.2 GB.

| Function | Calls | Avg | P50 | P95 | P99 | Total | % |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 1.7 KB | 1.0 KB | 4.1 KB | 9.2 KB | 10.5 GB | 115% |
| `for_each_zoom_simplified` | 6.6M | 1.3 KB | 800 B | 3.1 KB | 7.0 KB | 8.2 GB | 90% |
| `emit_polygon_feature` | 4.5M | 1.4 KB | 794 B | 3.2 KB | 7.4 KB | 6.0 GB | 65% |
| `add_feature_to_layer` | 14.8M | 303 B | 60 B | 898 B | 4.5 KB | 4.2 GB | 46% |
| `merge_same_attr_geometries` | 306K | 11.5 KB | 3.4 KB | 63.8 KB | 282.2 KB | 3.4 GB | 37% |
| `clip_polygon_into` | 8.1M | 327 B | 304 B | 1.3 KB | 2.7 KB | 2.5 GB | 27% |
| `emit_line_feature` | 2.0M | 1.2 KB | 808 B | 3.0 KB | 6.0 KB | 2.3 GB | 25% |

Per-thread:

| Thread | Alloc | Dealloc | Diff |
|---|---|---|---|
| Main | 10.0 GB | 11.4 GB | −1.4 GB |
| Rayon ×3 | ~2.4 GB each | ~1.6 GB each | ~0.6 GB each |

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

| Commit | Change | PBF Phase | Total |
|---|---|---|---|
| dm6/mmap baseline | 96 GB sparse mmap | — | 242s |
| d65ee36 | SortedNodeStore | 13.3s | 17.2s |
| 647a360 | Double-buffer way batches | 10.1s | 15.8s |
| ca87a20 | Block-level dispatch | 9.3s | 15.1s |
| 31f7c14 | Iterator API (no perf change) | 9.3s | 15.1s |
| 15be3bf | Dedicated drain thread + land_mask to rayon | 8.6s | 14.4s |
| 5c45361 | BlockType API (no perf change) | 8.6s | 14.4s |

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
