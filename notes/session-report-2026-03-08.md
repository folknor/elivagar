# Session Report: March 8, 2026 (+ continuation March 24)

Full reconstruction of the marathon debugging session. ~12 hours of work across
two sittings, focused on MapLibre rendering artifacts.

---

## Timeline

### 1. Review of pre-session work (12:14)

Session opened with review of ~1,200 lines of unstaged changes accumulated from
prior sessions:
- `diag` CLI subcommand (MVT protobuf decoder, per-ring winding analysis)
- Enhanced `verify` command (end-to-end PMTiles validation)
- SVG grid rendering (`-W`/`-H` for NxM grids, `--layers` filtering)
- `ocean_dissolve.rs` (Wagyu/Vatti polygon union port)
- `debug_check_rings` validation
- MVT cursor-reset fix in merge.rs
- `filter_holes_for_outer` stub

User kept the diag/verify changes, documented them in CLAUDE.md.

### 2. Road segments not connecting (12:18 - 13:05)

**Problem:** At z8/134/78, road segments had visible gaps at junction points.
Short connecting ways between highway sections were missing.

**Root cause #1:** Junction detection was block-local only. Two ways in different
PBF blocks sharing a node wouldn't have that node pinned → DP removed it →
coordinate divergence → gaps. A comment in the code even said: "Limitation:
cross-block shared nodes are intentionally not detected here."

**Fix:** Added `prepass_shared_nodes()` - fast first PBF pass building a global
`FxHashSet<i64>` of all shared node IDs (10M for Denmark). Threaded into workers
alongside per-block annotation. Cost: +6.9s prepass time.

**Result:** Roads connected at z12-14. But still broken at z11 and below.

**Root cause #2:** Short connecting ways were being dropped by
`merc_bbox_is_subpixel`. A 31-extent-unit way at z14 shrank to ~4 units at z11
and was filtered out. The `skip_size_filter` exception for Streets only applied
to `line_is_subpixel` (tile-coord check), not to `merc_bbox_is_subpixel`
(Mercator-space check).

**Fix:** Added `skip_bbox_check` parameter to `for_each_zoom_simplified`, guarded
`merc_bbox_is_subpixel` with layer check for Streets and Boundaries.

**Result:** Streets feature count jumped (z5: 238→2199, z8: 4195→17087). Roads
fully connected at all zoom levels. [x]

### 3. Jagged water polygon at low zoom (13:08 - 13:48)

**Problem:** Ringkøbing Fjord (z8/133/79) - boundary line looked great, but
`water_polygons` had visible staircase artifacts.

**Root cause:** Post-quantization staircase. Sequences like `L3716 2719, L3716
2718, L3716 2717` - 1-pixel Y increments creating zigzags. Mercator-space DP
can't see these (within tolerance in Mercator, but visible after quantization to
4096-unit tile space).

**Fix:** Added `simplify_tile_ring` - Douglas-Peucker on `(i32, i32)` tile
coordinates, runs after `close_and_orient` and before `encode_polygon`. Applied
z≤10, tolerance 16²=256 extent² units (1 rendered pixel). Removed the prior
`polygon_tol_boost` (Mercator-space graduated boost) which had no effect.

Tests disabled via `[lib] test = false` in Cargo.toml because signature changes
broke them. User: "fuck those tests, show me a pmtiles"

**Result:** User checked z10/535/318: "looks very good now I think" [x]

### 4. Fjord polygon "pops" beyond bounds in MapLibre (13:51 - 14:54)

**Problem:** At z9, the Ringkøbing Fjord polygon suddenly "pops" to fill the
entire tile edge. Only happens in MapLibre, NOT in SVGs.

This was the first encounter with the core issue that would dominate the rest of
the session. User was very frustrated:
- "again, I ALSO JUST SAID this doesn't happen in the SVG"
- "you're so confused now, I'm not sure I should trust your question"
- "can you undo the edit you just made please"
- "can you please write up the problem statement as you understand it, in
  excruciating detail"

**Diagnosis journey:**

1. SVG with nonzero fill-rule → identical to evenodd → winding is correct
2. Extracted ring coordinates → ran `ring_is_simple` → **4 self-intersections
   found** in one ring, 3 near-degenerate spikes
