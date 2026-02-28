# Box 4 Deep Investigation: Geometry and Topology Engine

Date: 2026-02-28
Scope: `src/geometry.rs`, `src/multipolygon.rs`, geometry callsites in `src/pipeline.rs` and `src/ocean.rs`
Method: Static code analysis with line references. No benchmark runs.

---

## 1. Mercator Projection

### 1.1 LUT Structure and Mechanics

The hot-path projection function is `project_e7` (`src/geometry.rs:169`). It takes e7 fixed-point integers (lat/lon in units of 1e-7 degrees, matching the PBF wire format) and returns a `Point` in Mercator [0,1] space.

**Longitude (X):** Direct linear transform, no LUT needed (`src/geometry.rs:171`):
```
x = (lon_e7 * 1e-7 + 180.0) / 360.0
```
This is 2 FP ops (mul, add, div). Exact.

**Latitude (Y):** Uses an 18-bit lookup table with linear interpolation (`src/geometry.rs:130-185`):
- LUT size: `2^18 + 1 = 262,145` entries of `f64` = **2,097,160 bytes (~2 MB)** (`src/geometry.rs:134`)
- Latitude range: -85.051129 to +85.051129 degrees (Web Mercator limits) (`src/geometry.rs:135-136`)
- Each entry stores the exact Mercator Y for that latitude via `0.5 - (tan(lat) + sec(lat)).ln() / (2*PI)` (`src/geometry.rs:147`)
- Interpolation: compute fractional index into LUT, linear interpolation between two entries (`src/geometry.rs:174-183`)
- Initialization: `OnceLock` lazy init on first call (`src/geometry.rs:139, 170`)

**Precision analysis:**
- The LUT maps the full latitude range (-85.05 to +85.05, ~170.1 degrees) into 262,144 intervals
- Each interval spans ~0.00065 degrees = ~72 meters at equator
- Comment states ~0.00032 degree error = 0.03 pixels at z14 (`src/geometry.rs:131-132`)
- Linear interpolation within each interval; Mercator Y is smooth and nearly linear at small scales, so interpolation error is dominated by the second derivative, which is negligible at this resolution
- At z14, one pixel = ~9.5 meters, so 0.03 pixel error is ~0.3 meters. Imperceptible.

**Performance claim:** "~3-4 cycles" vs "~250-400 cycles" for transcendentals (`src/geometry.rs:166`).
- VERIFIED as plausible: the lookup is 2 memory reads from a 2 MB table (fits L1/L2 cache), 1 FP multiply, 1 FP add. With warm cache, this is ~3-5 cycles on modern OOO CPUs.
- The exact `project()` function (`src/geometry.rs:157`, `#[cfg(test)]` only) uses `tan()`, `cos()` (reciprocal), and `ln()` -- three transcendentals at ~80-150 cycles each on Zen 3.

### 1.2 Projection Callsites in the Pipeline

Projection happens in two places:

1. **Node processing** (`src/pipeline.rs:648`): `project_e7(lat_e7, lon_e7)` -- once per tagged node. Untagged nodes are stored in the node store without projection.

2. **Way processing** (`src/pipeline.rs:767-769`): `coords_e7.iter().map(|&(lat, lon)| project_e7(lat, lon)).collect()` -- once per coordinate in the way. This is the hot path: a typical way has 5-20 nodes.

3. **Relation member ways** (`src/pipeline.rs:858-861`): `coords_e7.iter().map(|&(lat, lon)| project_e7(lat, lon)).collect()` -- per member way coordinate, during `prepare_relation`. This is sequential (in the PBF callback), not parallel.

4. **Ocean polygons** (`src/ocean.rs` via `geometry::from_epsg3857`): Ocean shapefiles are in EPSG:3857 (meters), not e7. Uses `from_epsg3857()` (`src/geometry.rs:189-195`) which is a simple linear transform (no LUT needed).

**Redundancy check:** Projection is applied ONCE per coordinate. The `merc` Vec is then reused across all zoom levels, tile iterations, and match iterations for that feature. No redundant projection detected.

**Inference:** Projection is not a bottleneck. The LUT makes it extremely cheap (~3-5 cycles per coordinate), and it's called exactly once per coordinate.

### 1.3 Alternative Projection: `from_epsg3857`

Used only for ocean shapefile processing (`src/geometry.rs:188-195`). Simple linear transform:
```
x = (meters_x + half_circumference) / circumference
y = (half_circumference - meters_y) / circumference
```
Two FP ops each. Not a concern.

---

## 2. Clipping

Two clipping algorithms are implemented:

### 2.1 Cohen-Sutherland Line Clipping

Used for linestrings (`src/geometry.rs:554-587`).

**Algorithm:**
1. Pre-test: AND all vertex outcodes (`src/geometry.rs:564-573`). If all vertices share a common outside bit, the entire line is rejected in O(n). Early-exit on first vertex with all-inside code.
2. Per-segment: `clip_segment()` (`src/geometry.rs:635-658`) clips each segment of the polyline against the rectangle using the standard Cohen-Sutherland iterative algorithm (loop until both endpoints inside or trivially rejected).
3. Results collected via `clip_segment_and_collect()` (`src/geometry.rs:590-617`), which tracks entry/exit points to split the line into multiple sub-linestrings when it re-enters the rectangle.

**Complexity:** O(n) per clip where n = vertex count. Each segment requires at most 4 intersection calculations (one per edge). The pre-test can reject in O(n) with a single comparison per vertex.

