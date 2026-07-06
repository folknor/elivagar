# How Tippecanoe Tames Ocean Geometry (and everything else)

The problem: 6000 features / 660 KB per tile at zoom 10 is too much for MapLibre JS
and other renderers. Tippecanoe prevents this using six complementary strategies.

## 1. Geometry Simplification (point removal)

Two algorithms available, both in `clip.cpp` and `visvalingam.cpp`:

- **Douglas-Peucker** (default): removes points closer than epsilon to the line
  between their neighbors. Stack-based recursive implementation.
- **Visvalingam-Whyatt** (optional): iteratively removes the point that forms the
  smallest triangle with its neighbors. Uses a custom min-heap.

Simplification is **zoom-dependent**. From `geometry.cpp:220-292`:

```
resolution = 1 << (32 - detail - z)
```

At zoom 0, the resolution is coarse so almost everything gets simplified away. At
zoom 14, resolution is fine so detail is preserved. The simplification threshold
scales with this resolution, so ocean coastlines at zoom 3 might keep 50 points
while at zoom 12 they keep thousands.

## 2. Polygon Clipping to Tile Boundaries

This is the single most important thing for the ocean problem. A giant ocean polygon
doesn't get stored whole in every tile -- it gets **clipped to tile extent + buffer**.

- **Algorithm**: Sutherland-Hodgman (`clip.cpp:780-877`)
- **Buffer**: default 5 "screen pixels" (1/256th of tile width) beyond the tile
  edge, preventing seam artifacts
- **Complex cases**: uses **Mapbox Wagyu** library for robust polygon boolean
  operations (handles self-intersections, winding order, holes)
- **Edge tracking**: points created at tile boundaries are marked "necessary" so
  simplification doesn't remove them and create gaps between adjacent tiles

So the ocean at zoom 10? Each tile only contains the **sliver of ocean visible in
that tile**, not the whole Pacific.

## 3. Tiny Polygon Reduction

`clip.cpp:1048-1140` -- polygons smaller than ~2x2 pixels get **replaced with a
single point-sized square**. Multiple tiny polygons accumulate and only emit a marker
when their combined area exceeds the threshold.

Default `tiny_polygon_size = 2` pixels. This alone can eliminate thousands of
features from a tile at low zooms.

## 4. Feature Dropping

When a tile still exceeds limits, tippecanoe has automatic dropping strategies:

| Strategy         | Flag | What it does                                          |
|------------------|------|-------------------------------------------------------|
| Drop densest     | -as  | Removes features in spatially crowded areas           |
| Drop smallest    | -an  | Removes features with smallest geometry               |
| Drop fraction    | -ad  | Removes a proportional fraction of features           |
| Coalesce densest | -aD  | Merges nearby features instead of dropping            |
| Coalesce smallest| -aN  | Merges small features instead of dropping             |

For oceans, **coalescing** is particularly relevant -- adjacent ocean polygons get
merged into one via Wagyu polygon union, then re-simplified.

## 5. Tile Size Limits with Iterative Retry

Hard limits enforced in `tile.cpp:1650-2902`:

- **Max tile size**: 500 KB compressed (configurable with `-M`)
- **Max features per tile**: 200,000 (configurable with `-O`)

When a tile exceeds limits, tippecanoe enters a **retry loop**:

1. Try increasing gamma (drop overlapping points)
2. Try dropping densest/smallest/fraction features
3. Reduce coordinate **detail level** (default 12 -> minimum 7), which makes
   simplification more aggressive
4. Repeat until tile fits

Each retry rebuilds the tile from scratch with stricter parameters.

## 6. Coordinate Quantization

Coordinates snap to a grid determined by the detail level. Detail 12 = 4096 grid
positions per tile axis. At lower detail levels, the grid is coarser, which
inherently merges nearby vertices. This is separate from simplification -- even
without removing points, quantization reduces precision to what's visually meaningful
at each zoom.

---

## What This Means for Elivagar

The 6000-feature, 660 KB tiles at zoom 10 suggest most or all of these are missing.
The highest-impact fixes, in order:

1. **Clip polygons to tile boundaries** -- this is non-negotiable. A zoom-10 tile
   covers a tiny area; don't include geometry outside it. Use Sutherland-Hodgman
   with a small buffer.

2. **Simplify geometry per zoom level** -- at zoom 10 with detail 12, the coordinate
   resolution is `1 << (32 - 12 - 10) = 1024` units. Any vertex displacement
   smaller than that is invisible. Douglas-Peucker with epsilon ~ resolution is a
   good starting point.

3. **Drop or merge tiny polygons** -- anything under ~2 pixels area is visual noise.
   Replace with a point or drop entirely.

4. **Enforce a tile size budget** -- set a hard ceiling and reduce detail iteratively
   until you fit.

5. **Coalesce adjacent ocean polygons** -- if the ocean is split into many features,
   merge them per-tile after clipping.
