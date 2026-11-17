I would rewrite phase12 around dependencies and independently owned output segments. The largest opportunities are eliminating repeated input work, removing the raw node store's single owner, and removing the way index's single writer and global finalization barrier. Fusing decode with processing supports those changes, but deleting channels alone is unlikely to deliver the biggest gain.

These conclusions come from source inspection only. I did not build, test, benchmark, edit files, read project Markdown, or consult history. The relevant ordered-reader implementation in the installed pbfhogg 0.6.0 matches the vendored source.

**What the current code actually serializes**

The two input paths differ substantially:

- Locations mode already delivers decoded blocks unordered and uses multiple node workers.
- Raw mode receives blocks in file order and sends all nodes through one store-owning worker.
- Way-plan construction already runs inside parallel way tasks. The dispatcher only receives, admits and launches blocks.
- Feature records already bypass the drain. The drain serializes way-index writes and merges totals.
- Every run starts a separate relation prepass. The first way block waits for it.
- Relations run only after every way task finishes and the way index completes its offset sort and mapping.

See [phase12 routing](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/mod.rs:226), [way dispatch](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:288), and [way drain](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:493).

Two premises therefore need correcting. There is no remaining serial way-plan builder to parallelize, and current pbfhogg does not have an unbounded ordered decode window. Its admission permit survives through reordering and consumption. Elivagar's explanation for its custom unordered source is stale on that latter point. The remaining ordered-reader costs are bounded retention, head-of-line blocking and extra handoffs, rather than runaway admission. See [decode admission and delivery](/home/folk/Programs/elivagar/research/pbfhogg/src/read/pipeline.rs:551).

My priorities would be:

| Change | Assessment |
|---|---|
| Replace repeated relation discovery with one reusable relation plan | Highest conviction removal of redundant work |
| Build raw node storage in parallel segments | Highest conviction removal of a one-core ceiling |
| Publish member-way geometry without the serial drain and global offset sort | Strong architectural opportunity |
| Fuse decode, planning and processing under one scheduler | Strong execution model; wall-time payoff depends on the work above |
| Reduce repeated traversal and unused planning work | Worth doing, but secondary |

**1. Replace the prepass/buffer/replay arrangement with one relation-planning operation**

This is a full input and dependency rewrite.

The prepass uses `BlobFilter::only_relations()`, but the filter runs inside decode tasks, after `BlobReader` has read and allocated the entire blob body. Consequently:

- Indexed input still incurs a complete additional compressed-body read. The filter saves decompression of indexed non-relation blobs.
- Unindexed input incurs another decompression and block construction for every data blob.
- The main pass decodes relations again.
- Once the relation buffer cap trips, it discards its decoded prefix and later decodes relations again through replay. Raw unindexed replay takes the filtered reader path and again visits every data blob.

These are logical reads; page cache may absorb some physical I/O. It cannot eliminate the extra allocation, envelope parsing and, on unindexed input, decompression and validation. The separate background provenance hash adds another full input read.

The relevant chain is [prepass construction](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/prepass.rs:79), [blob-body reading](/home/folk/Programs/elivagar/research/pbfhogg/src/read/blob.rs:843), [filter placement](/home/folk/Programs/elivagar/research/pbfhogg/src/read/pipeline.rs:488), and [relation buffering](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/mod.rs:105).

The stronger design is to parse relations once into an owned representation containing:

- The matched attributes currently recomputed in the tail.
- Relation ID and ordered member IDs, roles and original member positions.
- The member-way requirements.
- Boundary metadata contributions.

Keep or spill that compact representation, rather than retaining whole decompressed PBF blocks and later recovering them from the source.

For indexed input, discover frame descriptors and schedule relation bodies first, alongside independent node work. Once relation planning completes, way work has its dependencies. Process node and way bodies without revisiting relation bodies. pbfhogg already contains much of the required machinery: [header-only walking](/home/folk/Programs/elivagar/research/pbfhogg/src/read/header_walker.rs:1), [split kind schedules](/home/folk/Programs/elivagar/research/pbfhogg/src/scan/classify.rs:701), and [worker-local positional decode](/home/folk/Programs/elivagar/research/pbfhogg/src/scan/classify.rs:998). These are useful implementation ingredients, not APIs the redesign needs to preserve.

The prepass's reductions can also run in parallel. Needed-way membership is a union; boundary metadata combines minimum admin level and disputed-parent OR. Neither requires relation file order. Sharding those reductions by way ID avoids replacing the existing serial scan with one giant serial hash-map merge.

The payoff is unusually credible because it removes complete passes and repeated interpretation, not just synchronization overhead. On unindexed input, the current prepass duplicates work across the entire extract.

