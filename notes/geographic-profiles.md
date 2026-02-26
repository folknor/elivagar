# Geographic profiling

Tracking how different regions exercise the pipeline. Goal: find scaling issues
and code paths that Denmark alone doesn't stress.

## Hosts

### dm6
- CPU: AMD Ryzen 5 5600G (6 cores / 12 threads, 4.46 GHz boost)
- RAM: 32 GB DDR4
- Disk: 915 GB NVMe (nvme0n1p2)
- OS: Linux 6.19.0-3-generic x86_64

### plantasjen
- CPU: AMD Ryzen 9 5900X (12 cores / 24 threads, 4.95 GHz boost)
- RAM: 30 GB DDR4

## Regions tested

### Denmark (460 MB PBF) — baseline
- Host: dm6
- Profile: Coastal islands, moderate urban density, well-mapped
- Stresses: Ocean phase (islands), balanced pipeline
- See `hotpath-profile.md` for detailed Denmark profiles

### Germany (4.4 GB PBF) — 10x scale test
- Host: dm6
- Profile: Dense urban (Berlin, Munich, Hamburg), forests, complex multipolygons, mostly inland
- Stresses: Scale (10x features), assemble phase (dense tiles), multipolygon assembly

#### Run results (release, no hotpath)
- Total: 161.8s
- PBF: 116.3s (72%), Ocean: 5.9s (4%), Sort: 0.2s (<1%), Assemble: 35.5s (22%)
- Features: 146.8M, Unique tiles: 225.6K, Output: 2.6 GB
- Nodes: 429M, Ways: 69.6M, Relations: 882K
- RSS: ~1.4 GB — no memory pressure

#### Hotpath timing (dm6)
- Total: 135.3s wall
- PBF: 100.6s (74%), Ocean: 2.1s (2%), Sort: 0.2s (<1%), Assemble: 32.1s (24%)
- Top functions:
  - `process_raw_way`: 69.6M calls, 4.25 µs avg, 295.6s cumulative
  - `for_each_zoom_simplified`: 69.5M calls, 1.47 µs avg, 102.3s
  - `emit_polygon_feature`: 46.6M calls, 1.58 µs avg, 73.7s
  - `match_element`: 90.5M calls, 506 ns avg, 45.8s
  - `drain_processed_ways`: 8.7K calls, 5.13 ms avg, 44.6s
  - `emit_line_feature`: 22.9M calls, 1.90 µs avg, 43.4s
  - `add_feature_to_layer`: 146M calls, 269 ns avg, 39.4s

#### Hotpath alloc (dm6)
- Total alloc throughput: 35.9 GB (281.5 GB across threads)
- RSS: 9.9 GB (without mimalloc)
- Top allocators:
  - `process_raw_way`: 82.6 GB (1.2 KB avg)
  - `for_each_zoom_simplified`: 54.8 GB (847 B avg)
  - `add_feature_to_layer`: 38.6 GB (283 B avg)
  - `emit_polygon_feature`: 34.7 GB (800 B avg)
  - `merge_same_attr_geometries`: 24.7 GB (13.8 KB avg)
  - `emit_line_feature`: 20.1 GB (942 B avg)
  - `encode_tile_with`: 11.7 GB (54.1 KB avg)

#### Observations
- Profile shape identical to Denmark — same functions dominate, no new bottlenecks
- Assemble phase grew from ~2% to 24% of wall time (denser tiles)
- Sort remains negligible
- Linear scaling: 10x data ≈ 10-12x wall time
- SortedNodeStore handled 429M nodes comfortably in 32 GB RAM

### Norway (1.3 GB PBF) — ocean/geometry stress test
- Host: dm6
- Profile: Fjords (thousands of vertices per coastline polygon), sparse inland
- Stresses: Simplification, ocean phase (fjord coastlines), polygon clipping (multi-tile geometries)

#### Run results (release)
- Total: 55.7s
- PBF: 30.0s (54%), Ocean: 16.6s (30%), Sort: 0.3s (<1%), Assemble: 6.9s (12%)
- Features: 28.3M (24.3M PBF + 4.0M ocean), Unique tiles: 518.8K, Output: 1.0 GB
- Nodes: 208M, Ways: 12.0M, Relations: 777K
- RSS: ~1.7 GB

