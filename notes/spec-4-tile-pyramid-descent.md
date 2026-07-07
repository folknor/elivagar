# Spec 4: Recursive tile-pyramid descent for polygon emission

Written against `reference/technical-implementation-spec.md`. Source item:
`notes/performance-backlog.md` P1 (item 10, with items 5/9/12 folded in and
item 1 as the final landing; subsumes items 2/3/4). Failure history:
`notes/rendering-postmortem.md` - the R23 class (symmetric convention bugs
invisible to internal round-trips) is why every landing gates on the earcut
oracle, and the R21/R24 integer rewrite is the machinery this spec builds on,
not replaces.

Measurement record: `reference/performance.md` + `.brokkr/results.db`.
Pre-change baselines (plantasjen) the keep/revert verdicts are read against:

| dataset | commit | wall | phase12 | ocean | assemble |
|---|---|---|---|---|---|
| denmark | `9b51e46` | 31.8s | 15.2s | 11.8s | 4.2s |
| norway | `661cd1c` | 160.1s | 121.3s | 14.9s | 23.4s |
| germany | `9b51e46` | 231.5s | 187.6s | 10.8s | 32.2s |

Brick 0 (norway re-baseline at HEAD) is done: run `8d1d19ca`. The Landing 2
norway keep bound reads against 160.1s.

SPEC COMPLETE (2026-07-07): Landings 1+2+3 all landed and kept,
`c8f8184`..`a0fca65`. Denmark ocean 11.8 to 5.9s, wall 31.8 to 26.4s;
norway 160.1 to 105.0s. Every polygon layer earcut-clean after the
convexity-soundness fix (`a0fca65`): Landing 2's convex early-out used
an all-same-turn cross-product test that is necessary but not
sufficient for convexity, so a lapping/spiral ring passed it and
shipped self-intersecting - the oracle caught 10 land polygons, the
fix adds the exactly-one-revolution (2 x-flips, 2 y-flips) condition.
Denmark re-blessed at a0fca65. Deferred: germany verdict (phase12-
bound, pre-landing archives wiped) and norway bench-3. Recommend a
human visual re-check of denmark a0fca65 coastline/land tiles - the
last human QA was on c8f8184, before Brick 8 seam thinning and the
convexity fix.

LANDING 1 STATUS (2026-07-07): landed and kept on denmark evidence;
norway verdict pending machine availability. Landed as `c8f8184` plus
three perf-fix rounds (`aedc9cf` parallel split + root bisection,
`4cae762` multi-crossing reconnection splitter - Landing 3's Brick 9
pulled forward after profiling, `7d86aef` on-line-vertex fast path +
oversize item expansion). Denmark verdict at `7d86aef` (bench run
`83d03eb4`, binary identity verified by strings): wall 25.9s vs 31.8s
baseline, ocean_ms 5.6s vs 11.8s baseline against the 9s keep bound;
tiles +0.23% (documented buffer-coverage gain at old z8 pre-split
lines - the old unbuffered split DROPPED buffer-strip coverage there),
output bytes +5.1% (within bound). Correctness: earcut oracle clean on
the pre-splitter landing archive (1.4M ocean polygons, 0/0), human
visual QA passed, regress vs blessed shows all diffs ocean-only and
consistent with pre-split deletion; the splitter rounds cut vertices
differently (<= 1 unit along cut lines), so oracle + regress rerun on
a current archive is part of the pending norway batch. Deviations from
this spec discovered during landing, folded back: root fragments must
be built by range bisection (not per-cell whole-piece cuts), the split
frontier must expand by fragment size (not only count), and the
splitter needs the full crossing-reconnection form immediately - the
two-crossing S-H subset falls back on exactly the expensive fragments.

Hotpath evidence (2026-07-06 campaign, `95d6d52`): `intersect_rect_into`
157/721/243 thread-s (DK/NO/DE) and 67% of denmark's tracked allocation;
`emit_shape_for_zoom` 85/155/81; `cut_row_bands_with_scratch` 51/94/48;
`emit_normalized_per_tile` 11.8/528/139; norway's relation stack
(`process_prepared_relation_into` 614, `emit_multipolygon_feature` 595)
sits on the same per-tile boolean path; `ring_is_simple_complete`
9/31/110 thread-s.

## 1. The idea in one paragraph

Today every (piece, zoom) pair re-derives tile ownership from scratch:
rescale the whole shape, DP it, normalize it, rasterize its edges to find
boundary tiles, cut it into row bands by recursive y-bisection, then clip
every boundary tile out of a row-wide shape. The same geometry is re-noded
~15 x log(rows) times, and each boundary clip pays O(row vertices). The
descent replaces all of it with ONE top-down traversal of the tile pyramid
per piece, in base (maxz-grid) integer coordinates: at cell (z, tx, ty) hold
the piece's fragment clipped to the cell's buffered rect; emit the zoom-z
tile from the fragment (rescale + DP + normalize, all on a tile-local
fragment, so every operation is small); cut the fragment into its 4 children
and recurse. Fragments shrink geometrically with depth; a fragment equal to
its full buffered rect proves the entire subtree is full-tile fills and emits
them with zero geometry work; an empty fragment prunes the subtree. One code
path for ocean AND all multi-tile OSM polygons.

