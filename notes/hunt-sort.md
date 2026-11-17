I would replace the external merge sort with a memory-budgeted tile-bucket store. That is the strongest architectural opportunity in this scope. The current design orders records across producer batches, writes that order to disk, then reconstructs it through per-record merges. Assembly ultimately needs complete, deterministically ordered tiles. Making tiles the unit of ownership can remove substantial work between those two facts.

This is a code-based ranking, not a measured speedup claim. I read the scoped implementation and followed emission, assembly, geometry-pyramid and pbfhogg interfaces. I did not change files, build, test, inspect history or read project Markdown.

1. **Replace sorted runs and their merge with tile buckets. Full coherent rewrite; highest conviction.**

   The main producer path already uses arenas. Its remaining structural problem is what happens to them: `SpillCoalescer::append` copies every payload into a shared arena under one mutex, rebases every descriptor, then makes the producer that crosses the budget sort and write the accumulated buffer. Sorting and writing happen outside the lock, but the global copy remains serialized, and producers become spill workers. See [write.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/write.rs:392).

   The resulting runs are ordered by the packed key and then payload bytes. Assembly later opens all sources for a partition, maintains one current payload per source, chooses a heap winner for every record, and copies that payload into `RawBatch`. The packed integer comparison is already cheap; the repeated ordering, ownership transfers and payload movement are the larger targets. See [key.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:216) and [merge.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/merge.rs:116).

   I would replace this path end to end:

   - Producers emit into owned pages containing payload storage and compact descriptors. They transfer pages or page slices to bucket owners instead of copying everything into one coalescer.
   - Records are distributed by zoom and tile-ID prefix using radix or counting partitioning. They do not need full comparison sorting before spill.
   - Bucket storage retains resident pages while the shared memory budget permits it. Spill workers write substantial bucket blocks into a bounded number of extent files, with an explicit directory.
   - Once all contributing producers finish, a worker gathers a bucket, groups descriptors by tile, orders each tile's features, and encodes it directly.
   - Assembly receives complete tile jobs or borrowed tile views. It stops consuming a record-at-a-time merge iterator.

   This removes the need to establish order *between different tiles* by comparing every record. Tile order comes from enumerating occupied tile buckets. Inside each tile, grouping by layer and rank is cheap; payload ordering remains necessary within equal-key groups.

   The fixed z7 split should disappear as an ownership and scheduling rule. Today it creates 136,533 possible partitions using geography alone, while actual work depends on record density. Use prefix buckets for routing, then subdivide dense buckets using recorded byte and record distributions. Keep the directory sparse and active page buffers bounded. Creating a file or a substantial buffer for every tile would replace one bottleneck with another.

   The important sibling changes are concrete:

   **Emission should provide** an owned page plus descriptors such as `(tile_id, layer, rank, payload_handle)`. It should stop providing a borrowed arena that the store must copy before returning.

   **Assembly should request** complete tiles or bounded tile ranges with exact occupancy and size information. Workers should perform bucket loading, tile ordering and encoding as one task. The final PMTiles drain can remain ordered by Hilbert ID, including artifact-only ocean ranges, without requiring upstream records to be globally sorted.

   The plausible payoff comes from eliminating the global coalescer copy, comparison sorting across unrelated tiles, per-record heap maintenance, and the merge-to-batch payload copy. Resident data also avoids readback and decompression.

   There are real costs. Bucketing performs scatter operations, and poor page sizing can create fragmented reads. Buckets that outgrow memory need recursive repartitioning, which adds I/O passes. A single enormous tile cannot be divided into independently encoded tiles; it needs an explicit oversized-tile strategy. Loading whole buckets also requires budgeting descriptors, payloads and encoding scratch together. This rewrite needs a real memory-accounting design, not merely a different container.

   **Correctness need not require changed tile bytes.** Preserve the current order `(tile_id, layer, rank, legacy payload bytes)` inside each tile, then retain the existing assembly transformations. Bucket arrival order and worker scheduling become irrelevant.

