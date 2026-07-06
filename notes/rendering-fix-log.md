# Rendering Fix Log

Historical record of every approach tried for MapLibre rendering artifacts.
Append-only. Each entry has a short ID for cross-reference.

## Problem Statement

MapLibre's earcut tessellation produces garbage triangles from ocean polygon
geometry that renders correctly in SVG (fill-rule="evenodd"). The MVT encoder
is definitively innocent (proven by third-party encoder swap + round-trip
re-encode, both producing identical artifacts).

---

## Approaches Tried

### R01 - ring_is_simple gate (drop non-simple outers)
**Date:** 2026-03-08
**What:** Drop polygon features at z<14 if outer ring has proper crossings.
**Result:** REVERTED. Dropped valid features (Nissum Bredning disappeared at z9-10).
**Commit:** Reverted in cleanup `7fdeb0a`.

### R02 - cleanup_tile_ring (remove micro-edges, spikes, zigzags)
**Date:** 2026-03-08
**What:** Iterative cleanup pass removing consecutive near-duplicates, tiny
spikes, micro-edge zigzags from tile-coordinate rings.
**Result:** ZERO EFFECT. "lol it's fucking identical, this is hilarious"
**Commit:** Reverted in cleanup `7fdeb0a`.

### R03 - split_figure8_ring (split self-intersecting outers at crossing points)
**Date:** 2026-03-08
**What:** Find proper crossings in tile-coordinate rings, compute intersection
point, split into two sub-rings, recurse for multiple crossings.
**Result:** PARTIAL SUCCESS. z4-7 islands (Fyn, Mors) restored via simplified
ocean. z8-14 unchanged. Still in tree but not the primary fix path.
**Commit:** In cleanup `7fdeb0a`, kept as UNCERTAIN.

### R04 - nudge_coincident_hole_vertices
**Date:** 2026-03-08
**What:** Displace hole vertices that share exact coordinates with outer ring
vertices by 1 extent unit toward centroid.
**Result:** UNCERTAIN. Never confirmed to fix a specific visual bug. Low risk.
**Commit:** In cleanup `7fdeb0a`, kept.

### R05 - nudge_hole_off_boundary
**Date:** 2026-03-08
**What:** Nudge hole vertices off clip rect boundary by 1 extent unit inward.
**Result:** UNCERTAIN. Theoretically sound, never confirmed.
**Commit:** In cleanup `7fdeb0a`, kept.

### R06 - gap-fill → proper-clip in ocean.rs
**Date:** 2026-03-08
**What:** Replace full-tile gap-fill rectangles (4096×4096) with proper S-H
polygon clips for gap tiles and no-boundary rows.
**Result:** FIXED northern Jutland. Gap-fill rectangles were covering island
holes from other features in the same tile.
**Commit:** In cleanup `7fdeb0a`, kept.

### R07 - Wagyu/Vatti polygon union (ocean_dissolve.rs)
**Date:** 2026-03-08 / 2026-03-24
**What:** 1,776-line Wagyu port. Tried as post-S-H repair (self-union to split
figure-8s) and as ocean dissolve (merge overlapping features).
**Result:** "really REALLY broken" - amplified defects (755→2042 defects).
**Commit:** Reverted in cleanup `7fdeb0a`.

### R08 - Boolean ops with geo crate
**Date:** 2026-03-08
**What:** Boolean difference (tile_rect minus ocean → land holes) using the
`geo` crate's BooleanOps.
**Result:** BROKEN. "every tile that touches both land and ocean is 50/50
land/ocean, with the tile being two perfect triangles."
**Commit:** Reverted in cleanup `7fdeb0a`.

### R09 - Natural Earth ocean integration
**Date:** 2026-03-08
**What:** Natural Earth shapefiles (ne_110m, ne_50m, ne_10m) for z0-5 ocean.
**Result:** NE data doesn't have small islands at z0-5 scale. Different islands
gone vs before. No net improvement.
**Commit:** Reverted in cleanup `7fdeb0a`.

### R10 - Protobuf field reordering
**Date:** 2026-03-24
**What:** Reorder MVT layer fields from 15,1,5,2,3,4 to 1,2,3,4,5,15
(ascending, matching all other tile generators).
**Result:** NO VISUAL CHANGE. Protobuf spec allows any order; MapLibre's decoder
handles it correctly.
**Commit:** `e1f0862`.

