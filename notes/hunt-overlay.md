**I would rewrite the relationship between clipping, simplification, and normalization.** The largest opportunity is to stop repeatedly turning known polygon boundaries back into an unstructured segment soup and reconstructing their topology. For huge inputs, the second strong opportunity is a parallel arrangement engine that preserves the current global snapping semantics.

These are code-based hypotheses, not measured speedup claims. I only inspected source. The specified `research/iOverlay/` directory is absent in this checkout; I inspected cached i_overlay 9.1.0 source for upstream mechanisms. The in-tree dependency declaration names 9.2.0, so I have not treated the cached version as an arithmetic oracle.

| Priority | Change | Classification | Conviction |
|---|---|---|---|
| 1 | Carry topology through clipping and transformations; normalize only when necessary | Full coherent rewrite spanning engine and caller | High |
| 2 | Parallelize large arrangements within each global snap pass | Substantial engine rewrite | High |
| 3 | Replace repeated graph and contour materialization with compact boundary storage | Structural representation rewrite | High for memory, medium for wall time |
| 4 | Stitch ocean mosaic boundaries instead of performing a general union | Specialized end-to-end rewrite | Medium |
| 5 | Fuse passes and correct workspace sizing defects | Local changes | High, but smaller payoff |

Several obvious suggestions are already implemented. The engine retains split, fill, graph, extraction, and recycling buffers. Its adjacency is already CSR. Degree-two graph traversal already has a direct path. Small sorts already bypass the bin sorter below 64 elements. Most significantly, the pyramid already has bbox rejection, identity copies, and a specialized rectangle clipper before invoking the general engine. Further work should build on those facts.

1. **Replace repeated normalization with topology-aware geometry processing.**

   The single-contour normalization fast path is expensive even when its answer is "unchanged." [`find_intersections`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/simplify.rs:108) builds segments, sorts them, merges duplicates, runs intersection finding, and then checks for repeated vertices. Below 64 vertices, that last check is another pairwise search; above it, it copies and sorts the points.

   The split strategy uses a nested candidate scan below 4,000 segments. Its x-range break and y-range rejection help, but inputs with overlapping bounding ranges can still require quadratic candidate work despite having no actual crossings. See [`list_split`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/split.rs:353).

   Multi-contour shapes have no corresponding unchanged-input path. [`normalize_into`](/home/folk/Programs/elivagar/crates/lib/src/geometry/int_ocean.rs:801) sends them through the complete Subject operation, including fill, graph construction, extraction, and potentially hole binding.

   The caller repeatedly creates precisely these workloads. [`root_fragments`](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:274) normalizes the source, while [`emit_cell`](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:434) rescales and simplifies each fragment, then normalizes again unless the convex single-ring bypass applies.

   The stronger design is a polygon representation that carries:

   - Ring roles, containment, and contact information.
   - Whether its current boundaries are already fully noded.
   - Stable edge identities and reusable spatial indexing.
   - Which vertices and edges a transformation changed.
   - The information needed to reproduce the current output traversal.

   Keep the existing DP and VW decisions. Have those transformations report their resulting changes. Validate the resulting geometry against retained topology, and invoke arrangement construction only when the transformation actually requires repair.

   For a small single contour without reusable information, use a direct exact classifier over its boundary: detect filtering requirements, repeated vertices, and forbidden intersections without first constructing the complete split-solver representation. For larger inputs, use an indexed classifier. A conservative rejection can enter the existing arrangement algorithm.

   For multiple rings, the useful common case is broader than convexity: an unchanged simple outer with strictly contained, mutually disjoint holes. Its boundary and ownership do not need a new winding sweep and hole-binding sweep.

   **Why this can pay:** it removes entire categories of work from successful calls. Pooling only makes the existing work cheaper; this design avoids sorting, splitting, fill computation, graph construction, and rebinding where they establish facts already known.

   **What the caller must change:** pass geometry plus topology and mutation information, rather than only `Vec<Vec<Point>>`. Preserve that information across identity clips and transformations that make no geometric change.

   **Exact-output risk:** "valid polygon" is not sufficient to return its input verbatim. The current perfect single-ring path preserves its starting vertex, while reconstructed output follows graph traversal order. Multi-ring reconstruction also determines hole order and contour rotation. The replacement must reproduce these distinctions, including collinear cleanup and area filtering.

   Quantization and DP/VW can introduce contacts, collapse channels, or change containment. Their inputs being valid proves nothing about their outputs. The design needs exact post-transformation validation, not a persistent unchecked validity flag. Cases with touching boundaries also require the current non-OGC traversal semantics.

   This is my first choice for the many-small-call workload. A standalone convexity screen is a much narrower opportunity.

