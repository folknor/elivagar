I would rewrite the line path around shared source geometry and a final tile-local line compiler. The largest opportunity is eliminating repeated geometry materialization, followed by sharing simplification across zooms and making clipping proportional to the line's actual tile coverage.

These are three compatible architectural changes. Optimizing the existing merger in isolation is worthwhile, but has a lower ceiling.

This assessment comes from source inspection only. I did not read project Markdown, history or previous results, and did not change files, build, test or execute the pipeline. The ranking reflects structural opportunity, not measured speedups.

| Priority | Opportunity | Classification | Main payoff |
|---|---|---|---|
| 1 | Carry shared line geometry through fanout; merge before final encoding | Full rewrite across emission, sort and assembly | Removes geometry copies, codec passes and serialized payload traffic |
| 2 | Build one exact Douglas-Peucker hierarchy per source line | Full simplification-kernel rewrite | Removes repeated distance scans across zooms and layer matches |
| 3 | Route segments to tiles instead of scanning the full line in every bbox tile | Full clipping/fanout rewrite | Removes the line-length times bbox-area cost |
| 4 | Decode enriched ways into useful geometry batches once | Structural ingress change, medium expected value | Removes repeated coordinate decoding and unnecessary reference arrays |
| 5 | Replace allocating merger internals with flat spans and a compact endpoint graph | Medium-value subsystem rewrite | Reduces allocation, hashing and coordinate copying |

Several starting assumptions need correcting. Projection already happens once per matched way, before its layer matches are emitted. Simplification already happens once per requested zoom per match, before tile clipping. The expensive witness checker is absent from release builds. Ring grouping is absent from the default MVT generation path.

The remaining duplication is nevertheless substantial.

1. **Replace early MVT materialization with shared line geometry and final tile compilation.**

   The current path repeatedly changes the representation of the same geometry:

   `Mercator points -> clipped points -> tile coordinates -> MVT command words -> sort payload -> owned command words -> concatenated command words -> absolute line segments -> merged coordinates -> MVT command words`

   Emission quantizes and encodes every clipped piece in [emit.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:894). Assembly then allocates an owned command vector for every incoming feature in [wire_format.rs](/home/folk/Programs/elivagar/crates/lib/src/wire_format.rs:446). Same-attribute merging decodes each command stream while concatenating it, copies the concatenation into another vector, and discards the constituent buffers. Connected-line merging subsequently decodes that concatenation back into coordinates and encodes it again. See [mvt/merge.rs](/home/folk/Programs/elivagar/crates/lib/src/mvt/merge.rs:113).

   **The stronger redesign is to keep geometry as geometry until grouping and topology work are finished.** A source line packet should own coordinates, required vertices, its simplification hierarchy, and its layer matches with attribute visibility. Tile associations should reference coordinate spans and clipped endpoints. Attribute groups should collect those references directly. The tile-local merger should consume spans and emit final geometry once.

   The full version should also replace expanded per-tile geometry payloads in the external sort with compact associations backed by spatially bundled geometry. Region workers can load shared packets once and process their associated tiles. Geometry crossing regions needs replication or partitioned spans, but it does not need independent coordinate copies for every layer, zoom and tile.

   That storage must be designed for sequential regional reads. A global geometry file followed by random lookups from every tile would exchange one structural problem for another. Nor should every short z14-only line acquire a large persistent hierarchy and reference structure: packets without reuse can terminate directly as compact quantized spans.

   There is a serialized seam this also attacks. [WayAcc::flush](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:459) hands expanded records to a shared coalescer, and [SpillCoalescer::append](/home/folk/Programs/elivagar/crates/lib/src/sort/write.rs:392) copies their payload under one mutex. Reducing the bytes carried through this seam helps throughput even on fast disks. The rewrite should transfer ownership of bounded bundles to partition owners, eliminating that shared bulk-copy operation.

   This is my highest-payoff hypothesis because it reaches short lines and z14 output as well as long simplified lines. It removes allocation and memory traffic in addition to arithmetic.

   **Correctness risks:** the new representation must preserve per-zoom attribute equality, singleton versus merged feature IDs, representative feature placement, sub-line order, direction, duplicate handling and chain splitting. The current external sort also uses payload bytes as a deterministic tie-breaker in [sort/key.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:147). Changing the payload silently changes ordering unless ordering becomes explicit. That matters to cycle traversal and the vertex cap, not just compressed bytes.

   I would target identical decoded features and geometry first. Any changed ordering or command encoding should be declared explicitly; equal rendered coverage alone does not establish equivalence under the contract you gave.

   This change belongs on both the emission and assembly sides. Treating the existing `Feature`, record payload or `LayerBuilder` ownership model as a constraint would discard much of its value.

