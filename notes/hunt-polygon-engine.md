# Design reconnaissance: the polygon geometry engine

Scope: `src/geometry/` (mod.rs, int_ocean.rs, pyramid.rs, clip.rs,
simplify.rs, projection.rs, surface.rs, tiles.rs, mvt_decode.rs), the
in-tree boolean engine `src/geometry/overlay/`, and `src/geometry_tests.rs`.
Callers followed where the question led: `src/pipeline/emit.rs`,
`src/ocean.rs`, `src/tile_detail.rs`, `src/mlt.rs`, `src/verify.rs`,
`src/svg.rs`, `src/main.rs`, `src/mvt/merge.rs`.

Headline: the engine is correct-by-discipline, not by construction. Its
central fact - that one `IntPoint` can mean four different coordinate spaces
(base max-zoom pixels, zoom-z global pixels, tile-local pixels, and the
float Mercator unit square that feeds them) - lives only in variable names
(`_base`, `shape_z`, `ox`). The same is true of ring role (outer vs hole),
of "one pixel = 16 units", and of the simplifier parameter. Most of the
findings below are one of those four facts being re-derived at a new site.
The single highest-payoff move is a typed coordinate-space layer plus a
typed tile-local polygon as the engine's output, replacing the MVT command
stream as the internal interchange format.

---

## 1. Axes that should be types

### 1.1 Coordinate space is untyped (the root finding)

`IntPoint`, `Contour`, `Shape`, `Shapes` and `IntRect` are used, unchanged,
in at least three integer spaces:

- base space: global pixels at `maxz` (`quantize_polygon*`, the pyramid
  fragments, `buffered_cell_rect_base`, `buffered_range_rect_base`,
  `PyramidParams::pins`);
- zoom space: global pixels at zoom z (`rescale_shape_pinned_into` output,
  `shape_z`, `buffered_tile_rect`, `thin_window_runs`, `enforce_ring_cap`'s
  rect);
- tile-local space: `(i32, i32)` after `append_translated_ring` subtracts
  `tile_origin`.

The functions that convert between them take the space implicitly:
`buffered_tile_rect(tx, ty)` returns a zoom-space rect,
`buffered_cell_rect_base(maxz, cell)` a base-space rect, both as `IntRect`.
Passing one where the other belongs type-checks and only goes wrong at
zooms below `maxz`, which is exactly where the test geometry is thinnest.
`emit_cell` gets this right today by hand (`enforce_ring_cap(...,
buffered_tile_rect(cell.tx, cell.ty), ...)` against zoom-space `normalized`),
and `build_edge_flags` (base space, shifted buffer) vs `thin_window_runs`
(zoom space, unshifted buffer) compute the same "edge window" in two spaces
with two formulas (see 2.4).

Proposal: a zero-cost `Space` marker - `Pt<S>`, `Ring<S>`, `Poly<S>`,
`Rect<S>` with `S` in `Base<maxz>` / `Zoom` / `TileLocal` - or, cheaper and
almost as good, newtype `BaseShape`, `ZoomShape`, `TileShape` and
`BaseRect`/`ZoomRect`. Conversions become the only functions that change
the tag: `quantize: Merc -> Base`, `rescale(shift): Base -> Zoom`,
`localize(tile): Zoom -> TileLocal`. The overlay engine stays generic over
the tag (it is space-agnostic). This makes `buffered_tile_rect` vs
`buffered_cell_rect_base` unswappable and gives `PyramidCell` a home for
`rect_in(space)`.

The float side has the same gap: `Point` is documented as "Mercator [0,1]"
but `from_epsg3857`, `project_e7`, the antimeridian-unclamped values
quantize relies on (x > 1.0), and test-only `project` all mint it with no
type distinction, and `ClipRect` / `MercBbox` are two float rects for the
same space (`ClipRect::for_tile` vs `merc_bbox`).

### 1.2 Ring role is a positional convention plus a sign convention

Role (outer vs hole) is carried as "index 0 of a `Shape` is the outer, the
rest are holes" plus "outer has positive shoelace area". Neither is a type.
Consequences:

- Every in-place compaction must preserve the index-0 invariant by hand:
  `simplify_shape_dp` orients by ORIGINAL index (`read == 0`) and then
  drops the shape if slot 0 is not >= 3 points; `simplify_shape_vw` handles
  ring 0 separately; `clean_shape_in_place` orients by WRITE index
  (`write == 0`) - so if the outer is dropped for area, the first surviving
  hole is silently re-oriented as an outer. That is a different policy from
  DP's, in the same file. Today `clean_shape_in_place` only runs on overlay
  output where a dropped outer should drag its holes with it in the
  engine's own filter, so the divergence is latent, but nothing enforces it.
- `rescale_shape_pinned_into` uses ORIGINAL index (`i == 0`), and then
  zeroes `used` only if `out[0]` is short - if ring 0 dies and ring 1
  survives, ring 1 lands in slot 0 oriented as a hole (`i == 0` was false)
  and the shape is kept: a hole-wound ring now sits in the outer slot. Every
  downstream consumer re-orients by slot index (`encode_tile_shape` uses
  `i == 0`), so the emitted geometry is a free-standing hole promoted to
  outer. This is worth a targeted test: a base shape whose outer collapses
  under rescale while a hole survives. `rescale_shape` (test-only) has the
  same shape.
- The names for "positive area" disagree: `geometry::signed_area` says
  "positive = counter-clockwise"; `encode_tile_shape` /
  `append_translated_ring` call the outer `clockwise` and keep it positive;
  the overlay is configured `ContourDirection::CounterClockwise`; `diag`
  prints "CW = outer". All are the same convention (positive shoelace in a
  y-down space renders clockwise), named three ways.

Proposal: `struct Polygon<S> { outer: Ring<S>, holes: Vec<Ring<S>> }`
with orientation normalized at construction and a single
`Orientation::{Outer, Hole}` enum used by every orient/classify site. If
the pooled `Vec<Vec<IntPoint>>` layout is load-bearing for the allocation
work, keep it as the storage but expose it only through a type whose
methods own the slot-0 rule.

### 1.3 The simplifier and its parameter are split across two fields

`PyramidParams` carries `simplifier: Simplifier` and `dp_tol: &dyn Fn(u8) ->
i64`. In `Visvalingam` mode the closure's value is ignored except for its
sign (`vw_area_threshold(dp_tol)` returns the constant
`OCEAN_VW_AREA_2X` whenever `dp_tol > 0`). But the same `dp_tol` remains
live in VW mode through two other gates in `emit_cell`:
`dp_tol > 2` decides whether `thin_window_runs` runs (a DP pass at tol 2 -
so the ocean, nominally VW-only, also runs DP on its window runs), and
`dp_tol > 0` gates the convex fast path. So "ocean uses VW" is only
two-thirds true, and the ocean's real behaviour depends on a DP tolerance
(16) whose only visible role is to be > 2.

Proposal: `enum Simplify { None, Dp { tol: PxTol }, Vw { area2x: Area2x } }`
returned by one per-zoom policy function, with the window-thinning and
convex-fast-path eligibility expressed as methods on it
(`simplify.thins_window_runs()`), not as comparisons on a raw i64. That
puts the "seam-deferral layers stay verbatim" rule (the comment on
`dp_tol > 2`) in a named place.

### 1.4 Units travel as bare integers; three 256s mean different things

- `MIN_POLY_AREA = 256` (geometry/mod.rs): area in extent units^2.
- `ocean_min_area` returns literal `256` (ocean.rs): same unit, separately
  spelled, not tied to `MIN_POLY_AREA`.
- `OCEAN_VW_AREA_2X = 256` (int_ocean.rs): DOUBLED triangle area, i.e. a
  true area of 128. Same digits, half the meaning.
- `OCEAN_DP_TOL_PX = 16`, `OSM_DP_TOL_PX = 16` - named `_PX` but in
  extent units (16 units = 1 rendered pixel); `PX = 16` in mod.rs is the
  actual pixel constant and neither tolerance is derived from it.
- `min_area: u64` in the pyramid vs `true_area -> u128` vs
  `signed_area_2x -> i128` vs the overlay's `min_output_area: u64`.

