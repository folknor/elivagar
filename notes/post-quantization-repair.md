# Post-Quantization Polygon Repair: Consolidated Findings

Six independent reviewers (Planetiler, Tilemaker, Tippecanoe perspectives × Claude
+ Codex providers) were asked the same question about MapLibre rendering artifacts
at z10+ despite all validators passing. They converged on the same root cause and
the same class of fix.

## The Root Cause

**The geometry is perfect in f64 Mercator space. The problem is created by
`to_tile_coords()` rounding to i32.**

When dense coastline vertices are quantized from f64 to integer tile coordinates
(0-4096), three classes of degeneracy are introduced:

### 1. Pinch points (T-junctions)

Two coastline entry/exit points on the same tile edge, at slightly different f64
positions, snap to the same i32 coordinate. The ring now touches itself without
crossing - a vertex sits exactly on a non-adjacent edge.

```
Before quantization:     After quantization:
   /\                       /\
  /  \                     /  \
 / .. \  (0.3px gap)      /    \  (gap = 0, T-junction)
/    . \                  /      |
```

### 2. Backtrack spikes (A→B→A sequences)

Dense f64 vertices in a small area round to the same i32 point, creating sequences
where the ring goes forward, then backtracks to a previous coordinate:

```
f64: (100.3, 200.1) → (100.7, 200.4) → (100.2, 200.1)
i32: (100, 200)     → (101, 200)      → (100, 200)       ← A→B→A backtrack
```

### 3. Collinear overlap

Multiple f64 vertices along a nearly-straight line snap to the same i32 line,
creating zero-area collinear edge runs that look like a proper ring to validators
but confuse earcut's bridge algorithm.

### Why `ring_is_simple()` misses all of these

`ring_is_simple()` (`geometry/mod.rs:342`) calls `segments_cross()` which
requires ALL FOUR cross products to be non-zero (line 372):

```rust
d1 != d2 && d3 != d4 && d1 != 0 && d2 != 0 && d3 != 0 && d4 != 0
```

This rejects:
- T-junctions (one cross product is zero - vertex on edge)
- Collinear overlaps (multiple cross products are zero)
- Touching endpoints (zero cross product at shared point)

A stronger validator already exists in the codebase: `is_valid_simple_ring_points()`
(`emit.rs:262`) which DOES reject edge-touching cases. But it's currently unused
(commented out because it was too aggressive - it dropped valid features).

### Why z10+ specifically

At z1-9, clipped ocean rings are simple (few vertices per tile). With O(V × E)
vertex-edge proximity pairs, the probability of at least one quantization-induced
T-junction is low.

At z10+, detailed coastlines produce hundreds to thousands of vertices per tile.
The probability that quantization creates at least one touching/overlapping edge
approaches certainty for complex coastlines.

### Why everything else passes

- **SVG**: Uses `fill-rule="evenodd"` scanline fill. T-junctions and backtracks
  don't affect evenodd rendering - the fill still alternates correctly.
- **MVT validators (vtvalidate/vtzero)**: Check protobuf structure and command
  validity, not intra-ring topology.
- **Our verify tool**: Uses `ring_is_simple()` which only catches proper crossings.
- **Round-trip re-encode**: `@mapbox/vector-tile` decodes the geometry faithfully
  (the bytes are correct), then `vt-pbf` re-encodes the same T-junction geometry.
  Same input → same artifacts.
- **Earcut**: The ONLY consumer that assumes strictly simple input. T-junctions
  cause the triangulation to produce garbage triangles.

## How Each Competitor Fixes This

### Planetiler: `snapAndFixPolygon()`

After clipping and quantizing, Planetiler runs a 4-level repair cascade
(`GeoUtils.java:315-399`):

1. `GeometryPrecisionReducer` to detect new intersections from rounding
2. Rebuild valid polygon topology
3. Remove duplicate rounded points
4. Falls back to progressively coarser precision reduction if naive snapping breaks topology

Called in `FeatureRenderer.java:249`. Explicitly commented in source as
"very expensive, but necessary."

Planetiler also uses a stripe-based decomposition (clip by X-stripes then Y-stripes)
instead of per-tile boolean intersection, so each tile's clipped fragments are
smaller to begin with.

### Tilemaker: Three post-quantization defenses

1. **`scaleRing()` backtrack dedup** (`coordinates_geom.cpp:36-52`): After
   quantization, checks if each new point matches any of the LAST 5 points and
   truncates the spike. Elivagar only removes consecutive duplicates.

2. **Boost intersection fallback** (`tile_data.cpp:336-342`): If post-clip
   geometry fails `is_valid()`, retries with full topology-aware Boost intersection.