**Return type:** `SmallVec<[Vec<Point>; 1]>` (`src/geometry.rs:559`) -- inline for the common case of 0 or 1 output segments, heap-allocates for multi-segment results.

**Allocation concern:** Each output segment is a `Vec<Point>` allocation. In the common case (line crosses one tile edge), this is one Vec allocation per clip. The `current` buffer (`src/geometry.rs:575`) is taken via `std::mem::take` and pushed to result, avoiding copy.

**FINDING: `clip_linestring` allocates a new `Vec<Point>` per output segment per tile.** These are short-lived (consumed immediately in `emit_line_feature`), but at planet scale with millions of line features across multiple tiles, this is significant allocator churn. The function is NOT buffer-reuse aware -- there is no `clip_linestring_into` variant.

### 2.2 Sutherland-Hodgman Polygon Clipping

Used for polygon rings (`src/geometry.rs:685-798`).

**Algorithm:** Classic Sutherland-Hodgman with 4 clip edges (left, right, bottom, top):
1. Pre-test: AND all vertex outcodes (`src/geometry.rs:706-715`). Same early-reject as line clipping.
2. Copy input ring into `buf_a` (`src/geometry.rs:716`)
3. Clip against each of 4 edges sequentially (`src/geometry.rs:717-727`):
   - `clip_polygon_edge_into()` processes all edges of the polygon against one clip edge
   - Swap `buf_a`/`buf_b` after each edge pass
   - 4 swaps = even = result in `buf_a`

**Complexity:** O(4n) = O(n) per clip. Each vertex is processed against 4 edges, with potential intersection point generation. The output polygon can have at most n + 4 vertices (one new vertex per clip edge crossing).

**Buffer management:** The `_into` variant (`src/geometry.rs:691-728`) takes caller-owned `buf_a` and `buf_b`, enabling full buffer reuse. This is the variant used in all hot paths:
- `emit_polygon_feature` hoists `clip_a`/`clip_b` outside zoom loop (`src/pipeline.rs:1153-1154`)
- `emit_multipolygon_feature` same (`src/pipeline.rs:1217-1218`)
- `emit_ocean_polygon` same (`src/ocean.rs:397-398`)

**VERIFIED:** Polygon clipping has excellent buffer reuse. No per-call allocation in the hot path.

**Convenience `clip_polygon()`** (`src/geometry.rs:731-736`) allocates its own buffers. Used in:
- Ocean pre-split at z8 (`src/ocean.rs:236-241`) -- not hot path, runs once per polygon

### 2.3 Clip Rect Construction

`ClipRect::for_tile()` (`src/geometry.rs:544-552`) constructs a clip rectangle with buffer:
```
buffer = BUFFER_FRACTION / z_scale = (8/4096) / 2^z
```
The 8-pixel buffer (`src/geometry.rs:27`) ensures features extend slightly beyond tile boundaries for seamless rendering.

### 2.4 Missing: `clip_linestring_into` Buffer-Reuse Variant

**FINDING:** Unlike `clip_polygon_into`, there is no buffer-reuse variant for line clipping. `clip_linestring()` always allocates a new `SmallVec` and new `Vec<Point>` for each output segment.

In `emit_line_feature` (`src/pipeline.rs:1113`), the clipped segments are iterated and immediately consumed:
```rust
let clipped = geometry::clip_linestring(simplified, &clip);
for segment in &clipped {
    // convert to tile coords, encode, push record
}
```

Each call allocates:
- 1 `SmallVec<[Vec<Point>; 1]>` (inline for 1 segment)
- N `Vec<Point>` where N = number of output segments (typically 1)

This is called per-tile per-zoom-level per-line-feature. For a line crossing 5 tiles at 15 zoom levels, that's 75 `Vec<Point>` allocations just for clipping.

---

## 3. Simplification (Douglas-Peucker)

### 3.1 Implementation

`simplify_into()` (`src/geometry.rs:242-264`) is the hot-path variant.

**Algorithm:** Classic recursive Douglas-Peucker:
1. Mark first and last points as kept (`src/geometry.rs:255-256`)
2. `dp_recurse()` (`src/geometry.rs:277-292`) finds the farthest point from the line between endpoints
3. If farthest distance > tolerance, mark that point and recurse on both halves
4. Collect kept points into output

**Complexity:** O(n log n) average, O(n^2) worst case (pathological zigzag geometries where every recursion splits at a point adjacent to one endpoint).

**Stack depth:** Recursive implementation. Stack depth = O(log n) average, O(n) worst case. For a 10K vertex geometry, worst case is 10K stack frames at ~64-128 bytes each = ~1 MB stack. This is within typical thread stack limits (8 MB default on Linux, rayon inherits this).

**FINDING: The recursion is genuine O(n^2) worst case.** The `find_farthest()` function (`src/geometry.rs:302-320`) scans all points between start and end indices linearly. Each recursion splits the range but the total work across all recursion levels is O(n) on average (like quicksort), but can degrade to O(n^2) if the farthest point is always near one end.

The code comment at `src/geometry.rs:297-301` acknowledges this and states early termination was investigated and rejected.

### 3.2 Buffer Reuse

`simplify_into()` takes two caller-owned buffers:
- `keep_buf: &mut Vec<bool>` -- flag array sized to input
- `output: &mut Vec<Point>` -- result buffer

Both are reused across zoom levels in the cascading simplification loop.

### 3.3 Tolerance Calculation

`simplify_tolerance()` (`src/geometry.rs:224-227`):
```
tolerance = SIMPLIFY_PIXELS / (256 * 2^z)
```
Where `SIMPLIFY_PIXELS = 1.0` (`src/geometry.rs:24`).

