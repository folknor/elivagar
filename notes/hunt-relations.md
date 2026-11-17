I would rewrite the relation path around a retained relation plan, shared member geometry, and independently schedulable assembly and emission work. The current structure creates three avoidable limits: serial preparation, repeated coordinate materialization, and one-worker ownership of an entire large relation.

These are conclusions from source inspection, not measured speedup claims. I read the scoped code, its callers, emission and pyramid code, and the relevant pbfhogg internals. I did not read project Markdown or history, modify files, or build or run the software.

| Priority | Opportunity | Classification | Conviction |
|---|---|---|---|
| 1 | Replace the prescan and deferred lookup tail with a dependency-driven relation plan | Full architectural rewrite | High |
| 2 | Replace coordinate-chain assembly and exhaustive ring pairing with a topology compiler | Full algorithmic rewrite | High |
| 3 | Hand emission prepared geometry and divisible jobs | Architectural rewrite across the emission seam | High for large polygon relations |
| 4 | Give boundary labels a separate, demand-driven computation path | Targeted algorithm changes | High locally; workload-dependent overall |

**1. Retain the relation plan and resolve dependencies as ways arrive.** This is the strongest end-to-end opportunity.

The [prepass](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/prepass.rs:79) already reads relation tags, matches area relations, walks their members, and folds boundary metadata. It discards the actual relation jobs and retains membership sets and boundary metadata.

The main read subsequently buffers decompressed relation blocks. If that buffer exceeds its cap, it discards them and recovers them through replay or another filtered read. Only after all way processing finishes does it finalize the way index and start the [relation tail](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/mod.rs:599).

That tail has another serialization point. Its `flat_map` prepares every matching relation in a block before yielding them through `par_bridge`. Preparation includes every member lookup, coordinate decode, allocation, and projection. All of that happens on the serial iterator side of the bridge, under its pull lock. Parallelism starts after the expensive expansion into owned coordinates. See [preparation](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:46) and [dispatch](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:225).

Consequently:

- One huge relation monopolizes preparation while other workers cannot acquire further jobs.
- One block can expand into a large amount of prepared geometry before its first relation starts assembly.
- Shared ways are looked up, decoded, and projected independently for every relation occurrence.
- No relation can finish while ways are still processing, even when all its own dependencies are available.

The stronger replacement would make the first relation visit produce the actual execution plan:

1. Retain compact jobs containing relation ID, matched attributes, member roles, and member occurrence order.
2. Assign distinct referenced ways dense geometry slots.
3. Build a compact reverse incidence table from each way slot to its dependent relations.
4. Publish each resolved way's geometry once, directly from way processing.
5. Schedule a relation when its last dependency becomes available.
6. At end of input, resolve absent dependencies using the existing missing-member and salvage behavior.

Deduplicate geometry storage and lookup, **not member occurrences**. A repeated member can affect joining and shared-vertex pins.

This removes the reason to retain or replay relation blocks. It also removes the dependency on a globally finalized, sorted way index before relations can begin. The index implementation belongs to the sibling scope; the interface this scope wants is clear: stable geometry handles, cheap endpoint and size metadata, immutable published geometry, and batch access without repeated allocation.

The current way path already has the coordinates needed to publish those handles. Instead, it [clones them into `way_puts`](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:603), sends them through the drain, encodes them, then later decodes them through relation lookups. The redesign should remove that ownership round trip for geometry that remains live.

Memory management is part of this rewrite. Keep shared geometry while dependent jobs need it, release it after its final consumer, and spill cold geometry when necessary. Do not replace compressed storage with an unlimited global cache of projected `Point`s. Projection expands each coordinate from eight bytes to sixteen, before allocation and indexing overhead.

The plausible payoff is substantial because this changes both work volume and the critical path: fewer relation reads, fewer repeated member decodes, no serial coordinate-expansion producer, and overlap between ways and ready relations. It is more than parallelizing `prepare_relation`.

The risks are dependency accounting, premature publication, and memory retention. Missing ways cannot be declared missing until the input establishes that. Relations must consume fully resolved geometry, and concurrent jobs need admission based on geometry and working memory rather than job count alone. Early relation work also competes with way work for CPU and memory bandwidth, so the scheduler must manage them together.

