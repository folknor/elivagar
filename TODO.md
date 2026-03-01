# elivagar TODO.

## Memory work — instrumentation prerequisites

Before any memory optimization work (P1-P5), we need measurement infrastructure.
Blocked on brokkr v3 schema (`~/Programs/brokkr/SCHEMA_REDESIGN.md`) which adds
`peak_rss_mb`, `run_kv`, and `project` column. Do that first.

### Elivagar-side instrumentation (after brokkr v3)
- [ ] Add `peak_rss_kb()` helper — read `/proc/self/status` for `VmHWM`
- [ ] Emit per-phase RSS: `phase12_rss_kb=`, `ocean_rss_kb=`, `sort_rss_kb=`, `assemble_rss_kb=`
- [ ] Emit `final_rss_kb=` at pipeline end
- [ ] Add `AtomicUsize` high-water-mark counters for in-flight structures
- [ ] Emit: `max_way_inflight_bytes=`, `max_rel_batch_bytes=`, `max_assemble_batch_bytes=`
- [ ] Emit `sort_chunks=` in final kv summary (already tracked internally, just not printed)

### Already instrumented (sufficient)
- Phase timings: `total_ms`, `phase12_ms`, `ocean_ms`, `phase3_ms`, `phase4_ms`
- Feature/tile counts: `features`, `ocean_features`, `tiles`, `unique_tiles`, `output_bytes`
- Node store stats: `node_store_nodes`, `node_store_groups`

### Research documents
- `notes/memory/p1-byte-budgeted-inflight.md`
- `notes/memory/p2-stream-relation-outputs.md`
- `notes/memory/p3-tighten-assemble-memory.md`
- `notes/memory/p4-configurable-sort-chunk.md`
- `notes/memory/p5-pmtiles-directory-streaming.md`
- `notes/memory/research-conclusions.md` — theoretical overview
- `notes/memory/experiment-matrix.md` — testing methodology

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Performance

### Plantasjen TODO — completed 2026-02-27

- [x] **Re-baseline on plantasjen** — `bench-self.sh` best of 3 at `605a1a5`: 14.2s total (9.2s pbf, 1.4s ocean, 0.5s sort, 2.5s assemble). LRU cache optimizations show modest -0.1s PBF improvement (plantasjen's 64 MB L3 already covered most of the blob, unlike dm6's 16 MB).
- [x] **Confirm assemble phase regression** — **not real**. 2.5s at `605a1a5`, consistent with pre-compression baseline (2.2s). The old 2.7s measurement was noise.
- [x] **Update README performance numbers** — updated to `605a1a5` baseline.
- [x] **Run hotpath on plantasjen** — `decompress_chunk`: avg 560ns (vs 820ns on dm6), 13.4s total (60% wall). The 64 MB L3 gives a 32% latency reduction. P95 is 980ns on both machines — the true DRAM penalty when L3 misses. See hotpath-profile.md for full results.

### Baselines

Plantasjen Denmark baseline (best of 3, `605a1a5`, all optimizations):
14.2s total (9.2s pbf, 1.4s ocean, 0.5s sort, 2.5s assemble). 16.0M features, 56.4K unique tiles, 286 MB output.

Plantasjen Denmark baseline (best of 3, pre-optimization): 14.7s total (9.3s pbf, 1.5s ocean, 0.4s sort, 2.7s assemble). 16.0M features, 53.9K unique tiles, 273 MB output.

dm6 Denmark baseline (best of 3, `2db9494`, all optimizations applied):
21.2s total (13.7s pbf, 2.6s ocean, 0.5s sort, 2.9s assemble). 16.0M features, 56.4K unique tiles, 286 MB output.

dm6 Denmark baseline (best of 3, `d90d4a1`, pre-LRU-cache):
22.9s total (14.6s pbf, 2.9s ocean, 0.5s sort, 2.6s assemble). 16.0M features, 56.4K unique tiles, 286 MB output.

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

## Planet scale

- notes/research/action-plan.md

See [notes/planet-scale.md](notes/planet-scale.md) for the full roadmap.

- [x] Step 1: `pbfhogg node-stats` tool
- [x] Step 2: Validate compression on Germany/Norway/Japan (worst case 72%, planet fits under 64 GB)
- [x] Step 3: SortedNodeStore compression — **75% ratio achieved** (planet: 51 GB, fits in 64 GB)
  - [x] Arena allocation for SortedNodeStore chunks (per-group Vec<u8>)
  - [x] Selective compression — skip FOR when compressed ≥ raw
  - [x] BitPacker1x (32-value blocks) — tried, rejected: metadata overhead > compression gain
  - [x] Shrink ChunkMeta from 64B → 40B — 278 MB savings on Germany
  - [x] Exact-size bitpacking — replaced BitPacker4x (128-value padded blocks) with scalar N-value packing. Compressed chunks: 26% → 80%
  - [x] Flat byte blob per group — eliminated ChunkMeta struct, all metadata inline in `Box<[u8]>`. Removed `bitpacking` crate dependency
- [ ] Step 4: Full pipeline on North America (~17 GB) — needs ≥32 GB RAM
- [ ] Step 5: Full pipeline on Europe (~28 GB) — needs ≥64 GB RAM
- [ ] Step 6: Planet (~75 GB) — needs ≥64 GB RAM hardware

## Planet-scale performance squeeze

Master plan: `notes/research/action-plan.md` (7 tiers, 41 findings).
Deep-dive investigations completed 2026-02-28 on the top 3 items. Findings inline below.
Box-level investigations (Batch 1: Boxes 2, 4, 5, 8) launched 2026-02-28. Results in `.plans/investigations/`.

### Completed

