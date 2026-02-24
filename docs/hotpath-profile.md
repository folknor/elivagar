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

## Implied Priorities

1. **Reduce node index I/O pressure** — prefetch, batch lookups, or restructure to avoid
   random mmap faults. This is 54% of main thread time.
2. **Increase PBF phase parallelism** — move tag matching/collection off the serial callback.
   Currently only ~2 of 28 cores are utilized.
3. **Optimize simplification for large features** — the P99 tail in `for_each_zoom_simplified`
   is where most CPU goes. Early termination, incremental simplification, or subpixel culling
   at coarser zooms could help.
4. **Polygon-focused optimization** — polygons are 2x the workload of lines. Any polygon-specific
   improvement (e.g. ring area pre-filter before per-zoom processing) has outsized impact.