There is, however, a real unindexed-input problem. Frame headers cannot reveal information they do not contain. To eliminate the prepass while reading and decoding each body once, the pipeline must retain way information until later relations identify its uses. In particular, injected membership alone is insufficient: boundary metadata applies even to relations outside the multipolygon/type gate, and can make an otherwise untagged way emit a boundary.

A coherent single-scan replacement would emit ordinary way features independently while retaining an external, partitioned way representation for later membership and boundary joins. That is a large rewrite and can increase intermediate geometry volume substantially. I would benchmark that complete design against the relation-first indexed design, rather than assume literal single-pass input must always win. Replacing duplicate decompression with a larger external join is a trade, not a free optimization.

Correctness risks are member-order changes, missing-way handling, boundary metadata arriving too late, and losing original block-local pin semantics. The relation representation must preserve those explicitly.

**2. Remove the raw node store's single-owner requirement**

For raw extracts, this is the clearest one-core throughput ceiling.

One worker currently performs coordinate insertion, compact-store construction, extent calculation, tag collection, matching and node emission. The ordered source exists in large part to feed that mutable owner. At the first way block, routing joins the worker and converts the store into a reader. See [node-worker ownership](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/nodes.rs:62) and [way-phase transition](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/mod.rs:309).

The compact node store does have a genuine sequential builder today: each insertion advances mutable chunk and group state, compressing completed chunks and copying completed groups. That is an implementation dependency, not an output requirement. See [sorted-store insertion](/home/folk/Programs/elivagar/crates/lib/src/node_index.rs:882).

I would replace it with parallel construction of immutable ID-range segments:

1. Decode node blocks and emit tagged-node features on workers.
2. Build compressed chunks and groups independently.
3. Publish completed groups directly into the final directory.
4. Combine only fragments that cross block boundaries.
5. Validate ordering using intra-block checks and adjacent blocks' first/last IDs in original file order.

The coordination work becomes proportional to blocks and boundary fragments, rather than every node. Existing 256-node chunks and 64K-ID groups provide natural ownership units. Parallel construction need not change coordinate lookup semantics or the compressed reader's logical contents.

The sibling resolver scope should provide a builder that accepts independently completed chunks or groups and produces an immutable reader. Wrapping the existing `put` API in a mutex would preserve the bottleneck.

The main risks are groups split across blocks, preserving ordering rejection, and transient duplication during publication. The unsafe flat backend also needs an explicit policy for conflicting writes; blindly allowing unordered writes would change its current file-order overwrite behavior.

This rewrite should have materially greater raw-path payoff than tuning decode depth. More decode workers cannot outrun the worker that must touch every node.

**3. Replace the serial way-index drain with parallel publication, then remove the global relation barrier**

The drain still performs substantial per-coordinate work. Way tasks clone member coordinates into `way_puts`; the drain delta-varint encodes every stored member way and appends both geometry and offset entries. After every task completes, `finish_writing` serially sorts the offsets, writes another file, maps the files and populates them before relations begin.

See [coordinate handoff](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:607), [index encoding](/home/folk/Programs/elivagar/crates/lib/src/way_index.rs:449), [offset sorting](/home/folk/Programs/elivagar/crates/lib/src/way_index.rs:324), and [finalization](/home/folk/Programs/elivagar/crates/lib/src/way_index.rs:476).

I would move member-way encoding and storage publication into the producing tasks. Each task produces encoded geometry plus a local ID-to-offset segment. A partitioned directory locates those segments without funneling all coordinates through one thread or sorting one global unsorted offset file.

That requires a different way-index implementation. It should use buffered extents or bounded shard writers; creating a tiny file for every task would exchange serialization for fragmentation. Totals can reduce independently.

The stronger end-to-end version also replaces the global relation tail dependency. The relation plan knows which member IDs each relation needs. As member geometry becomes immutable and available, relations whose complete dependency sets are present can run. Only relations with unresolved members must wait for the way-input seal to establish absence.

This removes three artificial waits:

- Finished member geometry waiting inside a block-sized result.
- Every relation waiting for unrelated way tasks.
- Every relation waiting for global offset sorting and map population.

The relation sibling should accept ordered member descriptors backed by immutable geometry handles, rather than require a fully finalized global index. Member order must remain the original relation order, regardless of publication order.

Payoff scales with relation membership volume and the size of the current finalization/tail gap. This is a strong rewrite because encoding, coordinate ownership transfer and index construction all disappear from the serial drain together.

Risks include readiness accounting, large relations retaining geometry, missing-member completion and scheduling relation work against ongoing way work. These require one bounded dependency scheduler, not workers blocking while waiting for other workers to publish geometry.

**4. Fuse decode and block processing instead of passing decoded blocks between fixed pools**

This is the execution rewrite that makes the other changes fit together.