Boundary lines impose one necessary constraint: their metadata must be complete before emitting member ways. The current [boundary fold](/home/folk/Programs/elivagar/crates/lib/src/pipeline/boundary_prescan.rs:11) combines parent relations without requiring `type=multipolygon` or `type=boundary`. Therefore, I would retain a complete metadata planning pass. I would make that pass useful enough to eliminate the later reconstruction, rather than try to abolish it indiscriminately.

**pbfhogg already contains much of the reader machinery this needs.** Its private [classification pools](/home/folk/Programs/elivagar/research/pbfhogg/src/scan/classify.rs:803) perform scheduled positional reads and parallel per-block work. Its [fused transforms](/home/folk/Programs/elivagar/research/pbfhogg/src/read/reader.rs:364) run work on decode workers rather than serializing it after block delivery.

More significantly, [validator projections](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:1135) can collect selected data during validation. Existing [relation projections](/home/folk/Programs/elivagar/research/pbfhogg/src/read/project_ids.rs:104) retain member-array locations, but omit the roles and tags needed here. Extend that mechanism to produce compact relation jobs after validation succeeds. The planner should not need a fully built block followed by another complete element walk.

These facilities are private, so this requires changing our dependency. That is appropriate here. Afterwards, if this works, a reusable reader interface could follow. It should not dictate the execution design.

I would not simply switch to `par_map_reduce`: its implementation explicitly ignores `BlobFilter`, so it would decode every element during a relations-only pass.

**2. Replace the assembler with a topology compiler that manipulates fragment references until rings are settled.**

The current [joining algorithm](/home/folk/Programs/elivagar/crates/lib/src/multipolygon.rs:452) stores growing chains as coordinate vectors. Appending is sometimes cheap, but prepending allocates a replacement vector and copies the existing chain. Reversing a member can allocate another temporary vector. The second pass rebuilds the endpoint map, performs one successful merge, then starts again.

There are two independent bad cases:

- Repeated prepends copy successively larger prefixes, producing quadratic coordinate movement.
- Repeated second-pass joins rebuild maps over surviving chains, producing quadratic bookkeeping even before coordinate copying.

A country boundary assembled from many fragments is exactly the kind of input where these costs can become disproportionate.

The replacement should represent a chain as an ordered sequence of oriented fragment handles, with endpoint metadata and vertex counts. Joining changes fragment links and orientation bits. It should not copy the coordinates already joined. Once a ring is accepted, allocate its final coordinate storage once and gather its vertices in order, skipping the same junction and closure vertices as today.

For ordinary degree-two endpoint components, an endpoint graph provides linear traversal in member count, followed by linear vertex gathering. Independent components and rings can execute in parallel inside one relation.

The important difficulty is ambiguous or broken input. The current endpoint map keeps only one chain per endpoint, overwrites previous entries, and later attempts recovery. A graph walk that finds more valid cycles is not automatically equivalent. The replacement must explicitly reproduce the current branch choices and salvage results where they affect output.

That does not require preserving the current coordinate-vector implementation. Encode the observable branch decisions in the topology algorithm, with deterministic member ordinals and endpoint choices.

Also preserve its actual endpoint equivalence: [joining uses projected coordinates rounded at `1e9`](/home/folk/Programs/elivagar/crates/lib/src/multipolygon.rs:435). Replacing that with OSM node IDs, exact E7 equality, or tile-grid equality would change which fragments connect.

Ring role assignment needs an equally substantial replacement. [Unclassified rings](/home/folk/Programs/elivagar/crates/lib/src/multipolygon.rs:285) test containment against every explicit ring and every other unclassified ring. [Hole pairing](/home/folk/Programs/elivagar/crates/lib/src/multipolygon.rs:379) tests each hole against every outer. Each candidate comparison can rescan coordinates for bounding boxes, boundary membership, point-in-polygon, and shell area.

There is no spatial rejection before the expensive containment predicate. Even widely separated islands pay for one another.

I would produce ring descriptors containing cached bounds, area, canonical ordering information, and an indexed edge representation for repeated spatial queries. Then:

- Index potential containers spatially.
- Query candidate shells in the appropriate world copy.
- Evaluate the existing containment predicate only for candidates.
- Reuse containment results for inferred depth and explicit-hole pairing.
- Choose the smallest containing original outer with the existing tie behavior.

This removes exhaustive comparisons for geographically separated rings and avoids scanning the full outer boundary for every witness point.

The containment predicate deserves care. It uses the first vertex or edge midpoint that is off the shell boundary, rather than proving that the entire inner ring lies inside. Therefore, requiring full inner-bbox containment would silently change behavior for crossing or malformed rings. Spatial indexing must conservatively prune candidates under the predicate actually implemented. Touching rings and antimeridian-relative shifts must retain their current treatment.

This rewrite has high payoff for two different large-relation shapes: long fragmented rings and relations containing many separate rings. Its main risk is changing accepted geometry through seemingly reasonable topology improvements.

I also inspected pbfhogg's [ring assembler](/home/folk/Programs/elivagar/research/pbfhogg/src/geo.rs:824). It is not a suitable drop-in replacement: it uses full backtracking, can take exponential time without its budget, and has different coordinate and rejection semantics.

**3. Change the emission handoff so a large relation is divisible and geometry is prepared once.**

After assembly, [relation processing](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:378) retains the original projected member vectors and the assembled ring vectors. For each polygon match and component, it then clones outer and inner rings and unwraps them again.

[Multipolygon emission](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:1123) subsequently quantizes those rings, derives pin flags, creates another pin set, and invokes the pyramid. This repeats across matching polygon layers.

The better handoff is an immutable prepared polygon containing:

- Its assembled, correctly wrapped source geometry where labels or measurements still need it.
- One checked integer base shape.
- Pin information aligned with that shape.
- Bounds and cached measurements.
- The layer-specific output requests.

Build the integer representation once per component and world copy, then let polygon emissions borrow it. Preserve source geometry only as long as consumers require it.

Already-oriented floating rings are not sufficient to skip integer orientation checks: quantization can collapse vertices or change the relevant area result. Likewise, assembled rings are not necessarily topology-normalized. Orientation is not a certificate that root normalization is unnecessary. The useful seam is a checked integer shape, with normalized base geometry shared where the downstream policies permit it.

The biggest parallelism opportunity is unusually concrete. The pyramid already exposes [ `split_for_parallel` and `emit_shape_pyramid_cell`](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:189). Ocean emission [uses them](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:532). Relation emission does not.

Use these facilities to turn a large polygon into disjoint subtree jobs. The split emits ancestors once, and workers process the returned descendants. This preserves the pyramid's existing clipping and simplification structure while allowing one huge polygon to occupy multiple cores.

That matters because more relation-level parallelism cannot solve the last-relation problem. The current tail's wall time is bounded below by its serial preparation, total parallel work divided by available workers, and its slowest indivisible relation. The first rewrite attacks preparation; this one attacks the indivisible relation.

There is also an immediate memory defect in the execution structure: the eight-MiB accumulator threshold is checked **after the entire relation finishes**. It does not bound a giant relation's output. One relation can accumulate all its records before flushing, and shrinking capacity afterwards cannot undo that peak. See the [fold body](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:270).

Make emission write through a bounded flushing sink during traversal. The existing `FeatureRecordSink` abstraction already supplies the insertion point. Fanout totals can be accumulated independently of retaining all emitted records.

Keep fragment jobs and their scratch memory under one execution budget. Materializing every subtree before scheduling any of them would exchange a CPU bottleneck for a memory bottleneck.

Deterministic scheduling order is unnecessary if the same records are produced: the external sort already establishes [total order by key and payload](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:179). Deterministic geometry, feature IDs, and floating-point measurement order remain necessary.

This proposal crosses into the polygon-pyramid sibling scope. It is nevertheless essential to the relation redesign: handing one enormous polygon to a serial traversal leaves a structural bottleneck intact.

**4. Boundary labels deserve a separate computation path.**

Administrative boundary relations can require assembled geometry only for `boundary_labels`. Their boundary lines already emit from member ways. A pyramid parallelization does nothing for those label-only jobs.

Two avoidable costs stand out.

