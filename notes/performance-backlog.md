# Perf-hunt open items (polygon emission + full pipeline)

Consolidated, deduplicated backlog distilled from independent analyses.

## Open items

### 1. Dedicated O(n) integer rect clipper (Fable R1b)

**The largest remaining algorithmic win** if boundary-tile clipping
still dominates. `intersect_rect` is the single primitive for row
cutting, tile clipping, tier-2 clipping, pre-split, and data-bounds
clipping; a general boolean is the right tool for exactly one of those
(topology repair in `normalize`) - everywhere else the clip geometry
is a rectangle and the subject is already simple.

Implement an O(n) axis-aligned clip (four half-plane passes,
RectClip-style, exact i64 intersection arithmetic) for row-band cuts,
boundary-tile clips, the z8 pre-split, and the data-bounds clip in
`push_quantized_pieces`. Known failure mode of half-plane clipping - a
concave subject crossing a clip edge twice yields one bridged ring
instead of two components - is exactly what i_overlay's
`ContourDecomposition::decompose_contours` (core/divide.rs) solves
cheaply (sort-based duplicate-point splitting, no sweep). Pipeline:
half-plane clip -> if output ring has no repeated points, done; else
`decompose_contours` -> classify components by winding/area, re-nest
holes via the existing `point_in_contour`. Fall back to the full
`Overlay` boolean only if that classification fails a cheap validity
check (bounds the blast radius). `min_output_area` filtering is
replicated by the existing `contour_area_is_below`.

Risk: the risky item. Degenerate cases (hole tangent to clip edge,
collinear runs on the clip line, hole escaping through the cut) are
exactly what the earcut oracle exists to catch; the strict-validity
i_overlay fallback keeps it bounded. Gate on oracle + verify, every
polygon layer, keep-or-revert.

Alternative primitive (per-zoom emission report): an exact integer
split-by-axis-line instead of the four half-plane passes. Split by x=c
or y=c: walk the ring, compute crossings (exact rationals since the cut
line is on the grid), snap-round, sort crossings along the line,
reconnect. O(V + K log K) with tiny constants; plausibly 10-50x cheaper
per cut than the general boolean. Same correctness burden - snap-round
crossings can create slivers/self-touches (the R23 class). Mitigation:
keep normalize/i_overlay as a debug-assert oracle on the splitter's
output during bring-up; earcut oracle as the gate. That report
sequences it as a SECOND landing: land the recursive descent (item 10)
on trusted i_overlay booleans first, then swap the splitter in as a
separate measured landing.

Related fast path (tile-ownership report): after normalization, once a
ring is known simple and has no holes, i_overlay per boundary tile is
heavy - add a simple-ring rectangle-clip fast path for that case. Still
a local patch relative to items 1/2's full replacement, but a focused
cut into the boundary-tile boolean cost.

### 2. Coverage-span emitter (codex #1, full)

First-class full / empty / boundary tile model - the strictly stronger
end-state that supersedes rasterize + PIP + row-cut entirely, and
subsumes Landing 1's dilated-rasterization subset and Fable's
x-bisection stopgap (R1c, below). Today the engine has no such model;
it has row-shaped polygons and per-tile boolean clipping.

For each normalized zoom shape, build row edge buckets and buffered
boundary columns. Per tile row: compute filled x-spans with holes
applied, expand boundary columns by the tile buffer, `emit_full_tile`
for interior spans, and clip only boundary columns. Apply to ocean AND
OSM tier-3. Most large ocean pieces are area, not coastline, so full
interior tiles become O(1) emission instead of boolean overlays.

Weaker intermediate if the full rewrite is too much at once - **R1c
x-bisection** (Fable), independently proposed as **A4** by the pipeline
perf report: extend the row-band bisection into the x axis (recursive
2D cut - reuse `cut_row_bands` transposed per row) so every leaf
boundary clip sees only a local fragment. Today
`emit_normalized_shape_for_zoom` (int_ocean.rs:879) is asymmetric: rows
are cut by recursive bisection (`cut_row_bands_with_scratch`,
O(V log R) - good), but within each row every boundary tile clips the
entire row shape (`emit_clipped_tile_shape(boundary_tx, ty, row_shape,
...)` at int_ocean.rs:983), so a coastline row with B boundary tiles
and a row shape of V vertices pays B x O(V) i_overlay booleans - this
is where the 159s lives (gap tiles are already free via the dilated
rasterization). Recursively bisecting each row shape in x down to
single boundary tiles (skipping gap runs, which the boundary-tile set
already locates) makes each leaf clip operate on a few-tiles-wide
shape: O(V log B) per row instead of O(V B). Same primitive
(`intersect_rect_into`, min_area 0), same normalize-at-leaf semantics
as the row cut, so the spec-3 correctness argument carries over. Helps
OSM polygons too (`emit_shape_for_zoom` is the shared engine;
land/water_polygons fan out to up to 247 tiles at z14). Near-risk-free
in principle - but x-cuts need the same buffered-half-rect treatment
the row cut uses (buffered tile rects overlap by +-128 units between
neighbors), and the "leaf equals direct clip" ancestor-containment
property must hold in x as it does in y. Earcut oracle is the gate, run
on every polygon layer.

