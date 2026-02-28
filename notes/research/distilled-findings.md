# Distilled Findings from Deep Code Investigations

Date: 2026-02-28
Sources: Box 1 (pipeline), Box 2 (node/way storage), Box 3+6 (wire format/sort), Box 4 (geometry), Box 5 (ocean), Box 7 (assembly/MVT/compression), Box 8 (PMTiles writer)

**41 findings total: 3 critical, 10 high, 18 medium, 10 low**

---

## Memory

### F1: SortedNodeStore exceeds 64 GB at planet scale
- **Source**: Box 2, `node_index.rs:581-602` (see [box2-node-way-storage.md § 1.6 Memory Profile]); Box 1, `pipeline.rs:342` (see [box1-pipeline-orchestration.md § Phase 1+2: PBF Read + Feature Processing])
- **Severity**: critical
- **Category**: memory
- **Planet impact**: Blob data 44-52 GB; peak during way phase ~61 GB (node store + way index writes)
- **Denmark impact**: ~420 MB total, no concern
- **Effort**: high (architectural -- needs spilling or streaming)
- **Cross-box deps**: F2 (way index coexistence), F8 (sort buffer adds to peak)
- **Detail**: The SortedNodeStore blob grows to 44-52 GB for planet (8.5B nodes, ~64% FOR compression ratio). During the way phase, this coexists with way index writes (~10 GB populated), leaving only ~3 GB for sort buffers, rayon stacks, and OS. The 64 GB RAM target is dangerously tight.
- **Status**: already-in-TODO (Planet scale Step 3 validates compression ratio; peak memory concern is new detail)

### F2: Way index data file is ~45 GB at planet scale
- **Source**: Box 2, `way_index.rs:74-110` (see [box2-node-way-storage.md § 3.4 Memory Profile])
- **Severity**: high
- **Category**: memory
- **Planet impact**: ~45 GB data file + ~18 GB sparse offsets file; coexists with node store during way phase
- **Denmark impact**: ~384 MB data, no concern
- **Effort**: high (would require compression or streaming redesign)
- **Cross-box deps**: F1 (node store coexistence)
- **Detail**: Way data file stores ~5.6B coords at 8 bytes each = 44.8 GB. The mmap opens only after `finish_writing()` for relation processing, after the node store is dropped. Peak combined memory with node store is during way phase (writes, not mmap), so it is the BufWriter sequential writes that coexist, not the full mmap.
- **Status**: new

### F3: Per-batch `map_init` state recreation in assembly
- **Source**: Box 7, `pipeline.rs:1458` (see [box7-assembly-mvt-compression.md § 4.1 Compressor Initialization and Reuse])
- **Severity**: high
- **Category**: allocator-churn
- **Planet impact**: ~288 GB allocator churn (24K batches * 24 workers * ~500 KB compressor state); loss of warm Vec pools between batches
- **Denmark impact**: ~13 batches, negligible
- **Effort**: medium (refactor to persistent per-worker state struct)
- **Cross-box deps**: F18 (LayerBuilder per-tile creation), F19 (encode buffer undersize)
- **Detail**: `encode_tile_batch` uses `par_iter().map_init()` which creates compressor, pools, scratch buffers fresh for each batch call. State is dropped between batches, losing warm Vec pool capacity. At planet scale with 24K batches, this is significant allocator churn.
- **Status**: new

### F4: `collect_dir_entries` double-buffers directory data
- **Source**: Box 8, `pmtiles_writer.rs:348-350` (see [box8-pmtiles-writer.md § 6.11 `collect_dir_entries` reads entire file at once])
- **Severity**: medium
- **Category**: memory
- **Planet impact**: ~240 MB transient spike (raw bytes + parsed entries simultaneously)
- **Denmark impact**: <2 MB
- **Effort**: low (read entries directly in 24-byte chunks instead of `read_to_end`)
- **Cross-box deps**: F5 (entries Vec lifetime)
- **Detail**: In streaming mode, `read_to_end` loads the entire directory temp file into a `Vec<u8>`, then parses into a second `Vec<DirEntry>`. Both exist simultaneously before the raw buffer is dropped.
- **Status**: new

### F5: `entries` Vec lifetime extends through entire finalization write phase
- **Source**: Box 8, `pmtiles_writer.rs:233` (see [box8-pmtiles-writer.md § 4 Finalization])
- **Severity**: medium
- **Category**: memory
- **Planet impact**: ~120 MB held unnecessarily during output write
- **Denmark impact**: <1 MB
- **Effort**: trivial (drop after `build_directories` returns)
- **Cross-box deps**: F4 (double-buffer)
- **Detail**: The `entries: Vec<DirEntry>` is only needed through `build_directories` (line 236) but lives until `write_to` returns (line 296), holding ~120 MB of dead data during the multi-GB blob copy phase.
- **Status**: new

