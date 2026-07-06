# Perf-hunt reports (2026-07-06): full findings + disposition

Two independent analyses of the integer polygon emission engine,
commissioned via `notes/perf-hunt-brief-2026-07-06.md` (codex gpt-5.5
xhigh and a Fable agent; neither read history, both read code + the
hotpath profile). Preserved here in full so nothing is lost; the
disposition table at the end maps each item to spec 3
(`specs/ocean-perf-structural.md`) or to the leftovers backlog.

---

## Report 1: Fable agent

### Where the CPU actually goes (code-verified reading of the profile)

The 131s in `intersect_rect` (src/geometry/int_ocean.rs:195) is not one
bottleneck, it is four distinct call populations, each with its own fix:

1. Row-band bisection - `cut_rows_rec` (int_ocean.rs:1286) calls
   `intersect_rect` twice per bisection node, materializing owned
   `Shapes` at every internal level. 48s attributed to `cut_row_bands`
   is almost entirely these calls.
2. Gap-run tiles - `emit_gap_run` (int_ocean.rs:907) runs a full
   sweep-line boolean per interior tile whose provable output is the
   buffered tile rectangle. It clips against the entire row shape, so
   cost per interior tile is O(row vertex count). This is the most
   indefensible cost in the engine: the only tiles that escape it are
   rows whose cut result is exactly a rectangle (`fast_path_rect`,
   int_ocean.rs:680). Any row containing one boundary tile pays a
   boolean for every ocean tile in the run.
3. Boundary tiles - `emit_clipped_tile_shape` (int_ocean.rs:942) also
   clips against the whole row shape, so a row with V vertices and C
   boundary tiles costs O(V*C), not O(V log C). The heavy P95/P99 tail
   is exactly these large-V inputs.
4. Tier-2 OSM features - `emit_normalized_per_tile`
   (pipeline/emit.rs:376) is the worst offender structurally: for a
   bbox of up to 64 tiles, it does intersect_rect against the full
   shape for every tile in the bbox - no boundary detection, no
   interior fast path, no bisection. An 8x8-tile lake is 64 full
   booleans of the whole ring. Multiplied by 4.5M emit_polygon_feature
   calls this is a large slice of both the 131s and the 40.6s.

On top of that, every one of the 2.07M calls constructs a fresh
`Overlay` (int_ocean.rs:202): new segments Vec, new SplitSolver, new
GraphBuilder, new BooleanExtractionBuffer, then materializes nested
Vec<Vec<Vec<IntPoint>>> output which `clean_shapes` (int_ocean.rs:542)
immediately copies again ring-by-ring. The i_overlay API explicitly
supports the opposite: `Overlay` is a persistent object with `clear()`
(keeps segments capacity, split_solver, graph_builder, boolean_buffer -
core/overlay.rs:68-108,243-246), and `overlay_into(rule, fill, &mut
FlatContoursBuffer)` (core/overlay.rs:386) writes flat points+ranges
output with no nested allocation. `add_flat_buffer` (core/overlay.rs:252)
accepts flat input. None of this is used.

Finally, the ocean phase's 50.2s wall vs ~91s parallel CPU tells you
parallelism is poor: the shapefile parse + quantize + z8 pre-split
(ocean.rs:79-230) is fully serial, and the rayon fold parallelizes over
pieces only (3,449 of them, P99 660ms per piece) - classic long-tail
skew.

`simplify_shape_dp`, `normalize`, `rescale_shape` are confirmed
non-problems (~17s combined); leave them alone.

### R1. Kill the general boolean for axis-aligned cuts (the big one)

Bottleneck: ~all of intersect_rect's 131s is spent doing a full
sweep-line boolean (split -> graph build -> extract) for cuts against
axis-aligned rectangles, on shapes that normalize has already made
simple and correctly wound.

