  Key Architectural Difference: Planetiler Never Pre-Splits Ocean

  Planetiler processes each ocean polygon at its full original resolution for every zoom level. Your code pre-splits at z8 (SPLIT_Z), then processes all zoom levels (including z0-z7) with the split sub-polygons. This is the most likely
  root cause area.

  The Core Problem: Pre-Split + Scanline Fill at Low Zoom

  After your z8 pre-split (ocean.rs:205-256), each sub-polygon spans exactly 2×2 tiles at z9 and 1×1 tile at z8. Here's the critical geometric issue at z9:

  The z8 tile boundary edges lie exactly on z9 tile boundaries. Your DDA rasterizer (rasterize_segment, line 676) uses floor() to assign tiles:

  let mut cx = x0.floor() as i32;  // line 680

  For a vertical z8 boundary edge at x = 2*stx in z9 coords, the DDA marks tile cx = 2*stx (the tile to the right of the boundary). It does not mark 2*stx - 1. For the right edge at x = 2*stx+2, the DDA marks tile 2*stx+2 - which is
  outside the sub-polygon's 2×2 block.

  This means for your 2×2 z9 tile block [A,B; C,D]:
  - Top edge marks: A, B, and one tile outside
  - Left edge marks: A, C
  - Right edge marks: tiles outside the block
  - Bottom edge marks: tiles outside the block

  Tile D (bottom-right, 2*stx+1, 2*sty+1) gets NO boundary markers from the z8 edges. If the island hole isn't in tile D either, it becomes a gap tile between boundary tiles at x=2*stx (left edge) and x=2*stx+2 (right edge, outside). The
  PIP test at D's center returns true (inside ocean, not inside hole) → fill tile emitted.

  This is correct when D is fully ocean. But the gap structure at z9 is fragile - it depends on the right-edge boundary tile being at x=2*stx+2 (outside the block) to form the gap correctly.

  But the Deeper Issue: Multiple Sub-Polygons in One Tile

  At z5, a z5 tile contains 8×8 = 64 z8 sub-polygons. Each is processed independently. They each emit boundary tiles or fill tiles for the same z5 tiles. The assembly phase (assemble.rs:80-127) confirmed by the agent:

  ALL features are included in the tile. No overwriting occurs.

  And from the disabled code comments:
  // TEMPORARILY DISABLED - testing without all merge passes to isolate
  // rendering corruption (ocean swallowing islands, triangles, missing features).

  So if sub-polygon A (pure ocean, no hole) emits a small ocean polygon for a z5 tile, and sub-polygon B (with Fyn hole) also emits an ocean-with-hole for the same tile - both features appear. The viewer renders both. A's polygon doesn't
  overlap B's (they tile the z8 grid). This should be correct.

  What Planetiler Does That You Don't

  1. No pre-split: Each polygon keeps its full geometry for all zooms. Concavities are never split into potentially-shallow fragments.
  2. Striped clipping with integrated fill tracking (TiledGeometry.java): Instead of rasterize-edges + PIP, Planetiler slices geometry into X-strips then Y-strips, tracking filledRanges (RoaringBitmap) as a side effect. Outer rings add to
  fill ranges, inner rings subtract. This is mathematically exact.
  3. Implicit fill for holes: When a hole is the first thing to intersect a tile (meaning the outer ring fully covered it), Planetiler adds an implicit fill polygon (fill(buffer)) before the hole geometry (TiledGeometry.java:613-623). Your
   code relies on the PIP test instead.
  4. Natural Earth for low zoom: Planetiler's schemas use NE data (ne_110m_ocean, ne_50m_ocean, ne_10m_ocean) for z0-z5, avoiding high-resolution coastline processing entirely at low zoom.

  Recommended Debugging Steps

  1. Skip the pre-split for low zoom levels - this is the quickest test:

  // In emit_ocean_polygon, use original geometry for z < SPLIT_Z
  // Currently line 420-421 uses simp_outer/simp_inners which are
  // the (potentially split) polygon for ALL zoom levels

  Change the pre-split to only replace polygons for z >= SPLIT_Z, or better: process the original unsplit polygon for z < SPLIT_Z and only use the split sub-polygons for z >= SPLIT_Z.

  2. Add diagnostic logging for a specific tile: Pick a z9 tile containing Fyn. Log every SortRecord emitted for that tile_id - is it a fill or a boundary polygon? How many records? From which polygon index?

  3. Check whether Fyn is a HOLE or a CONCAVITY: Add a log in the parsing loop (ocean.rs:166-197) counting inner rings for polygons near Fyn's coordinates. If inners is empty, Fyn is a concavity in the outer ring. If non-empty, it's a
  hole.

  4. Visualize boundary vs gap tiles: For one zoom level that fails (e.g., z7), dump the boundary_tiles set and the gap fills for the polygon containing Fyn. Check whether any gap fill covers Fyn's area.

  5. Consider using Natural Earth for z0-z5: This is what Planetiler schemas do. The water-polygons-split-3857 data has far more detail than needed at z0-z5, and processing it at those zooms is both expensive and error-prone.

  The fastest fix is likely #1: don't use z8-split sub-polygons at zoom levels below z8. Process the original polygon geometry for z < 8.
