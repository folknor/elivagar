  Tilemaker vs. Your Pipeline: Key Differences

  Architecture

  Tilemaker is tile-centric: for each tile, query an RTree for all intersecting polygons, clip each to the tile, write all clips as separate features. Each clip covers only its polygon's footprint in the tile.

  Your code is polygon-centric: for each polygon, iterate zooms, scanline-fill to find tiles, emit boundary tiles (clipped) or gap fills (full-tile rectangles). Multiple polygons process independently.

  The Critical Difference: Gap Fills

  Tilemaker never emits pre-computed fill rectangles. Every polygon is clipped to every tile it touches. The clip accurately represents the polygon's footprint - no more, no less.

  Your code at ocean.rs:391-394:
  let fill_ring: [(i32, i32); 5] = [(0, 0), (ext, 0), (ext, ext), (0, ext), (0, 0)];
  Gap tiles get a full-tile rectangle covering the entire extent. This is correct for a single polygon, but when multiple polygon pieces share a tile, a gap fill from piece A covers the entire tile - including areas where piece B has a
  hole.

  The Smoking Gun

  At assemble.rs:450-457:
  // TEMPORARILY DISABLED - testing without all merge passes to isolate
  // rendering corruption (ocean swallowing islands, triangles, missing features).

  You've already seen this exact symptom. The merge was disabled but the problem persists. That's because the overlap happens at emission time, not at merge time. Even with separate features per tile, a renderer draws all features - a
  full-tile fill from polygon A covers the hole from polygon B.

  ---
  Root Cause Hypothesis

  At z1-z9, tiles are large enough that multiple water_polygons_split pieces share a tile. The scanline processes each piece independently:

  1. Piece A (pure ocean, no islands): scanline determines some tiles are gap tiles → emits full-tile rectangles
  2. Piece B (ocean with Fyn as inner ring): scanline clips boundary tiles correctly, producing polygons with holes
  3. Both features land in the same tile → the full-tile rectangle from A covers B's hole → island disappears

  At z10+, tiles are small enough that each tile is dominated by a single piece. No conflicting fills.

  ---
  How to Confirm

  Step 1: Add a diagnostic at emit_boundary_tile and the gap-fill path that logs which polygon piece emitted what for specific Fyn/Mors tiles. Something like:

  // At z9, find the tile that should contain Fyn (~10.3°E, 55.3°N)
  // and log every feature emitted for it, including feature_id and whether it has holes

  Step 2: At assembly time, count ocean features per tile. If tiles at z9 have multiple ocean features where one is a full-tile fill and another has rings - that's the bug.

  Step 3: Use your existing debug_check_ocean_layer to log ring counts per feature at z9 tiles covering Fyn. If you see features with 1 ring (full-tile fill) alongside features with 2+ rings (polygon with hole), confirmed.

  ---
  Fixes (Tilemaker's Approach)

  Option 1: Eliminate gap fills entirely. Always clip - even for interior tiles. This is what Tilemaker does. Slower (more S-H work) but correct. Each polygon covers only its actual footprint. No overlap.

  Option 2: Track tile ownership. Before emitting a gap fill, check if another polygon has already claimed that tile as a boundary tile. Requires inter-polygon coordination (hard in the parallel pipeline).

  Option 3: Post-hoc dissolve. Union all ocean features per tile before encoding. You tried Vatti and it was buggy. Tilemaker uses boost::geometry::union_() for its combinePolygons feature - a mature implementation. A correct Rust polygon
  union (perhaps via the geo crate) would fix it.

  Option 4 (cheapest): For gap tiles, instead of a full-tile rectangle, clip the actual polygon to the tile. The scanline still determines WHICH tiles to process, but the fill is a proper clip, not a rectangle. This preserves the
  scanline's O(1) PIP advantage for skipping tiles entirely, while ensuring each feature covers only its polygon's footprint.
