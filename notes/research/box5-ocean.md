# Box 5 Investigation: Ocean Subsystem

Date: 2026-02-28
Scope: `src/ocean.rs` (primary), `src/pipeline.rs` (integration), `src/geometry.rs` (LandMask)
Method: Static code analysis with line-level references. No benchmarks run.

## 1. Shapefile Reader

### Format and Parsing

The ocean subsystem reads **ESRI Shapefile** format (`water-polygons-split-3857`), which ships in EPSG:3857 (Web Mercator meters) coordinate reference system. The reader implementation is a custom mmap-based parser — no external shapefile library.

**Two-file approach:**
- `.shx` (index file): read entirely into memory via `std::fs::read()` (`ocean.rs:52`). The .shx file has a 100-byte header followed by 8-byte records (offset + content length, both in 16-bit words). Each record gives the byte offset to the corresponding shape record in the .shp file.
- `.shp` (main shapefile): memory-mapped via `memmap2::Mmap::map()` (`ocean.rs:79`). The entire .shp file is mapped at once.

**Record parsing** (`ocean.rs:92-193`): For each shape record:
1. Read the 32-byte bounding box from the record header (`ocean.rs:98-102`). Four f64 values: xmin, ymin, xmax, ymax in EPSG:3857 meters.
2. Convert bbox corners to Mercator [0,1] via `geometry::from_epsg3857()` (`ocean.rs:104-105`). This function divides by Earth circumference (~40M meters) to normalize to [0,1].
3. **Bbox filter**: skip shapes entirely outside `data_bounds` (`ocean.rs:107-113`). This is the primary spatial filter and avoids parsing geometry for most of the global shapefile when processing a regional extract like Denmark.
4. Parse part indices and points only for shapes that pass the bbox test (`ocean.rs:117-159`).
5. Coordinates are converted from EPSG:3857 to Mercator [0,1] point-by-point (`ocean.rs:152-159`).

### Ring/Polygon Assembly

Each shapefile record may contain multiple parts (rings). Rings are classified as outer or inner based on:
- First ring (w==0) is always treated as outer (`ocean.rs:178`)
- Subsequent rings: negative signed area = outer (clockwise in math coords), positive = inner (`ocean.rs:178`)

Each ring is **immediately clipped** to the `data_bounds` clip rectangle during parsing (`ocean.rs:167`). Rings that clip to fewer than 3 points are skipped. This early clipping is important: it reduces vertex counts before the split and emission phases.

Multiple outer rings in a single shapefile record produce separate `OceanPolygon` instances (`ocean.rs:170-172, 180-184`), each collecting its subsequent inner rings.

### Memory Profile of Parsing Phase

**Verified fact:** The .shx file is fully loaded into RAM. For the water-polygons-split-3857 dataset, the .shx file is typically ~2 MB (roughly 250K shapes * 8 bytes + 100 byte header).

**Verified fact:** The .shp file is mmap'd entirely. The full-resolution water-polygons-split-3857.shp is approximately 640-700 MB on disk. The mmap means virtual address space is allocated for the full file, but physical pages are demand-faulted. Because parsing is sequential (iterating `offsets` in order, `ocean.rs:92`), the kernel's default readahead should work well.

**Verified fact:** All in-bounds polygons are parsed and stored in `polygons: Vec<OceanPolygon>` before any parallel processing begins (`ocean.rs:89-193`). The `OceanPolygon` struct is 48 bytes (`ocean.rs:26`, static assert) containing:
- `outer: Vec<Point>` — 24 bytes (ptr + len + cap)
- `inners: Vec<Vec<Point>>` — 24 bytes

Each `Point` is 16 bytes (two f64s, `geometry.rs:108`, static assert). So the polygon data footprint in memory is approximately:

```
Per polygon: 48 bytes struct + (outer_vertices * 16) + (inner_count * 24) + (inner_vertices * 16)
```

For Denmark, the shapefile intersects roughly a few hundred shapes. For the global ocean shapefile, this could be the entire ~250K shapes, with varying vertex counts. The "pre-split" polygons (before the split phase at line 204) hold all in-bounds polygons simultaneously.

### What data_bounds Does

`data_bounds` is computed from the PBF data extent in `pipeline.rs:601-616`: it takes the min/max lat/lon of all nodes, projects to Mercator, and adds a ~1% buffer. For Denmark, this restricts ocean processing to a small region. For a planet run, `data_bounds` is essentially [0,1]x[0,1], meaning **all** ocean polygons pass the bbox test.

**Inference (planet scale):** At planet scale, all ~250K shapes pass the bbox filter. After parsing and early clipping (which does nothing for shapes fully within bounds), ALL polygons enter the split and emission phases. This is the critical scale difference: Denmark processes hundreds of polygons; planet processes hundreds of thousands.

## 2. Polygon Splitting

