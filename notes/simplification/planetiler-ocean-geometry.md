# How Planetiler Tames Ocean Geometry

Analysis of the Planetiler codebase (v0.10.1-snapshot) to understand how it keeps
ocean vector tiles small and renderable.

## The Core Problem

Ocean polygons cover ~70% of Earth's surface. Without optimization, every ocean tile
would contain full coastline detail, producing enormous tiles that choke renderers
like MapLibre GL JS.

---

## Technique 1: Filled Tile Detection (Biggest Win)

**Files:** `TiledGeometry.java`, `FeatureRenderer.java`, `IntRangeSet.java`

The single most impactful optimization. During the stripe-clipping phase, Planetiler
identifies tiles that are **completely interior** to an ocean polygon - no coastline
crosses them at all. These get a tiny canonical fill rectangle instead of real geometry.

- Uses `IntRangeSet` (backed by RoaringBitmap) to efficiently track continuous ranges
  of filled Y-coordinates per X-column
- `VectorTile.encodeFill()` emits a minimal rectangle extending just past the tile
  boundary - not the actual ocean polygon
- `TileArchiveWriter` deduplicates identical fill tiles so they're encoded once and
  reused across millions of ocean tiles

**Impact:** 99%+ size reduction for deep-ocean tiles. A tile in the middle of the
Pacific becomes a trivial rectangle instead of carrying coastline geometry from
thousands of kilometers away.

---

## Technique 2: Geometry Simplification Per Zoom

**Files:** `DouglasPeuckerSimplifier.java`, `VWSimplifier.java`, `FeatureRenderer.java`, `FeatureCollector.java`

Two algorithms available, applied in `FeatureRenderer.accept()` after scaling to tile
coordinates but before clipping:

### Algorithms

1. **Douglas-Peucker** (default) - Recursive; removes points within a distance
   tolerance by finding the furthest point from a line segment. Retains 4+ points for
   polygons to prevent collapse.

2. **Visvalingam-Whyatt** - Removes vertices based on effective triangle area.
   Supports weighted penalty (k=0.7 recommended) for preserving sharp corners.

### Tolerance Values

| Zoom Level     | Tolerance (tile pixels) | Effect                                    |
|----------------|-------------------------|-------------------------------------------|
| Below max zoom | **0.1**                 | Aggressive - removes most coastline detail |
| At max zoom    | **0.0625** (256/4096)   | Preserves detail for overzooming           |

The key insight: **simplification is in tile-pixel space, not world coordinates.** At
zoom 2, a single pixel covers a huge area, so the same 0.1px tolerance removes far
more real-world detail than at zoom 14. This naturally scales detail with zoom level.

Features can override tolerances via `setPixelTolerance()` and
`setPixelToleranceAtMaxZoom()` with per-zoom `ZoomFunction` overrides.

---

## Technique 3: Minimum Feature Size Filtering

**File:** `FeatureRenderer.java` (`renderLineOrPolygon()`)

Features smaller than `minPixelSize` are dropped entirely before encoding:

- Default: **1.0 tile pixels** below max zoom
- Default: **0.0625** at max zoom

Tiny islands, slivers, and micro-polygons vanish at low zooms. For polygons this is
an area check; for lines it's a length check.

---

## Technique 4: Polygon Merging

**File:** `FeatureMerge.java`

Three merging strategies, applicable via `Profile.postProcessLayerFeatures()` per tile:

1. **`mergeMultiPolygon`** - Groups polygons with identical attributes into
   MultiPolygons (Hilbert-curve ordered for spatial coherence). Simple concatenation,
   no topology operations.

2. **`mergeOverlappingPolygons`** - Unions touching/overlapping polygons. Filters by
   minimum area threshold.

3. **`mergeNearbyPolygons`** - Clusters polygons within a distance threshold using
   STRtree spatial index. Uses buffer-union-unbuffer technique to merge nearby
   polygons. Also removes holes smaller than `minHoleArea`.

This reduces feature count by combining adjacent water bodies into fewer, simpler
polygons. Turning 6000 features into a handful.

---

## Technique 5: Stripe Clipping

**File:** `TiledGeometry.java`

Adapted from mapbox/geojson-vt:

- Slices geometry vertically (X strips), then horizontally (Y strips) per tile
- Each tile only contains geometry within its bounds + a configurable buffer (default
  4px)
- `removeDetailOutsideTile()` in `FeatureMerge.java` further strips line segments
  outside the buffered tile area

This ensures a tile at zoom 10 only carries the coastline geometry that actually
intersects it, not the entire ocean polygon.

---

## Technique 6: Tile-Level Compression & Deduplication

- All tiles gzip-compressed (`tile_compression` config) - ocean tiles compress
  exceptionally well due to simple/repeated geometry
- `TileArchiveWriter` checks if tile contents match the previous tile to avoid
  re-encoding for large filled areas (i.e. oceans)
- Archive formats like MBTiles can further deduplicate at storage level

---

## Configuration Summary

From `PlanetilerConfig.java`:

| Parameter                        | Default    | Purpose                                       |
|----------------------------------|------------|-----------------------------------------------|
| `min_feature_size`               | 1.0        | Min tile pixel size below max zoom             |
| `min_feature_size_at_max_zoom`   | 0.0625     | Min tile pixel size at max zoom                |
| `simplify_tolerance`             | 0.1        | Simplification tolerance below max zoom        |
| `simplify_tolerance_at_max_zoom` | 0.0625     | Simplification tolerance at max zoom           |
| `max_point_buffer`               | Infinity   | Max pixels to include points outside tiles     |
| `skip_filled_tiles`              | false      | Skip writing tiles with only polygon fills     |

All of these can be overridden per-feature via `FeatureCollector` methods, and most
support per-zoom `ZoomFunction` overrides.

---

## What This Means for Elivagar

A 660KB / 6000-feature ocean tile at zoom 10 suggests several of these techniques are
missing:

1. **Filled tile detection** - Any tile fully inside the ocean should be a trivial
   fill rectangle, not real geometry. This alone would eliminate most of the problem.

2. **Simplify in tile-pixel space, not world coordinates.** A tolerance of 0.1 tile
   pixels at zoom 10 means points closer than ~15 meters get merged. At zoom 4,
   that's ~2.4 km. The coordinate system matters.

3. **Merge adjacent polygons post-clip.** After clipping ocean geometry to a tile,
   union polygons that touch. This can collapse thousands of features into one or two.

4. **Filter by minimum area.** Drop polygons smaller than 1 square tile-pixel. Tiny
   slivers from clipping artifacts should not survive.

5. **Compress.** Gzip on MVT output is standard and makes a huge difference for
   simple geometry like ocean fills.
