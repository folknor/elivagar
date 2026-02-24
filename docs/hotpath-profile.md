# Hotpath Profile — Denmark (2026-02-24)

Dataset: `denmark-latest.osm.pbf` (483 MB), 52.5M nodes, 6.6M ways, 46K relations.
Machine: 64 GB RAM, system under moderate load (not a clean baseline).

Wall time: 29.5s total. phase12=24.5s (83%), ocean=0.7s, sort=0.5s, assemble=2.1s.

## Function Timing (top 10 of 22 measured)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % Wall |
|---|---|---|---|---|---|---|---|
| `process_matched_way` | 6.1M | 9.03µs | 1.59µs | 20.78µs | 179.07µs | 55.3s | 187% |
| `for_each_zoom_simplified` | 6.6M | 4.79µs | 800ns | 8.38µs | 89.15µs | 31.5s | 107% |
| `emit_polygon_feature` | 4.5M | 6.08µs | 810ns | 12.14µs | 130.05µs | 27.7s | 94% |
| `phase_read_and_process` | 1 | 24.5s | — | — | — | 24.5s | 83% |
| `emit_line_feature` | 2.0M | 6.88µs | 1.20µs | 10.07µs | 143.36µs | 14.0s | 47% |
| `flush_way_batch` | 748 | 7.11ms | 4.26ms | 7.26ms | 46.53ms | 5.3s | 18% |
| `clip_polygon` | 8.9M | 331ns | 190ns | 720ns | 1.88µs | 2.9s | 10% |
| `phase_assemble` | 1 | 2.12s | — | — | — | 2.1s | 7% |

>100% totals = parallel work on rayon threads. % is CPU-time / wall-time.

## Thread Utilization

| Thread | CPU% | User | Sys | Total |
|---|---|---|---|---|
| Main | 26–100% | 9.97s | 11.81s | 21.78s |
| Rayon worker ×4 | 46–47% | ~3.4s | ~0.1s | ~3.5s each |

RSS: 414 MB. 28 threads total, most idle.

## Key Insights

### 1. Node index mmap I/O dominates the main thread

Main thread: **11.8s sys / 21.8s total = 54% kernel time.** This is mmap page faults on the
102 GB node index. The actual PBF decoding user-time is only ~10s. The node index I/O is
serializing the entire pipeline — rayon workers are starved.

### 2. Only ~2 cores effectively utilized during PBF phase

`process_matched_way` totals 55.3s across threads, wall time is 29.5s → 1.87x parallelism.
With 28 threads available, workers are idle most of the time, waiting for the serial PBF
read + node index lookups to produce batches.

### 3. Simplification dominates CPU, not clipping

`for_each_zoom_simplified` (31.5s total) is the #1 CPU consumer. It runs Douglas-Peucker
simplification at each zoom level from z14 down to z_lo. `clip_polygon` is cheap at 331ns
avg — the inner Sutherland-Hodgman loop is tight.

### 4. P50/P99 spread is 100x — long tail matters

| Function | P50 | P99 | Ratio |
|---|---|---|---|
| `process_matched_way` | 1.59µs | 179µs | 112x |
| `emit_polygon_feature` | 810ns | 130µs | 160x |
| `emit_line_feature` | 1.20µs | 143µs | 119x |

Most features are trivial (few nodes, few zoom levels). The P99 tail — complex coastlines,
large buildings, long roads — is where optimization effort should go.

### 5. Tag matching is NOT a bottleneck

`match_element` didn't make the top 10. Despite being called for every node/way/relation,
the linear scan over 3-15 tags is fast. This confirms reverting the binary search was correct.

### 6. Polygons dominate the workload

4.5M polygon features vs 2.0M line features. `emit_polygon_feature` (27.7s) is 2x
`emit_line_feature` (14.0s). Each polygon also calls `clip_polygon` (8.9M calls total,
~2 clips per polygon across tiles).

### 7. Assemble phase is not a bottleneck

`phase_assemble` = 2.1s (7%). MVT encoding + gzip + PMTiles write is fast. All optimization
effort should go into the PBF phase.