### R11 - Third-party MVT encoder swap (mvt crate)
**Date:** 2026-03-24
**What:** Replace our encoder with the `mvt` crate (Option B - geometry level).
Decode our command sequences, feed coordinates through mvt's GeomEncoder.
**Result:** MUCH WORSE. Third-party encoder produces worse output from same data.
Definitively proves encoder is innocent.
**Commit:** Not committed (feature-gated diagnostic, discarded).

### R12 - Round-trip re-encode (@mapbox/vector-tile → vt-pbf)
**Date:** 2026-03-24
**What:** Decode all 32,667 tiles through MapLibre's own decoder, re-encode
with vt-pbf. 0 decode errors.
**Result:** IDENTICAL ARTIFACTS. Proves the protobuf bytes are correct; the
geometry data itself is what MapLibre can't render.
**Commit:** Not committed (diagnostic script in scripts/validate/).

### R13 - u16 → u32 wire format fix
**Date:** 2026-03-24
**What:** Sort record wire format stored geometry command count as u16, silently
clamping at 65,535. Changed to u32.
**Result:** FIXED 3 malformed tiles (z8/143/73, z8/135/75, z9/287/147). Real
encoder bug but not the rendering issue.
**Commit:** `197f6b3`.

### R14 - i_overlay as primary ocean clipper (replace S-H entirely)
**Date:** 2026-03-24
**What:** Use i_overlay boolean intersection for ALL ocean boundary tile clips.
No S-H at all. Both pre-split and per-tile.
**Result:** z1-9 PERFECT. z10-12 broken. z13 missing tiles. Performance
unacceptable (49-80s ocean vs 1.5s baseline).
**Commit:** `d73732b`, reverted in `b3276ec`.

### R15 - Tile-space DP tolerance cap (z6-10 only)
**Date:** 2026-03-24 (cleanup session)
**What:** `tile_simplify_tol_sq` was unbounded at low zoom (4M at z0). Capped
to z6-10 range, returns 0 outside.
**Result:** Fixed 4 test failures from OOM. Correct behavior.
**Commit:** In cleanup `7fdeb0a`.

### R16 - Post-quantization backtrack dedup (5-point lookback)
**Date:** 2026-03-28
**What:** Port of tilemaker's `scaleRing()`. After to_tile_coords(), check each
new i32 point against previous 5 points, truncate backtrack spikes.
**Result:** NO VISIBLE IMPROVEMENT on its own. Handles A→B→A spikes but not
T-junctions or bridge artifacts.
**Commit:** `ba00c30`.

### R17 - Post-quantization repair (i_overlay integer simplify)
**Date:** 2026-03-28
**What:** Unconditional i_overlay `Simplify` with `FillRule::NonZero` on
quantized i32 tile coordinates. Resolves T-junctions, collinear overlaps,
self-intersections from f64→i32 rounding.
**Result:** FIXED Mors island stability at z7-9. Performance acceptable (16.5s).
Does NOT fix S-H concavity bridges (topologically correct but wrong area).
**Commit:** `fd6570f`.

### R18 - emit_full_tile for pure-ocean tiles
**Date:** 2026-03-28
**What:** When land mask says no land, emit full-tile rectangle instead of
skipping the tile. With land-colored background, ocean tiles must be emitted.
**Result:** FIXED missing ocean tiles at z12+.
**Commit:** `fd6570f`.

### R19 - ring_is_simple_merc (f64 proper-crossing check before quantization)
**Date:** 2026-03-28
**What:** Check S-H f64 output for proper crossings before quantization. If
non-simple, fall back to i_overlay Mercator clip.
**Result:** Only fired on 10 tiles (all z13-14). Did NOT fire for Fyn z7 or
z10-11 coastline artifacts. Reported visual degradation at z6. The S-H bridge
artifact is simple (no proper crossings), so this check can't catch it.
**Commit:** `53cc9fd` (WIP).

### R20 - Bridge detection gate (output-based, segment count on clip edges)
**Date:** 2026-03-28
**What:** Count boundary-running segments per clip edge in S-H output. Trigger
i_overlay fallback when any edge has >1 segment.
**Result:** FIRES TOO BROADLY - 22,000 of 30,000 ocean boundary tiles. Cannot
distinguish real exit-reentry bridges from normal clip-boundary segments.
Performance: 154s (v1), 1494s (v0 with any-match).
**Commit:** `53cc9fd` (WIP, needs replacement).

---

## Suggested But Not Yet Tried