2. **Replace partition-sized compressed streams with indexed, independently readable blocks. Substantial storage rewrite; high conviction, especially for compressed runs.**

   The strongest concrete read amplification is in hot-partition splitting.

   Before assembly workers start, `plan_pieces` scans each selected partition's sources to discover bucket boundaries. That scan reads and discards every payload. For plain chunks it still reads the payload bytes rather than seeking past them. See [schedule.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/schedule.rs:61).

   Compressed pieces are more expensive. Their source slices retain a record count to skip, rather than a compressed offset. Opening each piece restarts the section decoder and decompresses every preceding record. See [format.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/format.rs:514) and [merge.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/merge.rs:201).

   For a section evenly divided among `q` pieces, piece reads can process approximately `(q + 1) / 2` section-volumes, including useful reads and skipped prefixes, plus another complete section scan for planning. Actual amplification depends on how each source overlaps the pieces. The current merge-byte counter excludes scanned and skipped records, so it does not describe that total work.

   I would write a block directory while producing storage. Each block should carry compressed and uncompressed lengths, record count, tile bounds and enough indexing to locate tile-range cuts. Store uncompressed work estimates as well as physical extents. Assembly can then select work without scanning payloads and read a piece from its first relevant block, with at most bounded boundary-block decoding.

   This belongs in the bucket rewrite. It is also a coherent independent replacement of the existing chunk storage and read path if sorted runs remain.

   It addresses two additional costs:

   - Compressed multi-partition writing calls `stream_position()` before every section. With `BufWriter`, that forces pending output through the buffer. Every section also creates a fresh frame encoder. The nominal 1 MiB writer buffer therefore does not combine small compressed sections into large physical writes. See [format.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/format.rs:335).
   - Every source gets a 256 KiB `BufReader<File>` whose reads are not bounded by the section's physical extent. Small sections can fetch bytes belonging to adjacent sections. Reader memory also grows with concurrent merge fan-in. See [format.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/format.rs:804).

   Exact block-range reads through shared file handles would make read size and ownership explicit. Compression and decoding could run as block tasks instead of being inseparable from each partition's serial merge reader.

   Codec selection is secondary to this redesign. LZ4 or Snappy can save disk traffic while increasing CPU consumption on cores also needed for geometry and encoding. The current code does not establish which wins on the target machine. I would fix framing, indexing and repeated decoding before choosing a codec winner.

   Risks are smaller-block compression ratios, index overhead and excessive I/O concurrency. None requires changing tile contents.

3. **Separate record ordering from feature representation. Substantial representation rewrite; medium-to-high payoff confidence.**

   Every payload currently contains an eight-byte feature ID, geometry type, command count, four bytes per geometry command, and a complete encoded attribute list. Emission pre-encodes attributes by zoom, but copies those bytes into every resulting tile record. See [wire_format.rs](/home/folk/Programs/elivagar/crates/lib/src/wire_format.rs:315) and [emit.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:1090).

   After spill and merge, assembly allocates a geometry vector and copies the commands again. The layer builder then merges geometries, and MVT encoding packs the commands into protobuf varints. See [wire_format.rs](/home/folk/Programs/elivagar/crates/lib/src/wire_format.rs:424).

   I would give the bucket store a native feature representation:

   - A compact descriptor holds tile, layer, rank, feature ID, geometry range and attribute reference.
   - Command words live in owned command pages.
   - Attribute bundles are stored once within a bounded producer or bucket domain and referenced by their records.
   - Resident assembly borrows these structures. Geometry becomes owned where a transformation actually requires mutation.
   - Spilled blocks use a compact representation chosen for total encode, I/O and decode cost.

   Sharing attributes attacks a multiplicative cost. A bundle of `A` bytes copied into `F` records currently consumes `F * A` payload bytes before headers and geometry. The potential saving is particularly plausible for named lines and polygons with substantial tile fanout. The existing static dictionary already compresses `kind`; the remaining opportunity includes names, other attributes and repeated complete bundles.

   Geometry packing can also reduce spill volume, but it is less certain: compact commands introduce decode work where the current representation permits a bulk copy. I would keep resident commands native and judge spill packing separately.

   There is a subtle ordering constraint here. The current payload tie-break is lexicographic over serialized bytes. IDs are serialized **little-endian**, so numerical feature-ID order is different. Geometry counts and commands have similar byte-order considerations. Dictionary IDs and allocation handles are also unsuitable as tie-breaks.

   A new representation can preserve existing order with a comparator over the equivalent legacy byte sequence, without materializing that sequence for every record. Comparing IDs numerically or ordering by dictionary insertion would change tile bytes.

   Changing feature order has effects beyond presentation: attributes are interned in encounter order, and same-attribute geometry merging preserves the input order within groups and retains the first member as its destination. See [mvt/merge.rs](/home/folk/Programs/elivagar/crates/lib/src/mvt/merge.rs:88). I would preserve that order initially.

   The main risks are retaining large backing pages for a few surviving references, unbounded dictionaries and adding shared interning contention. Keep sharing local and lifetime-bounded. This requires changing the layer-builder ownership model as well as the sort payload.

4. **Carry worker ownership from PBF decode through emission into bucket storage. Architectural sibling change; worthwhile, but below the storage rewrite.**

   Locations-mode input currently crosses several execution domains: raw blobs, decode workers, a decoded-block channel, the router, node workers or way dispatch, and finally the shared spill coalescer. See [input.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/input.rs:371) and [phase12/mod.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/mod.rs:564).

   The vendored pbfhogg contains mechanisms that support a stronger path:

   - `for_each_fused_block` executes a transform on decode workers.
   - `BlockBuffer::project` gathers information during validation.
   - `parallel_classify_accumulate` and `parallel_project_accumulate` retain worker state across blobs and return accumulated results instead of shipping every decoded block through a consumer.

   These are private today, but API visibility is incidental. See [reader.rs](/home/folk/Programs/elivagar/research/pbfhogg/src/read/reader.rs:364) and [classify.rs](/home/folk/Programs/elivagar/research/pbfhogg/src/scan/classify.rs:1485).

   I would adapt these mechanisms so a worker can decode, match, construct geometry and publish owned bucket pages while retaining useful scratch. Only compact accounting, relation/index obligations and necessary dependency messages should leave that worker.

   This should be workload scheduling work, not simply moving expensive emission into the existing fixed decode pool. Workers blocked on spill or relation readiness must not prevent unrelated decode and geometry work from progressing.

   Raw inputs still have a real dependency: the node store must be ready before ways resolve coordinates. Relations still require their member geometry. Those dependencies should determine synchronization; decoded-block ownership need not determine it.

   One correction from reading the implementation: the vendored ordered reader already has admission control before decode-task submission. I would not justify replacing it by claiming its current implementation has an unbounded reorder window. Its remaining cost is the ownership and scheduling shape.

   This change can remove queues, block retention and repeated traversals, but it does not itself remove sort payload volume. That is why I rank it below tile ownership and storage.