## 2. Survey of the ground

### 2.1 The engine (`src/geometry/int_ocean.rs`, 2171 lines)

Types: `Contour = Vec<IntPoint>`, `Shape = Vec<Contour>` (ring 0 outer, CCW;
rest holes, CW), `Shapes = Vec<Shape>`. `IntEmitScratch` holds the reused
i_overlay `Overlay`, rect contour, tile encode buffers (`tile_points`,
`tile_ranges`, `geom_buf`), per-zoom `shape_z`/`flags_z`, and the
boundary-tile set/rows.

Emission chain for a multi-tile shape (`emit_shape_for_zoom`, line ~765):

1. `rescale_shape[_pinned]` - exact shift-round from base to zoom scale.
2. `simplify_shape_dp` - rotation-invariant pin-aware integer DP
   (anchor-min-lex + farthest, two chains).
3. `normalize_into` - i_overlay Subject/NonZero, `min_output_area`.
4. `emit_normalized_shape_for_zoom` (line ~880): single-tile shortcut, else
   `rasterize_shape_edges` (f64 DDA, dilated by the 128-unit buffer, into
   `FxHashSet<u64>`) -> `boundary_rows` -> `cut_row_bands_with_scratch`
   (recursive y-bisection; internal cuts min_area 0, leaf cuts leaf
   min_area; the ancestor-containment identity `(shape INTERSECT parent)
   INTERSECT leaf == shape INTERSECT leaf` is documented and tested there)
   -> per row, `emit_boundary_and_gap_tiles`: boundary tiles get
   `emit_clipped_tile_shape` (an `intersect_rect_into` of the WHOLE row
   shape against one buffered tile rect - this is where the 157/721/243
   thread-s live), gap runs get one point-in-shape test then
   `emit_full_tile` per tile (proven exact by the dilated rasterization -
   see the comment at line ~1072).

`intersect_rect_into` (line ~249) is the single boolean primitive: row cuts,
tile clips, tier-2 clips, ocean pre-split, ocean data-bounds clip. General
i_overlay `Intersect` with a rect contour; allocates fresh nested Vecs per
call (28.8 KB avg on denmark).

Support kept regardless of this spec: `quantize_polygon[_pinned]` (early
quantization to maxz pixel space, unclamped, antimeridian-safe),
`encode_tile_shape` (translate + orient + `mvt::encode_polygon_ranges`),
`shape_bbox`, `point_in_shape`, `lookback_dedup_contour_pinned`,
`contour_area_is_below`, tile helpers.

### 2.2 The OSM emitters (`src/pipeline/emit.rs`, 1119 lines)

`emit_polygon_feature` (line ~678) and `emit_multipolygon_feature` (line
~837): quantize once per feature at `OSM_POLYGON_MAX_Z = 14` (junction pins
from `preserve_vertex_mask` / relation shared-vertex keys become base-coord
pin flags, then a `base_pins: FxHashSet<(i32,i32)>` set); then a
DESCENDING zoom loop (`for z in (z_lo..=z_hi).rev()`) that breaks at the
first `merc_bbox_is_subpixel` zoom (z14 always emits), and per zoom picks a
tier:

- **Tier 1** (`emit_tier1_single_ring`, line ~350): pre-DP bbox fits one
  tile AND single ring -> lookback dedup, DP, `contour_area_is_below`,
  `ring_is_simple_complete` (O(n^2); 61.4M calls / 110 thread-s on
  germany), encode directly; non-simple rings escalate to `normalize_into`.
- **Tier 2** (`emit_normalized_per_tile`, line ~419): bbox_tiles <= 64
  (`OSM_TIER2_MAX_TILES`, line ~255) -> normalize once, then for EVERY bbox
  tile `intersect_rect_into` of the whole shape (no interior fast path, no
  empty-tile pruning; an 8x8 lake is 64 whole-ring booleans; norway's
  22,460-tile fjord features enter here when <= cap... no - they enter
  tier 3; what enters tier 2 is every mid-size coastal polygon, 1.78M calls
  / 528 thread-s on norway). Multipolygons additionally route single-tile
  multi-ring shapes here (line ~919).
- **Tier 3**: `emit_shape_for_zoom` from `shape_base` (lines ~804, ~983) -
  re-derives the full per-zoom chain from base every zoom.

Per-zoom knobs: `polygon_dp_tol` (0 at z >= 14 and at z <= seam_max_zoom -
the seam-DEFERRAL path for `--seam-reconcile-layers`, whose assemble-side
reconciliation is a separate, kept subsystem; see
`notes/simplify-then-reconcile-design.md`), `polygon_min_area` (0 at z14 and
for StreetPolygons/Bridges, else `MIN_POLY_AREA`), fanout caps (post-DP
bbox tile count vs per-layer cap; `cap_events` recorded and harvested by
the callers in phase12/relations).