At each zoom level:
| Zoom | Tolerance (Mercator units) | Approx meters (equator) |
|------|---------------------------|------------------------|
| 0    | 1/256 = 0.00391           | 157 km                 |
| 7    | 1/32768 = 0.0000305       | 1.2 km                 |
| 10   | 1/262144 = 0.00000381     | 153 m                  |
| 13   | 1/2097152 = 4.77e-7       | 19 m                   |
| 14   | N/A (no simplification)   | N/A                    |

**At z14 and above, simplification is skipped entirely** (`src/geometry.rs:369`: `if z < 14`).

### 3.4 Cascading Simplification

`for_each_zoom_simplified()` (`src/geometry.rs:354-393`) implements cascading DP:
- Iterates from z_hi down to z_lo (finest to coarsest)
- Each zoom's simplification uses the previous zoom's output as input ("cascade")
- This is correct because DP is hierarchical: the result at coarser tolerance is always a subset of the result at finer tolerance

**Three early-exit optimizations:**

1. **Subpixel bbox check** (`src/geometry.rs:373`): If the cascade's bounding box diagonal is < 1 pixel at this zoom, the feature is invisible at this and all coarser zooms. O(1) check vs O(n log n) DP.

2. **Vertex count check** ("Option E", `src/geometry.rs:378`): If cascade already has <= min_points vertices, DP can't reduce further.

3. **Convergence check** ("Option D", `src/geometry.rs:382`): If the max deviation from the last DP run is below this zoom's tolerance squared, the cascade has already converged. `simplify_into` returns `max_dev_sq` for exactly this purpose (`src/geometry.rs:242, 257`).

**FINDING: `for_each_zoom_simplified` allocates `cascade = merc.to_vec()` on every call** (`src/geometry.rs:363`). This copies the entire geometry. For a way with 100 vertices, that's 1.6 KB per call. Called once per feature per match, so for Denmark's 16M features this is potentially millions of 1.6 KB allocations. The subsequent `keep_buf` and `simp_buf` are empty Vecs that grow lazily and are reused across zoom levels within the same call.

### 3.5 Multi-polygon Variant

`for_each_zoom_simplified_multi()` (`src/geometry.rs:424-499`) handles outer ring + inner holes:
- Takes a `SimplifyMultiScratch` struct (`src/geometry.rs:398-416`) with pre-allocated buffers
- Inner rings are simplified at each zoom level and dropped when they fall below 4 vertices
- Per-inner convergence tracking (`inner_max_dev_sq`, `src/geometry.rs:461`) skips DP on converged inner rings

**Buffer reuse pattern:** `SimplifyMultiScratch` is hoisted outside tight loops in callers:
- `emit_multipolygon_feature` takes it as parameter (`src/pipeline.rs:1212`)
- `process_prepared_relation` creates one per relation (`src/pipeline.rs:929`)
- `emit_ocean_polygon` takes it from the per-rayon-thread accumulator (`src/ocean.rs:315`)

**FINDING: For multipolygon emission in relations, `SimplifyMultiScratch::new()` is allocated per relation** (`src/pipeline.rs:929`), not per rayon thread. This means for a relation with 200 polygons, the scratch is reused across those 200 `emit_multipolygon_feature` calls. But across relations, a new scratch is created. Since relation processing is batched with `par_iter()`, each rayon task processes one relation and creates one scratch -- reasonable.

### 3.6 Simplification Order vs Clipping

**VERIFIED: Simplification is applied BEFORE clipping** in all emission functions.

In `emit_line_feature` (`src/pipeline.rs:1080`):
```
for_each_zoom_simplified(merc, ..., |z, simplified| {
    for_each_tile_in_bbox(simp_bbox, z, |tx, ty| {
        clip_linestring(simplified, &clip)
    })
})
```

The same pattern in `emit_polygon_feature` (`src/pipeline.rs:1156`) and `emit_multipolygon_feature` (`src/pipeline.rs:1221`).

**Correctness:** Simplify-then-clip can produce slightly different results than clip-then-simplify. A vertex removed by simplification might have been a clip boundary intersection point, causing the clipped polygon to differ. However, this is standard practice in all tile generators (Planetiler, Tilemaker) because:
1. Clipping first would produce different simplified geometries for each tile, with seam artifacts
2. Simplifying in Mercator space before clipping ensures consistent simplification across tile boundaries

**Performance:** Simplifying first is also more efficient: it reduces vertex count before the O(n) clipping step. A 1000-vertex way at z10 might simplify to 50 vertices, making clipping ~20x cheaper.

---

## 4. Tile Fanout

### 4.1 Bbox-to-Tile Expansion

`for_each_tile_in_bbox()` (`src/geometry.rs:1013-1030`):
```rust
for ty in ty_min..=ty_max {
    for tx in tx_min..=tx_max {
        f(tx, ty);
    }
}
```

Simple rectangular scan. Tile coordinates computed from Mercator bbox scaled by 2^z.

**Important optimization:** The bbox is recomputed from the SIMPLIFIED geometry at each zoom level (`src/pipeline.rs:1085, 1161, 1226`). This means at low zooms, a highly simplified geometry may cover fewer tiles than the original bbox suggests. The comment at `src/pipeline.rs:1083-1084` explicitly notes this.

### 4.2 Single-Tile Fast Path

`is_single_tile()` (`src/geometry.rs:999-1009`): Checks if bbox falls within a single tile. When true, clipping is skipped entirely -- the geometry is converted directly to tile coordinates.

