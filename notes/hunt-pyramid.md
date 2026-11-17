**I would rewrite the polygon descent around shared boundaries and symbolic coverage.** The current unit of work is an owned, fully materialized polygon fragment. That choice repeatedly pays for geometry that has already been classified, clipped, or normalized.

The two strongest opportunities are:

1. Replace contour-copying descent with a boundary representation that shares unchanged chains and batches clipping work.
2. Keep full-fill subtrees symbolic through sorting and assembly, expanding them only where tile contents require it.

These are coherent architectural changes. More aggressive simplification before clipping is a less convincing proposal under your geometry contract: the current rounding, pinning, anchors, and area filtering make those operations order dependent.

This ranking comes from source inspection, without builds, tests, benchmarks, markdown, or history. I followed the callers, topology adapter and port, simplifiers, emission, sort ordering, assembly, archive writer, and relevant pbfhogg APIs. `research/iOverlay/` is absent in this checkout; the topology review used the in-tree port.

**What the code already avoids**

This is not a naive whole-polygon-per-tile implementation. Root selection already bisects ranges, descent has rejection and containment paths, rectangle clipping has a specialized implementation, full cells bypass further geometry work, and scratch buffers are pooled.

Initial float-to-base quantization also happens once per polygon emission invocation, rather than once per cell. The repeated cell work is rescaling that base geometry, deduplicating collapsed vertices, classifying pins, simplifying, repairing topology, and encoding it.

The central remaining cost is visible in [descend](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:391), [emit_cell](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:434), and [intersect_shapes_with_rect](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:754):

- Every child independently scans the parent's shapes and computes their bounding boxes.
- Containment still copies every coordinate into another owned shape.
- Crossing shapes enter a clipper that clones rings and materializes intermediate contours through four half-plane passes.
- Emission copies the fragment into zoom coordinates, simplifies it, and generally normalizes it again.
- That simplified result is discarded after encoding. Descendants continue from the unsimplified base-grid fragment.

Pooling reduces allocation overhead. It does not remove these scans, copies, intermediate polygons, or repeated topology construction.

**1. Replace owned-fragment descent with a boundary compiler. Highest-conviction general rewrite.**

The current representation makes every spatial cut produce another `Vec<Vec<Vec<IntPoint>>>`. Geometry ownership follows the traversal tree even when almost all coordinates remain unchanged.

The strongest replacement is an immutable, normalized source boundary representation with:

- Shared coordinate chains and explicit generated intersection vertices.
- Ring and component identity, source junction information, and cached spatial bounds.
- Cell fragments represented by chain references, cut endpoints, and clip-boundary connections.
- Explicit empty, full, and boundary-bearing coverage states.

The spatial hierarchy should distribute references to relevant boundaries. It should not repeatedly manufacture complete polygon trees merely to pass them to the next level.

For a contained child, the result becomes another reference to the same geometry. For a crossing child, only the affected chains and boundary connections need new storage. Hole ownership can follow the representation rather than being rediscovered by point-in-contour searches whenever the outer splits.

The existing fast clipper demonstrates why this matters. It clones the outer, repeatedly replaces contour collections, repeats the process for each hole, and can then abandon that work for the boolean fallback. See [clip_shape_rect_fast](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:809). This is a representation problem beyond the arithmetic of computing intersections.

The clipping interface should also change. I would ask the topology sibling for an operation conceptually like:

> Partition this prepared subject against these labeled rectangles, sharing subject preparation and intersection work, and return component views for each label.

That must retain separate outputs for overlapping buffered cells. Adding four rectangles as one boolean clip would union them and lose the required ownership information.

Batching the four children also needs to respect their actual buffered rectangles. They overlap, and their outside boundaries shrink relative to the parent's buffer. This is not simply a split along the parent's horizontal and vertical midpoint.

The present topology interface clears and uploads the subject for each operation, then splits segments, constructs the graph, extracts contours, and binds holes. Its retained buffers reuse storage, rather than providing a reusable prepared subject. See [intersect_rect_into](/home/folk/Programs/elivagar/crates/lib/src/geometry/int_ocean.rs:849) and [overlay_into_nested](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/mod.rs:212).

The payoff is broad:

- Fewer complete coordinate copies at every depth.
- Shared preparation across sibling cuts.
- Less reconstruction of unaffected holes and components.
- Earlier full/empty classification from boundary incidence and fill state.
- Lower live memory because ancestors and children share immutable geometry.

A cell without local boundary segments still needs a reliable fill classification. Corner tests alone are insufficient: a hole can lie entirely inside the buffered cell.

There are two substantial correctness risks.

First, current clipping is path dependent. A generated integer intersection can participate in a later cut, so changing the cut ancestry can change subsequent rounded coordinates. The rewrite should preserve that ancestry as a compact cut recipe wherever it affects output. Indexing and scheduling can change without silently changing the geometric construction.

Second, normalization cannot simply be removed because the source was normalized once. Rescaling, clipping, DP, and VW can create degeneracies or intersections. The new representation should carry evidence about which transformations changed geometry, then validate or repair affected components. It must retain a complete repair path for cases where local reasoning is insufficient.