### F6: Dedup HashMap not cleared during finalization
- **Source**: Box 8, `pmtiles_writer.rs:125` (see [box8-pmtiles-writer.md § 2 Tile Deduplication])
- **Severity**: low
- **Category**: memory
- **Planet impact**: ~50 MB held unnecessarily
- **Denmark impact**: ~2 MB
- **Effort**: trivial (clear at start of `write_to`)
- **Cross-box deps**: none
- **Detail**: The dedup HashMap (~50 MB at 1M cap) is no longer needed after all tiles are added but stays alive through finalization. Could be cleared to free memory before the write phase.
- **Status**: new

### F7: All ocean polygons held in memory before parallel processing
- **Source**: Box 5, `ocean.rs:89-193` (see [box5-ocean.md § 1 Shapefile Reader])
- **Severity**: medium
- **Category**: memory
- **Planet impact**: ~1-5 GB (all ~250K in-bounds polygons stored simultaneously)
- **Denmark impact**: ~10-50 MB
- **Effort**: high (requires streaming parse+split+emit architecture)
- **Cross-box deps**: none
- **Detail**: All in-bounds polygons are parsed and stored in `polygons: Vec<OceanPolygon>` before any parallel processing begins. At planet scale, all ~250K shapes pass the bbox filter and must be held simultaneously.
- **Status**: new

### F8: Sort buffer `buffer_bytes` undercounts actual memory
- **Source**: Box 3+6, `sort.rs:136-143` (see [box3-6-wire-format-sort.md § 5.3 Memory Profile During Sort])
- **Severity**: medium
- **Category**: memory
- **Planet impact**: Chunk memory is 24-38% higher than the 1 GB target (~1.24-1.38 GB actual)
- **Denmark impact**: Minimal (only ~2 chunks)
- **Effort**: trivial (count `data.len() + 32` instead of `data.len() + 8`)
- **Cross-box deps**: F1 (peak memory during way phase)
- **Detail**: `buffer_bytes` counts `data.len() + 8` per record (payload + key) but ignores the 24-byte `Vec<u8>` overhead and 32-byte `SortRecord` struct size. The flush trigger fires later than intended, allowing actual memory to exceed the target.
- **Status**: confirmed (Box 6 Finding 2 from theoretical review)

---

## CPU

### F9: Compression is top CPU sink at planet scale
- **Source**: Box 7, `pipeline.rs:1460,1514-1519` (see [box7-assembly-mvt-compression.md § 4.2 Compression Mechanics])
- **Severity**: high
- **Category**: cpu
- **Planet impact**: ~15-30 min compression CPU at level 6; z13-z14 tiles are 86.5% of tile count
- **Denmark impact**: ~1-1.5s (40-60% of 2.5s assemble)
- **Effort**: low (per-zoom level strategy ~20 lines)
- **Cross-box deps**: none
- **Detail**: libdeflate level 6 applied uniformly to all tiles. At planet scale with ~100M tiles, z13-z14 dominate. Dropping level to 3 for z13-z14 saves ~30-40% compression CPU (~150-400s) at ~5% size penalty. Per-zoom strategy already designed in TODO.
- **Status**: already-in-TODO (detailed strategy documented)

### F10: Per-tile clipping dominates CPU for large polygons (no interior tile detection)
- **Source**: Box 4, `pipeline.rs:1204` (see [box4-geometry-topology.md § 4.3 Worst-Case Fanout]); `geometry.rs:691-728` (see [box4-geometry-topology.md § 2.2 Sutherland-Hodgman Polygon Clipping])
- **Severity**: high
- **Category**: cpu
- **Planet impact**: Country boundaries at z14 produce ~300K tiles; interior tiles run full S-H clip unnecessarily
- **Denmark impact**: `for_each_zoom_simplified_multi`: 257K calls, 23.4s total (105% of wall time)
- **Effort**: medium (add 4-corner PIP test before clipping)
- **Cross-box deps**: F11 (inner ring bbox prefilter), F14 (row pre-clip)
- **Detail**: Tiles fully inside a polygon still run the complete 4-pass Sutherland-Hodgman clip, producing a near-copy of the tile extent. A "tile inside polygon" test (4 PIP checks) would allow emitting a full-tile rectangle without clipping. For Germany at z14, ~298K interior tiles could skip clipping.
- **Status**: already-in-TODO (item 2 in Box 4 investigation)