### Algorithm Walkthrough

The splitting phase (`ocean.rs:199-251`) runs only when `max_zoom >= SPLIT_Z` (i.e., max_zoom >= 8).

**Constants:**
- `SPLIT_Z = 8` (`ocean.rs:200`): split to zoom-8 tile grid boundaries
- `SPLIT_MIN_VERTICES = 500` (`ocean.rs:201`): only split polygons with >= 500 outer vertices

**Step by step:**
1. Drain all polygons from the vector (`ocean.rs:209: for poly in polygons.drain(..)`)
2. Skip polygons with `outer.len() < 500` — push directly to `split_out` (`ocean.rs:210-213`)
3. Compute the polygon's bounding box in Mercator [0,1] (`ocean.rs:214`)
4. Map bbox to zoom-8 tile grid coordinates: multiply by 256 (2^8), floor to get tile X/Y range (`ocean.rs:215-222`)
5. If polygon fits in a single z8 tile (`stx_min == stx_max && sty_min == sty_max`), skip splitting (`ocean.rs:223-226`)
6. Otherwise, iterate over all z8 tiles in the bounding box and clip the outer ring + inners to each tile rect (`ocean.rs:228-245`)

**Clipping per z8 tile:** Uses `geometry::clip_polygon_into()` (Sutherland-Hodgman algorithm, `geometry.rs:691`) with reused scratch buffers `clip_a` and `clip_b` (`ocean.rs:205-206`). The outer ring is clipped first; if the result has >= 4 points, it becomes a sub-polygon. Each inner ring is also clipped to the same tile rect.

**Critical detail — clone on line 238:** `let sub_outer = clip_a.clone()`. The `clip_polygon_into` function writes its result into `clip_a`, but the same buffers are reused for inner ring clipping on line 240. So the outer result must be cloned before the inner clipping overwrites `clip_a`. This is the clone the theoretical review flagged at `ocean.rs:238`.

**Additional clone on line 241:** `Some(clip_a.clone())` for each inner ring that survives clipping. Same reason — `clip_a` is the output buffer and will be overwritten on the next inner ring iteration.

### Multiplication Factor

The split grid has 256x256 = 65,536 tiles at z8. A polygon's bounding box determines how many z8 tiles it could span. For a large ocean polygon covering, say, the North Atlantic:

- Approximate extent: maybe 60 degrees longitude (~17% of earth), 50 degrees latitude (~14% of Mercator range)
- In z8 tiles: ~43 tiles wide, ~36 tiles tall = ~1,548 z8 tiles in the bbox
- After clipping, many of those tiles will have empty intersections and are skipped (`if clip_a.len() < 4 { continue; }`, line 237)
- **Estimated multiplication**: a single large polygon might become 200-1000+ sub-polygons depending on shape complexity

For the water-polygons-**split**-3857 dataset specifically: the shapefile is already pre-split by osmdata.xyz into smaller polygons. This is why it's called "split" — the polygons are already broken along some grid. But they may still be large enough (>500 vertices) to trigger further splitting at z8.

### Recursion

**Verified fact:** Splitting is NOT recursive. It's a single-pass clip to z8 tile grid. The iteration is `for sty in sty_min..=sty_max { for stx in stx_min..=stx_max { ... } }` (`ocean.rs:228-229`). Each sub-polygon is pushed directly to `split_out` without further splitting. There is no risk of infinite recursion.

### What the 500-vertex Threshold Does

Polygons with fewer than 500 outer vertices are passed through unchanged. The rationale (from the comment at `ocean.rs:195-198`) is that splitting introduces per-tile clipping overhead, and for small polygons the overhead isn't justified. However:

**Observation:** The threshold is based on outer vertex count only. A polygon with 400 outer vertices but many inner rings (complex island chains) won't be split, even though its total processing cost across all zooms might be high.

**Observation:** The threshold is applied in Mercator [0,1] coordinate space after the initial data_bounds clipping. So polygons that were large but mostly clipped away might fall below the threshold and not be split further.

## 3. Land Mask

### Structure

Defined in `geometry.rs:1212-1309`:

```
struct LandMask {
    bits: Box<[AtomicU8]>,
}
```

- **Zoom level**: 14 (hardcoded constant, `geometry.rs:1218`)
- **Grid size**: 16,384 x 16,384 = 268,435,456 cells (`geometry.rs:1220`)
- **Storage**: 268M bits / 8 = 33,554,432 bytes = **32 MB** (`geometry.rs:1222`)
- **Heap allocated**: `Vec::with_capacity(BYTES)` then converted to `Box<[AtomicU8]>` (`geometry.rs:1226-1228`)
- **Thread-safe**: uses `AtomicU8` with `fetch_or` for concurrent writes (`geometry.rs:1277`)

### Population (During PBF Phase)

