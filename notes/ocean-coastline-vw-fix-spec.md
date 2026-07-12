# Ocean coastline spike fix: Visvalingam ocean simplifier + coverage oracle

Implementation specification. Written 2026-07-12.

Contract: `reference/technical-implementation-spec.md`.
Source problem statement (the item this is spawned from):
`notes/ocean-coastline-spike-problem.md`.
Rendering failure ledger consulted for the survey:
`notes/rendering-postmortem.md` (R22-R24 ClosePath saga) and the
"Prior Visvalingam attempt" section of the source problem statement.
Measurement record: `reference/performance.md` + `.brokkr/results.db`.

## 1. Goal

Kill the low-zoom (z1-6) land-colored coastline spikes on the ocean layer.
Root cause (already diagnosed in the source doc, not re-derived here):
`simplify_shape_dp` run at the fixed ocean tolerance `OCEAN_DP_TOL_PX = 16`
in `emit_cell` bounds *perpendicular* deviation, not the *length* of a
removed narrow water feature, so a long sub-pixel-wide fjord collapses into a
straight chord that cuts ocean coverage; the exposed background reads as a
thin land-colored notch. The result is a valid, non-self-intersecting ring,
which is exactly why earcut passes.

The fix is to simplify the ocean polygons with an area-based
Visvalingam-Whyatt (VW) rule instead of perpendicular-distance
Douglas-Peucker (DP), scoped to the ocean path only, and to build the
coverage oracle that gates it (earcut is blind to one-sided coverage loss).

Why VW and not the alternatives already tried (survey obligation, tech-spec
point 8): tighter DP tolerance and a *global* DP-to-VW swap were both tried
and rejected - see `notes/ocean-coastline-spike-problem.md` "Prior
Visvalingam attempt". The prior VW revert failed on perf+output-size for the
~10-vertex OSM geometry class using a different function
(`for_each_zoom_simplified`, the OSM *line* path in `geometry/simplify.rs`),
and its own writeup named "fewer, larger geometries" as where VW wins. Ocean
coastline rings are exactly that (the located spike's source ring held 700+
vertices), they are a tiny fraction of features, and they use a *different*
simplify entry point (`simplify_shape_dp` in the pyramid). None of the prior
failure reasons transfer; the area metric is the direct cure for the
coverage-cutting chords.

## 2. Survey of the ground

### 2.1 The one shared simplify entry point

Both ocean and OSM polygons emit through the same pyramid:

`emit_shape_pyramid` -> `descend`/`split_for_parallel` -> `emit_cell`
(`src/geometry/pyramid.rs`). `emit_cell` per cell, per shape:

1. `build_edge_flags(shape, params, cell, &mut edge_flags)` - pins the
   current cell's tile-edge window (within the 128-unit buffer of the four
   tile-edge lines, scaled to base resolution) plus `params.pins` junctions,
   plus both endpoints of any segment crossing an edge line.
2. `rescale_shape_pinned_into(...)` - shift-round the base-resolution shape
   to this cell's zoom, carrying the pin flags (`flags_z`).
3. `simplify_shape_dp(&mut int, &mut shape_z, dp_tol, Some(&flags_z))` -
   **the call this spec replaces for ocean.** `dp_tol = (params.dp_tol)(z)`.
4. `if dp_tol > 2 { thin_window_runs(..) }` - a *second* DP pass at
   tolerance 2 over the pinned edge-window runs only (Brick 8 seam
   thinning). Left as DP (see 9. Stopping rule).
5. Convex fast path or `normalize_into` -> `encode_tile_shape`.

`dp_tol` is a closure on `PyramidParams`:
- Ocean: `ocean_dp_tol(_z) -> OCEAN_DP_TOL_PX` = constant 16 at every zoom
  (`src/ocean.rs`). `ocean_min_area` = constant 256.
- OSM: `polygon_dp_tol(z, seam_max_zoom, tol_scale)` in
  `src/pipeline/emit.rs`, a scaled `OSM_DP_TOL_PX` (also 16) with seam and
  max-zoom zeroing.

The pyramid is **non-cascading per zoom**: `descend` clips the
*base-resolution* fragment per cell and passes base-resolution geometry to
children; `emit_cell` rescales that base geometry to the cell's zoom and
simplifies afresh. Each zoom therefore simplifies from full base resolution,
independently. This resolves the "cascading vs non-cascading" open question
from the source doc: the structure is already non-cascading, VW slots in at
exactly the same point as DP, and the low-zoom output-size delta comes purely
from VW being more coverage-preserving than DP, not from any cascade change.

### 2.2 `simplify_shape_dp` internals to mirror

`src/geometry/int_ocean.rs`:
- `simplify_shape_dp(scratch, shape, tol, pins)` - in place, per contour,
  compacts survivors forward, recycles dead contours to the pool, drops the
  whole shape if slot 0 falls below 3 points. Orientation follows the
  ORIGINAL ring index (index 0 = outer). CAVEAT (R1): the driver does not
  literally "drop the shape when the OUTER dies". It compacts survivors with
  `shape.swap(write, read)`, so if the outer (read 0) fails simplification but
  a later hole survives, that hole is swapped into slot 0 and, being >= 3
  points, keeps the shape alive as a promoted pseudo-outer (the final guard
  only tests slot 0). VW must NOT inherit this latently: the VW driver adds an
  explicit outer-first guard - if the outer contour (read 0) fails, drop the
  entire shape (recycle all contours) regardless of hole survival - and
  Landing 2 adds a named test (`simplify_shape_vw_drops_shape_when_outer_dies`)
  pinning that an invalid outer drops the whole shape rather than promoting a
  hole. (This also flags a latent DP edge case, out of scope to change here.)
- `simplify_shape_dp` opens with `if tol <= 0 { return; }` - the verbatim
  short-circuit. VW mirrors it exactly (see 3.2 step 0).