2. **Compute the Douglas-Peucker subdivision once and reuse it exactly across zooms and matches.**

   [emit_line_feature](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:976) calls Douglas-Peucker afresh on the original source at every zoom below z14. Each call resets keep flags, rediscovers splits and compacts an output vector. Closed lines additionally repeat canonical rotation, two farthest-point sweeps and several allocations in [simplify_closed](/home/folk/Programs/elivagar/crates/lib/src/geometry/simplify.rs:204).

   The important property is already present in the implementation: source coordinates, required vertices and closed-line anchors are independent of tolerance. The repeated calls are traversals of the same subdivision tree, stopping at different thresholds.

   **Replace them with one subdivision traversal down to the finest requested simplification tolerance.** Record which zooms retain each vertex. Coarser zooms then select vertices rather than recomputing point-to-segment distances. Required vertices partition the problem into independent intervals. Closed lines compute their annotated canonical rotation and anchors once, with an index mapping back to source order.

   This can reproduce the current output exactly. It does not require simplifying an already simplified line.

   One subtlety matters: a split's visibility depends on every ancestor having passed its threshold. A child's raw deviation is not sufficient. Either traverse the tree with the zooms still active at each node, or use the minimum deviation along the ancestor path as effective importance. Preserve the current strict `>` comparison, farthest-point tie behavior and source-order output.

   Build this once per source geometry, rather than once per `LayerMatch`. [ways.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:699) dispatches matches separately, so roads and their labels, or waterways and their labels, currently repeat geometry work at overlapping zooms. Their attributes and size-filter policies differ; their underlying simplification does not.

   The same traversal can produce zoom bboxes and visibility information, avoiding repeated source bbox scans and separate output bbox passes. A compact per-vertex zoom annotation is sufficient; retaining a heavyweight permanent tree is unnecessary.

   **Why this is plausibly valuable:** it removes whole repeated distance scans, especially on major roads, railways, rivers and boundaries. Closed lines also lose their repeated anchor preparation and temporary vectors.

   **Limits and risks:** many fine roads start at z13, while streams and ditches start at z14. They do not pay fourteen simplifications. Those rules are explicit in [streets.rs](/home/folk/Programs/elivagar/crates/lib/src/shortbread/streets.rs:146) and [water.rs](/home/folk/Programs/elivagar/crates/lib/src/shortbread/water.rs:114). This makes DP reuse less universal than the representation rewrite.

   Antimeridian copies, closed-line start requirements, coincident vertices, required closing duplicates and narrowed zoom windows all need identical treatment. Reassociating floating-point arithmetic can change borderline retention decisions. The implementation should reuse the current distance calculation while changing how often it runs.

   This is a full kernel rewrite with high confidence in the opportunity. It is also independently useful if the broader line-path rewrite has not yet landed.

