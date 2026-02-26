# Hotpath Profile — Denmark

Dataset: `denmark-latest.osm.pbf` (483 MB), 52.5M nodes, 6.6M ways, 46K relations.
Machine: plantasjen, 30 GB DDR4, NVMe, Ryzen 9 5900X (12c/24t).
SortedNodeStore: ~420 MB in-RAM (bitmask+popcount).

## Current Baseline (2026-02-26, commit d65ee36)

Wall time: **17.2s**. phase12=13.3s (77.4%), ocean=0.8s, sort=0.3s, assemble=2.1s.
14.8M features, 56K tiles (54K unique), 283 MB output. RSS: 768 MB.

### Function Timing (top 10 of 26 measured)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % Wall |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 9.10µs | 2.11µs | 22.89µs | 176.51µs | 60.2s | 349% |
| `for_each_zoom_simplified` | 6.6M | 3.83µs | 770ns | 5.18µs | 75.07µs | 25.2s | 146% |
| `emit_polygon_feature` | 4.5M | 4.90µs | 830ns | 7.28µs | 113.73µs | 22.3s | 129% |
| `phase_read_and_process` | 1 | 13.35s | — | — | — | 13.35s | 77.4% |
| `emit_line_feature` | 2.0M | 5.98µs | 1.07µs | 7.43µs | 130.62µs | 12.1s | 70.5% |
| `flush_raw_way_batch` | 808 | 9.41ms | 7.06ms | 11.76ms | 47.87ms | 7.6s | 44.1% |
| `match_element` | 10.1M | 287ns | 260ns | 600ns | 1.10µs | 2.9s | 16.9% |
| `add_feature_to_layer` | 14.8M | 187ns | 130ns | 450ns | 860ns | 2.8s | 16.0% |

>100% totals = parallel work on rayon threads. % is CPU-time / wall-time.

### Thread Utilization

| Thread | CPU% | User | Sys | Total |
|---|---|---|---|---|
| Main | 23–101% | 7.3s | 3.6s | 10.9s |
| Rayon ×3 | 47–50% | ~3.9s | ~0.1s | ~4.0s each |

**CPU-dominated.** Main thread 33% kernel time (sort file I/O + way index mmap writes).
Rayon workers ~98% user — pure CPU work, no I/O stalls. Total CPU across all threads:
~23s on 17.2s wall = ~1.3× parallelism utilization (limited by serial PBF callback).

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
(3.6s / 10.9s = 33%) is sort file I/O and way index mmap writes.

### 2. Simplification is the #1 CPU consumer

`for_each_zoom_simplified` (25.2s total CPU, 146% wall) dominates feature processing.
Douglas-Peucker runs at each zoom level from z14 down to z_lo. Pre-DP subpixel bbox
check and DP convergence tracking reduce unnecessary work, but DP itself is inherently
O(n²) per level. No further algorithmic improvements available (VW tried and reverted).

### 3. Polygons dominate the workload

4.5M polygon features vs 2.0M line features. `emit_polygon_feature` (22.3s) is 1.8×
`emit_line_feature` (12.1s). Each polygon also calls `clip_polygon_into` (~2 clips per
polygon across tiles).

### 4. Allocation throughput dropped 29%

Global throughput: 58.9 GB → 41.6 GB (−29%). SortedNodeStore lookups are pure pointer
chasing — no allocation on read path. The remaining allocation is inherent to the data
model: geometry Vecs, sort record Vecs, MVT encode buffers. No low-hanging fruit.

### 5. Parallelism is bottlenecked by serial PBF callback

Total CPU across threads ~23s on 17.2s wall = ~1.3× utilization. The serial PBF callback
(handle_node! + handle_way!) feeds rayon batches sequentially. Rayon workers are idle
between batches. This is inherent to pbfhogg's `for_each_pipelined` architecture —
the callback runs on the main thread, only the batch processing is parallel.

### 6. Planet-scale projection

