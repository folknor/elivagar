# Spec: bound every classified polygon at MapLibre's 500-ring clamp

Written against `reference/technical-implementation-spec.md`. Spawned
from the roadmap item "OPEN: three full-pass ocean polygons exceed
MapLibre's 500-ring clamp" (`notes/planet-30gb-roadmap.md`, H5) and the
ring-cap census readings in `reference/performance.md` (low-zoom ocean
union section). Failure history reconciled against
`notes/rendering-postmortem.md` (the all-polygon-layer earcut
requirement and the independent-consumer lesson both bind gates below).
Revision 2, 2026-07-24: critiqued pre-code by codex-xhigh (14 findings);
the merger enforcement point was removed (wrong clamp unit), the depth
floor was removed (violated the invariant), and the corpus, visual,
layer-scope, referee, and sequencing gates were rebuilt per findings.

## The defect, stated precisely

MapLibre's classifyRings groups a feature's rings into polygons (an
outer plus its following opposite-wound holes) and applies the 500-ring
clamp TO EACH CLASSIFIED POLYGON separately - the verbatim port in
`scripts/validate/earcut-oracle.mjs` clamps `polygons[j]`, never the
feature's ring total. The invariant this spec enforces is therefore:

    no classified polygon exceeds MAX_FEATURE_RINGS (500) rings.

Two consequences shape the design:

- Merging any number of already-safe polygons into one multi-geometry
  feature cannot recreate the defect: each concatenated outer starts a
  new classified polygon with its own clamp budget (the encoder
  normalizes winding per ring role in `append_translated_ring`, so
  self-calibration groups correctly). `merge_same_attr_geometries` is
  NOT an enforcement point and is untouched by this spec.
- The clamp drops rings only in MapLibre-semantics consumers. OpenLayers
  keeps all rings (`reference/corpus.md`, viewer geometry section) and
  `elivagar svg` renders every decoded ring - so plain SVG renders
  CANNOT evidence this fix; only the canonical corpus renderer (which
  ports the clamp) or MapLibre itself can.

Three offenders exist, all full-pass (z8+) single ocean polygons:

- denmark z9/285/148 feat 10: 510 rings
- denmark z9/286/147 feat 4: 602 rings
- world artifact z10/546/260 feat 1: 725 rings

Composition confirmed 2026-07-24 by feature-probe on the pinned
comparand archive (see Sequencing): feature id 5285 at z9/285/148 is
ONE outer (896 v, spanning the buffered tile) plus 509 hole rings. A
single classified polygon - only geometric partition can fix it.

## Survey of the ground (verified 2026-07-24 at `da6995f`)

- **Emission**: `src/geometry/pyramid.rs` `emit_cell` - each shape of
  the cell fragment goes rescale -> simplify -> `normalize_into`, and
  each normalized `Shape` (`src/geometry/int_ocean.rs` alias: one outer
  contour + N hole contours, N unbounded) encodes as one polygon
  geometry via `encode_tile_shape`. Coordinates are GLOBAL zoom-z
  integers until `encode_tile_shape` subtracts the tile origin.
- **Coverage**: every multi-ring polygon reaches `emit_cell` - ocean
  extract passes and `ocean-build` (`src/ocean.rs` call sites), OSM
  closed ways and multipolygon relations (`src/pipeline/emit.rs`, both
  `emit_shape_pyramid` sites). The bypasses are single-ring by
  construction: `emit_full_subtree` (canonical full tiles) and the
  convex early-out.
- **Post-emission recombination is safe, reviewed conclusion**:
  `merge_same_attr_geometries` concatenates whole polygons whose clamp
  budgets are independent (above); the MVT batch path additionally
  skips ocean outright; `merge_connected_lines` skips non-LineString
  features; seam reconciliation runs pre-merge and rewrites rings
  in-place, concatenating nothing; the artifact serve path splices
  encoded ocean BYTES without recombining features
  (`src/pipeline/assemble.rs` run-copy and layer-splice sites). Paint
  order is unaffected: partition pieces carry identical layer, rank,
  and attrs. The MLT scaffold merges ocean too - outside every
  contract, named, not chased.