#### Hotpath timing (dm6)
- Total: 58.8s wall
- PBF: 33.3s (57%), Ocean: 16.2s (28%), Sort: 0.2s (<1%), Assemble: 7.1s (12%)
- Top functions — COMPLETELY DIFFERENT from Germany:
  - `for_each_zoom_simplified_multi`: 788K calls, **249 µs avg**, 195.8s (333%!)
  - `emit_ocean_polygon`: 317K calls, **521 µs avg**, 165.0s (280%)
  - `clip_polygon_into`: 31.8M calls, 3.29 µs avg, 104.6s (178%)
  - `process_raw_way`: 12.0M calls, 4.46 µs avg, 53.6s
  - `process_prepared_relation`: 464K calls, **86 µs avg**, 40.1s
  - `emit_multipolygon_feature`: 471K calls, **68 µs avg**, 31.9s

#### Hotpath alloc (dm6)
- Total alloc throughput: 11.4 GB (295.5 GB across threads!)
- RSS: 3.5 GB (without mimalloc)
- Top allocators — ocean clipping dominates:
  - `for_each_zoom_simplified_multi`: 788K calls, **277 KB avg**, 208.4 GB (1830%!)
  - `clip_polygon_into`: 31.8M calls, **6.5 KB avg**, 196.9 GB (1729%)
  - `emit_ocean_polygon`: 317K calls, **644 KB avg**, 194.4 GB (1707%)
  - `process_prepared_relation`: 464K calls, 57.6 KB avg, 25.5 GB
  - `process_raw_way`: 12.0M calls, 1.6 KB avg, 18.7 GB
  - `multipolygon::assemble`: 464K calls, 25.8 KB avg, 11.4 GB

#### Observations
- **Radically different profile from Germany/Denmark.** Ocean+geometry dominates, not PBF.
- Ocean phase is 30% of wall time (vs 2-4% for Germany). Fjord polygons are massive.
- `clip_polygon_into` allocates **197 GB** — fjord polygons spanning many tiles at every zoom.
- `for_each_zoom_simplified_multi` avg call is 249 µs (vs 1.5 µs for `_simplified`).
  Multipolygon simplification is the hotspot, not single-way simplification.
- Relations are significant: 464K `process_prepared_relation` calls at 86 µs avg.
- Assemble phase is only 12% — tiles are sparse (lots of ocean, few features per tile).
- Thread alloc 295.5 GB total despite only 11.4 GB cumulative main — massive parallel churn.

### Japan (2.3 GB PBF) — urban density + archipelago
- Host: dm6
- Profile: Extremely dense urban (Tokyo), complex archipelago, CJK tags, dense POIs
- Stresses: PBF processing (dense features), ocean (archipelago), tag matching

#### Run results (release)
- Total: 69.6s
- PBF: 41.4s (60%), Ocean: 8.4s (12%), Sort: 0.8s (1%), Assemble: 17.0s (24%)
- Features: 74.4M (73.5M PBF + 0.9M ocean), Unique tiles: 182.3K, Output: 1.2 GB
- Nodes: 301M, Ways: 42.9M, Relations: 217K
- RSS: ~2.0 GB

#### Hotpath timing (dm6)
- Total: 79.4s wall
- PBF: 48.3s (61%), Ocean: 8.6s (11%), Sort: 0.8s (1%), Assemble: 19.3s (24%)
- Top functions — hybrid profile (PBF + ocean geometry):
  - `process_raw_way`: 42.9M calls, 3.44 µs avg, 147.8s (186%)
  - `for_each_zoom_simplified_multi`: 205K calls, **454 µs avg**, 92.8s (117%)
  - `emit_ocean_polygon`: 118K calls, **733 µs avg**, 86.8s (109%)
  - `for_each_zoom_simplified`: 42.7M calls, 1.33 µs avg, 56.7s
  - `emit_polygon_feature`: 30.3M calls, 1.19 µs avg, 35.9s
  - `emit_line_feature`: 12.4M calls, 2.42 µs avg, 30.0s
  - `drain_processed_ways`: 5.4K calls, 5.16 ms avg, 27.7s