Risk (full emitter): buffer semantics, edges exactly on grid lines,
holes, ring orientation. Needs oracle coverage on every polygon layer.

### 3. Edge/ring arena engine (codex #2)

The deepest rewrite on the table. Represent per-zoom geometry as a flat
edge/ring arena with row buckets instead of owned `Shape`/`Shapes`
handoffs between every stage. Row traversal produces tile work items
directly (full-tile spans + boundary windows); boundary clipping
becomes a grid/window operation over the arena, not "make a new owned
polygon, then clip it again." Removes the `cut_row_bands` owned-Vec
churn (53.8 s) and makes the full-tile fast path natural rather than a
rectangle detector. Subsumes items 5 and 6.

Hard parts: hole attachment and self-touching normalized output. A real
engine rewrite; do it only after items 1-2 prove the direction.

### 4. Cross-zoom LOD cascade or tree (Fable R4 / codex #3)

The per-zoom loop re-derives everything from the z14 base 15 times.
Derive zoom z from z+1's already-simplified/normalized result (halving
coordinates), or build an adaptive LOD tree with cached per-zoom cover
state. Shrinks input to rescale/DP/normalize/tiling geometrically.
Legal under the contract (verify + oracle + counts-within-noise, not
geometry parity). But cascaded DP compounds error and can collapse thin
coastal features differently enough to move low-zoom feature counts,
and DP+rescale are only ~10s today. Only worth it once per-zoom tiling
is linear-cost (i.e. after item 1/2); re-profile first, then do it as a
full switch, not a knob.