Proposal: `ExtentUnits(i64)`, `Area(u64)`, `Area2x(i128)` newtypes, with
`Px::ONE = ExtentUnits(16)` as the single pixel definition and every
tolerance written as `Px(1.0)`-derived. That kills the `_PX` misnomer and
makes `OCEAN_VW_AREA_2X` vs `MIN_POLY_AREA` unconfusable.

### 1.5 Zoom levels and tile coordinates are bare `u8`/`u32`

`maxz`, `z_top`, `z_bottom`, `cell.z`, and `shift = maxz - z` are all `u8`.
`maxz - z` is computed in at least six places (`emit_cell`,
`buffered_range_rect_base`, `buffered_cell_rect_base`,
`cell_tile_size_base`, `build_edge_flags`, emit.rs
`tile_count_for_base_rect`) and underflows on a bad call;
`valid_params` checks the ordering at the entry but `emit_shape_pyramid_cell`
and helpers trust it. A `ZoomSpan { maxz, top, bottom }` constructed once
(validated) with `shift_for(z) -> Shift` removes the subtraction from every
site. `(z, tx, ty)` is a `PyramidCell` in pyramid.rs but bare
`(u8, u32, u32)` in `PyramidSink`, `tile_filter`, and every caller sink -
the sink should receive a `TileId`/`PyramidCell`.

### 1.6 Pins: three representations of "this vertex must survive"

Merc-space pin keys `FxHashSet<(i64, i64)>` (emit.rs `merc_point_key`),
base-space pins `FxHashSet<(i32, i32)>` (`PyramidParams::pins`), and
per-ring `Vec<Vec<bool>>` flags (`flags_base`, `edge_flags`, `flags_z`)
that must stay index-parallel to the shape. The parallel-vec invariant is
maintained by hand in `push_nonduplicate_pinned`,
`remove_closing_duplicate_pinned`, `orient_ring_pinned` (reverse both),
and the DP/VW `truncate/resize` repair code that silently pads missing
flags with `false` - which would hide a desync rather than surface it.
A `PinnedRing { pts: Vec<IntPoint>, pinned: BitVec }` (or interleaved
`(IntPoint, bool)`) makes desync unrepresentable; the pad-with-false
repair becomes unnecessary.

### 1.7 Failure as prose / as panic

- `partition_shape_to_ring_cap` `expect`s on an indivisible rect - a
  documented "unreachable" that aborts the whole run. Defensible, but a
  typed `EmitError::RingCapIndivisible { cell }` that the producer can
  count and surface would keep one pathological shape from killing a
  planet build.
- `quantize_coord` / `shift_round` / `crossing_point` `expect` on i32
  overflow. Fine as invariants, but the bound is a property of
  `maxz + 12 <= 30` that no type states; `quantize_*` takes any `u8`.
- `enforce_ring_cap` returns `bool` that its only caller discards.
- `decode_mvt_polygon` silently skips unknown commands and returns partial
  rings on truncation, while it is consumed by `verify` - a validator using
  the lenient decoder. `tile_detail` already has a strict decoder; verify
  should use it (see 2.2).

---

## 2. Decisions made in more than one place

### 2.1 Which ring is an outer (role classification) - already disagreeing

Sites that classify a decoded or in-flight ring as outer/hole:

- `tile_detail::decode_polygon` (brokkr's canonical hash): `area > 0` is
  outer, zero-area is a hole; a leading hole opens a component anyway.
- `mlt::group_rings_to_polygons`: self-calibrating - the FIRST ring's sign
  defines "exterior" (MapLibre's classifyRings convention), and zero area
  counts as positive (`>= 0`).
- `ocean.rs` shapefile reader: `w == 0 || signed_area >= 0.0`.
- `multipolygon.rs`: `signed_area < 0.0` / `> 0.0` in two places.
- In-engine: `orient_ring(ring, outer)` by slot, `clip_shape_rect_fast`
  re-orients by hand, `append_translated_ring` re-orients by slot.

`tile_detail` and `mlt` already disagree on (a) a zero-area ring and (b) a
geometry whose first ring is negative. On engine output both cases should
not occur, so the disagreement is latent - but `tile_detail` is the gate's
decoder and `mlt` is a product encoder, so they are exactly the two sites
that must agree on what the engine emitted. Owner: one
`ring_role(area2x) -> Orientation` in the geometry layer (with the zero
case decided explicitly), consumed by all of them; better, MLT should not
re-derive roles at all (see 3.1).

### 2.2 Decoding MVT polygon commands - three decoders

`geometry::mvt_decode::decode_mvt_polygon` (lenient, used by verify,
assemble, tests), `tile_detail` (strict, used by brokkr's gate), and
`mlt::parse_paths`/`decode_polygon_geometry` (Result-returning, own zigzag).
Three answers to "what does this command stream mean", plus three
`unzigzag`s. The doc comment on `mvt_decode.rs` says it is for "trusted
in-pipeline data", yet `verify` - whose job is to distrust - uses it.
Owner: `tile_detail` with its strictness knob, and delete the other two.

### 2.3 Which tiles a base-space rect covers

- `pyramid::tile_range_for_base_rect` + `base_tile_index` (root fragments);
- `emit.rs::tile_count_for_base_rect` + its own `base_tile_index` - a
  verbatim copy, used for the fanout cap that decides `z_bottom`;
- `int_ocean::tile_range_for_rect` + `tile_index` (`#[allow(dead_code)]`,
  used only by pyramid's test reference `collect_direct`);
- float `tiles::clamp_tile` / `for_each_tile_in_bbox` / `is_single_tile`
  for the line/point path.

The fanout cap must count exactly the tiles the pyramid will visit, or the
cap admits a feature whose real fan-out exceeds it. Today they agree by
copy. Note also that neither integer version accounts for the tile buffer:
a shape whose bbox ends within 128 units past a tile edge is emitted into
the neighbour by the buffered descent but not counted by the cap. Owner:
`PyramidCell::cover(rect: BaseRect, span) -> CellRange` in pyramid.rs,
used by emit.rs.

### 2.4 The edge window (which vertices are near a tile edge)

`build_edge_flags` (base space, buffer shifted by `maxz - z`, also flags
segments crossing an edge line) and `thin_window_runs` (zoom space,
unshifted buffer, no crossing test) both decide "is this vertex in the
128-unit window of the cell's tile edges", with separately written
`left/right/top/bottom` arithmetic. Because shift-rounding moves vertices by
up to half a unit, a vertex exactly at the window boundary can be in the
window under one computation and out under the other. Owner: a
`CellWindow` value built once per cell (`window.contains(p)`), in one
space, applied after rescale.

### 2.5 The tile buffer and extent

`TILE_BUFFER_I32 = 128`, `TILE_EXTENT_I32 = 4096` (int_ocean),
`EXTENT = 4096.0` and `BUFFER_FRACTION = 8/256` (geometry/mod.rs),
`mvt::merge::BUFFER_UNITS` derived from `EXTENT * BUFFER_FRACTION`,
`verify::MVT_EXTENT = 4096`, `svg::EXTENT = 4096.0`. The int and float
buffers agree only because 4096 * 8/256 = 128; nothing ties
`TILE_BUFFER_I32` to `BUFFER_FRACTION`. A geometry test pins
`BUFFER_FRACTION`'s value, which is the pairwise-agreement-test pattern:
it catches only the drift it exercises. Owner: one `TileGrid` constants
module (extent, buffer in units, buffer fraction derived) imported by all.

### 2.6 Mercator -> pixel quantization

`int_ocean::quantize_coord` (`round(v * 2^(z+12))`, global) vs
`projection::merc_to_tile_px` and `tiles::to_tile_coords_into`
(`round((v * 2^z - tx) * 4096)`, tile-local). Algebraically equal, but the
float evaluation differs, so a vertex shared by a polygon (int path) and a
line or point (float path) - a boundary way and the landuse polygon it
bounds - can land one unit apart in the same tile. `merc_to_tile_px` and
`to_tile_coords_into` are themselves the same function twice. Owner: one
quantizer, `Merc -> Base` at `maxz`, with the line/point path either moved
onto it or derived from it by `rescale`.

### 2.7 "Is this the verbatim (max) zoom"

`z < 14` hardcoded twice in simplify.rs (`for_each_zoom_simplified`,
`for_each_zoom_simplified_multi`), `OSM_POLYGON_MAX_Z = 14` in emit.rs
(gating `polygon_dp_tol`, `polygon_min_area`, `polygon_z_start`),
`maxz` in `PyramidParams`, and ocean's `max_zoom` argument. The archive's
max zoom is a pipeline-level fact; these sites each hold a copy. Owner:
the `ZoomSpan` of 1.5, constructed from config.

### 2.8 Sub-pixel / minimum-size decisions

`merc_bbox_is_subpixel` (float, diagonal < 1 px, decides `z_start` for
polygons), `line_is_subpixel` (int, diagonal^2 < 256), `MIN_POLY_AREA`
(area, 1 px^2), `contour_area_is_below(r, 1)` in the fast clip (area 1
unit), the overlay's `min_output_area`. Polygons decide their start zoom
on a float bbox diagonal of the unquantized input, then decide survival on
integer area after quantize+simplify. These are different questions but
the "one pixel" behind them is spelled four ways (`1.0 / (256 * 2^z)`,
`PX * PX`, `256`, `16`). Owner: the `Px` unit of 1.4.

### 2.9 Area threshold inclusivity

`clean_shape_in_place` keeps `true_area >= min_area`;
`contour_area_is_below` drops `min_area > 0 && area < min_area`; the port's
extractor applies `min_output_area` with its own comparison (tested to
match: area 4 survives at threshold 4). The single-contour fast path
applies no filter, so `normalize_into` re-filters - a deliberate re-check,
but of a comparison written three times. Low risk; fold into `Area` from
1.4 as `area.meets(min)`.

### 2.10 Signed area / self-intersection, scattered

Shoelace is implemented in `geometry::signed_area` (f64),
`int_ocean::signed_area_2x` (Contour, i128), `signed_area_tile_2x` (tuples,
i128), `tile_detail::signed_area` (i128, closed ring), `mlt::ring_signed_area`
(i64), the overlay test's `ring_area`, and inline in `vw_refresh` and
`is_convex_ring`. `geometry::ring_is_simple` is duplicated verbatim as
`main.rs::diag_ring_is_simple`. These are duplicated helpers more than
duplicated decisions, except where the sign feeds role (2.1).
(Small aside: `ring_is_simple`'s `j + 1 == ring.len() && i == 0` guard can
never be true since `j < len - 1`; harmless because a shared vertex makes a
cross sign zero, but the guard is dead.)

---

## 3. Structure

### 3.1 The engine's output is an MVT command stream; make it a typed polygon

`PyramidSink` is `FnMut(u8, u32, u32, &[u32], PyramidEmitKind)` - the
engine calls `mvt::encode_polygon_ranges` itself (`encode_tile_shape`,
`emit_full_tile`). So geometry depends on the MVT encoder, MVT command
streams become the internal interchange format through the sort, and the
MLT encoder has to DECODE MVT commands back into rings and re-derive roles
(with the different classifier of 2.1). The dependency edge points the
wrong way: the engine should emit `TilePolygon { rings with roles }` in
tile-local space, and the format layer (MVT or MLT) should encode it. The
sort record could carry a format-neutral compact ring encoding (or MVT
commands as one implementation of it), but the role information should
not have to be reconstructed from winding at the far end. This is the
big structural move and it subsumes 2.1 and 2.2.

### 3.2 `int_ocean.rs` is misnamed and does five jobs

It holds: quantization (Merc -> Base), rescale (Base -> Zoom), two
simplifiers (DP, VW) with their scratch, ring hygiene (dedupe, orient,
validate), the overlay facade (`normalize_into`, `intersect_rect_into`),
tile arithmetic (`tile_origin`, `buffered_tile_rect`, dead
`tile_range_for_rect`), and MVT encoding (`encode_tile_shape`,
`emit_full_tile`). The name says ocean; AGENTS.md already says it serves
ALL polygon emission. Suggested split under `geometry/poly/`:
`space.rs` (types + quantize/rescale/localize), `ring.rs` (hygiene,
orientation, area), `simplify.rs` (DP, VW, window thinning - moved out of
pyramid), `boolean.rs` (normalize/intersect facade over `overlay`),
`clip_rect.rs` (the fast S-H rect clip currently in pyramid.rs),
`pyramid.rs` (descent only). The ocean-policy constants
(`OCEAN_DP_TOL_PX`, `OCEAN_VW_AREA_2X`) belong in `ocean.rs` beside
`OCEAN_POLICY_VERSION`, and `OSM_DP_TOL_PX` beside `polygon_dp_tol` in
emit.rs; the engine should not know which producer it serves.

### 3.3 `pyramid.rs` mixes descent with geometry primitives

At ~1200 non-test lines it contains the descent and parallel split, ring-cap
partition, a complete fast rect clipper (`clip_shape_rect_fast`,
`clip_ring_half_plane_multi`, `crossing_point`, `round_div`), convexity
testing, edge-flag building and window thinning. The clipper is a
geometry primitive with its own fallback contract and deserves its own
module and its own property tests against `intersect_rect_into` (it
currently is tested only through the pyramid). Note its doc comment and
`clip_shape_rect_fast`'s still say it falls back to "the full i_overlay
boolean" - it falls back to the in-tree port.

### 3.4 Policy smeared across `PyramidParams` closures and `emit_cell`

Per-zoom policy is two boxed closures (`dp_tol`, `min_area`) plus an enum,
and then `emit_cell` hard-codes further policy on the closure results
(`dp_tol > 2`, `dp_tol > 0`). The producers (emit.rs twice, ocean.rs once)
each build a `PyramidParams` with near-identical boilerplate;
`emit_polygon_feature` and `emit_multipolygon_feature` are ~95% the same
function (pin quantize, z_start, cap, params, sink). Proposal: a
`trait PolygonPolicy { fn at(&self, z) -> ZoomPolicy }` where
`ZoomPolicy { simplify: Simplify, min_area: Area, thin_window: bool }`,
implemented by `OsmPolicy { layer, tol_scale }` and `OceanPolicy`; and one
`emit_osm_polygon(outer, inners, ...)` in emit.rs. The ocean policy then
becomes a single value that can be hashed into the artifact key, which
would make the OCEAN_POLICY_VERSION bump discipline (three separate
"remember to bump" comments in int_ocean.rs, pyramid.rs, overlay/mod.rs)
partly mechanical: a test that hashes `OceanPolicy` and fails when the
hash changes without a version bump.

### 3.5 Dead float polygon machinery still wired in

- `for_each_zoom_simplified_multi` and `SimplifyMultiScratch` are parked
  with no production caller, but the scratch is still threaded through
  relations.rs and emit.rs (`_simp_scratch` parameter) and ~15 pipeline
  tests construct it. The doc comment justifies keeping it "holding the
  path open for re-enabling" - that is a reason for git history, not for
  live plumbing.
- `clip_polygon_into` / `clip_polygon` (float Sutherland-Hodgman) have no
  production caller; AGENTS.md still says "Sutherland-Hodgman for OSM
  layers" under geometry.rs.
- `is_ccw`, `is_cw`, `reverse_ring` (test-only), `point_in_shape`,
  `recycle_shape`, `tile_range_for_rect`, `tile_count_for_rect`,
  `rect_intersection` (`#[allow(dead_code)]`), `intersect_shapes_with_rect`'s
  unused `_min_area`, `enforce_ring_cap`'s ignored return.

Recommend deleting all of it; the float side of `geometry/` then shrinks
to projection, the line clipper, line DP, and label placement - which is
the real seam: `geometry::line` (float, lines/points) vs `geometry::poly`
(integer, polygons) vs `geometry::overlay` (engine).

### 3.6 `geometry/mod.rs` and the glob re-exports

`mod.rs` re-exports everything with `pub use x::*` "so callers don't
change", and itself hosts constants, sub-pixel tests, float shoelace,
Mercator area, point-in-polygon, and the int self-intersection test. The
glob re-export is why `geometry::EXTENT`, `geometry::MIN_POLY_AREA` and
friends are reached from everywhere without anyone owning them. Pre-1.0
with no API constraint: drop the globs and make callers name the module.
`mvt_decode.rs` does not belong in geometry at all (see 2.2).

### 3.7 Tests mirror history, not design

- `geometry_tests.rs` is loaded via `#[path]` from `mod.rs` and covers the
  float side (projection, line DP, clip, area, label point,
  `multi_simplify_*` for the parked cascade, buffer constants, MVT decode)
  plus a "simplify-then-reconcile hypothesis" benchmark citing
  `notes/simplify-then-reconcile-design.md` - a durable file citing
  notes/, which the document rules forbid, for a design that was retired.
- The integer engine's tests live inline in int_ocean.rs under two modules
  named `tests` and `landing1_tests` (a landing name, not a subject);
  `bounds_boolean_skipped_when_bbox_contained` asserts a property of
  `ocean.rs::push_quantized_pieces` from inside int_ocean.
- `landing_b_*` test names likewise encode project history.
- pyramid tests use a test-only `rescale_shape` and `collect_direct`
  reference that reimplements the descent's tiling - itself relying on the
  dead `tile_range_for_rect` - so the reference and the real path share no
  cover function, which is good for independence but means 2.3's drift
  between emit.rs and pyramid.rs is not covered.

Proposal: tests per module (`space`, `ring`, `simplify`, `clip_rect`,
`pyramid`, `overlay` differential), named by property; one property suite
that runs random shapes through quantize -> pyramid -> decode and checks
the invariants (outer positive, holes inside outers, <= 500 rings, no
ring in slot 0 with hole orientation - this would catch 1.2's rescale
case).

### 3.8 Scratch ownership is inverted

`IntEmitScratch` owns the `BoolOverlay` and exposes its pool methods;
`PyramidScratch` owns an `IntEmitScratch` plus its own `frag_pool`, and
reaches into `scratch.int.recycle_*`. Pool discipline (return every
`Shapes` you took) is enforced only by convention across ~20 call sites in
pyramid.rs. An RAII guard (`PooledShapes<'a>` that returns on drop) would
make leaks impossible; with the borrow structure here that likely needs
the pool behind a `RefCell` or an arena index, which is the kind of
rewrite worth doing once the module split of 3.2 is in.

### 3.9 Debug counters inside the engine

`pyramid.rs` imports `crate::debug::RING_CAP` and bumps global atomics.
Geometry depending on the pipeline's telemetry module is the wrong
direction; return a per-call stats struct (or count in the sink) and let
the producer emit counters.

---

## Lateral findings

- Possible bug, worth a test: `rescale_shape_pinned_into` can leave a
  hole-oriented ring in slot 0 when the outer collapses and a hole
  survives (1.2). Encoding then promotes it to an outer.
- `clean_shape_in_place` and `simplify_shape_dp` use different slot-0
  orientation rules (write index vs read index) (1.2).
- The fanout-cap tile count ignores the 128-unit buffer while the descent
  emits into buffered neighbours, so the cap undercounts (2.3).
- `verify` decodes with the lenient decoder (2.2).
- AGENTS.md cites `specs/` ("see specs/") and the brief cites
  `reference/architecture.md`; neither exists in the tree.
- AGENTS.md's module list still says `geometry.rs` uses Sutherland-Hodgman
  for OSM layers; the float S-H polygon clip has no production caller.
- Comments in pyramid.rs still describe the fallback as "the full
  i_overlay boolean"; it is the in-tree port.
- Ocean "VW" mode also runs DP (tol 2) on window runs because
  `thin_window_runs` is gated on `dp_tol > 2` and ocean's `dp_tol` is 16
  (1.3). Possibly intended; nothing says so.
- `emit_multipolygon_feature` takes an unused `_simp_scratch`.
- `PyramidParams` test helper leaks a `Box` per call (`Box::leak`) to get
  `'static` closures - harmless in tests, a symptom of the closure-typed
  policy of 3.4.
