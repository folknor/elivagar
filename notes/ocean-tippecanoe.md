Tippecanoe Polygon Handling - Structured Report

  1. Polygon Clipping to Tile Boundaries

  Algorithm: Sutherland-Hodgman, clipping against 4 edges sequentially.

  - clip.cpp:780-877 (clip_poly1): Core clipping function. Clips polygon rings edge-by-edge against tile boundaries (left, right, top, bottom).
  - When prevent_simplify_shared_nodes=true (lines 808-845), intersection points on tile boundaries are recorded in an edge_nodes array and marked as VT_MOVETO with rounded coordinates.
  - These boundary nodes are marked necessary=true in geometry.cpp:219-267 (simplify_lines) to prevent removal during simplification - this ensures adjacent tiles share identical edge geometry.
  - geometry.cpp:188-217 (impose_tile_boundaries): Ensures all line segments crossing tile boundaries have explicit nodes at crossing points using Cohen-Sutherland line clipping.

  Does clipping introduce shared edges between holes and tile rect? Yes - the Sutherland-Hodgman algorithm introduces new vertices where ring edges intersect tile boundaries. These are handled implicitly by the algorithm, and the
  subsequent Wagyu union operation (see §2) resolves any topological conflicts that arise.

  2. Polygon Repair, Simplification, and Boolean Operations

  Yes, Tippecanoe does all three.

  Repair - clip.cpp:260-388 (clean_or_clip_poly):
  - Converts rings to mapbox::geometry::linear_ring<long long>
  - Uses the Wagyu library (mapbox/geometry/wagyu/) to perform a UNION operation (clip_type_union) with fill_type_positive
  - This fixes self-intersections and invalid ring nesting
  - Loops until convergence (while (again))
  - Optional 16x coordinate upscaling for precision on large polygons

  Validation - geometry.cpp:72-145 (check_polygon):
  - Uses snap_round() from mapbox/geometry/snap_rounding.hpp
  - Detects self-intersecting polygons
  - Validates winding order (outer rings positive area, holes negative)

  Simplification - Two algorithms available:
  - Douglas-Peucker: clip.cpp:909-1000 - respects necessary flags on tile boundary nodes
  - Visvalingam: visvalingam.cpp - area-based simplification using min-heap

  Boolean operations:
  - Union: clean_or_clip_poly() for repair (Wagyu)
  - Intersection: clip.cpp:390-450 (clip_poly_poly) for clipping against arbitrary boundaries (Wagyu)
  - Line-polygon intersection: clip.cpp:490-505 (clip_lines_poly) using Clipper2

  3. Large Polygon Handling

  clip.cpp:1926-1986 (coalesce_polygon):
  - Polygons with <100k vertices: processed normally via clean_or_clip_poly
  - Polygons with ≥100k vertices: split in half at a ring boundary, each half cleaned separately, then unioned back together

  Tiny polygon reduction - clip.cpp:1048-1127:
  - Rings smaller than tiny_polygon_size are removed
  - Accumulated area tracked; when threshold exceeded, a pixel marker is created
  - If outer ring removed, its holes are discarded too

  4. Complex Polygon Splitting for Earcut

  No. There is a chop_polygon() function at geometry.cpp:361-436 that would recursively subdivide polygons with >700 vertices, but it is disabled (wrapped in #if 0). Tippecanoe does not split complex polygons into simpler ones for earcut
  compatibility - Wagyu handles them as-is.

  5. Ring and Hole Handling

  - Rings represented as VT_MOVETO (start) + VT_LINETO vertices, closed (first=last point)
  - Outer rings: positive area (shoelace formula, geometry.cpp:606-689)
  - Holes: negative area
  - Area calculation guards against integer overflow by scaling down if intermediate values exceed 2^53
  - Multi-polygon decoding from Wagyu: each polygon has outer ring (index 0) + holes (indices 1+)

  ---
  6. OpenMapTiles Ocean Pipeline (Before Tippecanoe)

  The ocean data goes through three stages before Tippecanoe ever sees it:

  Stage A - OSMCoastline (upstream, at osmdata.openstreetmap.de):
  1. Collects all OSM ways tagged natural=coastline
  2. Assembles into closed rings (land left, water right)
  3. Auto-closes gaps up to --close-distance
  4. Builds water polygons as inverse of land
  5. Recursively splits polygons exceeding --max-points vertices
  6. Adds --bbox-overlap (meters) at split boundaries to prevent rendering seams
  7. Output: water-polygons-split-3857.zip (EPSG:3857 Shapefile)

  Stage B - PostGIS import (openmaptiles-tools/docker/import-data/):
  - import_data.sh uses ogr2ogr to bulk-load the shapefile into table osm_ocean_polygon
  - No additional splitting or simplification at this stage

  Stage C - SQL views (openmaptiles/layers/water/water.sql):
  - Zoom 0-5: Uses Natural Earth data (ne_110m_ocean, ne_50m_ocean, ne_10m_ocean) with progressive ST_Simplify(geometry, ZRes(N))
  - Zoom 6+: Uses osm_ocean_polygon directly (already split by OSMCoastline)
  - ST_Dump() explodes multipolygons into simple polygons
  - Area-based filtering removes small features at low zooms
  - layer_water(bbox, zoom_level) unions all sources filtered by zoom and bbox

  Key OpenMapTiles files:

  ┌──────────────────────────────────────────────────────┬────────────────────────────────────────────────────────────┐
  │                         File                         │                          Purpose                           │
  ├──────────────────────────────────────────────────────┼────────────────────────────────────────────────────────────┤
  │ layers/water/water.yaml                              │ Layer definition (buffer=4, classes: ocean/river/lake/...) │
  ├──────────────────────────────────────────────────────┼────────────────────────────────────────────────────────────┤
  │ layers/water/water.sql                               │ SQL views, layer_water() function                          │
  ├──────────────────────────────────────────────────────┼────────────────────────────────────────────────────────────┤
  │ layers/water/mapping.yaml                            │ imposm3 mapping for OSM water features                     │
  ├──────────────────────────────────────────────────────┼────────────────────────────────────────────────────────────┤
  │ openmaptiles-tools/docker/import-data/import_data.sh │ ogr2ogr import                                             │
  ├──────────────────────────────────────────────────────┼────────────────────────────────────────────────────────────┤
  │ osmcode/osmcoastline                                 │ Upstream splitting/repair tool                             │
  └──────────────────────────────────────────────────────┴────────────────────────────────────────────────────────────┘

  Key insight: By the time Tippecanoe processes ocean polygons, the hardest problems are already solved - OSMCoastline has split continent-spanning polygons into manageable pieces with overlap at seams, and PostGIS has done
  zoom-appropriate simplification. Tippecanoe then handles tile-level clipping (Sutherland-Hodgman), repair (Wagyu union), and simplification (Douglas-Peucker/Visvalingam).

 Crunched for 3m 43s

 What assumptions does tippecanoe make about the map tile viewers background color?

● Searched for 2 patterns (ctrl+o to expand)

● No references to background color in the actual Tippecanoe source code. Let me check if there are any assumptions baked into how it handles polygon rendering, overzoom, or tile defaults.

● Searched for 5 patterns, read 1 file (ctrl+o to expand)

● Tippecanoe makes no assumptions about the viewer's background color. Here's why:

  Tippecanoe is purely a vector tile encoder - it produces MVT (Mapbox Vector Tile) data containing geometry and attributes. It doesn't emit any style information, background color, paint properties, or rendering hints. The background
  color is entirely determined by the map style (e.g., a Mapbox GL style JSON) applied at render time by the viewer.

  That said, there are indirect consequences of its geometry processing that are background-color-relevant:

  1. Tiny polygon reduction (clip.cpp:1048-1127): When polygons are too small to render meaningfully, Tippecanoe accumulates their area and emits a single pixel-sized square once enough area has accumulated. This means tiny polygons don't
  just disappear - they get replaced by a dot. But the dot is still a vector feature styled by the viewer, not a pixel of any particular color.
  2. No "background fill" emission: Tippecanoe doesn't generate background-fill polygons. If you want ocean to be the background color, you either need to include ocean polygons in your tileset, or set the background color in your map
  style. Tippecanoe won't synthesize either approach for you.
  3. The OpenMapTiles convention is relevant here: ocean is rendered as explicit polygon features (class ocean), not as the map background. The map style then independently sets a background color (typically matching the ocean color) as a
  fallback for areas with no tile coverage or outside the data bounds.

  In short: Tippecanoe is style-agnostic. Background color is a rendering concern, not a tiling concern.

 Cooked for 34s

 So the tiles that Tippecanoe outputs cover 100% of the surface, and it's up to the viewer to color each layer.

● Correct. Each tile covers its full rectangular extent in the tile grid. Tippecanoe outputs geometry features (polygons, lines, points) within those tiles - but doesn't fill any "background." Areas with no features are simply empty.

  The viewer/renderer is responsible for:
  - Setting a background color (typically in the style JSON's background layer)
  - Deciding how to paint each feature layer (fill color, stroke, opacity, etc.)
  - Layering order

  So if you have ocean polygons in your tileset, the ocean is visible because the style paints those polygon features blue - not because Tippecanoe colored anything. And land without any polygon coverage just shows the style's background
  color.

  This is why the OpenMapTiles approach works: they set the map style background to ocean blue, then render land polygons on top. Areas with no land polygon show through as "ocean" - even though there's no ocean geometry there at all. The
  explicit ocean polygon layer exists mainly for interactivity and labeling, not strictly for visual coverage.

 you say "Areas with no features are simply empty", what areas would that be? Because if by default it doesnt emit land, that would suggest that Tippecanoe expects the viewer by default to prefill with land color, whatever that is.

● You're right to push back - my statement was misleading. Tippecanoe doesn't "expect" anything about prefill because it doesn't decide what features exist in the first place. It just encodes whatever geometry you feed it.

  The empty areas are simply wherever your input data has no features. Tippecanoe has no concept of "land" or "ocean" - it's geometry-agnostic. If you feed it only building footprints, then 99% of every tile is empty space. If you feed it
  wall-to-wall land cover polygons, nothing is empty.

  So there's no default assumption. The responsibility chain is:

  1. You decide what polygon layers to include (ocean? land? both? neither?)
  2. Tippecanoe encodes them into tiles faithfully
  3. Your style sets a background color and paints features on top

  The "set background to ocean blue, overlay land polygons" pattern is a convention chosen by style authors (like OpenMapTiles), not something Tippecanoe drives or expects. You could equally do the inverse - background as land color,
  overlay ocean polygons - and Tippecanoe wouldn't care either way.
