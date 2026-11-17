**I would rewrite assemble's scheduling and ownership model.** The strongest opportunity is to make independently seekable tile ranges the unit of work, with one owner carrying each tile from merge through encoding. Delete the raw-batch handoff and per-partition encoder threads. Keep deterministic ordering in a small output planner, and separate that planner from payload hashing and disk writes.

The current design exposes parallelism, but repeatedly turns it back into ordered waiting: partition claims, serial merge readers, complete raw batches, complete parallel encode batches, then ordered partition draining.

These are code-derived hypotheses, not measured speedups. I only read source code.

1. **Highest-conviction architectural rewrite: replace reader/encoder pairs with a tile-range execution engine.**

   Each partition worker runs one serial merge reader. It fills a `RawBatch`, sends that batch to a newly spawned encoder thread, and continues reading through a three-batch recycling pool. The encoder launches parallel tile work but waits for the entire batch before publishing anything. See [read_encode_partition](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/schedule.rs:703) and [encode_tile_batch_mvt](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/encode.rs:106).

   This creates three structural costs:

   - Merge concurrency is tied to the partition-worker count, capped at eight by default, independently of the machine's available encode parallelism.
   - Every record crosses an ownership seam. The merge copies its source buffer into the raw slab; encoding later reads that slab and constructs owned geometry and attributes.
   - Batch completion is a publication barrier. One expensive tile delays every completed tile in its batch, including an otherwise writable prefix.

   The slab is already substantially better than allocating every record separately. The remaining opportunity is **removing the materialization**, not improving its allocator.

   I would use a persistent worker pool over cost-balanced, contiguous tile ranges. A worker owns its merge cursors, consumes records directly into the current tile's builders, finishes that tile, encodes and compresses it, and publishes the result. Source payloads need only remain borrowed until the builder consumes them. The merge already owns reusable record-head buffers; [next_into](/home/folk/Programs/elivagar/crates/lib/src/sort/merge.rs:116) currently copies those buffers into the batch. A borrowed consume-before-advance operation can remove that copy.

   Completed tiles should leave through small output chunks with explicit range-completion markers. Those chunks amortize messaging; they do not require all tiles in a large encode batch to finish together. Empty tiles must still advance completion, because they determine where artifact fallback applies.

   This removes:

   - `RawBatch` slabs and feature-reference tables from the normal path.
   - Three circulating raw batches per reader.
   - Reader-to-encoder ownership transfers.
   - Encoder thread creation and joining for every sort partition.
   - The whole-batch Rayon collection barrier.

   For scale, the existing defaults permit 24 circulating raw batches across eight readers, nominally around 768 MiB of batch budgets before merge buffers, encoding state and completed output. That is an illustration of the topology, not a measured resident-memory total.

   **Why this could materially improve throughput:** it removes a full raw-payload copy, shortens tile lifetimes, increases merge parallelism where useful, and replaces batch stragglers with smaller scheduling units. It also improves locality: the core reading a feature can immediately consume it.

   **Main risks:** simply increasing concurrent cursors can multiply file descriptors, decompression state and source buffers. Each current source reader has a 256 KiB buffered reader. The rewrite therefore needs bounded block residency and shared file access, not just more instances of today's readers. A single enormous tile remains indivisible at the tile level; independent layer preparation is the encoder-side escape hatch if those tiles dominate. Geometry merging within a layer still needs its complete input.

