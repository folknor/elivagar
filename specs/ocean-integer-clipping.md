# Ocean pipeline rewrite: quantize-early integer boolean clipping

**Contract:** `reference/technical-implementation-spec.md`.
**Spawned from:** `notes/rendering-fix-log.md` (the R01-R20 / S01-S09 ledger),
`notes/post-quantization-repair.md` (six-reviewer consensus on repair placement),
`notes/session-report-2026-03-08.md` (the unsolved problem statement).
**Revision 3** - rev 1 reviewed by four codex xhigh runs (Planetiler,
Tilemaker, Tippecanoe personas + contract review, 2026-07-06); rev 2 fixed:
missed parse-time S-H site, unsafe full-rect gap fills (R06), missing NonZero
winding invariant, rotation-variant DP (S07), `s = 0` rescale, API pins,
LandMask survey, placeholder gates, deferred performance risk. Rev 3 closes
the contract re-review's findings: Shape/Shapes data-flow pinned (piece
model), fast path exactly defined with edge-tile exclusion, Landing 2 builds
its own artifact, relations.rs LandMask sites added, serialization-prep
exemption pinned, `output_direction` pinned, bench gate de-placeholdered.

**Classification:** full coherent rewrite of the ocean geometry path
(`src/ocean.rs` emission + a new integer-geometry module). Not a local change.

---

## 1. Problem and premise

Ocean polygons render with earcut artifacts in MapLibre at specific
zooms/places (Fyn missing at z7, coastline damage at z10-11) even though the
tiles pass `elivagar verify`, vtvalidate, and round-trip re-encoding, and
render correctly as SVG with `fill-rule="evenodd"`.

The premise, established by the ledger and by reading the current code:

1. **Unguarded Sutherland-Hodgman clipping destroys topology for concave
   inputs.** When a coastline exits and re-enters a clip edge, S-H bridges the
   gap and fills the concavity - a simple ring covering the wrong area. S-H is
   not inherently unusable (tilemaker's `fast_clip` is S-H - guarded by
   `is_valid` + Boost-intersection fallback, `research/tilemaker/src/tile_data.cpp:378,398`);
   what is broken here is S-H with no guard and no correct fallback, at FOUR
   places: parse-time bounds clipping (`ocean.rs:172` via `clip_polygon`),
   the z8 pre-split (`ocean.rs:241,245`), the per-row Y-band pre-clip
   (`ocean.rs:468,476`), and the per-tile clip (`ocean.rs:587,595`). Damage
   introduced at any of them survives everything downstream, because:
2. **The only repair step cannot see area-level damage.**
   `repair_quantized_polygon` (i_overlay integer `Simplify`, `ocean.rs:606`)
   resolves T-junctions/self-intersections from f64→i32 rounding - R17, which
   fixed Mors z7-9 - but a bridge-filled ring is topologically clean, so the
   repair keeps it verbatim.
3. **Geometry is mutated after the last topology-safe operation.**
   `nudge_hole_off_boundary` (`ocean.rs:623`) and
   `nudge_coincident_hole_vertices` (`ocean.rs:634`) displace hole vertices by
   one extent unit AFTER the repair, voiding its guarantee. Both are R04/R05:
   "theoretically sound, never confirmed to fix a specific bug."