3. **`remove_spikes()`** (`tile_worker.cpp:227`): Explicit spike removal after
   simplification.

Additionally:
- `make_valid()` (`geom.cpp:138-144`) - full geometric correction step
- `correct()` (`geom.cpp:124-133`) - re-corrects polygons after simplification
- Strips consecutive duplicate vertices before handing to vtzero encoder
  (`tile_worker.cpp:174-205`)

### Tippecanoe: Post-quantization Wagyu union

After tile-scale quantization, tippecanoe runs Wagyu polygon union
(`clean_or_clip_poly()`, `clip.cpp:260-387`):

1. `remove_noop()` (`clip.cpp:532-604`): Strips consecutive duplicates, unused
   movetos, empty rings BEFORE any area/winding calculations
2. Wagyu union with `fill_type_positive` (non-zero fill rule), called after
   `to_tile_scale()` (`tile.cpp:688-698`)
3. `fix_polygon()` (`clip.cpp:1755-1887`): Canonicalizes winding AND rotates each
   ring so the start vertex is on a "far edge" - this affects which vertices DP
   pins, since DP always retains the first/last vertex
4. `coalesce_polygon()` (`clip.cpp:1926-1985`): Only splits very large polygon
   batches (~100K vertices) recursively

The Wagyu pass uses **non-zero fill rule**, not even-odd. This is potentially
significant - see fill rule section below.

## Additional Findings

### Fill rule mismatch (potentially critical)

Elivagar calls i_overlay with `FillRule::EvenOdd` (`clip.rs:353`). MapLibre's
polygon fill shader uses the non-zero winding rule (GPU stencil buffer default).
Tippecanoe explicitly uses `fill_type_positive` (non-zero) in Wagyu.

EvenOdd and NonZero produce different results for self-touching polygons:
- EvenOdd: a region covered twice cancels out (becomes exterior)
- NonZero: a region covered twice with same winding stays interior

After quantization creates a pinch point, the polygon effectively self-overlaps
at that point. EvenOdd may interpret the overlapping region differently than
NonZero, producing geometry that looks correct in SVG (evenodd) but wrong in
MapLibre (nonzero).

**This should be tested**: change `FillRule::EvenOdd` to `FillRule::NonZero` in
`clip_polygon_robust()` and rebuild.

### `filter_holes_for_outer()` u64 bitmask limit

(`tiles.rs:362`): Stores the keep set in a `u64` and iterates only
`ring_count.min(64)`. Any polygon with more than 63 holes silently drops holes
beyond the 63rd. Not the likely cause of z10+ artifacts but a latent correctness
bug. The z8/135/75 tile had 30 rings - close but not over the limit.

### Ring start vertex affects DP simplification

Tippecanoe's `fix_polygon()` rotates each ring so the start vertex is at a
"far edge" position before DP simplification. Elivagar preserves whatever
arbitrary start vertex came out of clipping. Since DP always pins the first and
last vertex, different start positions produce different simplified rings. For
long clipped coastlines, this can change which vertices survive simplification
and whether the result is earcut-safe.

### Inconsistent polygon paths in emit.rs

The single-ring polygon path (`emit.rs:711`) does clip → quantize → orient →
tile-DP → encode with NO earcut safety passes. The multipolygon path
(`emit.rs:1006-1037`) applies hole filtering and nudging. The ocean path
(`ocean.rs:584-618`) has the most hardening. If any water/ocean-adjacent geometry
reaches the weaker single-ring path, artifacts are expected.

## Proposed Fix Options (ordered by implementation cost)

### Option 1: Backtrack dedup (cheapest, O(n))

Port tilemaker's `scaleRing()` 5-point lookback. After `to_tile_coords()`, scan
the ring and remove any vertex that matches any of the previous 5 vertices.
This eliminates A→B→A spikes and short-range backtracks from quantization.

```rust
fn dedup_quantized_ring(ring: &mut Vec<(i32, i32)>, lookback: usize) {
    let mut write = 0;
    for read in 0..ring.len() {
        let p = ring[read];
        let start = write.saturating_sub(lookback);
        if (start..write).any(|j| ring[j] == p) {
            // Backtrack detected - truncate back to the first match
            while write > start && ring[write - 1] != p {
                write -= 1;
            }
        } else {
            ring[write] = p;
            write += 1;
        }
    }
    ring.truncate(write);
}
```

Handles: backtrack spikes.
Does NOT handle: T-junctions, collinear overlap.

### Option 2: i_overlay re-run in tile-integer space (medium cost)