### S01 - Input-crossing-count gate
**Source:** Tilemaker Claude + Tippecanoe Claude (round 4, 2026-03-28)
**What:** Check the INPUT polygon (before S-H) for how many times its edges
cross each clip boundary. If any boundary has >2 crossings, S-H will create a
bridge. O(n), should fire on <5% of tiles.
**Status:** NOT IMPLEMENTED.

### S02 - Row-level i_overlay, tile-level S-H
**Source:** Planetiler Claude (round 4, 2026-03-28)
**What:** Replace S-H row pre-clip with i_overlay (one call per row, correct
topology). Then S-H per-tile on the already-correct ~50-vertex row polygon.
S-H bridges at tile level become rare because major concavities resolved by
row-level i_overlay.
**Status:** NOT IMPLEMENTED.

### S03 - Feed row-preclipped geometry to i_overlay fallback
**Source:** Tilemaker Claude + Planetiler Claude (round 4, 2026-03-28)
**What:** When fallback fires, clip from row-preclipped geometry (~50 verts)
instead of original Mercator polygon (hundreds of verts). 10-100x faster.
**Status:** NOT IMPLEMENTED.

### S04 - Per-tile ocean feature union before encoding
**Source:** Tilemaker Codex + Tippecanoe Codex (round 3+4, 2026-03-28)
**What:** Union all ocean features in a tile before MVT encoding. Fixes
inter-feature overlap (full-tile fill covering another feature's island hole).
**Status:** NOT IMPLEMENTED.

### S05 - Unconditional integer-space topology rebuild (not just simplify)
**Source:** Planetiler Codex + Tilemaker Codex (round 4, 2026-03-28)
**What:** Current `repair_quantized_polygon` uses i_overlay Simplify, which is
too weak for bridge/topology cases. Need a full integer-space boolean/noding
rebuild (self-union or overlay-against-tile-rect in i32 space).
**Status:** NOT IMPLEMENTED.

### S06 - Edge splitting for T-junctions
**Source:** Planetiler Claude (round 2, 2026-03-28)
**What:** Scan for T-junctions (vertex on non-adjacent edge), insert vertex into
that edge. O(n²) but trivial integer math. Handles T-junctions specifically
without full boolean engine overhead.
**Status:** NOT IMPLEMENTED. Noted as potential fast-path alternative.

### S07 - Ring start vertex canonicalization
**Source:** Tippecanoe Codex (round 1, 2026-03-24)
**What:** Rotate each ring so start vertex is at a "far edge" position before DP.
Since DP pins first/last vertex, different start positions produce different
simplified rings. Tippecanoe's `fix_polygon()` does this.
**Status:** NOT IMPLEMENTED.

### S08 - Fill rule change (EvenOdd → NonZero)
**Source:** Tippecanoe Claude (round 1, 2026-03-24)
**What:** Change i_overlay from FillRule::EvenOdd to FillRule::NonZero. MapLibre
uses nonzero winding. Already done in R17 (repair_quantized_polygon uses NonZero).
Clip path still uses EvenOdd.
**Status:** PARTIALLY DONE (repair uses NonZero, clip uses EvenOdd).

### S09 - i_overlay in tile-coordinate f64 space (not Mercator)
**Source:** Tippecanoe Claude (round 4, 2026-03-28)
**What:** Transform to f64 tile coordinates (0-4096 range) before running
i_overlay, instead of Mercator (0-1 range). Better precision for i_overlay's
internal snap-rounding. Avoids the precision issues we saw at z10+.
**Status:** NOT IMPLEMENTED.

---

## Currently Active Fixes (in committed code)

| ID | Fix | Commit |
|---|---|---|
| R03 | split_figure8_ring (partial, z4-7) | `7fdeb0a` |
| R04 | nudge_coincident_hole_vertices | `7fdeb0a` |
| R05 | nudge_hole_off_boundary | `7fdeb0a` |
| R06 | gap-fill → proper-clip | `7fdeb0a` |
| R13 | u16 → u32 wire format | `197f6b3` |
| R15 | tile-space DP cap z6-10 | `7fdeb0a` |
| R16 | backtrack dedup | `ba00c30` |
| R17 | post-quantization integer simplify (NonZero) | `fd6570f` |
| R18 | emit_full_tile for pure-ocean | `fd6570f` |
| R19 | ring_is_simple_merc + i_overlay fallback | `53cc9fd` (WIP) |
| R20 | bridge detection gate (too broad) | `53cc9fd` (WIP) |

### S01-attempt - Input-crossing-count gate (attempted, reverted)
**Date:** 2026-03-28
**What:** Check input polygon for >2 crossings per clip boundary before S-H.
Feed row-preclipped geometry (~50 verts) to i_overlay fallback (S03).
**Result:** MUCH WORSE. Row pre-clip itself uses S-H, so row-preclipped geometry
can already have bridge artifacts from Y-band clip. Feeding damaged input to
i_overlay produces damaged output. 30s performance (acceptable) but visual
regression. Reverted to fd6570f baseline.
**Key learning:** The fallback MUST use un-preclipped original geometry, not
row-preclipped. But un-preclipped is too slow (R14: 49-80s). The performance
vs correctness tradeoff remains unsolved.

## Recommended Next Steps (from round 4 reviewers)

Priority order based on reviewer consensus:

1. **S01** - Input-crossing-count gate. Replace R20. Precise, O(n), low false positive rate.
2. **S03** - Feed row-preclipped geometry to fallback. Fixes the 150s→11s performance gap.
3. **S04** - Per-tile ocean feature union. Fixes inter-feature overlap class.
4. **S05** - Stronger integer repair. Fixes remaining self-touch/pinch cases.

### R22 - Earcut oracle: the problem was never the ocean layer
**Date:** 2026-07-06
**Instrument:** `scripts/validate/earcut-oracle.mjs` - runs MapLibre's actual
tessellator (earcut) with MapLibre's winding-based classifyRings over every
polygon in a PMTiles archive, measuring earcut.deviation per polygon.
**Findings on denmark-5403f33 (post-R21):**
- ocean (rewritten path): 1.95M polygons, deviation 0.000 at every zoom,
  100% ring classification. The R21 architecture is PROVEN earcut-clean.
- water_polygons (old emit.rs path): 2366 polygons over 1% deviation,
  worst 4.87e2; winding misclassification discards most rings
  (z14: 23130 features -> 7897 renderable polygons).
- land (old emit.rs path): 25309 polygons over 1% deviation, worst 1.50e3;
  z7: 8 features -> 0 renderable (entire layer invisible).
**Conclusion:** the three-month "ocean rendering" saga was two defects in
EVERY OSM polygon layer via emit.rs (earcut-hostile geometry + wrong ring
winding), visually indistinguishable from ocean breakage because inland
water (Limfjorden, Ringkobing Fjord, Nissum Bredning) is water_polygons,
not ocean. Next: port emit.rs polygons to int_ocean machinery + fix
winding; oracle (deviation 0, 100% classification, all layers) becomes the
primary gate.

### R23 - THE BUG: ClosePath cursor semantics in the MVT encoder
**Date:** 2026-07-06
**What:** `encode_polygon` reset the delta cursor to the ring's MoveTo
position after ClosePath. MVT spec 4.3.3.3: ClosePath does NOT move the
cursor (it stays at the last LineTo vertex). Every ring after the first in
every multi-ring polygon feature was therefore DISPLACED by
(first_vertex - last_vertex) of the preceding ring in every spec-compliant
decoder - MapLibre, @mapbox/vector-tile, vtzero. Proof: z2/2/1 ocean feat
id=539 hole decodes at (440,1045) in our decoder but (485,1039) in
MapLibre; delta arithmetic matches the wrong-reference hypothesis exactly.
**Why it survived three months:** our decoders (diag, svg, verify,
mvt_decode) mirrored the SAME wrong convention, so every internal
round-trip was self-consistent; R12's @mapbox round-trip "proof of
innocence" re-encoded the already-displaced decode faithfully; the March
merge.rs "cursor-reset fix" (R-era) codified the bug rather than fixing it;
vtvalidate checks command validity, not nesting.
**Fix:** encoder (mvt/mod.rs), decoders (geometry/mvt_decode.rs, svg.rs,
main.rs diag), merge (mvt/merge.rs), tests updated to spec semantics.
**Oracle verdict (denmark-a13222e, faithful maplibre classifyRings):**
ocean 0 over-threshold / 0 misattached across 1.95M polygons (was 37447);
misattached holes drop water_polygons 2700->9, land 16500->352. Remaining
over-threshold polygons (water_polygons 1808, land 22824, buildings 552)
are genuine emit.rs geometry defects - next spec: port emit.rs polygons to
int_ocean machinery.
**Supersedes:** the R11/R12 "encoder definitively innocent" conclusion.