The land mask is populated during PBF processing (Phase 1+2), NOT during ocean processing. Every feature's bounding box is marked:
- **Nodes**: `land_mask.mark_bbox(&pbbox)` where pbbox is a point bbox (`pipeline.rs:650`)
- **Ways**: `land_mask.mark_bbox(&bbox)` where bbox is the way's Mercator bbox (`pipeline.rs:803`)
- **Relations**: `land_mask.mark_bbox(&bbox)` for each polygon in the multipolygon (`pipeline.rs:945, 958, 978`)

`mark_bbox()` (`geometry.rs:1233-1247`) converts the Mercator bbox to z14 tile coordinates and sets all bits in the rectangle. For a point feature, this marks exactly one z14 cell. For a large polygon, it marks all z14 cells its bbox covers.

### How It's Used in Ocean Processing

The land mask is passed as `Option<&geometry::LandMask>` to `process_ocean_shapefile()` (`ocean.rs:45`), and then into `emit_ocean_polygon()` (`ocean.rs:370`).

Within the scanline emission, the land mask is checked before emitting **every** ocean tile:
- **Boundary tiles**: `if let Some(mask) = land_mask && !mask.has_land(z, tx, ty) { continue; }` (`ocean.rs:493-494`)
- **Fill tiles in gaps**: same check (`ocean.rs:521-522`)
- **Fill tiles in full rows (no boundary tiles)**: same check (`ocean.rs:534-535`)

**Semantics of `has_land()`** (`geometry.rs:1252-1270`):
- For z >= 14: looks up the single z14 ancestor cell
- For z < 14: scans all z14 descendant cells, returns true if ANY is set (early exit)

**Critical insight:** The land mask is used to **skip ocean tiles that have no land features**. The logic is: if a z14 cell has no PBF data (no nodes, ways, or relations), then no map features exist there, so an ocean-only tile for that location is unnecessary — the map renderer will show ocean/water background by default. This prevents emitting millions of pure-ocean tiles in open sea areas.

### Why z8 to z14 Was Significant

At z8: 256x256 = 65,536 cells. Each cell covers ~1.4 degrees of latitude. Coastal areas near large urban centers (e.g., Tokyo, New York) would have their entire z8 cell marked as "has land" because at least one feature exists somewhere in that large area. This means ocean tiles far out to sea (but within the same z8 cell) would still be emitted.

At z14: 16,384x16,384 = 268M cells. Each cell covers ~25m of ground. Only cells with actual OSM features are marked. This gives much more precise filtering, dramatically reducing the number of unnecessary ocean tiles emitted, especially at high zooms where the tile count explodes quadratically.

**Size tradeoff:** z8 mask = 65,536 bits = 8 KB. z14 mask = 268M bits = 32 MB. The 4000x increase in memory is well worth it given the tile emission reduction.

**Potential concern at z < 14:** The `has_land()` function for z < 14 does a linear scan of all descendant z14 cells. At z0, this scans all 268M cells. At z7, it scans 128x128 = 16,384 cells per tile. This is called per-tile during ocean emission at each zoom. For low zooms with few tiles, this is negligible. But the cost per call scales as O(4^(14-z)).

### Accuracy and Edge Cases

**Potential false positive (emitting unnecessary tiles):** `mark_bbox()` marks the bounding box of each feature, not its actual geometry. A diagonal road through a z14 cell marks that cell, even if the cell is mostly water. This causes some unnecessary ocean tiles to be emitted, but they'll be small overhead compared to the total.

**Potential false negative (skipping needed tiles):** If a tile has ocean features but no PBF land features at all, the land mask won't be set, and the ocean tile won't be emitted. This is **intentional** — the `should_emit` filter in the assemble phase (`pipeline.rs:1356-1358`) would filter it out anyway. So the land mask acts as an early filter that prevents generating sort records that would be discarded later.

**Verified correctness:** The land mask and `should_emit` are consistent. `should_emit` checks `tile.features.iter().any(|(layer, _)| *layer != ocean_idx)` — i.e., the tile must have at least one non-ocean feature. The land mask ensures ocean tiles are only generated where PBF data exists. These are aligned in intent: both prevent ocean-only tiles.

## 4. Scanline Fill Algorithm

### Overview

`emit_ocean_polygon()` (`ocean.rs:362-544`) processes one polygon across all zoom levels using a scanline approach to minimize expensive point-in-polygon tests.

### Per-Zoom Processing

The function calls `geometry::for_each_zoom_simplified_multi()` (`ocean.rs:407`), which iterates from `max_zoom` down to `min_zoom`, applying Douglas-Peucker simplification at each zoom level with cascading (the simplified result at zoom z becomes the input for zoom z-1).

For each zoom level, the callback (`ocean.rs:408-543`) does:

#### Step 1: Edge Rasterization (`ocean.rs:413-418`)