- **A per-FEATURE total-ring policy is a named non-goal**: nothing in
  MapLibre or the census measures feature ring totals; if one is ever
  wanted it needs its own rationale and its own instrument.
- **Instruments**: `ring-cap-census.mjs` and `earcut-oracle.mjs` both
  default to the `ocean` layer only and both accept `--unique`
  (payload-deduplicated scan). The all-polygon-layer earcut requirement
  (`notes/rendering-postmortem.md`) therefore needs an instrument brick
  before the gates below can claim layer coverage.
- **Referee**: the in-tree overlay engine prunes Xor
  (`src/geometry/overlay/port/extract.rs`); the differential XOR
  referee is the `i_overlay` dev-dependency, precedent at
  `src/geometry/pyramid.rs` (`OverlayRule::Xor` tests). Independent of
  the implementation under test, per the R23 lesson.
- **Corpus mechanics**: `corpus check` compares contract BEFORE content
  and exits 2 on the policy-version bump without naming tiles (the v3
  rotation demonstrated exactly this). `corpus bless` WITHOUT
  `--rotate` crosses the contract, computes the leaf diff without
  writing, and exits 1 with the changed tiles appended to the refusal
  (`src/corpus.rs`, the `rotation requires --rotate` path) - that is
  the pre-rotation two-tile evidence instrument.
- **Policy version**: `OCEAN_POLICY_VERSION = 3` (`src/ocean.rs`) with
  the v2/v3 history in its doc comment; both the artifact key and the
  ocean chunk-resume key carry it.

## Target artifacts

1. `pub const MAX_FEATURE_RINGS: usize = 500;` in `src/mvt/mod.rs`,
   comment naming MapLibre's classifyRings per-polygon clamp as the
   source and `ring-cap-census.mjs` as the mirror.

2. **The partition**, in `src/geometry/pyramid.rs`:

   ```rust
   fn partition_shape_to_ring_cap(
       scratch: &mut PyramidScratch,
       shape: Shape,        // normalized; contours > MAX_FEATURE_RINGS
       rect: IntRect,       // the shape's buffered clip rect, global
                            // zoom-z coords (tile_origin - TILE_BUFFER
                            // .. tile_origin(t+1) + TILE_BUFFER)
       out: &mut Shapes,    // every element <= MAX_FEATURE_RINGS
   )
   ```

   Pinned semantics, so two implementations match:

   - Coordinates: global zoom-z integers, same space `emit_cell` holds
     shapes in; `rect` starts as the buffered tile rect already
     computed for the cell.
   - Split axis: the longer of (max_x - min_x) vs (max_y - min_y);
     tie splits x. Midpoint: `min + (max - min) / 2` in integer
     arithmetic (floor). A rect is indivisible when the chosen axis
     has `mid == min`.
   - Halves: `[min, mid]` then `[mid, max]` on the split axis - both
     closed at the shared cut coordinate `mid`, so cut vertices are
     exact integers identical on both sides. Low half traverses first,
     depth-first; `out` receives pieces in traversal order (the
     within-run sort key downstream is content-derived, but
     deterministic emission order keeps builds byte-identical, the
     existing determinism property).
   - Clip: `intersect_rect_into` with `min_area = 0` - island survival
     was decided by `normalize_into`; partition must not re-decide it.
   - Recursion: any product still over the cap recurses with its half
     rect. There is NO depth floor and NO silent fallback: if a product
     is over the cap and its rect is indivisible on both axes, that is
     `expect()` - a loud invariant violation, not an emission. (It is
     unreachable: more than 500 disjoint positive-area contours cannot
     inhabit an indivisible integer rect. The error path exists because
     "unreachable" is an argument, not a proof, and a silent over-cap
     emission would be the incompatible-with-categorical-gate outcome.)
   - Scratch: pieces use the existing `take_shapes`/`return_shapes`
     pool idiom; the input shape returns to its pool after partition.

   Call site: the `normalize_into` loop in `emit_cell`. Shapes at or
   under the cap take the existing path with zero new work.

