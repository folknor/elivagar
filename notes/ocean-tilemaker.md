Tilemaker Ocean/Land Pipeline - Structured Report

  1. Data Sources

  Tilemaker uses OSM water polygon shapefiles (e.g. coastline/water_polygons.shp - the standard water-polygons-split-3857 dataset). Configured in JSON:

  "ocean": {
    "source": "coastline/water_polygons.shp",
    "write_to": "water"
  }

  See resources/config-openmaptiles.json and resources/config-coastline.json.

  ---
  2. Background Assumption

  Tilemaker assumes an ocean-colored (blue) background in the viewer. It emits water polygons (ocean areas from the shapefile), not land polygons. The write_to: "water" directive routes these into a water MVT layer with attribute
  class=ocean. The viewer is expected to paint a blue background, then overlay land/other features on top.

  There is no land polygon generation - land is implied by the absence of water.

  ---
  3. The "Island as Concavity" Problem

  Tilemaker preserves holes (interior rings) from the shapefile throughout the pipeline. It does NOT run boolean ops to flatten holes into separate polygons. The flow:

  1. Shapefile parsing (src/shp_processor.cpp:237-250): Ring orientation detected via signed area. CW = exterior ring, CCW = interior ring (hole). Holes are stored as Polygon::inners().
  2. Validity correction (include/geometry/correct.hpp): make_valid() uses a dissolve algorithm to fix self-intersections, but does not eliminate holes.
  3. Clipping uses boost::geometry::intersection() which preserves hole topology.

  So an ocean polygon with an island hole remains a polygon-with-hole through the entire pipeline, including in the output MVT.

  ---
  4. Output MVT Structure

  Tilemaker emits ocean/water polygons (not land). The MVT polygon structure preserves both outer rings and holes:

  // src/tile_worker.cpp:249-261
  void writeMultiPolygon(... MultiPolygon &mp ...) {
      for (const Polygon &poly : mp) {
          writeRing(fbuilder, exterior_ring(poly));  // outer ring
          for (const Ring &ring : interior_rings(poly))
              writeRing(fbuilder, ring);              // holes (islands)
      }
  }

  Output uses vtzero for MVT encoding. Rings with <4 points or zero-length segments are filtered.

  ---
  5. Concavity Resolution Timing

  Concavities (holes) are never explicitly resolved - they pass through as-is. The sequence is:

  1. Load shapefile → holes preserved
  2. Clip to import bounding box (boost::geometry::intersection) → holes preserved
  3. Store in RTree spatial index
  4. Per-tile: simplify → clip → scale → encode MVT → holes still preserved

  There is also a Sutherland-Hodgman fast clipper (src/geom.cpp:172-246, fast_clip()) used for tile-level clipping, but it operates on individual rings.

  When combinePolygons is enabled, boost::geometry::union_() merges adjacent polygons with identical attributes (src/tile_worker.cpp:345-357), but this merges separate polygons, not removes holes.

  ---
  6. Low Zoom vs High Zoom

  Zoom-dependent behavior is configured per-layer via JSON:

  ┌────────────────┬───────────────┬─────────────────────────────────────────────────────────────────────────┐
  │   Parameter    │ Example Value │                                 Effect                                  │
  ├────────────────┼───────────────┼─────────────────────────────────────────────────────────────────────────┤
  │ simplify_below │ 13            │ Simplify geometry at z0-z12                                             │
  ├────────────────┼───────────────┼─────────────────────────────────────────────────────────────────────────┤
  │ simplify_level │ 0.0001        │ Base tolerance                                                          │
  ├────────────────┼───────────────┼─────────────────────────────────────────────────────────────────────────┤
  │ simplify_ratio │ 2             │ Exponential scaling: tolerance *= pow(ratio, (simplify_below-1) - zoom) │
  ├────────────────┼───────────────┼─────────────────────────────────────────────────────────────────────────┤
  │ filter_below   │ 12            │ Filter small polygons at z0-z11                                         │
  ├────────────────┼───────────────┼─────────────────────────────────────────────────────────────────────────┤
  │ filter_area    │ 0.5           │ Minimum polygon area (scaled per zoom)                                  │
  └────────────────┴───────────────┴─────────────────────────────────────────────────────────────────────────┘

  From src/tile_worker.cpp:427-442:
  - At low zoom (z0-z11): aggressive simplification + small polygon filtering. Tiny island holes may disappear entirely because their ring perimeter falls below 3 * max_distance threshold.
  - At high zoom (z13+): no simplification, all polygons kept.

  Two algorithms available: Douglas-Peucker (default) or Visvalingam-Whyatt (simplify_algorithm: "visvalingam" in OpenMapTiles config).

  During simplification, small holes are explicitly dropped (src/geom.cpp):
  if (new_inner.size() > 3 && boost::geometry::perimeter(new_inner) > 3 * max_distance)
      // keep hole

  A clip cache (include/clip_cache.h) avoids redundant re-clipping by caching results keyed by objectID and zoom level.

  ---
  7. Earcut / Hole Handling

  Tilemaker does NOT use earcut - it outputs MVT vector tiles, not triangulated geometry. Holes are emitted as standard MVT interior rings (per the MVT spec, which follows the winding-order convention). The renderer/viewer is responsible
  for triangulation if needed.

  Tilemaker relies on:
  - Boost.Geometry for all polygon boolean ops (intersection, union, validation)
  - vtzero for MVT encoding (which handles ring ordering)
  - The viewer's MVT decoder to correctly interpret exterior vs interior rings

  ---
  Key Files Reference

  ┌────────────────────────────────────┬───────────────────────────────────────────────────────────────┐
  │                File                │                             Role                              │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ src/shp_processor.cpp              │ Shapefile loading, ring orientation, validity, clipping       │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ src/shp_mem_tiles.cpp              │ RTree + bitset spatial indexing of loaded shapes              │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ src/tile_worker.cpp                │ Per-tile simplification, filtering, scaling, MVT output       │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ src/geom.cpp                       │ Simplification (DP/Visvalingam), union, fast_clip             │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ include/geometry/correct.hpp       │ make_valid() dissolve algorithm                               │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ include/coordinates_geom.h         │ TileBbox, coordinate scaling with self-intersection avoidance │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ include/clip_cache.h               │ Cached clipped geometry per zoom                              │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ resources/config-openmaptiles.json │ Layer config with zoom-dependent simplification               │
  ├────────────────────────────────────┼───────────────────────────────────────────────────────────────┤
  │ resources/process-coastline.lua    │ Lua attribute mapping (class=ocean)                           │
  └────────────────────────────────────┴───────────────────────────────────────────────────────────────┘