3. Root cause: Mercator-space DP at z9 removed too many vertices → self-crossing
4. Added `ring_is_simple` gate → dropped the ring → **zero visual change**
5. Realized the problematic feature was a DIFFERENT polygon (p21, Denmark
   coastline with holes, not p2 the fjord)

**Failed attempt:** `cleanup_tile_ring` (removing micro-edges, tiny spikes) - no
visual effect.

**User's key insight:** "If emit-time ring_is_simple drops had no visual effect,
then the problematic ring is created later or somewhere else entirely."

**Breakthrough:** Dropped ALL multipolygon holes at z≤10. **"Problem is gone!"**

**Root cause (confirmed):** After S-H clipping + quantization, holes escape their
outer ring. Earcut tessellates the escaped hole incorrectly, creating fill that
extends to the tile edge.

**Fix:** `filter_holes_for_outer` - for each polygon, validate each hole against
the outer ring via ray-cast point-in-ring. Drop escaped holes and sub-pixel holes
(<4px² area). Applied z≤10.

**Result:** "it's gone!" [x] No regression for Ringkøbing Fjord.

### 5. Nissum Bredning water missing at z9/z10 (14:40 - 14:54)

**Problem:** Water feature disappears at z9/z10 while visible at z8 and z11+.

**Root cause:** The `ring_is_simple` gate (added to fix the earcut problem) was
dropping valid rings that happened to become self-intersecting from Mercator DP
at intermediate zoom levels.

**Fix:** Removed the `ring_is_simple` gate and `cleanup_tile_ring` from emit
paths (dead weight causing collateral damage). Kept only tile-space DP and hole
filter.

**Result:** Fixed, no regression. [x]

### 6. Re-enabling ocean (14:57 - 15:55)

Applied `filter_holes_for_outer` to `emit_boundary_tile` in `ocean.rs`. Removed
`ring_is_simple` hard-drop from ocean boundary tile emission.

SVGs looked good. MapLibre still broken for some areas. Diagnosed and fixed
Aalborg/Egholm at z10 (was being dropped by leftover ring_is_simple gate).

**Result:** "looks good both!" at z10 [x]

### 7. THE MAIN BATTLE: Islands disappear at z1-9 (15:28 - session end)

**Problem:** Mors, Fyn, Egholm and other Danish islands completely invisible at
z1-9 in MapLibre. **Confirmed visible in SVG.** This is the problem that
dominated the rest of the session and ultimately was not solved.

The core issue: SVG uses `fill-rule="evenodd"` which handles self-intersecting
polygons correctly (overlapping areas cancel). MapLibre uses earcut triangulation
which produces garbage for self-intersecting input.

#### Attempt 1: Raise SPLIT_Z from 8 to 10
Pre-split fragments become smaller (420→5094 polygons), but deep concavities
survive within tiles. **No effect.** Reverted.

#### Attempt 2: Sub-grid fragmentation of boundary tiles
S-H clipping on a 4×4 sub-grid within each tile. Realized immediately: S-H
clipping fills concavities when the clip boundary crosses the channel opening.
**Would make things worse.** Reverted immediately.

#### Attempt 3: Boolean ops with `geo` crate
Wrote `examples/geo_spike.rs` - boolean difference (tile rectangle minus ocean →
land holes). After fixing bugs (wrong layer, cursor-reset), produced correct
SVGs. Very fast (~160µs/tile).

Implemented in ocean.rs as `OCEAN_BOOLEAN_MAX_ZOOM = 9`. Collected rings per
tile, ran boolean diff.

**Result:** "Unfortunately it's quite broken... every tile that touches both land
and ocean is 50/50 land/ocean, with the tile being two perfect triangles."
Switched from difference to intersection approach. z7 near-perfect, z9 Mors
visible, z8 Mors gone. Partial success at best.

Added union step. **Still failed** for Limfjorden (concavity within a single
shapefile polygon). User: "Collapsing Limfjorden at z6 is not acceptable."

**Reverted all boolean ops.**

#### Attempt 4: Land-mask carveout approach
Various land-mask based attempts. All failed.

#### External research phase
User requested three parallel research agents to study how Planetiler, Tilemaker,
and Tippecanoe handle ocean/land rendering. Key findings:

- **Tilemaker:** Assumes ocean blue background. Emits ocean polygons as-is. Earcut
  failures are invisible because incorrect triangulation still shows blue.
- **Planetiler:** Land background. Emits ocean polygons with holes preserved.
  Relies on viewer earcut handling it.
- **Tippecanoe:** Uses Natural Earth data for z0-5, not ocean shapefile. Runs
  Wagyu union post-clip as repair.

**Confirmed rendering convention:** Land-colored background (`#f2efe9`), ocean
polygons drawn on top. No explicit "land fill" layer.

#### Attempt 5: Natural Earth ocean integration
Added `src/natural_earth.rs`, CLI flag `--natural-earth`, NE shapefiles
(`ne_110m_ocean`, `ne_50m_ocean`, `ne_10m_ocean`) for z0-5.

**Result:** "Good news: Jytland is back! Bad news: Mors and Fyn still gone."
NE data doesn't have smaller islands at z0-5 scale. **Reverted.**

#### Key discovery: Gap-fill rectangles (20:04)
A Tilemaker dev identified the real mechanism for SOME of the missing islands:
when multiple pre-split ocean polygons share a tile, a pure-ocean polygon emits
a full-tile gap-fill rectangle (4096×4096) that covers another polygon's properly-
clipped island hole. Both features are in the same tile. The rectangle draws on
top, hiding the island.

**Fix:** Replaced gap-fill rectangles with proper polygon clips in ocean.rs.

**Result:** Northern Jutland restored. Mors and Fyn still missing.

#### Attempt 6: Disable DP simplification in ocean entirely
Removed all simplification. SVGs perfect. MapLibre still broken - too many
vertices, earcut fails on complexity. **Re-enabled DP.**

#### Attempt 7: Wire cleanup_tile_ring at all 8 emit sites
Extended `cleanup_tile_ring` and `filter_holes_for_outer` to all zoom levels.
Added self-intersection fallback to `simplify_tile_ring`.

**Result:** User: "lol it's fucking identical, this is hilarious"

#### Attempt 8: split_figure8_ring (21:41)
New approach: post-S-H repair. Split figure-8 self-intersecting rings at their
crossing points into two proper sub-rings. Also nudge hole vertices that coincide
with outer ring vertices at tile boundaries.

Added to geometry/mod.rs:
- `split_figure8_ring` - finds crossing, computes intersection point, splits
  into two closed sub-rings, recurses for multiple crossings
- `nudge_coincident_hole_vertices` - displaces by 1 extent unit toward centroid

Wired into `emit_boundary_tile` in ocean.rs and both polygon paths in emit.rs.

**Result:** User: **"PARTIAL SUCCESS"** - z4-7 (simplified ocean) now shows Fyn
and Mors correctly. z8-14 (detailed ocean) still missing.

#### Attempt 9: Wagyu self-union repair (March 24 continuation)
Hypothesis: the z8 pre-split clips large polygons using S-H, creating
self-intersecting results that feed into z8-14 processing. Tried using the
existing `ocean_dissolve.rs` (Wagyu/Vatti port) as a post-clip repair.

Modified `union_polygons` to handle single polygons. Applied in
`emit_boundary_tile` and pre-split.

**Result:** User: "okay now we're really REALLY broken!" - **Wagyu made
everything worse.** Immediately reverted.

#### Attempt 10: f64 split_figure8_ring for pre-split
Added Mercator-space versions of `ring_is_simple_merc`, `split_figure8_ring_merc`,
`segments_cross_f64` for the pre-split path.

**Result:** "same as before" - pre-split wasn't actually the issue.

#### Attempt 11: Keep only largest sub-hole after splitting
Discovered that `split_figure8_ring` on a self-intersecting hole produced two
sub-holes sharing the intersection point (1952, 1241). Both kept as separate
holes in the same feature → earcut's bridge algorithm fails on coincident
vertices.

Fix: when splitting a hole, keep only the largest sub-ring (the small one is
always the figure-8 crossing artifact).

Built `denmark-repair4.pmtiles`. **User never confirmed whether this worked.**
Session ended shortly after.

---

## What actually worked (confirmed improvements)