Used in:
- `emit_line_feature` (`src/pipeline.rs:1086`)
- `emit_polygon_feature` (`src/pipeline.rs:1162`)
- `emit_multipolygon_feature` (`src/pipeline.rs:1227`)

**VERIFIED:** When `single_tile == true`, no clipping function is called. Direct `to_tile_coords_into` or `to_tile_coords` followed by encoding.

### 4.3 Fanout Cost Analysis

For a feature covering N tiles at zoom Z:
- Simplification: O(1) per zoom level (done once, outside tile loop)
- Bbox computation: O(V) where V = simplified vertex count (done once per zoom level)
- Per tile: O(V) for clipping + O(V') for coordinate transform + O(V') for encoding
- Total per zoom level: O(V) + N * O(V)

For a large coastline at z14 with V=1000 simplified vertices covering 100 tiles:
- 100 full Sutherland-Hodgman polygon clips, each O(1000) = 400,000 operations
- Or 100 Cohen-Sutherland line clips for lines

**FINDING: There is no deduplication or sharing of clip work across tiles.** Each tile independently clips the full (simplified) geometry. For features spanning many tiles, this is the dominant cost.

The ocean subsystem partially addresses this with **row pre-clipping** (`src/ocean.rs:459-486`): before processing individual tiles in a row, the polygon is clipped to the row's Y-band, producing a smaller polygon that individual tile clips then operate on. This is a significant optimization for large ocean polygons.

**FINDING: The PBF feature emission path (pipeline.rs) does NOT have row pre-clipping.** This is only in ocean.rs. Large PBF features (country boundaries, coastlines) that span many tiles at high zoom levels will clip the full simplified polygon against every single tile.

### 4.4 Worst-Case Fanout

At z14, the world is 16384x16384 tiles. A feature spanning the entire Denmark extent (~3 degrees longitude, ~2.5 degrees latitude) might cover:
- z14: ~130 x ~160 = ~20,800 tiles

Each of these tiles gets a full S-H polygon clip or C-S line clip. For a 500-vertex simplified polygon, that's 20,800 * 4 * 500 = ~41.6M edge-vertex comparisons just for clipping at z14.

Country-scale boundaries (Germany, France) can be worse. Russia or Brazil at z14 would be catastrophic.

### 4.5 Ocean Tile Fanout (Scanline Fill)

The ocean subsystem uses a sophisticated scanline fill algorithm (`src/ocean.rs:350-543`) that avoids the naive bbox expansion:

1. **DDA rasterization** (`src/ocean.rs:600-614`): Marks only tiles that polygon edges actually cross ("boundary tiles")
2. **Scanline rows** (`src/ocean.rs:420-432`): Groups boundary tiles by row
3. **Gap filling** (`src/ocean.rs:504-528`): For gaps between boundary tiles in a row, a single point-in-polygon test determines if the entire gap should be filled
4. **Non-boundary rows** (`src/ocean.rs:529-541`): Rows with no boundary tiles get a single PIP test for the whole row

This reduces PIP calls from O(tiles_in_bbox) to O(gaps * rows + empty_rows). Combined with the land mask filter, this is very efficient.

---

## 5. Multipolygon Assembly

### 5.1 Ring Assembly: `join_ways()`

`join_ways()` (`src/multipolygon.rs:199-315`) joins way segments end-to-end:

**Data structures:**
- `chains: Vec<Vec<Point>>` -- active chains being built
- `endpoint_map: HashMap<(i64, i64), usize>` -- maps quantized endpoints to chain indices
- `closed: Vec<Vec<Point>>` -- completed rings

**Algorithm (two passes):**

**Pass 1** (`src/multipolygon.rs:205-219`): Greedy append
- For each way:
  1. Check if already a closed ring (first == last quantized): add to `closed` directly
  2. Try to find a chain with a matching endpoint via `endpoint_map`
  3. Attach way to chain (4 orientation cases handled in `attach_way`, `src/multipolygon.rs:358-398`)
  4. If no match, start a new chain

**Pass 2** (`src/multipolygon.rs:222-306`): Merge remaining unclosed chains
- Loop until no more merges possible:
  1. Rebuild endpoint_map from surviving chains
  2. Try to join each unclosed chain with another via shared endpoints
  3. Break on first successful merge (restarts the scan)
  4. If merged chain becomes closed, move to `closed`

**FINDING: Pass 2 has O(C^2) worst case** where C = number of unclosed chains after pass 1. Each iteration scans all chains and breaks on first merge, then restarts. In the worst case (all chains mergeable but in reverse order), this is C iterations of C scans = O(C^2). For a relation with 1000 member ways where greedy pass 1 fails to connect them, this could be 1000^2 = 1M iterations.

However, the typical case is much better: most OSM relations have ways ordered such that pass 1 closes most rings, leaving few chains for pass 2.

### 5.2 Endpoint Quantization

`quantize()` (`src/multipolygon.rs:184-189`):
```rust
let x = (p.x * 1e9).round() as i64;
let y = (p.y * 1e9).round() as i64;
```

Precision: 1e-9 Mercator units = ~0.04mm at equator. This is far finer than any coordinate precision in the data.

**Potential concern:** Floating-point rounding. Two ways that share an endpoint in the PBF may project to slightly different Mercator coordinates due to the LUT interpolation. But since both ways reference the same node ID, they will project to the SAME e7 values, and the LUT is deterministic, so quantization will match. Safe.

### 5.3 Inner-to-Outer Pairing: `pair_rings()`

`pair_rings()` (`src/multipolygon.rs:140-174`):

