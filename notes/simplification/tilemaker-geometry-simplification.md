# How Tilemaker Simplifies Geometry (Especially Oceans)

Tilemaker uses 6 layered strategies to keep tiles small. Our ocean tiles at z10
can have ~6000 features and be 660KB as raw SVG - way too much for MapLibre JS.
This documents every mechanism tilemaker uses to avoid that problem.

Source: analysis of tilemaker source code (github.com/systemed/tilemaker).

---

## 1. Two Simplification Algorithms

### Visvalingam-Whyatt (preferred for water/ocean)

File: `src/visvalingam.cpp`

Removes the least visually important points first by computing the triangle area
formed by each point and its neighbors. Points with the smallest triangle area
are removed first. Better than Douglas-Peucker for natural features like
coastlines because it preserves overall shape character.

Key details:
- Custom min-heap for efficient point removal
- Uses double triangle area (cross product) to avoid floating point issues
- Enforces minimum point retention: 2 for linestrings, 4 for polygons (to keep them valid)
- When removing a point, the new area is set to max(current, removed) to prevent oscillation
- After simplifying MultiPolygons, runs `make_valid()` to fix any introduced issues

### Douglas-Peucker (default)

File: `src/geom.cpp:15-136`

Removes points within a perpendicular distance tolerance. Has a built-in
self-intersection guard using an R-tree spatial index:
- Checks if middle points lie on envelope boundaries and preserves them
- Only removes points if distance < tolerance AND no self-intersections are detected
- Inner rings are simplified first, then outer ring is simplified against inner ring R-tree
- Rings with perimeter < 3 * tolerance are dropped entirely
- Overlapping inner rings are merged via boolean union (`simplify_combine()`)

### Per-Layer Configuration

```json
"ocean": {
    "simplify_algorithm": "visvalingam",
    "simplify_below": 13,
    "simplify_level": 0.0001
}
```

Defined in `src/shared_data.cpp:314-327`. Constants `DOUGLAS_PEUCKER = 0` and
`VISVALINGAM = 1` in `include/shared_data.h:47-48`.

---

## 2. Zoom-Dependent Simplification Scaling

File: `src/tile_worker.cpp:428-451`

Tolerance increases exponentially at lower zoom levels:

```cpp
simplifyLevel *= pow(simplifyRatio, (simplifyBelow - 1) - zoom);
```

With `simplify_ratio: 2.0` (default), each zoom level below the threshold
doubles the simplification tolerance. Example with `simplify_below: 13` and
`simplify_level: 0.0001`:

| Zoom | Tolerance (degrees) |
|------|---------------------|
| 12   | 0.0001              |
| 11   | 0.0002              |
| 10   | 0.0004              |
| 9    | 0.0008              |
| 8    | 0.0016              |

There's also `simplify_length` (in meters) as an alternative to
`simplify_level` (in degrees), converted via `meter2degp()`.

---

## 3. Small Polygon Area Filtering

File: `src/tile_worker.cpp:77-94`

This is probably the single biggest win for ocean tiles. It removes tiny
polygons and inner rings that fall below an area threshold - eliminating
thousands of tiny islands, reefs, and coastal detail fragments that are
invisible at low zoom anyway.

```cpp
void RemovePartsBelowSize(MultiPolygon &g, double filterArea) {
    // Remove polygons with area < filterArea
    g.erase(std::remove_if(..., [&](const Polygon &poly) -> bool {
        return std::fabs(geom::area(poly)) < filterArea;
    }), g.end());

    // Remove inner rings with area < filterArea
    for (auto &outer : g) {
        outer.inners().erase(std::remove_if(..., [&](const Ring &inner) -> bool {
            return std::fabs(geom::area(inner)) < filterArea;
        }), outer.inners().end());
    }
}
```

The area threshold also scales exponentially by zoom (`tile_worker.cpp:441-442`):