I have high conviction in eliminating the owned contour trees and duplicated sibling preparation. The precise reduction in topology calls depends on how much validity can be preserved or established cheaply.

**2. Preserve full-fill subtrees as coverage ranges. Highest-conviction end-to-end materialization removal.**

The full-cell shortcut saves geometry processing, but still expands the entire subtree into individual tile records.

[emit_full_subtree](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:690) recursively visits every descendant. Each visit calls [emit_full_tile](/home/folk/Programs/elivagar/crates/lib/src/geometry/int_ocean.rs:1387), which reconstructs and encodes the same tile-local rectangle. The sinks then copy geometry and attributes into ordinary sort records.

For a full cell with `d` remaining levels, this produces:

`1 + 4 + ... + 4^d`

records, although its coverage can be described by one subtree and one payload context.

I would replace that expansion with a descriptor containing the subtree, zoom span, feature identity, layer, paint rank, attributes, and multiplicity. Convert its coverage to Hilbert intervals at each emitted zoom and merge those intervals with ordinary boundary-fragment events.

Assembly then has two cases:

- Where other tile content exists, inject the fill feature into the tile's deterministic feature stream.
- Where the complete tile contents remain identical across an interval, encode and compress once and write a PMTiles run.

The repository already contains useful machinery:

- [The archive writer's `add_run`](/home/folk/Programs/elivagar/crates/lib/src/archive/writer.rs:916) accepts one compressed payload covering consecutive tile IDs.
- [The artifact consumer](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/artifact.rs:66) handles interval-based run copies.
- [Its Hilbert range traversal](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/artifact.rs:185) already recognizes completely covered quadtree blocks.

This removes work across the entire pipeline: tile enumeration, repeated rectangle encoding, payload copying, sort entries, spill bytes, merge comparisons, and repeated encoding and compression of identical completed tiles.

Caching the rectangle command buffer alone captures very little of that opportunity.

The main risks are feature semantics and ordering. A full fill must retain precisely the feature identity and multiplicity that today's records would produce. Overlapping fills cannot automatically be collapsed. Attributes and compression policy can vary by zoom. Mixed-content tiles still require normal assembly.

This is particularly attractive for large filled OSM areas and computed ocean. Existing artifact run copies have already removed much of this work for artifact-served ocean interiors, so their savings should not be counted again.

This rewrite can preserve decoded tile contents exactly.

**3. Make large-polygon work available incrementally. Architectural scheduling change, with payoff concentrated in long tails.**

The code has parallel ocean work, but substantial preparation precedes that parallelism:

- `root_fragments` materializes all roots before descent consumes them.
- `split_for_parallel` expands selected fragments and emits expanded cells synchronously.
- Ocean preparation collects the prepared items before starting the subsequent processing pass.
- OSM polygon descent stays inside the worker processing that way or prepared relation.

See [root_fragments](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:274), [split_for_parallel](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:203), and [the ocean preparation and processing passes](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:515).

A large relation or coastal component can therefore remain a serial ownership unit long after other workers exhaust their work.

I would use a bounded queue of polygon-region jobs referencing the shared geometry from opportunity 1. Root preparation should yield jobs incrementally. Workers should expose expensive descendants as independent jobs while retaining small descendants locally. Full coverage should yield the descriptors from opportunity 2, rather than being enumerated during preparation.

This is more than replacing recursion with Rayon calls. The source geometry must be shared, task-local output must avoid a shared mutable sink, and queued jobs must be bounded by retained geometry and estimated work. Otherwise parallelism merely multiplies contour storage and scratch peaks.

Determinism does not require serial traversal. Sort already defines a scheduling-independent total order using key and payload bytes in [record_cmp](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:151). Identical fragment payloads can arrive from independently scheduled jobs.

The likely benefit is lower tail latency and lower frontier memory. It will not help much when ordinary polygons already saturate all cores, and it does not by itself parallelize one enormous normalization operation. I would treat it as part of the boundary rewrite, not count its expected speedup independently.

**Simplification reuse is useful, but a global simplified polygon per zoom is not an equivalent replacement.**

There is considerable repeated work here:

- [Rescaling](/home/folk/Programs/elivagar/crates/lib/src/geometry/int_ocean.rs:534) constructs another shape and carries pin flags through collapsed vertices.
- [Integer DP](/home/folk/Programs/elivagar/crates/lib/src/geometry/int_ocean.rs:987) copies the contour, constructs two chains and their masks, computes keep sets, constructs output, and copies it back.
- [VW](/home/folk/Programs/elivagar/crates/lib/src/geometry/int_ocean.rs:677) rebuilds its linked-neighbor arrays and heap.
- A second whole-shape DP pass may follow.

Ocean currently selects VW, while OSM selects DP. Both can receive the supplemental window DP pass.

I would have the simplification sibling accept indexed chain views and return survivor selections. That lets the boundary compiler avoid constructing temporary contours merely to select a subset of their vertices.

Sharing results requires the actual simplification context to match: rounded coordinates, pins, chain endpoints, closed-ring anchors, and algorithm. The current DP anchors depend on the clipped ring. A cached source-chain result is not automatically the result that the cell would have produced.

Two additional details prevent a wholesale reordering:

- First-pass edge pins are classified in base coordinates; window thinning classifies rounded zoom coordinates. These differ at rounding boundaries. See [build_edge_flags](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:1234) and [thin_window_runs](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:1189).
- Area survival is decided on the cell's normalized geometry. Filtering whole polygons or holes earlier can change which fragments survive.

Consequently, “simplify globally, then tile” is a geometry-policy rewrite. It needs more justification than “the tolerance is the same.” I would first remove representation and ownership costs while retaining the current simplification contexts.

**Smaller changes worth doing, but below the architectural rewrites**

- **Carry normalization provenance and consume terminal fragments.** Root preparation copies and normalizes the input. At OSM z14, rescaling is an identity, tolerance is zero, and minimum area is zero, yet emission copies and normally normalizes again. A fragment that has undergone only identity cuts can retain its normalized status. At the terminal level, emission can consume the owned fragment because no descendant needs it. This should remove an entire geometry copy and some redundant topology work on an important common path. Ocean union and exact clipping outputs can likewise carry their established normalization status.

- **Prepare source geometry once across compatible layer matches.** Ways invoke emission for every match; relations additionally clone and unwrap component rings inside the match loop. Quantization, pin preparation, and root normalization can repeat for the same source geometry. See [way emission](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:680) and [relation emission](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:404). Prepare wrapped, quantized source geometry once. Share emitted geometry only where the complete policy matches: different root zooms, caps, area thresholds, or tolerances prevent unconditional sharing.

- **Batch ring-cap bisection.** [partition_shape_to_ring_cap](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:618) runs two complete rectangle intersections at every split. A prepared boundary operation could compute the shared cut once and extract both closed halves. Preserve the existing longer-axis split, midpoint, traversal order, and zero area threshold. Balancing by hole count changes the partition result. This is potentially valuable for dense island tiles, but the path only runs above 500 surviving contours; it cannot explain ordinary polygon throughput.

- **Move ownership checks ahead of child clipping.** Children are clipped before `descend` rejects unowned cells. Testing the child filter first avoids constructing geometry that is immediately discarded.

- **Cache bounds and vertex counts; encode directly from ring views.** Repeated bbox scans and frontier vertex recounts are unnecessary. The final translation into `tile_points` can also be replaced by an encoder accepting global-coordinate ring views plus an origin. These are useful bandwidth reductions, not the main redesign.

The pbfhogg APIs are already sufficient to feed this work: [way references, locations, and shared-node pin bitmaps](/home/folk/Programs/elivagar/research/pbfhogg/src/read/elements.rs:263) are available. The intervening materializations are largely in Elivagar. I would retain node/edge identity and pin information through geometry preparation instead of repeatedly rebuilding coordinate-key sets. A batched decode-into API could help afterwards, if this works; it is not a prerequisite for the redesign.

`surface.rs` performs label placement rather than per-cell descent. Its scans are not this multiplier. Likewise, changing the float helpers in `tiles.rs` would miss the integer polygon path.

**Correctness findings from the source**

These are source-level findings; I did not execute reproductions.

- **The fast clipper applies a positive area filter during descent.** It drops outer and hole contours below `Area::ONE`, while the exact fallback clips with `Area::ZERO`. Since integer true area floors doubled area divided by two, a nondegenerate triangle with doubled area 1 is discarded by the fast path. A tiny hole can therefore disappear from the base-grid descendant geometry before the requested zoom's area policy runs. See [the fast-path filters](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:833) and [area conversion](/home/folk/Programs/elivagar/crates/lib/src/geometry/polygon.rs:92).

- **Window thinning loses source-junction protection.** First-pass pins include `params.pins`. The second pass constructs a new mask containing only window-run endpoints, then simplifies the whole shape. A previously protected interior junction can be removed when any part of the ring triggers window thinning. Whether that breaks a particular shared boundary depends on the neighboring geometry, but the pin protection itself is absent from this pass.

- **A singleton root is emitted without clipping to its buffered rectangle.** `root_bisect` returns immediately for a one-cell range. Normally the bbox is contained in that tile, but world-clamped root indices do not establish containment for geometry crossing the world edge. An antimeridian-shifted polygon can reach emission outside the intended buffer. See [the singleton branch](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:336). Separately, root selection uses zero margin, so buffer-only neighboring roots are omitted; changing that would change tile presence.

- **Quantization can reject nonempty NonZero geometry before repair.** `ring_is_valid` rejects zero signed shoelace area. A self-crossing ring can have cancelling signed area while enclosing nonempty NonZero regions. Such input is discarded before normalization can repair it. See [ring_is_valid](/home/folk/Programs/elivagar/crates/lib/src/geometry/int_ocean.rs:951).

For the major rewrites, unchanged decoded geometry is a realistic target, but byte identity with today's implementation needs separate attention to ring starts, collinear vertices, component ordering, and command encoding. Any intentional ocean byte change also needs the artifact policy version updated. None of these opportunities requires changing the CLI, provenance semantics, or explicit-or-unset input selection.