### F11: No inner ring bbox prefilter in multipolygon emission
- **Source**: Box 4, `pipeline.rs:1269-1281` (see [box4-geometry-topology.md § 5.3 Inner-to-Outer Pairing])
- **Severity**: high
- **Category**: cpu
- **Planet impact**: Every inner ring runs O(n) outcode scan against every tile; precomputing inner bboxes enables O(1) rejection
- **Denmark impact**: Part of the 23.4s multipolygon cost
- **Effort**: low (precompute inner bboxes per zoom, add bbox-vs-bbox test)
- **Cross-box deps**: F10 (interior tile detection)
- **Detail**: Before clipping each inner ring against a tile, no bbox intersection check is performed. Currently every inner ring runs the full outcode scan O(n) against every tile. A fast O(1) bbox-vs-bbox test would skip most inner rings for most tiles.
- **Status**: already-in-TODO (item 3 in Box 4 investigation)

### F12: Multipolygon `pair_rings` is O(I * O * V)
- **Source**: Box 4, `multipolygon.rs:140-174` (see [box4-geometry-topology.md § 5.3 Inner-to-Outer Pairing])
- **Severity**: medium
- **Category**: cpu
- **Planet impact**: Pathological with many outers (>10) and many inners; e.g., 200 inners * 100 outers * V PIP tests
- **Denmark impact**: Most relations have 1-3 outers, negligible
- **Effort**: medium (spatial index for inner-to-outer pairing)
- **Cross-box deps**: none
- **Detail**: For each inner ring, PIP test is run sequentially against each outer ring until a match is found. The PIP test itself is O(V) ray-casting. Pathological for complex country/forest relations with many outers and inners.
- **Status**: confirmed (Box 4 Finding 2 from theoretical review)

### F13: `join_ways` pass 2 has O(C^2) worst case
- **Source**: Box 4, `multipolygon.rs:222-306` (see [box4-geometry-topology.md § 5.1 Ring Assembly])
- **Severity**: medium
- **Category**: cpu
- **Planet impact**: Pathological for relations with many unclosed chains after greedy pass 1; C=1000 gives 1M iterations
- **Denmark impact**: Most relations close in pass 1, negligible
- **Effort**: medium (rewrite pass 2 with better chain merging strategy)
- **Cross-box deps**: none
- **Detail**: Each iteration of pass 2 scans all chains and breaks on first merge, then restarts. For a relation with 1000 unclosed chains after pass 1, this is O(C^2). Typical OSM data has well-ordered ways making pass 1 effective.
- **Status**: new

### F14: No row pre-clipping for PBF polygon features
- **Source**: Box 4, `pipeline.rs:1156,1221` (see [box4-geometry-topology.md § 4.3 Worst-Case Fanout]); contrast with `ocean.rs:460-486` (see [box5-ocean.md § 4 Scanline Fill Algorithm])
- **Severity**: medium
- **Category**: cpu
- **Planet impact**: Large PBF polygons (forests, country boundaries) clip full simplified geometry per tile; row pre-clip gives ~rows_covered speedup
- **Denmark impact**: Moderate (large forest/water polygons)
- **Effort**: medium (port ocean.rs row pre-clip pattern to emit_polygon_feature and emit_multipolygon_feature)
- **Cross-box deps**: F10 (interior tile detection)
- **Detail**: The ocean subsystem clips polygons to Y-band before per-tile clipping (~50 vertices instead of ~1000 for large polygons). The PBF emission paths (`emit_polygon_feature`, `emit_multipolygon_feature`) do not have this optimization, clipping the full simplified polygon against every tile.
- **Status**: already-in-TODO (item 4 in Box 4 investigation)

### F15: Douglas-Peucker recursion is O(n^2) worst case
- **Source**: Box 4, `geometry.rs:277-292` (see [box4-geometry-topology.md § 3.1 Implementation])
- **Severity**: low
- **Category**: cpu
- **Planet impact**: Pathological zigzag geometries only; typical ways have <100 vertices with O(n log n) average
- **Denmark impact**: None observed
- **Effort**: low (iterative DP with explicit stack)
- **Cross-box deps**: none
- **Detail**: The recursive DP implementation has O(n) worst-case stack depth and O(n^2) worst-case time for pathological geometries. For very long ways (10K+ vertices like coastlines), this could approach stack limits on rayon worker threads. Typical case is ~7 levels of recursion.
- **Status**: new