2. **An essential part of that rewrite: make sort output independently addressable without an assemble prescan.**

   The current hot-partition splitter pays for parallelism before useful assembly starts.

   [plan_pieces](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/schedule.rs:138) scans every source of a hot partition to discover bucket boundaries. The outer planning loop completes all those plans before starting assembly workers. Its supposedly discarded payloads still pass through `io::copy` into a sink, so even uncompressed sources are read through in full. See [next_header_discard_payload](/home/folk/Programs/elivagar/crates/lib/src/sort/format.rs:899).

   Compressed sources have a worse consequence: a piece retains the original frame start and a record count to skip. Opening each piece decompresses and discards its prefix again. For a source spanning \(P\) roughly equal pieces, prefix replay can approach \((P-1)/2\) additional source traversals, besides the initial planning scan and useful traversal. The exact cost depends on the source's distribution across pieces. See [SourceSeek::slice](/home/folk/Programs/elivagar/crates/lib/src/sort/format.rs:514) and [open_slices](/home/folk/Programs/elivagar/crates/lib/src/sort/merge.rs:201).

   **The interface I want from the sort sibling:** immutable sorted blocks with first/last keys, record counts, uncompressed byte counts, and independently seekable offsets. Compressed blocks need independent restart points. Boundaries should respect tiles, with explicit handling for a tile larger than the target block size.

   Sort already traverses sorted records when writing them. That is where this indexing belongs. Assemble should choose task boundaries from metadata and immediately start merging useful ranges.

   Tasks should be balanced using record count and uncompressed volume, with geometry complexity estimates where cheaply available. Compressed source size alone is a weak estimate of encode work. Fixed Hilbert buckets and a 32-piece ceiling also cannot provide a reliable work-size bound.

   **Why high payoff:** this removes startup work on precisely the largest partitions and prevents repeated decompression from becoming the price of parallelism.

   **Risks:** smaller frames trade compression ratio and metadata size for seekability. Excessively fine tasks repeatedly initialize merge heaps and source cursors. Use indexed blocks to support moderately sized tasks; do not reopen every source for every tile.

   One important boundary finding: there is no separately materialized final merged stream to eliminate here. The orchestrator's sort phase flushes and opens sources; the actual merge already happens during assemble. The valuable fusion is **merge consumption with tile construction**, not merely renaming the sort/assemble boundary.

3. **The output side needs a coherent rewrite too: deterministic placement should not own all payload work.**

   The writer thread currently receives batches, updates two levels of `BTreeMap`, drains them in partition/batch order, hashes payloads, performs deduplication, writes unique bytes and updates directory state.

   Every computed payload is scanned twice by `DefaultHasher` in [dedup_hashes](/home/folk/Programs/elivagar/crates/lib/src/archive/writer.rs:313). Those scans happen synchronously inside `add_tile` or `add_run`, even after the dedup insertion cap has been reached. Unique data then passes through the same thread's buffered file writer.

   Determinism requires stable decisions about payload identity, first occurrence and layout. It does **not** require the thread making those decisions to hash and copy every byte.

   I would have workers publish descriptors containing:

   - Tile ID or addressed run.
   - Payload length and precomputed fingerprints.
   - Owned compressed storage, or an artifact payload identity.
   - Completion information sufficient to prove the next ordered prefix.

   A small planner consumes that prefix, resolves deduplication, assigns deterministic final offsets, and emits directory descriptors. Separate bounded I/O workers write payloads at those offsets. Physical writes may complete out of order without affecting archive bytes.

   This can retain Hilbert clustering and today's first-seen dedup policy. There is no need to introduce completion-order-dependent layout, an intermediate compressed archive, or an extra full-data copy.

   The vendored dependency already contains the relevant mechanism: pbfhogg's [parallel writer](/home/folk/Programs/elivagar/research/pbfhogg/src/write/parallel_writer.rs:252) assigns offsets in ordered delivery and dispatches positional writes and source-range copies. Its implementation is concrete evidence that ordered placement and parallel byte movement can be separated. Its round-robin worker queues are not necessarily the best dispatch policy for tiles of uneven sizes.

   **Why plausibly high payoff:** it removes two full payload scans and bulk byte movement from the serial critical path. As tile production gets faster, leaving these there imposes an increasingly hard ceiling.

   **Risks and limits:** offset assignment still waits for preceding lengths and dedup decisions. This design reduces the work behind that ordering requirement; it does not abolish it. Actual disk-write parallelism may add little if the existing buffered writer already saturates storage. Hash offloading is more certain mechanically than its wall-time benefit.

   Moving the existing hashes to encode workers is a worthwhile **local change** on its own. Splitting placement, directory construction and byte writes is the full architectural change. Fully concurrent deduplication is less compelling initially: canonical ownership must remain independent of which worker arrives first.