- `DpScratch` (11 reused buffers) lives inside `IntEmitScratch` and is
  cleared per call - the churn-reduction contract from the 2026-07-09
  profile (DP was the #1 allocation sink). The VW scratch must follow the
  same pattern.
- `simplify_contour_dp_in_place` is pin-aware (`pins: Option<&[bool]>`),
  rotation-invariant (anchors on min-lex + farthest vertex), and validates
  with `ring_is_valid` before committing.
- Helpers reused verbatim by VW: `remove_closing_duplicate`,
  `ring_is_valid`, `orient_ring`, `signed_area_2x`, `push_nonduplicate`.

### 2.3 Every `PyramidParams` construction (teardown inventory)

Adding a simplifier selector to `PyramidParams` touches every construction
site. Exhaustive list:
- `src/ocean.rs` `ocean_params` (-> Visvalingam).
- `src/pipeline/emit.rs` two sites (~line 810, ~line 928) (-> DouglasPeucker).
- `src/geometry/pyramid.rs` test helper `params(...)` (-> DouglasPeucker,
  keeps existing DP tests exercising DP).

No other construction sites exist (`grep -n "PyramidParams {" src`).

### 2.4 Ocean consumers and the artifact reality

Two consumers funnel the same shapefile through the same `int_ocean`/pyramid
code (source doc "Which ocean, and which build"): the runtime shapefile path
(`elivagar run` -> `ocean.rs`) and the precomputed artifact
`data/ocean-tiles.pmtiles` (built by `elivagar ocean-build`). At HEAD (`88f40af`) the
artifact is ACTIVE (blessed at `7cccbb7`, "denmark baseline rotated to the
artifact-active build"), so a plain denmark/norway build serves ocean
interior tiles from the artifact and computes only the boundary band. The
artifact has the spike baked in, so **a fix in `int_ocean` is invisible in an
artifact-active build until the artifact is rebuilt.**

Development therefore runs against the shapefile path by moving the artifact
aside; shipping rebuilds it and re-blesses (Landing 4). This mirrors the
source doc "Shipping implication".

## 3. Target artifacts (concrete)

### 3.1 `Simplifier` selector on `PyramidParams`

`src/geometry/pyramid.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Simplifier {
    DouglasPeucker,
    Visvalingam,
}

pub(crate) struct PyramidParams<'a> {
    pub maxz: u8,
    pub z_top: u8,
    pub z_bottom: u8,
    pub dp_tol: &'a (dyn Fn(u8) -> i64 + Sync),
    pub min_area: &'a (dyn Fn(u8) -> u64 + Sync),
    pub pins: Option<&'a FxHashSet<(i32, i32)>>,
    pub tile_filter: Option<&'a (dyn Fn(u8, u32, u32) -> bool + Sync)>,
    pub simplifier: Simplifier,   // NEW
}
```

`emit_cell` dispatches on `params.simplifier`:

```rust
let tol = (params.dp_tol)(cell.z);
match params.simplifier {
    Simplifier::DouglasPeucker =>
        simplify_shape_dp(&mut scratch.int, &mut shape_z, tol, Some(&scratch.flags_z)),
    Simplifier::Visvalingam =>
        simplify_shape_vw(&mut scratch.int, &mut shape_z, vw_area_threshold(tol),
                          Some(&scratch.flags_z)),
}
```

The tolerance closure is reused unchanged for both; ocean's constant 16 is
converted to a VW area threshold by `vw_area_threshold` (3.3). This keeps the
per-zoom hook (`dp_tol`) as the single tuning surface. `thin_window_runs`
(step 4) stays DP tol-2 for both simplifiers (see 9).

### 3.2 `simplify_shape_vw` + `VwScratch`

`src/geometry/int_ocean.rs`. Signature parallels `simplify_shape_dp` exactly
(drop-in at the emit_cell call site), so the shape-level driver (in-place,
compact-forward, recycle dead, drop-if-outer-dead, orient by original index)
is copied from `simplify_shape_dp`, swapping the per-contour worker. The
driver mirrors DP's `if tol <= 0 { return; }` opener (VW: `if area_2x_thresh
<= 0 { return; }`) AND replaces DP's slot-0 final guard with an explicit
outer-first drop (2.2 CAVEAT): if `read == 0` fails, recycle the whole shape,
never promote a hole into slot 0.

```rust
#[hotpath::measure]
pub(crate) fn simplify_shape_vw(
    scratch: &mut IntEmitScratch,
    shape: &mut Shape,
    area_2x_thresh: i128,
    pins: Option<&[Vec<bool>]>,
) { /* mirror simplify_shape_dp's driver; per-contour -> simplify_contour_vw_in_place */ }
```

`VwScratch`, a new field of `IntEmitScratch` alongside `dp: DpScratch`,
cleared per call, warm after the first contour (same churn contract):

```rust
#[derive(Default)]
struct VwScratch {
    work: Contour,         // working copy of the input ring (closing dup removed)
    pin_flags: Vec<bool>,  // per-vertex pin, resized to work.len() (pad false)
    prev: Vec<u32>,        // circular linked list over live vertices
    next: Vec<u32>,
    area2x: Vec<i128>,     // current effective 2x-triangle area per vertex; i128::MAX if pinned
    alive: Vec<bool>,
    heap: BinaryHeap<Reverse<VwEntry>>, // min-heap by (area, tie key)
    seq: Vec<u32>,         // staleness counter per vertex for lazy deletion
    out: Contour,
}
```

(R1: `work` and `pin_flags` are the working-ring and pin buffers step 1
references; they were missing from the first draft's struct. Both live in
`VwScratch` so the per-contour worker allocates nothing after warmup, same
churn contract as `DpScratch`.)

`VwEntry` ordering key (rotation-invariant, deterministic - see 3.4):
`(area_2x: i128, x: i32, y: i32, gen: u32, idx: u32)`; `Reverse` makes the
`BinaryHeap` a min-heap. The `gen` recorded in the entry is compared against
`seq[idx]` on pop; mismatched entries are stale and skipped.

`simplify_contour_vw_in_place(contour, area_2x_thresh, pins, outer, vw) -> bool`:

0. (Driver-level, stated here for completeness) `area_2x_thresh <= 0` returns
   at the driver before any contour work - the verbatim short-circuit that
   mirrors DP's `tol <= 0` guard. Load-bearing (R2): without it, threshold 0
   still re-walks every ring from a min-lex start and strips closing
   duplicates in step 5, which is NOT verbatim.
1. Copy `contour` into `vw.work` (a working index space);
   `remove_closing_duplicate`. Resize `vw.pin_flags` to ring length
   (truncate/pad false), same as DP.
2. If live count <= 3: validate with `ring_is_valid`, orient, commit, return
   (identical short-circuit to DP).
3. Build `prev`/`next` circular lists; `alive[i]=true`; `seq[i]=0`. For each
   `i`, `area2x[i] = if pins[i] { i128::MAX } else { tri_area_2x(prev, i, next) }`
   using `signed_area_2x`-style cross product on the three points
   (magnitude, `.unsigned_abs()` cast to i128). Push all non-pinned vertices.
4. Pop loop: pop `Reverse(e)`; skip if `!alive[e.idx]` or `e.gen != seq[e.idx]`;
   stop if `e.area_2x >= area_2x_thresh` (all remaining meet the bar) OR live
   count `<= 3` (a ring floor). Otherwise remove `e.idx`: `alive=false`,
   splice `prev/next`, then for each of the two neighbors `m` that is not
   pinned, bump `seq[m]`, recompute `area2x[m]` from ITS current live
   neighbors, push a fresh entry. (Pinned neighbors keep `i128::MAX`, never
   re-pushed.)
5. Walk from the min-lex live vertex (canonical start, matches DP's
   rotation-invariance intent) following `next`, `push_nonduplicate` into
   `vw.out`. `remove_closing_duplicate(&mut vw.out)`.
6. `if !ring_is_valid(&vw.out) { return false }`. Else `contour.clear();
   contour.extend_from_slice(&vw.out); orient_ring(contour, outer); true`.

`tri_area_2x(a, b, c) = ((b.x-a.x)*(c.y-a.y) - (b.y-a.y)*(c.x-a.x)).abs()` in
i128 - the standard VW effective-area metric. Pinned vertices are never
candidates, so tile-edge windows, junctions, and edge-crossing endpoints
survive exactly as under DP; the seam behavior is preserved by construction.

Deliberate deviation from classic VW (R2 smell, stated not silent): step 4
recomputes RAW neighbor triangle areas; it does NOT clamp
`effective_area = max(recomputed, area_of_last_removed)`. The per-removal
bound (nothing below `area_2x_thresh` is removed) still holds, so every gate
is unaffected, but cumulative one-sided displacement over `n` removals is
bounded by `n * thresh / 2`, not by `thresh`. This is the honest bound behind
the "coverage-preserving" intuition and is why the coverage oracle (3.5), not
the per-vertex bound, is the real gate. If Landing 3 finds the raw-area
version drifts, adding the monotone clamp is the first knob (no interface
change).

### 3.3 Threshold conversion `vw_area_threshold`

`src/geometry/int_ocean.rs`:

```rust
pub(crate) const OCEAN_VW_AREA_2X: i128 = /* set by Landing 3 measurement */;

pub(crate) fn vw_area_threshold(dp_tol: i64) -> i128 {
    if dp_tol <= 0 { 0 } else { OCEAN_VW_AREA_2X }
}
```

`pub(crate)` is required (R1): `emit_cell` in `src/geometry/pyramid.rs` calls
`vw_area_threshold` across the module boundary from `int_ocean.rs`. A private
`fn` would not compile at the 3.1 dispatch site.

DP tolerance and VW area are not the same quantity (perpendicular distance vs
triangle area), so there is no exact conversion; `OCEAN_VW_AREA_2X` is a
tuned constant, not `dp_tol` squared. The `dp_tol <= 0` guard preserves the
"tolerance 0 = keep verbatim" contract for any zoom a closure zeroes. The
value is chosen by the Landing 3 sweep. The per-zoom `dp_tol` hook still
gates on/off per zoom; ocean returns a nonzero constant at every zoom, so VW
runs at every ocean zoom.

Starting sweep set for Landing 3: `OCEAN_VW_AREA_2X in {16, 64, 256, 1024}`
(2x-area units in zoom pixel^2). Decision rule in Landing 3.

### 3.4 Rotation invariance (why it holds)

DP achieves seam determinism by anchoring on min-lex + farthest vertex. VW's
removal order is a function of the geometric `(area_2x, x, y)` key, not vertex
index or traversal start, and recomputation reads only live neighbors; the
final start vertex is chosen min-lex. So two rotations of the same ring
produce the same live set and the same canonical output. This is asserted by
a test (5, Landing 2) paralleling `simplify_shape_dp_rotation_invariance`.

Scope limit (R2 smell): the invariance argument holds only for rings with
DISTINCT vertex coordinates. The `VwEntry` key falls through to
`(gen, idx)` after `(area_2x, x, y)`, and those two fields are
traversal/index dependent. Two live vertices at IDENTICAL `(x, y)` with equal
effective area (legal - the overlay's Intersect output can emit point-touching
rings) tie on `(area_2x, x, y)` and then break rotation-dependently, so
removal order and output can diverge across rotations. The single-synthetic-
ring test does not exercise this. Mitigations, in order of preference: (a)
ocean rings feeding VW are `normalize`d NonZero output, which does not emit
coincident-but-distinct vertices on a single contour, so the case is believed
unreachable on the ocean path - state that as the argument; (b) if it is
reachable, scope the invariance CLAIM to coincidence-free rings and let the
seam guard (pins) carry determinism at tile edges, which it does regardless.
Do not claim unconditional rotation invariance.

Cross-*feature* shared boundaries away from tile edges (the latent piece
seam, source doc "Second, separate defect") are NOT addressed here and are
NOT made worse: VW keeps at least the vertices DP's pins keep (pinned = never
removed under either), so any pre-existing seam guard behaves identically.
That defect is explicitly out of scope (9).

### 3.5 Coverage oracle: `elivagar ocean-coverage` (same-source, same-zoom)

New subcommand (model: `inspect.rs`/`diag.rs` decode-and-analyze commands;
registered in `src/main.rs` `enum Command` alongside `Inspect`, `Diag`,
`Verify`, `Regress`). Wrapped by `brokkr ocean-coverage` (model: `brokkr
pmtiles-inspect` / `brokkr diag`, which wrap elivagar subcommands). The brokkr
wrapper is a NEW brick landing in the brokkr repo, not this one (named in
Landing 1); this repo owns only the elivagar subcommand.

```
elivagar ocean-coverage <FILE> --baseline <REF>
                                [--zmin 1] [--zmax 6]
                                [--threshold-2x 512] [--layer ocean]
```

**Why the design changed (R1 crit 1+2, R2 gap 2+3).** The first draft used the
archive's own high-zoom ocean (default `--ref-zoom 8`) as the reference for
z1-6. That is invalid on THREE counts, each independently fatal to the gate the
whole fix is judged by:

1. **Cross-source confound.** z0-7 ocean is built from
   `simplified_water_polygons.shp`; z8+ from the full shapefile (two passes -
   `process_ocean_shapefile` runs up to twice, `src/ocean.rs` OceanStats
   comment; source doc "Which ocean, and which build"). A z8 reference is a
   DIFFERENT upstream dataset than gated z1-6, so `S_ref \ S_low` folds in
   upstream-simplification loss the VW fix cannot recover - noise exactly at
   the gated zooms. "z8 ocean is faithful" conflated "past the spike zooms"
   with "same source"; it is not the same source.
2. **Negative-`d` vacuous false-positive gate.** With `ref_zoom = 8`, the
   z12-14 false-positive command needs `d = ref_zoom - z < 0`; no ref tiles
   fall under `T`, "tiles with no ref children are skipped" skips every tile,
   and the gate passes without measuring anything. z14 has no higher-zoom
   reference at all.
3. **Downscale-truncation + buffer noise.** Scaling child coords into `T` by
   `>> d` truncates to whole parent pixels (at z1 a 4096-wide child collapses
   to 32 px), injecting ~1 px boundary jitter whose one-sided area along a
   coast is the same O(coastline length) magnitude as the threshold. And
   emitted MVT coords occupy the 128-unit buffer beyond `0..4096`, so unioning
   child geometry without clipping to the unbuffered extent unions duplicated
   buffer bands.

The redesign removes all three by making the reference the SAME SOURCE at the
SAME ZOOM, differing ONLY in whether ocean simplification ran:

- `<FILE>` is the build under test (VW, or the still-DP known-bad build).
- `<REF>` (`--baseline`) is a companion archive built from the SAME shapefile
  path with ocean simplification DISABLED - i.e. ocean's per-zoom tolerance
  forced to 0, which hits the verbatim short-circuit (3.2 step 0 / DP's
  `tol <= 0`). This needs a small dev knob: a `--no-ocean-simplify` flag on
  `elivagar run` (or `ELIVAGAR_OCEAN_NO_SIMPLIFY=1`) that makes `ocean_dp_tol`
  return 0 at every zoom. Ocean geometry is then emitted verbatim per zoom
  (still clipped/quantized identically), so `<REF>` is the maximal-coverage
  same-source, same-zoom truth for each tile.

For each ocean tile `T=(z,x,y)` present in `<REF>` with `zmin <= z <= zmax`:
1. Decode `<REF>`'s ocean polygons for `T` into `T`'s pixel space; clip to the
   unbuffered `[0,4096]` extent (drop buffer geometry) with the in-tree
   `intersect_rect_into`; `normalize_into` -> `S_ref`.
2. Decode `<FILE>`'s ocean polygons for the SAME `T`, same clip + normalize ->
   `S_low`. If `<FILE>` lacks `T` entirely, `S_low` is empty (all baseline
   coverage counts as lost - a real, not spurious, total notch).
3. `lost = area(S_ref) - area(S_ref INTERSECT S_low)`, computed with the
   in-tree overlay's `Intersect` rule (the overlay exposes only `Subject` and
   `Intersect` - no `Difference`/`Xor`, `src/geometry/overlay/mod.rs`; this
   identity IS the method, not a fallback). Areas are `signed_area_2x`
   magnitudes, so `lost` is in **2x-pixel^2** units - the same units as
   `--threshold-2x`, pinned throughout (R2 nit). No cross-zoom scaling occurs,
   so there is no `>> d` truncation.
4. Record per `z` the max and p99 `lost` and the worst tile id.

Exit non-zero if any tile's `lost > threshold_2x`. `--threshold-2x` DEFAULTS to
512 (R1/R2: the first draft left it unset while both L1 commands omitted it,
making exit behavior undefined); pass explicitly to gate at the Landing-1
value. Report prints a per-zoom table (max/p99 lost, worst tile) and the
offender list. Tiles present in `<FILE>` but absent from `<REF>` are skipped
(baseline is the coverage authority). `--zmax` may run to the archive max zoom
for the false-positive floor; no reference-zoom parameter exists anymore.

One-sided by construction (`area(S_ref) - area(S_ref INTERSECT S_low)`, not
symmetric): legitimate simplification may *add* ocean coverage (rounding a
concave inlet outward); only *removed* coverage exposes land. The bug is
strictly coverage-lost, so the oracle bounds only that direction.

Runtime note (R2): this design decodes two archives tile-for-tile at matched
`(z,x,y)` - O(tiles in the z-range), no `4096:1` fan-in. The dropped cross-zoom
mosaic (a z1 tile unioning 16,384 z8 descendants) was the expensive part;
matched-zoom pairing is cheap enough to run z1-6 over the world artifact in
Landing 4 without a per-tile budget worry.

## 4. Landings (ordered; each kept/reverted on its gate)

`brokkr check` and `elivagar verify` stay green at every boundary. Benchmark
discipline: commit, then measure, then record against the hash.

### Landing 1 - the coverage oracle (instrument first)

Per tech-spec point 5: the gate has no existing command, so the instrument is
its own brick, laid before the fix it gates. Implements 3.5 end to end (the
`ocean-coverage` subcommand + its `brokkr` wrapper + the `--no-ocean-simplify`
dev knob on `elivagar run` that the baseline reference needs). No
geometry-algorithm change - `--no-ocean-simplify` only zeroes ocean's
tolerance closure, reusing the existing verbatim short-circuit.

Two archives are needed for every oracle run (3.5): the build under test and a
`--no-ocean-simplify` baseline of the SAME source. Landing 1 builds both from
the current (still-DP) code:
- move `data/ocean-tiles.pmtiles` aside to `.disabled` (force the shapefile
  path, 2.4);
- `brokkr tilegen --dataset norway --variant locations` -> the DP build under
  test (`<DP>.pmtiles`);
- `brokkr tilegen --dataset norway --variant locations --no-ocean-simplify` ->
  the verbatim baseline (`<REF>.pmtiles`).

Discrimination proof (the instrument must price the bug before it can gate a
fix): the DP build must show large one-sided coverage loss vs the verbatim
baseline at the documented spike zooms, and near-zero loss at full-resolution
zooms.

Gates:
- `brokkr check` - green (new subcommand + dev knob compile, any unit tests
  pass).
- Discrimination, must FAIL (nonzero exit, offenders at z2-5 including the
  documented notch tiles):
  `brokkr ocean-coverage --file <DP>.pmtiles --baseline <REF>.pmtiles --zmin 1 --zmax 6`
- False-positive floor, must PASS: same pair at full-resolution zooms
  `--zmin 12 --zmax 14` (DP does not notch there; lost area ~0, below
  threshold). This is now a REAL measurement - matched-zoom pairing means no
  negative-`d` skip (R1 crit 2 resolved).

The threshold that cleanly separates bad-at-low from clean-at-high is recorded
as the gate `--threshold-2x` for Landings 2-4 (expected O(hundreds) of
2x-px^2; the z4/8/3 notch is a chord ~100 px long, lost area ~thousands of
2x-px^2, so a threshold near 512 - the subcommand default - discriminates with
margin). If no single threshold separates them, that is a finding - the oracle
design is revisited before proceeding (do not lay Landing 2 against a blind
gate).

### Landing 2 - VW simplifier + ocean dispatch

Implements 3.1-3.4. Adds `Simplifier` to `PyramidParams`, sets every
construction site (2.3), adds `simplify_shape_vw` + `VwScratch` +
`vw_area_threshold`. Ocean selects `Visvalingam`; OSM and the pyramid test
helper select `DouglasPeucker`.

`OCEAN_VW_AREA_2X` lands here at its FINAL value, not a provisional 256 (R1/R2
ordering finding): the sweep that picks it is Landing 3, and Landing 2's
coverage gate cannot pass "at the Landing-1 threshold" against an unknown
constant. Resolution - Landing 3's sweep runs FIRST as `--force` plain builds
(not committed, results not stored, no bench), producing only the
coverage-oracle pass/fail and byte readings needed to choose the constant;
that chosen value is what Landing 2 commits. So the ordering in prose is L1 ->
L3-sweep (force, unstored) -> L2 (commit the winner) -> L3-bench (commit-then-
measure the winner's perf). This keeps "commit first, then benchmark"
(AGENTS.md) intact: nothing benchmarked is uncommitted, and nothing committed
is un-gated.

Scope note (R1: blast radius vs goal): ocean's tolerance closure returns a
nonzero constant at EVERY zoom (`ocean_dp_tol` in `src/ocean.rs`), so selecting
`Visvalingam` for ocean applies VW at z0-14, not only the z1-6 spike band.
This is deliberate, not an oversight: VW is more coverage-preserving at every
zoom and there is no reason to keep DP's coverage-cutting behavior anywhere on
the ocean path. The high-zoom half of the range is not left ungated - the
earcut oracle and `regress` both run over all zooms (below), and the coverage
oracle's false-positive floor (Landing 1, z12-14) already prices that VW does
not disturb full-resolution coverage. A zoom-dispatched hybrid (DP above z6)
is the fallback ONLY if a high-zoom gate regresses; it is not the default.

Dev setup (artifact aside so the shapefile path exercises the fix, 2.4):
move `data/ocean-tiles.pmtiles` to `data/ocean-tiles.pmtiles.disabled` for
the duration of Landings 2-3.

New unit tests (7, section 5) added in this landing.

Gates:
- `brokkr check` - green. Includes the new VW unit tests AND the untouched DP
  tests (OSM still DP), plus the existing pyramid seam test.
- Geometry/MVT container gate (zero errors), shapefile path:
  `brokkr tilegen --dataset norway --variant locations`
  then `elivagar verify <that archive>.pmtiles`.
- Earcut tessellation-fidelity gate (0 over threshold, 0 misattached, EVERY
  polygon layer - AGENTS.md standing gate), from `scripts/validate/`, run bare
  (R2 nit: the first draft pinned `ocean` only; same cost order, matches the
  standing gate):
  `node earcut-oracle.mjs <that archive>.pmtiles`
- Coverage oracle, must now PASS at the Landing-1 threshold, against a fresh
  `--no-ocean-simplify` baseline of THIS landing's source build:
  `brokkr ocean-coverage --file <that archive>.pmtiles --baseline <REF>.pmtiles --zmin 1 --zmax 6 --threshold-2x <T>`
- Non-ocean neutrality + ocean-additive classification: use the SEMANTIC
  archive-pair diff, NOT `brokkr compare-tiles` (R1/R2 finding: compare-tiles
  is tile/count level - every coastal tile mixes ocean with other layers, so a
  tile-level diff cannot assert "non-ocean byte-identical" and nothing in it
  classifies ocean diffs as coverage-additive). The `elivagar regress`
  subcommand already takes an arbitrary archive pair
  (`regress <current> --against <ref> --json`, `src/main.rs` `RegressArgs`) and
  classifies per layer into tolerance/vertex/structural buckets - that is the
  tool. Because the blessed baseline is artifact-active and dev runs the
  shapefile path, compare two SHAPEFILE-path builds to avoid the
  artifact-vs-computed benign-diff confound: pre-fix (`HEAD~`, DP) vs this
  landing (VW). Acceptable = diffs confined to the `ocean` layer, classified
  tolerance/vertex-count-increase (VW keeps more vertices), ZERO non-ocean
  layer changes, zero ring-role / hole-containment structural changes. Read
  the JSON per-layer classification, not a bare exit code (this landing is NOT
  output-neutral by design). If the `brokkr` wrapper does not expose an
  arbitrary-pair regress, invoke the elivagar subcommand it wraps; the pair
  form exists at the binary level.
- High-zoom coverage floor (broadened-scope gate, must PASS): VW at z7-14 must
  not lose full-resolution coverage vs the verbatim baseline:
  `brokkr ocean-coverage --file <that archive>.pmtiles --baseline <REF>.pmtiles --zmin 7 --zmax 14 --threshold-2x <T>`
- Human render gate (MapLibre / `elivagar svg`): the notch is gone.
  `brokkr svg --file <that archive>.pmtiles -z 4 -x 8 -y 3 -l ocean`
  and the same for z4/8/4, z3/4/2, z2/2/1. Correct = smooth ocean fill edge,
  no thin land-colored concave notch; specifically the `(3857,2272)` apex in
  z4/8/3 path is absent. (Source doc "Reproduction".)

Baseline capture (tech-spec point 10) is deferred to Landing 3's first step
(section 8), NOT duplicated here (R2 nit: the two sites disagreed on timing;
Landing 3 is where the perf comparison is read, so the pre-change bench is
captured there). Landing 2 is a correctness landing; its gates above are
container/earcut/coverage/render, none timing.

### Landing 3 - tune `OCEAN_VW_AREA_2X`; price output size and perf

First step (baseline capture, tech-spec point 10, host `plantasjen`): from a
clean tree at the parent commit, record pre-change bench:
`brokkr tilegen --bench 3 --dataset norway --variant locations` and
`--dataset denmark --variant locations`. Expected starting points from
`reference/performance.md`: denmark locations bench-3 ~11.7s; norway is the
ocean-heavy dataset (ocean 59% of sort records, no norway locations bench row
is banked yet) and is where the ocean-cost delta is legible.

Sweep `OCEAN_VW_AREA_2X in {16, 64, 256, 1024}`, shapefile path, artifact
still aside. Sweep discipline (R2 nit - each value is a code change, so
"rebuild + measure per value" cannot both honor commit-then-measure AND avoid
a commit per throwaway value): run the sweep as `--force` PLAIN builds (dirty
tree, results not stored, no bench), reading only what picks the constant -
coverage-oracle pass/fail at all bug zooms, ocean-layer size, total archive
bytes. Do NOT bench the losing variants. Only the WINNER is committed (in
Landing 2) and only the committed winner is benched (below). This is also the
ordering fix from Landing 2: the sweep precedes the L2 commit.

Ocean-size metric (R1: name which quantity is bounded). `sort_layer_ocean_bytes`
is PRE-ASSEMBLY payload bytes (`src/ocean.rs` OceanStats: "payload bytes only,
matching the other sort_layer_*_bytes counters"), not encoded or compressed
output. Bound the ENCODED ocean protobuf bytes instead - `elivagar verify
--geometry-stats` sums `encoded_bytes` per zoom (`src/verify.rs ZoomGeomStats`)
- and keep total archive size (on-disk `.pmtiles` bytes) as a SEPARATE reported
metric. `sort_layer_ocean_bytes` may be quoted as a cheap proxy from
`brokkr sidecar <uuid> --counters` (per-zoom under `ELIVAGAR_LAYER_STATS=1`),
but the +15% bound below is on encoded ocean bytes, the shippable quantity.

Decision rule: choose the LARGEST threshold that still passes the coverage
oracle with margin (>= 2x threshold headroom) at every bug zoom - largest
because it minimizes output-size growth while staying safe. If two values
both pass with margin, take the one with smaller ocean-layer bytes. Record
the winner as `OCEAN_VW_AREA_2X`.

Explicit accepted-cost bounds (tech-spec point 5, feature paying for
capability): ocean is correctness-critical, so throughput may regress within
a stated bound. Bounds, read on norway (ocean-heavy):
- ocean-layer bytes: accept up to +15% vs the pre-fix norway build (VW keeps
  more coastline detail; that IS the fix). Growth beyond +15% means the
  threshold is too tight - revisit the sweep, do not ship.
- norway bench-3 wall: accept up to +3% (VW is O(n log n) with heap alloc,
  but ocean rings are large so the alloc amortizes - the exact "fewer, larger
  geometries" case the prior VW writeup named as where VW wins; a larger
  regression is a finding to investigate, not silently accept).
- denmark bench-3: off the ocean-dominated path (ocean is a small fraction),
  expect within noise; state the unchanged result as the neutrality gate.

Gates: all Landing-2 gates re-run at the final threshold, plus the recorded
bench numbers written into `reference/performance.md` (host `plantasjen` +
commit hash). Commit first, then benchmark, then record.

### Landing 4 - rebuild artifact + re-bless (ship; user-gated)

The fix only ships once the world artifact is rebuilt (2.4). Blessing
rotates the baseline and is done ONLY on explicit user say-so (AGENTS.md);
this landing is gated on the user's go.

Steps:
1. Restore/remove the aside `data/ocean-tiles.pmtiles.disabled`; rebuild the
   durable world-ocean artifact (MVT+gzip z0-14) with the VW code. Exact
   command (R1/R2: "brokkr invocation of elivagar ocean-build" named no
   command, and no `brokkr ocean-build` wrapper exists yet - it is a NEW brokkr
   brick to add, wrapping `elivagar ocean-build`, alongside the
   `brokkr ocean-coverage` wrapper): `brokkr ocean-build` (once added), or
   until then the elivagar binary it wraps with the shipping shapefile inputs.
   Record the new artifact size / polygon count against the H5 baseline:
   942.7 MB, 212.4M ADDRESSED TILES, 17.7M polygons (`reference/performance.md`
   - NOT "212.4M features"; 212.4M is the addressed-tile count).
2. Coverage oracle on the rebuilt artifact, must PASS, against a verbatim
   world-artifact baseline (a `--no-ocean-simplify` `ocean-build`):
   `brokkr ocean-coverage --file data/ocean-tiles.pmtiles --baseline <REF-artifact>.pmtiles --zmin 1 --zmax 6 --threshold-2x <T>`
3. Full artifact-active denmark build + regress against the OLD blessed
   baseline to quantify the intended ocean change:
   `brokkr tilegen --dataset denmark --variant locations` then
   `brokkr regress --dataset denmark` (expect ocean-layer diffs only,
   coverage-additive; non-ocean byte-identical).
4. Re-bless (user say-so): `brokkr bless --dataset denmark` promoting the
   VW+artifact build, rotating `blessed/denmark-*.pmtiles` and updating
   `brokkr.toml`. Per AGENTS.md the gate machine must carry the same
   rebuilt `data/ocean-tiles.pmtiles`.
5. `brokkr regress --dataset denmark` again post-bless must be zero-diff
   (new baseline == current build).

Gate: `brokkr check` + `elivagar verify` on the artifact-active denmark
build green; coverage oracle pass; post-bless regress zero.

## 5. Named unit tests (behavior no oracle reaches)

In `src/geometry/int_ocean.rs` `#[cfg(test)]`, paralleling the DP tests:

1. `simplify_shape_vw_rotation_invariance` - clone of the DP test: two
   rotations of one ring (COINCIDENCE-FREE vertices, per the 3.4 scope limit)
   produce identical sorted-point output under VW.
2. `simplify_shape_vw_keeps_pinned_vertices_under_aggressive_threshold` -
   clone of `landing_b_pin_aware_dp_keeps_...`: a pinned vertex survives a
   large `area_2x_thresh` that removes its unpinned twin.
3. `simplify_shape_vw_preserves_coverage_on_thin_fjord` - THE
   discrimination test (the fix's reason for existing): a synthetic
   fjord-like ring (a long narrow sub-tolerance inlet). Assert that VW's
   simplified ring's one-sided lost area vs the source
   (`area(source) - area(source INTERSECT vw_result)`) is bounded small,
   while DP at tol 16 on the same ring loses a large chord (assert
   `dp_lost > k * vw_lost`). This pins, in a unit test, the exact property
   the whole spec turns on.
4. `simplify_shape_vw_ring_floor_keeps_triangle` - a 4-point ring with one
   near-collinear vertex under a huge threshold collapses to exactly 3
   points (the live-count `<= 3` floor holds; ring stays valid).
5. In `src/geometry/pyramid.rs`, `seam_window_vw_xor_empty_for_shared_window`
   - clone of `seam_window_dp_tol_16_xor_empty_for_shared_window` with
   `Simplifier::Visvalingam`: the edge-window pins bound cross-side seam
   drift for VW to the same budget (the pins are never removed, so the seam
   vertices are identical on both sides; window thinning at tol 2 applies
   equally).
6. `simplify_shape_vw_drops_shape_when_outer_dies` (R1, see 2.2 CAVEAT) - a
   shape whose OUTER collapses below 3 points under an aggressive threshold
   while a hole would otherwise survive: assert the WHOLE shape is dropped, not
   the surviving hole promoted into slot 0. Pins the explicit outer-first
   guard that VW adds over DP's slot-0-only final check.
7. `simplify_shape_vw_threshold_zero_is_verbatim` (R2) - `area_2x_thresh <= 0`
   returns the ring unchanged (no min-lex re-walk, no closing-dup strip),
   pinning the verbatim short-circuit (3.2 step 0).

## 6. Verification matrix (every brick names its gate)

Placeholders (`<DP>`, `<REF>`, `<VW>`, `<T>`, `<uuid>`) are bound per landing,
not literal - the matrix is copy-paste-ready ONCE its landing has produced its
archives (R1: the first draft claimed copy-pasteable while carrying unbound
placeholders; they are irreducible - the archive paths do not exist until the
build runs). `<DP>` = still-DP norway shapefile build; `<REF>` = its
`--no-ocean-simplify` verbatim baseline; `<VW>` = the VW shapefile build; `<T>`
= the threshold fixed in Landing 1 (default 512). All `elivagar ...` gates run
through their `brokkr` wrappers (`elivagar` is not on PATH); where a wrapper is
new (`brokkr ocean-coverage`, `brokkr ocean-build`) it is itself a Landing-1 /
Landing-4 brick.

| Brick | Gate command |
|---|---|
| L1 oracle + knob compile | `brokkr check` |
| L1 discriminates (FAIL) | `brokkr ocean-coverage --file <DP>.pmtiles --baseline <REF>.pmtiles --zmin 1 --zmax 6` |
| L1 no false-positive (PASS) | `brokkr ocean-coverage --file <DP>.pmtiles --baseline <REF>.pmtiles --zmin 12 --zmax 14` |
| L2 unit + DP intact | `brokkr check` |
| L2 container | `brokkr tilegen --dataset norway --variant locations` then `brokkr verify <VW>.pmtiles` |
| L2 earcut (all layers) | `node earcut-oracle.mjs <VW>.pmtiles` (from `scripts/validate/`) |
| L2 coverage z1-6 (PASS) | `brokkr ocean-coverage --file <VW>.pmtiles --baseline <REF>.pmtiles --zmin 1 --zmax 6 --threshold-2x <T>` |
| L2 coverage z7-14 (PASS) | `brokkr ocean-coverage --file <VW>.pmtiles --baseline <REF>.pmtiles --zmin 7 --zmax 14 --threshold-2x <T>` |
| L2 non-ocean neutrality | `brokkr regress <DP>.pmtiles --against <VW>.pmtiles --json` (semantic per-layer; NOT compare-tiles) |
| L2 render (human) | `brokkr svg --file <VW>.pmtiles -z 4 -x 8 -y 3 -l ocean` (+ z4/8/4, z3/4/2, z2/2/1) |
| L3 output+perf | `brokkr tilegen --bench 3 --dataset norway --variant locations`; `brokkr sidecar <uuid> --counters` |
| L3 denmark neutrality | `brokkr tilegen --bench 3 --dataset denmark --variant locations` |
| L4 artifact coverage | `brokkr ocean-coverage --file data/ocean-tiles.pmtiles --baseline <REF-artifact>.pmtiles --zmin 1 --zmax 6 --threshold-2x <T>` |
| L4 regress (bless) | `brokkr regress --dataset denmark` |

## 7. Data flow (unchanged except the one dispatch)

`shapefile / artifact -> ocean.rs quantize -> emit_shape_pyramid -> descend
 -> emit_cell -> build_edge_flags -> rescale_shape_pinned_into ->
 [DISPATCH: simplify_shape_dp | simplify_shape_vw] -> thin_window_runs (DP 2)
 -> normalize_into / convex fast path -> encode_tile_shape -> MVT`.

The only new edge in the graph is the dispatch at the simplify step; OSM
keeps the DP edge, ocean takes the VW edge. Ownership: `VwScratch` is owned
by `IntEmitScratch` (thread-local via `OCEAN_INT_SCRATCH` and per-worker
`PyramidScratch.int`), same lifetime and recycling as `DpScratch`.

## 8. Baselines (tech-spec point 10)

Pre-change baseline, host `plantasjen`, commit = the parent this lands on
(HEAD `88f40af` at time of writing; blessed baseline `7cccbb7`). From
`reference/performance.md`: denmark locations bench-3 ~11.7s; artifact-active
denmark ocean phase 1.87s; H5 world artifact 942.7 MB, 212.4M ADDRESSED TILES,
17.7M polygons (R1: "212.4M features" was a misread of the addressed-tile
count). Norway is the ocean-heavy read and its pre-fix bench-3 (shapefile path)
must be captured fresh at the parent commit before Landing 3's comparison (no
norway locations bench row is currently banked in `reference/performance.md`;
captured as the first Landing-3 step - the single canonical capture point).
Post-change numbers are written back to `reference/performance.md` with host +
commit after each measured landing.

## 9. Stopping rule (blast radius)

In scope: the ocean per-zoom polygon simplification (the `simplify_shape_dp`
call in `emit_cell`, ocean branch only, at ALL ocean zooms z0-14 - see the
Landing 2 scope note), the `Simplifier` selector, the VW implementation +
scratch, the threshold constant + conversion, the coverage oracle
(`elivagar ocean-coverage` subcommand + `brokkr ocean-coverage` /
`brokkr ocean-build` wrappers), and the `--no-ocean-simplify` dev knob on
`elivagar run` that the oracle's baseline reference needs (zeroes ocean's
tolerance closure only; no algorithm change).

Explicitly OUT of scope (named, not deferred - genuinely separate items):
- **OSM polygon simplification** stays DP. The prior VW revert was about the
  OSM geometry class; not retouched.
- **`thin_window_runs`** stays DP tol-2. It touches only vertices inside the
  128-unit edge-window with pinned run endpoints, at tolerance 2 - it cannot
  cut a notch of the observed (tens-of-px) magnitude, and it is the tuned
  Brick-8 seam mechanism the seam test budgets against. Changing it is a
  different investigation.
- **The latent cross-piece seam** (source doc "Second, separate defect"):
  ocean sets `pins: None`, so a boundary genuinely shared between two source
  pieces away from tile edges is simplified independently on each side. This
  spec neither fixes nor worsens it (3.4). It has its own fix machinery
  already present (`quantize_polygon_pinned_into`, `PyramidParams.pins`) and
  gets its own spec. Named and excluded.
- **`ocean.rs` "scanline fill" stale comment** and the unused per-zoom
  `ocean_dp_tol`/`ocean_min_area` closure shape (source doc "Lateral
  findings") - editorial, fold into whichever landing touches the file, not
  a driver of this spec.

## 10. Lateral-findings channel

If, while implementing, an agent notices that the coverage oracle also fires
on a *non*-simplification cause (e.g. a clip/normalize coverage loss the DP
diagnosis missed), or that VW interacts badly with the convex fast path
(`is_convex_single_ring`) or the artifact merge, flag it - it is in the blast
radius of "why does low-zoom ocean lose coverage" even though this spec
attributes the bug wholly to DP.

## 11. Review resolution (R1 codex + R2 fable, 2026-07-12)

Two independent reviews (`notes/ocean-spec-review-R1-codex.md`,
`notes/ocean-spec-review-R2-fable.md`) were validated against the code and
folded in above. Both reviews confirmed the survey (2.x) and the core direction
(ocean-only VW at the right dispatch point) as sound; the substance was the
oracle and gate machinery. Findings, each validated then folded:

ACCEPTED and folded:
- **Oracle cross-source confound** (R1 crit 1, R2 gap 2). Confirmed:
  `src/ocean.rs` runs `process_ocean_shapefile` up to twice - simplified z0-7,
  full z8+. Fixed by redesigning the oracle to a same-source, same-zoom
  before/after-simplification diff against a `--no-ocean-simplify` baseline
  (3.5).
- **Negative-`d` vacuous false-positive gate** (R1 crit 2, R2 bug). Confirmed:
  `ref_zoom 8` vs z12-14 skips every tile. Fixed - matched-zoom pairing has no
  `d`, the floor is a real measurement (3.5, Landing 1).
- **Mosaic clipping + `>> d` truncation + buffer noise** (R1 crit 3, R2 gap).
  Fixed - same-zoom pairing removes downscaling; both sides clipped to the
  unbuffered `[0,4096]` extent before the area diff (3.5).
- **Overlay has only `Subject`/`Intersect`** (R2 gap). Confirmed
  `src/geometry/overlay/mod.rs`. The `area(S_ref) - area(S_ref INTERSECT S_low)`
  identity is stated as THE method, not a fallback (3.5 step 3).
- **`vw_area_threshold` must be `pub(crate)`** (R1). Cross-module call from
  `pyramid.rs`. Fixed (3.3).
- **`VwScratch` missing `work` + `pin_flags` buffers** (R1). Added (3.2).
- **Driver does not literally drop-on-outer-death** (R1). Confirmed the
  compaction promotes a surviving hole into slot 0; VW gets an explicit
  outer-first drop + named test (2.2 CAVEAT, 3.2, test 6).
- **Tolerance-0 verbatim guard unstated** (R2 bug). Confirmed DP's
  `if tol <= 0 { return; }` at the driver head; VW mirrors it (3.2 step 0,
  test 7).
- **Blast radius: VW at every zoom** (R1 med). Confirmed `ocean_dp_tol`
  constant. Made deliberate + gated (Landing 2 scope note, z7-14 coverage
  floor, all-layer earcut, all-zoom regress).
- **`compare-tiles` cannot verify L2 neutrality** (R1, R2). Confirmed it is
  tile/count level. Replaced by the semantic archive-pair
  `regress <a> --against <b> --json` (the elivagar `RegressArgs` accepts an
  arbitrary pair - `src/main.rs`) - Landing 2 gate + matrix.
- **Landing 2/3 ordering** (R1, R2). The constant is swept as `--force` unstored
  builds BEFORE the L2 commit; only the winner is committed then benched
  (Landing 2 preamble, Landing 3 sweep discipline).
- **Baseline metrics** (R1). HEAD is `88f40af` not `7cccbb7`; 212.4M is
  addressed tiles (17.7M polygons), not features; `sort_layer_ocean_bytes` is
  pre-assembly payload, bound encoded ocean bytes from `verify --geometry-stats`
  instead. All fixed (2.4, 8, Landing 3, Landing 4).
- **Default `--threshold-2x`** (R1, R2). Set to 512 (3.5).
- **Landing 4 artifact rebuild + wrapper commands** (R1, R2). Named
  `brokkr ocean-build` as a new wrapper brick; exact steps (Landing 4).
- **Rotation invariance overclaimed for self-touching rings** (R2 smell).
  Scoped to coincidence-free rings with the reachability argument (3.4).
- **VW monotonicity deviation unstated** (R2 smell). Stated as a deliberate
  raw-area deviation with the `n*thresh/2` cumulative bound (3.2).
- **Oracle runtime unpriced** (R2 note). Priced - matched-zoom pairing is
  O(tiles), no fan-in (3.5).
- **Norway-baseline capture inconsistency** (R2 nit). Single capture point =
  first Landing-3 step (Landing 2, 8).
- **Earcut narrowed to one layer** (R2 nit). Run bare over all polygon layers
  (Landing 2, matrix).
- **Units drift** (R2 nit). 2x-pixel^2 pinned throughout the oracle (3.5).
- **Matrix placeholders / `elivagar` not on PATH** (R1). Placeholders declared
  irreducible-until-built; all gates routed through `brokkr` wrappers (6).

REJECTED or corrected:
- R2's "cited baselines match: 212.4M features" - REJECTED. `reference/
  performance.md` says 212.4M ADDRESSED TILES and 17.7M polygons; R1 is
  correct, R2 misread. Folded per R1.
- R1 finding 6 premise "the current public MVT decoder only reports layer
  counts (`pmtiles_reader.rs`)" - CORRECTED, not adopted as stated. Full
  geometry decoders already exist in-tree (`regress.rs`, `svg.rs`, `diag.rs`
  decode MVT rings); the oracle reuses those. The valid residue (name the exact
  decoder the subcommand reuses) is folded; the "no decoder exists" framing is
  not.
- R2's "brokkr regress only compares current-vs-blessed (no arbitrary pair)" -
  PARTIALLY CORRECTED. True of the `brokkr regress --dataset` wrapper, but the
  underlying `elivagar regress` binary takes `<current> --against <ref>`
  (`src/main.rs RegressArgs`). The arbitrary-pair capability exists at the
  binary level; the spec uses it directly, noting the wrapper may need the pass
  through (Landing 2 gate).