Why the structure causes it: intersect_rect is the single primitive for
row cutting, tile clipping, gap tiles, tier-2 clipping, pre-split, and
data-bounds clipping. A general boolean is the right tool for exactly
one of those jobs (topology repair in normalize); everywhere else the
clip geometry is a rectangle and the subject is already simple.

The redesign (one coherent change, three parts):

- (a) Dilated boundary rasterization -> free interior tiles.
  rasterize_shape_edges (int_ocean.rs:977) marks tiles crossed by edges
  using unbuffered tile bounds. Because the emitted tile rect is
  buffered by 128/4096, a gap tile can't be trusted to be edge-free in
  its buffer zone, so emit_gap_run must clip. Rasterize with coordinates
  dilated by TILE_BUFFER_I32/TILE_EXTENT_I32 instead (mark any tile
  whose buffered rect an edge touches), and every gap tile becomes
  emit_full_tile - zero booleans, byte-identical output (a shape
  covering the whole buffered rect intersects to exactly that rect,
  which is what emit_full_tile at int_ocean.rs:955 emits). The existing
  debug_assert_no_boundary_in_fast_tiles shows the containment
  invariant is already understood. For ocean at z12-14 this deletes the
  majority of per-tile boolean calls outright.

- (b) Dedicated integer rect clipper for the remaining cuts. Implement
  an O(n) axis-aligned clip (four half-plane passes, RectClip-style,
  exact i64 intersection arithmetic) for: row-band cuts, boundary-tile
  clips, the z8 pre-split, and the data-bounds clip in
  push_quantized_pieces. The known failure mode of half-plane clipping -
  a concave subject crossing a clip edge twice produces one bridged
  ring instead of two components - is precisely what i_overlay's
  ContourDecomposition::decompose_contours (core/divide.rs:34) solves
  cheaply (sort-based duplicate-point splitting, no sweep). Pipeline:
  half-plane clip -> if the output ring has no repeated points, done;
  else decompose_contours -> classify components by winding/area,
  re-nest holes by the existing point_in_contour. Fall back to the full
  Overlay boolean only if that classification fails a cheap check. The
  earcut oracle + verify are the gate; min_output_area filtering is
  replicated by the existing contour_area_is_below.

- (c) Bisect in x as well as y. emit_boundary_and_gap_tiles clips each
  boundary tile against the whole row shape. Extend the cut_rows_rec
  recursion into the x axis (or just re-use cut_row_bands transposed on
  each row shape) so every leaf clip sees only a local fragment. This
  turns the O(V*C) boundary-row cost into O(V log C) and - critically -
  collapses the P95/P99 tail, which is what caps ocean wall time given
  the parallelism skew.

Why plausibly high-payoff: part (a) alone deletes the largest call
population; parts (b)+(c) convert the remainder from O(V*C) sweep-line
booleans (with per-call graph construction) into O(V log C) linear
scans. Between them this attacks essentially the entire 131s plus the
48s cut_row_bands slice, on both ocean and OSM paths.

Risks: (b) is the risky part - degenerate cases (hole tangent to clip
edge, collinear runs on the clip line, hole escaping through the cut)
are exactly what the earcut oracle exists to catch; keeping the
i_overlay fallback behind a strict validity check bounds the blast
radius. (a) and (c) are near-risk-free (same primitive, same outputs).
Per the brief's style: do the full intrusive change, run the oracle on
every polygon layer, keep or revert.

### R2. One persistent flat-buffer boolean engine per worker

Bottleneck: for every boolean that survives R1 (all of normalize, the
fallback path, and everything until R1 lands): fresh Overlay
construction, nested-Vec extraction, then the redundant clean_shapes
copy - per call, 2.07M times today.