After `to_tile_coords()`, cast the i32 ring back to f64 and run
`clip_polygon_robust()` against the tile rectangle in tile-coordinate space.
i_overlay will detect and resolve any topology damage from quantization.

```rust
// After to_tile_coords:
let tile_rect = ClipRect::new(-128.0, -128.0, 4224.0, 4224.0);
let f64_ring: Vec<Point> = ring.iter()
    .map(|&(x, y)| Point::new(f64::from(x), f64::from(y)))
    .collect();
let repaired = clip_polygon_robust(&f64_ring, &holes_f64, &tile_rect);
```

Handles: all three degeneracy classes (T-junctions, backtracks, collinear overlap).
Cost: one additional boolean intersection per tile.
Performance: acceptable for the S-H fast path / i_overlay fallback hybrid, where
only tiles that fail the simple-ring check take this path. NOT acceptable as a
blanket pass on every tile.

### Option 3: Hybrid fast-path with post-quantization repair (recommended)

Combine Options 1 and 2 with the existing S-H fast path:

1. S-H clip (fast)
2. `to_tile_coords()` (quantize to i32)
3. Backtrack dedup (Option 1, O(n))
4. `ring_is_simple()` check (but upgraded to detect T-junctions)
5. If simple: orient + DP simplify + encode (fast path)
6. If not simple: i_overlay re-run in tile-integer space (Option 2)
7. Orient + DP simplify + encode

This gives S-H speed for the ~95% of tiles that produce clean geometry after
dedup, and i_overlay robustness for the ~5% that need repair.

The key upgrade needed in step 4: strengthen `ring_is_simple()` (or use
`is_valid_simple_ring_points()`) to detect T-junctions, not just proper crossings.

### Option 4: Change fill rule to NonZero (separate test)

Independent of Options 1-3: change `FillRule::EvenOdd` to `FillRule::NonZero`
in `clip_polygon_robust()` and rebuild. If this alone fixes z10+, the fill rule
mismatch is the primary issue and the post-quantization repair may be less urgent.

## Follow-Up: When Does Each Competitor Repair?

A second question was sent to all six reviewers: "Does your repair step operate
on integer coordinates or floating-point coordinates?" All six confirmed the same
fundamental answer.

### Answer: All three competitors repair AFTER quantization to integers.

**Planetiler** (`snapAndFixPolygon`, `GeoUtils.java:368-399`):

Operates on grid-snapped doubles - effectively integers represented as floats.
The sequence is:

1. `PointwiseRounder.transform(geom)` - rounds each coordinate to the tile grid
   (PrecisionModel(16) → grid spacing of 1/16 in tile-relative space). Values
   become `0.0, 0.0625, 0.125, ...`. Coordinates remain `double` throughout.
2. `isValid()` - full JTS/OGC validity check (T-junctions, overlaps, all).
3. If invalid: `GeometryPrecisionReducer.reduce()` - uses JTS noding framework
   to detect new edge intersections from rounding, split edges at those points,
   and rebuild polygon topology. All in double space on grid-snapped values.
4. Final cast to int (`Math.round(cx * SCALE)`) is lossless because doubles are
   already exactly on the target grid.

Comment in source: "very expensive, but necessary" (`FeatureRenderer.java:249`).

The key insight: Planetiler repairs in "constrained float" space where values are
already on the integer grid. The final int cast introduces zero new damage because
the grid-snapped doubles are exactly representable.

**Tilemaker** (`scaleRing`, `coordinates_geom.cpp:36-52`):

Repairs DURING quantization itself - as each f64 vertex is converted to i32, it's
checked against the last 5 integer points:

```cpp
std::vector<Point> TileBbox::scaleRing(Ring const &src) const {
    for(auto &i: src) {
        auto scaled = scaleLatpLon(i.y(), i.x());  // float → int
        for (size_t j=1; j<5; j++) {                // check last 5 int points
            if (points[points.size()-j] == scaled) {
                points.resize(points.size()-j+1);   // BACKTRACK: kill spike
                break;
            }
        }
    }
}
```

This catches degeneracies AT THE MOMENT they're created by rounding. Then after
DP simplification in integer space, `remove_spikes()` runs again as a second pass.
Additionally, if post-clip geometry fails `is_valid()`, tilemaker falls back to
full Boost topology-aware intersection (`tile_data.cpp:336-342`).

The pipeline: clip(float) → validate(float) → quantize+repair(int) →
simplify(int) → spike removal(int) → dedup(int)

**Tippecanoe** (`coalesce_polygon` → `clean_or_clip_poly`, `tile.cpp:688-698`):

Quantizes FIRST, then repairs on the quantized integers:

