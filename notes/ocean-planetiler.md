  Planetiler Ocean/Land Pipeline Report

  1. Data Sources

  Primary source: water-polygons-split-3857.zip from https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip
  - Pre-split ocean polygons in EPSG:3857 (Web Mercator)
  - Read via ShapefileReader.java
  - Configurable path: water_polygons_path / water_polygons_url in config-example.properties:116-117

  Low-zoom source (Natural Earth): Used in the natural_earth.yml sample schema (planetiler-custommap/src/main/resources/samples/natural_earth.yml):
  - z0-1: ne_110m_ocean
  - z2-4: ne_50m_ocean
  - z5: ne_10m_ocean
  - Read via NaturalEarthReader.java from GeoPackage

  The two sample schemas (owg_simple.yml, shortbread.yml) use water-polygons-split-3857 directly for all zoom levels, with no Natural Earth fallback. Natural Earth is only used in the dedicated natural_earth.yml example.

  2. Background Assumption

  Planetiler makes no background assumption itself. It emits ocean polygons - there is no "land prefill" or "ocean prefill" logic. The viewer/renderer is expected to paint a land-colored background, and ocean tiles paint over it.

  The skip_filled_tiles config option (PlanetilerConfig.java:55,207, default false) can omit tiles containing only polygon fills. This only makes sense if the renderer paints a matching background color - confirming the "land background +
  ocean polygon overlay" model.

  3. The "Island as Concavity" Problem

  Planetiler does NOT run boolean/union operations on the input shapefiles. It relies on the input data (water-polygons-split-3857) already having islands represented as holes (inner rings) in ocean polygons.

  The concavity resolution happens during tile clipping, not before. In TiledGeometry.java:

  - Outer rings (ocean) → addFilledRange(x, yRange) - marks tiles as filled using RoaringBitmap.or() (line ~710)
  - Inner rings (islands/holes) → removeFilledRange(x, yRange) - subtracts from filled set using RoaringBitmap.andNot() (line ~720)
  - Fill detection uses edge-counting: counts vertical edges to left/right; odd count = inside polygon (lines 574-592)

  Special case (line ~613-623): When a hole is the first geometry to intersect a tile (meaning the outer shell completely covered it), the code detects this and adds an implicit fill polygon before the hole geometry, with an assertion that
   the tile was previously marked as filled.

  4. What Gets Emitted

  Ocean polygons only - no land polygons are generated. The MVT output structure:

  - Edge tiles (where ocean meets land): Full polygon geometry with outer rings (CCW) and holes (CW) preserved in standard MVT winding order. Holes ARE kept - VectorTile.java:302-336 reassembles rings by winding direction into
  Polygon(shell, holes[]).
  - Filled tiles (entirely ocean): A single rectangle fill via VectorTile.encodeFill(buffer) at line 481 - a simple MOVE_TO → LINE_TO → LINE_TO → LINE_TO → CLOSE_PATH covering the tile extent.

  In the Shortbread schema, ocean goes into a layer called "ocean" (shortbread.yml:180-183). In OWG Simple, it goes into "water" with attribute natural=water (owg_simple.yml:41-45).

  5. When Concavities Are Resolved

  After clipping to tiles. The TiledGeometry striped clipping algorithm handles outer/inner rings as part of the same slicing pass. There is no pre-processing step that resolves holes. The fill-tracking (filledRanges map) is built
  incrementally as rings are sliced into tile columns and rows.

  6. Low Zoom vs High Zoom

  Planetiler core has no zoom-specific ocean logic. Zoom differentiation is entirely in the schema/profile configuration:

  - The natural_earth.yml example uses progressively detailed NE layers at different zoom ranges
  - The shortbread.yml and owg_simple.yml schemas use water-polygons-split-3857 at all zoom levels with no differentiation
  - Standard polygon simplification (simplify_tolerance) applies at all zooms
  - min_feature_size filtering drops small polygons at low zooms

  7. Earcut and Holes

  Planetiler does not use earcut. It outputs MVT polygons with standard outer/inner ring encoding. Holes are preserved as-is in the MVT output (CW inner rings following CCW outer rings per MVT spec). Triangulation is the renderer's
  responsibility.

  The TiledGeometry fill-tracking correctly handles holes via removeFilledRange, ensuring that tiles completely inside a hole are NOT marked as filled. The filled-tile optimization only applies to tiles with zero edge intersections that
  are fully covered by the polygon (after hole subtraction).

  Key File Reference

  ┌────────────────────────────────────────────────────────────┬───────────────────────────────────────────────────┐
  │                            File                            │                       Role                        │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../reader/ShapefileReader.java            │ Reads water-polygons-split-3857                   │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../reader/NaturalEarthReader.java         │ Reads NE ocean layers                             │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../render/TiledGeometry.java              │ Striped clipping, fill tracking, hole subtraction │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../render/FeatureRenderer.java:290-315    │ Emits filled tiles via emitFilledTiles()          │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../VectorTile.java:481                    │ encodeFill() - rectangle fill geometry            │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../VectorTile.java:302-336                │ Ring reassembly preserving holes                  │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../VectorTile.java:606-646                │ Fill detection and dedup heuristic                │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../archive/TileArchiveWriter.java:273-309 │ Memoization of identical ocean tiles              │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../collection/IntRangeSet.java            │ RoaringBitmap-backed fill range tracking          │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-core/.../config/PlanetilerConfig.java:55        │ skipFilledTiles config                            │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-custommap/.../samples/shortbread.yml:178-183    │ Shortbread ocean layer config                     │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-custommap/.../samples/owg_simple.yml:21-45      │ OWG Simple water layer config                     │
  ├────────────────────────────────────────────────────────────┼───────────────────────────────────────────────────┤
  │ planetiler-custommap/.../samples/natural_earth.yml         │ NE multi-scale ocean example                      │
  └────────────────────────────────────────────────────────────┴───────────────────────────────────────────────────┘