4. **There is a concrete memory-bound defect to fix as part of the redesign.**

   The result channel is unbounded. `parked_bytes` is charged only after the writer receives a batch, and subtracted before that batch's items are actually written. See [channel creation](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/schedule.rs:338), [charging](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/schedule.rs:529) and [draining](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/schedule.rs:559).

   Consequently, the budget excludes:

   - Completed batches queued in the channel.
   - The batch currently being drained.
   - Unused capacity in payload vectors.
   - Run-copy descriptors and their containing vectors.

   While the writer spends time hashing or writing, producers can finish batches and claim additional partitions using an unchanged parking total. The advertised parking budget therefore does not bound all completed output. Admission also happens only at partition boundaries, so already admitted work can produce substantial additional output.

   Replace this with credits attached to storage ownership. Reserve before admitting work, adjust for actual allocated capacity, and retain the charge through channels, reordering and asynchronous writes until storage is released. Bound descriptor counts as well as payload bytes. Oversized tiles need an explicit exclusive allowance.

   **Progress must be designed with the bound:** later work must never consume all credits needed to complete the earliest unfinished range. A bounded shared channel alone is insufficient if later results can occupy its capacity and prevent the head from publishing.

   Pbfhogg's [read pipeline](/home/folk/Programs/elivagar/research/pbfhogg/src/read/pipeline.rs:144) already carries a permit with decoded results until consumption. Its [ordered streamed execution](/home/folk/Programs/elivagar/research/pbfhogg/src/ordered_pool.rs:1218) provides per-task channels and terminal messages. Both mechanisms are useful here, although their count bounds need adaptation to bytes.

   These are currently internal implementations. I would adapt the mechanism needed by this rewrite. Afterwards, if this works, extracting a shared execution primitive could be useful.

5. **Artifact handling deserves a smaller, focused rewrite: streaming interval join plus payload identities.**

   The current implementation already makes the most important choice correctly: artifact-only coverage stays as runs. It intersects ownership ranges rather than expanding long runs tile by tile, and borrowed compressed bytes remain in the mapped artifact until needed.

   The remaining waste is repeated rediscovery and content processing.

   [plan_artifact_items](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/artifact.rs:269) invokes gap planning between successive surviving encoded tiles. Each call rediscovers intersecting source runs and ownership ranges. Surviving tiles separately look up their splice run. Run copies eventually lose their source identity at `add_run`, where the same source blob can be hashed again for every fragment.

   I would give each tile-range task a monotone cursor over artifact runs already intersected with ownership intervals. Merge that stream with computed tile outcomes:

   - Surviving computed tile: emit the combined payload.
   - Empty computed tile: leave artifact coverage intact.
   - Uncovered interval: emit a run referencing the artifact payload identity.

   The output planner should translate `(artifact, BlobRef)` to an output payload handle once. Subsequent fragments reuse the handle directly, including after the ordinary dedup map reaches its cap. The first encounter can still participate in content deduplication against computed payloads.

   This removes repeated range searches, intermediate run-copy vectors, repeated hashing of known-identical source blobs, and scheduling jobs whose only purpose is to package artifact-only ranges.

   **Payoff:** medium overall, potentially substantial for artifact-heavy archives with many fragmented runs. This is not the leading hypothesis for a compute-heavy country build.

   The correctness constraint is crucial: **OSM survival must be decided before artifact fallback.** An input tile that encodes to nothing must not punch a hole in artifact coverage. The existing code handles this correctly.

   Repeated gunzip and envelope parsing for spliced tiles is a secondary opportunity. A bounded task-local cache keyed by blob could help, but source inspection alone does not establish enough expensive reuse to rank it highly. Changing gzip composition to avoid recompression belongs to the encoding sibling and requires consumer compatibility evidence.

A few additional findings matter:

- **Worker panics can strand the current scheduler.** Normal error paths reach stop-and-notify, but a worker panic has no equivalent guard. If the writer waits for that worker's missing batch while other workers retain senders and sleep on the claim condition, the channel need not close. The nested encoder join can propagate such a panic. Pbfhogg's ordered pools install unwind guards specifically around gate closure.
- **Deduplication is probabilistic equality.** Matching length and two hashes is accepted without byte comparison. That is a correctness limitation under a literal exact-content contract, although I found no collision artifact. Moving hashes off-thread preserves this limitation rather than resolving it.
- **There is an existing TileId writer API that avoids a needless round trip.** [write_encoded](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/report.rs:26) converts the ID to coordinates, then `add_tile` converts coordinates back to an ID. `add_run(tile_id, 1, ...)` already accepts the ID directly. This is a local improvement, not an architectural priority.
- `RawBatch::recycle` trims only byte storage. Feature and tile-vector capacities survive unusually large batches. Trimming those is another local memory improvement if the current batching survives temporarily.

My priority is the combined tile-range rewrite: indexed source blocks, merge-to-builder ownership, streaming completion, and storage credits that survive every handoff. Follow it with descriptor-based deterministic placement and separate payload writes. Artifact identity reuse fits naturally into that output model.

Those changes can preserve existing tile payload bytes and feature order. Preserve the current key-plus-payload record ordering, builder policies, layer order, compression settings and survival decisions. Changes to dedup retention or archive placement may change archive bytes relative to today's implementation, while remaining deterministic across repeated builds and leaving decoded tile contents unchanged.