- [x] **Tier 0** — 11 trivial optimizations (`f41e433`). Denmark -2.4%, Japan -2.6%.
- [x] **Tier 1, F18** — Key string → u8 key_id interning (`a6b1977`, fix `6de3e1a`). ~44 GB planet savings.
- [x] **Tier 1, F18b** — Kind value → u8 value_id interning, 129-entry table (`aa5cdff`). ~19 GB planet savings.
- [x] **Tier 1, F20** — Dismissed: osm_id flows to MVT Feature.id, cannot be removed.

Cumulative dm6 results at `aa5cdff` vs baseline `61a85b0` (powersave governor):
| Dataset | Baseline | Current | Delta |
|---------|----------|---------|-------|
| Denmark (461 MB) | 20.5s | 20.2s | -1.5% |
| Norway (1.3 GB) | 53.8s | 51.5s | -4.3% |
| Japan (2.3 GB) | 77.7s | 74.3s | -4.4% |

### Next up: Tier 2 (safety guardrails), then Tier 3 (geometry alloc reduction)

### INVESTIGATED: Sort payload width amplification (Boxes 3, 6) — CONFIRMED, high leverage

**Status:** Ready to implement. Localized change in `wire_format.rs`, ~20 lines each side.

**Problem:** Wire format encodes attribute **keys as raw UTF-8 strings** on every sort record.
There are only ~40 unique key strings in the Shortbread schema, but they're repeated verbatim
across all records. Interning only happens during assembly (`add_feature_to_layer` in
`wire_format.rs:187`), never in the sort phase. Every record carries full string keys through
sort I/O only to have them converted to interned u16 indices at decode time.

**Wire format layout per record (on disk):**
```
Sort framing:  [8B sort_key] [4B data_len]              = 12 bytes
Wire header:   [8B osm_id] [1B geom_type] [4B cmd_cnt]  = 13 bytes
Geometry:      [N*4B commands]                           = variable
Attributes:    [1B attr_count] then per attr:
                 [1B key_len] [key_len bytes key_str]    ← THE WASTE
                 [1B value_type] [value bytes]
```

**Per-record size breakdown by feature type:**

| Feature type | Geometry | Attrs | Overhead | Total | Attr % |
|---|---|---|---|---|---|
| Street (residential, z14, no flags) | 80B | 20B | 25B | 125B | 16% |
| Street (bridge+tunnel+surface) | 80B | 57B | 25B | 162B | 35% |
| Street label (name+ref+ref_rows/cols) | 80B | 89B | 25B | 194B | 46% |
| POI (amenity+cuisine+name+housenumber) | 12B | 120B | 25B | 157B | 76% |
| Water polygon | 160B | 14B | 25B | 199B | 7% |
| Building (no attrs) | 80B | 1B | 25B | 106B | 1% |

**Zoom fan-out amplifier:** Each feature is emitted for every zoom level `z_lo..=z_hi`. A street
with min_zoom=5 and max_zoom=14 produces 10 sort records, each carrying identical attribute
key strings. Denmark: 6.6M ways → 16.0M sort records (2.4x fan-out average).

**Waste quantification:**
- Average key bytes per record: ~2.5 keys × ~8 bytes (1 len + ~7 key_name) = ~20 bytes
- Denmark (16M records): ~280 MB of redundant key string bytes in sort I/O
- Planet (~2.4B records): **~44 GB of redundant key strings**
- `kind` values are also raw strings (~100 distinct static values like "residential", "motorway"),
  averaging ~11 bytes where a u8 enum would cost 1 byte. ~10 bytes/record wasted.
- Combined key+kind savings: **~420 MB Denmark, ~63 GB planet**

**Fix — key IDs (u8):**
~40 unique key strings → fits in a single u8. Replace `[1B key_len][N bytes key_str]` with `[1B key_id]`.
- Encode side (`encode_attrs_bytes`, wire_format.rs:37-39): `push(key_id)` instead of `push(kb.len()); extend(kb)`
- Decode side (`add_feature_to_layer`, wire_format.rs:173-179): read key_id, lookup `KEY_NAMES[key_id]`, call `layer.intern_key()`
- Per-key savings: `kind` saves 4B, `bridge` saves 6B, `surface` saves 7B, `oneway_reverse` saves 15B, `recycling:glass_bottles` saves 24B. Average ~6B per key.

**Fix — kind value IDs (u8):**
~100 distinct `kind` values → fits in u8. Replace `[1B type=string][2B len][N bytes]` with `[1B type=enum][1B kind_id]`.
- Saves ~10 bytes per record on every feature that has a `kind` attribute (nearly all except buildings/ocean).
- Requires a bidirectional lookup table for the ~100 kind values.

**Anchors:** `wire_format.rs:29` (encode), `wire_format.rs:72` (attr encoding), `wire_format.rs:109` (decode),
`wire_format.rs:173-187` (key string parsing + interning), `sort.rs:137` (chunk write), `sort.rs:201` (chunk read).

### INVESTIGATED: Assemble pipeline backpressure (Boxes 1, 7) — DISMISSED, not a real bottleneck

**Status:** Downgraded to low priority. No action needed.

**Architecture recap:** 3-stage pipeline in `phase_assemble` (pipeline.rs:1318-1448):
- Stage 1 (reader thread): k-way merge via `sort_reader.next()`, groups by tile_id, batches 4096 tiles → `read_tx`
- Stage 2 (main thread + rayon): receives batch, `encode_tile_batch()` does parallel MVT encode + gzip → `encode_tx`
- Stage 3 (writer thread): receives encoded batch, `pmtiles.add_tile()` per tile (hash + dedup + buffered write)

Both channels are `sync_channel::<Vec<...>>(1)` (pipeline.rs:1342-1343).

**Why it's not a problem:**

