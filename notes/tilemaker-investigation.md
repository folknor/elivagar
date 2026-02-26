# Tilemaker Performance Investigation

Goal: understand why elivagar (28s) is not 30%+ faster than Tilemaker (29s) on Denmark.
We should be at ~20s. Something is leaving 8-10 seconds on the table.

## Current Numbers (plantasjen, Denmark 483 MB PBF)

### Elivagar (28s total)
| Phase | Time |
|-------|------|
| PBF + features | 17s |
| Ocean | 3.3s |
| Sort | 0.7s |
| Assemble (MVT + gzip + PMTiles) | 4.6s |

### Tilemaker (29s total)
No phase breakdown — we only measure wall-clock. **Phase 1 fixes this.**

### Output size (Denmark with ocean)
| Tool | Output |
|------|--------|
| elivagar | 380 MB |
| Tilemaker | 308 MB |
| Planetiler | 406 MB |

elivagar without ocean: 317 MB. The 72 MB gap vs Tilemaker needs investigation.

---

## Tilemaker Architecture Summary

Source: `data/tilemaker/` (~15K lines C++, 67 source files).

**Pipeline (no external sort):**
1. PBF read → SortedNodeStore (StreamVByte-compressed, in RAM) + SortedWayStore
2. Shapefile read → ShpMemTiles (in memory)
3. Collect tile coordinates (which tiles exist)
4. Parallel tile output: per-tile geometry build → clip → simplify → MVT encode → compress → PMTiles write

**Key implementation choices:**
- `--fast` flag = materializeGeometries (pre-build way geometries during PBF read)
- Compression: **libdeflate** level 6 (thread-local reusable compressor instances)
- MVT encoding: **vtzero** library (header-only, direct protobuf)
- Simplification: supports both Douglas-Peucker and Visvalingam
- **Shortbread config uses Douglas-Peucker** (same as us)
- Node storage: SortedNodeStore with StreamVByte delta compression (all in RAM)
- Way storage: SortedWayStore with StreamVByte delta compression (stores node IDs, resolves coords at tile time)
- Geometry library: boost::geometry
- Threading: boost::asio thread_pool
- PBF reading: multi-pass per-phase (RelationScan → WayScan → Nodes → Ways → Relations)

**Key source files by area:**

| Area | Files |
|------|-------|
| PBF reading | `pbf_reader.cpp` (590L), `pbf_processor.cpp` (784L) |
| Node storage | `sorted_node_store.cpp` (618L), `sharded_node_store.cpp` |
| Way storage | `sorted_way_store.cpp` (653L), `sharded_way_store.cpp` |
| Tag processing | `osm_lua_processing.cpp` (1222L), `tag_map.cpp` (173L) |
| Tile generation | `tile_worker.cpp` (537L), `tile_data.cpp` (592L) |
| Geometry/clipping | `geom.cpp` (246L), `coordinates_geom.cpp` (189L) |
| Simplification | `visvalingam.cpp` (265L) |
| Compression | `helpers.cpp` (253L) — libdeflate wrappers |
| PMTiles output | `pmtiles.cpp` (173L) |
| Tile sorting | `tile_sorting.cpp` (151L) |
| Main orchestrator | `tilemaker.cpp` (567L) |
| Attribute storage | `attribute_store.cpp` (459L) |

---

## Phase 2 Results: Deep Dive Findings

### Agent A — PBF + Node/Way Storage

**SortedNodeStore architecture:**
- Hierarchical 2-level index: 256K groups → 256 chunks per group → 256 nodes per chunk
- O(1) lookup via popcount on 32-byte bitmasks (group → chunk → node)
- StreamVByte delta encoding: first lat/lon stored explicitly, rest zigzag-delta-encoded
- Only compresses if smaller than uncompressed (adaptive per chunk)
- **~450-500 MB RAM for Denmark's 52.5M nodes** (vs our 96 GB sparse mmap)
- Thread-local storage with orphanage pattern for boundary groups

