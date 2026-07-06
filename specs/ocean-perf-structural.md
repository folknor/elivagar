# Ocean/polygon emission perf: remove the rectangle-boolean mismatch

**Contract:** `reference/technical-implementation-spec.md`.
**Spawned from:** `notes/rendering-fix-log.md` R24 Landing A verdict (ocean
perf deferred; ocean_ms 50249 at commit `6830301`) and the two independent
perf-hunt reports commissioned via `notes/perf-hunt-brief-2026-07-06.md`
(codex gpt-5.5 xhigh + Fable agent, 2026-07-06 - convergent diagnosis).

**Classification:** structural changes to the integer emission engine's
tile-resolution layer + parallelism; the correctness architecture
(quantize-early, integer booleans for topology, normalize-last) is
untouched.

## 1. Problem and premise (measured)

Bench at `6830301` (plantasjen): total 73854 ms, ocean_ms 50249, RSS
1867 MB. Hotpath (instrumented commit, run after `552172e`):
`intersect_rect` 131.2 s CPU / 2.07M calls (P50 4.6 us, P99 509 us);
`cut_row_bands` 48.2 s; `emit_polygon_feature` 40.6 s;
`normalize` + `simplify_shape_dp` only 16.8 s combined. Ocean wall 50.2 s
vs ~91 s parallel CPU: tail-bound parallelism (3449 piece-granular work
items, P99 660 ms).

Convergent diagnosis (both reports, independently): the engine runs a
general sweep-line boolean, with per-call `Overlay` construction and owned
nested-Vec extraction, for millions of cuts whose clip is an axis-aligned
rectangle and whose answer is usually "the whole rectangle". Interior
tiles are not free; row cutting materializes owned `Shapes` per bisection
level; Tier-2 OSM features boolean the whole shape once per bbox tile;
the ocean parse/quantize/pre-split prologue is serial.

Contract for every landing (unchanged, the only observable contract):
`elivagar verify` PASS; earcut oracle 0 over-threshold / 0 misattached on
all nine polygon layers; per-layer/zoom feature counts within +-2%;
bench keep/revert per landing.

## 2. Survey of the ground

- `int_ocean.rs:907 emit_gap_run`: one PIP decides a run is interior,
  then still booleans EVERY tile in the run against the row shape via
  `emit_clipped_tile_shape` (942). Only `fast_path_rect` (680) rows skip.
- Root cause the gap tiles cannot be trusted free: rasterization
  (`rasterize_shape_edges`, 977) marks tiles crossed by edges against
  UNBUFFERED tile bounds, while emission clips against buffered rects
  (+-128); an edge inside a neighbor's buffer zone is invisible to the
  gap tile's classification, so the clip is what keeps output correct.
- `cut_rows_rec` (1286): two `intersect_rect` + full owned `Shapes` per
  internal node.
- `emit_normalized_per_tile` (`pipeline/emit.rs:376`, Tier 2): booleans
  the FULL shape once per tile of the bbox (<= 64), no boundary/interior
  classification at all.
- `Overlay::with_shapes_options` per call (int_ocean.rs:202): fresh
  segments Vec, SplitSolver, GraphBuilder, BooleanExtractionBuffer;
  `clean_shapes` (542) then copies the output ring-by-ring AGAIN.
  i_overlay 7.0.2 offers `Overlay::new_custom` + `clear()` (buffer-
  preserving, core/overlay.rs:68,244), `overlay_into` writing a reusable
  `FlatContoursBuffer` (386), `add_contour`/`add_flat_buffer` input reuse
  (191-252). Caveat (codex report, build/boolean.rs:46): fills+graph are
  still rebuilt per clip - reuse eliminates allocation churn only.
- Ocean prologue (`ocean.rs:79-230`): record parse, quantize, data-bounds
  intersect, z8 pre-split - all serial, and the pre-split loops per-tile
  booleans of the full piece. `SPLIT_MIN_VERTICES` gate counts ONLY the
  outer ring (`piece.first().map_or(0, Vec::len)`, ocean.rs:199) - hole-
  heavy pieces dodge splitting.
- Rayon fold (`ocean.rs:281`) parallelizes over pieces only; per-zoom
  iterations inside `emit_ocean_polygon` (374) are independent (records
  are externally sorted afterwards - ordering is irrelevant).
- `boundary_tiles: HashSet<u64>` (SipHash) rebuilt per zoom
  (int_ocean.rs:30); consumer immediately re-buckets into per-row sorted
  Vecs.
- `push_quantized_pieces` (`ocean.rs:322`) booleans against `data_rect`
  even when the shape bbox is fully inside (the common case).