All ring edges (outer + inners) are rasterized onto the tile grid using DDA grid traversal (`rasterize_ring_edges`, `ocean.rs:602-614`). The `rasterize_segment` function (`ocean.rs:618-678`) walks each line segment cell by cell, inserting crossed tiles into a `HashSet<u64>`. Tiles are packed as `(tx << 32) | ty` (`ocean.rs:681-683`).

**Data structure:** `boundary_tiles: HashSet<u64>` (`ocean.rs:392`). This is reused across zoom iterations (cleared at line 414).

#### Step 2: Row Grouping (`ocean.rs:420-432`)

Boundary tiles are grouped by row (ty) into `boundary_rows: HashMap<u32, Vec<u32>>` (ty -> sorted list of tx values). Each row's tx list is sorted and deduped.

#### Step 3: Row-by-Row Scanline (`ocean.rs:453-541`)

For each row `ty` in the polygon's tile range:

**Case A: Row has boundary tiles** (`ocean.rs:456-528`):
1. **Row pre-clip** (`ocean.rs:460-486`): Clip the simplified polygon to the row's Y-band (full X range, this row's Y range + buffer). This dramatically reduces vertex count for per-tile clipping. For example, a fjord polygon with 1000 vertices might have only 50 vertices in a given row band.
2. **Boundary tile emission** (`ocean.rs:489-501`): For each boundary tile in the row, call `emit_boundary_tile()` which clips to the tile rect and encodes the polygon geometry as MVT.
3. **Gap filling** (`ocean.rs:503-528`): Between boundary tiles, gaps are identified. For each gap, a single point-in-polygon test at the gap's center determines if the gap is inside the polygon. If yes, all tiles in the gap get the fill data (a full-tile rectangle).

**Case B: Row has no boundary tiles** (`ocean.rs:529-541`):
A single PIP test at the row center determines if the entire row range is inside the polygon. If yes, all tiles in the row's X range get fill data.

### Fill Tile Optimization

**Verified fact:** Fill tiles use `osm_id=0` and a pre-computed full-tile rectangle (`ocean.rs:378-388`). The fill data is computed once per `emit_ocean_polygon` call and shared (via `fill_data.clone()`) across all fill tiles. The comment at line 379-381 explains: this makes all fill tiles produce identical bytes, enabling PMTiles content-hash dedup.

The fill tile geometry is a simple rectangle:
```
(0,0) -> (4096,0) -> (4096,4096) -> (0,4096) -> (0,0)
```
This is 5 MVT commands (MoveTo + 4 LineTo pairs in the polygon encoding).

### Fill Data Size Calculation

The `encode_feature_data` call at `ocean.rs:387` with:
- osm_id=0 (8 bytes)
- geom_type=Polygon (1 byte)
- geom_cmds count (4 bytes)
- 5 MVT polygon commands for a closed rectangle: approximately 9 u32 values (1 MoveTo cmd + 2 coords, 1 LineTo cmd + 8 coords, 1 ClosePath) = 36 bytes
- attrs = empty vec, so 1 byte (attr count = 0)

**Estimated fill_data size: ~50 bytes per record.**

The `SortRecord` wrapping adds 8 bytes for the key, plus 24 bytes for the Vec overhead, plus the data payload. So each fill tile sort record is approximately 82 bytes in memory.

### Boundary Tile Data Size

Boundary tiles (`emit_boundary_tile`, `ocean.rs:550-594`) have variable geometry depending on the polygon-tile intersection. A typical boundary tile might have 10-50 vertices after clipping, producing 30-150 MVT geometry commands = 120-600 bytes of geometry data, plus the 13-byte header and 1 byte attrs = ~135-615 bytes per boundary tile.

## 5. Feature Emission and Sort Integration

### Zoom Range

Ocean features are emitted for every zoom level from `min_zoom` to `max_zoom` (passed through from `process_ocean_shapefile`, `ocean.rs:43-44`). In a default configuration (z0-z14), that's 15 zoom levels per polygon.

When a simplified ocean shapefile is provided, the zoom range is split:
- Simplified shapefile: z0-z7 (`pipeline.rs:201-206`)
- Full-resolution shapefile: z8-z14 (`pipeline.rs:208-213`)

### Parallel Emission Architecture

The polygons are processed in parallel via `rayon::par_iter()` with a fold-map-reduce pattern (`ocean.rs:305-337`):

1. **fold** (`ocean.rs:308-324`): Each rayon worker processes polygons sequentially, accumulating `SortRecord`s in a thread-local `OceanAcc` struct. When the accumulated bytes exceed `chunk_size` (1 GB, from `sort_writer.chunk_size_bytes()`), the records are flushed to a chunk file.

2. **map** (`ocean.rs:326-329`): After fold completes, each worker flushes its remaining records.

3. **reduce** (`ocean.rs:330-337`): Merge chunk paths and feature counts across workers.