Locations input currently crosses a raw channel, a decode pool, a decoded channel, the router, a one-slot way channel, a dispatcher and a Rayon task before processing. Decode uses `threads / 3`; node emission has a separate pool capped at six workers; way processing uses the global Rayon pool. The prepass has another decoder pool.

The router does little computation, so removing it alone is not a convincing major speedup. The structural cost is that decoded blocks cross ownership boundaries and wait while independently sized pools cannot lend capacity to each other. A blocked node or way send also stops routing unrelated block kinds.

See [custom source](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/input.rs:371), [resource allocation](/home/folk/Programs/elivagar/crates/lib/src/resources.rs:211), and [way channels](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:115).

The replacement should queue compressed work or frame descriptors. A worker decompresses, validates, builds the block's necessary plan, processes it and publishes its outputs before releasing the block. Ready work shares execution capacity; dependencies remain pending work items rather than parked workers.

Admission should follow retained ownership through completion. Today a way ticket covers decompressed block size plus estimated plan bytes, and releases after the result send. It does not cover queued `way_puts`, emission scratch or full coalescer buffers being written. A bounded channel count is not the same thing as a useful live-memory budget.

pbfhogg already demonstrates the important mechanisms:

- [Fused transforms on decode workers](/home/folk/Programs/elivagar/research/pbfhogg/src/read/reader.rs:364).
- [Worker-resident bounded decoding and reduction](/home/folk/Programs/elivagar/research/pbfhogg/src/read/reader.rs:623).
- [Validated projections](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:1135).

Its fused ordered reader would still serialize consumption and reorder results. Its element-level `par_map_reduce` lacks the block context needed for current pin planning. Exposing either unchanged would leave important constraints intact. Build the block-execution shape phase12 needs; afterwards, if this works, generalizing the reader machinery is a separate follow-up.

Determinism is compatible with this design. The external sort already establishes a total order by key and payload, independent of producer scheduling and chunk layout. See [record ordering](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:147). File-order execution is unnecessary for ordinary feature emission. Original block membership, relation-member order and coordinate semantics still matter.

**Medium-value changes that should not be mistaken for the architectural answer**

There is worthwhile redundant work inside each way block.

Construction validates the way arrays. Injected-member validation then walks elements to count ways; pin validation walks them again and counts references. Plan construction walks them again, collects every reference list and traverses inline locations for extent. Processing reparses the ways and traverses locations again. Inline processing also clones the planned references.

See [decode-time enrichment validation](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/input.rs:511), [plan construction](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/plans.rs:67), and [way processing](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:530).

A better block representation would carry validation-derived counts, borrowed way descriptors and compact arrays actually needed by processing. Injected pins plus inline coordinates do not need an owned reference vector for every way. Block-local pins do need cross-way reference analysis, but can use one flat array with offsets and produce position masks directly.

pbfhogg's validator already records actual element kinds and total element count internally, and supports projection hooks. Extend that machinery to produce the needed facts during validation. Do not substitute header claims for decoded facts, or commit projection side effects before the whole block validates.

Other useful local changes are:

- Use `elements_skip_metadata` for node processing. Current iteration advances dense-node metadata that Elivagar does not consume.
- Avoid constructing `needed_ways` when injected membership makes the returned set unused.
- Remove the full `superset_ways` hash set from production planning if its diagnostic count is not required.

These reduce CPU and memory, but they do not remove the raw node owner or the index drain.

**The next structural blocker outside this scope**

Every producer still copies its payload into one `SpillCoalescer` buffer under one mutex. Sorting and writing happen outside the lock, but all record bytes pass through its serialized copy. A task crossing the threshold also performs the full chunk sort and write before returning. See [coalescer append](/home/folk/Programs/elivagar/crates/lib/src/sort/write.rs:392).

The emission/sort sibling should provide ownership transfer of payload slabs, with bounded coalescing and large output chunks. Simply having each producer write small chunks independently risks expensive merge fragmentation. The useful redesign removes the central copy while retaining large-chunk behavior.

Two correctness issues also deserve attention:

- The way index has no duplicate-ID policy. Repeated way IDs produce equal offset keys, and lookup chooses an arbitrary equal entry. With unordered append order, conflicting duplicates can make relation geometry scheduling-dependent. Reject duplicates or define a source-order winner.
- `same_file` compares the replay handle with the pathname after the main reader opened it, rather than comparing both open handles. Together with separately opened header, prepass and hash readers, concurrent pathname replacement can mix input identities. This is outside normal immutable-input operation, but the current check does not fully establish what its comment claims.

I would pursue the relation-input rewrite and parallel ownership changes before channel tuning. Fusion then becomes a coherent replacement for the actor pipeline, with real work removed and dependencies narrowed, rather than the same bottlenecks connected by fewer queues.