2. **Make rectangle clipping a batched boundary operation that participates in that topology model.**

   Adding a rectangle fast path is not the opportunity: [`intersect_shapes_with_rect`](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:754) already has one.

   Its remaining structure is costly. Each child is processed separately. The fast clipper clones rings and runs four successive half-plane operations. Those operations materialize chains, endpoint lists, partner arrays, visited arrays, and reconstructed rings. Holes are processed separately and subsequently assigned to outers. Ambiguous cases discard that work and enter the general engine. See [`clip_shape_rect_fast`](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:809) and [`clip_ring_half_plane_multi`](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:912).

   The identity tier avoids overlay work but still copies every point into another owned shape.

   I would replace the four separate child calls with one operation over the parent boundary and the four buffered child rectangles. It should:

   - Classify parent edges against the relevant cut lines together.
   - Share untouched boundary runs between outputs.
   - Materialize cut events and replacement boundary runs.
   - Preserve ownership for unaffected holes.
   - Identify the boundary contacts that need exact arrangement processing.

   Buffered child rectangles overlap. This must produce four overlapping intersections, rather than divide the parent into four exclusive quadrants.

   The engine-facing shape should be a prepared parent plus a batch of rectangles, returning boundary views and owned changed portions. An ordinary reusable scratch argument is useful but does not solve the repeated ownership and reconstruction problem.

   **Why this can pay:** it removes repeated subject scans and copies, shares preparation between siblings, and avoids rebuilding topology independently for every result. It also addresses allocations in the caller's existing fast path, which engine buffer reuse cannot reach.

   **Exact-output risk is unusually important here.** [`crossing_point`](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:1060) uses a different rounding policy from the overlay's crossing solver. The overlay also has endpoint snapping and iterative repair. Consequently, replacing the current fast path with an exact rectangle overlay is not byte-preserving.

   To retain output, the batched implementation must reproduce the current half-plane sequence, rounding, fallback decisions, and result ordering independently for each child. It must not clip every descendant directly from the original source: intermediate rounded geometry participates in the current result.

   This belongs with opportunity 1 as one coherent rewrite. Moving the existing clipper into `overlay/`, or merely pooling its temporary vectors, would capture only part of the benefit.

3. **Rewrite large-input arrangement execution around parallel global snap passes.**

   The huge-union path has explicit serial ownership. [`union_pieces`](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:154) submits one contour soup to one normalizer. The large splitter constructs spatial fragment groups, but [`process`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/split.rs:975) processes every group sequentially into one marks vector.

   That is a strong opportunity on a many-core machine. Candidate testing within groups reads immutable segments and produces marks. Existing code already normalizes pair orientation and later sorts and deduplicates marks.

   Cached upstream code contains a concrete [`parallel_split` implementation](/home/folk/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/i_overlay-9.1.0/src/split/solver_fragment.rs:114). The useful mechanism is parallel candidate discovery followed by deterministic reduction; adopting upstream's broader APIs is unnecessary.

   My redesign would retain a global sequence:

   1. Generate candidate pairs in parallel.
   2. Compute crossings using the current arithmetic, pair orientation, and snap state.
   3. Merge, sort, and deduplicate marks deterministically.
   4. Apply splits and merge segments.
   5. Advance the snap state exactly as today.

   Parallelize large sorts and segment reconstruction as part of the same execution model. Use prefix counts and disjoint output ranges so reconstruction does not funnel through concurrent pushes or a single append loop.

   The current fragment representation also deserves replacement. Long segments are represented in multiple columns, carrying original segment coordinates in every fragment. A pair can reach the crossing solver through more than one group, with duplicate marks removed later. A compact adaptive spatial index with explicit pair ownership can reduce both replicated storage and duplicate exact arithmetic.

   The practical first implementation can retain the existing groups while parallelizing them. A stronger implementation changes candidate generation too.

   **Why this can pay:** it attacks a large call that currently cannot use the machine, without needing a new geometric interpretation. The benefit is limited to the large arrangements actually executed by the run; it is not a claim about time already avoided through ocean artifacts.

   **Risks:** one dense group can remain a straggler; per-task marks can increase peak memory; fill and extraction remain serial limits after splitting improves. Those require load-balanced tasks, bounded mark storage, and compact reduction.

   Preserve the global snap passes. Arbitrarily unioning batches and then unioning their results is a different algorithm: intermediate integer snapping can change later intersections. Mathematical union associativity does not establish byte equivalence.

