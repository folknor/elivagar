# Hotpath Profile — Denmark (2026-02-25)

Dataset: `denmark-latest.osm.pbf` (483 MB), 52.5M nodes, 6.6M ways, 46K relations.
Machine: 64 GB RAM, system under moderate load (not a clean baseline).

Wall time: 29.1s total. phase12=24.5s (84%), ocean=0.7s, sort=0.5s, assemble=1.8s.

## Function Timing (top 10 of 28 measured)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % Wall |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 12.64µs | 2.13µs | 31.31µs | 215.29µs | 83.7s | 288% |
| `phase_read_and_process` | 1 | 24.5s | — | — | — | 24.5s | 84% |
| `for_each_zoom_simplified` | 6.6M | 3.34µs | 890ns | 7.47µs | 39.39µs | 22.0s | 76% |
| `emit_polygon_feature` | 4.5M | 4.31µs | 961ns | 10.02µs | 66.11µs | 19.6s | 67% |
| `flush_raw_way_batch` | 808 | 12.85ms | 8.95ms | 21.23ms | 50.17ms | 10.4s | 36% |
| `emit_line_feature` | 2.0M | 4.13µs | 1.22µs | 7.37µs | 53.34µs | 8.4s | 29% |
| `add_feature_to_layer` | 14.9M | 274ns | 190ns | 620ns | 1.14µs | 4.1s | 14% |

>100% totals = parallel work on rayon threads. % is CPU-time / wall-time.

**Newly instrumented:** `add_feature_to_layer` (wire format decode), `multipolygon::assemble`,
`merge_same_attr_geometries`. Only `add_feature_to_layer` ranked — the other two are below
the top 10 threshold, confirming they are not bottlenecks.

**Dropped out:** `prepare_relation` (was 4.0s / 12%) — relation tag cloning fix and early-exit
for unmatched relations reduced it below the top 10.

## Thread Utilization

| Thread | CPU% | User | Sys | Total |
|---|---|---|---|---|
| Main | 20–100% | 8.67s | 10.94s | 19.61s |
| Rayon worker ×3 | 70–72% | ~3.3s | ~0.6s | ~3.9s each |

`process_raw_way` totals 83.7s CPU across threads, wall time 29.1s → **2.9× parallelism.**
Lower ratio than previous profile (3.5×) because `process_raw_way` got 20% faster per call
(pbfhogg improvements), not because parallelism regressed — same wall time, less total CPU.
Main thread still dominates at 19.6s (67% of wall), mostly kernel time (10.9s sys = 56%)
from mmap page faults on the node index.

## Key Insights

### 1. Node index mmap I/O still dominates the main thread

Main thread: **10.9s sys / 19.6s total = 56% kernel time.** Improved from 11.8s sys (was
54%) by parallel node lookups in rayon, but still the single biggest bottleneck. Rayon
workers at ~3.9s each — pipeline remains I/O-serialized by PBF read + node index page faults.

### 2. Simplification dominates CPU, not clipping

`for_each_zoom_simplified` (22.0s total) is the #1 CPU consumer among feature-processing
functions. Pre-DP subpixel bbox check reduced it from 31.5s (−35%). Douglas-Peucker runs
at each zoom level from z14 down to z_lo. `clip_polygon` is cheap — the inner
Sutherland-Hodgman loop is tight.

### 3. P50/P99 spread — long tail is genuine complexity

| Function | P50 | P99 | Ratio |
|---|---|---|---|
| `process_raw_way` | 2.13µs | 215µs | 101x |
| `emit_polygon_feature` | 961ns | 66µs | 69x |
| `emit_line_feature` | 1.22µs | 53µs | 43x |

P99 ratios were reduced by the subpixel check (was 112–160×). Remaining tail is genuine
complexity — coastlines, large buildings, long roads.

### 4. Tag matching and relation processing are NOT bottlenecks

