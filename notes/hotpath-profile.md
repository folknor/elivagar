# Hotpath Profile — Denmark

Dataset: `denmark-latest.osm.pbf` (483 MB), 52.5M nodes, 6.6M ways, 46K relations.
Machine: plantasjen, 30 GB DDR4, NVMe, Ryzen 9 5900X (12c/24t).
SortedNodeStore: ~420 MB in-RAM (bitmask+popcount).

## Current Baseline (2026-02-26, commit 31f7c14)

Wall time: **14.6s**. phase12=11.5s (79%), ocean=16ms, sort=0.3s, assemble=2.0s.
14.8M features, 56K tiles (54K unique), 283 MB output. RSS: 1.2 GB.

Architecture: block-level dispatch via `into_blocks_pipelined`. Node blocks processed
inline, way blocks sent to a worker thread via `sync_channel(1)`, worker extracts
RawWay + runs rayon `par_iter`. Main thread drains results between blocks.

### Function Timing (top 10 of 28 measured)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % Wall |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 9.12µs | 2.13µs | 22.50µs | 172.29µs | 60.3s | 414% |
| `for_each_zoom_simplified` | 6.6M | 3.85µs | 790ns | 5.34µs | 71.49µs | 25.3s | 174% |
| `emit_polygon_feature` | 4.5M | 4.82µs | 850ns | 7.42µs | 106.62µs | 21.9s | 150% |
| `emit_line_feature` | 2.0M | 6.12µs | 1.08µs | 7.49µs | 128.57µs | 12.4s | 85% |
| `drain_way_results_nonblocking` | 828 | 5.49ms | 2.41ms | 19.42ms | 27.61ms | 4.55s | 31% |
| `drain_processed_ways` | 828 | 5.49ms | 3.99ms | 8.23ms | 13.59ms | 4.55s | 31% |
| `match_element` | 10.1M | 291ns | 260ns | 620ns | 1.12µs | 2.95s | 20% |
| `add_feature_to_layer` | 14.8M | ~190ns | ~130ns | ~450ns | ~860ns | ~2.8s | ~19% |

>100% totals = parallel work on rayon threads. % is CPU-time / wall-time.

### Drain Analysis

828 way blocks from the PBF (natural batching by PBF block boundaries, ~8000 ways/block).
`drain_way_results_nonblocking` and `drain_processed_ways` have identical totals (4.55s) —
the non-blocking drain always finds exactly 1 result ready per call (no empty polls).
`drain_way_results_blocking` called once at way→relation transition, negligible.

The drain accounts for **39% of PBF phase time** (4.55s / 11.5s). This is the serial
bottleneck: way_index.put() + sort_writer.push() + land_mask.mark_bbox() cannot be
parallelized (all take `&mut self`).

### Thread Utilization

| Thread | CPU% | User | Sys | Total |
|---|---|---|---|---|
| Main | 15–101% | 6.3s | 3.7s | 10.0s |
| Rayon ×3 | 74–80% | ~3.8s | ~0.1s | ~3.9s each |

Main thread 37% kernel time (sort file I/O + way index mmap writes). Rayon workers
~98% user — pure CPU, no I/O stalls. Total CPU across all threads: ~22s on 14.6s wall
= ~1.5× parallelism utilization (up from ~1.3× before double-buffering).

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
(dm6, mmap) to 9.1µs — 82× faster per call. The remaining sys time on main thread
(3.7s / 10.0s = 37%) is sort file I/O and way index mmap writes.

### 2. Simplification is the #1 CPU consumer

`for_each_zoom_simplified` (25.3s total CPU, 174% wall) dominates feature processing.
Douglas-Peucker runs at each zoom level from z14 down to z_lo. Pre-DP subpixel bbox
check and DP convergence tracking reduce unnecessary work, but DP itself is inherently
O(n²) per level. No further algorithmic improvements available (VW tried and reverted).

### 3. Polygons dominate the workload

4.5M polygon features vs 2.0M line features. `emit_polygon_feature` (21.9s) is 1.8×
`emit_line_feature` (12.4s). Each polygon also calls `clip_polygon_into` (~2 clips per
polygon across tiles).

### 4. Serial drain is the parallelism bottleneck

`drain_processed_ways` takes 4.55s (39% of PBF phase), all on the main thread. This is
`way_index.put()` (mmap write) + `sort_writer.push()` (file I/O) + `land_mask.mark_bbox()`.
All three require `&mut self`. The worker thread and rayon are idle while draining.
Further overlap is limited by this serial dependency.

### 5. Block-level dispatch improved parallelism

Total CPU ~22s on 14.6s wall = **1.5× utilization** (was 1.3× before double-buffering).
The main thread no longer does per-way extraction (String copies for tags, Vec for
node_refs). It just classifies blocks by peeking first element and sends way blocks to
the worker via channel. The worker does extraction + rayon processing.

### 6. Planet-scale projection

Denmark is ~1/150th of planet by PBF size. Extrapolating: ~6.2 TB of allocator throughput
at planet scale. SortedNodeStore for planet (8.5B nodes × 8 bytes = 68 GB uncompressed)
won't fit in 64 GB RAM — needs StreamVByte delta compression (~40 GB estimate).

## Performance History

| Commit | Change | PBF Phase | Total |
|---|---|---|---|
| dm6/mmap baseline | 96 GB sparse mmap | — | 242s |
| d65ee36 | SortedNodeStore | 13.3s | 17.2s |
| 647a360 | Double-buffer way batches | 10.1s | 15.8s |
| ca87a20 | Block-level dispatch | 9.3s | 15.1s |
| 31f7c14 | Iterator API (no perf change) | 9.3s | 15.1s |

## Remaining Opportunities

1. ~~**Visvalingam-Whyatt**~~ — Tried and reverted. VW's allocation overhead (5 Vecs +
   BinaryHeap per call) exceeds DP savings for small geometries (avg ~10 vertices).
   Non-cascading VW also produces +7 MB output at low zooms. See `notes/vw-simplification-experiment.md`.
2. **Polygon-focused optimization** — polygons are 1.8× the total CPU of lines, but
   per-feature cost is similar. The ratio is mostly feature count (2.25×).
3. ~~**Node storage redesign**~~ — Done. SortedNodeStore replaces 96 GB sparse mmap with
   ~420 MB in-RAM hierarchical store (bitmask+popcount). PBF phase −5.2s.
4. **StreamVByte delta compression** — needed for planet scale (68 GB uncompressed SortedNodeStore
   won't fit in 64 GB RAM). Delta compression estimate: ~40 GB. Not needed for extracts.
5. ~~**PBF callback parallelism**~~ — Done. Block-level dispatch via `into_blocks_pipelined`.
   Way blocks sent to worker thread, main thread drains results in parallel. PBF phase: 13.3s → 9.3s.
6. **Reduce serial drain cost** — `drain_processed_ways` takes 4.55s (39% of PBF phase).
   `way_index.put()` and `sort_writer.push()` are `&mut self` — cannot parallelize directly.
   Possible approaches: batch I/O writes, reduce way_index write volume, defer sort pushes.

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