```cpp
// tile.cpp:687-688 - STEP 1: quantize
drawvec geom = (*features)[i]->geometry;
to_tile_scale(geom, z, out_detail);

// tile.cpp:690-698 - STEP 2: repair on QUANTIZED integers
if (t == VT_POLYGON) {
    // Comment: "Scaling may have made the polygon degenerate."
    coalesce_polygon(geom, true);  // true = try 16x scaling
}
```

`coalesce_polygon` calls `clean_or_clip_poly` which runs Wagyu union on the
integer coordinates. The `true` parameter means it multiplies integers by 16
before feeding to Wagyu (4 extra bits of precision to resolve near-coincident
vertices). If 16x doesn't round-trip cleanly, it retries at 1x.

Uses `fill_type_positive` (non-zero fill rule) in Wagyu, NOT even-odd.

Also has earlier `fix_polygon()` (`clip.cpp:1755-1887`) that runs on integer
world coordinates (not float) to canonicalize winding and rotate ring start
vertex before tile quantization.

### Elivagar's Current Pipeline (the gap)

```
clip_polygon_robust()     ← i_overlay in f64 Mercator (topologically perfect)
to_tile_coords()          ← f64→i32 rounding (introduces T-junctions, spikes)
close_and_orient_cw()     ← winding fix on i32
simplify_ring_safe()      ← DP on i32, ring_is_simple() only catches proper crossings
filter_holes_for_outer()  ← hole containment on i32
nudge_*()                 ← small vertex adjustments on i32
encode_polygon()          ← MVT encoding from i32
```

No post-quantization topology repair. The i_overlay output is perfect in f64,
but `to_tile_coords()` destroys that guarantee, and nothing restores it. Every
competitor has a repair step at exactly this point.

### Summary Table

| | Repair coordinate space | Repair timing | Repair method |
|---|---|---|---|
| Planetiler | Grid-snapped doubles (effectively integers) | After snap, before final int cast | JTS GeometryPrecisionReducer (noding + topology rebuild) |
| Tilemaker | Integers | During quantization (5-point lookback) + after simplification (spike removal) | Backtrack dedup + Boost intersection fallback |
| Tippecanoe | Integers (16x upscaled for precision) | After quantization | Wagyu union with non-zero fill rule |
| **Elivagar** | **None** | **N/A** | **N/A - this is the bug** |

## Round 2 Findings (6 reviewers, 2026-03-28)

Follow-up question: should we use a stronger validity check as a gate, or repair
unconditionally? And should the repair be i_overlay intersection with tile rect,
or something else?

**Unanimous answers:**

1. **Don't gate - repair unconditionally.** Both Planetiler and Tippecanoe repair
   every polygon after quantization with no validity check. Tippecanoe's comment:
   "Scaling may have made the polygon degenerate." The assumption is that
   quantization damage is probabilistic and hard to detect exhaustively. A hybrid
   check-then-repair is strictly harder to get right than always-repair.

2. **Self-union, not tile-rect intersection.** Tippecanoe runs `clip_type_union`
   with `clip=false` - the polygon is unioned with itself, resolving T-junctions,
   collinear overlaps, and self-intersections. No clip rect needed since the ring
   is already clipped. For i_overlay: cast i32→f64 (lossless), run
   `OverlayRule::Union` on the ring set.

3. **Alternative: edge splitting (potentially much faster).** The Planetiler
   Claude agent proposed: scan for T-junctions (vertex on non-adjacent edge),
   insert the vertex into that edge, re-dedup. This is O(n²) but trivial integer
   math - no boolean engine overhead. Handles T-junctions specifically. Could be
   a fast-path replacement for the full self-union if profiling shows the union
   is too expensive at planet scale.

## What To Do First

1. **Test fill rule change** (5 minutes): EvenOdd → NonZero in clip.rs. Rebuild.
   If z10+ improves, this is a significant finding.

2. **Implement backtrack dedup** (30 minutes): Port tilemaker's 5-point lookback.
   Apply after `to_tile_coords()` in ocean.rs and emit.rs. Rebuild and test.

3. **Strengthen ring_is_simple** (30 minutes): Add T-junction detection (vertex
   exactly on non-adjacent edge). This makes the S-H fast path / i_overlay
   fallback hybrid viable.

4. **Implement Option 3 hybrid** (2-3 hours): The full solution. S-H fast path
   with post-quantization repair fallback.

5. **Performance recovery**: Once correctness is established, optimize the ocean
   path back toward the 1.5s baseline. The hybrid approach should achieve this
   since most tiles don't need the expensive i_overlay path.