### F16: `find_chunk_in_blob` linear scan at planet scale
- **Source**: Box 2, `node_index.rs:409-429` (see [box2-node-way-storage.md § 6.2 `find_chunk_in_blob` Linear Scan Cost at Planet Scale])
- **Severity**: medium
- **Category**: cpu
- **Planet impact**: ~254 chunks/group average; each miss scans ~8.6 KB of blob data. With 24% miss rate and ~48M+ lookups, ~46 GB of blob data scanned
- **Denmark impact**: ~7 chunks/group average, cheap
- **Effort**: medium (pre-compute chunk offset table per group)
- **Cross-box deps**: none
- **Detail**: The blob scan at `find_chunk_in_blob` linearly iterates through chunks to find the target. Comment says this was tried with offsets and reverted due to high hit rate. At planet scale with denser groups (~254 chunks vs ~7), the cost/benefit changes.
- **Status**: new

### F17: Redundant tag lookups across matchers
- **Source**: Box 3+6, `shortbread/mod.rs:248-275` (see [box3-6-wire-format-sort.md § 4.5 Redundant Tag Lookups])
- **Severity**: low
- **Category**: cpu
- **Planet impact**: ~30-50% more string comparisons than necessary; confirmed non-bottleneck by hotpath profiling (<5% PBF phase)
- **Denmark impact**: Confirmed negligible
- **Effort**: low (pre-scan common keys into local struct)
- **Cross-box deps**: none
- **Detail**: `tags.get("highway")` is checked in 5 matchers, `tags.get("waterway")` in 5 matchers, etc. Linear scan over the element's 3-15 tags is repeated. Binary search was tried and rejected (+55% PBF phase).
- **Status**: confirmed (Box 3 Finding 3 from theoretical review, verified low impact)

---

## I/O

### F18: Wire format repeats attribute key strings in every sort record
- **Source**: Box 3+6, `wire_format.rs:29,72` (see [box3-6-wire-format-sort.md § 6.3 Attribute Key Strings: Interning Pre-Sort])
- **Severity**: high
- **Category**: io
- **Planet impact**: ~44 GB redundant key strings; combined key+kind interning saves ~63 GB
- **Denmark impact**: ~280 MB key strings, ~420 MB combined key+kind
- **Effort**: low (u8 key_id lookup table, ~20 lines encode + decode)
- **Cross-box deps**: F20 (sort payload width), F22 (geometry varint)
- **Detail**: ~40 unique key strings repeated verbatim across 2.4B records. Interning happens only at assembly decode time. Replacing `[1B key_len][N bytes key_str]` with `[1B key_id]` saves ~6 bytes per key, ~20 bytes per record average.
- **Status**: already-in-TODO (detailed implementation plan in TODO.md)

### F19: Sort chunks are uncompressed
- **Source**: Box 3+6, `sort.rs:201-222` (see [box3-6-wire-format-sort.md § 5.1 Full Sort Lifecycle])
- **Severity**: medium (informational)
- **Category**: io
- **Planet impact**: Key interning savings apply in full to sort I/O (not reduced by gzip); total sort I/O ~538 GB
- **Denmark impact**: ~3.6 GB total sort I/O
- **Effort**: N/A (context finding, not actionable on its own)
- **Cross-box deps**: F18 (key interning), F22 (geometry varint)
- **Detail**: Sort chunks are written as raw bytes with no compression. The theoretical review asked whether gzip would reduce interning gains; the answer is no, because compression only happens at MVT tile level, never at sort chunk level.
- **Status**: new

### F20: osm_id consumes 8 bytes per sort record, optional for output
- **Source**: Box 3+6, `wire_format.rs:79,120` (see [box3-6-wire-format-sort.md § 6.1 osm_id])
- **Severity**: medium
- **Category**: io
- **Planet impact**: ~19.2 GB (2.4B records * 8 bytes)
- **Denmark impact**: ~128 MB
- **Effort**: low (make optional via pipeline flag, ~15 lines)
- **Cross-box deps**: F18 (sort payload width)
- **Detail**: MVT spec says feature IDs are optional. The osm_id is used for `merge_same_attr_geometries` (which clears it on merge anyway) and MVT output. Removing it saves 8 bytes per record. Semantic consideration: some map renderers use feature IDs.
- **Status**: new