- `encode_tile_shape` (int_ocean.rs:777,790) allocates a ring Vec per
  contour + a ring_refs Vec per tile; ocean's `encode_feature_data`
  re-encodes (empty) attrs per feature (`wire_format.rs:307`).
- Deferred by both reports (do NOT do here): cross-zoom LOD cascade,
  full edge-arena engine, coverage-span emitter beyond Landing 1's
  dilated-rasterization subset, DP/normalize tuning.

## 3. Landings

Every landing is one commit and runs THE STANDARD GATE BLOCK below
verbatim (from the repo root; the oracle output redirects preserve the
tables for the next landing's parity comparison), then commits, then runs
the bench and reads its stated accepted-cost bound as the keep/revert
verdict. Bench baseline for Landing 1's comparison: ca904211 at `6830301`
(total_ms 73854, ocean_ms 50249, rss 1867 MB).

STANDARD GATE BLOCK (H = $(git rev-parse --short HEAD)):
```
brokkr check
brokkr tilegen --dataset denmark
./target/release/elivagar verify data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles
mkdir -p notes/qa
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles ocean 0.01 > notes/qa/oracle-ocean-$(git rev-parse --short HEAD).txt
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles water_polygons 0.01 > notes/qa/oracle-water_polygons-$(git rev-parse --short HEAD).txt
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles land 0.01 > notes/qa/oracle-land-$(git rev-parse --short HEAD).txt
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles buildings 0.01 > notes/qa/oracle-buildings-$(git rev-parse --short HEAD).txt
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles sites 0.01 > notes/qa/oracle-sites-$(git rev-parse --short HEAD).txt
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles dam_polygons 0.01 > notes/qa/oracle-dam_polygons-$(git rev-parse --short HEAD).txt
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles pier_polygons 0.01 > notes/qa/oracle-pier_polygons-$(git rev-parse --short HEAD).txt
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles street_polygons 0.01 > notes/qa/oracle-street_polygons-$(git rev-parse --short HEAD).txt
node scripts/validate/earcut-oracle.mjs data/tilegen_tmp/denmark-$(git rev-parse --short HEAD).pmtiles bridges 0.01 > notes/qa/oracle-bridges-$(git rev-parse --short HEAD).txt
```
Pass criteria: verify exits 0; every oracle file shows 0 over_thresh and
0 misattached at every zoom; the per-zoom `features` column of each
oracle file is within +-2% of the previous landing's corresponding
notes/qa/oracle-<layer>-<prevhash>.txt (compared by reading both files;
the notes/qa files are gitignored working artifacts... if notes/ is not
gitignored, delete them before commit - they are gate evidence, recorded
in the ledger, not repo content).
Bench command after commit: `brokkr tilegen --bench --dataset denmark`;
read `total_ms`/`ocean_ms`/peak RSS from its kv output.

### Landing 1 - free interior tiles (dilated rasterization) + exact fixes

Rasterize edges DILATED by the tile buffer: mark every tile whose
BUFFERED rect (+-128 pixel units) an edge touches (equivalently:
rasterize each segment against the grid with coordinates expanded by
128/4096 of a tile - implement as marking the up-to-4 tiles whose
buffered rect contains each DDA-visited cell corner region; exactness
rule: over-marking is safe, under-marking is the bug, same as Landing 1
of spec 2). Consequences, all landed together:
- Gap runs and interior rows emit `emit_full_tile` DIRECTLY (a tile with
  no dilated-boundary marks has a fully-covered buffered rect; the
  boolean it replaces provably returned exactly that rect). The
  `fast_path_rect` special case and its debug_assert become dead - delete
  them.
- Tier-3/ocean boundary tiles: unchanged (still clipped).
- `SPLIT_MIN_VERTICES` counts total shape vertices (all rings).
- `push_quantized_pieces`: skip the data-bounds boolean when
  `shape_bbox` is contained in `data_rect`; skip per-tile pre-split
  booleans when the piece bbox is inside one split tile (existing) AND
  when a split tile's rect contains the whole piece bbox (emit the piece
  unchanged for that tile).
- `boundary_tiles` HashSet -> `FxHashSet` (rustc-hash is a dependency).
Named unit tests (int_ocean.rs cut/rasterize test modules):
- `dilated_rasterize_marks_neighbor_within_buffer` (segment 128 units
  from a tile edge marks that tile; 129 units away does not; exactly at
  the buffered boundary marks both).
- `dilated_rasterize_never_undermarks_grid_corner_ties` (45-degree
  corner crossings, both side tiles + diagonal, dilated variant).
- `gap_tile_bytes_identical_to_boolean_path` (fixture: coastal row
  shape; for each gap tile, encode via emit_full_tile AND via the old
  intersect_rect path; assert identical DECODED ring vertex cycles -
  rotation/direction-normalized; byte identity is deliberately not
  claimed, see Landing 1 text).