4. **No simplification exists anywhere in the ocean path.** Pre-clip Mercator
   DP is deliberately skipped (`ocean.rs:403-408`, correct - cascading DP was
   the coastline-collapse bug), and the tile-space DP fallback is dead code.
   Rings are emitted at full shapefile density; session-report Attempt 6
   recorded this exact configuration failing ("too many vertices, earcut
   fails on complexity").
5. **Every gated hybrid failed** (R19, R20, S01-attempt) because the gate
   either misses the damage class or fires on legitimate geometry, and
   because the fallback needs un-preclipped input (slow) - the row pre-clip
   is itself S-H (damaged).

Competitor practice, stated precisely (per-persona review corrections):
Planetiler never boolean-clips per tile - it slices geometry through X then Y
stripes with exact fill-range tracking (`TiledGeometry.java`), so bridges
cannot occur. Tippecanoe quantizes to tile scale first, then runs Wagyu union
with `fill_type_positive` as the LAST geometry operation (`clip.cpp:507,319`).
Tilemaker repairs short backtracks during float→int scaling (`scaleRing`,
floor-based, 4-point lookback, `coordinates_geom.cpp:28-36`), validates
clipped geometry in float space and falls back to Boost intersection when
invalid - its repairs are guards around S-H, not post-quantization topology
rebuilds. `research/stedsplakat` rebuilds ocean from noded linework via JTS
`UnaryUnionOp` + `Polygonizer` and renders artifact-free.

**Design law for the rewrite: after the final topology-establishing
operation (`normalize` / `intersect_rect` output), no code may move, add, or
remove a vertex** - with exactly one pinned exemption, serialization prep
(§3.3: constant-offset translation, ring closing, winding orientation -
operations that do not alter the vertex set's geometry). The MVT encoder's
existing duplicate-skip (`src/mvt/mod.rs:456`) must be a no-op on ocean
input; the geometry-stats instrument (Landing 2) proves it
(consecutive-duplicate count = 0).

## 2. Survey of the ground

### 2.1 Current flow (`src/ocean.rs`, working tree)

```
process_ocean_shapefile
  parse .shp records; per ring: clip_polygon vs data_bounds   [S-H #1  ocean.rs:172]
  pre-split at z8 for polygons ≥500 verts                     [S-H #2  ocean.rs:241,245]
  rayon fold over polygons → emit_ocean_polygon per polygon
    per zoom z = max..min:
      rasterize_ring_edges (f64 DDA) → boundary tile set
      group by row; per row:
        row Y-band pre-clip                                   [S-H #3  ocean.rs:468,476]
        boundary tiles → emit_boundary_tile
        gap runs → PIP test → emit_boundary_tile per tile
        no-boundary rows → PIP → emit_boundary_tile per tile
      land-mask shortcut: !has_land → emit_full_tile (boundary AND gap tiles)
emit_boundary_tile
  per-tile clip                                               [S-H #4  ocean.rs:587,595]
  to_tile_coords (f64→i32, ±128 buffered 4096 space)
  repair_quantized_polygon        [i_overlay integer Simplify, NonZero]
  per output poly: MIN_RING_AREA filter, close_and_orient,
    nudge_hole_off_boundary, filter_holes_for_outer,
    nudge_coincident_hole_vertices                            [post-repair mutations]
  encode_polygon → SortRecord
```

### 2.2 Load-bearing facts

- **Callers:** `process_ocean_shapefile` is called from `pipeline/mod.rs`
  (twice when a simplified shapefile is configured: simplified low zooms,
  detailed high zooms - `min_zoom..=max_zoom` parameterizes this). Signature
  change (dropping `land_mask`) updates both call sites.
- **Output contract:** `SortRecord { key, data }` via rayon-local `OceanAcc`
  chunk flushing. Untouched.
- **`rasterize_segment` DDA** (`ocean.rs:721`): documented corner-case - at an
  exact grid-corner crossing (`t_max_x == t_max_y`) only the Y step is taken,
  skipping a side tile. Unreachable with arbitrary f64 input; REACHABLE with
  integer-valued inputs (exact 45° corner hits). Landing 1 fixes it. Note:
  after this rewrite, boundary-set completeness is an efficiency property,
  not a correctness property - gap tiles clip hole-aware (§3.3), so a missed
  boundary tile yields correct geometry via the clip. Exactness still
  matters: it makes the fast path (§3.3) and PIP-per-run sound.
- **`LandMask` full inventory** (contract-review finding): type
  `geometry/tiles.rs:541`; created `pipeline/phase12.rs:161`; marked per
  polygon feature `phase12.rs:602` and via antimeridian wrapper
  `mark_bbox_wrapped` (`pipeline/emit.rs:83`, called `phase12.rs:1053`);
  threaded through worker signatures (`phase12.rs:235,295,491,509,591,880`);
  unwrapped + logged (`phase12.rs:549-551`); returned (`phase12.rs:575`,
  part of `phase_read_and_process`'s tuple); persisted standalone as
  `land_mask.bin` (`pipeline/mod.rs:214,251-258`) for `--skip-to ocean`;
  loaded `mod.rs:406`; consumed ONLY by `process_ocean_shapefile`
  (`mod.rs:415` passes it; no other reader). Deletion bricks in §3.5.
  `land_mask.bin` is inside gitignored `tmp_dir`; stale copies are inert
  once the load call is gone (`brokkr clean` clears them).
- **S-H (`clip_polygon_into`) has non-ocean users** (`pipeline/emit.rs`,
  seams, tests). It stays. Only ocean call sites move off it.
- **`nudge_*` functions have emit.rs users.** Functions stay; ocean call
  sites are removed. emit.rs semantics out of scope except Landing 4.
- **Upstream data property (used by the §3.3 fast path):**
  water-polygons-split-3857 records are produced by OSMCoastline with
  `--bbox-overlap`: adjacent records duplicate the same coastline geometry in
  their overlap strips (see `notes/ocean-tippecanoe.md` §6 Stage A). Where two
  records both cover an area, they agree on what is water there.
- **Failure-ledger reconciliation:**
  - R06/S04 (full-tile fills covering another feature's island holes): NOT
    re-proposed. Gap/interior tiles emit boolean-clipped geometry
    (hole-aware) exactly like boundary tiles; the only full-rect emission is
    the fast path whose trigger condition (our own shape covers the entire
    row band) plus the upstream duplication property above makes a covered
    foreign hole impossible - if real land existed there, our record would
    carry its coastline and the trigger would not fire.
  - S05 ("i_overlay Simplify too weak"): that objection was raised against
    repairing S-H bridge damage - area-wrong-but-simple rings, which no
    topology operation can fix because they are not topology defects. In this
    design `normalize` never faces bridges (nothing upstream can create one);
    it faces only quantization/DP-induced self-intersections, which a noding
    sweep + fill-rule rebuild (what `Simplify` is - the same engine as
    `overlay`) resolves by construction.
  - S07 (DP result depends on ring start): closed by rotation-invariant
    anchor selection in `simplify_rings_dp` (§3.2).
  - R14 (all-i_overlay, 49-80s): not re-proposed - that variant ran f64
    Mercator booleans per tile on full-polygon inputs. Here booleans are
    integer, per-tile inputs are DP-thinned row bands, and deep-ocean rows
    skip booleans entirely via the fast path. Priced in §4 Landing 3 gate 7.
  - R19/R20/S01 (gates): none exist in this design. R07 (Wagyu port),
    R02/R04/R05 (cleanup/nudges): deleted, not replaced.
- **Dependency state:** i_overlay 7.0.2 (bumped, compiles). Verified API pins
  (contract review, against crate source): `Overlay`, `ShapeType`,
  `OverlayRule`, `FillRule`, `Simplify`, `IntOverlayOptions`, `IntPoint`
  exist. Corrections: i_shape's `IntContour`/`IntShape`/`IntShapes` are
  GENERIC aliases in i_shape 3.0 and i_shape is not a direct dependency -
  the module therefore pins local concrete aliases (§3.2 `Contour`/`Shape`/
  `Shapes` over `IntPoint`) matching the concrete types i_overlay's integer
  API consumes and returns; `min_output_area` requires
  `Overlay::with_shapes_options` / `Overlay::new_custom` (or setting
  `overlay.options`) - plain `with_shapes` uses defaults. `min_output_area`
  compares TRUE area (extract.rs:328), so 256 equals today's
  doubled-shoelace `MIN_RING_AREA = 512`. `output_direction` is pinned
  explicitly (§3.2) because the winding invariant is load-bearing.

### 2.3 Known-bad reference tiles (visual gate targets)

| Defect | Where |
|---|---|
| Fyn missing | z7 ~(67,40); z8 ~(135,80) |
| Mors stability (R17 fixed - must not regress) | z7-z9, z8 ~(134,78) |
| Inland ocean flooding (Tissø) | z10/544/321 - NO ocean layer at all (z10/544/316 in the March notes is mislabeled: that tile is open Kattegat where full-tile ocean is correct) |
| Pure-ocean tiles present (R18 - must not regress) | open-water z12 west of Jutland |
| Coastline artifacts | z10-z11 Limfjorden / west-coast fjords |

## 3. Target structure

### 3.1 Coordinate model

One quantization, as early as possible; everything after is exact i32/i64
integer arithmetic.

- **Base space:** global pixel grid at the run's `max_zoom` (≤14):
  `q = round(merc × 2^(maxz+12))`, range `0..=2^(maxz+12)` (≤ 2^26 - i32-safe;
  i_overlay-internal i64 products safe; DP internals use i128 where products
  of full-span deltas appear). Y is Mercator-y scaled: Y-down, same as tile
  space.
- **Per-zoom derivation** ("snap to max-output grid"): exact shift-round for
  `s = maxz − z`: `q_z = if s == 0 { q } else { (q + (1 << (s-1))) >> s }`.
  Differs from direct per-zoom rounding by ≤1 unit near half-grid thresholds
  (accepted, invisible at 1/16 px); the property purchased is global
  consistency - every tile at zoom z derives shared-edge coordinates from the
  same base values, so cross-tile seams are exact by construction.
- **Winding invariant (NonZero precondition):** `quantize_polygon` orients
  every outer ring to positive doubled-shoelace and every hole to negative
  (in Y-down integer space), using the parse-time outer/hole classification.
  Every subsequent i_overlay call uses `FillRule::NonZero` against inputs
  that satisfy this invariant; outputs of i_overlay calls satisfy it by
  construction (fill-rule extraction), so chained calls stay valid.
- **Tile membership:** `tile(q_z) = q_z >> 12`; tile-local coordinate =
  `q_z − (t << 12)`, range after buffered clipping `−128..=4224`.

### 3.2 New module: `src/geometry/int_ocean.rs`

```rust
use i_overlay::i_float::int::point::IntPoint;

// Local concrete aliases (i_shape's IntContour/IntShape/IntShapes are
// generic aliases in i_shape 3.0; we pin the i32 instantiation locally and
// convert at the i_overlay call boundary, which accepts exactly this shape
// of data - no `i_shape` dependency added to Cargo.toml).
pub(crate) type Contour = Vec<IntPoint>;          // one open ring
pub(crate) type Shape   = Vec<Contour>;           // [outer, holes...]
pub(crate) type Shapes  = Vec<Shape>;             // disjoint polygons

pub(crate) struct IntRect { pub min_x: i32, pub min_y: i32,
                            pub max_x: i32, pub max_y: i32 }

/// Quantize a Mercator-f64 polygon into base-zoom pixel space.
/// - round() per coordinate; consecutive duplicates collapsed;
///   rings degenerating below 3 distinct points dropped.
/// - Enforces the winding invariant: ring 0 positive doubled-shoelace,
///   rings 1.. negative (Y-down space).
/// Output: one contour list [outer, holes...] (open rings, i_overlay style).
pub(crate) fn quantize_polygon(
    outer: &[Point], inners: &[Vec<Point>], maxz: u8,
) -> Shape;

/// Exact shift-round rescale from base zoom to z; s == 0 returns a copy.
/// Collapses consecutive duplicates created by the downshift.
pub(crate) fn rescale_shape(shape: &Shape, s: u8) -> Shape;

/// Rotation-invariant closed-ring Douglas-Peucker in integer space.
/// Anchors: a0 = index of lexicographically smallest (x, y) point (rotation-
/// and start-independent); a1 = point farthest from a0. DP runs on the two
/// chains a0→a1 and a1→a0. The anchors themselves are retained by
/// construction (the OLD ring seam is not privileged - that is the S07
/// closure; the deterministic anchors are). tol is perpendicular distance in
/// pixel units; i128 intermediates (cross products of full-span deltas
/// exceed i64 when squared). Rings shrinking below 3 points are dropped.
pub(crate) fn simplify_shape_dp(shape: &mut Shape, tol: i64);

/// THE topology normalizer: i_overlay Simplify, FillRule::NonZero,
/// options = IntOverlayOptions { min_output_area,
///   output_direction: ContourDirection::CounterClockwise, ..default }.
/// ContourDirection::CounterClockwise is i_overlay's numerically-positive
/// area direction, which in our Y-down space IS the outer-positive winding
/// invariant (§3.1) - pinned explicitly because the invariant is
/// load-bearing, and asserted by a unit test on output.
/// min_output_area = 0 in base space, 256 in per-zoom space (1 rendered
/// px², = today's MIN_RING_AREA 512 in doubled-area terms).
/// Output: simple polygons with correctly nested holes.
pub(crate) fn normalize(shape: Shape, min_area: u64) -> Shapes;

/// Boolean intersection of ONE shape with an axis-aligned rect:
/// Overlay::with_shapes_options(subj = &[shape], clip = &[rect_shape],
/// options as in normalize) → overlay(OverlayRule::Intersect,
/// FillRule::NonZero). A concave shape crossing the rect edge twice yields
/// multiple disjoint output shapes - the case S-H bridges.
pub(crate) fn intersect_rect(shape: &Shape, rect: IntRect, min_area: u64) -> Shapes;

/// Integer ray-cast PIP on a shape (outer minus holes), exact.
pub(crate) fn point_in_shape(x: i32, y: i32, shape: &Shape) -> bool;

/// Constant: ocean DP tolerance, 1 rendered pixel = 16 pixel units.
/// (Planetiler defaults to 0.1 tile px for generic layers; 1 px is the
/// deliberate ocean choice here - coastline detail below 1 rendered px is
/// noise. The visual gate (Landing 3 gate 6) is the arbiter; this is a code
/// constant, not a runtime knob.)
pub(crate) const OCEAN_DP_TOL_PX: i64 = 16;
```

### 3.3 Rewritten flow

Piece model: every structural cut (bounds, pre-split) returns `Shapes`; the
result is FLATTENED - each output `Shape` becomes an independent piece
(`OceanPolygon` is replaced by `Shape` in base space). Emission consumes one
`Shape` at a time; there is no multi-shape ambiguity downstream.

```
process_ocean_shapefile
  parse .shp records (unchanged byte-level parsing; bbox prefilter stays)
  quantize_polygon at maxz → Shape                  [f64 → int, ONCE; winding enforced]
  intersect_rect(&shape, data_bounds_rect_in_base_space, 0)
    → flatten Shapes into pieces                         [replaces S-H #1]
  pre-split at z8: intersect_rect per z8 rect, min_area 0
    → flatten into pieces (vertex threshold unchanged: ≥500)  [replaces S-H #2]
  rayon fold → emit per piece (one Shape in base space)
    per zoom z = max..min:
      shape_z = rescale_shape(&piece, maxz − z)
      simplify_shape_dp(&mut shape_z, OCEAN_DP_TOL_PX)
      shapes = normalize(shape_z, 256)                [topology established HERE]
      per shape in shapes:
        rasterize edges (exact DDA, Landing 1) on the q_z/4096 grid → boundary tiles
        per row (rows with any bbox overlap):
          row_shapes = intersect_rect(&shape, row band ± 128, 256)
          FAST PATH (per row; see definition below): interior tiles emit
            emit_full_tile with zero booleans
          otherwise, per shape S in row_shapes:
            boundary tile → tile_shapes = intersect_rect(&S, tile ± 128, 256)
                            → translate to tile-local (subtract t << 12)
                            → serialization prep + encode_polygon → SortRecord
            gap runs / interior rows (PIP-inside via point_in_shape on S) →
              same intersect_rect path per tile           [replaces S-H #3+#4;
                                                            hole-aware: closes R06/S04]
emit_full_tile                                       (unchanged)
```

FAST PATH definition (exact, testable): let `R = bbox(shape) ∩ band`, both
in q_z pixel units, where `band` is the buffered row rect. The fast path
fires iff `row_shapes` is exactly one shape containing exactly one 4-vertex
contour whose vertex SET equals the four corners of `R` (rotation/direction
independent set comparison). When it fires: (a) the shape has no edges
inside R, so the row's boundary-tile set restricted to R must be empty -
`debug_assert` that; (b) `emit_full_tile` is emitted for every tile `t` in
the row whose BUFFERED rect `tile ± 128` is contained in `R`; (c) the ≤2
tiles per row edge whose buffered rect exceeds `R` take the normal
`intersect_rect` path. No PIP is needed on fast-path rows (containment in R
implies interior).

Serialization prep (the pinned exemption to the design law): after
`intersect_rect` output, exactly three operations run before encoding -
integer translation to tile-local coordinates (constant offset), ring
closing (duplicating the existing first vertex), and winding orientation
per MVT ring role (`close_and_orient_cw`/`_ccw` by ring index, which may
reverse vertex ORDER). None alters the vertex set's geometry; nothing else
is permitted.

Buffer membership: gap tiles clip against `tile ± 128`, so coastline slivers
within the buffer of an adjacent interior tile are included exactly as today
(today's gap tiles S-H-clip with the same buffered rect). Tiles entirely
outside the polygon get nothing, as today. No regression class.

### 3.4 Deletions (ocean path)

All four S-H call sites; `to_tile_coords` usage; the `repair_quantized_polygon`
call (delete the function too if emit.rs count of references is zero - it is
today); both nudge call sites; `filter_holes_for_outer` call site; manual
`MIN_RING_AREA` filter (subsumed by `min_output_area`); `simplify_ring_safe`
(dead); `emit_boundary_tile` in its current form; the row S-H buffers.

### 3.5 LandMask deletion bricks (complete, from §2.2 inventory)

1. `ocean.rs`: drop `land_mask` param + both mask-gated branches (its
   boundary-tile branch is a live bug - coastline tiles over unmapped land
   get flooded; the fast path in §3.3 replaces its legitimate gap-tile use).
2. `pipeline/mod.rs`: drop `LAND_MASK_FILE`, `save_land_mask`,
   `load_land_mask`, the `mask`/`mask_ref` plumbing at 379-415.
3. `pipeline/phase12.rs`: drop creation (161), Arc clones + worker-signature
   params (235, 295, 491, 509, 591, 880), `mark_bbox` (602),
   `mark_bbox_wrapped` call (1053), unwrap + log (549-551), tuple slot (575, 87).
4. `pipeline/emit.rs`: drop `mark_bbox_wrapped` (83-91).
5. `pipeline/relations.rs`: drop the `mark_bbox_wrapped` import, the
   `&geometry::LandMask` params, and the three call sites (309, 361, 386)
   (missed in rev 1's survey; found by contract re-review).
6. `geometry/tiles.rs`: drop `LandMask` + its tests.
`--skip-to ocean` compatibility: `land_mask.bin` simply stops being
written/read; stale files in `tmp_dir` are inert. No format versioning exists
or is needed.

## 4. Landings (keep/revert units, ordered green)

Artifact path convention used by every gate below: `brokkr tilegen --dataset
denmark` renames its output to `data/tilegen_tmp/denmark-$(git rev-parse --short
HEAD).pmtiles`. Every landing is a single commit; revert = `git revert` of it.

### Landing 0 - deps + lint green (in flight)

i_overlay 7.0.2 etc., dead-code deletion, cast fixes. Gates:
```
brokkr check
brokkr tilegen --dataset denmark
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles
```
Commit. Then record the pre-change baseline (host: plantasjen; hash: this
commit - the numbers the Landing-3 verdict is read against):
```
brokkr tilegen --bench --dataset denmark
```
`ocean_ms`, total wall (`total_ms`), and peak RSS are printed in the bench
output's kv summary and stored in results.db; the Landing-3 comparison gate
is `brokkr results --compare-last` (no UUID needed). Record the numbers in
the Landing-3 section of `notes/rendering-fix-log.md` entry R21 when it is
written.

### Landing 1 - exact integer rasterization

`rasterize_segment` tie-handling: when `t_max_x == t_max_y`, mark BOTH
side cells (`(cx+step_x, cy)` and `(cx, cy+step_y)`) before stepping
diagonally. Unit tests (expanded per Tippecanoe review): 45° through exact
corners marks both side cells; segment starting exactly on a corner; segment
ending exactly on a corner; axis-aligned segment running along a grid line;
tangent corner touch; zero-length; negative coords. Over-marking is safe
(a clipped uncrossed tile yields its correct geometry); under-marking is the
bug. Gates:
```
brokkr check
brokkr tilegen --dataset denmark
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles
```

### Landing 2 - geometry-stats instrument

`elivagar verify <FILE> --geometry-stats`: per zoom, for the ocean layer:
feature count, ring count, max and p99 ring vertex count, count of
consecutive duplicate vertices, count of full-tile rectangle features.
This is the instrument for the density premise (Tilemaker review: "tens not
thousands" must be measured, not assumed) and for the design law (duplicate
count must read 0 after Landing 3). This spec is justified by correctness,
not by an estimated volume, so per contract no proceed/close threshold
gates Landing 3 on these readings - they are the before/after evidence and
the post-rewrite duplicate-count gate. Gates (this landing builds its own
artifact so the path matches its own commit):
```
brokkr check
brokkr tilegen --dataset denmark
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles --geometry-stats
```
(the stats output records the BEFORE density numbers - the ocean geometry at
this commit is still the old path; paste into the R21 ledger entry).

### Landing 3 - the rewrite

One coherent change: `src/geometry/int_ocean.rs` (new; §3.2) + `src/ocean.rs`
emission rewrite (§3.3) + deletions (§3.4) + LandMask removal (§3.5) +
`pipeline/mod.rs` call-site adjustment.

Unit tests (named bricks, all in int_ocean.rs tests or ocean.rs tests):
- `quantize_polygon`: winding enforced for misordered input; duplicate
  collapse; ring collapse below 3 points dropped.
- `rescale_shape`: `s = 0` identity; `s > 0` within ±1 of direct
  quantization; duplicate collapse after downshift.
- `simplify_shape_dp`: rotation invariance (rotated copies of the same
  ring produce the identical simplified vertex set); staircase (1-unit
  zigzag) collapses at tol 16; the old ring seam not privileged (a spike at
  the input ring's start/end is removed like any other vertex).
- `normalize`: figure-8 input → two simple polygons; same-winding
  outer+hole input is corrected by quantize (test the invariant, not
  normalize's tolerance of its violation); sub-min-area contour dropped.
- `intersect_rect`: concave polygon crossing a rect edge twice → two
  disjoint output polygons (THE S-H bridge case, as a unit test); hole
  entirely inside the rect → rect-outer + hole output (Planetiler's
  implicit-fill case); hole partially crossing the rect; output winding
  satisfies the invariant.
- fast path: row_shapes exactly the single 4-corner rect of
  `bbox(shape) ∩ band` → fires; one vertex displaced by 1 unit → does not
  fire; extra contour (hole) → does not fire; when fired, edge tiles whose
  buffered rect exceeds R take the boolean path (containment condition
  unit-tested); rotated/reversed corner order still fires (set comparison).
- `point_in_shape`: inside outer, inside hole, on-boundary determinism.

Gates, in order (all copy-pasteable):
```
brokkr check
brokkr tilegen --dataset denmark
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles --geometry-stats
elivagar diag data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles -z 10 -x 544 -y 321
mkdir -p notes/qa
elivagar svg data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles -z 7 -x 66 -y 39 -W 3 -H 3 -l ocean -o notes/qa/z7-fyn-mors.svg
elivagar svg data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles -z 8 -x 133 -y 77 -W 4 -H 4 -l ocean -o notes/qa/z8-fyn-mors.svg
elivagar svg data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles -z 11 -x 1076 -y 632 -W 4 -H 3 -l ocean -o notes/qa/z11-limfjorden.svg
```
Pass criteria: verify exits 0 with zero errors; geometry-stats shows ocean
consecutive-duplicate count 0 and z10-14 max ring vertices ≤ 4096 (sanity
bound; record actuals); diag shows no ocean layer at the true Tissø tile (z10/544/321); SVGs
show Fyn/Mors as land with continuous coastline.

Human gate (the one non-command gate): MapLibre pass over §2.3 - Fyn z6-z8,
Mors z7-z9, Limfjorden z10-z11, open-water z12+. Correct = islands visible
as land, no triangle spray, no missing water tiles. Failure = revert this
landing or re-spec; never patch-in-place.

Performance gate, AFTER the commit (benchmark discipline):
```
brokkr tilegen --bench --dataset denmark
brokkr results --compare-last
```
Accepted cost bound (the keep/revert verdict, per contract stance on paying
throughput for capability): `ocean_ms ≤ 3×` Landing-0 baseline AND total
wall ≤ 1.5× baseline. Expectation is parity (booleans run on DP-thinned
row bands; deep-ocean rows take the zero-boolean fast path; the removed
per-tile Simplify pays back), but the bound is what the verdict reads
against. Exceeding it = revert the landing and author a new spec; no gated
fallback ships.

### Landing 4 - `filter_holes_for_outer` beyond 63 rings

Replace the `u64` keep-mask with a `Vec<bool>` (or SmallVec) sized to
`ring_count`; delete both `.min(64)` clamps. Unit test: 70-hole polygon
keeps all contained holes. Callers: `pipeline/emit.rs:770,904`. Gates:
```
brokkr check
brokkr tilegen --dataset denmark
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles
```

### Landing 5 - docs + ledger (bundled with the code commits per repo rules)

CLAUDE.md architecture section (ocean.rs description, LandMask removal,
geometry module list, `--geometry-stats`); `notes/rendering-fix-log.md`
entry R21 (design, landings, gate readings incl. baseline + post numbers
with host/hash, commit hashes); memory update.

## 5. Stopping rule / out of scope

- `pipeline/emit.rs` OSM polygon/multipolygon semantics (S-H, dedup,
  tile-DP, nudges, hole filter) - untouched except Landing 4. Porting
  emit.rs to int_ocean-style clipping is a separate spec if Landing 3
  vindicates the architecture.
- Assemble-side merge passes, seam reconciliation, MLT format, Natural
  Earth, fanout caps: untouched.
- Simplified-vs-detailed shapefile routing in `pipeline/mod.rs`: unchanged
  except the dropped `land_mask` argument.
- hotpath 0.20 / mlt-core 0.12 upgrades: separate errand.

## 6. Risks, resolved inline

- **Per-tile boolean cost** (Planetiler review; R14 history): priced, not
  assumed - inputs are DP-thinned row bands (post-presplit pieces span ≤64
  tiles per axis), deep-ocean rows skip booleans via the fast path, and the
  Landing-3 performance gate carries an explicit accepted-cost bound with
  revert-and-respec on failure. No fallback design ships inside this spec.
- **Fill-rule semantics** (Tippecanoe review): NonZero with the enforced
  winding invariant (§3.1) is equivalent to Wagyu's Positive on
  correctly-oriented input; the invariant is established at quantize time,
  preserved by i_overlay outputs, and unit-tested.
- **Rescale double-rounding** (Tippecanoe review): bounded ≤1 pixel unit,
  bought deliberately for exact cross-tile seam consistency ("snap to
  max-output grid"). Planetiler's PrecisionModel snap is the same trade.
- **`min_output_area` dropping cross-tile slivers**: applied only in
  per-zoom space (256 = 1 rendered px², today's threshold); base-space
  structural cuts use 0. Later intersections only reduce area, so a
  legitimate visible sliver in one tile is never removed by a cut in
  another.
- **Full-rect fast path vs foreign holes** (R06 class): trigger requires our
  own record to cover the entire row band; upstream overlap strips duplicate
  coastline across records (§2.2), so land omitted by our record but present
  in another cannot exist where the trigger fires. All other interior tiles
  clip hole-aware.
- **Encoder duplicate-skip as hidden mutation** (Tilemaker review): made
  observable - geometry-stats duplicate count must read 0 (Landing 3 gate).
- **Antimeridian/pole:** shapefile is pre-split at ±180 upstream;
  quantization clamps to `0..=2^(maxz+12)`; no new wraparound behavior.