`landing_b_tests` (line ~1016) pin tier1/tier2 byte-equality for a
single-tile shape and tier1 bowtie escalation - both tests die with the
tiers and are replaced (Brick 6).

### 2.3 The ocean driver (`src/ocean.rs`, 1071 lines)

Parse phase: `.shx` index -> `par_iter` over records ->
`parse_ocean_record` -> `push_quantized_pieces` (quantize at the range's
max_zoom; data-bounds clip via `intersect_rect_into` unless fully
contained) -> `split_piece` (line ~549): pre-split at `SPLIT_Z = 8` grid
when a piece has >= 500 vertices, via per-split-tile `intersect_rect_into`.
Process phase: global `pieces: Vec<Shape>`, then a `(piece, zoom)`
`par_iter().fold()` over `piece_count * zoom_count` work items into
`OceanAcc` (per-worker payload arena + `(key, offset, len)` records,
flushing `PayloadRecord` chunk files adopted by the sort writer - chunk
naming `chunk_NNNN.bin` continuing the PBF sequence so `--skip-to sort`
works). `emit_ocean_polygon_zoom` (line ~629) calls `emit_shape_for_zoom`
with `dp_tol = OCEAN_DP_TOL_PX = 16`, `min_area = 256`, no pins, via a
thread-local `IntEmitScratch`.

Called twice per run: simplified shapefile for the low-zoom range,
full-resolution for the high range (each with its own min/max zoom and its
own quantization maxz). Feature id = piece index.

### 2.4 Facts the design leans on (verified in code)

- The cut identity and its buffered-rect containment argument are already
  proven and unit-tested for `cut_row_bands`; the descent generalizes the
  same identity from y-bands to cells. Child buffer is exactly half the
  parent buffer in base units (`B_z = 128 << (maxz - z)`), so every child
  buffered rect is contained in its parent's buffered rect, including at
  parent-tile edges.
- Zoom-z cell rects are exact multiples of `2^(maxz-z)` in base units, so
  `shift_round` maps a base fragment contained in the buffered cell rect
  into the zoom-z buffered tile rect exactly (corners map exactly; interior
  points cannot round past a corner). DP only deletes vertices and
  normalize's intersection points stay within the input hull, so emission
  needs NO further clipping after the cut.
- Pins survive cuts without flag-threading: i_overlay preserves original
  vertex coordinates (new vertices appear only on cut lines), and both pin
  mechanisms are coordinate-keyed lookups at emission time (`base_pins`
  set; edge-strip pins are computed geometrically per fragment).
- `intersect_rect_into` callers outside the emission chain (ocean
  data-bounds clip) survive unchanged; the primitive itself is untouched
  until Landing 3.
- World-edge note (behavior delta, an improvement): today's
  `row_range_rect` clamps x at 0 while `buffered_tile_rect(0, ty)` extends
  to -128, so content in `[-128, 0)` is dropped from row shapes before edge
  tiles clip - harmless for ocean (data-bounds pre-clip guarantees x >= 0)
  and near-unreachable for OSM. The descent's buffered cell rects extend
  past world edges uniformly, which is strictly more correct. Accepted;
  not byte-compatible at the antimeridian edge.

## 3. Target structure

New module `src/geometry/pyramid.rs`; `int_ocean.rs` keeps the shared
primitives (quantize/rescale/DP/normalize/intersect/encode) and loses the
emission chain.

### 3.1 Types and signatures

```rust
/// Per-zoom emission knobs, provided by the caller as closures so ocean
/// (constant tol, min_area 256) and OSM (seam-deferral zeroing, layer-
/// dependent min_area) share one engine.
pub(crate) struct PyramidParams<'a> {
    /// Base grid zoom: quantization scale of `shape_base` (14 for OSM,
    /// the range's max_zoom for ocean).
    pub maxz: u8,
    /// Emitted zoom range, top-down inclusive. z_top computed by the
    /// caller (subpixel cutoff, layer min_zoom); z_bottom < maxz only
    /// when a fanout cap truncates the range.
    pub z_top: u8,
    pub z_bottom: u8,
    pub dp_tol: &'a dyn Fn(u8) -> i64,
    pub min_area: &'a dyn Fn(u8) -> u64,
    /// Junction pins in base coords (existing mechanism, unchanged).
    pub pins: Option<&'a FxHashSet<(i32, i32)>>,
}

/// Sink now receives the zoom: one traversal emits all zooms.
/// (z, tx, ty, encoded geometry commands)
type PyramidSink<'a> = &'a mut dyn FnMut(u8, u32, u32, &[u32]);

pub(crate) struct PyramidScratch {
    // Reused across cells and features:
    pub int: IntEmitScratch,          // overlay, encode buffers, shape_z/flags_z
    frag_pool: Vec<Shapes>,           // recursion levels reuse fragment Vecs
    edge_flags: Vec<Vec<bool>>,       // per-ring pin flags incl. edge strip
}

/// Entry point. `shape_base` is the quantized base shape (normalized NOT
/// required; the root normalizes once).
pub(crate) fn emit_shape_pyramid(
    shape_base: &Shape,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
);

/// Ocean parallel driver support: descend to a target fragment count
/// WITHOUT emitting, returning (cell, fragment) work items whose subtrees
/// partition the piece. Used to kill the fat-tail piece.
pub(crate) struct PyramidCell { pub z: u8, pub tx: u32, pub ty: u32 }
pub(crate) fn split_for_parallel(
    shape_base: &Shape,
    params: &PyramidParams<'_>,
    target_items: usize,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
) -> Vec<(PyramidCell, Shapes)>;
// A cell's fragment is `Shapes`, not `Shape`: one cut can split a piece
// into multiple outer shapes and the work item carries all of them. The
// splitter EMITS the tiles of every level above the split frontier
// through `sink` as it walks (they are few - at most sum(4^k) for
// k < split depth); the returned items' subtrees emit everything at and
// below the frontier. Frontier cells whose fragment is full or empty are
// resolved during the walk (full-subtree emission / pruning) and do not
// become items.
```