- `split_min_vertices_counts_all_rings` (hole-heavy piece above the
  threshold only when holes are counted -> gets split).
- `bounds_boolean_skipped_when_bbox_contained` (contained bbox emits
  the shape unchanged, no boolean - assert via call-count test seam or
  by equality of output on a shape a boolean would have re-noded).
Accepted-cost bound (keep/revert): total_ms <= 73854 AND
ocean_ms <= 30000 AND rss_mb <= 1961 (1.05x baseline). Expectation is
far lower ocean_ms; the bound is the floor of "must clearly move".

### Landing 2 - allocation-churn removal (i_overlay reuse + sinks)

- `IntEmitScratch` gains a persistent `Overlay` (new_custom + clear) and
  reusable rect-contour storage; `intersect_rect` becomes
  `intersect_rect_into(&mut scratch, ..., out: &mut Shapes)` (clear+reuse
  output Vecs); `normalize` single-contour common case goes through
  `simplify_contour`-style early-out (no graph build when already clean).
- `clean_shapes` operates in place on the overlay output (no second
  copy).
- `encode_tile_shape`: encode from translated slices via scratch-owned
  buffers; no per-contour ring Vec, no per-tile ring_refs Vec.
- Ocean pre-encodes its (empty) attrs once per layer instead of per
  feature.
- Sink: per-batch SoA arena in `OceanAcc`/emit scratch - one Vec<u8>
  payload + Vec<(key, offset, len)> index, sorted by index, written by
  slice; the cross-phase chunk FILE format is untouched.
Accepted-cost bound (keep/revert): total_ms <= Landing-1 bench total_ms
AND ocean_ms <= Landing-1 ocean_ms AND rss_mb <= 1.05x Landing-1 rss -
strict: this landing is pure allocation removal, any wall-time
regression on either metric = revert.

### Landing 3 - ocean parallelism restructure

- Parse+quantize+bounds-intersect parallel over `.shx` record offsets
  (shared mmap, rayon); pre-split inside the same pass.
- Emission fan-out over (piece x zoom) work items instead of pieces
  (records are externally sorted; ordering irrelevant; chunk-flushing
  accumulator already granularity-agnostic).
Accepted-cost bound (keep/revert): ocean_ms <= 10000 AND
total_ms <= Landing-2 bench total_ms AND rss_mb <= 1.10x Landing-2 rss
(the fan-out duplicates scratch per work item; 10% RSS is the accepted
price). After L1+L2 the parallel CPU shrinks; 24-way fan-out over ~50K
items should approach the serial-parse floor.

### Landing 4 - close-out (a stopping rule, not deferred work)

Run `brokkr tilegen --hotpath --dataset denmark` at the Landing-3 commit
and record the profile. Then CLOSE this spec: record final numbers in
ledger R25 and update CLAUDE.md baselines (commit first, then bench,
then write hash-anchored numbers - benchmark discipline). This spec's
scope ends here regardless of the reading. The boundary-window work
(x-bisection of row shapes, Tier-2 unification through the shared
classifier) and the deeper rewrites are NAMED, OUT-OF-SCOPE follow-ups
owned by `notes/perf-hunt-reports-2026-07-06.md` items 1-3; if Landing
3's bound was met the follow-ups are optional; if the profile shows
`intersect_rect` still dominant they are the next spec's premise, with
this landing's profile as its ready-made survey. Nothing in this spec's
originating problem (the R24 deferred-perf item) requires them: the R24
item is discharged by the Landing-3 bound or by this close-out
recording why not.

## 4. Stopping rule / out of scope

Coverage-span/edge-arena engine rewrite, cross-zoom LOD cascade, DP or
normalize changes, MVT/wire format changes, hotpath/mlt-core dep bumps,
NA/planet runs. The three-tier OSM routing stays as-is except where
Landing 1's rasterizer change applies mechanically to Tier 3.

## 5. Risks, resolved inline

- Dilated rasterization over-marks (some true-interior tiles classified
  boundary): costs one redundant clip each, output identical - the
  exactness rule is asymmetric by design.
- Buffered full-tile emission for newly-freed gap tiles: emits the same
  rect the boolean returned (proven by the Landing-1 fixture test), so
  byte parity per tile.
- (piece x zoom) fan-out changes record production ORDER only; the
  external sort is the ordering authority (same reason Landing B's
  parity held).
- Overlay reuse across calls: i_overlay's clear() is documented
  buffer-preserving; the Landing-2 bound (strictly no regression)
  reverts it if the persistent buffers interact badly with rayon worker
  counts (memory).