`match_element` and `prepare_relation` both dropped out of the top 10. Relation tag cloning
fix (match during prepare, store results instead of all tags) and early-exit for unmatched
relations reduced `prepare_relation` from 4.0s to below the threshold.

### 5. Polygons dominate the workload

4.5M polygon features vs 2.0M line features. `emit_polygon_feature` (19.6s) is 2.3×
`emit_line_feature` (8.4s). Each polygon also calls `clip_polygon` (~2 clips per polygon
across tiles).

### 6. Assemble phase: `add_feature_to_layer` is the hot function

`phase_assemble` = 1.8s (6%). Within it, `add_feature_to_layer` (wire format decode →
Feature struct) accounts for 4.1s CPU across threads (14.9M calls, 274ns avg). Mostly
the per-feature `Vec<u32>` allocation for geometry commands. `merge_same_attr_geometries`
and `multipolygon::assemble` are both below the top 10 — confirmed not worth optimizing.

## Allocation Profile (hotpath-alloc, system allocator — no mimalloc)

Separate run with `--features hotpath-alloc`. Mimalloc disabled (hotpath-alloc provides its own
`#[global_allocator]`). Wall-clock times are not meaningful — only allocation counts and bytes.

Total allocated: **6.3 GB** for Denmark. Global throughput: **55.7 GB alloc, 55.7 GB dealloc.**

### Allocation by function (cumulative, top 10 of 25)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 1.8 KB | 1.0 KB | 4.9 KB | 11.0 KB | 11.3 GB | 180% |
| `for_each_zoom_simplified` | 6.6M | 1.4 KB | 828 B | 3.8 KB | 8.8 KB | 9.1 GB | 144% |
| `emit_polygon_feature` | 4.5M | 1.5 KB | 800 B | 3.8 KB | 9.0 KB | 6.3 GB | 100% |
| `emit_line_feature` | 2.0M | 1.4 KB | 828 B | 3.9 KB | 8.5 KB | 2.8 GB | 44% |
| `clip_polygon_into` | 8.9M | 297 B | 304 B | 1.3 KB | 2.7 KB | 2.5 GB | 40% |
| `mvt::encode_tile_with` | 55.9K | 33.9 KB | 9.3 KB | 140.4 KB | 404.0 KB | 1.8 GB | 29% |

Note: cumulative means parent includes children. Exclusive allocations are the deltas.

### Per-thread allocation

| Thread | Alloc | Dealloc | Diff |
|---|---|---|---|
| Main | 7.2 GB | 8.8 GB | -1.7 GB |
| Rayon worker ×4 | ~3.2 GB each | ~2.8 GB each | ~410 MB each |

RSS: 1.6 GB. System allocator is much less memory-efficient than mimalloc.

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

#### 4. pbfhogg wire parser eliminated protobuf Vec allocations — DONE

Was 9.2 GB (7.4K blobs × 1.3 MB avg) from `blob::decode_blob`. pbfhogg's new wire parser
eliminated ~9 GB of protobuf `Vec` allocations for packed repeated fields (node IDs, lats,
lons, etc.) by decoding varints on-the-fly instead of materializing intermediate vectors.
After fix: `decode_blob` dropped out of the top 10 entirely. Global dealloc throughput
dropped 64.0 GB → **55.7 GB** (−8.3 GB, −13%). RSS: 2.0 GB → **1.6 GB** (−20%).
At planet scale, this eliminates ~1.5 TB of cumulative allocations.

#### 5. Planet-scale projection

Denmark is ~1/150th of planet by PBF size. Extrapolating naively: **~1.1 TB of allocator
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
4. ~~**Eliminate projection transcendentals**~~ — Done. 18-bit LUT (262K entries, 2 MB)
   with linear interpolation replaces tan/cos/ln in `project_e7`. Error: 0.03 pixels at z14.
   `process_raw_way` avg −14% (29.65→25.50µs), P99 −55% (189.82→85.31µs) in alloc profile.
   No allocation regression. Planet-scale: eliminates ~387-775s of transcendental cost.