Representation stance (item 5, honest form): fragments are i_overlay
`Shapes` because the boolean's output representation is the crate's, and
fragments are transient per subtree - the win is that every boolean now
operates on tile-local geometry, plus pooled reuse of the fragment Vecs
(`frag_pool`). Everything WE own on the per-cell emission path (rescale
output, DP flags, dedup, tile points/ranges, geom buf) already lives in
flat reused buffers in `IntEmitScratch` and stays that way. No nested
`Vec<Vec<bool>>` rebuild per zoom: `edge_flags` is reused and refilled per
cell.

### 3.2 The recursion

```
descend(cell(z, tx, ty), frag: Shapes):
    if frag.is_empty(): return                     # subtree pruned
    if frag == full buffered rect of cell:         # single 4-corner ring
        emit_full_subtree(cell)                    # zero geometry work
        return
    if z >= params.z_top:
        emit_cell(cell, frag)                      # rescale+DP+normalize+encode
    if z == params.z_bottom: return
    for child in 4 children of cell:               # direct 4 rect cuts
        child_frag = frag INTERSECT buffered_rect(child), min_area 0
        descend(child, child_frag)
```

- Root: normalize `shape_base` once (min_area 0), then compute the z_top
  tile range from the bbox and cut the normalized shape into one fragment
  per z_top cell (for OSM this is almost always one cell). Cells above
  z_top are never materialized - the descent starts at z_top.
- `emit_cell`: for each shape in the fragment: rescale by
  `maxz - z` with pin flags (junction pins via set lookup + edge-strip
  pins, section 3.3), dedup, `simplify_shape_dp(dp_tol(z))`,
  `normalize_into(min_area(z))`, `encode_tile_shape` at (tx, ty). A
  convexity early-out (O(n) cross-product sign scan) skips normalize for
  convex single-ring fragments - the principled replacement for tier 1's
  O(n^2) `ring_is_simple_complete` (convex => simple; everything else
  normalizes).
- `emit_full_subtree`: for zz in max(z, z_top)..=z_bottom, emit the
  canonical full-tile geometry for every descendant tile at zz (a coord
  double loop; `emit_full_tile`'s constant ring, now zoom-parameterized in
  the sink call). This extends today's gap-run fast path across zooms.
- Full check: fragment is exactly one shape, one ring, 4 vertices equal to
  the buffered rect corners (post-cut clean guarantees no collinear
  padding; assert via debug the area equality as a second witness).
- Cuts: 4 direct `intersect_rect_into` calls on the parent fragment with
  min_area 0 (identical semantics to `cut_row_bands`' internal cuts:
  structural cuts never drop slivers; only emission applies min_area).

Cost shape: every vertex participates in O(log(tiles)) cuts total instead
of O(zooms x log(rows)) whole-shape passes plus O(boundary tiles) row-wide
booleans; every DP/normalize call sees a fragment bounded by one buffered
tile.

### 3.3 Seams (cut-then-simplify), and why the buffer makes it local

Today neighbors clip from ONE globally simplified zoom-z shape, so their
overlap-zone geometry is verbatim identical and renderers compose
seamlessly. In the descent, neighbors DP their fragments independently, so
without countermeasures the coastline's crossing of a shared tile edge can
drift by up to dp_tol between the two tiles' renderings.

Fix (pin the shared window): at `emit_cell`, before DP, pin (a) every
vertex whose base coordinates lie within `B_z` of any of the cell's four
TILE edge lines (not the clip rect - the tile edge is where rendering
composes), and (b) BOTH endpoints of every segment that crosses a tile
edge line, regardless of endpoint distance - rule (b) is what covers the
long-segment case, where a segment spans the whole window with far-away
endpoints that rule (a) would leave unpinned and DP could drop differently
per side. Cut-introduced vertices lie exactly on clip lines (on the
original chain, inside the window).