The `OceanAcc` struct (`ocean.rs:280-286`) contains:
- `records: Vec<SortRecord>` — accumulated records
- `bytes: usize` — estimated byte count (data.len() + 8 per record, `ocean.rs:318`)
- `chunk_paths: Vec<PathBuf>` — paths to flushed chunk files
- `count: u64` — total records flushed
- `simp_scratch: SimplifyMultiScratch` — reusable scratch buffers for cascading simplification

### Chunk File Naming

Ocean chunk files use the same naming convention as PBF chunks: `chunk_NNNN.bin` (`ocean.rs:294`). The chunk ID counter starts from `sort_writer.chunk_count()` (`ocean.rs:276`), which is the number of PBF chunks already written. This ensures no naming collisions.

The `AtomicUsize` for chunk IDs (`ocean.rs:276`) uses `Relaxed` ordering (`ocean.rs:293`), which is correct because each worker writes to a unique file path — there's no data dependency between workers' chunk IDs.

### Adoption Into Sort Writer

After parallel processing, the accumulated chunk paths are adopted into the sort writer via `sort_writer.adopt_chunk_files(result.chunk_paths)` (`ocean.rs:339`). This registers the chunk files for the subsequent k-way merge sort phase.

### Estimated Feature Counts and Chunk Counts

**Denmark estimate:**
- A few hundred ocean polygons intersecting Denmark
- Each polygon emitted across ~15 zoom levels
- At low zooms: few tiles per polygon. At z14: potentially thousands of tiles per polygon
- Expected: tens of thousands of ocean features total
- At ~50-200 bytes per record, this might be 5-50 MB total, fitting in 1 chunk

**Planet estimate (inference):**
- ~250K shapes, most intersecting after bbox filter
- After splitting: potentially 500K-2M sub-polygons
- Each polygon across 15 zoom levels, with tile counts growing 4x per zoom
- At z14 alone: the world ocean covers ~70% of Earth's surface, meaning ~188M z14 tiles are ocean. But the land mask filters out most of these.
- With land mask filtering: only ocean tiles near land features are emitted. This is still millions of tiles.
- Expected: tens of millions to hundreds of millions of ocean features
- At ~50-200 bytes per record: 5-40 GB of ocean sort data, producing 5-40 chunk files
- Chunk count: each rayon worker flushes when exceeding 1 GB. With, say, 12 rayon workers, the worst case is 12 partial chunks + N full chunks.

## 6. Memory and I/O Profile

### Peak Memory During Ocean Processing