#### Hotpath alloc (dm6)
- Total alloc throughput: 14.2 GB (194.5 GB across threads)
- RSS: 2.1 GB (without mimalloc)
- Top allocators — ocean clipping still huge despite fewer polygons:
  - `for_each_zoom_simplified_multi`: 205K calls, 245 KB avg, 47.8 GB (336%)
  - `clip_polygon_into`: 6.0M calls, 8.1 KB avg, 46.4 GB (326%)
  - `process_raw_way`: 42.9M calls, 1.1 KB avg, 46.2 GB (325%)
  - `emit_ocean_polygon`: 118K calls, 400 KB avg, 45.2 GB (317%)
  - `for_each_zoom_simplified`: 42.7M calls, 800 B avg, 31.8 GB
  - `add_feature_to_layer`: 73.7M calls, 280 B avg, 19.3 GB
  - `merge_same_attr_geometries`: 1.1M calls, 14.9 KB avg, 16.0 GB

#### Observations
- Hybrid profile: PBF-heavy like Germany, but ocean geometry is significant (unlike Germany).
- `emit_ocean_polygon` avg 733 µs — even higher than Norway per-call. Fewer but larger polygons.
- Sort phase 0.8s — first time it's measurable. 74M features produce more chunks.
- Assemble 24% — same as Germany. Dense urban tiles drive MVT encoding.
- `emit_line_feature` avg 2.42 µs — higher than Germany (1.90 µs). Longer roads/railways?

## Cross-region comparison (all dm6)

| Metric | Germany (4.4 GB) | Norway (1.3 GB) | Japan (2.3 GB) |
|--------|------------------|------------------|-----------------|
| **Total wall** | 135.3s | 58.8s | 79.4s |
| **PBF %** | 74% | 57% | 61% |
| **Ocean %** | 2% | 28% | 11% |
| **Assemble %** | 24% | 12% | 24% |
| Features | 146M | 28M | 74M |
| Ocean features | 741K | 4.0M | 906K |
| Unique tiles | 226K | 519K | 182K |
| Output | 2.6 GB | 1.0 GB | 1.2 GB |
| RSS | 1.4 GB | 1.7 GB | 2.0 GB |
| Thread alloc | 281 GB | 296 GB | 195 GB |
| **Top bottleneck** | `process_raw_way` | `for_each_zoom_simplified_multi` | `process_raw_way` |
| **#2 bottleneck** | `for_each_zoom_simplified` | `emit_ocean_polygon` | `for_each_zoom_simplified_multi` |
| **clip_polygon_into alloc** | (not in top 10) | **197 GB** | **46 GB** |

### Key findings

1. **Ocean polygon clipping is the biggest alloc hotspot globally.** Norway's fjords push
   `clip_polygon_into` to 197 GB — more than Germany's entire pipeline. Even Japan at 46 GB.
   These are large polygons being clipped across many tiles at every zoom level. The single-tile
   fast path doesn't help here because ocean polygons span many tiles by nature.

2. **`for_each_zoom_simplified_multi` is dramatically more expensive than `_simplified`.**
   Multi-ring polygon simplification (ocean, multipolygon relations) averages 250-450 µs
   vs 1-3 µs for single-way simplification. The multi variant drives 3x more alloc than
   the entire Germany pipeline.

3. **Profile shape is geography-dependent, not size-dependent.** Norway (1.3 GB) stresses
   completely different code paths than Germany (4.4 GB). Coastline complexity matters more
   than feature count for ocean/geometry phases.

4. **Assemble phase scales with urban density, not coastline.** Germany and Japan both hit
   24% assemble time (dense urban tiles). Norway is only 12% (sparse coastal tiles).

5. **No crashes, no OOM across all three.** RSS stays under 2 GB with mimalloc, under 10 GB
   without. SortedNodeStore handles up to 429M nodes on 32 GB RAM.