4. **Replace the general backend's materialization chain, rather than only tuning its containers.**

   A general call maintains several representations of substantially the same boundary:

   - Segments and segment-sort scratch.
   - Spatial fragments and split marks.
   - Fill bytes.
   - Links containing endpoint coordinates and node IDs.
   - Endpoint records and endpoint-sort scratch.
   - CSR adjacency.
   - A walked contour buffer.
   - Copied contour vectors and shape shells.
   - A second collection of segments for hole binding.

   These are visible across [`GraphBuilder`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/fill.rs:60), [`build_nodes_and_connect_links`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/graph.rs:61), and [`extract_into`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/extract.rs:1120). Reusing allocations leaves the representation changes and memory traffic intact.

   I would use compact vertex storage, indexed boundary edges, and a contour arena with ranges. Extraction should write directly into that arena. Hole binding should attach contour IDs, and collinear cleanup should operate on those contours after binding. The pyramid should consume ranges and boundary views, avoiding conversion back into nested owned vectors between operations.

   Retained input ring connectivity should supply successor information where topology remains unambiguous. General angular routing is needed at actual junctions. The current implementation already special-cases degree two during traversal; the larger gain is avoiding the reconstruction that discovers those ordinary connections.

   **Why this can pay:** lower peak memory and less memory bandwidth, especially during huge calls. It also makes the shared-boundary outputs proposed above practical.

   **Risks:** a fully general half-edge structure can consume more memory than today's arrays. The representation needs to be designed for boundary processing, with junction information only where needed. Preserve extraction order explicitly rather than allowing arena allocation order to determine output.

   Hole ownership must still be decided on raw noded geometry before collinear removal. The current [`extraction sequence`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/extract.rs:1192) makes that ordering explicit. A fused implementation must preserve its behavior.

   Afterwards, if this works, broader flat-geometry APIs could follow. They should not be a prerequisite.

5. **For ocean mosaics, consider replacing general union with boundary stitching.**

   This is potentially a larger win than accelerating the general union, but I have lower conviction because it depends on establishing source and quantization invariants.

   The ocean input is processed as pre-split pieces. [`union_pieces`](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:154) then discards piece structure and asks a general nonzero arrangement to recover the combined boundary.

   The existing initial sort and merge already cancel identical opposite segments. The stronger algorithm would also split differently segmented shared borders into common intervals, cancel interior intervals, and connect the surviving boundary directly.

   Have the ocean producer retain piece identity and known source-border relationships. Normalize local defects where necessary, stitch shared boundaries in parallel, then extract and bind the surviving contours. This replaces general interior intersection discovery with processing of known interfaces.

   **Why this can pay:** it eliminates work rather than distributing it. Most source coastline edges need not participate in a global candidate search or winding sweep merely because their pieces share artificial cut boundaries.

   **Risks:** quantization can introduce contacts or crossings away from source seams, and shared borders need not have identical segmentation. Point contacts, holes crossing source cells, and existing contour ordering all matter. A reliable implementation must establish those conditions and handle actual defects; simply deleting reverse-equal edges is insufficient.

   I would implement this as a complete specialized mosaic assembler, not a hierarchical sequence of ordinary unions. Its payoff should be judged against the parallel general engine.

The local changes are worth doing, but I would keep them below those rewrites:

- **Fuse fill consumption with boundary selection.** [`FillHandler`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/fill.rs:127) already provides the mechanism, yet the current handler stores all fill bytes and another pass constructs links. Extraction then reevaluates the same inclusion predicate on already-filtered links. Preserve original segment ordering when fusing: the sweep processes equal-origin edges in angular order.
- **Avoid reserving a whole graph's worth of points for one contour.** [`extract_into`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/extract.rs:1131) reserves the total visited-link count in its single-contour buffer. That can greatly exceed the largest contour when output contains many small components.
- **Replace linear contour-pool searches with size classes.** [`take_vec_with_capacity`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/extract.rs:1094) searches for a sufficient buffer, then scans again for the largest if none qualifies. Repeated misses can make pool management expensive.
- **Remove an allocation inside the supposedly reused fragment path.** [`fragment_split`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/split.rs:935) replaces its buffer with `FragmentBuffer::empty()`, which allocates border capacity. Restructuring field borrowing avoids allocating that temporary replacement each pass.

I also found two concrete workspace-sizing defects:

- [`Reserve::reserve_capacity`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/extract.rs:37) passes `desired_capacity - current_capacity` to `Vec::reserve`, although `reserve` takes an amount additional to **length**. With length 0, capacity 8, and desired capacity 12, `reserve(4)` need not grow anything. [`KeyExpList`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/scan.rs:55) has the same issue. Pushes remain safe, but the intended reservation can fail and move growth into hot loops.
- [`init_fragment_buffer`](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/grid.rs:65) counts nonvertical fragments over `[i0, i1)`, while insertion also writes the final group `i1`. Single-column nonvertical segments contribute no reservation count. This under-reserves without changing geometry.

For exact output, my preferred implementations preserve coordinates, contour traversal, shape and hole ordering, feature partitioning, filtering, and determinism. Ring rotation could be changed while preserving exactly the same geometric boundary, but it would still change decoded sequences and tile bytes. That relaxation is unnecessary for the main opportunities above. Changing rounding or intermediate union grouping is more consequential: it can change the boundary itself.