Tile-ownership report, concrete form (its high-conviction rewrite #1):
replace `emit_shape_for_zoom` with `emit_shape_zoom_range`, building a
per-shape zoom pyramid in ONE call - base integer contours, per-zoom
rounded contours, DP survival state, normalized shapes, bbox/tile
ranges, boundary rows, and full-tile runs - so the planning and
simplification cascade are shared while direct output still uses exact
per-zoom integer coordinates. Removes repeated O(vertices) and repeated
i_overlay setup across up to 15 zooms; for ocean it attacks the
dominant multiplier directly (that report notes ocean parallelizes
(piece, zoom) rather than (piece), which worsens the rebuild). Risks:
exact shift-round semantics, pinned vertex survival, min-area drops,
and normalization changes can alter topology - full intrusive branch,
benchmark, keep-or-revert. (Overlaps item 10's descent, which achieves
the same cross-zoom sharing recursively; this framing keeps the
existing per-zoom structure but hoists the shared planning.)

### 5. Fully-flat Shape/Shapes representation (Fable R2 remainder)

Landing 2 adopted only the overlay-boundary reuse (`IntOverlayScratch`,
`overlay_into` flat buffers). Go the rest of the way: make
`FlatContoursBuffer` (contiguous points + ranges) THE engine type
through `cut_row_bands` levels, `rescale_shape`, `simplify_shape_dp`,
`encode_tile_shape` - each currently allocates a fresh `Vec<IntPoint>`
per ring per level per zoom. Removes the per-ring allocation storm and
is a strictly better layout for DP and area scans. Deferred with item 3
(the arena engine is the natural home for this).

Per-zoom emission report, same item: `Shape = Vec<Vec<IntPoint>>` means
per-ring heap allocations cloned at every rescale, every zoom; the
encoder already uses the right shape (`tile_points` + `tile_ranges`).
If the item-10 descent rewrite happens, build it on flat buffers
(points + ring ranges + shape ranges) from day one instead of porting
the nested Vecs. Two specific per-zoom scratch-churn sites fall out for
free with flat buffers: `flags_from_pin_set` rebuilds `Vec<Vec<bool>>`
per zoom per piece, and `rescale_shape` allocates fresh rings per zoom.

### 6. Per-row bitset for boundary tiles (Fable, remainder)

Landing 1 swapped `boundary_tiles: HashSet<u64>` to FxHashSet. The
stronger form: a per-row bitset over `tx_min..=tx_max` - the consumer
immediately re-buckets into sorted per-row Vecs anyway
(int_ocean.rs, the ~819-830 region). Goes with item 2/3. Tile-ownership
report, same site (int_ocean.rs:1077): replace the FxHashSet boundary-
tile collection with row-sorted intervals or a reusable sorted vector;
worth doing after the bigger shape of the emitter is settled.

### 7. build_graph_view multi-rule extraction (codex, remainder)

i_overlay's `build_graph_view` helps only when extracting multiple
rules from ONE graph. Our clip rectangle changes per tile, so it does
nothing today. Only pays off if a future design extracts multiple
overlay rules per tile (e.g. Intersect + Difference for land/water
complements). Parked until such a design exists.

### 8. read_ring_points per-ring buffer reuse (Landing 3 review)

`read_ring_points` (src/ocean.rs) allocates a fresh
`RING_READ_BUFFER_BYTES` (64 KB) `Vec<u8>` per ring in the hot parallel
parse path. Threading a reused per-record (or per-thread) buffer
through the parallel parse cuts parse-path allocation - **the concrete
named lever for the residual ocean_ms gap to sub-10s** and helps RSS
too. Real refactor: the buffer lifetime must survive the rayon closure
boundary. Deferred from Landing 3 as out-of-scope (surgical, no
gold-plating).

Also observed: the `read_exact_at` calls in `read_ring_points` and its
caller are unconditional, but the trait that provides them
(`std::os::unix::fs::FileExt`, ocean.rs:18) is `#[cfg(unix)]`-gated - a
non-unix target fails to compile. Acceptable for this Linux-only
project; a proper fix needs an mmap fallback and is out of scope.

Pipeline perf report, same site, sharper fix: the shapefile is already
mmapped (the mmap is currently only used for headers), so
`read_ring_points` can read rings straight from the mmap and drop the
per-ring buffer entirely rather than threading a reused buffer through
the closure. "Small but free."

### 9. Tier-2 unification (Landing 4 decision point, deferred)

Route `emit_normalized_per_tile` (pipeline/emit.rs:376) through the
same boundary-rasterize + interior-fast-path + bisect machinery as
`emit_shape_for_zoom`, instead of per-tile booleans over the whole
bbox (an 8x8-tile lake is 64 full booleans of the whole ring today).
Nearly free once item 1 or 2 lands (same primitives); a direct cut into
the OSM polygon cost. The 64-tile tier threshold then stops mattering
and the tier seam can be deleted - one path for all multi-tile
polygons. Tile-ownership report, same lever
(`OSM_TIER2_MAX_TILES = 64`, emit.rs:418): that tier clips every bbox
tile directly; lowering or removing it to route more OSM multi-tile
polygons through the row-band/gap-run engine is a focused improvement
even standalone.

### 10. Recursive tile-pyramid descent (per-zoom emission report)

Presented by that report as its high-conviction big rewrite, done as a
full coherent rewrite of the emission engine, not an incremental patch.
Replace prologue split + per-zoom row bands + gap runs + per-tile
boolean clips + OSM tier2/tier3 with a single recursive descent over
the tile pyramid in base (z14-grid) coordinates, per piece:

- At cell (z, tx, ty) hold the piece's fragment clipped to the cell's
  buffered rect, in base coords.
- Emit the tile for zoom z from the fragment: shift-round rescale
  (exact, as today), dedup, pin-aware DP, normalize (the fragment is
  tiny, so i_overlay here is cheap), encode.
- Recurse: cut the base-coord fragment into 4 children (two axis
  bisections), reusing the ancestor-containment identity already proven
  and tested for `cut_row_bands`
  ((shape INTERSECT parent) INTERSECT child == shape INTERSECT child;
  child buffered rects are contained in the parent's because the child
  buffer is exactly half the parent's in base units).
- Uniform-subtree shortcut: when a fragment equals the full buffered
  rect, the entire subtree is full-tile fills - emit canonical
  full-tile records for the whole run with zero further geometry work.
  Replaces the rasterize + gap-run machinery with a trivially correct
  check and extends it across zooms, not just across one row.

Every boolean now operates on geometry that shrinks geometrically with
depth: the whole piece is noded once at the top instead of
~15 x log(rows) times, and z14 boundary tiles clip tile-local fragments
instead of row-global ones. Deletes outright: `rasterize_shape_edges`
and its three dilated-crossing helpers (f64 DDA + FxHashSet per shape
per zoom), `cut_row_bands`, `emit_boundary_and_gap_tiles`,
`split_piece`, and the `emit_normalized_per_tile` tier - one code path
used by ocean AND OSM polygons (subsumes item 9).

The one real design problem is seams. Today's simplify-then-cut
guarantees adjacent tiles clip from the same globally simplified shape;
cut-then-simplify breaks that - neighbors simplify their fragments
independently and the coastline's tile-edge crossing can drift by up to
`dp_tol`. Fix with existing machinery: pin fragment vertices lying
within a strip around the fragment's clip border (the pin-aware DP was
built for exactly this). Cut-introduced vertices lie exactly on shared
cut lines; both neighbors hold the identical unsimplified chain in the
shared buffer zone; pinning it makes the edge-crossing geometry
verbatim-identical on both sides. Interior simplification stays free.
Cost: unsimplified vertices in a ~buffer-wide strip per tile at low
zoom - measurable via the existing per-zoom tile-size stats; earcut
oracle + `verify --geometry-stats` + `compare-tiles` gate correctness.

Payoff (per that report): attacks the top three hotpath entries
(159 + 89 + 54 thread-s) simultaneously - all the same redundancy.
Parallel grain improves - today's (piece x zoom) fan-out has a fat tail
(P99 67ms, the biggest z14 piece defines the critical path); the
descent parallelizes within a piece via rayon join on subtrees, so work
granularity tracks geometry, killing the tail. Should also cut the
2.8 GB ocean-phase RSS (fragments are transient and per-worker instead
of a global pre-split Vec<Shape>). Expected: Denmark ocean 12.4s to
~3-4s on structure alone, plus a slice of the pbf phase since OSM
tier2/tier3 polygons ride the same engine. Prize is planet scale, where
coastline pieces are far larger and the per-zoom re-noding redundancy
grows super-linearly with piece size.

Risks: seam drift if the pinned-strip argument has a hole (gated by
oracle, verify, visual QA on coastline tiles); output not byte-identical
(DP anchor points change - accept, gate on the oracle not on diffing);
low-zoom tile-size inflation from pinned strips (measure; if bad, fall
back to a tiny strip tolerance rather than hard pins). Do it as a full
rewrite behind nothing (no env vars, no routing switch), benchmark,
keep or revert. Ocean and OSM tier3 first, fold tier2 in immediately
after. Relates to items 2/3/4/5/9, which this single design subsumes.

### 11. Memoize the canonical full-tile record (per-zoom emission report)

`emit_full_tile` re-encodes constant bytes on every call, and the sink
re-appends an identical payload per tile. For ocean it is the single
most common record; encode once, reuse the slice. (Independently raised
by the tile-ownership report: precompute canonical full-tile ocean
feature or layer bytes - easy, but not its first-effort spend.)

### 12. ring_is_simple_complete is O(n^2) in the tier1 path

Per-single-tile-polygon per-zoom O(n^2) in the tier1 path. Fine for
buildings, bad for medium land polygons. In the item-10 descent world
it disappears - just normalize the (small) fragment instead and delete
the function. Source: per-zoom emission report.

### 13. Hoist zoom-independent OSM attr encoding (per-zoom emission report)

`encode_attrs_bytes` runs per feature per zoom even when the attr set
is zoom-independent. Ocean already precomputes it once (Landing 2); OSM
emitters could hoist the common case the same way.

### 14. Partitioned sort + fully-parallel assemble (pipeline perf report A1)

That report's #1 pick for the planet target. The
planet-critical bottleneck: assemble is a three-stage pipeline
(reader -> rayon encode -> writer) but the reader is ONE thread doing,
per record, a `BinaryHeap` pop/push + a fresh `Box<[u8]>` in
`ChunkReader::read_record` (sort.rs:502). Denmark pushes 15M records
through this; NA pushes ~512M. At even 200 ns/record of serial work
that is ~100s - matching the observed 155-164s NA assemble that no
amount of rayon-encode parallelism dents (the writer thread is serial
too but it is cheap sequential I/O; the merge reader is the wall). Root
cause: a classic external merge sort - producers push
individually-boxed records into one `SortWriter`, chunks are globally
unordered, so global tile order is recovered only by a single k-way
merge that touches every record on one thread.

Redesign: partition by tile-id range at WRITE time instead of merging
at read time. The sort key's high bits are the Hilbert tile id
(spatially contiguous ranges). Give the pipeline P partitions (e.g. 256
tile-id ranges calibrated from z6/z7 boundaries); every producer (way,
ocean, relation workers - they already write their own chunk files)
routes each record into a per-partition buffer and flushes per-partition
chunk files. Assemble becomes: for each partition in order, load-or-merge
it (small enough to sort in RAM as one arena: keys sorted as
(u64 key, offset, len) over a payload blob - the `PayloadRecord`
machinery already exists), encode its tiles with rayon, hand ordered
output to the writer. Partitions prefetch/sort/encode ahead while
earlier ones are written, so the pipeline is parallel across AND within
partitions, and the `BinaryHeap` + Box-per-record disappear. PMTiles
needs Hilbert order; partition order gives it for free.

Payoff: removes the only structurally serial O(total-records) loop in
the pipeline (~1/3 of NA wall; at planet scale, per the sort.rs header
comment ~100+ GB of sort records, the difference between assemble
scaling with cores or not) and deletes ~512M small allocations on one
thread. Risks: skew - one dense partition (Tokyo, NYC) becomes the
straggler; mitigate with more/smaller partitions or recursive splitting
of hot ones. `--skip-to` checkpoint semantics change (rewrite the chunk
naming/layout, do not preserve). Peak RSS needs a per-partition budget;
fall back to a per-partition k-way merge when a partition exceeds budget
(still parallel across partitions). Classification: full rewrite of
sort.rs + the assemble reader, touching all producers' flush paths;
that report would do it FIRST for the planet target (but sequences it
after A3 so producers already write arenas, then re-baseline NA - both
NA numbers are pre-rewrite and stale).

### 15. Shared-node prepass restructure (pipeline perf report A2)

`prepass_shared_nodes` (phase12.rs:797) does a complete SECOND scan of
the PBF's way blobs before the main read, building two
`FxHashSet<i64>` (seen, shared). Hotpath shows `pbfhogg::run_pipeline`
called twice: ~6.5s prepass + ~13.9s main read - the 6.5s is pure
serial latency (30% of Denmark phase12). At planet scale `seen` holds
~2B unique node ids: an `FxHashSet<i64>` that size is 30-60 GB, which
alone breaks the 64 GB box. This path has never been benchmarked at NA
scale (both NA baselines predate it). Cause: junction pinning needs
"which nodes appear in >=2 ways" before the first way is simplified,
and ways come after nodes in the file, so it brute-forces a serial
pre-scan with the most memory-hungry structure available.

Two independent halves:
1. Overlap, do not serialize. Spawn the prepass on its own thread at
   phase12 start, join it right before the first way block dispatches.
   The main read spends its opening seconds on node blocks (52M for
   Denmark) while the prepass reads a different file section
   (`BlobFilter::only_ways`). Hides essentially all 6.5s on Denmark.
   Small, safe. (That report sequences this FIRST overall: smallest
   change, -6s Denmark, de-risks nothing else.)
2. Replace the hash sets with rank-indexed 2-bit saturating counters.
   The `SortedNodeStore` gives every node a dense rank; a 2-bit counter
   array by rank is ~600 MB for planet vs tens of GB. Requires the node
   store to exist before counting; alternatively keep the prepass
   concurrent with the node phase but emit raw ref streams into
   fixed-size sorted runs and count via merge (no giant set). For
   locations-on-ways (the production path - no node store), substitute a
   sort-based count or a Bloom-pair (first-seen filter + shared filter,
   accepting tiny over-pinning - over-pinning is SAFE, it only preserves
   extra vertices).

Bonus from the same scan: scan relation blobs too (a tiny fraction of
the file) and record which way ids are relation members, so
`way_index.put()` only stores member ways. Today the way index stores
geometry for ALL ways (210M for NA) when only the ~5% referenced by
relations are read back - a 10-20x cut in way-index write volume,
encode CPU, and disk. The relation-member prescan must be conservative
about nested relations (resolve one level, over-approximate).

Half 2 (compact counters + way-index filtering) is a correctness-of-
scale REQUIREMENT before any planet attempt, not just perf.
Classification: the overlap is local; the counter/scan + way-index
filtering is one intrusive rewrite of the phase12 pre-analysis.

### 16. Way-phase ownership rewrite (pipeline perf report A3)

The largest phase12 lever; establishes the arena idiom the other
rewrites converge on. The way phase has a serialized ownership chain:
(1) a single worker-dispatch thread parses every way out of every block
and copies all tags to `(String, String)` plus collects
node_refs/coords into fresh Vecs (phase12.rs:335-379) BEFORE rayon
starts; (2) `process_raw_way` immediately re-borrows those Strings as
`&str` and, in locations-on-ways mode, clones `coords_e7` again
(phase12.rs:962-964); (3) every emitted feature is a separate
`Box<[u8]>` funneled through a channel to a drain thread that pushes
them one by one into `SortWriter`, which later re-serializes them into a
contiguous buffer anyway. Old alloc profiles put ~18 GB of Denmark
churn on `write_sorted_chunk` + `process_raw_way`; `process_raw_way` is
67 cross-thread seconds today. `RawWay` exists because PBF element
borrows do not outlive the callback - but the block DOES outlive it (it
is already sent by value to the worker); the materialization is an
artifact of where extraction happens, not a real lifetime constraint.

Redesign: send the whole `PrimitiveBlock` into the rayon task. Inside:
parse elements, run the block-local shared-node annotation, process
ways with borrowed `&str` tags directly (zero tag allocation,
extraction now parallel across blocks instead of serial on the
dispatcher). Emit records into a per-worker (key, offset, len) + payload
arena - the `OceanAcc`/`RelAcc` pattern ocean and relations already use
- and flush per-worker chunk files directly (or per-partition buffers
if item 14 lands first). The drain thread shrinks to way-index puts
only; with item 15's member filtering that too becomes small.
`SortRecord { Box<[u8]> }` stops existing on the hot path. Payoff:
removes the serial extraction ceiling on way throughput, eliminates the
two largest phase12 alloc-churn sources, and deletes a thread handoff
(results channel + drain) whose condvar/byte-budget backpressure exists
mostly to manage the materialization this removes; phase12 is the
largest phase at every scale. Risks: block-held-alive memory (a block
pinned by a rayon task holds its decoded buffer; the existing in-flight
byte budget covers it if it counts block bytes); the block-local +
global shared-node annotation moves inside the task; way ordering into
the way index no longer matters (`WayIndex` external-sorts offsets
anyway). Classification: full rewrite of the way phase's data flow;
intentionally converges way/ocean/relation producers on one
arena-and-flush idiom (the honest cleanup afterwards, not before).

### 17. Take chunk sorting/writing off the drain thread (pipeline perf report)

`write_sorted_chunk` sorts and writes ~1 GB inline on the drain thread
(0.6-1.2s stalls on Denmark, proportionally more at scale), stalling the
results channel (capacity 8). Hand full buffers to a background writer.
Obsolete if items 14/16 land. (Related medium note from the same report:
unify all producers on `PayloadRecord` arena records and delete
`SortRecord { Box<[u8]> }` - subsumed by 14+16 if they land; standalone
still worth a few percent of phase12 and simplifies sort.rs. The
tile-ownership report notes the same: `write_sorted_payload_chunk`
already exists (sort.rs:421) but ways and relations still allocate
boxed payloads - extending arena-backed payloads beyond ocean helps
memory traffic, not the core geometry cost. The relation-aware rewrite
report independently lists this as its first medium change: apply
ocean-style arena payloads to OSM way and relation output, keeping the
old sort boundary but removing many `Box<[u8]>` allocations.)

### 18. Parallelize relation prepare (pipeline perf report)

`prepare_relation` (way-index lookups + projection of every member way)
runs serially on the main thread. Denmark: negligible. NA: 2.3M
relations - batch the lookups into rayon like everything else. Do it
when NA gets re-benchmarked post-rewrite. Relation-aware rewrite report,
same lever (as its fallback if the full item-22 relation rewrite is
deferred): batch relation way lookups instead of per-member binary
search + decode through `WayIndex`.

### 19. Ocean as a tile compositor, not per-piece features (tile-ownership report)

That report's recommended FIRST rewrite. Ocean has one layer, no
attributes, and visually wants tile coverage, yet it is emitted as
millions of piece-derived feature records (ocean.rs:629): each piece is
clipped, written as sort payloads, sorted, then read back in assemble
(which even skips normal same-attribute geometry merging for Ocean).
Redesign: build an `OceanTileLayerStore` - for each zoom collect
coverage per tile, union or concatenate compatible clipped rings per
tile, emit one canonical ocean layer payload per tile. Full interior
tiles become a shared canonical geometry; boundary tiles are resolved
once per tile, not once per piece touching the tile. Payoff: ocean
dominates the workload and has the least semantic baggage (no
per-feature attrs to preserve), so this cuts clipping work, sort record
volume, payload serialization, assembly decode, and tile size. Risks:
adjacent shapefile pieces, holes and islands, winding, and canonical
output need careful validation; ocean feature IDs likely disappear
(acceptable internally, tests may need updating). Relates to the old
per-tile ocean union idea (S04 in the old rendering-saga ledger, now
in git history) and to item 2's coverage-span model, but framed as an
ocean-specific tile store.

### 20. Tile-owned polygon output (tile-ownership report)

After clipping, the pipeline serializes feature payloads, writes sorted
chunks, reads them back, decodes them (wire_format.rs:348), then builds
layers (assemble.rs:37) - a hard feature-owned handoff exactly after
the expensive per-tile geometry is already produced. Redesign: make
tile or Hilbert-range shards the ownership boundary. Polygon emitters
send geometry into tile accumulators or per-layer tile records; shards
are finalized in PMTiles order so `pmtiles_writer` (pmtiles_writer.rs:
335) still sees ordered tiles. Staged: start with layer-level records
for ocean (item 19), then expand to polygon layers. Payoff: removes
serialization, sort payload copies, record heap allocation, k-way merge
reads, and feature rehydration - and enables per-tile polygon
composition. Risks: memory control and backpressure become harder;
relations and ways arrive in PBF order, so the shard design must be
explicit and bounded. This is a more radical sibling of item 14
(partitioned sort): 14 keeps the sort and partitions it; 20 deletes the
feature-owned sort/rehydration seam entirely.

Relation-aware rewrite report, same rewrite (its #2, high conviction):
replace `SortWriter` with a tile-range partitioner - emitters send
features to Hilbert-range shards; each shard owns arenas, groups by
tile, finalizes tiles, and drains shards in PMTiles order. Spill files
are fine at scale but should be tile buckets, not one global feature
stream. Surfaces named: emit.rs:242, sort.rs:112, sort.rs:502,
assemble.rs:22; ocean already proves arena-backed chunk writing via
`PayloadRecord` (sort.rs:419, ocean.rs:227). That report stresses the
seam is more expensive than the visible sort phase suggests because the
allocation / serialization / deserialization / regrouping cost is
charged to adjacent phases (emit, assemble), and that removing it
unlocks writer parallelism and low-zoom aggregation. Risks: hard
ordering bugs, bounded memory for dense tiles, shard skew, PMTiles
dedup offset handling. Keep-or-revert branch, not a knob.

### 21. OSM polygon feature planner (tile-ownership report)

Each polygon `LayerMatch` quantizes and emits geometry separately, so a
closed way matching multiple polygon layers repeats base quantization
and often repeats per-zoom clipping (`process_raw_way` loops matches and
calls `emit_polygon_feature` per match, which owns quantization, pins,
simplification, caps, and output). Redesign: split matching from
geometry. Build one `PolygonFeaturePlan` per shifted geometry (quantized
base shape, pin flags, zoom plans, fanout decisions, clipped tile
geometries), then attach multiple layer outputs with their attrs and
zoom ranges. Payoff lower than ocean but attacks millions of OSM
features and removes the tier1/tier2/large-polygon split (relates to
item 9). Risks: layer-specific min zooms, attr zoom filtering, seam
deferral, fanout caps, and centroid outputs must stay correct.
Recommended order in that report: item 19 ocean compositor -> fold into
the item-4 multi-zoom planner -> move OSM polygons (this item) onto the
same planner and delete the small/large emitter split -> only afterward
clean up sort payloads and writer APIs if the tile-owned path proves
out.

### 22. Selective way resolution + relation planning (relation-aware rewrite report)

That report's top pick (full coherent rewrite, high conviction), and
the strongest form of items 15-bonus/16. Today the way phase resolves
coordinates and writes `WayIndex` geometry for broadly EVERY way
because relations may need untagged member ways later:
`process_raw_way` resolves coords before tag matching
(phase12.rs:944), and `WayIndex` writes/sorts all retained way geometry
(way_index.rs:426, way_index.rs:452). Cause: relation blocks are
buffered (phase12.rs:193) and processed later (phase12.rs:490), so the
way phase cannot cheaply know which untagged ways matter, making the
safe default "resolve and index everything."

Redesign: an upfront relation planning pass storing compact relation
skeletons + the set of member way IDs actually needed. In the main way
pass, classify tags while PBF borrows are alive, and only collect node
refs / resolve coordinates for ways that either match Shortbread
directly OR are in the relation-member set; store only relation-member
geometries for later assembly. Attacks PBF-phase work BEFORE geometry
starts: node-store lookups, tag cloning, way-index writes, offset
sorting, relation binary searches, decode allocations - and makes
LocationsOnWays much stronger (resolved coordinates stop being
re-indexed for unrelated ways). Risks: relation-prepass memory for
planet-scale member sets, exact missing-ref reporting, unusual PBF
ordering, and topology pinning (which that report would solve as PART
of this redesign rather than keeping the current all-ways shared-node
prepass sacred - i.e. it explicitly subsumes item 15's overlap/counter
work and replaces the "resolve everything" default, going beyond item
15's bonus which only filtered the way-index write, and beyond item 16
which kept resolving every way).

### 23. Durable/cached ocean tile source (relation-aware rewrite report)

Its rewrite #3 (medium-high conviction), distinct from item 19's
per-run compositor. Ocean is static but regenerated each run: parse,
clip, split, zoom fanout, chunk writing, then sort + assembly (parsed
pieces are materialized before processing at ocean.rs:183, ocean.rs:
210). Redesign: precompute a DURABLE ocean tile stream keyed by
shapefile identity, zoom range, and simplification policy; at
generation time merge that tile source with OSM tile partitions (in the
item-14/20 world, ocean becomes just another ordered tile-layer input).
For repeated runs this ERASES the ocean phase rather than tuning it -
ocean is independent of PBF content except bounds and zoom selection.
Risks: cache invalidation, exact geometry parity, storage, and handling
simplified vs full-resolution shapefile sources. Composes with item 19
(compositor produces it; this caches it).

### 24. Delay multi-zoom fanout to post-partition (relation-aware rewrite report)

Its rewrite #4 (medium conviction, potentially large). Feature emitters
fan out early across zooms and tiles - lines loop zooms (emit.rs:550),
polygons (emit.rs:677), multipolygons (emit.rs:836) - multiplying
records before features meet their tile context. Redesign: after tile
partitioning (items 14/20), build zoom bands or a tile pyramid where
lower-zoom finalization happens per tile/layer, not per source feature.
Distinct payoff angle beyond items 4/10: for streets and boundaries,
topology-sensitive simplification can then happen when neighboring
features for a tile are visible together (line-merge/simplify with tile
context, not per isolated feature). Also opens a path to delete the
global shared-node prepass (phase12.rs:206). Risks: deriving parent
tiles from clipped children can be wrong - the correct version needs
buffered parent geometry or retained source geometry per partition.

### 25. Pinned line DP cascade (relation-aware rewrite report)

For pinned line simplification, replace per-zoom DP from original
geometry with a pinned cascade that carries pin flags forward. The
line-layer analogue of item 4's polygon cross-zoom cascade (streets /
boundaries rather than area layers); listed as a medium local change.

### 26. PMTiles writer sharding + faster dedup fingerprint (relation-aware rewrite report)

If the PMTiles writer is still single-threaded after partitioning
(items 14/20), shard tile-blob writing and use faster non-cryptographic
128-bit content fingerprints for dedup. Medium local change, gated on
the partitioning rewrites landing first.

---

## Non-targets (explicit, per report noted)

Per-zoom emission report:
- Sort + assemble (0.6s + 4.1s Denmark) - not the bottleneck for
  per-zoom emission. (The pipeline perf report DISAGREES at scale:
  assemble is its #1 planet-critical item, 155-164s on NA, item 14 -
  the two reports are consistent, differing only on Denmark vs planet
  framing.)
- Micro-optimizing `simplify_shape_dp` / `rescale_shape` internals -
  11s and ~1s of thread-time across everything; the win is calling them
  on fragments, not making them faster.
- Tuning `SPLIT_Z` / `SPLIT_MIN_VERTICES` / chunk sizes - knob-turning
  on a structure the item-10 rewrite deletes.

Pipeline perf report:
- `decompress_chunk` (12.3 cross-thread s) is the standard-path node
  store cost; do NOT invest - the production pipeline is
  locations-on-ways, which deletes the node store entirely. Treat the
  standard path as legacy-adequate.
- Do not micro-tune `match_element`, `find_chunk_in_blob`, bitpacking,
  or the DP inner loops - the file's own comments document prior
  attempts that died on DRAM latency; none are structural.
- Do not touch seam-reconciliation or dedup machinery for perf; both
  are cheap in the profile.
- RSS 2.8 GB on Denmark is 24 `OceanAcc`/worker arenas under mimalloc's
  non-purging arenas. If planet RSS budgeting gets tight, enable
  mimalloc periodic purge or shrink `OCEAN_CHUNK_SIZE_LIMIT` - a memory
  knob, not a throughput item.

Tile-ownership report:
- Do not start with micro-optimizing DP, protobuf encoding, or generic
  writer abstractions - the code is already past that point. The
  remaining throughput is in deleting repeated zoom work and deleting
  feature-owned handoffs.

Relation-aware rewrite report:
- Do not center generic writer API cleanup, tag-lookup micro-tuning, or
  env-var experiment scaffolding. The right bet is an intrusive branch
  that rewires ownership: plan relations first, resolve only needed
  ways, emit into tile partitions, then benchmark and keep or revert.