```cpp
filterArea = meter2degp(ld.filterArea, latp) * pow(2.0, (ld.filterBelow-1) - zoom);
```

Ocean config example:
```json
"filter_below": 12,
"filter_area": 0.5
```

---

## 4. Feature Count Limits

File: `src/shared_data.cpp:320-325`, enforced at `src/tile_worker.cpp:447`

Hard cap on features per layer per tile:

```json
"feature_limit": 200,
"feature_limit_below": 10
```

```cpp
if (ld.featureLimit > 0 &&
    end - ooListSameLayer.first > ld.featureLimit &&
    zoom < ld.featureLimitBelow)
    end = ooListSameLayer.first + ld.featureLimit;
```

This is a safety net - features beyond the limit are simply dropped.

---

## 5. Polygon Merging (Boolean Union)

File: `src/tile_worker.cpp:345-357`, `src/geom.cpp:150-169`

Adjacent or overlapping polygons in the same layer are merged via boolean union.
Uses an efficient pairwise cascade that doubles the step distance each
iteration, achieving O(n log n) complexity:

```cpp
void union_many(std::vector<MultiPolygon> &to_unify) {
    do {
        half_step = step; step *= 2;
        for (i = 0; i + half_step < to_unify.size(); i += step) {
            MultiPolygon unified;
            boost::geometry::union_(to_unify[i], to_unify[i + half_step], unified);
        }
    } while (step < to_unify.size());
}
```

Configured with `combine_polygons_below` (zoom threshold).

For linestrings, there's a similar merge (`tile_worker.cpp:333-343`) that
connects disconnected linestrings by matching endpoints, with a hard limit of
6000 vertices per merged linestring.

---

## 6. Tile Clipping

### Sutherland-Hodgman Clipping

File: `src/geom.cpp:172-246`

Fast polygon clipping to tile boundaries using bit-coded point positions
(left/right/bottom/top as bits 1,2,4,8). Clips against each edge of the
bounding box recursively, calculating intersection points via linear
interpolation.

### Validation Fallback

File: `src/tile_data.cpp:270-350`

After clipping, validates geometry:
1. Checks for spikes via `geom::is_valid()`
2. If spikes found: `geom::remove_spikes()`
3. If self-intersections found: falls back to Boost `geom::intersection()` (more robust but slower)
4. Runs `geom::correct()` on the result

### Clip Cache

File: `include/clip_cache.h`

Multi-level LRU cache (1024 entries) that reuses clipped geometry from parent
zoom levels. When clipping at zoom z, it first checks if a clip exists at z-1,
z-2, etc., and clips from that instead of the full geometry.

---

## 7. Coordinate Scaling Deduplication

File: `src/coordinates_geom.cpp:36-52`

When scaling geometry to tile coordinates (4096 grid for standard, 8192 for
hires), points that map to the same grid position are deduplicated. Also
backtracks if a point repeats within the last 5 positions. Rings with fewer
than 4 points after scaling are dropped.

---

## 8. Geometry Validation & Correction

File: `include/geometry/correct.hpp`

Multi-stage correction applied throughout the pipeline:
1. Remove points with NaN coordinates
2. Ensure rings are closed (first == last point)
3. Fix winding order (exterior CCW, interior CW)
4. Detect and resolve self-intersections via R-tree, tracing non-intersecting paths
5. Remove degenerate polygons below area threshold

---

## Priority Order for Our Generator

1. **Filter small polygons by area** - Could eliminate 80%+ of features at low zoom. Tiny coastal features are invisible at z10 anyway.
2. **Simplify with Visvalingam** - Better than Douglas-Peucker for natural coastlines.
3. **Scale simplification by zoom** - Exponential scaling (2x per zoom level) is key.
4. **Merge adjacent polygons** - Union overlapping ocean polygons to reduce feature count.
5. **Cap feature count** - Safety net: hard-limit features per tile.
6. **Clip, then simplify** - Clip to tile bounds first, then simplify. Cache clips from parent zooms.