3. **Replace bbox-wide clipping with spatial routing of segments.**

   For every simplified line, emission enumerates every tile in its axis-aligned bbox. Each candidate tile then receives the whole line. The clipper performs an outcode prepass and, unless rejected, walks all source segments in [clip.rs](/home/folk/Programs/elivagar/crates/lib/src/geometry/clip.rs:89).

   For a line with \(N\) vertices and \(B\) bbox tiles, this exposes roughly \(N \times B\) work. A long diagonal can occupy a thin corridor through a large rectangular tile range. Most work then establishes that the line misses a tile.

   **The stronger algorithm should enumerate line-to-tile intersections from source segments.** Walk the segments through the tile grid, account for the buffer, and collect ordered segment references per candidate tile. Clip only those referenced runs. A segment-bounds hierarchy can further reject or accept contiguous runs without inspecting each vertex.

   The target cost is proportional to source segments, actual segment/tile incidences and output vertices. It should not scale with the area of empty space enclosed by the line's bbox.

   Preserve source segment ordinals so that pieces entering the same tile repeatedly retain their current ordering and separation. Entirely interior runs can remain borrowed spans; only intersections need new coordinates. The same references can feed the tile compiler in opportunity 1.

   **This should operate on each zoom's selected source geometry.** Clipping the source first and then running DP on the pieces changes endpoints and therefore changes simplification. Likewise, recursively clipping a parent's already clipped geometry can introduce vertices and rounding differences. Spatial subdivision should narrow candidate references while final clipping still uses the original selected segments.

   The existing single-tile fast path is useful and should remain an algorithmic fast case. This rewrite principally targets long, winding and spatially sparse lines, rather than claiming a large benefit for every residential street.

   **Correctness risks:** candidate coverage around buffer edges, corners and world edges; preserving disconnected pieces; boundary tangencies; and matching the clipper's floating-point intersection order. I would initially retain the current intersection arithmetic and replace candidate discovery and traversal.

   There is also an existing candidate-selection asymmetry: tile enumeration uses the unbuffered bbox. A line contained just inside one tile is never emitted into its neighbor, even when it lies inside that neighbor's clip buffer. See [tiles.rs](/home/folk/Programs/elivagar/crates/lib/src/geometry/tiles.rs:112).

   That omission is established by the code; a visible rendering defect is not established by this inspection. A segment router that naturally discovers every buffered overlap would change output. Preserve the current candidate restriction for the performance rewrite, or declare buffer coverage as a separate behavior correction.

4. **Fuse enriched-way decoding and geometry preparation.**

   This is outside the kernel files, but it directly feeds their working sets.

   The enriched path decodes every way's locations while building plans to calculate extract extent, and decodes selected ways' locations again during processing. It also materializes every way's reference IDs, clones those IDs on the locations path, allocates e7 coordinates, creates a pin mask, and then fills the projected vector. See [plans.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/plans.rs:94) and [ways.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:570).

   With injected membership and shared-node pins, much of the reference materialization has no remaining geometry purpose. Membership is positional; pins already arrive as a bitmap. Counts are still needed for validation and missing-reference accounting.

   **Produce a concrete validated way batch containing coordinate spans, membership, pin positions and extent information.** Decode locations once. Project selected geometry directly into its packet. Retain e7 coordinate arrays only for consumers that require them, such as the relation way index.

   pbfhogg already has a useful foundation: [BlockBuffer::project](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:1135) and [GroupProjection](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:1256) collect useful information during block validation. These are currently crate-private. Exposing or extending that concrete path is justified; designing a general callback framework first is not.

   The present hook runs after geometry arrays have been validated, so using it alone does not eliminate the validator's packed-array scan. Eliminating that scan too requires a deliberately fused validate-and-decode implementation. The immediate, clearer win is removing Elivagar's duplicate decoding and unnecessary owned arrays.

   **Risks:** extract extent includes unselected and untagged ways, so filtering must not erase that accounting. Coordinate framing, conversion, missing references and malformed-input rejection must remain equivalent. The raw-input path still needs reference resolution and block-local pin detection.

   I expect medium value from this change independently, and greater value as the ingress to the shared-packet rewrite.

The merger itself has substantial avoidable work, but its basic degree-2 traversal is already appropriate. I would change its representation and ownership rather than replace it with a more elaborate graph algorithm.

[dedup_parallel_segments](/home/folk/Programs/elivagar/crates/lib/src/geometry/line_merge.rs:140) allocates a reversed coordinate vector for every segment and sometimes allocates another forward copy. It retains full coordinate vectors as hash keys. Endpoint construction allocates a `Vec<SegEnd>` for every distinct endpoint. Decoding clears `Vec<Vec<IntPoint>>`, dropping the inner allocations, and chain output creates another set of owned coordinate vectors.