### F21: cmd_count stored as u32, max ~2000 commands
- **Source**: Box 3+6, `wire_format.rs:81` (see [box3-6-wire-format-sort.md § 2.2 Wire Format Binary Layout])
- **Severity**: low
- **Category**: io
- **Planet impact**: ~4.8 GB savings (2.4B records * 2 bytes)
- **Denmark impact**: ~32 MB
- **Effort**: trivial (change to u16, ~5 lines)
- **Cross-box deps**: F18 (sort payload width)
- **Detail**: The `cmd_count` field is u32 (4 bytes) but the maximum number of MVT geometry commands per tile feature is well under 65K. A u16 saves 2 bytes per record with zero risk.
- **Status**: new

### F22: Geometry commands stored as fixed u32, varint would save ~45%
- **Source**: Box 3+6, `wire_format.rs:84-86` (see [box3-6-wire-format-sort.md § 6.2 Geometry Commands: Fixed u32 vs Varint])
- **Severity**: medium
- **Category**: io
- **Planet impact**: ~115 GB savings on geometry portion; loses fast memcpy decode path
- **Denmark impact**: Proportional
- **Effort**: medium (varint encode/decode, benchmark needed for decode cost)
- **Cross-box deps**: F18 (sort payload width), F19 (uncompressed chunks)
- **Detail**: MVT commands are stored as fixed 4-byte u32 LE, enabling an unsafe `copy_nonoverlapping` memcpy decode. Varint encoding averages ~2.2 bytes per command (~45% savings). The trade-off is per-element varint parsing vs bulk memcpy in `add_feature_to_layer`.
- **Status**: new

### F23: Output BufWriter uses default 8 KB buffer for multi-GB write
- **Source**: Box 8, `pmtiles_writer.rs:274` (see [box8-pmtiles-writer.md § 4 Finalization])
- **Severity**: medium
- **Category**: io
- **Planet impact**: Frequent small writes for ~3 GB blob copy; asymmetric with 1 MB BufReader
- **Denmark impact**: Minimal (286 MB)
- **Effort**: trivial (change to `BufWriter::with_capacity(1 << 20, file)`)
- **Cross-box deps**: none
- **Detail**: `write_to` opens the output file with `BufWriter::new(file)` (8 KB default). The blob copy reads in 1 MB chunks via BufReader but writes in 8 KB pieces. For planet-scale ~3 GB sequential writes, a 1 MB buffer reduces syscall count significantly.
- **Status**: new

### F24: Directory temp file BufWriter uses 8 KB default
- **Source**: Box 8, `pmtiles_writer.rs:167` (see [box8-pmtiles-writer.md § 3 Streaming Architecture])
- **Severity**: low
- **Category**: io
- **Planet impact**: More syscalls than necessary for ~5M directory entry writes
- **Denmark impact**: Negligible
- **Effort**: trivial (change to `BufWriter::with_capacity(1 << 16, dir_file)`)
- **Cross-box deps**: none
- **Detail**: Directory entry writes are 24 bytes each through an 8 KB BufWriter (~341 entries per flush). At planet scale with millions of entries (after run-length encoding), a 64 KB buffer would reduce syscalls.
- **Status**: new

### F25: Finalization re-reads entire blob temp file
- **Source**: Box 8, `pmtiles_writer.rs:285-290` (see [box8-pmtiles-writer.md § 4 Finalization])
- **Severity**: medium
- **Category**: io
- **Planet impact**: ~6 GB sequential I/O (3 GB read + 3 GB write) during finalization
- **Denmark impact**: ~572 MB
- **Effort**: high (would require pre-allocating header space and writing tiles directly to output)
- **Cross-box deps**: none
- **Detail**: PMTiles format requires header (with data offset) before tile data, but data offset depends on directory sizes unknown until all tiles are added. The blob must be written to a temp file first, then copied to the final output after directories are computed. Unavoidable in current architecture.
- **Status**: confirmed (Box 8 Finding 2 from theoretical review)

---

## Allocator Churn

### F26: `for_each_zoom_simplified` allocates cascade Vec per call
- **Source**: Box 4, `geometry.rs:363` (see [box4-geometry-topology.md § 3.4 Cascading Simplification])
- **Severity**: high
- **Category**: allocator-churn
- **Planet impact**: ~2 GB cascade copies for Denmark; proportionally larger for planet
- **Denmark impact**: ~2 GB of 5.8 GB total `for_each_zoom_simplified` allocations
- **Effort**: low (add `SimplifySingleScratch` struct or take ownership of input)
- **Cross-box deps**: F27 (clip_linestring allocations)
- **Detail**: `cascade = merc.to_vec()` copies the entire geometry on every call. The multi-polygon variant has `SimplifyMultiScratch` for buffer hoisting, but the single-geometry variant does not. Called once per feature per match (millions of times).
- **Status**: already-in-TODO (item related to Vec creation in Box 4 investigation)