Denmark is ~1/150th of planet by PBF size. Extrapolating: ~6.2 TB of allocator throughput
at planet scale. SortedNodeStore for planet (8.5B nodes × 8 bytes = 68 GB uncompressed)
won't fit in 64 GB RAM — needs StreamVByte delta compression (~40 GB estimate).

## Comparison vs Previous Profile (dm6/mmap → plantasjen/SortedNodeStore)

Previous: commit 2ba4c0f on dm6 (32 GB RAM, Ryzen 5 5600G), 96 GB sparse mmap node index.
Current: commit d65ee36 on plantasjen (30 GB RAM, Ryzen 9 5900X), SortedNodeStore in-RAM.

**Note:** Different machines. CPU speedup is partly from faster CPU (5900X vs 5600G), but
the dominant factor is eliminating mmap page faults. The mmap variant on plantasjen ran at
~24s (mmap fits in page cache on 30 GB RAM), so the SortedNodeStore saved ~7s even without
page fault pressure.

| Metric | dm6/mmap | plantasjen/sorted | Change |
|---|---|---|---|
| Wall time | 242.3s | 17.2s | **−93%** |
| `process_raw_way` avg | 750µs | 9.10µs | −99% |
| `process_raw_way` total | 4962s | 60.2s | −99% |
| `flush_raw_way_batch` avg | 274.5ms | 9.41ms | −97% |
| `for_each_zoom_simplified` total | 64.6s | 25.2s | −61% |
| `emit_polygon_feature` total | 40.0s | 22.3s | −44% |
| `emit_line_feature` total | 30.3s | 12.1s | −60% |
| `match_element` total | 14.0s | 2.9s | −79% |
| Rayon sys% | ~97% | ~2% | CPU-bound |
| Alloc throughput | 58.9 GB | 41.6 GB | −29% |
| RSS (alloc) | 1.4 GB | 2.2 GB | +57% |

RSS increase is expected: SortedNodeStore holds ~420 MB in-RAM vs mmap which pages on
demand. The 2.2 GB RSS reflects SortedNodeStore + system allocator overhead (no mimalloc
in alloc profiling mode).

## Historical Comparison (commit 96274ae → 2ba4c0f, dm6)

Changes: projection LUT, pre-DP subpixel check, buffer hoisting, `with_capacity`,
`Box<[...]>`, `codegen-units=1`, `panic=abort`.

| Metric | Before | After | Change |
|---|---|---|---|
| Wall time | 289.4s | 242.3s | **−16%** |
| `for_each_zoom_simplified` total | 83.0s | 64.6s | −22% |
| `emit_polygon_feature` total | 54.0s | 40.0s | −26% |
| `emit_line_feature` total | 39.4s | 30.3s | −23% |
| `clip_polygon_into` total | 11.3s | 9.4s | −17% |
| RSS (alloc) | 1.7 GB | 1.4 GB | −18% |

## Remaining Opportunities

1. ~~**Visvalingam-Whyatt**~~ — Tried and reverted. VW's allocation overhead (5 Vecs +
   BinaryHeap per call) exceeds DP savings for small geometries (avg ~10 vertices).
   Non-cascading VW also produces +7 MB output at low zooms. See `notes/vw-simplification-experiment.md`.
2. **Polygon-focused optimization** — polygons are 1.8× the total CPU of lines, but
   per-feature cost is similar. The ratio is mostly feature count (2.25×).
3. ~~**Node storage redesign**~~ — Done. SortedNodeStore replaces 96 GB sparse mmap with
   ~420 MB in-RAM hierarchical store. PBF phase: 16s → 10.8s on plantasjen. Total: 24s → 17s.
4. **StreamVByte delta compression** — needed for planet scale (68 GB uncompressed SortedNodeStore
   won't fit in 64 GB RAM). Delta compression estimate: ~40 GB. Not needed for extracts.
5. **PBF callback parallelism** — the serial callback is the main bottleneck now (1.3× utilization
   of available cores). Would require architectural changes to pbfhogg.

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