**PBF reading:**
- Multi-pass sequential phases (RelationScan → WayScan → Nodes → Ways → Relations)
- Per-phase thread pool with block-level parallelism
- Adaptive granularity: large batches for nodes/ways, single-block for relations
- Block subdivision for uneven relation distribution
- Per-thread PBF stream + zlib decompressor (reuses buffers)

**Lua tag processing:**
- ~59M Lua calls for Denmark (52.5M nodes + 6M ways + relations)
- SignificantTags pre-filter gates Lua calls (boolean filter before Lua VM)
- KnownTagKey avoids string allocation for tag lookups
- TagMap: flat vector-based map, O(num_tags) linear search per key

**--fast / materializeGeometries:**
- When true: way/relation geometries stored as coordinates in OsmMemTiles
- When false: stores reference IDs, resolves geometry from WayStore at tile time
- With --fast, tile output doesn't need to re-resolve node coordinates

**Way storage (SortedWayStore):**
- Stores **node IDs, not coordinates** — resolves to coords via NodeStore at tile time
- Hierarchical: 32K groups → 256 chunks → variable ways
- StreamVByte compression for node ID deltas
- ~200-300 MB for Denmark's 6M ways

**Peak RSS for Denmark: ~850 MB - 1.1 GB**

### Agent B — Tile Generation

**CRITICAL: Shortbread uses Douglas-Peucker, not Visvalingam.**
- `simplify_level`: 0.0001-0.0003 (in degrees)
- `simplify_ratio`: 2.0 (scales per zoom level)
- `simplify_below`: mostly 14
- Visvalingam available but NOT used by Shortbread config

**Clipping: Standard Sutherland-Hodgman** (same as us)
- `fast_clip()` in geom.cpp: 4-pass bit-code based, O(n)
- Falls back to boost::geometry intersection if self-intersections detected

**MVT encoding: vtzero**
- Header-only, direct protobuf building (no intermediate representation)
- Stream-based: features added to layer as processed
- Attribute deduplication via AttributeStore indices

**Tile data model:**
- Z6 clustering: 4,096 z6 tiles, each with sorted OutputObject entries
- RTree for large objects spanning many tiles (boost::geometry rtree, quadratic<128>)
- Binary search within z6 bucket for tile lookup

**Feature combining:**
- Compatible consecutive points → multipoint
- Compatible consecutive linestrings → merged + reordered
- Compatible consecutive polygons → union_many (pairwise tree reduction)

**Weight-based tile batching:**
- z13-z14: weight 1 (cheap), z12: 10, z11: 100, z0-z10: 1000 (expensive)
- Batch target: 1000 weight units
- Ensures low-zoom tiles don't starve thread pool

### Agent C — Compression + Output

**Compression comparison:**
- Tilemaker: libdeflate level 6, thread-local reusable `Compressor` instance
- Elivagar: flate2 (zlib-ng) level 6, new `GzEncoder` per tile
- Performance gap: libdeflate is ~10-20% faster than zlib-ng (not 2-3x as initially hypothesized)
- Estimated savings from switching: **~0.3-0.5s**

**Compression threading:**
- Tilemaker: compression on worker threads (parallel), thread-local compressor reused
- Elivagar: compression in rayon `par_iter` with `map_init`, new encoder per tile
- Both are parallel; difference is per-tile state allocation overhead

**PMTiles writer comparison:**
- Tilemaker: dedup on raw MVT before compression (tiny cache for tiles <100 bytes)
- Elivagar: dedup on compressed gzip data (SipHash + length guard), cap at 1M entries
- Tilemaker: hierarchical tile clustering sort (NOT Hilbert)
- Elivagar: Hilbert curve ordering

**Output size gap (380 MB vs 308 MB):**
- 72 MB / 23% larger output — too large to be compression ratio difference alone
- Likely causes: different feature encoding, ocean polygon handling, simplification parameters
- elivagar without ocean: 317 MB, still 9 MB larger than Tilemaker with ocean