**Algorithm:**
1. Create one `(outer_ring, Vec::new())` per outer ring
2. For each inner ring:
   - Test `inner[0]` against each outer ring using `point_in_polygon`
   - Assign to first matching outer
   - If no match, assign to outer[0] (fallback for malformed data, `src/multipolygon.rs:169`)

**Complexity:** O(I * O * V) where:
- I = number of inner rings
- O = number of outer rings (tested sequentially until match found)
- V = average outer ring vertex count (for PIP test)

**FINDING: This is the O(I * O * V) cost the theoretical review flagged.** For a typical relation (1 outer, 2-3 inners), this is negligible. For pathological cases:
- A forest relation with 500 clearings (inners) and 3 outer polygons: 500 * 3 * V PIP tests
- A country boundary with 100 exclaves (outers) and 200 holes (inners): 200 * 100 * V PIP tests

The PIP test itself (`src/geometry.rs:1181-1200`) is the standard ray-casting algorithm, O(V) per test.

**Only `inner[0]` is tested** (`src/multipolygon.rs:161`). This is correct for valid OSM geometry but wrong for self-intersecting or degenerate rings. The code documents this tradeoff.

### 5.4 Memory Profile for Complex Relations

For a relation with R rings of average V vertices each:
- `member_ways`: R * (24 + V*16) bytes (Vec<MemberWay>, each MemberWay has Vec<Point>)
- `chains`: up to R vectors, peak size is sum of all vertices (~R*V*16 bytes) but chains are consumed/moved as rings close
- `endpoint_map`: ~2*R entries * ~48 bytes each (HashMap overhead) = ~96*R bytes
- `closed`: R vectors when fully assembled
- Output `MultiPolygon`: R polygons, total R*V*16 bytes in Points

For a pathological relation with 1000 rings, 100 vertices each:
- member_ways: ~1000 * (24 + 100*16) = ~1.6 MB
- Peak memory during assembly: ~3-4 MB (chains + endpoint_map + closed)
- Output: ~1.6 MB

This is manageable. The real concern is CPU time in pass 2 and PIP testing, not memory.

### 5.5 Allocation Patterns in Multipolygon Assembly

Notable allocations per call to `assemble()`:
1. `separate_by_role` (`src/multipolygon.rs:80-97`): 3 `Vec::with_capacity(members.len())` of slice references -- cheap
2. `join_ways` (`src/multipolygon.rs:200-203`): `chains`, `endpoint_map`, `closed` -- all with `Vec::with_capacity(ways.len())`
3. `append_way_to_chains` (`src/multipolygon.rs:349`): `way.to_vec()` for new chains -- copies vertex data
4. `attach_way` reversal cases (`src/multipolygon.rs:383, 389`): `way.iter().copied().rev().collect()` -- allocates a new Vec
5. Pass 2 reversals (`src/multipolygon.rs:251, 282`): `into_iter().rev().collect()` -- allocates new Vecs

**FINDING: Way reversal allocates a new Vec each time** (`src/multipolygon.rs:251, 282, 341-342, 383, 389`). For a relation where many ways need orientation correction, this creates O(R) temporary Vec allocations. Could be avoided by reversing in-place.

---

## 6. Allocation Patterns in Hot Geometry Paths

### 6.1 Tracing the 5.8 GB: `for_each_zoom_simplified`

The hotpath alloc profile reports `for_each_zoom_simplified` as 5.8 GB of allocations. Let's trace every allocation inside this function:

**Per-call allocations in `for_each_zoom_simplified` (`src/geometry.rs:354-393`):**
1. `cascade = merc.to_vec()` (`src/geometry.rs:363`): Copies entire input geometry. For V vertices, this is V * 16 bytes.
2. `keep_buf: Vec<bool> = Vec::new()` (`src/geometry.rs:364`): Starts empty, grows to V on first DP call.
3. `simp_buf: Vec<Point> = Vec::new()` (`src/geometry.rs:365`): Starts empty, grows to accommodate simplified output.

The `cascade` is swapped with `simp_buf` at each zoom level (`src/geometry.rs:384`), so no new allocation per zoom. The `keep_buf` resizes but `Vec::resize` doesn't reallocate if capacity is sufficient (it grows only on the first zoom level).

**The dominant allocation is #1: `merc.to_vec()`.** Called once per feature per match. For Denmark with 16M features, if average geometry is ~8 vertices (128 bytes per call):
- 16M * 128 bytes = 2.0 GB just for cascade copies

But that's only 2 GB of the reported 5.8 GB. Where's the rest?

**The callback is the key.** `for_each_zoom_simplified` is hotpath-measured, so its allocation profile includes everything in the callback closure. The callback in `emit_line_feature` (`src/pipeline.rs:1080-1134`) allocates:
- `SmallVec<[Vec<Point>; 1]>` from `clip_linestring()` -- per tile, per zoom (see Section 2.4)
- `Vec<u8>` from `encode_feature_data_with_attrs()` -- per sort record

And in `emit_polygon_feature` (`src/pipeline.rs:1156-1199`) -- less allocation because `clip_polygon_into` reuses buffers, but still:
- `Vec<u8>` from `encode_feature_data_with_attrs()` -- per sort record

**Inference:** The 5.8 GB likely breaks down as:
- ~2.0 GB: `merc.to_vec()` cascade copies (line 363)
- ~1.5-2.0 GB: `clip_linestring` output Vecs (allocated per tile per zoom per line feature)
- ~1.5-2.0 GB: `encode_feature_data_with_attrs` sort record data (per emitted record)