Redesign: put one Overlay<i32> plus two FlatContoursBuffers in
IntEmitScratch (it's already threaded through everything). Replace the
slice.simplify(...) in normalize (int_ocean.rs:186) with
overlay.simplify_flat_buffer / simplify_contour (core/simplify.rs:92,183)
- note simplify_contour's find_intersections early-out returns without
any graph build or extraction when the ring is already clean, which is
the common case for post-DP single-ring shapes (586K normalize calls).
Replace Overlay::with_shapes_options(...).overlay(...) with clear() +
add_contours + overlay_into(&mut flat). Then go the rest of the way:
make Shape/Shapes BE FlatContoursBuffer (contiguous points + ranges)
through the whole engine - cut_row_bands levels, rescale_shape,
simplify_shape_dp, encode_tile_shape all currently allocate a fresh
Vec<IntPoint> per ring per level per zoom. The brief says the
Shape/Shapes structures are not sacred; a flat representation removes
the per-ring allocation storm and is a strictly better memory layout
for the DP and area scans too.

Payoff: the P50 of intersect_rect is 4.62us - at that size,
construction + extraction IS the call. Millions of calls x setup cost,
plus the whole nested-Vec churn in the hot loop. High conviction,
mechanical correctness (output values unchanged, only containers
change).

Risk: low. Invasive but semantics-preserving; parity gates should pass
trivially.

### R3. Restructure ocean-phase parallelism

Bottleneck: 50.2s wall against ~91s parallel CPU. Two causes visible in
ocean.rs: (1) parse + quantize + pre-split (ocean.rs:79-230) are serial
- including intersect_rect calls against the largest coastline polygons
in the whole dataset; (2) the rayon fold at ocean.rs:281 parallelizes
over 3,449 pieces with P99 660ms - the wall time of the parallel
section is set by the fattest pieces.

Redesign: parallelize record parsing/quantization over .shx offsets
(each record is independent; the mmap is shared); do the z8 pre-split
with the R1 clipper inside the same parallel pass (bisected, not the
current per-tile clip of the full piece - that loop at ocean.rs:214-224
is O(V x split-tiles) serial work today). Then fan the emit out over
(piece x zoom) work items instead of pieces: the 15 zoom iterations in
emit_ocean_polygon (ocean.rs:374) are fully independent (each builds
its own records; ordering is irrelevant - everything is externally
sorted afterwards). 15x more, 15x smaller work items flattens the tail.
Also lower SPLIT_MIN_VERTICES gating to total shape vertices - it
currently only counts the outer ring (piece.first().map_or(0, Vec::len),
ocean.rs:199), so hole-heavy pieces dodge the split.

Payoff: even with zero per-call improvement this converts ~50s wall
toward 91s/24 ~= 4s + serial parse. Combined with R1 shrinking the tail
pieces, ocean stops being the dominant phase.

Risk: low; chunk-flushing accumulator already supports arbitrary work
granularity. Slight duplicate scratch memory per work item.

### R4 (lower conviction): cross-zoom derivation cascade

The per-zoom loop re-derives everything from the z14 base at every zoom
(prepare_shape_for_zoom, emit_shape_for_zoom). A cascade (derive zoom z
from zoom z+1's simplified/normalized result, halving coordinates)
would shrink the input to rescale/DP/normalize/tiling geometrically
instead of paying full base vertex count 15 times. It is legal under
the stated contract (verify + oracle + counts-within-noise, not
geometry parity), and after R1 the per-zoom tiling cost is proportional
to input vertex count, so cascading multiplies R1's win. But: cascaded
DP compounds error and can collapse thin coastal features differently
enough to move feature counts at low zooms, and DP+rescale are only
~10s of the profile today. Hold until R1-R3 land and re-profile; if
per-zoom rescale/DP shows up as the new top, do it then - as a full
switch, not a knob.

### Medium-value local changes

- Tier-2 unification (pipeline/emit.rs:376): route
  emit_normalized_per_tile through the same boundary-rasterize +
  interior-fast-path + bisect machinery as emit_shape_for_zoom instead
  of per-tile booleans over the whole bbox. If R1 lands this is nearly
  free (same primitives); even standalone it's a direct cut into the
  40.6s OSM polygon cost. The 64-tile threshold then stops mattering
  and the tier seam can be deleted - one path for all multi-tile
  polygons.
- Sink flattening (src/sort.rs:114, wire_format::encode_feature_data):
  every feature allocates a Box<[u8]>, and write_sorted_chunk
  (sort.rs:347) then re-copies all of them into one contiguous buffer
  anyway. Replace the per-batch Vec<SortRecord> with an SoA arena - one
  Vec<u8> payload + Vec<(key, offset, len)> index; sort the index;
  write payload slices in index order. Eliminates millions of small
  allocations and the serialize copy. The comment in sort.rs:106-113
  rejecting arenas protects the cross-phase chunk file format, which
  this doesn't touch - only the in-memory batch before
  write_sorted_chunk, which ocean already owns privately (OceanAcc).
- boundary_tiles: HashSet<u64> (int_ocean.rs:30) uses std SipHash on
  millions of inserts per zoom; switch to FxHashSet (already a
  dependency) or, better, a per-row bitset over tx_min..=tx_max - the
  consumer immediately re-buckets it into sorted per-row Vecs anyway
  (int_ocean.rs:819-830).
- Bbox early-outs: push_quantized_pieces (ocean.rs:322) runs a full
  boolean against data_rect even when the shape bbox is entirely inside
  it (the overwhelming case for an in-bounds extract); same for
  pre-split tiles fully containing/contained by a piece. One shape_bbox
  containment test skips the call.
- rect_shape allocation (int_ocean.rs:227): a heap Vec<Vec<IntPoint>>
  per clip call; with R2's persistent overlay this becomes
  add_contour(&[p0,p1,p2,p3]) on a stack array.

### Sequencing (Fable)

R2 (mechanical, de-risks everything) -> R1a (dilated rasterization,
biggest single deletion) -> R3 (parallelism) -> R1b/R1c (rect clipper +
x-bisection) -> tier-2 unification -> sink flattening -> re-profile
before deciding on R4. Each step is a complete change gated on verify +
oracle + counts, benchmarked, keep-or-revert.

One-sentence version: the engine pays a general-purpose sweep-line
boolean, with per-call construction, for millions of cuts whose clip
geometry is an axis-aligned rectangle and whose answer is usually "the
whole rectangle" - remove that mismatch (R1) and amortize what's left
(R2), and the 50s ocean phase should compress toward its serial-parse
floor once the parallel fan-out (R3) stops being tail-bound.

---

## Report 2: codex gpt-5.5 xhigh

Bottom line: the biggest remaining win is to stop using intersect_rect
as the unit of work. Right now the engine asks i_overlay millions of
tiny rectangle questions, and each answer is materialized as owned
Shapes before being re-encoded. Visible in intersect_rect
(int_ocean.rs:195), cut_row_bands (1271), emit_clipped_tile_shape (942),
and OSM tier-2 at emit_normalized_per_tile (pipeline/emit.rs:376).

### High-conviction architectural rewrites

1. Replace row/tile clipping with a coverage-span emitter.
   Bottleneck: interior tile runs still call intersect_rect tile by
   tile. emit_gap_run uses one PIP test to decide a run is filled, then
   still calls emit_clipped_tile_shape for every tile in the run
   (int_ocean.rs:907). Only the narrow fast_path_rect case emits full
   tiles (840).
   Why: the engine has no first-class "full buffered tile" / "empty
   tile" / "boundary tile" model - it has row-shaped polygons and
   per-tile boolean clipping.
   Redesign: for each normalized zoom shape, build row edge buckets and
   buffered boundary columns. For each tile row, compute filled x-spans
   with holes applied, expand boundary columns by the tile buffer, emit
   emit_full_tile for interior spans, and clip only boundary columns.
   Apply to ocean AND OSM tier-3.
   Payoff: directly attacks the 2.07M intersect_rect calls; most large
   ocean pieces are area, not coastline - full interior tiles should be
   O(1) emission, not boolean overlays.
   Risks: buffer semantics, edges exactly on grid lines, holes, ring
   orientation. Needs oracle coverage, but it is the right first big
   rewrite.

2. Delete recursive owned row-band cutting; slice once, stream fragments.
   Bottleneck: cut_row_bands recursively intersects upper/lower row
   ranges and materializes Vec<Shapes> at every level (int_ocean.rs:
   1281,1316). Then boundary/gap tile clipping intersects again.
   Why: row cutting is implemented as repeated polygon booleans because
   Shape/Shapes is the handoff type between every stage.
   Redesign: represent per-zoom geometry as an edge/ring arena with row
   buckets. Row traversal produces tile work items directly: full-tile
   spans plus boundary windows. Boundary clipping becomes a grid/window
   operation over the arena, not "make a new owned polygon, then clip
   it again."
   Payoff: removes the 48s cut_row_bands line and a major source of
   owned intermediate contours; makes the full-tile fast path natural
   instead of a special rectangle detector.
   Risks: a real engine rewrite. Hole attachment and self-touching
   normalized output are the hard parts. I would still do it.

3. Build a multizoom polygon/tile-cover pipeline instead of deriving
   every zoom independently.
   Bottleneck: ocean loops every piece over every zoom (ocean.rs:373);
   emit_shape_for_zoom rescales, simplifies, normalizes, rasterizes,
   row-cuts, and clips independently (int_ocean.rs:740). OSM polygons
   do the same through prepare_shape_for_zoom and tier routing
   (emit.rs:287,627).
   Redesign: build an adaptive spatial/LOD tree from the quantized base
   polygon. Cache per-zoom simplified boundary, bbox, row buckets, and
   tile-cover state. Parent zooms reuse or aggregate child cover
   classifications where legal, not rediscover membership from the base
   polygon.
   Payoff: current cost is multiplied across 15 zooms; compounds with
   the coverage-span rewrite. Less urgent than killing per-tile
   overlays.
   Risks: simplification parity and pinned OSM vertices. Keep behind
   the same full rewrite, not as a separate micro-optimization.

### i_overlay findings (codex)

Current code uses Simplify for normalization and one-shot
Overlay::with_shapes_options(...).overlay(Intersect, NonZero)
(int_ocean.rs:191,202). i_overlay does expose reusable state:
Overlay::new/new_custom keep segment, split, graph, and boolean buffers
(core/overlay.rs:68); clear reuses the object (244); overlay_into
writes flat contours (386); extraction buffers are explicitly reusable
(core/extract.rs:34). BUT build_boolean_overlay still rebuilds fills
and graph for each clip geometry (build/boolean.rs:46). So reusable
i_overlay is a medium local win, not the main answer. build_graph_view
helps when extracting multiple rules from one graph; our clip rectangle
changes per tile, so it does not remove the structural problem.

### Medium-value local changes (codex)

- Add IntOverlayScratch and replace intersect_rect(shape, rect) ->
  Shapes with intersect_rect_into(..., scratch, out). Reuse
  Overlay::new_custom, clear, rect contour storage, and
  BooleanExtractionBuffer. Reduces allocation churn but still one sweep
  per rectangle.
- Use overlay_into only where flat contours are sufficient or where a
  new encoder can classify rings itself. Avoids nested Vec<Vec<Vec<_>>>
  but does not preserve hole grouping by itself - not a drop-in for all
  current Shapes uses.
- Stop allocating in encode_tile_shape: fresh ring Vec per contour and
  fresh ring_refs vector per tile (int_ocean.rs:777,790). Scratch-owned
  ring refs or encode from offset IntPoint slices directly.
- Precompute full-tile geometry and avoid ocean's per-feature attrs
  allocation: ocean calls encode_feature_data which allocates an attrs
  buffer each time (ocean.rs:378, wire_format.rs:307); OSM already
  pre-encodes attrs. More important once full-tile emission is cheap.
- Do NOT spend first effort on solver knobs, generic writer cleanup, or
  DP tweaks. Measured DP/normalize cost is not where the wall time is,
  and i_overlay API reuse cannot compensate for asking millions of
  independent rectangle-overlay questions.

---

## Disposition

Adopted into `specs/ocean-perf-structural.md`:
- Fable R1a / codex #1 (interior-tiles-free subset): spec Landing 1
  (dilated rasterization; fast_path_rect deleted as dead).
- Fable R2 / codex IntOverlayScratch + encode_tile_shape + attrs +
  sink SoA: spec Landing 2.
- Fable R3 (parallel prologue, piece x zoom fan-out,
  SPLIT_MIN_VERTICES all-rings): spec Landing 3.
- Fable bbox early-outs, FxHashSet: spec Landing 1.
- Fable R1c (x-bisection) + tier-2 unification: spec Landing 4
  decision point, gated on the fresh profile.

LEFTOVERS (preserved, not scheduled - revisit after spec 3's Landing 4
profile):
1. Fable R1b: dedicated O(n) integer rect clipper (half-plane passes +
   i_overlay decompose_contours for concave splits + point_in_contour
   re-nesting, full-boolean fallback behind a strict validity check).
   The largest remaining algorithmic win if boundary-tile clipping
   still dominates after Landings 1-3.
2. Codex #1 full coverage-span emitter: first-class full/empty/boundary
   tile model with filled x-spans and hole application at the span
   level; supersedes rasterize+PIP+row-cut entirely. Strictly stronger
   than Landing 1's subset; the end-state if ocean must approach
   planetiler-class throughput.
3. Codex #2 edge/ring arena engine: per-zoom geometry as a flat edge
   arena with row buckets; boundary clipping as grid/window operations;
   eliminates owned Shape handoffs everywhere. The deepest rewrite on
   the table.
4. Fable R4 / codex #3 cross-zoom LOD cascade or tree: derive zoom z
   from z+1 (or an adaptive LOD tree with cached per-zoom cover state)
   instead of re-deriving from base 15x. Legal under the contract;
   risks count drift at low zooms; only worth it once per-zoom
   tiling is linear-cost.
5. Fable full-flat Shape/Shapes representation (FlatContoursBuffer as
   THE engine type, not just an I/O buffer at the overlay boundary) -
   Landing 2 only adopts the boundary-level reuse; the fully-flat engine
   representation is deferred with #3.
6. Fable per-row bitset for boundary tiles (Landing 1 only swaps to
   FxHashSet; the bitset-over-x-range representation goes with #2/#3).
7. Codex build_graph_view multi-rule extraction: only pays off if some
   future design extracts multiple overlay rules from one graph (e.g.
   Intersect + Difference per tile for land/water complements).
8. Landing 3 review: `read_ring_points` (src/ocean.rs) allocates a fresh
   `RING_READ_BUFFER_BYTES` (64 KB) `Vec<u8>` per ring in the hot parse
   path. Threading a reused per-record (or per-thread) buffer through
   the parallel parse would cut parse-path allocation - helping both
   ocean_ms and RSS - but is a real refactor of a working path (buffer
   lifetime now has to survive across the rayon closure boundary),
   deliberately deferred as out-of-scope for Landing 3 (surgical, no
   gold-plating). Also observed: the `read_exact_at` calls in
   `read_ring_points` and its caller are unconditional, but the trait
   import that provides them (`std::os::unix::fs::FileExt`, ocean.rs:18)
   is `#[cfg(unix)]`-gated - a non-unix target would fail to compile.
   Acceptable for this Linux-only project; a proper fix needs an mmap
   fallback for non-unix and is out of scope here.