5. ~~**Pre-DP subpixel bbox check**~~ — Done. `merc_bbox_is_subpixel` checks cascade bbox
   before each DP call; breaks the zoom loop when geometry is < 1 pixel. Results:
   `for_each_zoom_simplified` total CPU −35% (31.5s→20.4s), P99 −63% (89µs→33µs).
   `emit_polygon_feature` −37%, `emit_line_feature` −45%. 630K fewer feature-zoom combos (−3.7%).
6. ~~**Eliminate relation tag cloning**~~ — Done. `match_element` moved into `prepare_relation`
   while PBF borrows alive; `PreparedRelation` stores match results instead of cloned tags.
   Early-exit for unmatched relations skips member way resolution. `prepare_relation` dropped
   from 4.0s (12%) to below top 10. `process_raw_way` avg −20% (15.73→12.64µs).
7. ~~**Recompute bbox from simplified cascade**~~ — Done. `emit_line_feature`,
   `emit_polygon_feature`, and `emit_multipolygon_feature` now recompute `merc_bbox` from
   simplified coords per zoom. Eliminates wasted tile iterations + S-H clipping at low zooms
   where DP reduces geometry extent. `bbox` parameter removed from all three functions.
8. ~~**DP max-deviation tracking (Option D) + vertex pre-check (Option E)**~~ — Done.
   `simplify_into` returns max squared deviation; cascade skips DP when deviation < next
   zoom's tolerance (converged). Pre-check skips DP when cascade ≤ min_points.
9. ~~**Eliminate intern_value String allocation on cache hit**~~ — Done. Added
   `intern_string_value(&str)` with separate `string_value_map` for zero-alloc lookup by
   borrowed `&str`. ~90% cache hit rate = ~27-40M saved alloc+dealloc cycles per Denmark run.
10. **Further simplification** — remaining option: Visvalingam-Whyatt (O(n log n) one-time
    importance, then threshold per zoom). Would replace DP entirely.
11. **Polygon-focused optimization** — polygons are 2.3× the total workload of lines, but
    per-feature cost is only 4% higher (4.31µs vs 4.13µs). The 2.3× ratio is almost entirely
    the 2.25× feature count. Optimizations that help both paths (simplification, outcode
    pre-test) have more impact than polygon-specific work.

## Benchmark — Denmark (2026-02-25, dm6)

Dataset: `denmark-latest.osm.pbf` (483 MB). Different host from earlier profiles.
Commit: `95ddf61` — includes intern_value fix, bbox recompute, DP D+E optimizations.

### Self-benchmark (best of 3)

| Phase | Time |
|---|---|
| **Total** | **49.1s** |
| PBF | 29.0s |
| Ocean | 8.6s |
| Sort | 1.0s |
| Assemble | 5.3s |
| Features | 16.3M |
| Tiles | 667K |
| Output | 375 MB |

### Hotpath profile (single run, no ocean — wall 38.3s)

| Function | Calls | Avg | P50 | P95 | P99 | Total | % Wall |
|---|---|---|---|---|---|---|---|
| `process_raw_way` | 6.6M | 5.16µs | 2.34µs | 11.34µs | 25.49µs | 34.1s | 89% |
| `for_each_zoom_simplified` | 6.6M | 2.77µs | 1.04µs | 6.58µs | 16.03µs | 18.2s | 47.5% |
| `emit_polygon_feature` | 4.5M | 2.92µs | 1.13µs | 7.55µs | 17.71µs | 13.3s | 34.7% |
| `emit_line_feature` | 2.0M | 3.33µs | 1.33µs | 6.05µs | 14.21µs | 6.8s | 17.6% |
| `flush_raw_way_batch` | 808 | 17.84ms | 12.78ms | 28.13ms | 65.96ms | 14.4s | 37.6% |

Thread utilization: main 28.4s (15–101% CPU), 3 rayon workers ~4.5s each (51–81% CPU).

Not directly comparable to plantasjen numbers due to different hardware, but serves as the
baseline for future optimizations on this host.