A compact replacement would:

- Store coordinates in a flat slab with segment ranges and orientation flags.
- Hash canonical oriented spans without allocating reversed copies, with exact equality checks on collisions.
- Store endpoint degree and the first two incidences inline. Higher-degree endpoints stop traversal, so their complete adjacency vectors are unnecessary.
- Construct deterministic starts from segment endpoints and the degree table.
- Traverse oriented spans directly into final geometry, avoiding `merged: Vec<Vec<IntPoint>>`.

This is a medium-value rewrite independently, and a natural component of opportunity 1. It removes allocator traffic and memory amplification across many small groups.

The topology must remain tile-local and operate after quantization. Moving same-attribute merging globally ahead of simplification or clipping would change forced endpoints, quantized coincidences, local degree and clipping-created terminals. The current output contract gives a reason for that ordering even though it gives no reason to preserve today's stage boundaries.

The checker is not a release bottleneck. Its invocation and the expensive independent structures are guarded by `debug_assertions` or test configuration in [mvt/merge.rs](/home/folk/Programs/elivagar/crates/lib/src/mvt/merge.rs:170) and [line_merge.rs](/home/folk/Programs/elivagar/crates/lib/src/geometry/line_merge.rs:353). In debug builds it can plausibly cost more than merging: it independently deduplicates, canonicalizes and sorts coordinate vectors, reconstructs chains, decodes output and builds edge multisets.

Witness and chain-boundary records are nevertheless populated in release. Compiling out that bookkeeping is a valid local improvement. It is much smaller than eliminating coordinate copies and endpoint allocations.

Projection and ring grouping do not justify major standalone rewrites for the default workload:

- [project_e7](/home/folk/Programs/elivagar/crates/lib/src/geometry/projection.rs:76) already uses a shared latitude lookup table. It is not doing transcendental projection per tile. A global projected-node cache would add storage and lookup costs to avoid relatively cheap arithmetic. Reprojection does occur when relation members are read back, but that belongs to relation storage and reuse.
- Tile-coordinate conversion repeats across overlapping tiles and layer matches. Shared spans and geometry results can remove some of it. Premature integer quantization or changed rounding rules would alter geometry.
- [group_rings](/home/folk/Programs/elivagar/crates/lib/src/geometry/polygon.rs:189) is a linear orientation-based scan. Its generation consumer is the MLT conversion path in [mlt.rs](/home/folk/Programs/elivagar/crates/lib/src/mlt.rs:288); default MVT generation does not regroup rings this way. For MLT, carrying polygon structure through assembly would avoid decoding and rediscovering it. That is the same representation opportunity, with a smaller workload reach here.

A few additional findings matter to implementation and diagnostics:

- Both DP routines recurse without a depth bound. Required vertices are split at the first required interior index, producing depth proportional to pin count on heavily pinned lines. Adversarial DP splits can also produce deep recursion and quadratic work. An explicit work stack and prepartitioned required intervals remove the stack-overflow risk; hierarchy reuse alone does not remove worst-case DP work. See [simplify.rs](/home/folk/Programs/elivagar/crates/lib/src/geometry/simplify.rs:325).
- `MAX_LINE_VERTICES` bounds concatenation, not an individual input segment. An oversized first segment is accepted whole. It therefore provides no hard bound on chain allocation or tile geometry size. See [line_merge.rs](/home/folk/Programs/elivagar/crates/lib/src/geometry/line_merge.rs:179).
- [ring_is_simple](/home/folk/Programs/elivagar/crates/lib/src/geometry/polygon.rs:322) detects proper crossings only. Touches and collinear overlaps can pass. It is a diagnostic limitation, not a generation hotpath or proof of full ring simplicity.

For the three principal rewrites, my target would be unchanged decoded features and geometry, including segmentation and ordering where they affect the result. Their expected payoff comes from performing fewer transformations and visits. No geometric approximation is needed to obtain it.