3. **Counters**, atomic statics aggregated across workers (the
   `src/pipeline/assemble.rs` metrics pattern), cumulative per run,
   flushed with the existing end-of-run counter emission, live on
   every path through the pyramid (extract, `ocean-build`, OSM):

   - `ring_cap_partitions`: count of normalized shapes that exceeded
     the cap and were partitioned.
   - `ring_cap_pieces`: total pieces emitted BY partitioned shapes
     (a shape split into 2 adds 2 here, 1 to partitions).

   Observability, not gates - the gate is the census at zero plus the
   hard error. Expected denmark readings: partitions 2; pieces likely
   4 (510 and 602 rings each bisect once); recorded, not asserted.

4. **`OCEAN_POLICY_VERSION` 3 -> 4** (`src/ocean.rs`): durable ocean
   tile bytes change. The v4 entry joins the v2/v3 history in the doc
   comment: "v4: polygons over the MapLibre 500-ring clamp are
   partitioned by rect bisection; previously their smallest islands
   were invisible to MapLibre-semantics consumers."

5. **Instrument brick** (laid first, gates depend on it): both
   `ring-cap-census.mjs` and `earcut-oracle.mjs` accept `all` as the
   layer argument, iterating every layer that contains polygon
   features. Mechanical iteration only - the per-polygon math is
   untouched, and the calibration check is that `all` reproduces the
   single-layer ocean numbers exactly on the same archive.

## Sequencing and gates

Every command exact. Bricks 2-5 are one landing commit; the corpus
rotation is its own commit (the v3 precedent, `2ab6f83` then
`a40c077`).

**Brick 0 - pre-change evidence, banked before anything changes.**
The tree is clean at `da6995f` and the v3 artifact is live; both stop
being reproducible mid-landing, so this comes first:

- Fresh baseline (the stored 8.8s row is anchored at `bc71cf1`, which
  is stale): `brokkr tilegen --bench 3 --dataset denmark --variant
  locations`. Record the best-of-3.
- Bank pre-v4 canonical (MapLibre-clamped) renders of all three
  offenders into `data/ring-cap-evidence/`:
  `brokkr pmtiles-corpus render --file
  data/tilegen/denmark-locations-da6995f.pmtiles
  -z 9 -x 285 -y 148 -o data/ring-cap-evidence/pre-z9-285-148.svg`
  (same for 9/286/147, and z10/546/260 from
  `data/ocean-tiles.pmtiles`).
- The pinned regress comparand IS
  `data/tilegen/denmark-locations-da6995f.pmtiles` - do not delete it
  during this landing.

**Brick 1 - instruments.** The `all` layer mode (target artifact 5).
Gate: on the pinned archive, `node scripts/validate/earcut-oracle.mjs
data/tilegen/denmark-locations-da6995f.pmtiles all` and `node
scripts/validate/ring-cap-census.mjs
data/tilegen/denmark-locations-da6995f.pmtiles all` run green except
the two known ocean offenders, and the ocean rows byte-match the
single-layer invocations. (The census FIRING here on the known-bad
archive is the fire half of both-direction calibration.)

**Bricks 2-5 - the landing.** Constant, partition + call site,
counters, policy bump, the `ocean.rs` v4 history entry, and the
partition unit tests:

- a synthetic single-tile shape of 1 outer + 600 holes partitions into
  pieces each at or under 500 contours;
- the `i_overlay` XOR of the original shape against the union of the
  pieces is empty (coverage provably unchanged; dev-dep referee, not
  the engine under test);
- a shape at exactly 500 contours is not partitioned;
- two runs produce byte-identical piece streams (determinism).

Test geometry confined to one tile per the convention. Gate: `brokkr
check`. Commit the landing.

**Brick 6 - artifact rebuild** at the landing commit:
`brokkr ocean-build`. Gates:
`elivagar verify data/ocean-tiles.pmtiles --unique-payloads` exit 0;
`node scripts/validate/ring-cap-census.mjs data/ocean-tiles.pmtiles
ocean --unique` 0 over cap;
`node scripts/validate/earcut-oracle.mjs data/ocean-tiles.pmtiles
ocean 0.01 --unique` 0 over threshold, 0 misattached (the artifact
gets its own MapLibre tessellation gate; denmark does not prove the
world-only offender).

