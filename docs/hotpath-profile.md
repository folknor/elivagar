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

Total allocated: **14.0 GB** for Denmark. Global throughput: **55.1 GB alloc, 64.0 GB dealloc.**

### Allocation by function (cumulative, top 10 of 25)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 1.8 KB | 1.0 KB | 4.9 KB | 11.0 KB | 11.3 GB | 238% |
| `blob::decode_blob` (pbfhogg) | 7.4K | 1.3 MB | 751 KB | 4.8 MB | 5.4 MB | 9.2 GB | 195% |
| `for_each_zoom_simplified` | 6.6M | 1.4 KB | 828 B | 3.8 KB | 8.8 KB | 9.1 GB | 191% |
| `emit_polygon_feature` | 4.5M | 1.5 KB | 800 B | 3.8 KB | 9.0 KB | 6.3 GB | 133% |
| `emit_line_feature` | 2.0M | 1.4 KB | 828 B | 3.9 KB | 8.5 KB | 2.8 GB | 58% |
| `clip_polygon_into` | 8.9M | 297 B | 304 B | 1.3 KB | 2.7 KB | 2.5 GB | 52% |

Note: cumulative means parent includes children. Exclusive allocations are the deltas.

### Per-thread allocation

| Thread | Alloc | Dealloc | Diff |
|---|---|---|---|
| Main | 5.6 GB | 15.0 GB | -9.4 GB |
| Rayon worker ×4 | ~3.4 GB each | ~2.9 GB each | ~490 MB each |

RSS: 2.0 GB. System allocator is much less memory-efficient than mimalloc.

### Key allocation insights

#### 1. clip_polygon was the #1 exclusive allocator — FIXED

Was 8.9M calls × 530 B avg = 4.4 GB. Added `clip_polygon_into()` with reusable double-buffers
hoisted in `emit_polygon_feature` and `emit_multipolygon_feature`. After fix: **2.5 GB**
(297 B avg). Residual is first-call growth per rayon thread. Global throughput dropped
67.3 GB → 63.1 GB (−4.2 GB).

#### 2. simplify() was the #2 exclusive allocator — FIXED

Was allocating a `vec![bool]` keep array and result `Vec<Point>` per call from
`for_each_zoom_simplified`. Added `simplify_into()` with reusable buffers hoisted outside
the zoom loop, using `swap` instead of re-allocating each level. After fix:
`for_each_zoom_simplified` dropped 10.3 GB → **9.1 GB** (−1.2 GB). Global throughput
63.1 GB → **62.6 GB** (−0.5 GB).

#### 3. Parallel node lookups reduced global throughput — DONE

Moved node coord resolution + tag matching from the serial PBF callback into rayon
batches (`process_raw_way` replaces `process_matched_way`). `NodeIndex` converted to
read-only `NodeIndexReader` (Sync) after node phase. Global alloc throughput dropped
62.6 GB → **55.1 GB** (−7.5 GB, −12%). `emit_polygon_feature` dropped 8.7 GB → 6.3 GB.
Rayon workers went from ~3.5s CPU each to **~10.3s each** (3× more utilized).

#### 4. pbfhogg blob decoding is 9.2 GB — being improved upstream

7.4K blobs × 1.3 MB avg. This is zlib decompression buffers inside pbfhogg. Work is
underway in pbfhogg to reduce this.

#### 5. Planet-scale projection

Denmark is ~1/150th of planet by PBF size. Extrapolating naively: **~1.3 TB of allocator
throughput** at planet scale. Even mimalloc will feel pressure at that level — contention
across rayon threads, TLB misses from fragmentation, and RSS bloat from thread-local heaps.

## Implied Priorities

1. ~~**Eliminate clip_polygon per-call allocation**~~ — Done. 4.4 GB → 2.5 GB.
2. ~~**Eliminate simplify per-call allocation**~~ — Done. 10.3 GB → 9.1 GB cumulative.
3. ~~**Move node lookups into rayon to solve I/O + parallelism together**~~ — Done.
   Node coord resolution + tag matching moved from serial PBF callback into rayon batches.
   `NodeIndex` → read-only `NodeIndexReader` (Sync) after node phase. Rayon workers went
   from ~3.5s to ~10.3s CPU each (3× more utilized). Global alloc throughput −12%.
   Denmark PBF phase neutral (~18s); planet-scale benefit expected to be significant
   (page faults spread across threads instead of serializing one thread).
4. **Optimize simplification for large features** — the P99 tail in `for_each_zoom_simplified`
   is where most CPU goes. Early termination, incremental simplification, or subpixel culling
   at coarser zooms could help.
5. **Polygon-focused optimization** — polygons are 2x the workload of lines. Any polygon-specific
   improvement (e.g. ring area pre-filter before per-zoom processing) has outsized impact.
