# OSM polygon layers: port to integer boolean clipping + ocean perf recovery

**Contract:** `reference/technical-implementation-spec.md`.
**Spawned from:** `notes/rendering-fix-log.md` R22/R23 (earcut-oracle findings:
after the ClosePath cursor fix, ocean is oracle-perfect while the emit.rs
polygon layers retain ~25K earcut-deviant polygons) and the stopping rule of
`specs/ocean-integer-clipping.md` ("porting emit.rs is a separate spec if
Landing 3 vindicates the architecture" - it did), plus that spec's failed
Landing-3 performance gate (ocean_ms 58856 vs 6987 baseline, hotpath
0704c891: per-row `intersect_rect` re-nodes the whole piece per row,
O(rows x V)).

**Revision 3** - contract review closed: antimeridian brick,
pin-flags-through-dedup, IntEmitScratch pinned, repo-root gates, full
polygon-layer enumeration, survey wording, bisection min_area pinned.
Persona reviews (Planetiler, Tippecanoe) closed: three-tier emission
paths with the design-law amendment below, feature-count parity gate,
backtrack regression fixture, min_output_area declared a deliberate
non-Tippecanoe tradeoff matching current semantics, DP tolerance
re-attributed, zero-area-outer closure mechanism corrected.

**Design-law amendment (Planetiler review):** the no-gates law bans
INCOMPLETE damage heuristics that route between differently-correct paths
(the R19/R20 failure class). A COMPLETE validity check in front of an
always-correct repair is permitted and is Planetiler's own structure
(pointwise rounding, JTS isValid, escalate to GeometryPrecisionReducer).
Tier 1 below uses exactly that: an exhaustive O(n^2) segment-pair
simplicity test on small single rings (complete on that domain), never a
heuristic.

**Classification:** two coherent rewrites - (A) the ocean emission loop's
row-cutting structure (perf), (B) the emit.rs polygon/multipolygon geometry
operations (correctness). Landings are independent keep/revert units.

## 1. Problem and premise

The earcut oracle (`scripts/validate/earcut-oracle.mjs`, faithful
maplibre-gl classifyRings) on `denmark-a13222e`:

| layer | polys over 1% deviation | worst | misattached holes |
|---|---|---|---|
| ocean | 0 (of 1.95M) | 7e-5 | 0 |
| water_polygons | 1,808 | 147x | 9 |
| land | 22,824 | Infinity (zero-area outer) | 352 |
| buildings | 552 | Infinity | 105 |

The remaining defect classes are exactly the ones the ocean rewrite
eliminated by construction, still live in `pipeline/emit.rs`:
unguarded S-H clipping (bridge fills on concave polygons crossing clip
edges), Mercator-space DP without topology restoration (figure-8s),
post-orientation vertex mutation (`nudge_hole_off_boundary`,
`nudge_coincident_hole_vertices`), hole dropping (`filter_holes_for_outer`),
and quantization damage repaired only by `dedup_quantized_ring` +
`simplify_tile_ring`'s revert-guard.

Separately, the ocean rewrite's correctness architecture overshoots its
accepted performance cost: 58.9s ocean vs the 21s bound, because each row
band is cut from the WHOLE piece (`ocean.rs` per-row
`intersect_rect(&shape, band)`), O(rows x V) with i_overlay constants.

Design laws carry over from `specs/ocean-integer-clipping.md` unchanged:
quantize early (base grid at max_zoom, exact shift-round per zoom); all
cuts are integer boolean ops; topology established by `normalize`; nothing
mutates vertices afterward except pinned serialization prep; no
damage-detection gates.

## 2. Survey of the ground

### 2.1 emit.rs polygon paths (working tree, post-R23)

- `emit_polygon_feature` (single ring, ways): per zoom (two zoom-loop
  modes - seam-reconcile and plain), Mercator DP (`simplify_into` /
  `simplify_into_with_required` when `preserve_vertex_keys` pins exist),
  then per tile: single-tile fast path (`is_single_tile` -> quantize,
  `dedup_quantized_ring`, subpixel filter, `close_and_orient_cw`,
  `simplify_tile_ring`) or multi-tile (S-H row band -> `tile_is_interior`
  fast path emitting `INTERIOR_TILE_RING` | S-H tile clip -> quantize ->
  dedup -> orient -> tile-DP), encode, `push_sort_record`.
- `emit_multipolygon_feature` (relations, emit.rs:770-911): same structure
  plus per-tile hole clipping, `nudge_hole_off_boundary` (887),
  `filter_holes_for_outer` (892), `nudge_coincident_hole_vertices` (897).
- Both consult fanout caps (bbox tile count vs `fanout_caps[layer]`) with
  capped-event metrics - BEHAVIOR PRESERVED, the cap check stays in the
  callers.
- Zoom-loop modes: `seam_max_zoom > 0` mode emits FULL-RES geometry at
  z <= seam_max_zoom (assemble-phase `reconcile_boundary_seams`,
  assemble.rs:299, decodes emitted features via the now-spec-correct
  `decode_mvt_polygon` and re-encodes); z in (seam_max_zoom, 13] uses
  Mercator DP with optional pinned vertices; z >= 14 full-res. Plain mode:
  Mercator DP cascade. Subpixel early-exits (`merc_bbox_is_subpixel`,
  `ring_is_subpixel`) with `skip_bbox_check`/`skip_size_filter` layer
  exemptions (Streets/Boundaries) - PRESERVED.
- **Pins:** `preserve_vertex_keys` marks shared boundary vertices so DP
  keeps them, keeping cross-feature seams aligned. Any replacement DP must
  honor pins or seam reconciliation regresses.
- Line and point emission paths: untouched by this spec. S-H
  (`clip_polygon_into`) retains non-polygon users only if any exist after
  the port; if the port orphans it, delete it (and `Edge`,
  `clip_polygon_edge_into`, `edge_intersect`, `is_inside`); the linestring
  clipper (`for_each_clipped_segment`, Cohen-Sutherland) is separate and
  stays.
- Callers: `pipeline/phase12.rs` (ways) and `pipeline/relations.rs`
  (relations) call both emit fns; signatures may change freely.
- `INTERIOR_TILE_RING` (emit.rs): buffered full-tile rect preset - replaced
  by the engine's fast path semantics.
- To DELETE after the port (verify orphanhood by grep, then remove):
  `nudge_hole_off_boundary`, `nudge_coincident_hole_vertices`,
  `filter_holes_for_outer` (+ its Landing-4 test moves to a port
  regression test), `dedup_quantized_ring` (polygon-only users),
  `simplify_tile_ring`/`tile_dp_recurse`/`tile_find_farthest` (if
  polygon-only), `tile_is_interior`, `INTERIOR_TILE_RING`, the disabled
  `is_valid_simple_ring_points` comment blocks, `ring_is_simple` (if its
  only remaining users are verify.rs - verify keeps its own copy or keeps
  the fn; decide by grep at implementation, keep it if verify uses it).

### 2.2 Ocean emission loop (the perf defect)

`ocean.rs` per zoom, per shape: `for ty in ty_min..=ty_max {
row_shapes = intersect_rect(&shape, band, 256); ... }` - the whole shape is
re-noded once per row. Hotpath 0704c891: `emit_ocean_polygon` 227.5s CPU,
P50 38us / P99 1.58s - a few large pieces dominate. Fast-path rows still
pay the full-shape intersect to discover they are rectangular.

### 2.3 Shared machinery available

`geometry/int_ocean.rs`: `quantize_polygon`, `rescale_shape`,
`simplify_shape_dp` (rotation-invariant, i128), `normalize`,
`intersect_rect`, `point_in_shape`, `Shape`/`Shapes` aliases,
`OCEAN_DP_TOL_PX = 16`. The ocean emission loop in `ocean.rs`
(`emit_ocean_polygon`, `emit_boundary_and_gap_tiles`,
`emit_clipped_tile_shape`, `emit_full_tile`, `fast_path_rect`, exact DDA
rasterizer).

### 2.4 Ledger reconciliation

- R02/R04/R05 (cleanup/nudges): deleted, not replaced. R16 (backtrack
  dedup): subsumed by quantize+normalize. R15 (tile-DP cap): superseded by
  per-zoom integer DP from base geometry (no cascade by construction).
- S07 (DP seam dependence): already closed by rotation-invariant DP; pins
  added here extend it, they do not reintroduce seam dependence (pinned
  vertices are deterministic, position-derived, feature-independent).
- R14/R21-perf: the band-bisection landing is the priced fix for the
  measured O(rows x V) structure, not a new gated hybrid (no gates: the
  bisection is the unconditional cut structure).

## 3. Target structure

### 3.1 Landing A - ocean row cutting by recursive band bisection

New in `geometry/int_ocean.rs`:

```rust
/// Cut a shape into per-row-band shapes for rows ty0..=ty1 (buffered), by
/// recursive bisection: intersect the shape with the top/bottom halves of
/// the row range, recurse into each half with the (much smaller) result.
/// O(V log R) total noding instead of O(V x R). Leaves in row order.
/// Rows whose result is empty yield an empty Shapes.
pub(crate) fn cut_row_bands(
    shape: &Shape, ty0: u32, ty1: u32, world_max: i32, buffer: i32,
) -> Vec<Shapes>;
```

Recursion: `cut(shape, lo, hi)`: if `lo == hi` return
`[intersect_rect(shape, band(lo) +- buffer)]`; else `mid = (lo+hi)/2`,
`upper = intersect_rect(shape, rect(rows lo..=mid) + buffer)`,
`lower = intersect_rect(shape, rect(rows mid+1..=hi) + buffer)`, recurse
each half over ITS OWN result shapes and concatenate. The half-rects are
expanded by the row buffer (128) so leaf bands equal today's
`row_band_rect` exactly - unit test pins leaf-equality against direct
`intersect_rect(shape, row_band_rect(ty))` output (same Shapes, order- and
rotation-normalized comparison).

Internal bisection cuts use `min_area = 0` (structural cuts must not drop
slivers that a leaf band would keep); leaf-band intersects use 256 exactly
as today. Each leaf yields one `Shapes`; the return Vec is indexed by
`ty - ty0` (empty `Shapes` for empty rows), so the consumer's row loop is
a direct index. `emit_ocean_polygon`'s per-zoom loop consumes
`cut_row_bands` output instead of calling `intersect_rect` per row;
everything downstream (fast path, boundary/gap emission) is unchanged.

### 3.2 Landing B - the emit.rs port

**Antimeridian (contract-review High #1):** the OSM paths emit shifted
copies for features crossing +-180 (`antimeridian_shifts_for_bbox`,
emit.rs:71; callers phase12.rs:996, relations.rs:309): the Mercator
geometry is shifted by +-1.0 in x before emission. `quantize_polygon`
currently clamps to [0, scale] and would flatten shifted copies. Brick:
quantization becomes UNCLAMPED (i32 range at maxz=14 admits x in
[-2^26, 2^27] comfortably; i_overlay is i32-native); the shifted Mercator
copy is quantized as-is, exactly like today's flow shifts before emission.
The ocean path is unaffected (its inputs are bounds-intersected in base
space; the clamp was redundant there). Unit test: a shifted copy
quantizes to coordinates beyond the world edge and round-trips through
rescale/normalize/intersect_rect without clamping.

New shared engine in `geometry/int_ocean.rs` (extracted from ocean.rs,
parameterized; ocean.rs becomes a caller):

```rust
pub(crate) struct IntEmitScratch {
    pub boundary_tiles: HashSet<u64>,          // packed (tx,ty)
    pub boundary_rows: HashMap<u32, Vec<u32>>, // ty -> sorted tx list
    pub all_rings: Vec<Vec<(i32, i32)>>,       // serialization staging
    pub geom_buf: Vec<u32>,                    // MVT command staging
    pub shape_z: Shape,                        // rescaled per-zoom shape
    pub flags_z: Vec<Vec<bool>>,               // pin flags (empty if unused)
}
// One per rayon worker (lives in the existing EmitScratch / OceanAcc).

pub(crate) struct ZoomEmitParams {
    pub z: u8,
    pub maxz: u8,
    pub dp_tol: i64,          // 0 = no DP (seam-deferred zooms, z>=14)
    pub min_area: u64,        // 256, or 0 when skip_size_filter
    pub pins: Option<&FxHashSet<(i32, i32)>>, // base-space pinned vertices
}

/// Per-zoom emission of one base-space Shape: rescale -> DP (pin-aware) ->
/// normalize -> emit via `sink` per tile. Small shapes (bbox within one
/// tile at z) take a direct path (translate + orient + encode, no
/// rasterize/rows). Large shapes use the ocean machinery: exact DDA
/// rasterization, cut_row_bands, deep-water/interior fast path emitting
/// full-tile rects, per-tile intersect_rect for boundary and gap tiles.
/// `sink(tx, ty, encoded_geom: &[u32])` owns record construction.
pub(crate) fn emit_shape_for_zoom(
    shape_base: &Shape, params: ZoomEmitParams,
    scratch: &mut IntEmitScratch, sink: &mut dyn FnMut(u32, u32, &[u32]),
);
```

- **Pin-aware DP (contract-review High #2 resolution):** pins travel as
  per-vertex FLAGS, never indices, so duplicate-collapse cannot desync
  them. At quantize time: `quantize_polygon_pinned(outer, inners, maxz,
  pin_test: impl Fn(&Point) -> bool) -> (Shape, Vec<Vec<bool>>)` - the flag
  vector is built in the same pass that collapses duplicates (a dropped
  duplicate ORs its flag into the survivor; the dropped closing vertex ORs
  into the first). `rescale_shape_pinned(&Shape, s, &flags) -> (Shape,
  Vec<Vec<bool>>)` preserves flags through downshift-dedup the same way.
  `simplify_shape_dp` takes `pins: Option<&[Vec<bool>]>`; a flagged vertex
  is unconditionally kept (marked in the keep array before recursion).
  `pin_test` wraps today's `preserve_vertex_keys` membership check on the
  Mercator vertex. Pinning applies only where it applies today: z in
  (seam_max_zoom, 13] when `preserve_vertex_keys` is non-empty (pins=None
  otherwise - flag plumbing skipped entirely).
- `emit_polygon_feature` / `emit_multipolygon_feature`: quantize the
  feature once at maxz (`quantize_polygon_pinned` - winding invariant,
  unclamped), keep the existing zoom loops, subpixel early-exits, fanout
  caps, and seam-mode zoom routing EXACTLY as today; the per-zoom body
  routes by size into THREE tiers (`dp_tol` = 0 for z <= seam_max_zoom or
  z >= 14, else `OSM_DP_TOL_PX = 16` - the value of today's
  SIMPLIFY_PIXELS = 1.0 rendered px, NOT a Tippecanoe-derived number;
  named separately from OCEAN_DP_TOL_PX so they can diverge without
  archaeology):
  - **Tier 1 - single-tile, single-ring** (the millions-of-buildings
    case; bbox within one tile at z): rescale -> lookback dedup (the
    existing `dedup_quantized_ring` backtrack repair, RETAINED here) ->
    DP -> COMPLETE simplicity check (exhaustive segment-pair test incl.
    improper touching, O(n^2), n is small) -> if simple: orient, encode
    (today's cost profile, no i_overlay); if not simple: escalate to
    `normalize` (always-correct repair). Single-tile shapes WITH holes
    skip the check and normalize unconditionally (nesting must be
    re-established).
  - **Tier 2 - multi-tile, bbox tile count <= 64**: rescale -> DP ->
    normalize once -> per-tile `intersect_rect` directly (no
    rasterize/rows - at these sizes the row machinery costs more than it
    saves).
  - **Tier 3 - bbox tile count > 64** (large landuse/water): the full
    ocean machinery (rasterize, cut_row_bands, fast path, boundary/gap
    emission). The 64 threshold is a cost-routing constant between two
    correct paths (like SPLIT_MIN_VERTICES), not a damage gate.
- Multipolygon: outer + inners form one `Shape`; multiple outers per
  relation (from `multipolygon.rs` assembly) form one Shape EACH, emitted
  in a loop (each is an independent polygon; MapLibre grouping needs no
  cross-polygon ordering guarantees across separate features).
- Serialization prep exemption and encode path: identical to ocean
  (`close_and_orient_cw/_ccw` by ring index, translate, `encode_polygon`).
- All deletions from section 2.1's list once orphaned.

### 3.3 What each remaining oracle defect class maps to

| Defect | Closed by |
|---|---|
| S-H bridges on concave landuse/water crossing tile edges | integer boolean cuts everywhere |
| Mercator-DP figure-8s | integer DP + normalize per zoom, no cascade |
| Escaped/nudged holes | nudges deleted; nesting is normalize/intersect output |
| Zero-area outers with holes (deviation=Infinity) | quantize `ring_is_valid` drops degenerate rings; `clean_shapes`/normalize drop the rest (min_output_area is only the final guard) |
| Cascading DP drift across zooms | every zoom derives from base quantization |

## 4. Landings

Artifact path convention: `data/tilegen_tmp/denmark-$(git rev-parse --short
HEAD).pmtiles`. ALL gate commands are run from the repo root (the oracle
is invoked as `node scripts/validate/earcut-oracle.mjs <repo-root path>`).
Every landing is one commit; revert = `git revert`.

### Landing 0 - baseline

No code. Record at current HEAD (post-R23):
```
brokkr tilegen --bench --dataset denmark
```
Record total/ocean_ms/RSS (host plantasjen + hash) into the R24 ledger
entry. The oracle BEFORE tables are already recorded in R23.

### Landing A - ocean band bisection

`cut_row_bands` + ocean loop change + unit tests: leaf-equality vs direct
per-row intersect (concave shape spanning 8 rows; shape with holes;
single-row range; empty rows in range). Gates:
```
brokkr check
brokkr tilegen --dataset denmark
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles ocean 0.01
```
Oracle must stay 0 over / 0 misattached. Then commit, then:
```
brokkr tilegen --bench --dataset denmark
```
Accepted-cost bound (keep/revert): `ocean_ms <= 15000` (Landing-0 R21
baseline was 6987; the correct architecture buys topology at a bounded
premium; expectation after bisection is well under this). Revert +
re-spec if exceeded.

### Landing B - emit.rs polygon port

Engine extraction + three-tier emit paths + pin-aware DP + deletions +
unit tests: pin-aware DP keeps pinned vertices under aggressive tolerance;
Tier-1 escalation fires on a quantize-induced bowtie and the result is
simple; Tier-1 and Tier-2 produce identical encoded bytes for a shape
eligible for both; multipolygon with 2 outers + nested holes emits 2
features each earcut-clean; the historical Infinity class (zero-area
outer with holes, AND a sub-min-area outer with larger-than-outer holes)
emits nothing; backtrack regression fixture (Tippecanoe review): a ring
whose rescale produces an A-B-A spike, through each tier, encodes with no
backtrack (assert via decode: no vertex equals its second predecessor).
Gates (in order):
```
brokkr check
brokkr tilegen --dataset denmark
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles
elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles --geometry-stats
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles ocean 0.01
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles water_polygons 0.01
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles land 0.01
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles buildings 0.01
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles sites 0.01
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles dam_polygons 0.01
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles pier_polygons 0.01
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles street_polygons 0.01
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles bridges 0.01
```
Pass = every layer 0 over-threshold, 0 misattached, AND per-layer/zoom
feature counts within +-2% of the same oracle run against the Landing-A
artifact (coverage-parity guard: min_output_area and tier routing must not
silently change what exists - the deliberate, documented exception being
polygons the old path emitted with zero/degenerate area). This enumerates ALL
polygon-emitting Shortbread layers per the matchers (shortbread/water.rs:
dam_polygons 148, pier_polygons 172; shortbread/streets.rs:
street_polygons 133, bridges 326; plus ocean, water_polygons, land,
buildings, sites). A layer the oracle reports as absent passes vacuously.
Then commit, then:
```
brokkr tilegen --bench --dataset denmark
brokkr results --compare <landing-A-commit-shorthash> <this-commit-shorthash>
```
Accepted-cost bound: total wall <= 1.2x the Landing-A bench total AND peak
RSS <= 1.2x. Revert + re-spec if exceeded.

Human gate (after command gates): MapLibre pass over the full map - the
R-ledger sites (Fyn z6-z8, Mors z7-z9, Limfjorden/Ringkobing Fjord
z8-z12, Nissum Bredning z9-z10), dense urban z14 (buildings), forest
landuse z10-z13. Correct = no triangle spray, no missing landcover/water,
islands and lake holes render.

### Landing C - docs + ledger

Bundled with the code commits: CLAUDE.md architecture updates (emit.rs
description, deleted-helpers list, engine location), R24 ledger entry with
all gate readings (hash-anchored), spec cross-references.

## 5. Stopping rule / out of scope

- Line/point emission, label placement, attribute handling: untouched.
- Assemble merges, seam reconciliation ALGORITHM (only its input geometry
  quality changes - for the better), MLT, fanout-cap policy: untouched.
- hotpath/mlt-core dep upgrades: separate errand.
- Further ocean perf below the Landing-A bound: only if its gate fails.

## 6. Risks, resolved inline

- **normalize per feature per zoom on millions of small features**
  (Planetiler review High): resolved structurally - Tier 1 runs no
  i_overlay at all on the common path (complete simplicity check ~100ns
  for building-sized rings), escalating only on actual damage; Tier 2
  normalizes once per feature-zoom on small shapes; only Tier 3 pays the
  full machinery. The accepted-cost bound (1.2x) remains the
  verdict-reader.
- **Tiny-polygon hard drop vs Tippecanoe accumulation** (Tippecanoe
  review High): min_output_area/min-area filters hard-drop sub-threshold
  rings exactly as today's `ring_is_subpixel` does; we deliberately do
  NOT adopt Tippecanoe's area-accumulation markers (a Shortbread
  quality choice, revisitable). The feature-count parity gate (+-2%)
  bounds any unintended coverage change.
- **Pin correctness across quantization:** pins are matched by base-space
  vertex index (computed once at quantize time from `preserve_vertex_keys`
  Mercator coords -> base-space equality), so pin survival is exact and
  rescale-independent.
- **Seam reconciliation input change:** at z <= seam_max_zoom features are
  emitted with dp_tol 0 - full base-resolution geometry, strictly cleaner
  than today's input (no S-H bridges); `reconcile_boundary_seams` decodes
  via the spec-correct decoder. Its own tests + verify + oracle gate any
  regression.
- **Fanout caps interplay:** cap checks operate on bbox tile counts,
  computed the same way (integer bbox now); capped-event metrics fields
  unchanged.
- **`sites`/rare polygon layers missed:** the oracle gate enumerates all
  Shortbread polygon layers present in the archive; a final sweep
  `for L in $(layers)` is encoded in the gate list above explicitly.