**libdeflate in Rust:**
- `libdeflater` crate exists (v1.25, Apache-2.0, active)
- Non-streaming API (buffer-in, buffer-out) — actually simpler than flate2
- Drop-in replacement possible in pipeline.rs and pmtiles_writer.rs

---

## Revised Hypotheses (post-Phase 2, ranked by impact)

### BUSTED hypotheses:
- ~~Simplification algorithm gap~~: Both use Douglas-Peucker for Shortbread
- ~~libdeflate 2-3x faster~~: Actual gap is ~10-20%

### CONFIRMED hypotheses:
1. **Compression library gap (small)**: libdeflate ~10-20% faster. Saves ~0.3-0.5s. Easy win.
2. **Lua overhead is a headwind for Tilemaker**: 59M Lua calls. They compensate elsewhere.

### NEW hypotheses:
3. **Output size gap (72 MB)**: We produce 23% more output. This means more compression work,
   more I/O, AND potentially more MVT encoding work. Root cause unknown — could be different
   feature counts, different ocean handling, different simplification thresholds, or MVT encoding
   differences. **This is the biggest unknown.**
4. **Visvalingam as novel advantage**: Neither tool uses it for Shortbread. Our hotpath shows
   simplification is #1 CPU consumer (64.6s CPU time). Switching to O(n log n) Visvalingam
   could save 2-5s — a win over BOTH tools.
5. **Encoder reuse**: Tilemaker reuses thread-local compressor state. We allocate new GzEncoder
   per tile (56K tiles for Denmark). Minor overhead but easy to fix.

---

## Revised Action Plan

### Phase 1: Instrument Tilemaker (still needed)

Add `clock_gettime(CLOCK_MONOTONIC)` timing to `tilemaker.cpp` around:
1. Shapefile/GeoJSON reading (lines 264-284)
2. PBF reading (lines 296-326)
3. Tile coordinate collection + sorting (lines 396-461)
4. Tile output loop (lines 463-542)
5. PMTiles finalization (lines 549-555)

Emit key=value pairs on stderr. Rebuild and run Denmark.

**Deliverable:** Phase timings for Tilemaker, directly comparable to our 17s/3.3s/0.7s/4.6s.

### Phase 3: Output Size Investigation (NEW — high priority)

Why is our output 72 MB (23%) larger than Tilemaker's?

**Method:**
1. Run `scripts/compare-tiles.sh` (already exists!) to diff feature counts per layer per zoom
2. Compare simplification parameters between our Shortbread config and Tilemaker's
3. Compare ocean tile generation (count, size)
4. Sample specific tiles and compare MVT structure (feature count, vertex count, attribute encoding)

**Deliverable:** Root cause of the 72 MB gap. Could reveal wasted work that also costs CPU time.

### Phase 4: Implement Optimizations (revised priorities)

| # | Optimization | Est. savings | Difficulty | Dependencies |
|---|-------------|-------------|------------|--------------|
| 1 | **Output size investigation** | Unknown (removes wasted work) | Research | None |
| 2 | **Visvalingam simplification** | 2-5s (our #1 CPU consumer) | Medium | None |
| 3 | **libdeflate + encoder reuse** | 0.3-0.5s | Easy | None |
| 4 | **Node storage redesign** | 0s Denmark / huge at planet | Hard | None |

### Phase 5: Micro-Benchmarks (if needed)

1. **Compression shootout**: flate2 (zlib-ng) vs libdeflater — benchmark with real MVT payloads
2. **Simplification shootout**: DP vs Visvalingam on real geometries
3. **MVT encoding**: Compare against vtzero-equivalent if encoding shows up in profiling

---

## Success Criteria

- Phase breakdown for Tilemaker (within 10% accuracy)
- Root cause of 72 MB output size gap identified
- Identified ≥2 concrete optimizations worth ≥1s each
- Elivagar Denmark time reduced from 28s to ≤22s (20%+ faster than Tilemaker)
