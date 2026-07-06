# MapLibre rendering saga: postmortem

Frozen record of the three-month MapLibre earcut rendering-artifact
investigation, trimmed to its durable core. The full blow-by-blow
ledger - ~20 attempted fixes (R01-R20) and 9 pre-root-cause
suggestions (S01-S09) chasing the wrong hypothesis - lives in git
history. All of it was obsoleted once R23 found the real bug and the
R21 integer rewrite + R24 emit.rs port deleted the S-H clipping path
those fixes patched.

## Problem statement

MapLibre's earcut tessellation produced garbage triangles from polygon
geometry that rendered correctly in SVG (fill-rule evenodd). For three
months this was pursued as an "ocean" geometry problem, and the MVT
encoder was cleared as innocent - R11 (third-party encoder swap) and
R12 (@mapbox/vector-tile round-trip re-encode) both reproduced the
artifact from our byte stream. Both conclusions were wrong: it was
every OSM polygon layer, and it was the encoder.

## R22 - it was never the ocean layer

The earcut oracle (`scripts/validate/earcut-oracle.mjs`) runs
MapLibre's actual tessellator (earcut) with MapLibre's winding-based
`classifyRings` over every polygon in a PMTiles archive and measures
`earcut.deviation` per polygon. On denmark post-R21 the rewritten
integer ocean path was already clean (1.95M polygons, deviation 0.000,
100% classification), while the OLD emit.rs path was broken on EVERY
OSM polygon layer: water_polygons 2366 polygons over 1% deviation;
land 25309 (z7: 8 features -> 0 renderable, entire layer invisible).
The "ocean rendering" saga was two defects in every OSM polygon layer,
indistinguishable from ocean breakage only because inland water
(Limfjorden, Ringkobing Fjord, Nissum Bredning) is water_polygons, not
ocean.

## R23 - the root cause: ClosePath cursor semantics

`encode_polygon` reset the MVT delta cursor to the ring's MoveTo
position after ClosePath. MVT spec 4.3.3.3: ClosePath does NOT move the
cursor - it stays at the last LineTo vertex. Every ring after the first
in every multi-ring polygon was therefore displaced by
(first_vertex - last_vertex) of the preceding ring in every
spec-compliant decoder (MapLibre, @mapbox/vector-tile, vtzero). Proof:
z2/2/1 ocean feat id=539 hole decoded at (440,1045) in our decoder but
(485,1039) in MapLibre; the delta arithmetic matched the
wrong-reference hypothesis exactly.

Why it survived three months - the durable lesson: our own decoders
(diag, svg, verify, mvt_decode) mirrored the SAME wrong convention, so
every internal round-trip was self-consistent; R12's @mapbox "proof of
innocence" faithfully re-encoded an already-displaced decode; a March
merge.rs "cursor-reset fix" codified the bug rather than fixing it;
vtvalidate checks command validity, not ring nesting. A symmetric
encoder/decoder convention violation is invisible to any single-decoder
round-trip. Only an independent oracle (earcut's own `classifyRings`)
catches it.

Fix (active in the tree): encoder `mvt/mod.rs`, decoders
`geometry/mvt_decode.rs` + `svg.rs` + `main.rs` diag, merge
`mvt/merge.rs`, tests all moved to spec semantics; `mvt/tests.rs` now
enforces "ClosePath does not move the cursor."

## R24 - resolution

Spec 2 (emit-polygon-integer-port) ported all OSM polygon emission onto
the int_ocean integer machinery and fixed ring winding. Result: all
nine polygon layers earcut-clean (0 over-threshold, 0 misattached
holes, worst deviation 0.000, 5.9M+ polygons), verify 1322463 tiles,
human visual QA passed. The 2026-03-08 problem is resolved.

Consequence for this ledger: the port deleted the entire S-H clipping
path and every R01-R20 band-aid built on it - emit.rs S-H polygon
clipping, INTERIOR_TILE_RING, simplify_tile_ring, filter_holes_for_outer,
both hole nudges (R04/R05), ring_is_simple_merc (R19), split_figure8_ring
(R03). Verified 2026-07-06: none survive in `src/`. The only fixes from
the whole saga still in the tree are R23 (ClosePath, encoder + all
decoders) and R13 (u16 -> u32 wire-format geometry-command count, a real
overflow bug that clamped at 65535). Everything else is now architecture
(int_ocean `normalize_into` + `emit_full_tile`), documented in
AGENTS.md/specs, not a "fix."

## Standing gate

The earcut oracle is the permanent regression gate for anything
touching geometry or MVT encoding: 0 deviant polygons, 0 misattached
holes, on every polygon layer, every build. It is the instrument that
caught R23 after every internal validator passed for three months. Run
it (and `feature-probe.mjs` / `winding-probe.mjs` for drilling into an
offender) on any change to geometry or MVT encoding.