### F27: `clip_linestring` allocates new Vec per output segment per tile
- **Source**: Box 4, `geometry.rs:559,575` (see [box4-geometry-topology.md § 2.1 Cohen-Sutherland Line Clipping])
- **Severity**: high
- **Category**: allocator-churn
- **Planet impact**: Millions of Vec<Point> allocations (one per tile per zoom per line feature)
- **Denmark impact**: ~1.5-2 GB of the 5.8 GB `for_each_zoom_simplified` allocations
- **Effort**: low (add `clip_linestring_into` buffer-reuse variant)
- **Cross-box deps**: F26 (cascade copy)
- **Detail**: Unlike `clip_polygon_into`, there is no buffer-reuse variant for line clipping. Each call creates a `SmallVec` container and new `Vec<Point>` for each output segment. Line features are ~50% of all features. For a line crossing 5 tiles at 15 zoom levels: 75 Vec allocations per feature.
- **Status**: already-in-TODO (item 5 in Box 4 investigation)

### F28: `to_tile_coords` allocating variant used in multipolygon emission
- **Source**: Box 4, `pipeline.rs:1235,1247,1261,1274` (see [box4-geometry-topology.md § 6.4 Specific Findings])
- **Severity**: medium
- **Category**: allocator-churn
- **Planet impact**: 4+ Vec allocations per tile per zoom in the hottest emission path (257K features)
- **Denmark impact**: Part of the multipolygon allocation overhead
- **Effort**: low (switch to `to_tile_coords_into` with managed buffers)
- **Cross-box deps**: F10 (multipolygon clipping cost)
- **Detail**: `emit_line_feature` and `emit_polygon_feature` correctly use `to_tile_coords_into`. But `emit_multipolygon_feature` uses the allocating `to_tile_coords()` for all rings (outer and each inner). Each call allocates a new `Vec<(i32, i32)>`.
- **Status**: already-in-TODO (item 1 in Box 4 investigation)

### F29: `ring_refs` Vec allocated per tile per zoom in multipolygon
- **Source**: Box 4, `pipeline.rs:1285` (see [box4-geometry-topology.md § 6.4 Specific Findings]); also `ocean.rs:585` (see [box5-ocean.md § 4 Scanline Fill Algorithm])
- **Severity**: low
- **Category**: allocator-churn
- **Planet impact**: Small Vec (1-10 elements) per tile, millions of tiles
- **Denmark impact**: Minimal individual cost, high call count
- **Effort**: trivial (replace with `SmallVec<[&[(i32,i32)]; 4]>`)
- **Cross-box deps**: none
- **Detail**: `let ring_refs: Vec<&[(i32, i32)]> = ...` allocates a heap Vec per tile when the common case has 1-4 rings. A SmallVec with inline capacity 4 would avoid the heap allocation.
- **Status**: already-in-TODO (item 6 in Box 4 investigation)

### F30: Per-record Vec allocation in `encode_feature_data_with_attrs`
- **Source**: Box 3+6, `wire_format.rs:72-89` (see [box3-6-wire-format-sort.md § 7.1 Per-Record Vec Allocation in Encode Path])
- **Severity**: medium
- **Category**: allocator-churn
- **Planet impact**: 2.4B heap allocations of varying sizes; mimalloc handles efficiently but contributes to 9.9 GB `write_sorted_chunk` alloc
- **Denmark impact**: 16M allocations
- **Effort**: high (arena allocation for sort records within a chunk)
- **Cross-box deps**: F8 (sort buffer memory accounting)
- **Detail**: Every sort record's `data` is a separately allocated `Vec<u8>`. The Vec needs to be owned by SortRecord, so a reusable buffer would still require `.to_vec()`. Arena allocation would eliminate per-record allocation entirely but requires significant refactoring.
- **Status**: new