The invariant this buys is GEOMETRIC, not textual: within the shared
window `[edge - B_z, edge + B_z]`, both neighbors' emitted boundaries lie
exactly on the original chain - all original vertices there are kept
(rule a), every segment entering or crossing the window is either kept
verbatim (rule b, both endpoints pinned and present) or truncated at a cut
vertex that is collinear with it (the cut vertex lies ON the segment). The
command sequences differ (one side ends chains at cut vertices, the other
carries original endpoints), and DP subproblems outside the window differ,
but the drawn point set inside the window is identical, which is what tile
composition needs. Pinning uses the existing pin-aware DP machinery;
window flags are computed per fragment (O(V) scans), OR-ed with
junction-pin flags. The property is pinned by a test, not by this
argument: for adjacent-cell emissions of a jagged synthetic coastline,
clip both tiles' emitted polygons to the shared window rect and assert the
i_overlay XOR of the two clipped sets is empty (Brick 1 test c).

Cost: unsimplified vertices in a 2x128-unit-wide band per tile edge at low
zoom. Priced by the L1/L2 gates: per-zoom `tile bytes` and
`--geometry-stats` p99 ring vertices against baseline, bound +20% per zoom
on the ocean layer / +10% total output bytes. Contingency (Brick 8, only
if the bound trips): replace hard pins in the strip with a strip-local DP
tolerance of 2 units - still deterministic and identical on both sides
(same chain, same tolerance, same rotation-invariant anchors within the
pinned-endpoints window), but allows collinear-ish collapse.

Interaction with the seam-DEFERRAL subsystem: unchanged. `dp_tol(z)`
returns 0 for deferred layers/zooms exactly as `polygon_dp_tol` does today,
which makes the strip pinning vacuous there (nothing is simplified at all);
the assemble-side reconciliation machinery is out of scope (stopping rule).

### 3.4 Ocean driver rewrite

`process_ocean_shapefile` keeps: shx/mmap parse, `parse_ocean_record`,
`push_quantized_pieces` minus the `split_piece` call (delete `split_piece`,
`SPLIT_Z`, `SPLIT_MIN_VERTICES` - the descent's top cells ARE the split,
derived exactly instead of via a fixed z8 grid), `OceanAcc`, chunk
adoption.

Process phase becomes: build work items - small pieces (< 4096 vertices)
are one item each `(piece_id, whole pyramid)`; large pieces call
`split_for_parallel(piece, target_items = 4 * threads)` and contribute
`(piece_id, cell, fragment)` items. One `par_iter().fold()` over all items
into `OceanAcc` exactly as today (sink writes `(key, offset, len)` +
payload; key now derives z from the sink argument instead of the work
item). Parallel grain follows geometry instead of (piece x zoom), killing
the fat tail (norway P99 piece dominance). Feature id stays the piece
index - with a called-out consequence: deleting `split_piece` changes
piece indexing, so ocean feature IDS change for every previously-split
piece, and per-tile ocean feature COUNTS shift where the old z8 pre-split
cut differently than the descent (a tile once covered by two split
fragments of one source shape may now see one feature, or vice versa).
Ocean ids are synthetic and carry no semantics (accepted; Spec 5's regress
matches ocean features geometrically, not by id), but Brick 3's count
bounds are set per-counter to avoid both false alarms and masked
regressions. `--skip-to sort` chunk adoption is preserved (same
`chunk_NNNN.bin` continuation, same `adopt_chunk_files`) and gains an
explicit gate in Brick 3.

### 3.5 OSM emitters rewrite

`emit_polygon_feature` / `emit_multipolygon_feature` keep their signatures,
quantization, pin plumbing, cap_events contract, and seam-deferral inputs.
The per-zoom loop and all three tiers collapse into:

1. Compute `z_start`: today's descending-loop-with-break semantics inverted -
   the smallest z in `[z_lo, z_hi]` with `!merc_bbox_is_subpixel(merc, z)`
   (monotone in z), except z14 (== z_hi) always emits. Same emitted zoom
   set as today, verified by Brick 6's parity test.
2. Compute `z_bottom` from fanout caps: bbox tile count per zoom from the
   BASE bbox (monotone nondecreasing in z, so capped zooms form a suffix);
   record `cap_events` for each capped (layer, z) exactly as today.
   Semantic delta, accepted and priced: today's cap reads the post-DP
   bbox, the descent reads the pre-DP bbox; the pre-DP bbox is never
   smaller, so caps can only trigger equal-or-earlier. Gate: `capped_*`
   counters on the NA cap policy datasets stay within noise (denmark/
   norway/germany run uncapped by default, where this is a no-op).
3. One `emit_shape_pyramid` call with `dp_tol = polygon_dp_tol(z,
   seam_max_zoom, tol_scale)`, `min_area = polygon_min_area(z, layer)`,
   junction pins; sink pushes `SortRecord`s with
   `xy_to_tile_id(z, tx, ty)` from the sink's z.

Pinned details the single traversal must preserve (each is a behavior the
per-zoom loop provides implicitly today):

- **Zoom-dependent attrs**: `encode_attrs_bytes(&attrs, z)` filters attrs
  by zoom (`wire_format.rs`). The sink keeps a lazily-filled per-zoom
  cache (`attrs_by_zoom: [Option<Vec<u8>>; 15]` in the emit scratch,
  encoded on first use of each z) and pushes the z-appropriate bytes.
  A record encoded with another zoom's attrs is invisible to the earcut
  oracle - this is gated by the existing Shortbread spec tests (attr zoom
  gating cases) plus a new unit test: a feature with a zoom-gated attr
  emitted over z12-z14 carries the attr exactly where today's loop does.