1. **Batch size dominates channel depth.** Each channel message is a `Vec` of 4096 tiles — hundreds of ms
   of work. `sync_channel(1)` with 4096-item batches ≈ `sync_channel(4096)` with single items.
   Natural batching absorbs all per-tile variance.

2. **Double-buffering already achieved.** Reader prepares batch N+1 while encoder processes batch N.
   Encoder processes batch N while writer writes batch N-1. This is textbook double-buffering.

3. **Encoder is the clear bottleneck.** MVT encode + gzip on rayon dominates. The writer is trivially
   cheap (SipHash + HashMap lookup + 1MB BufWriter). Deeper queues can't help when the slowest
   stage is already parallel — you can't buffer your way past the critical path.

4. **Scale doesn't change this.** Denmark: ~14 batches. Planet: ~25K batches. More batches means
   better amortization of startup/drain. Steady-state throughput = encoder throughput regardless
   of queue depth.

5. **Memory cost of deeper queues.** Each in-flight `PendingTile` batch holds raw feature data for
   4096 tiles. Each `EncodedTile` batch holds compressed output (~20-30 MB). Extra queue depth
   just wastes memory for no throughput gain.

**One edge case noted:** If the writer's BufWriter flush triggers a kernel dirty-page writeback stall
longer than one full encoder batch cycle, the encoder could briefly block on `encode_tx.send()`.
Unlikely given 1MB BufWriter and ~5KB average tile, but theoretically possible on very slow storage.
Not worth optimizing for.

**Compression details (for the "compression as CPU sink" item below):**
- Library: libdeflate (C library via `libdeflater` crate), default level 6 (configurable via `--compression-level`)
- Compressor + output buffer reused per rayon worker via `par_iter().map_init()` (pipeline.rs:1458-1464)
- One unavoidable allocation per tile: `gz_buf[..compressed_len].to_vec()` for the owned `EncodedTile`

### INVESTIGATED: Flat node-index fallback safety (Box 2) — CONFIRMED critical, guardrail needed

**Status:** Ready to implement. Low risk of triggering in standard workflow (Geofabrik/OSM planet PBFs
are sorted), but catastrophic if triggered. Guardrails are cheap and important.

**Current selection logic** (pipeline.rs:335-346):
```rust
let is_sorted = reader.header().is_sorted() || config.force_sorted;
// if is_sorted → SortedNodeStore::new()
// else → NodeIndex::create() (flat mmap)  ← NO GUARDS
```