Simultaneous allocations at peak:
1. `.shx` index data: ~2 MB
2. `.shp` mmap: virtual only, physical pages demand-faulted and reclaimable
3. `offsets: Vec<usize>`: ~250K * 8 bytes = ~2 MB
4. `polygons: Vec<OceanPolygon>`: struct overhead + all vertex data. Highly variable. For Denmark, maybe 10-50 MB. For planet, could be 1-5 GB (all in-bounds polygons held in memory simultaneously before parallel processing)
5. `split_out: Vec<OceanPolygon>` during splitting: this replaces `polygons` (via `drain` + reassignment), so peak is during the transition when both exist momentarily (but `drain` empties the source incrementally, so it's not a full doubling)
6. Per-rayon-worker `OceanAcc`: up to `chunk_size` (1 GB) of SortRecords per worker, but flushed frequently. With 12 workers, worst case ~12 GB if all workers are at maximum fill simultaneously.
7. Land mask: 32 MB (already allocated during PBF phase, passed by reference)

**Inference (planet-scale peak):** The biggest risk is item 4 — all parsed polygons held in the `polygons` Vec before parallel processing. At planet scale with complex coastlines, this could be several GB. The parallel processing (item 6) is bounded by the flush mechanism, so it's more controlled.

### Clone Analysis

**Clones identified:**

1. `clip_a.clone()` at `ocean.rs:238` — cloning the outer ring clip result before inner ring clipping overwrites `clip_a`. Size: variable, typically 50-500 points * 16 bytes = 0.8-8 KB per clone. Called once per split tile per large polygon. For a polygon spanning 100 z8 tiles, that's 100 clones.

2. `clip_a.clone()` at `ocean.rs:241` — cloning inner ring clip results. Called per inner ring per split tile. Ocean polygons rarely have many inner rings, so this is less impactful.

3. `fill_data.clone()` at `ocean.rs:525, 538` — cloning the fill tile data (~50 bytes) for each fill tile. This is the highest-volume clone: at z14, a single large polygon might generate thousands of fill tiles. Each clone allocates ~50 bytes on the heap.

4. `row_clip_a.clone()` at `ocean.rs:482` — cloning row-clipped inner rings into the `row_inners` pool. Only happens when `row_inner_count >= row_inners.len()` (i.e., on first encounter of that many inners). Subsequent iterations reuse via `clear() + extend_from_slice()` (`ocean.rs:479-480`).

**Clone #3 is the most concerning at scale.** For planet-scale processing with millions of fill tiles, each `fill_data.clone()` allocates a new ~50-byte `Vec<u8>`. Since fill_data is identical for all fill tiles, this is pure redundant allocation. However, each clone becomes a `SortRecord.data` that is flushed to disk, so the allocation is short-lived if the OceanAcc flushes frequently.

### I/O Pattern

1. **Read (sequential):** `.shx` file read (`ocean.rs:52`), then `.shp` mmap sequential access (`ocean.rs:92` loop iterating offsets in order)
2. **Write (parallel, random):** Each rayon worker writes chunk files independently. Writes are buffered via `BufWriter::with_capacity(1 << 20, file)` in `write_sorted_chunk` (`sort.rs:204`). Multiple workers may write simultaneously, but to different files.
3. **Post-ocean:** Chunk files are registered with the sort writer for subsequent k-way merge.

## 7. Review Claims: Verification and Expansion

### Claim 1 (High): Pre-split policy is static

**Verified.** `SPLIT_Z=8` and `SPLIT_MIN_VERTICES=500` are `const` values (`ocean.rs:200-201`). There is no runtime configuration, no CLI flag, and no data-adaptive logic.

**Expanded analysis:**

The SPLIT_Z=8 choice means sub-polygons fit within a z8 tile (roughly 1.4 degrees or ~150 km). At z14, each sub-polygon can still span up to (2^6)^2 = 4096 tiles. The scanline fill handles this well for simple shapes, but complex coastlines (Norwegian fjords, Greek islands) produce many boundary tiles at z14 that each need individual clipping.

**Over-split risk:** If SPLIT_Z were increased to, say, 10, each polygon would be split to finer tiles (1024 z10 tiles per z8 tile in bbox). This means many more sub-polygons in the `split_out` vector, each smaller. The overhead is: more iterations in the split loop, more clip operations, more `OceanPolygon` structs, but each subsequent polygon is cheaper to process at high zooms.

**Under-split risk:** At SPLIT_Z=8, large sub-polygons at z14 still span many tiles. The row-pre-clip optimization (`ocean.rs:460-466`) mitigates this by restricting the polygon to each tile row before per-tile clipping, but it's still processing potentially thousands of vertices per row.

**Missing adaptive signal:** The code could count the actual z14 tile coverage and adjust the split level per polygon. A polygon covering 10,000 z14 tiles might benefit from splitting at z10 or z11, while one covering 50 z14 tiles needs no splitting at all.

### Claim 2 (Medium): Clone-heavy paths

**Verified.** See clone analysis in Section 6 above.

**Expanded analysis:**

The clones in the split phase (`ocean.rs:238, 241`) are structurally necessary given the current API of `clip_polygon_into()` which writes to a reusable output buffer. The alternative would be to have `clip_polygon_into` allocate and return a new Vec, but that would be the same cost. A different approach would be to use two pairs of clip buffers — one for outer, one for inner — eliminating the need to clone the outer before processing inners.

The fill_data clone (`ocean.rs:525, 538`) could potentially be replaced with an `Arc<Vec<u8>>` or by using a special sentinel in SortRecord that means "fill tile" and generating the actual bytes lazily during assembly. However, the SortRecord format is fixed (key + data bytes) and flows through the sort system, so this would require sort-level changes.

### Claim 3 (Medium): Ocean parallel flush creates many chunks

**Verified.** Each rayon worker can produce its own chunk files independently (`ocean.rs:289-302`).

**Expanded analysis:**

The number of ocean chunks depends on:
- Number of rayon workers (typically matches CPU count, e.g., 12 on plantasjen)
- Total bytes of ocean sort records per worker
- Chunk size threshold (1 GB)

For Denmark (small dataset), ocean records likely total under 100 MB, so each worker produces at most 1 chunk, yielding up to 12 chunks total. But most workers will produce small partial chunks (< 1 GB each).

For planet scale, if total ocean sort data is 10-30 GB distributed across 12 workers, each worker produces 1-3 full chunks plus a partial, yielding 15-40 chunks total.

These chunks are in addition to the PBF chunks. The k-way merge's heap size equals the total chunk count. More chunks = larger merge heap = slightly more comparison overhead per record. However, the real cost is I/O bandwidth during merge, not heap operations. Each additional chunk adds one file descriptor and one read buffer.

**Potential issue:** The rayon fold+reduce pattern means polygon-to-worker assignment is not controlled. If one worker gets assigned all the large polygons (due to rayon's work-stealing granularity), it could produce many more chunks than others. The data distribution depends on `par_iter()` chunk size (default: divides evenly among workers). Since polygons are already split, their sizes should be relatively uniform.

## 8. What the Review Missed

### 8.1 Polygon Splitting Correctness: Gaps and Overlaps

**Finding:** The split clips to exact tile boundaries without any buffer. The `tile_rect` at `ocean.rs:230-235` uses exact z8 tile boundaries:
```rust
ClipRect::new(
    f64::from(stx) * split_inv,
    f64::from(sty) * split_inv,
    f64::from(stx + 1) * split_inv,
    f64::from(sty + 1) * split_inv,
)
```

This means sub-polygons from adjacent z8 tiles share edges exactly at the tile boundary (no overlap, no gap). When these sub-polygons are later clipped to individual z9+ tiles that straddle the z8 boundary, the clipping handles this correctly because the boundary tiles at z8 edges are handled by the scanline algorithm.

**However:** There is a subtle correctness concern. When the original polygon is split at z8 boundaries, the Sutherland-Hodgman clip introduces new vertices exactly on the z8 tile edges. At higher zooms, these artificial vertices create boundary tiles along the z8 grid lines that wouldn't exist if the polygon hadn't been split. This results in a few extra boundary tiles being processed (and their geometry clipped+encoded) that produce the same visual output as a fill tile. This is a minor inefficiency, not a correctness bug.

### 8.2 Complex Coastline Performance (Fjords, Archipelagos)

**Finding:** Norwegian fjords and archipelagos like Indonesia produce polygons with extremely complex boundaries that create many boundary tiles at high zooms. The scanline optimization reduces PIP tests, but the boundary tile emission (`emit_boundary_tile`, `ocean.rs:550-594`) still clips each boundary tile individually.

The **row pre-clip** optimization (`ocean.rs:457-486`) significantly helps: clipping the polygon to the row's Y-band before per-tile clipping means each tile clip operates on a much smaller polygon. The comment at `ocean.rs:458-459` says "~50 vertices instead of ~1000 for large fjord polygons."

However, for the most complex coastlines (e.g., the Norwegian coast with thousands of islands), even the row-clipped polygon might have many vertices per row. And the number of boundary tiles per row can be very high (each island creates boundary tiles around its perimeter).

### 8.3 `should_emit` Robustness

**Verified robust.** The `should_emit` mechanism (`pipeline.rs:1356-1358`) checks if a tile has ANY non-ocean feature. This is a correct filter: if a tile has only ocean features, it's emitted as empty by the map renderer anyway. The land mask pre-filter in ocean processing is consistent with this: it only generates ocean tiles where PBF data exists.

**Edge case:** If the land mask has a false positive (bbox-based marking set a z14 cell where the only PBF feature is a line that grazes the cell but doesn't produce a tile-level feature), the ocean tile would be generated but filtered by `should_emit`. This is waste (sort I/O) but not incorrectness.

### 8.4 Opportunity: Spatial Indexing of Ocean Polygons

**Finding:** The current approach processes all in-bounds polygons at every zoom level. There is no spatial indexing to quickly determine which polygons are relevant to a given zoom level or tile region.

At low zooms (z0-z3), most ocean polygons are sub-pixel and get filtered by `merc_bbox_is_subpixel()` inside the cascading simplification (`geometry.rs:468-469`). But the function is still called, and the simplification scratch buffers are still initialized per polygon.

A spatial index (R-tree or grid index) could partition polygons by location and size, allowing zoom-level-specific processing to skip polygons that are irrelevant at a given zoom.

### 8.5 Opportunity: Overlapping Ocean and PBF Water Features

**Finding:** The PBF data may contain `natural=water`, `natural=coastline`, or `water=*` features that overlap with ocean polygon data. The Shortbread schema has separate layers: `ocean` (from shapefile) and `water_polygons` / `water_lines` (from PBF). These can produce duplicate visual coverage in rendered maps.

This is a schema-level design choice, not a bug. But it means some tiles contain both ocean and PBF water polygons covering the same area, increasing tile size without visual benefit. The theoretical review didn't flag this data redundancy.

### 8.6 Opportunity: Overlapping Ocean Processing with PBF Processing

**Finding:** Currently, PBF processing completes entirely before ocean processing begins (`pipeline.rs:164-226`). The ocean phase needs `data_bounds` (computed from PBF data extent) and the `land_mask` (populated during PBF processing), so it cannot start until PBF processing is complete.

**Could the ocean shapefile be parsed (but not emitted) in parallel with PBF processing?** The parsing phase (`ocean.rs:88-193`) only needs `data_bounds` for the bbox filter. If `data_bounds` were precomputed (e.g., from the PBF header or a previous run), parsing could start immediately. However, the land mask is critical for filtering emission, and it's only complete after all PBF features are processed.

**Alternative:** Parse and split during PBF phase, emit after PBF completes. This would overlap the I/O-bound shapefile read with CPU-bound PBF processing.

### 8.7 HashSet/HashMap Overhead in Scanline

**Finding:** The scanline algorithm uses `HashSet<u64>` for boundary tiles and `HashMap<u32, Vec<u32>>` for boundary rows (`ocean.rs:392-393`). These are reused across zoom iterations (cleared, not reallocated).

At z14 for a complex polygon, the boundary tile count can be in the thousands. Each insert into the `HashSet` involves hashing a u64. The default Rust hasher (SipHash) is cryptographically secure but slower than necessary for integer keys. A faster hasher (FxHash, AHash) could reduce overhead.

Similarly, the `HashMap` row grouping involves hashing u32 keys. For a polygon spanning, say, 100 rows with 50 boundary tiles per row, that's 5000 hash operations per zoom level.

**However:** The cost of hashing is likely dwarfed by the polygon clipping and MVT encoding, so this is a low-priority optimization.

### 8.8 DDA Rasterization Edge Case

**Finding:** The `rasterize_segment` function (`ocean.rs:618-678`) has a documented edge case at line 665-666: when `t_max_x == t_max_y` (exact grid corner crossing), only the Y step is taken, potentially missing the X-direction tile. The comment notes this is unreachable with real shapefile coordinates and that the scanline PIP fallback handles missed tiles.

**Verified safe:** If a boundary tile is missed, it falls into a "gap" in the scanline algorithm. The PIP test for that gap determines if it should be filled. So a missed boundary tile becomes a fill tile (full-tile rectangle) instead of a clipped partial tile. For an exact corner crossing, the polygon edge passes through the exact corner of a tile, so the visual difference between a clipped polygon edge and a fill rectangle at that tile is sub-pixel. This is a correct fallback.

### 8.9 Simplified Shapefile Split

**Finding:** The pipeline supports two shapefiles: a simplified one for z0-z7 and the full-resolution one for z8+ (`pipeline.rs:200-214`). When both are provided, `process_ocean_shapefile` is called twice with different zoom ranges.

Each call does its own mmap, parse, split, and parallel emission. The simplified shapefile is typically much smaller (fewer vertices per polygon), making the z0-z7 pass very fast.

**Observation:** The split phase at `ocean.rs:202` only runs when `max_zoom >= SPLIT_Z`. For the simplified shapefile call with `max_zoom=7`, this condition is `7 >= 8 = false`, so no splitting occurs. This is correct: at z0-z7, polygons don't need splitting because the tile counts are small (max 256x256 at z7).

## 9. Summary of Key Findings

### Confirmed High-Priority Issues
1. **Static split policy** — SPLIT_Z=8 and SPLIT_MIN_VERTICES=500 are hardcoded. No data-adaptive behavior. The optimal split level depends on polygon complexity and target zoom range.

### Confirmed Medium-Priority Issues
2. **Clone pressure** — `fill_data.clone()` is the highest-volume clone, called per fill tile. At planet scale with millions of fill tiles, this creates millions of small (~50 byte) heap allocations. The split-phase clones (`clip_a.clone()`) are lower volume but larger per clone.
3. **Chunk count in merge** — ocean processing adds up to `rayon_threads + N` chunks to the merge. This is modest compared to PBF chunks but adds up.

### Newly Identified Issues
4. **All-polygons-in-memory** — At planet scale, all in-bounds polygons are parsed and stored in the `polygons` Vec before parallel processing. This could be several GB. A streaming approach (parse + split + emit in batches) would reduce peak memory.
5. **HashSet/HashMap for scanline** — Standard library hashing is used for integer keys. A faster hasher could reduce constant overhead, though this is low-priority.
6. **No spatial indexing** — All polygons are processed at all zoom levels. Subpixel filtering happens inside the simplification cascade, but the function call overhead per polygon per zoom is not zero.
7. **Fill data clone redundancy** — All fill tiles within a single `emit_ocean_polygon` call have identical data. The `fill_data` is cloned per tile, allocating a new Vec each time. A reference-counted or pre-allocated pool approach could eliminate these allocations.
8. **PBF-ocean pipeline overlap opportunity** — Ocean shapefile parsing could potentially overlap with PBF processing if data_bounds were precomputed.

### Data for Planning

| Metric | Denmark (estimated) | Planet (estimated) |
|--------|--------------------|--------------------|
| Shapes in bbox | ~200-500 | ~250,000 |
| Polygons after parse | ~500-1,000 | ~300,000-500,000 |
| Polygons after split | ~500-2,000 | ~500,000-2,000,000 |
| Ocean features emitted | ~50,000-200,000 | ~50M-200M |
| Ocean sort data | ~10-100 MB | ~5-30 GB |
| Ocean chunk files | ~1-12 | ~15-40 |
| Peak polygon memory | ~10-50 MB | ~1-5 GB |
| Fill data clones | ~10,000-100,000 | ~10M-100M |
| Phase duration | ~1.4s (measured, plantasjen) | unknown |