### 6.2 Per-Feature Allocations

Allocations that occur once per feature (way or relation polygon):

| Location | Allocation | Size | Avoidable? |
|----------|-----------|------|------------|
| `pipeline.rs:767-769` | `merc: Vec<Point>` from projection | V*16 bytes | No (must own for bbox + emission) |
| `pipeline.rs:781` | `records: Vec<SortRecord>` | variable | No (must collect for drain thread) |
| `geometry.rs:363` | `cascade = merc.to_vec()` in `for_each_zoom_simplified` | V*16 bytes | **Yes** -- could take ownership or use a passed-in buffer |
| `geometry.rs:364-365` | `keep_buf`, `simp_buf` | V*1 + V*16 bytes | **Yes** -- could be hoisted to caller |

### 6.3 Per-Tile Allocations

Allocations that occur once per tile per zoom per feature:

| Location | Allocation | Size | Avoidable? |
|----------|-----------|------|------------|
| `geometry.rs:559` | `clip_linestring` return SmallVec | ~24 bytes inline | Mostly inline (SmallVec<1>) |
| `geometry.rs:575` | `current: Vec<Point>` inside clip_linestring | variable | **Yes** -- could reuse buffer |
| `geometry.rs:622` | `std::mem::take(current)` creates new Vec per segment | variable | **Yes** -- could reuse |
| `pipeline.rs:1235,1247,1261,1274` | `to_tile_coords()` (allocating variant) in multipolygon | V'*8 bytes | **Yes** -- buffer variant exists but not used |
| `pipeline.rs:1285` | `ring_refs: Vec<&[(i32,i32)]>` | R*16 bytes | **Yes** -- typically <=26 elements, could use SmallVec or array |
| `ocean.rs:585` | `ring_refs: Vec<&[(i32,i32)]>` | same | Same |

### 6.4 Specific Findings

**FINDING 1: `to_tile_coords` (allocating) used in multipolygon emission instead of `to_tile_coords_into`.**

In `emit_multipolygon_feature` (`src/pipeline.rs:1235, 1247, 1261, 1274`):
```rust
let mut outer_tc = geometry::to_tile_coords(simp_outer, tx, ty, z);
let mut inner_tc = geometry::to_tile_coords(inner, tx, ty, z);
```

The buffer-reuse variant `to_tile_coords_into` exists (`src/geometry.rs:1065-1081`) and is used in `emit_line_feature` and `emit_polygon_feature` for the outer ring. But the multipolygon path uses the allocating variant for ALL rings (outer + each inner). Each call allocates a new `Vec<(i32, i32)>`.

For a multipolygon with 10 inners covering 20 tiles at z14, that's 20 * 11 = 220 Vec allocations just for coordinate transforms.

**FINDING 2: `ring_refs` Vec allocated per tile per zoom.**

`src/pipeline.rs:1285`:
```rust
let ring_refs: Vec<&[(i32, i32)]> = all_rings.iter().map(Vec::as_slice).collect();
```

This creates a small Vec (typically 1-10 elements) per tile. The comment says "must be local (can't hoist across calls)" but a `SmallVec<[&[(i32, i32)]; 8]>` would avoid the heap allocation for the common case (< 8 rings).

Similarly in `src/ocean.rs:585`.

**FINDING 3: `for_each_zoom_simplified` internal buffers not hoistable.**