**Brick 7 - extract gates** on a fresh
`brokkr tilegen --dataset denmark --variant locations` build
(archive: `data/tilegen/denmark-locations-<landing>.pmtiles`):

- `node scripts/validate/ring-cap-census.mjs <archive> all` - 0 over
  cap (the clear half of calibration).
- `node scripts/validate/earcut-oracle.mjs <archive> all` - 0 over
  threshold, 0 misattached, every polygon layer.
- `elivagar verify <archive>` - exit 0.
- Two-tile corpus preflight (`corpus check` exits 2 on the v4 contract
  by design, so the evidence instrument is the non-writing bless):
  `brokkr pmtiles-corpus bless --file <archive> --corpus corpus/denmark`
  WITHOUT `--rotate` - exit 1, and the appended leaf diff names EXACTLY
  z9/285/148 and z9/286/147. Any third tile is a stop:
  `brokkr regress --file <archive> --against
  data/tilegen/denmark-locations-da6995f.pmtiles --overlay
  data/ring-cap-evidence/overlays` and attribute before any bless.
- Post-change bench, same command as brick 0:
  `brokkr tilegen --bench 3 --dataset denmark --variant locations`.
  Keep/revert reads best-of-3 against brick 0 under the
  `reference/performance.md` noise rules; expected delta zero (the
  partition executes on 2 denmark features and nothing else changed
  on the hot path).
- Human visual gate, canonical renders only: re-render the three
  brick-0 tiles from the new archive/artifact into
  `data/ring-cap-evidence/post-*.svg` and compare. Correct looks
  like: pre-v4 renders MISSING the smallest islands (the clamp at
  work), post-v4 renders showing them, island field continuous, no
  seam or fill discontinuity along the cut line (pieces share exact
  integer cut edges under nonzero fill).

**Brick 8 - rotation commit.**
`brokkr pmtiles-corpus bless --file <archive> --corpus corpus/denmark
--rotate` from the clean landing-commit build, PLUS: append z9/285/148 and
z9/286/147 to `corpus/denmark/manifest.toml` (the hard-tiles ledger is
append-only and every fixed visual bug drops its tile in) so bless
re-renders them into the committed SVG corpus; update
`reference/performance.md` (bench numbers, census/counter readings,
calibration record); close the roadmap H5 offender item; refresh the
v3 wording in `reference/cli.md` (ocean section) and the AGENTS.md
architecture lines that name v3. Commit the corpus diff + docs. The
reviewable diff: two leaf lines, the contract policy version, two new
manifest entries with their SVGs.

No corpus instrument recalibration is owed - digest hashing, canonical
decode, and render core are untouched (`reference/corpus.md` rule).
The census fire (brick 1) and clear (brick 7) is this landing's
both-direction calibration; the XOR test is the coverage-preservation
proof the census alone cannot give (a fix that DELETED 225 holes would
also clear the census).

## Keep/revert

Keep on: all brick 6-7 gates green, the preflight naming only the two
expected tiles, bench within noise. The keep/revert read happens at
brick 7, BEFORE the rotation commit, so revert is:
`git revert` the landing commit, then `brokkr ocean-build` at the
reverted tree to restore a v3 artifact (the file was overwritten in
brick 6; the corpus baseline, never having rotated, still names v3 and
goes back to green by itself). A bench regression outside noise is a
stop-and-attribute; the change has no legitimate cost surface.

## Out of scope, stated

- Any per-feature total-ring policy (named non-goal above).
- `merge_same_attr_geometries` - correct as-is for this invariant; the
  MLT path's ocean-merge inconsistency stays named-not-chased.
- The z8-z14 cross-piece ocean seam (separate roadmap item).
- Optimal ring distribution across pieces - bisection suffices for 3
  offenders worldwide.
- germany/NA archives (no corpus baselines; standing denmark-only
  policy).