5. **Move fanout behind spatial ownership. A second, higher-risk architectural rewrite.**

   After the bucket design works, the larger opportunity is to store fewer fully expanded tile records.

   Points currently become separate records at every emitted zoom. Lines and polygons become records after per-tile geometry generation. A spatial work store could instead retain a feature or canonical geometry fragment plus its coverage, then let bucket workers generate the tile records directly into tile-local storage.

   The polygon engine already exposes a useful continuation boundary: `emit_shape_pyramid_cell`, and ocean work already carries `(PyramidCell, Shapes)` tasks. See [pyramid.rs](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:189) and [ocean/emit.rs](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:707).

   This could eliminate expanded intermediate records rather than merely make them cheaper. It is plausibly the highest-upside follow-up for features whose descendant tile records greatly exceed their source representation.

   Confidence is lower because geometry policy must survive the move. Canonical grids, inherited fragments, simplification, pins, buffered clipping, fanout caps, ring-cap partitioning and antimeridian handling must remain identical. Independently clipping an original polygon in each destination bucket is not an equivalent substitute for the current pyramid.

   Reusing exact pyramid continuation state offers a credible route to equivalent geometry. This is a geometry-and-storage rewrite, not an API cleanup.

There are two constraints I would make explicit before counting the benefits.

**Tile completion needs evidence.** PBF order is not tile order, relations arrive later, and ocean contributes afterwards. Neither the existing blob metadata nor approximate spatial locality proves that a tile is complete. A bucket store can ingest, partition, spill and prepare work concurrently, but final tile encoding needs all contributing producers to finish or an exact dependency/watermark scheme. I would not invent early completion from source order.

**No-spill execution and today's durable resume behavior are different promises.** Producers drain their residual arenas, and the pipeline persists producer completion before assembly. See [phase12/mod.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/mod.rs:669) and [pipeline/mod.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/mod.rs:847).

A resident bucket can be consumed directly while also being persisted for resume. That removes readback, but retains writes. Avoiding those writes requires replay-backed checkpoints or another deliberate change to resume behavior. A checkpoint could explicitly mark buckets as reconstructible from identity-checked inputs; a later resume would then perform producer work. That tradeoff must be stated. Complete reusable producer state does not become durable merely because it fits in RAM.

The medium-value local changes are narrower:

- **Improve the remaining merge algorithm.** A loser tree, cached comparison prefixes, winner-run draining and a direct single-source path can reduce comparisons and payload touches. These retain the fundamental record-by-record merge and all spill traffic.
- **Stop rescanning plain payloads to locate cuts.** Write cut information during serialization; until then, plain-source scanning can seek past payloads.
- **Bound section reads and release exhausted source storage.** Merge sources currently retain their readers and payload high-water buffers until the partition reader drops.
- **Compact arena descriptors together.** Both offset and length can use page-local `u32` fields, making a descriptor 16 bytes rather than 24 when pages are bounded below 4 GiB. Changing only length would not achieve that.
- **Remove the node path's remaining boxes.** Node emission still creates boxed `SortRecord` payloads and immediately copies them into its arena. It can target an arena sink directly. See [nodes.rs](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/nodes.rs:262).
- **Evaluate radix sorting if sorted runs remain.** The primary key is already a packed integer and payload sorting is restricted to ties. Radix sorting could help large batches, but it preserves both ordering passes and the disk seam.

I would not prioritize a faster Hilbert implementation, smaller integer fields in the packed key, another buffer-size adjustment or checkpoint metadata writes ahead of the architectural work.

Two correctness findings also deserve attention:

- The chunk serializer narrows payload lengths and record counts with unchecked `as u32` casts. Geometry command count is checked elsewhere, but complete payload length and batch count are not checked here. Oversized inputs can therefore produce an incorrect wire length or count. See [format.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/format.rs:305).
- Chunk readers validate tile-range membership but do not validate monotonic record order. An in-range corruption can change layer/rank or payload ordering without necessarily causing the archive writer's tile-order check to fail. This is a persisted-data validation gap, not evidence that normal producers emit unsorted records. See [format.rs](/home/folk/Programs/elivagar/crates/lib/src/sort/format.rs:837).