First, `process_prepared_relation_into` builds the [shared-vertex pin map](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:94) before checking whether assembly produced anything, and regardless of whether any polygon output needs pins. That map scans all member vertices and builds relation-wide hash tables. For a label-only country relation, this is entirely unnecessary.

The relation plan should identify its consumers. Labels need assembled geometry, areas, and point placement; they do not need polygon simplification pins.

Second, [surface-point aggregation](/home/folk/Programs/elivagar/crates/lib/src/multipolygon.rs:213) computes a surface point for every polygon component, then chooses the successful component with greatest net planar area.

Instead, compute component areas first and visit components in descending area order, preserving original order for ties. Stop at the first successful point. This gives the same winner while avoiding surface searches across all the smaller islands.

The surface search itself contains a stronger algorithmic opportunity. For every outer scanline interval, [it rescans and sorts every hole's intersections](/home/folk/Programs/elivagar/crates/lib/src/geometry/surface.rs:297). Interval subtraction repeatedly allocates replacement segment lists. A complex outer with many intersections and many holes multiplies the same work.

Compute each ring's intersections once per existing scanline. Sort and combine the hole intervals once, then subtract them from the outer intervals with an ordered sweep. Retain the existing scanlines, candidate validation, and tie rules so label coordinates remain unchanged.

This is a targeted rewrite of label computation, not a replacement label-placement heuristic. Its payoff could be large for complex administrative relations, although its contribution to full-run wall time depends on how much of the relation tail those labels occupy.

**The remaining local changes are useful, but secondary.**

- **Parallelize preparation without replacing the lifecycle.** Yield lightweight jobs or block-owned relation references, then resolve geometry on workers. This removes the bridge's serial lookup ceiling, but retains deferred processing, repeated member decoding, and the final straggler. It is a credible medium-value change, not the complete redesign.
- **Replace quadratic canonical rotation.** [`normalize_ring_start`](/home/folk/Programs/elivagar/crates/lib/src/multipolygon.rs:744) compares candidate rotations by rescanning their common prefixes. A linear minimum-rotation algorithm over the same quantized keys removes its quadratic worst case. Preserve its tie choice.
- **Cache ring sort keys and measurements.** Bounding boxes are recomputed inside sorting comparisons, and component area-centroid calculations repeat in aggregate centroid selection. These belong naturally in ring descriptors.
- **Remove the prepass's full superset set when its only product is a count.** It adds another potentially large hash set beside the required membership set. This is input-scaled bookkeeping rather than useful geometry work.
- **Avoid repeated geometry work across equivalent layer policies.** Sharing base quantization is straightforward; sharing complete pyramid results requires identical simplification, minimum-area, zoom, and cap behavior. Keep this specific to demonstrated duplicate work.

I would not center this effort on projection arithmetic. `project_e7` already uses a lookup table. Eliminating repeated decoding and projection is useful; replacing expensive transcendentals is not the opportunity here.

**Several current behaviors must be made explicit before rewriting.** These are observations from the code, not proposed policy changes.

- Broken relations are partially salvaged. Missing members and unclosed chains do not necessarily reject the whole relation.
- Orphan inner rings are promoted only when at least one outer exists. With no outers, `pair_rings` immediately returns empty.
- Shared-vertex pins count distinct member occurrences containing a coordinate, including members whose chains assembly later discards. Computing pins only from accepted rings would change simplification.
- Endpoint joining, pin identity, and tile geometry use three different precision domains: `1e9`, `1e12`, and the z14 integer grid. Collapsing them changes behavior.
- `ClosedRings::push` checks vertex count, not distinct-vertex count. Some degenerate rings survive assembly until later geometry processing. Tightening acceptance during this rewrite would need a separately stated behavior change.

There are two additional concerns beyond the requested scope. The shared [spill coalescer](/home/folk/Programs/elivagar/crates/lib/src/sort/write.rs:392) copies producer payloads under a mutex; enough new relation parallelism may expose that serialized memory traffic next. Its disk writes already happen outside the lock. Also, the current pbfhogg ordered pipeline has an [admission gate](/home/folk/Programs/elivagar/research/pbfhogg/src/read/pipeline.rs:106), despite Elivagar source commentary describing an older unbounded implementation. A reader redesign should be justified by removing serial work and unnecessary representation, rather than assuming that older queue pathology remains.