The `cascade`, `keep_buf`, and `simp_buf` in `for_each_zoom_simplified` (`src/geometry.rs:363-365`) are allocated inside the function. The multi-polygon variant (`for_each_zoom_simplified_multi`) has a `SimplifyMultiScratch` struct for hoisting, but the single-geometry variant does not. This means every call to `emit_line_feature` and `emit_polygon_feature` allocates 3 new Vecs (even if they're empty initially, the cascade is always V * 16 bytes).

A similar `SimplifySingleScratch` struct could be introduced to hoist these buffers.

---

## 7. Numerical Precision

### 7.1 Coordinate Space Ranges

- Mercator [0,1]: All coordinates in range 0.0 to 1.0. f64 has 52 bits of mantissa, giving ~15-16 decimal digits of precision. At 0.5 (center), ulp = 1.1e-16. This is ~0.0044 micrometers. No precision concern.

- Tile coordinates (i32): `EXTENT = 4096`, with buffer this ranges from about -8 to 4104 per axis. Well within i32 range.

- Signed area in tile coordinates (`src/geometry.rs:1160-1171`): Uses f64 accumulation for i32 * i32 cross products. Each cross product is at most ~4104^2 = ~16.8M, well within f64 exact integer range (2^53).

### 7.2 Tile Boundary Edge Cases

`clamp_tile()` (`src/geometry.rs:1034-1043`) uses `val.floor()`. For a coordinate at exactly a tile boundary (e.g., Mercator x = 0.5 at z1), `floor(0.5 * 2) = floor(1.0) = 1`. This assigns the point to tile 1, not tile 0. Since the clip rect has an 8-pixel buffer extending beyond tile boundaries, this is safe -- the feature will still be included in tile 0's clipped output.

**VERIFIED:** No precision issues at tile boundaries due to the buffer.

### 7.3 Perp Distance Squared

`perp_dist_sq()` (`src/geometry.rs:324-338`): All arithmetic is in f64. The `t` parameter is clamped to [0, 1], which handles endpoint projections correctly. The `len_sq < 1e-30` guard (`src/geometry.rs:325`) handles degenerate segments. No precision concerns.

---

## 8. Complexity Analysis Summary

### Per-Feature Costs (single feature, one match)

| Operation | Avg Complexity | Worst Case | Where |
|-----------|---------------|------------|-------|
| Projection | O(V) | O(V) | `pipeline.rs:767-769` |
| Bbox computation | O(V) | O(V) | `geometry.rs:1109-1121` |
| Simplification (per zoom) | O(V log V) | O(V^2) | `geometry.rs:277-292` |
| Tile enumeration | O(T) | O(T) | `geometry.rs:1025-1029` |
| Polygon clip (per tile) | O(V') | O(V') | `geometry.rs:691-728` |
| Line clip (per tile) | O(V') | O(V') | `geometry.rs:559-587` |
| Coord transform (per tile) | O(V') | O(V') | `geometry.rs:1065-1081` |
| Total per zoom level | O(V log V + T*V') | O(V^2 + T*V') | |

Where V = original vertex count, V' = simplified vertex count, T = tiles covered.

### Per-Relation Costs (multipolygon)

| Operation | Avg Complexity | Worst Case | Where |
|-----------|---------------|------------|-------|
| Way joining (pass 1) | O(W * V_avg) | O(W * V_avg) | `multipolygon.rs:205-219` |
| Way joining (pass 2) | O(C) | O(C^2) | `multipolygon.rs:222-306` |
| Ring pairing | O(I * O * V_outer) | O(I * O * V_outer) | `multipolygon.rs:140-174` |
| Per-polygon emission | same as per-feature | same | per polygon in multipolygon |

Where W = member ways, C = unclosed chains after pass 1, I = inner rings, O = outer rings.

---

## 9. What the Theoretical Review Got Right

### Finding 1 (HIGH): "Tile fanout can dominate CPU for large geometries; per-tile clipping is expensive"

**CONFIRMED and DETAILED.** Per-tile clipping is O(V') per tile with no work sharing. For features covering many tiles at high zoom, this is the dominant cost. The single-tile fast path helps for small features, but large features (country boundaries, coastlines, large forests) can cover hundreds or thousands of tiles at z14.

The ocean subsystem has row pre-clipping to mitigate this, but the PBF emission path does not.

### Finding 2 (MEDIUM): "Multipolygon pairing uses point-in-polygon scans"

**CONFIRMED.** O(I * O * V_outer) PIP tests. Pathological for relations with many outers and many inners. Additionally, the way-joining pass 2 has O(C^2) worst case for unclosed chains.

### Finding 3 (MEDIUM): "Frequent Vec creation despite scratch buffers"

**CONFIRMED and EXTENDED.** Key remaining allocation sites:
1. `cascade = merc.to_vec()` in `for_each_zoom_simplified` (per feature, ~2 GB for Denmark)
2. `clip_linestring` output Vecs (per tile per zoom per line feature)
3. `to_tile_coords()` allocating variant in multipolygon emission (per ring per tile)
4. `ring_refs` Vec in multipolygon tile encoding (per tile)

---

## 10. What the Review Missed

### 10.1 No `clip_linestring_into` Buffer-Reuse Variant

The polygon clipping has an excellent `_into` variant with reusable buffers. Line clipping does not. Given that line features are likely ~50% of all features (streets, boundaries), this is a significant source of allocator churn. Each line clip creates a `SmallVec` container (inline for 1 segment) but allocates a `Vec<Point>` for each output segment.

### 10.2 `to_tile_coords` vs `to_tile_coords_into` Inconsistency

`emit_line_feature` and `emit_polygon_feature` correctly use `to_tile_coords_into` for their single ring. But `emit_multipolygon_feature` uses the allocating `to_tile_coords()` for all rings (outer and each inner). At 4+ calls per tile per zoom, this adds up.

### 10.3 `for_each_zoom_simplified` Cascade Copy

The `merc.to_vec()` at `src/geometry.rs:363` copies the entire geometry on every call. For the multi-polygon variant, `SimplifyMultiScratch` avoids this via `cascade_outer.extend_from_slice()` into a pre-existing buffer. But the single-geometry variant has no equivalent -- it allocates a fresh Vec every time.

This is fixable by either:
- Adding a `SimplifySingleScratch` struct
- Taking `merc` by ownership when the caller is done with it (e.g., `for_each_zoom_simplified_owned`)

### 10.4 Row Pre-Clipping Only in Ocean

The ocean subsystem clips each polygon to a tile row's Y-band before per-tile clipping, reducing per-tile clip input from V to ~V/rows_covered. This optimization is absent from the PBF emission path (`emit_polygon_feature`, `emit_multipolygon_feature`).

For a polygon covering 10 rows of tiles at z14, row pre-clipping would reduce clipping work by ~10x. This would primarily benefit large polygons (forests, water bodies, industrial areas) that span many tiles.

### 10.5 Douglas-Peucker Recursive Stack Depth

The recursive DP implementation has O(n) worst-case stack depth. For very long ways (10K+ vertices, e.g., coastlines), this could approach stack limits on rayon worker threads. Not a correctness issue with typical data, but worth noting for planet-scale robustness.

An iterative DP implementation using an explicit stack (heap-allocated) would eliminate this concern. However, the typical case (ways with <100 vertices) has ~7 levels of recursion, so this is low priority.

### 10.6 Redundant Bbox Computation

`merc_bbox()` is called multiple times for the same geometry:
1. In `process_raw_way` (`src/pipeline.rs:771`): `bbox = merc_bbox(&merc)`
2. In `emit_polygon_feature` via `for_each_zoom_simplified` callback (`src/pipeline.rs:1161`): `simp_bbox = merc_bbox(simplified)` -- but this is for the SIMPLIFIED geometry, so it's necessary

The second call is justified (bbox of simplified geometry differs from original). No redundancy here.

### 10.7 SIMD Opportunities

Several inner loops are SIMD-friendly but not explicitly vectorized:

1. **`merc_bbox`** (`src/geometry.rs:1109-1121`): Min/max reduction over Point array. Could use SIMD min/max operations.
2. **`to_tile_coords_into`** (`src/geometry.rs:1072-1081`): Uniform transform (multiply + subtract + multiply + round) per point. Perfectly vectorizable.
3. **`clip_polygon_edge_into`** (`src/geometry.rs:780-798`): Harder to SIMD due to conditional output, but the `is_inside` check could be batched.
4. **`signed_area`** (`src/geometry.rs:808-820`): Cross-product accumulation over pairs. Vectorizable with appropriate interleaving.

These are all O(n) loops with simple arithmetic. The compiler may auto-vectorize some of them, but explicit SIMD (via `std::simd` or `packed_simd`) could guarantee it. Impact: likely 2-4x on these specific loops, but they may not be the bottleneck compared to cache misses and allocation overhead.

### 10.8 Interaction Between Simplification and Subpixel Filtering

Simplification at zoom Z reduces vertices. Then per-tile, the code checks `ring_is_subpixel()` or `line_is_subpixel()` in tile coordinates. But the subpixel check in `for_each_zoom_simplified` (`merc_bbox_is_subpixel`, line 373) uses the cascade (which may have been simplified at a finer zoom). If the cascade has been simplified aggressively at z13, it might pass the subpixel check at z10 even though the original geometry wouldn't. This is correct behavior -- it's checking the simplified version, which IS the geometry at that zoom level.

---

## 11. Optimization Candidates (Ranked)

### HIGH Impact

1. **Add `clip_linestring_into` with buffer reuse.** Eliminate per-tile Vec allocation for line clipping. This affects every line feature at every tile at every zoom. Estimated impact: eliminate ~1.5 GB of allocator churn for Denmark.

2. **Add `SimplifySingleScratch` to eliminate `merc.to_vec()` in `for_each_zoom_simplified`.** The cascade buffer, keep_buf, and simp_buf could be hoisted to the caller. Estimated impact: eliminate ~2 GB of allocator churn for Denmark.

3. **Row pre-clipping for PBF polygon features.** Port the ocean subsystem's row pre-clipping to `emit_polygon_feature` and `emit_multipolygon_feature`. Would reduce clipping work by ~rows_covered for large polygons. Benefit scales with feature size and zoom level.

### MEDIUM Impact

4. **Use `to_tile_coords_into` in `emit_multipolygon_feature`.** Replace allocating `to_tile_coords()` with buffer variant for outer and inner rings. Requires managing a pool of buffers (one per ring in `all_rings`). The `all_rings: Vec<Vec<(i32, i32)>>` already exists and is reused -- the issue is that `to_tile_coords()` returns a new Vec that gets pushed into `all_rings`, discarding the old capacity. Could instead use `all_rings[i]` as the output buffer directly.

5. **Replace `ring_refs: Vec<&[...]>` with `SmallVec<[&[...]; 8]>`.** Avoids heap allocation for the common case (1-7 rings). Trivial change, small impact per call but called millions of times.

6. **Spatial index for multipolygon inner-to-outer pairing.** For relations with many outers (>10), a bounding-box index would reduce PIP tests from O(I * O) to O(I * log O). Only benefits pathological relations.

### LOW Impact

7. **Iterative Douglas-Peucker.** Replace recursive with iterative using explicit stack. Only matters for very long ways (>5K vertices), which are rare outside coastlines.

8. **SIMD vectorization of inner loops.** Potentially 2-4x on specific loops, but these loops are not the dominant cost for typical features. Most benefit for features with many vertices (coastlines, complex polygons).

---

## 12. Data Flow Summary

```
PBF element
  |
  v
project_e7 (O(V), once per feature)
  |
  v
merc: Vec<Point>           ---> bbox computation (O(V))
  |                              |
  v                              v
for_each_zoom_simplified   for_each_tile_in_bbox (O(T))
  |                              |
  v                              v
cascade = merc.to_vec()    per tile:
  |                          - clip (O(V'))
  v                          - to_tile_coords (O(V'))
dp_recurse (O(V log V))     - size filter (O(V'))
  |                          - orient (O(V'))
  v                          - encode MVT (O(V'))
simplified (V' < V)          - push SortRecord
  |
  v
callback(z, simplified)
```

For multipolygons, add before emission:
```
member_ways
  |
  v
join_ways (O(W * V_avg), two passes)
  |
  v
pair_rings (O(I * O * V_outer))
  |
  v
per polygon: for_each_zoom_simplified_multi
```

---

## 13. Key Metrics to Measure

To validate this analysis and prioritize optimizations, the following metrics would be most informative:

1. **Distribution of tiles-per-feature at each zoom level** -- how many features are single-tile vs multi-tile? This determines the value of clip optimization.
2. **Total clip calls vs total features** -- the clip amplification factor.
3. **Allocation bytes in `for_each_zoom_simplified` broken down by callsite** -- to confirm the cascade copy dominance.
4. **Number of multipolygon relations with >10 outers** -- to assess spatial index value.
5. **Number of line features covering >100 tiles at z14** -- to assess row pre-clip value for PBF features.
6. **P99 vertex count per feature** -- to assess DP recursion depth and worst-case clipping cost.