- **Deferral stats**: today `DeferralStats::record` fires per zoom in the
  z-loop before emission (emit.rs:824 and the relations mirror), and
  `is_disabled` is consulted at feature entry to zero seam deferral. Both
  survive verbatim: the emitters still compute the per-zoom parameter
  tables (a plain z loop over closures' inputs, no geometry) and record
  deferral for each z <= seam_max_zoom in `z_start..=z_bottom` before
  calling the descent; `is_disabled` stays at feature entry.
- **Empty zoom set**: if every z in `[z_lo, z_hi]` with z < 14 is
  subpixel: when z_hi == 14 the feature emits at z14 only (today's loop
  never breaks at z == 14); when z_hi < 14 the feature emits nothing
  (today's loop breaks on its first iteration). The z_start computation
  reproduces this exactly and Brick 6's parity test covers both corners.
- **Antimeridian**: unchanged division of labor - callers
  (phase12/relations) duplicate shifted geometry via
  `antimeridian_shifts_for_bbox` + `unwrap_antimeridian_path` exactly as
  today; the engine performs NO wrapping. Cells exist only for
  tx, ty in `[0, 2^z)`; the root cell range is the bbox tile range
  CLAMPED to the world (as `tile_range_for_rect` does today), so
  out-of-world base coordinates contribute geometry to edge cells through
  their buffered rects (which extend past the world edge - the 2.4 delta).
  The existing antimeridian tests (phase12/emit test suites) are named
  gates of Brick 6 and must pass unmodified.

Deleted: `emit_tier1_single_ring`, `emit_normalized_per_tile`,
`prepare_shape_for_zoom`, `OSM_TIER2_MAX_TILES`, both emitters' tier
dispatch, `ring_is_simple_complete` (int_ocean; replaced by the convexity
early-out + normalize), `lookback_dedup_contour_pinned` if its only caller
(tier 1) dies and the dedup inside rescale+clean covers it - Brick 6
verifies via the oracle on denmark before deletion.

### 3.6 Deleted from int_ocean.rs (after Landing 2)

`emit_shape_for_zoom`, `emit_normalized_shape_for_zoom`,
`emit_boundary_and_gap_tiles`, `emit_gap_run`, `emit_clipped_tile_shape`,
`rasterize_shape_edges` + `mark_endpoint_dilated` + `mark_x_crossing_dilated`
+ `mark_y_crossing_dilated` + `rasterize_segment_clamped` +
`rasterize_horizontal_grid_line` + `rasterize_vertical_grid_line` +
`is_grid_line_coord` + `DILATE_TILE_UNITS`, `cut_row_bands[_with_scratch]` +
`cut_rows_rec` + `row_range_rect`, the `boundary_tiles`/`boundary_rows`
scratch fields, and their tests (replaced per Brick 6). `emit_full_tile`
survives, zoom-parameterized. `ZoomEmitParams` dies with its consumer.

## 4. Bricks

Each brick names its gate with the exact command. `brokkr check` means
clippy + full test suite. The output artifact of a bench run is
`data/tilegen_tmp/<dataset>-<commit>.pmtiles`; oracle commands run from
`scripts/validate/` (pnpm installed there).

**Brick 0 - re-baseline norway at HEAD.**
`brokkr tilegen --bench 3 --dataset norway`; record in
`reference/performance.md`. Gate: none (measurement only).

### Landing 1: the pyramid engine + ocean on it (one commit, keep-or-revert)

**Brick 1 - `pyramid.rs` core.** `emit_shape_pyramid`, `descend`,
`emit_cell`, `emit_full_subtree`, `split_for_parallel`, buffered-cell-rect
helpers in base coords, convexity early-out, edge-strip pin computation.
Unit tests (new, in-module): (a) cut identity - WITH dp_tol 0 (byte
identity only holds without simplification; with dp_tol > 0 the design
intentionally changes DP anchors), descend of a synthetic multi-ring shape
emits, per (z, tile), byte-identical geometry to a direct
`intersect_rect(shape_z_normalized, buffered_tile_rect)` +
`encode_tile_shape` reference for every tile at every zoom of a 3-zoom
pyramid; (b) full-subtree shortcut - a shape covering a whole cell emits
exactly the full-tile record set of the naive descent, byte-identical;
(c) seam window, dp_tol 16 - for two adjacent cells' emissions of a
jagged synthetic coastline (including a long segment crossing the shared
edge with both endpoints outside the window - the rule-b case), clip both
emitted polygon sets to the shared window rect and assert their i_overlay
XOR is empty; (d) empty pruning emits nothing; (e) world-edge cell rects
extend past 0 and max, and out-of-world geometry lands in clamped edge
cells only. Test geometry confined to one tile at test zoom per the AGENTS.md
OOM rule. Gate: `brokkr check`.

**Brick 2 - ocean on the pyramid.** Rewrite the process phase per 3.4;
delete `split_piece`/`SPLIT_Z`/`SPLIT_MIN_VERTICES`; `emit_ocean_polygon_zoom`
becomes `emit_ocean_piece` (one call per work item, sink derives z).
OSM emitters untouched (still on `emit_shape_for_zoom` - the old chain
stays alive through Landing 1). Gate: `brokkr check`.

**Brick 2b - size instrument.** `verify --geometry-stats` reports ring
counts and vertex percentiles but NOT per-zoom ocean-layer byte totals,
which the size bound below needs; ocean bypasses the sort-layer byte
counters entirely (its records are payload chunks, invisible to the drain
stats). Extend `GeometryStats` with per-zoom ocean-layer encoded-byte
sums (it already walks every tile) and print them in the summary. Laid
before the gate that reads it, per contract clause 5. Gate:
`brokkr check` + one denmark `verify --geometry-stats` run at the
pre-landing commit to record the baseline numbers the bound is read
against.

**Brick 3 - Landing 1 verdict.** Commit, then (archive paths: bench
outputs land in the brokkr scratch dir and are copied to `data/probes/`
immediately after each producing run - scratch is wiped by the next run):
- `brokkr tilegen --bench 3 --dataset denmark`
- `brokkr tilegen --bench 3 --dataset norway`
- `brokkr verify pmtiles --dataset denmark --geometry-stats`
- `brokkr verify pmtiles --dataset norway --geometry-stats`
- `node earcut-oracle.mjs ../../data/probes/denmark-<commit>.pmtiles`
- `node earcut-oracle.mjs ../../data/probes/norway-<commit>.pmtiles`
- `brokkr compare-tiles data/probes/denmark-9b51e46.pmtiles data/probes/denmark-<commit>.pmtiles --sample 500`
  (the 9b51e46 archive is the noop-probe regeneration from Spec 5 Brick 4;
  if Spec 5 has landed, additionally
  `elivagar regress data/probes/denmark-<commit>.pmtiles --against <blessed> --tol 16`)
- `brokkr tilegen --dataset denmark --skip-to sort` immediately after the
  last full denmark run, then `brokkr verify pmtiles --dataset denmark` -
  the checkpoint gate: chunk adoption must survive the ocean process
  rewrite (`tiles`/`unique_tiles` identical to the full run's).

Keep bounds (revert the landing if any fails):
- Correctness: oracle 0 deviant / 0 misattached on every polygon layer;
  verify exit 0 both datasets; `tiles` and `unique tiles` within 0.1% of
  baseline (tile coverage must not move); OSM layer record counts
  unchanged (untouched path); `ocean_features` within 10% - the pre-split
  deletion legitimately changes piece fragmentation (3.4), and this bound
  is wide for that reason alone; the tile-coverage bound above is what
  catches real loss. Output is NOT byte-identical (DP anchors move).
- Perf: denmark `ocean_ms` <= 9s (baseline 11.8s; expected 3-5s), norway
  `ocean_ms` <= 13s (baseline 14.9s); no other phase regresses > 5%.
- Size: per-zoom ocean-layer bytes (Brick 2b instrument) <= +20% at every
  zoom; total `output bytes` <= +10%.
- Human: `elivagar svg` eyeball of denmark z10 540/318 + z10 545/320
  (coastline oversize tiles), norway z14 8431/4770 (fjord), and one z6
  full-ocean tile - coastline continuous across tile edges, no missing
  water, no full-tile land squares. Record verdict in the landing commit
  message.

### Landing 2: OSM polygons on the pyramid + teardown (one commit)

**Brick 4 - emitters rewrite** per 3.5. Gate: `brokkr check`.

**Brick 5 - teardown** per 3.6, same commit: the old chain must not
survive as dead code. Gate: `brokkr check` (dead-code lints enforce the
completeness of the rip).

**Brick 6 - test migration**, same commit. Delete
`landing_b_tests` (tier parity is meaningless without tiers) and the
`cut_row_bands`/`rasterize_segment` tests; replace with: zoom-set parity
(z_start/z_bottom computation vs a brute-force of today's loop conditions
over synthetic bboxes, covering both empty-zoom-set corners from 3.5),
cap-suffix property test, convexity early-out vs normalize equivalence on
random simple quads, the zoom-gated-attr parity test from 3.5. The 65+
Shortbread spec cases, multipolygon assembly tests, and the existing
antimeridian tests run UNCHANGED - any of them needing edits means the
rewrite changed semantics it must not change. Gate: `brokkr check`.

**Brick 7 - Landing 2 verdict.** Commit, then denmark + norway + germany
(archives copied to `data/probes/` after each producing run):
- `brokkr tilegen --bench 3 --dataset <each>`
- `brokkr verify pmtiles --dataset <each> --geometry-stats`
- `node earcut-oracle.mjs ../../data/probes/<each>-<commit>.pmtiles`
- `brokkr compare-tiles` denmark vs `data/probes/denmark-9b51e46.pmtiles`,
  `--sample 500` (and regress with `--tol 16` if Spec 5 has landed)
- Cap-policy probe (the pre-DP vs post-DP bbox delta from 3.5 needs an
  ACTIVE cap to be tested; the default runs cap nothing):
  `brokkr tilegen --bench 1 --dataset denmark --fanout-cap water_polygons=64`
  at the pre-landing baseline commit and at the landing commit; compare
  `capped_features` / `capped_tiles_estimated` per layer/zoom. Keep bound:
  landing caps the same or slightly MORE features (pre-DP bbox is never
  smaller), delta <= +5%, every newly-capped feature individually
  explained by the bbox-basis change (spot-check top capped osm_ids).

Keep bounds:
- Correctness: oracle clean on all three; verify exit 0; `tiles` and
  `unique tiles` within 0.1% per dataset; per-layer record counts
  (`sort_layer_*_records`) within 1% per polygon layer, exact for point
  and line layers (untouched paths); `capped_*` zero on the default runs.
- Perf: norway wall <= 140s (baseline 160.1s at `661cd1c`; the 528+595
  thread-s tier-2/multipolygon stack is the target), germany wall <= 225s
  (emit_polygon 326 thread-s target), denmark wall <= 30s. Revert if any
  dataset's wall regresses.
- Size: total `output bytes` <= +10% per dataset; geometry-stats bound as
  Landing 1.
- Human: denmark z14 8764/5127 (worst building tile), a z12 lake
  (water_polygons) straddling 4 tiles via `elivagar svg -W 2 -H 2`,
  norway fjord z13/z14 pair - polygon fills continuous across edges.

**Brick 8 - contingency (only if a size bound trips):** strip-local DP
tolerance 2 in place of hard pins (section 3.3). Its own commit + rerun of
the tripped landing's full gate.

### Landing 3: exact axis-line splitter (item 1's strong form, one commit)

**Brick 9.** Replace the 4-child rect cuts inside `descend` (NOT the
public `intersect_rect_into`) with an exact integer split-by-axis-line:
walk each ring, compute crossings of x=c (then y=c) as exact rationals
(cut lines are grid multiples, crossings snap-round to i32), sort crossings
along the line, reconnect into left/right rings, re-nest holes via
`point_in_contour`, classify by winding. Degenerate outputs (repeated
points -> bridged rings, the R23 class of risk) route through
`normalize_into` when a cheap validity scan (repeated-point check) fails.
During bring-up a debug_assert compares the splitter's output to
`intersect_rect_into` (area + point-set equality per fragment) on every
cut. Gate: `brokkr check` with the debug oracle enabled on the full test
suite, then the complete Landing 2 gate battery (all three datasets,
oracle, verify, compare-tiles, bounds: denmark wall <= 28s, norway <= 135s,
germany <= 220s; correctness bounds identical). Keep-or-revert on its own.

## 5. Expected effect (priced by the campaign profiles)

Landing 1 attacks denmark's 157 thread-s `intersect_rect_into` (row cuts +
boundary clips collapse to fragment-local cuts) and the 85/72/51 thread-s
per-zoom chain: denmark ocean 11.8s -> 3-5s, norway ocean 16s -> 5-7s.
Landing 2 attacks norway's 528 (tier 2) + 595 (multipolygon) + part of 614
(relations) thread-s and germany's 326 thread-s `emit_polygon_feature` +
110 thread-s `ring_is_simple_complete`: norway wall target 171 -> <= 150s,
germany 231 -> <= 225s (phase12 there is decode/way-bound; the polygon
share is real but not the wall), denmark 31.8 -> <= 30s. Landing 3 is a
constant-factor cut on the remaining boolean cost, priced honestly only
after 1+2 land. RSS: ocean-phase RSS should fall (fragments are transient
per-worker; the global pre-split `Vec<Shape>` shrinks to parse output).
The prize is planet scale, where per-zoom re-noding grows super-linearly
with coastline piece size.

## 6. Stopping rule

Untouched: line/point emission, sort/assemble/PMTiles, MVT encoding,
seam-reconcile assemble machinery and its config, relation PREPARATION
(only multipolygon emission changes), ocean parse/mmap path (item 8 stays
parked), fanout cap policy (modulo the pre-DP bbox delta stated in 3.5),
node store, phase12 data flow (P2's territory). `intersect_rect_into`
remains the public boolean for data-bounds clipping; Landing 3 changes only
the descent's internal cut primitive. No env vars, no routing switches, no
compatibility shims for the deleted tiers.

## 7. References

- `reference/technical-implementation-spec.md` - the contract this spec is
  written against.
- `notes/performance-backlog.md` - source item (P1: 10 + 5 + 9 + 12 + 1).
- `reference/performance.md` - baselines and gate-reading rules.
- `notes/rendering-postmortem.md` - failure history; earcut oracle as the
  standing gate; the R23 lesson driving the debug-oracle in Landing 3.
- `notes/simplify-then-reconcile-design.md` - the cross-FEATURE seam
  subsystem this spec must not disturb (cross-TILE seams are this spec's
  3.3).
- `src/geometry/int_ocean.rs` cut_row_bands doc comment - the containment
  identity the descent generalizes.