## Allocation Profile (hotpath-alloc, system allocator — no mimalloc)

Separate run with `--features hotpath-alloc`. Mimalloc disabled (hotpath-alloc provides its own
`#[global_allocator]`). Wall-clock times are not meaningful — only allocation counts and bytes.

Total allocated: **14.0 GB** for Denmark. Global throughput: **67.3 GB alloc, 75.4 GB dealloc.**

### Allocation by function (cumulative, top 10 of 25)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % |
|---|---|---|---|---|---|---|---|
| `process_matched_way` | 6.1M | 2.1 KB | 1.1 KB | 6.7 KB | 17.6 KB | 12.4 GB | 89% |
| `for_each_zoom_simplified` | 6.6M | 1.9 KB | 826 B | 5.9 KB | 15.7 KB | 11.6 GB | 83% |
| `blob::decode_blob` (pbfhogg) | 7.4K | 1.4 MB | 853 KB | 5.2 MB | 5.9 MB | 10.2 GB | 73% |
| `emit_polygon_feature` | 4.5M | 2.0 KB | 932 B | 6.7 KB | 18.5 KB | 8.7 GB | 63% |
| `clip_polygon` | 8.9M | 530 B | 320 B | 1.2 KB | 3.2 KB | 4.4 GB | 31% |
| `emit_line_feature` | 2.0M | 1.5 KB | 824 B | 4.2 KB | 9.1 KB | 2.9 GB | 21% |

Note: cumulative means parent includes children. Exclusive allocations are the deltas.

### Per-thread allocation

| Thread | Alloc | Dealloc | Diff |
|---|---|---|---|
| Main | 15.7 GB | 25.6 GB | -9.9 GB |
| Rayon worker ×4 | ~3.3 GB each | ~2.9 GB each | ~450 MB each |

RSS: 1.6 GB. System allocator is much less memory-efficient than mimalloc.

### Key allocation insights

#### 1. clip_polygon is the #1 exclusive allocator

8.9M calls × 530 B avg = **4.4 GB.** This is `input = ring.to_vec()` (the double-buffer
input copy) plus the output Vec created each call. Fix: pass in reusable double-buffers
from the caller so the Vecs grow to max size and stop allocating.

#### 2. simplify() is the #2 exclusive allocator

Called from `for_each_zoom_simplified` at every zoom level. Allocates a `vec![bool]` keep
array and a result `Vec<Point>` per invocation. 6.6M calls × ~1.4 KB exclusive (subtracting
clip_polygon's share) ≈ **~7 GB**. Fix: bitset for keep array + reusable output buffer.

#### 3. pbfhogg blob decoding is 10.2 GB — out of our hands

7.4K blobs × 1.4 MB avg. This is zlib decompression buffers inside pbfhogg. We don't
control this unless we modify pbfhogg.

#### 4. Planet-scale projection

Denmark is ~1/150th of planet by PBF size. Extrapolating naively: **~2 TB of allocator
throughput** at planet scale. Even mimalloc will feel pressure at that level — contention
across rayon threads, TLB misses from fragmentation, and RSS bloat from thread-local heaps.

## Implied Priorities

1. **Eliminate clip_polygon per-call allocation** — 4.4 GB from 8.9M calls. Pass reusable
   double-buffers from the caller. Highest bang-for-buck change.
2. **Eliminate simplify per-call allocation** — ~7 GB exclusive from 6.6M calls. Bitset for
   keep array + reusable output buffer.
3. **Reduce node index I/O pressure** — 54% of main thread CPU is kernel time from mmap
   page faults. Prefetch, batch lookups, or restructure to reduce random access.
4. **Increase PBF phase parallelism** — only ~2 of 28 cores utilized. Move tag
   matching/collection off the serial callback into parallel batches.
5. **Optimize simplification for large features** — the P99 tail in `for_each_zoom_simplified`
   is where most CPU goes. Early termination, incremental simplification, or subpixel culling
   at coarser zooms could help.
6. **Polygon-focused optimization** — polygons are 2x the workload of lines. Any polygon-specific
   improvement (e.g. ring area pre-filter before per-zoom processing) has outsized impact.
