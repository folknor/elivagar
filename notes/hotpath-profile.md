# Hotpath Profile — Denmark

Dataset: `denmark-latest.osm.pbf` (483 MB), 52.5M nodes, 6.6M ways, 46K relations.
Machine: folk-pc, 32 GB RAM, NVMe. Node index: 110 GB sparse file (3.4× RAM).
`MADV_RANDOM` active, `MADV_POPULATE_READ` skipped.

## Current Baseline (2026-02-25, commit 2ba4c0f)

Wall time: **242.3s**. phase12=236.7s (97.7%), ocean=0.5s, sort=1.1s, assemble=3.0s.
14.9M features, 56K tiles (54K unique), 323 MB output. RSS: 612 MB.

### Function Timing (top 10 of 26 measured)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % Wall |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 750µs | 5.56µs | 5.09ms | 13.35ms | 4962s | 2048% |
| `phase_read_and_process` | 1 | 236.7s | — | — | — | 236.7s | 97.7% |
| `flush_raw_way_batch` | 808 | 274.5ms | 221ms | 686ms | 1.34s | 221.8s | 91.6% |
| `for_each_zoom_simplified` | 6.6M | 9.82µs | 2.62µs | 26.21µs | 99.52µs | 64.6s | 26.7% |
| `emit_polygon_feature` | 4.5M | 8.80µs | 2.16µs | 25.31µs | 95.04µs | 40.0s | 16.5% |
| `emit_line_feature` | 2.0M | 14.92µs | 4.71µs | 32.99µs | 139.90µs | 30.3s | 12.5% |
| `match_element` | 10.1M | 1.38µs | 430ns | 5.85µs | 11.66µs | 14.0s | 5.8% |
| `clip_polygon_into` | 8.3M | 1.14µs | 310ns | 2.83µs | 7.88µs | 9.4s | 3.9% |

>100% totals = parallel work on rayon threads. % is CPU-time / wall-time.

### Thread Utilization

| Thread | CPU% | User | Sys | Total |
|---|---|---|---|---|
| Main | 24–100% | 8.2s | 10.3s | 18.4s |
| Rayon ×3 | 43–49% | ~5.0s | ~150s | ~155s each |

**I/O dominated.** 110 GB node index on 32 GB RAM → cold page faults dominate.
Main thread 56% kernel time. Rayon workers ~97% sys — waiting on mmap I/O.

### Allocation Profile (system allocator, no mimalloc)

Total allocated: **6.3 GB**. Global throughput: 58.9 GB alloc, 60.6 GB dealloc. RSS: 1.4 GB.

| Function | Calls | Avg | P50 | P95 | P99 | Total | % |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 1.8 KB | 1.0 KB | 5.0 KB | 11.3 KB | 11.4 GB | 181% |
| `for_each_zoom_simplified` | 6.6M | 1.5 KB | 807 B | 3.9 KB | 9.1 KB | 9.1 GB | 145% |
| `emit_polygon_feature` | 4.5M | 1.5 KB | 794 B | 3.8 KB | 9.2 KB | 6.3 GB | 101% |
| `add_feature_to_layer` | 14.9M | 316 B | 64 B | 926 B | 4.5 KB | 4.4 GB | 70% |
| `merge_same_attr_geometries` | 307K | 13.4 KB | 3.9 KB | 76.8 KB | 319 KB | 3.9 GB | 62% |
| `emit_line_feature` | 2.0M | 1.4 KB | 810 B | 4.1 KB | 9.0 KB | 2.8 GB | 45% |
| `clip_polygon_into` | 8.3M | 320 B | 304 B | 1.3 KB | 2.7 KB | 2.5 GB | 39% |

Per-thread:

| Thread | Alloc | Dealloc | Diff |
|---|---|---|---|
| Main | 7.2 GB | 9.2 GB | −2.0 GB |
| Rayon ×4 | ~3.1 GB each | ~2.7 GB each | ~465 MB each |

## Key Observations

### 1. I/O dominates — node index mmap page faults

Main thread: 10.3s sys / 18.4s total = 56% kernel time. Rayon workers spend ~97% of
their time in sys (mmap page faults). `process_raw_way` avg inflated from ~5µs (64 GB
RAM machine) to 750µs — 150× slowdown from I/O wait, not CPU regression.

### 2. Simplification is the #1 CPU consumer

`for_each_zoom_simplified` (64.6s total CPU) dominates feature processing. Douglas-Peucker
runs at each zoom level from z14 down to z_lo. Pre-DP subpixel bbox check and DP
convergence tracking reduce unnecessary work, but DP itself is inherently O(n²) per level.

### 3. Polygons dominate the workload

4.5M polygon features vs 2.0M line features. `emit_polygon_feature` (40.0s) is 1.3×
`emit_line_feature` (30.3s). Each polygon also calls `clip_polygon_into` (~2 clips per
polygon across tiles).

### 4. Allocation profile is flat

All major allocators unchanged since the buffer hoisting and LUT optimizations. The
remaining allocation is inherent to the data model: geometry Vecs, sort record Vecs,
MVT encode buffers. No low-hanging fruit.

### 5. Planet-scale projection

Denmark is ~1/150th of planet by PBF size. Extrapolating: ~1 TB of allocator throughput
at planet scale. On this machine (32 GB RAM, 110 GB node index), planet-scale PBF phase
would be ~95% I/O wait.

## Comparison vs Previous Profile (commit 96274ae → 2ba4c0f)

Changes between profiles: projection LUT, pre-DP subpixel check, buffer hoisting
(ocean.rs, pipeline.rs), `with_capacity`, `Box<[...]>`, `codegen-units=1`, `panic=abort`.

| Metric | Before | After | Change |
|---|---|---|---|
| Wall time | 289.4s | 242.3s | **−16%** |
| `for_each_zoom_simplified` total | 83.0s | 64.6s | −22% |
| `emit_polygon_feature` total | 54.0s | 40.0s | −26% |
| `emit_line_feature` total | 39.4s | 30.3s | −23% |
| `clip_polygon_into` total | 11.3s | 9.4s | −17% |
| RSS (timing) | — | 612 MB | — |
| RSS (alloc) | 1.7 GB | 1.4 GB | −18% |

Wall time improvement is a mix of CPU savings (LUT, subpixel check, DP convergence) and
compiler optimizations (`codegen-units=1`, `panic=abort`). I/O wait dominates on this
machine, so CPU savings are diluted — the same changes would show larger gains on a
machine where the node index fits in RAM.

## Remaining Opportunities

1. ~~**Visvalingam-Whyatt**~~ — Tried and reverted. VW's allocation overhead (5 Vecs +
   BinaryHeap per call) exceeds DP savings for small geometries (avg ~10 vertices).
   Non-cascading VW also produces +7 MB output at low zooms. See `notes/vw-simplification-experiment.md`.
2. **Polygon-focused optimization** — polygons are 1.3× the total CPU of lines, but
   per-feature cost is similar. The ratio is mostly feature count (2.25×).
3. **Planet-scale I/O** — on this machine, everything is I/O-bound. Faster storage or
   more RAM would have more impact than any code change.
4. ~~**Node storage redesign**~~ — Done. SortedNodeStore replaces 96 GB sparse mmap with
   ~420 MB in-RAM hierarchical store. PBF phase: 16s → 10.8s on plantasjen. Total: 24s → 17s.

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