1. **Global shared-node prepass** - road junction gaps fixed at all zoom levels
2. **Skip-bbox-check for Streets/Boundaries** - short connecting ways no longer
   dropped at lower zooms
3. **Tile-space DP (simplify_tile_ring)** - post-quantization staircase artifacts
   fixed at z≤10
4. **Hole containment filter (filter_holes_for_outer)** - escaped holes no longer
   cause earcut pop artifacts
5. **Gap-fill → proper clip in ocean.rs** - northern Jutland restored (gap-fill
   rectangles no longer cover island holes)
6. **split_figure8_ring on outer rings** - z4-7 islands (Fyn, Mors) restored via
   simplified ocean path

## What didn't work / made things worse

1. **ring_is_simple gate** - dropped valid features that happened to self-intersect
   at intermediate zooms (Nissum Bredning)
2. **Boolean ops (geo crate)** - correct in theory, broken in practice (50/50
   triangle artifacts, couldn't handle single-polygon concavities)
3. **Natural Earth integration** - didn't have small islands, couldn't replace
   shapefile for z0-5
4. **Wagyu self-union repair** - "really REALLY broken", amplified defects
5. **Raising SPLIT_Z** - no effect on the actual problem
6. **Sub-grid fragmentation** - would make S-H concavity problem worse
7. **Disabling DP entirely** - too many vertices, earcut chokes on complexity
8. **cleanup_tile_ring at all sites** - zero visible effect

## Unsolved problem

**Islands (Fyn, Mors) invisible at z8-14 in MapLibre.** The data is correct (SVG
renders perfectly, diag shows proper holes). The specific failure:

- Sutherland-Hodgman clipping of concave ocean coastlines produces
  self-intersecting (figure-8) rings
- SVG handles these via `fill-rule="evenodd"` (overlapping areas cancel)
- MapLibre's earcut triangulation cannot handle self-intersecting input →
  produces garbage triangles that cover island areas

`split_figure8_ring` fixes z4-7 (simplified ocean, simpler geometry) but not z8+
(detailed ocean). The z8+ failure appears to be from either:
- More complex self-intersections that `split_figure8_ring` can't handle
- Self-intersecting holes producing sub-holes with coincident split-point vertices
- The pre-split at z8 creating mangled input (unconfirmed - f64 split had no
  effect)

The last attempted fix (keep-only-largest-sub-hole) was never visually confirmed.

## Current state of the working tree

~2,070 lines changed across 21 files. A mix of:
- Confirmed good fixes (road gaps, tile-space DP, hole filter, gap-fill fix)
- Experimental debris (split_figure8_ring wiring, f64 Mercator split functions,
  Wagyu modifications, nudge_hole_off_boundary, cleanup_tile_ring at all sites)
- Tests still disabled (`[lib] test = false`)

The code is in an uncertain state - some of the experimental changes may be
harmless, others may be actively harmful (the user remembers things being
"substantially worse" when they stopped).

## Recommendations for next session

1. **Start clean.** Commit the confirmed-good fixes (road gaps, tile-space DP,
   hole filter, gap-fill→clip) on a branch. Revert everything else.

2. **Verify baseline.** Build from the clean state and confirm: roads connected,
   no staircase artifacts, no earcut pop at Ringkøbing Fjord, northern Jutland
   visible. Fyn/Mors still missing at z8+ (known).

3. **Attack the z8+ problem fresh.** The root cause is confirmed (S-H + concave
   coastlines → self-intersecting rings → earcut failure). Possible approaches
   that were NOT tried:
   - **Weiler-Atherton clipping** - designed for concave polygons, produces
     correct output where S-H fails. Significant implementation effort.
   - **Greiner-Hormann clipping** - another concave-polygon clipper, simpler
     than Weiler-Atherton.
   - **Post-clip earcut-safe repair** - instead of splitting figure-8s (which
     creates coincident vertices), detect self-intersections and emit as
     multi-feature (separate MoveTo commands in MVT) so each piece is simple.
   - **Clipper2 crate** - mature polygon boolean ops library, could replace S-H
     entirely for ocean polygons.
   - **Accept the limitation** - use evenodd fill in MapLibre style layer
     definition (if MapLibre supports it for fill layers).
