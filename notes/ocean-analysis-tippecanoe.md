Here's my analysis. The core question is: does water-polygons-split-3857 represent Fyn and Mors as holes (inner rings) in the ocean polygon, or as gaps between polygon pieces?

  OSMCoastline's default is to create holes for free-standing islands. If the data HAS holes, I see one suspicious code path but no obvious smoking gun. If the data does NOT have holes (and islands are just gaps in coverage), then I
  believe I've found the bug.

  Most likely cause: DP simplification collapses the island gap

  If the water polygons don't have inner rings for the islands (some OSMCoastline configurations use --no-holes), the island exists as a concavity or gap in the polygon's outer ring. At low zoom:

  1. The ocean polygon's outer ring follows the coastline around the island
  2. emit_boundary_tile clips to the tile → the concavity around the island is tiny in tile coords
  3. simplify_ring_safe runs DP at 1px tolerance (16 extent units)
  4. The concavity is < 1 pixel → DP removes it → the ring snaps shut, covering the island
  5. ring_is_simple() returns true (the closed polygon is valid, just wrong) → simplified version is used

  At zoom 10+, the concavity is large enough that DP preserves it.

  If the data DOES have inner rings (holes)

  Then the issue is likely multiple overlapping polygon features in the same tile, where one feature has the hole and another doesn't. This can happen because:

  - Each OceanPolygon from the shapefile is processed independently via emit_ocean_polygon
  - If polygon A has a hole for Fyn, and polygon B (a different shapefile record) also covers the Fyn area without a hole, both emit features for the same tile
  - The renderer draws B on top of A, covering the hole

  Debugging steps I'd recommend

  Add diagnostic logging around the Fyn area (approximately lon 10.3°, lat 55.3° → Mercator ~(0.529, 0.385)):

  1. Check whether the shapefile data has inner rings:
  // After parsing, around line 198
  for (i, poly) in polygons.iter().enumerate() {
      if !poly.inners.is_empty() {
          let bb = merc_bbox(&poly.outer);
          if bb.min_x < 0.54 && bb.max_x > 0.52 && bb.min_y < 0.39 && bb.max_y > 0.38 {
              eprintln!("Polygon {i} near Fyn: {} outer verts, {} inners",
                  poly.outer.len(), poly.inners.len());
          }
      }
  }

  2. Check for overlapping polygons at the Fyn position:
  // Count how many polygons' outer rings contain the Fyn centroid
  let fyn_pt = Point::new(0.529, 0.385);
  let covering: Vec<usize> = polygons.iter().enumerate()
      .filter(|(_, p)| point_in_polygon(&fyn_pt, &p.outer))
      .map(|(i, _)| i)
      .collect();
  eprintln!("Polygons covering Fyn: {:?} ({} have holes)",
      &covering, covering.iter().filter(|&&i| !polygons[i].inners.is_empty()).count());

  If covering > 1 and not all have holes, that's the bug: multiple polygons overwrite each other's holes.

  3. Track fill emissions at the Fyn tile:
  // In the gap fill and no-boundary-tile sections, log when emitting near Fyn
  // At zoom 9, Fyn is roughly at tile (271, 165) - verify with your tile math

  What Tippecanoe does differently (and what matters)

  From my analysis of Tippecanoe's codebase, the key difference is architectural:

  ┌────────────────────────────┬─────────────────────────────────────────┬────────────────────────────────────────────────────────┐
  │          Concern           │               Tippecanoe                │                       Your code                        │
  ├────────────────────────────┼─────────────────────────────────────────┼────────────────────────────────────────────────────────┤
  │ Polygon repair             │ Wagyu union fixes self-intersections    │ ring_is_simple fallback only                           │
  ├────────────────────────────┼─────────────────────────────────────────┼────────────────────────────────────────────────────────┤
  │ Tiny polygon handling      │ Accumulates area, emits pixel marker    │ Drops rings < 1px²                                     │
  ├────────────────────────────┼─────────────────────────────────────────┼────────────────────────────────────────────────────────┤
  │ Multiple features per tile │ Each feature is independent; no merging │ Same approach, but multiple ocean polygons may overlap │
  ├────────────────────────────┼─────────────────────────────────────────┼────────────────────────────────────────────────────────┤
  │ Pre-clip simplification    │ DP on Mercator coords, then clip        │ Skipped (good - avoids channel collapse)               │
  └────────────────────────────┴─────────────────────────────────────────┴────────────────────────────────────────────────────────┘

  The critical insight: Tippecanoe's reduce_tiny_poly never drops inner rings (holes) of a large outer ring - see clip.cpp:1103-1106:
  "it is a tiny hole out of a real polygon, which we are still treating as a real geometry because otherwise we can accumulate enough tiny holes that we will drop the next several outer rings"

  Your code has two area checks on inner rings - MIN_RING_AREA (512 = 1px²) at ocean.rs:606 and MIN_HOLE_AREA_2X (2048 = 4px²) in filter_holes_for_outer. These could drop small island holes at low zoom, though Fyn/Mors should be large
  enough to survive them at zoom 9.

  My bet is on diagnosis #1 (no inner rings in data, DP closes the gap) or #2 (overlapping polygons, one without holes). The logging above will tell you which.