`is_sorted()` checks `optional_features` for `"Sort.Type_then_ID"` string in the PBF HeaderBlock
(pbfhogg `read/block.rs:203-212`). Geofabrik and OSM planet dumps set this flag. The `--force-sorted`
CLI flag (main.rs:80-82) bypasses the header check (panics if nodes aren't monotonic).

**Flat path behavior at planet scale (catastrophic):**
1. File starts at 1 GB, grows in 1 GB increments (node_index.rs:25, no upper limit)
2. Max node ID ~12B → file grows to `12B * 8 = 96 GB` sparse file
3. Actual pages: ~68 GB (8.5B real nodes × 8 bytes)
4. On 64 GB RAM: continuous page eviction during random way-processing lookups
5. Previous madvise investigation showed **3.5x slowdown** (45→160s) from page fault overhead
   on sparse files — planet would be far worse
6. Likely outcome: hours instead of 30-60 min, possible OOM kill

**Risk scenarios for triggering flat path:**
- Third-party PBF tools that don't set the sorted header flag
- PBF merge/filter operations that strip `optional_features`
- Files sorted in practice but missing the header (this is why `--force-sorted` exists)
- Corrupted/truncated headers where `optional_features` is empty

**Current gaps:**
- No file size check or limit in `NodeIndex::put()` (node_index.rs:67-87) — unbounded growth
- No PBF size check at the decision point — a 73 GB planet PBF silently takes the flat path
- `run-safe.sh` estimation (lines 65-66) assumes SortedNodeStore — wildly wrong for flat path

**Recommended fixes:**

P0 — **PBF size guard at decision point** (pipeline.rs:344-345):
When `is_sorted` is false, check PBF file size via `std::fs::metadata`. If >1 GB, abort with:
```
ERROR: PBF file is 73 GB but does not declare Sort.Type_then_ID.
The flat node index would create a ~96 GB sparse file, causing severe performance
degradation on machines with <128 GB RAM. Options:
  1. Use --force-sorted if the PBF is actually sorted (most Geofabrik extracts are)
  2. Sort the PBF first with: osmium sort input.pbf -o sorted.pbf
  3. Use a sorted PBF from Geofabrik or planet.openstreetmap.org
```

P0 — **Hard cap on flat index size** (node_index.rs:71, inside `if needed > self.file_len`):
Add `const MAX_FLAT_INDEX_SIZE: u64 = 16 * 1024 * 1024 * 1024` (16 GB). Panic with clear message
if growth would exceed this. Prevents runaway even if decision-point guard is bypassed.

P1 — **Fix memory estimation in `brokkr run --mem`**:
The pre-flight formula `store_gb = (node_count * 8 + 1.5G) / 1e9` assumes SortedNodeStore.
If flat path is taken, estimate should use `max_node_id * 8` instead. Check sorted flag via
`pbfhogg fileinfo` or just always assume sorted (since we're adding the PBF size guard above).

P2 — **Heuristic sorted detection** (pipeline.rs:386-392, `handle_node!` macro):
If first N nodes (e.g., 10,000) arrive in strictly ascending order, offer to switch to
SortedNodeStore mid-stream. Feasible since SortedNodeStore builds incrementally. Nice-to-have
safety net for PBFs that are sorted but lack the header flag.

**Anchors:** `pipeline.rs:335-346` (decision), `node_index.rs:25` (GROW_INCREMENT),
`node_index.rs:40-63` (create), `node_index.rs:67-87` (put + growth), `node_index.rs:109-127` (read),
`pbfhogg read/block.rs:203-212` (is_sorted), `main.rs:80-82` (--force-sorted).

### INVESTIGATED: Sorted node-store cache sizing (Box 2) — CONFIRMED, modest gain, cheap to try

**Status:** Bump from 4→8 entries is free and likely helps. Bigger wins from prefetching (speculative).

**Cache implementation** (node_index.rs:436-546):
Each `CacheEntry` is ~2,098 bytes: `group_id(8) + chunk_idx(8) + node_mask(32) + coords(2048) + count(2)`.
4-entry `DecompressCache` = ~8.4 KB per thread, stored in `thread_local!` with `UnsafeCell` (not RefCell)
to avoid borrow-check overhead on ~48M calls per Denmark run.

LRU policy: linear scan of 4 entries checking `(group_id, chunk_idx)`. Hit → `swap(0, i)` to promote.
Miss → `swap(0, CACHE_ENTRIES-1)` to evict LRU, then decompress into slot 0.

**Access pattern analysis:**
- Node lookups follow way-reference order (pipeline.rs:740-746, `raw.node_refs.iter().filter_map(|&id| node_reader.get(id))`)
- OSM editing assigns node IDs chronologically — a way's nodes are typically consecutive IDs
- With chunks covering 256 consecutive node IDs, a typical 5-10 node way hits 1-3 chunks within 1 group
- Inter-way locality also good: PBF blocks contain ~8K ways sorted by way_id, and consecutive ways
  reference similar node-ID bands
- Denmark: 23.8M decompress calls out of ~48M get() calls → ~50% raw miss rate, 76% effective hit rate
  (some get() calls return None without decompressing)

**What changes at planet scale:**
The hit rate is driven by OSM way locality patterns, NOT blob size — same editing patterns regardless
of dataset. **Hit rate should be similar (~76%) at planet scale.** What changes is the **cost per miss**:
- Denmark (270 MB blob): avg 560ns per decompress (partial L3 coverage on 64 MB cache)
- Planet (51 GB blob): avg ~950-980ns per decompress (no L3 coverage at all — pure DRAM latency)
- P95 is 980ns on both plantasjen and dm6 — the true DRAM floor when L3 misses

**Planet decompress cost estimate (same 76% hit rate):**
- ~6.4B lookups × 0.24 miss rate = 1.54B decompress calls
- At 980ns avg: **1,505 seconds cumulative** across all rayon threads
- Wall time contribution: 63-125s (across 12-24 threads) = **4-8% of PBF wall time**

**Cache size tuning — memory cost is trivial:**

| Entries | Per thread | 24 threads |
|---------|-----------|------------|
| 4       | 8.4 KB    | 0.19 MB    |
| 8       | 16.8 KB   | 0.39 MB    |
| 16      | 33.7 KB   | 0.77 MB    |
| 32      | 67.3 KB   | 1.54 MB    |

All negligible vs the 51 GB blob. Even 64 entries × 24 threads = 3 MB.

**Expected hit rate gains (speculative):**

| Entries | Est. hit rate | Planet decompress calls | DRAM cost (980ns) | Wall savings vs 4 |
|---------|--------------|------------------------|-------------------|--------------------|
| 4       | ~76%         | 1.54B                  | 1,505s            | baseline           |
| 8       | ~79-81%      | ~1.22-1.34B            | ~1,200-1,310s     | ~10-15s wall       |
| 16      | ~82-84%      | ~1.02-1.15B            | ~1,000-1,130s     | ~20-25s wall       |

Diminishing returns after 16. The miss rate floor is set by first-access-per-chunk-per-way, which
no cache size eliminates. Going from 4→8 helps larger ways (20+ nodes spanning 5+ chunks) and
improves inter-way cross-pollination within PBF blocks.

**The actual decompression** (node_index.rs:341-387):
- Counts set bits in node_mask, reads 10-byte header (min_lat, min_lon, lat_bits, lon_bits),
  calls `bitunpack_values()` twice (lat/lon offsets), reconstructs absolute coords
- `bitunpack_values` (lines 217-252): u64 unaligned reads, shift+mask per value. Stack-allocated
  `[0u32; 256]` scratch buffers (2 KB, fits in L1)
- Compute: ~25ns when L1-hot (synthetic benchmark). Memory: ~535-955ns (DRAM stall)
- **95%+ of the cost is waiting for DRAM**, not arithmetic

**Higher-impact alternative — software prefetching:**
If the next `node_id` in the way is known (it is — from `raw.node_refs`), the blob offset for the
next chunk could be prefetched while the current decompress executes. This hides DRAM latency by
overlapping computation with memory fetches. Would require `_mm_prefetch` intrinsics or `std::arch`
prefetch hints. More complex than cache size tuning but attacks the fundamental bottleneck.

**Another alternative — batch node resolution:**
Instead of resolving node refs one at a time (`filter_map`), sort the way's node_refs by ID to
improve spatial locality in the blob, then unsort results. Trades compute for cache-friendliness.
Speculative — needs measurement.

**Recommendation:** Bump `CACHE_ENTRIES` from 4 to 8 as a cheap first step. Measure on North America
(17 GB PBF) before going further. Prefetching is the real win but needs careful implementation.

**Anchors:** `node_index.rs:436-453` (CacheEntry), `node_index.rs:465-489` (DecompressCache),
`node_index.rs:497-546` (get_from_group_cached), `node_index.rs:341-387` (decompress_chunk),
`node_index.rs:217-252` (bitunpack_values), `pipeline.rs:740-746` (way node resolution).

### INVESTIGATED: Tile fanout and per-tile clipping cost (Box 4) — CONFIRMED, multiple optimization paths

**Status:** Several concrete fixes identified. Two are low-hanging fruit (allocation elimination),
two are architectural improvements (interior tile detection, inner ring bbox prefilter).

**Five emission paths exist** (pipeline.rs), each with different clipping behavior:

| Path | Function | Geom type | Clipping | Hot? |
|---|---|---|---|---|
| Points | `emit_point_or_centroid` (line 1027) | point | None (just project) | No |
| Lines | `emit_line_feature` (line 1066) | linestring | Cohen-Sutherland per segment | Yes (2.0M calls, 7.9s) |
| Polygons | `emit_polygon_feature` (line 1139) | single ring | Sutherland-Hodgman 4-pass | Yes (4.5M calls, 11.3s) |
| Multipolygons | `emit_multipolygon_feature` (line 1204) | multi-ring | S-H per ring per tile | **Hottest** (257K calls, 23.4s) |
| Ocean | `emit_ocean_polygon` (ocean.rs:362) | polygon | S-H + scanline fill | Separate phase |

**Simplification is done BEFORE tile fanout** (correct architecture):
- `for_each_zoom_simplified` (geometry.rs:354): cascading DP from z_hi down to z_lo
- z14: no simplification (tolerance=0). z13: tol=0.0000305. z7: tol=0.00195.
- Early exits: subpixel bbox check (O(1)), min_points check, DP convergence tracking
- Bbox recomputed from simplified geometry → fewer tiles at lower zooms

**Existing early-rejection optimizations:**
1. `is_single_tile` fast path (geometry.rs:999): skips clipping entirely when bbox fits in one tile — covers most small features
2. Outcode AND pre-test (geometry.rs:702-714): rejects geometries entirely outside a tile edge. O(n) worst case, O(1) best case (early break when first inside vertex found)
3. `merc_bbox_is_subpixel` (geometry.rs:373): breaks zoom cascade when geometry invisible
4. DP convergence tracking (geometry.rs:382): skips DP when already below tolerance
5. Ocean row pre-clip (ocean.rs:460-466): clips polygon to Y-band before per-tile clip
6. Ocean scanline fill: interior tiles get cheap fill rectangles instead of full clipping

**Worst case — large country boundary at z14:**
Consider Germany boundary: ~10,000+ vertices outer ring, bbox spans ~600×500 = 300,000 tiles at z14.
- Perimeter tiles (~2,000): full S-H clip, O(4 × 10,000) = 40K ops each
- Interior tiles (~298,000): outcode pre-test passes fast (first vertex inside), BUT then full
  4-pass S-H runs anyway, producing a clipped copy ≈ original. This is wasteful.
- **Each inner ring** is also clipped against every tile — no spatial index, no bbox prefilter

**Hotpath profile confirms** (Denmark, `605a1a5`):
- `for_each_zoom_simplified_multi`: 257K calls, 91.00us avg, 23.4s total, **105% of wall time** (#2 CPU consumer)
- `for_each_zoom_simplified`: 6.6M calls, 1.78us avg, 11.7s total
- `emit_polygon_feature`: 4.5M calls, 2.48us avg, 11.3s total
- `clip_polygon_into`: 8.1M calls, 327 B avg allocation, 2.5 GB total alloc

**Optimization opportunities, ranked by impact:**

**1. `to_tile_coords_into` in `emit_multipolygon_feature`** (LOW-HANGING FRUIT)
The multipolygon path uses the allocating `to_tile_coords` at lines 1235, 1247, 1261, 1274 (pipeline.rs).
Each call allocates a new `Vec<(i32, i32)>`. The single-polygon path (`emit_polygon_feature`) already
uses `to_tile_coords_into` with hoisted buffers. Fixing this eliminates 4 heap allocations per tile
in the hottest emission path (257K features × many tiles × multiple rings).
Same issue in ocean.rs:569, 579 (`emit_boundary_tile`).

**2. Interior tile detection for large polygons** (MEDIUM EFFORT, HIGH IMPACT)
For tiles where all 4 corners are inside the outer ring, the polygon fully covers the tile.
Currently these run full S-H and produce a near-copy of the tile extent. Adding a "tile inside polygon"
test (4 point-in-polygon checks, or even simpler: compare tile bbox against ring winding) would allow
emitting a full-tile rectangle without clipping. Helps country/state boundaries and large forests
at high zoom. For Germany at z14: ~298,000 interior tiles could skip clipping entirely.

**3. Inner ring bbox prefilter** (LOW EFFORT, HIGH IMPACT FOR RELATIONS)
Before clipping each inner ring against a tile (lines 1269-1281), check if the inner ring's bbox
intersects the tile's clip rect. Currently every inner ring runs the full outcode scan O(n) against
every tile. Precomputing inner ring bboxes (once per zoom level, after simplification) and doing a
fast O(1) bbox-vs-bbox test would skip most inner rings for most tiles.

**4. Row pre-clip for non-ocean multipolygons** (MEDIUM EFFORT)
The ocean path already clips polygons to Y-band before per-tile clipping, dramatically reducing
input vertex count. The regular `emit_multipolygon_feature` does NOT do this — each tile clips
the full simplified polygon. For large polygons spanning many tile rows, row pre-clipping could
provide a significant speedup.

**5. `clip_linestring_into` buffer reuse** (LOW EFFORT)
`clip_linestring` (geometry.rs:559) returns `SmallVec<[Vec<Point>; 1]>`. SmallVec avoids outer alloc
for single-segment case, but each clipped segment is a fresh `Vec<Point>`. A buffer-reusing variant
would eliminate heap allocs in the 2.0M-call line emission path.

**6. `ring_refs` SmallVec** (TRIVIAL)
`let ring_refs: Vec<&[(i32, i32)]>` at pipeline.rs:1285 allocates per tile. Replace with
`SmallVec<[&[(i32,i32)]; 4]>` for the common 1-4 rings case.

**Anchors:** `pipeline.rs:1027` (emit_point), `pipeline.rs:1066` (emit_line), `pipeline.rs:1139` (emit_polygon),
`pipeline.rs:1204` (emit_multipolygon), `geometry.rs:354` (for_each_zoom_simplified),
`geometry.rs:691` (clip_polygon_into), `geometry.rs:780` (clip_polygon_edge_into),
`geometry.rs:559` (clip_linestring), `pipeline.rs:1235,1247,1261,1274` (allocating to_tile_coords),
`pipeline.rs:1269-1281` (inner ring clip loop), `ocean.rs:460-466` (row pre-clip).

### INVESTIGATED: Compression as top CPU sink (Box 7) — CONFIRMED, per-zoom strategy is the win

**Status:** Per-zoom compression levels can be implemented in ~20 lines. Already has CLI `--compression-level`.
A global level drop (6→3) works today for quick wins.

**Current compression setup** (pipeline.rs:1450-1525):
- Library: libdeflate (C library via `libdeflater` crate)
- Default level: 6 (main.rs:25, CLI `--compression-level 0-10`, libdeflate actually supports 0-12)
- Compressor created once per rayon worker via `map_init` (pipeline.rs:1458-1464) — properly reused
- Output buffer (`gz_buf`) also reused. One unavoidable `.to_vec()` per tile (line 1519) for owned `EncodedTile`
- No size threshold: every non-empty MVT tile is compressed, even tiny ones
- Level applied uniformly to ALL tiles regardless of zoom or density

**MVT encoding** (mvt.rs:276-284, `encode_tile_with`):
Pure protobuf serialization — iterates layers, encodes features with interned key/value tags.
Key/value interning happens during `add_feature_to_layer` (wire_format.rs), not during encode.
`merge_same_attr_geometries` (mvt.rs:494-577) runs before encode: sorts features by (geom_type, tags),
concatenates consecutive same-attr geometries. Denmark: reduced feature count 97% (1.2M→35K), output
630→457 MB (27% reduction). Merging is NOT a significant CPU cost relative to compression.
Allocation: `encode_tile_with` allocates a fresh `Vec<u8>` per tile (line 277, 4096 initial capacity).
Germany avg: 54 KB uncompressed per tile. Could be changed to accept `&mut Vec<u8>` for reuse.

**Tile size distribution (Denmark, level 6, from tile-comparison notes):**

| Zoom | Tiles | MB | Avg compressed | % of tile count |
|------|-------|----|----------------|-----------------|
| z0-z8 | 494 | 9.9 | 20 KB | 0.3% |
| z9-z10 | 2,720 | 35.9 | 13 KB | 1.7% |
| z11-z12 | 18,677 | 85.6 | 4.6 KB | 11.5% |
| z13-z14 | 141,094 | 303.4 | 2.2 KB | **86.5%** |

z13+z14 account for **86.5% of tiles** and 70% of output bytes. These are the compression throughput
bottleneck by sheer count.

**libdeflate level characteristics:**
- Level 1: ~3-4x faster than level 6, ~8-15% larger output (simple greedy matching)
- Level 3-4: ~2x faster than level 6, ~3-5% larger output (good balance)
- Level 6: default, standard deflate quality
- Level 9-12: ~2-5x slower than level 6, ~1-3% smaller output (diminishing returns)

**Assemble time split estimate:**
Denmark assemble: 2.5s. Compression fraction likely 40-60% based on hotpath allocation profiles.
Germany assemble: 35.5s (225K tiles, 2.6 GB output). Planet: projected 500-1000s at level 6.

**Optimization 1 — Per-zoom compression levels** (estimated: -20-30% assemble time)

| Category | Zooms | Tiles (DK) | Suggested level | Rationale |
|----------|-------|------------|-----------------|-----------|
| Low | z0-z8 | 494 (0.3%) | 9 | Few tiles, max quality, negligible time |
| Mid | z9-z12 | 21,397 (13.2%) | 6 | Moderate count, keep default |
| High | z13-z14 | 141,094 (86.5%) | 3 | Vast majority, speed matters |

Implementation: ~20 lines in `encode_tile_batch`. Extract zoom from `tile_id_to_zxy(tile.tile_id)`.
Create 2-3 `Compressor` instances per rayon worker in `map_init` (one per level tier). Select
compressor by zoom. libdeflate compressors are small structs — memory cost negligible.

At planet scale (~100M tiles): compressing 86M tiles at level 3 instead of 6 could save
30-40% of compression CPU = **150-400 seconds**.

**Optimization 2 — Global lower level** (user-tunable today)
`--compression-level 3` already works. Saves ~30-40% assemble time at ~5% size penalty.
At planet scale: ~1.5 GB larger output (30 GB → 31.5 GB). Good for iteration builds.
Documented in `notes/website-guide.md:130`: "Level 3-4 is a good tradeoff for faster builds."

**Optimization 3 — zstd tile compression** (estimated: -50-70% assemble time, BLOCKED)
PMTiles v3 supports zstd (header byte 98 = 4, hardcoded as gzip=2 at pmtiles_writer.rs:414).
zstd level 1-3 is typically 3-5x faster than gzip level 6 with comparable or better ratio.
**Blocked by client-side support**: MapLibre GL JS and most PMTiles viewers expect gzip.
Browser `fetch()` handles gzip natively but zstd requires explicit JS decompression.
Would need `zstd` crate dependency (not currently in Cargo.toml).

**Optimization 4 — MVT Vec pooling** (estimated: -5-10% assemble time)
`encode_tile_with` allocates a fresh `Vec<u8>` per tile (line 277). Change to accept `&mut Vec<u8>`
parameter, clear and reuse per rayon worker. Saves one 54 KB avg allocation per tile.
Separate from compression but easy to combine.

**Anchors:** `pipeline.rs:1450-1525` (encode_tile_batch), `pipeline.rs:1458-1464` (compressor init),
`pipeline.rs:1514-1519` (compression call), `main.rs:25` (default level), `main.rs:56-63` (CLI),
`mvt.rs:276-284` (encode_tile_with), `mvt.rs:494-577` (merge_same_attr_geometries),
`pmtiles_writer.rs:414` (hardcoded gzip header).

### INVESTIGATED: Ocean split policy (Box 5) — DISMISSED, already well-tuned

**Status:** No changes needed. Constants are empirically justified. Configurable params are low-effort
nice-to-have but current values are sound for all tested workloads including planet projections.

**Ocean pipeline flow** (ocean.rs, 863 lines):
1. **Parse**: mmap shapefile (`water-polygons-split-3857`, EPSG:3857), filter by `data_bounds` bbox
2. **Pre-split**: clip large polygons (≥500 vertices) to z8 tile grid boundaries
3. **Process**: rayon parallel — per polygon, `emit_ocean_polygon` does cascading DP simplification +
   scanline edge rasterization + row pre-clip + per-tile S-H clip for boundary tiles + fill for interior
4. **Flush**: each rayon worker flushes accumulated sort records to chunk files

**Split policy** (ocean.rs:200-201):
- `SPLIT_Z=8`: polygons clipped to z8 tile grid (256×256 = 65K tiles globally)
- `SPLIT_MIN_VERTICES=500`: only split polygons with ≥500 vertices
- NOT recursive — single-level clip to z8 grid
- Guard: only applied when `max_zoom >= SPLIT_Z` (line 202)

**Why z8 is the right choice:**
The `water-polygons-split-3857` shapefile is already pre-split on a 1×1-degree grid (~23,000 polygons
globally), so many polygons already fit within a single z8 tile. After splitting at z8, each sub-polygon
covers at most 2^(z-8) tiles per axis at zoom z:
- z8: 1 tile, z10: 16 tiles, z12: 256 tiles, z14: 4,096 tiles (worst case)

Without splitting, a fjord polygon spanning 10×10 z8 tiles would cover 409,600 tiles at z14 with
thousands of vertices each. Post-split, 100 sub-polygons of 50-200 vertices each — dramatically
cheaper per-tile clipping.

Going lower (z6): sub-polygons still too large, heavy downstream clipping.
Going higher (z10+): many tiny sub-polygons with per-polygon overhead (simplification setup,
edge rasterization) outweighing savings. Diminishing returns.

**Why 500 vertices is right:** Polygons under 500 vertices are cheap to clip per-tile. The shapefile's
1-degree pre-split means most polygons are 100-800 vertices. Only large coastline polygons exceed 500.
The threshold was validated against Norway's fjord polygons (the stress test) where it delivered a
59% ocean phase reduction (16.6s → 6.8s, documented in `notes/geographic-profiles.md:196-209`).

**Clone paths at lines 238/241 are not a concern:**
The split phase runs once, single-threaded, on a few thousand polygons. `clip_polygon_into` writes
into reusable `clip_a` buffer, so the result must be cloned before the buffer is overwritten.
Cloning a 500-vertex polygon is ~8 KB memcpy — microseconds. Total split cost for planet: <1 second.

**Chunk count is bounded:** Each rayon worker creates at most a few chunk files during ocean flush.
Denmark: ~5-15 ocean chunks. Planet: maybe 50-100. The k-way merge heap handles this fine —
O(N log K) where K is chunk count, and log(100) is negligible.

**Scanline fill optimization** (ocean.rs:350-543):
Instead of clipping every tile in the bbox, DDA edge rasterization identifies boundary tiles
(where polygon edges actually cross). Interior tiles get cheap fill rectangles with a single
point-in-polygon test per gap. Reduces PIP tests from O(bbox_tiles) to O(gaps × rows).
Row pre-clip (lines 460-486) clips polygon to Y-band before per-tile clipping — reduces
per-tile input from ~1000 vertices to ~50.

**LandMask** (z14 resolution, 32 MB bitset): filters out tiles with no PBF features.
Cuts tile output ~5x (from ~300K to ~56K unique tiles for Denmark).

**Planet-scale projection:** Ocean phase is 1.4s for Denmark (10% of total). Norway stress test: 6.8s.
Planet: likely 5-15% of total wall time, consistent with regional behavior. The split policy
is not the bottleneck — total sort record volume from ocean features is the scaling factor.

**Anchors:** `ocean.rs:200-201` (constants), `ocean.rs:202` (guard), `ocean.rs:210-250` (split logic),
`ocean.rs:238,241` (clone paths), `ocean.rs:350-543` (emit_ocean_polygon),
`ocean.rs:460-486` (row pre-clip), `ocean.rs:602` (rasterize_ring_edges).

### INVESTIGATED: PMTiles dedup cap saturation (Box 8) — LOW IMPACT, simple fix available

**Status:** The 1M cap is probably NOT a significant problem at planet scale. Ocean-only tile skipping
already eliminates the dominant duplicate source. Raising cap to 5-10M is cheap insurance.

**Dedup mechanism** (pmtiles_writer.rs:187-226):
1. Hash compressed tile data with `DefaultHasher` (SipHash-1-3) → 64-bit hash (line 190-192)
2. Check `HashMap<u64, (u64, u32)>` (hash → blob offset + compressed length) (line 194)
3. If hash matches AND length matches: reuse offset, skip blob append → deduplicated (line 196-203)
4. If no match: append to blob, insert into dedup map **only if** map < `MAX_DEDUP_ENTRIES` (line 220-222)
5. `MAX_DEDUP_ENTRIES = 1_000_000` (line 56)

**Cap behavior:** Stops inserting new entries but **continues checking existing ones**. Tiles whose
hash was recorded before the cap was hit can still be deduplicated. No logging when cap is hit.

**Collision handling:** Hash match + length match only — no full content verification. False positive
probability analyzed as ~2^-81 per pair (2^-64 SipHash × 2^-17 length match). Negligible even at
planet scale.

**When does the cap fill during a planet run?**
Tiles arrive in Hilbert order: z0 first, then z1, z2, ..., z14.
- z0-z10: ~420K cumulative unique tiles (under cap)
- z11: ~1.26M unique tiles at this zoom → **cap fills during z11** (~1.7M cumulative)
- z12 (~5M unique), z13 (~20M unique), z14 (~80M unique): no new entries inserted

**~99.3% of unique tiles (z12-z14) are added after the cap is full.** However, the existing 1M
entries (covering z0-z11) remain available for matching.

**Why it doesn't matter much:**
1. **Ocean-only tile skipping** (pipeline.rs:1353-1358) eliminates the dominant duplicate source.
   Before this optimization: 667K tiles → 54K unique (92% dedup). After: 56K → 54K (3.6% dedup).
   The highest-value dedup targets are already removed from the pipeline.

2. **z12-z14 tiles are almost always unique.** Each tile has distinct geometry from clipping to
   unique tile boundaries. The only realistic duplicates are tiles with a single simple feature
   (e.g., just a land fill polygon). Estimated duplicate rate at z12-z14: **under 1%**.

3. **Early entries are the most valuable.** Ocean fill tile hashes and low-zoom sparse tiles are
   added early (z0-z10) before the cap. These are the patterns most likely to recur.

4. **Output size impact is minimal.** If ~2% of post-cap tiles would have been deduplicated at
   ~2 KB each: 100M tiles × 2% × 2 KB = 4 GB wasted out of ~100+ GB archive = ~3-4% inflation.

**Memory cost at various caps:**

| Entries | Memory (with HashMap overhead) |
|---------|-------------------------------|
| 1M (current) | ~32 MB |
| 5M | ~162 MB |
| 10M | ~324 MB |
| 50M (unlimited) | ~1.6 GB |
| ~200M (true planet) | ~7 GB (per code comment, lines 52-55) |

**Dedup rates by region (post ocean-only-skip):**

| Region | Total tiles | Unique tiles | Dedup rate |
|--------|------------|--------------|------------|
| Denmark | ~56K | ~54K | ~3.6% |
| Norway | ~595K | ~519K | ~12.8% |

**Recommended fixes:**

**Simple — raise cap to 5-10M** (one line change, pmtiles_writer.rs:56):
Costs ~160-320 MB RAM (well within 13 GB headroom on 64 GB machine for planet runs).
Covers z0-z12 fully and most of z13, capturing the vast majority of duplicate-prone tiles.

**Add telemetry** (regardless of cap change):
Print `eprintln!` when cap is first hit, and final dedup stats (map size, hit count, miss count)
at end of run. Lets planet runs quantify actual impact without algorithmic changes.

**NOT recommended:**
- Windowed/LRU dedup: duplicates are scattered (all "just land" tiles), not spatially nearby.
  LRU window would miss most of them since duplicates are separated by thousands of tiles in
  Hilbert order.
- Bloom filter: adds complexity, still needs secondary lookup for offset. Overkill for <3% dedup.
- Tiered dedup (full for small tiles, skip for large): sound but adds complexity for marginal gain.

**Anchors:** `pmtiles_writer.rs:56` (MAX_DEDUP_ENTRIES), `pmtiles_writer.rs:125` (HashMap type),
`pmtiles_writer.rs:187-226` (add_tile with dedup), `pmtiles_writer.rs:190-192` (hashing),
`pmtiles_writer.rs:194-203` (dedup check), `pmtiles_writer.rs:220-222` (cap guard),
`pmtiles_writer.rs:414` (gzip header byte), `pipeline.rs:1353-1358` (ocean-only skip).

## Code TODOs

- [ ] **Dedup correctness is probabilistic in PMTiles writer:** Dedup accepts
  hash match + length match without byte-for-byte validation.
  Ref: `src/pmtiles_writer.rs:196`. Collision risk is very low, but non-zero;
  if strict correctness guarantees are needed, this is the place.

- [ ] **Clear perf bug/opportunity in tile assembly:** `compressed.clone()` is
  done per tile in encode path. Ref: `src/pipeline.rs:1708`. Unnecessary
  copy/allocation pressure; should be fixable without behavior changes.

- [ ] **Potentially expensive finalize step in way index:** Finalization reads
  full offsets file into memory and sorts. Ref: `src/way_index.rs:197`. Can
  create a big late-phase cost (time + transient memory), even if not the top
  RSS driver.

- [ ] **Double decode/iteration on way blocks:** Ways are counted via one
  iteration, then iterated again for extraction. Ref: `src/pipeline.rs:466`,
  `src/pipeline.rs:503`. Pure throughput waste on large runs.

- [ ] **Small-but-frequent alloc churn in tag handling:** Owned tags are copied
  to `Vec<(String, String)>` for ways, then converted again to borrowed vec.
  Ref: `src/pipeline.rs:508`, `src/pipeline.rs:788`. Good candidate for
  CPU/alloc cleanup independent of memory envelope work.

- [ ] **`write_to` does a full directory collection pass in streaming mode:**
  Streaming still materializes full directory entries vector later.
  Ref: `src/pmtiles_writer.rs:349`. Mostly memory-related, but also
  architecture/correctness complexity for very large outputs.