### F31: `current_coords` Vec replaced instead of cleared at group/chunk transitions
- **Source**: Box 2, `node_index.rs:666,672` (see [box2-node-way-storage.md § 6.9 `Vec::with_capacity(NODES_PER_CHUNK)` Leak in Group/Chunk Transitions])
- **Severity**: low
- **Category**: allocator-churn
- **Planet impact**: ~25M unnecessary alloc/dealloc cycles (one per chunk transition)
- **Denmark impact**: ~205K alloc/dealloc cycles
- **Effort**: trivial (use `.clear()` instead of `Vec::with_capacity()`)
- **Cross-box deps**: none
- **Detail**: At group/chunk transitions, `self.current_coords = Vec::with_capacity(NODES_PER_CHUNK)` drops the old Vec and allocates a new one. Could use `.clear()` to retain capacity. The scratch Vecs for lats/lons are already hoisted and reused.
- **Status**: new

### F32: LayerBuilder created/dropped per tile instead of pooled per worker
- **Source**: Box 7, `pipeline.rs:1467,1530` (see [box7-assembly-mvt-compression.md § 8.3 LayerBuilder Creation Per Tile])
- **Severity**: medium
- **Category**: allocator-churn
- **Planet impact**: ~500M LayerBuilder alloc/dealloc cycles (100M tiles * ~5 layers avg)
- **Denmark impact**: 270K instances, ~54 MB churn
- **Effort**: medium (pool LayerBuilders per rayon worker, clear instead of drop)
- **Cross-box deps**: F3 (per-batch state recreation)
- **Detail**: Each tile creates LayerBuilder instances with String name, empty Vecs, empty FxHashMaps. These could be `.clear()`ed and reused per worker thread instead of allocated and dropped for each tile.
- **Status**: new

### F33: `encode_tile_with` underestimates initial buffer capacity
- **Source**: Box 7, `mvt.rs:277` (see [box7-assembly-mvt-compression.md § 8.2 `Vec::with_capacity(4096)` in `encode_tile_with` Underestimates])
- **Severity**: medium
- **Category**: allocator-churn
- **Planet impact**: 2-4 reallocations per tile * 100M tiles (4 KB initial vs ~54 KB average)
- **Denmark impact**: 2-4 reallocs per tile * 54K tiles
- **Effort**: trivial (increase to 65536 or reuse from per-worker state)
- **Cross-box deps**: F3 (per-batch state)
- **Detail**: `Vec::with_capacity(4096)` allocates 4 KB initial buffer for MVT output, but average encoded tile is ~54 KB (Germany data). Almost every tile triggers 2-4 reallocations (4096 -> 8192 -> 16384 -> 32768 -> 65536).
- **Status**: new

### F34: Ocean fill_data.clone() per fill tile
- **Source**: Box 5, `ocean.rs:525,538` (see [box5-ocean.md § 6 Clone Analysis])
- **Severity**: medium
- **Category**: allocator-churn
- **Planet impact**: ~10M-100M clones of ~50 bytes each; each allocates a new Vec<u8>
- **Denmark impact**: ~10K-100K clones
- **Effort**: medium (use Arc or special sentinel in SortRecord)
- **Cross-box deps**: none
- **Detail**: All fill tiles within a single `emit_ocean_polygon` call have identical ~50-byte data. Each `fill_data.clone()` allocates a new heap Vec. The clone becomes `SortRecord.data` that is flushed to disk, so the allocation is short-lived but high-volume at planet scale.
- **Status**: confirmed (Box 5 clone analysis, expanded from theoretical review)

### F35: Way reversal allocates new Vec in multipolygon assembly
- **Source**: Box 4, `multipolygon.rs:251,282,341-342,383,389` (see [box4-geometry-topology.md § 5.5 Allocation Patterns in Multipolygon Assembly])
- **Severity**: low
- **Category**: allocator-churn
- **Planet impact**: O(R) temporary Vec allocations per relation with reversed ways
- **Denmark impact**: Negligible
- **Effort**: trivial (reverse in-place)
- **Cross-box deps**: none
- **Detail**: Way reversal in `join_ways` and `attach_way` uses `into_iter().rev().collect()` or `iter().copied().rev().collect()`, allocating a new Vec each time. Could be done in-place with `.reverse()`.
- **Status**: new

---

## Correctness

### F36: Flat fallback has no size guard or warning at planet scale
- **Source**: Box 2, `pipeline.rs:344-345` (see [box2-node-way-storage.md § 4.1 Node Store Selection]); `node_index.rs:67-87` (see [box2-node-way-storage.md § 2 Flat Node Index])
- **Severity**: critical
- **Category**: correctness
- **Planet impact**: Silently creates 96 GB sparse file, causes 14x measured regression even on Denmark (242s vs 17.2s)
- **Denmark impact**: Message says "PBF not sorted" but no size warning
- **Effort**: low (add PBF size guard + abort message, ~15 lines)
- **Cross-box deps**: none
- **Detail**: When PBF header lacks `Sort.Type_then_ID`, the flat mmap path is selected with no warning about expected file size or memory impact. No upper bound on file growth. Previous madvise attempts caused 3.5x slowdown due to sparse file behavior.
- **Status**: already-in-TODO (detailed P0/P1/P2 fixes documented)

### F37: Way index sentinel edge case
- **Source**: Box 2, `way_index.rs:149-152,233-243` (see [box2-node-way-storage.md § 3.5 Sentinel Bug])
- **Severity**: low
- **Category**: correctness
- **Planet impact**: None (guarded in practice by `pipeline.rs:717`)
- **Denmark impact**: None
- **Effort**: trivial (use non-zero sentinel or separate flag)
- **Cross-box deps**: none
- **Detail**: An empty way (0 coordinates) that is the first insertion gets `(data_offset=0, coord_count=0)`, indistinguishable from "unset". The pipeline guards against this by filtering empty ways, but it is a latent correctness risk if a future code path bypasses the check.
- **Status**: new

---

## Safety

### F38: `into_reader()` diagnostic scan reads entire blob at planet scale
- **Source**: Box 2, `node_index.rs:749-797` (see [box2-node-way-storage.md § 6.3 `into_reader()` Creates a Memory Spike])
- **Severity**: low
- **Category**: cpu
- **Planet impact**: Sequential scan of ~51 GB blob data during node-to-way transition; adds seconds
- **Denmark impact**: ~270 MB scan, negligible
- **Effort**: trivial (skip diagnostics in release or make optional)
- **Cross-box deps**: none
- **Detail**: When `SortedNodeStore::into_reader()` is called, it runs diagnostics scanning every group's blob data to compute statistics. This is a one-time cost but at planet scale reads 51 GB sequentially.
- **Status**: new

### F39: `gzip_compress` creates new Compressor per leaf directory chunk
- **Source**: Box 8, `pmtiles_writer.rs:549-558` (see [box8-pmtiles-writer.md § 6.6 Leaf directory compression creates a new Compressor per leaf])
- **Severity**: low
- **Category**: allocator-churn
- **Planet impact**: ~1220 compressor alloc/dealloc cycles during finalization
- **Denmark impact**: ~11 cycles
- **Effort**: trivial (pass reusable `&mut Compressor`)
- **Cross-box deps**: none
- **Detail**: `gzip_compress()` allocates a new `Compressor` for each call. During `build_leaf_directories`, called once per leaf chunk (~1220 for planet). Each creates ~400 KB of internal state. Could accept a reusable compressor parameter.
- **Status**: new

### F40: Redundant Hilbert tile_id <-> (z,x,y) conversion in writer thread
- **Source**: Box 8, `pipeline.rs:1401` (see [box8-pmtiles-writer.md § 1 Hilbert Tile ID System]); `pmtiles_writer.rs:188` (see [box8-pmtiles-writer.md § 1 Hilbert Tile ID System])
- **Severity**: low
- **Category**: cpu
- **Planet impact**: ~28 integer ops per tile, negligible even at 100M tiles
- **Denmark impact**: None
- **Effort**: trivial (add `add_tile_by_id` method)
- **Cross-box deps**: none
- **Detail**: The pipeline has the tile_id from the sort key, converts to (z,x,y) via `tile_id_to_zxy`, passes to `add_tile`, which immediately calls `xy_to_tile_id` to get back the same tile_id. Pointless round-trip but trivially cheap.
- **Status**: new

### F41: Sorted node store cache size fixed at 4 entries
- **Source**: Box 2, `node_index.rs:465` (see [box2-node-way-storage.md § 1.5 Thread-Local LRU Cache])
- **Severity**: medium
- **Category**: cpu
- **Planet impact**: Bump 4->8 saves ~10-15s wall time; miss cost dominated by DRAM latency (980ns), not cache policy
- **Denmark impact**: 76% hit rate already; 4->8 gives marginal improvement
- **Effort**: trivial (change constant)
- **Cross-box deps**: none
- **Detail**: The thread-local LRU cache has 4 entries at ~2.1 KB each. Memory cost of 8 or 16 entries is negligible (<1 MB total across all threads). The hit rate floor is set by first-access-per-chunk-per-way. Going from 4->8 helps larger ways (20+ nodes spanning 5+ chunks). The real bottleneck is DRAM latency, not cache policy.
- **Status**: already-in-TODO (recommendation to bump 4->8 documented)
