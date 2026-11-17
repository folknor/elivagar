**I would rewrite the coordinate handoffs, especially the way store.** The largest opportunities are to remove the serial geometry writer and reader, make inline coordinates a direct decode path, and replace raw-node scalar lookups with a representation and access pattern designed together.

These are source-based judgments. I did not build, test, benchmark, inspect history, or read project documents. Confidence below means confidence in the structural opportunity, not a measured speedup.

1. **Replace the way index with a relation dependency plan and shared geometry spans. Full architectural rewrite; highest priority across both input shapes.**

   The current lifetime of a relation-member geometry is expensive:

   - Resolve coordinates into a new E7 vector.
   - Clone that vector into `way_puts`.
   - Send it to one drain thread.
   - Delta-varint encode it and append data plus a 16-byte offset entry.
   - Sort the offset entries after all ways finish.
   - Map and populate both files.
   - For each relation-member occurrence, binary-search the offsets, allocate and decode another E7 vector, then allocate and project a Mercator vector.

   These operations are explicit in [way processing](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:570), [the writer and finalization](/home/folk/Programs/elivagar/crates/lib/src/way_index.rs:449), and [relation preparation](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:61).

   The reader is particularly consequential: relation preparation happens inside the iterator feeding `par_bridge`, before parallel processing. Consequently, searches, coordinate decoding, allocation and projection run under the bridge's producer serialization. Mapping the files does not make that work parallel.

   **The stronger design starts with information Elivagar already reads.** The [relation prepass](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/prepass.rs:80) visits relations and their members, then retains a set of needed way IDs and boundary metadata. Retain the actual dependency plan instead: matched relation descriptors, member roles and occurrence order, dense handles for distinct required ways, and the inverse mapping from each way handle to its consuming relations.

   Way workers would resolve a member geometry once and publish an immutable span under its dense handle. Relations would hold handles and roles, rather than rediscover geometry through way-ID searches. Publishing a span would satisfy dependencies; ready relations could enter assembly immediately. At end of input, unresolved handles would become terminal missing results.

   Geometry storage should belong to the producing workers, using immutable slabs with bounded resident bytes and sequential spill storage. The shared directory publishes span descriptors, not per-way coordinate vectors for another thread to encode. Cold slabs can be compressed losslessly at a coarser granularity. Compression and spill remain parallel producer work.

   This removes the centralized writer, the offset sort, the mandatory finalization barrier, and way-ID searches during relation consumption. It also creates a route to release geometry after its last consumer finishes.

   The neighbouring assembler should accept **oriented spans**. Its current API requires owned `Vec<Point>` members, and joining then copies and sometimes reverses those points again. Construct chains from span references and directions, then materialize the final ring once. The copies are visible in [way joining](/home/folk/Programs/elivagar/crates/lib/src/multipolygon.rs:451). Merely replacing the index while retaining owned member vectors leaves substantial work behind.

   **Why high payoff:** this removes a complete write-sort-read representation cycle and two serial ownership points. Shared ways also stop requiring independent decoded member vectors for every relation occurrence.

   **Risks:** dependency storage and geometry retention must be explicitly bounded. A giant relation can keep early geometry alive until late in the input; unrestricted shared allocations would trade disk work for excessive RSS. Spilling must allow producers to continue even when unresolved consumers retain earlier slabs. Preserve member occurrence order, duplicate occurrences, role handling and missing-reference accounting. Published geometry must remain in its original wrapped coordinates; an individual consumer must not unwrap shared storage in place.

   This rewrite can preserve tile bytes. Geometry representation and scheduling can change while retaining the current projection, joining decisions and deterministic output ordering.

2. **Make locations-on-ways a fused coordinate decode path. Full decoder-to-consumer rewrite; high conviction.**

   Locations mode already selects `NodeStoreMode::None`. There is no hidden node lookup to eliminate there. The remaining waste is around the inline coordinates.

   [Plan construction](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/plans.rs:75) currently materializes every way's references and decodes every inline location to compute extent. Later, admitted ways decode their locations again and clone their reference vectors into `resolved_node_refs`. With injected pins, those reference values are unnecessary for coordinate resolution and pin placement.

   There are additional walks before this: injected-pin validation counts refs, and injected-membership validation walks ways to count them. Meanwhile, pbfhogg's geometry validation already counts the refs and location arrays.

   **The stronger design is an Elivagar-shaped validated block result:** way descriptors containing ID, tags, membership, validated counts, pin information, and geometry spans. Decode inline coordinates once, accumulate extent during that decode, and deliver retained geometry directly to its consumers.

   For input declaring injected pins, keep reference counts without materializing reference values. For locations input without injected pins, decode refs once for the existing block-local sharing calculation and produce positional pin masks directly. The absence of injected pins is a real semantic distinction; locations alone does not eliminate all reference work.

   Pbfhogg already contains useful foundations:

   - [`BlockBuffer::project` and `GroupProjection`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:1135) attach projections to validation.
   - [`WaySpans`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/project_ids.rs:60) records array positions without copying their contents.
   - [Fused reader transforms](/home/folk/Programs/elivagar/research/pbfhogg/src/read/reader.rs:364) perform work on decode workers.

   These are internal facilities, so using them requires changing pbfhogg's boundary. That is worthwhile here. Extend the validation result to retain counts and useful spans, and integrate coordinate decoding where it replaces subsequent passes. Generalizing that interface can happen afterwards, if this works.

   **Extent must stay comprehensive.** It includes coordinates from ways that emit no features, because locations inputs can omit untagged node elements. Decode those coordinates into the extent reduction without retaining their geometry. Preserve both ordinary and shifted longitude extrema; a conventional bbox alone cannot reproduce [the current dateline decision](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/extent.rs:58).

   **Why high payoff:** this removes repeated work proportional to total reference volume, including reference allocation and copying on the input shape intended to avoid resolution work.

   **Risks:** retain full malformed-array detection rather than making filtering silently skip validation. Missing location arrays must still produce the current missing-reference outcome. Block-local pins must count occurrences across the same ways as today, including ways later rejected by feature matching.

   This can also preserve tile bytes exactly.

3. **Rewrite raw-node construction and resolution together: parallel immutable storage, direct point access, and block-level gathers. Full architectural rewrite; high conviction.**

   Raw input currently funnels node work through one worker. That worker handles coordinate storage, extent tracking and tagged-node emission. The sorted store's mutable builder makes coordinate compression serial even though completed node ranges are independent.

   Reading the store then requires a dependent sequence: group lookup, presence/rank calculation, cache search, variable-length chunk navigation on a miss, full chunk decompression, and another presence/rank calculation. [The lookup implementation](/home/folk/Programs/elivagar/crates/lib/src/node_index.rs:737) exposes the chain directly.

   The important observation is that **the FOR encoding does not inherently require full-chunk decoding**. Latitude and longitude are independently bitpacked offsets from minima. Once the coordinate rank and chunk count are known, either component can be extracted directly at its bit position. Nevertheless, the current miss path reconstructs every coordinate in the chunk, up to 256 pairs, to answer one query.

   I would replace this with independently built immutable node ranges:

   - Decode, validate and encode ranges on workers.
   - Retain direct chunk locations, presence masks and counts.
   - Merge boundary fragments where input blocks split storage chunks.
   - Validate ordering across ranges using their boundaries.
   - Resolve admitted way references in batches, grouped by node chunk, then scatter coordinates back to their original occurrence positions.

   The default sparse-query operation should extract one exact E7 coordinate. A batch requesting many coordinates from one chunk can decode that chunk once. This makes the amount of decoded data follow the actual demand.

   Grouping and deduplicating requests within a block also exposes independent memory accesses and shares resolutions across ways. It replaces the current way-at-a-time scalar interface with the workload's natural unit. Duplicate occurrences still retain distinct output positions and missing-reference counts.

   The builder should publish owned range storage rather than copying every finalized group out of reusable staging storage. Heavy compression and tagged-node emission need not wait behind the same owner.

   **Why high payoff:** this attacks both serial index construction and per-reference lookup amplification. It preserves compact coordinates without relying on an eight-entry cache to make whole-chunk reconstruction affordable.

   **Risks:** batching introduces request metadata, grouping and scatter work. Excessively large batches can increase memory and delay emission. A direct-access format needs more location metadata, and hot repeated accesses may favour full-chunk decode. Those tradeoffs belong inside this complete design.

   I would not default to storing all nodes as projected doubles. E7 pairs occupy eight bytes; projected `Point` values occupy sixteen. Nor would I assume an uncompressed rank store wins merely because lookups become simpler: a larger resident working set can undo the CPU savings.

   Pbfhogg's [rank-indexed sparse store](/home/folk/Programs/elivagar/research/pbfhogg/src/commands/altw/sparse.rs:136) demonstrates an alternative with parallel population and compact addressing by present IDs. Its machinery is relevant, but collecting a referenced-node set and constructing its rank index are additional hot build costs, not free setup.

4. **For sufficiently large raw inputs, replace the node index entirely with a demand-driven external join. Full alternative architecture; high upside, medium confidence in wall-time superiority.**

   Sorted node IDs do not make way references arrive in node-ID order. A country-to-continent workload can therefore spend heavily on random memory access even after the node representation improves.

   The more aggressive solution is to invert resolution:

   Read ways and identify those requiring geometry. Emit requests containing node ID and a destination occurrence. Partition requests by node-ID range, join them against sorted nodes, then partition resolved results back into bounded way batches. Completed batches feed emission and the relation geometry publication described above.

   This converts random lookups into partitioned sequential work. Unreferenced nodes need no coordinate store, although all nodes must still contribute to extent and eligible tagged-node emission.

   Pbfhogg already implements the central algorithm in [its external ALTW join](/home/folk/Programs/elivagar/research/pbfhogg/src/commands/altw/external/mod.rs:1): node-ID requests, merge resolution, destination-slot permutation, and publication of completed blob payloads. Its [concurrent payload router](/home/folk/Programs/elivagar/research/pbfhogg/src/commands/altw/external/coord_payloads.rs:49) also demonstrates consumption before every payload is finished.

   **Integrate the join with Elivagar's consumers.** Writing an enriched PBF and then reading it again would retain an avoidable serialization, compression and I/O cycle. Join only geometry that Elivagar needs, and publish completed coordinate batches directly.

   **Why plausible:** it removes the global node store and changes the access pattern fundamentally. That is a stronger response to an out-of-cache or out-of-memory index than further tuning individual lookups.

   **Why conditional:** the join writes and rereads request and result streams and adds input traversal or retained way data. Those costs scale with reference count. A compact resident index can beat them. I would favour this architecture when random access and index residency dominate, rather than assume it wins on every country extract.

   There are two concrete compatibility traps:

   - Pbfhogg's ALTW path fills unresolved slots with zero coordinates. Elivagar's raw path drops unresolved coordinates and counts the missing references. The integrated result needs explicit validity information.
   - Pbfhogg's raw coordinate scanner uses checked scaling, while the element accessors use wrapping arithmetic and narrowing. Reusing the scanner unchanged can alter accepted coordinates on pathological input.

   Neither `(0,0)` nor a coordinate payload alone can represent missingness.

**Earlier integer quantization is a separate geometry rewrite with medium conviction.** It could eventually eliminate much of the E7-to-double-to-integer traffic, particularly for polygon consumers. Two integer components can occupy eight bytes instead of a sixteen-byte Mercator `Point`.

However, projection already uses [a lookup table](/home/folk/Programs/elivagar/crates/lib/src/geometry/projection.rs:83). This is not a remaining transcendental calculation per reference. The stronger argument for integer geometry is compact storage and fewer representations.

It is also not automatically equivalent to the current pipeline. Way closure uses exact E7 coordinate equality. Multipolygon joining uses projected keys rounded at `1e9`; relation pin detection uses keys at `1e12`. Area matching, labels and line simplification consume projected geometry before final polygon quantization. Dateline unwrapping is path-dependent.

A canonical integer geometry design therefore needs coordinated changes to those consumers and an explicit explanation of altered tile geometry. I would first implement the lossless ownership and resolution rewrites above. Moving projection alone earlier, while keeping every later representation, is unlikely to deliver comparable gains.

Several local changes remain worthwhile, and should be distinguished from those rewrites:

- **Fix the node cache's payload swaps.** `CacheEntry` contains an inline 2,048-byte coordinate array. [Cache promotion and replacement](/home/folk/Programs/elivagar/crates/lib/src/node_index.rs:753) swap entire entries. These are value swaps, not pointer swaps, despite the comments. Maintain recency with small slot indices instead. The current swap-to-front policy is also not true LRU: on replacement it moves the previous front entry into the next eviction slot. This is a local change with potentially substantial raw-path benefit.

- **Use metadata-skipping node iteration.** [Node workers](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/nodes.rs:208) use `for_each_element`, whose dense iterator advances metadata that Elivagar does not consume. Pbfhogg already exposes [`elements_skip_metadata`](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:667). This benefits both input shapes when metadata is present.

- **Move relation preparation into parallel tasks.** Feeding blocks or lightweight relation descriptors into parallel work would remove coordinate reads and projection from the serialized producer. This is an immediately useful local change, although it leaves the writer, offset sort and repeated geometry reconstruction intact.

- **Remove unnecessary coordinate and reference copies.** Inline `resolved_node_refs` need not clone the plan's refs. Raw resolution can construct positional pins alongside coordinates instead of constructing another reference vector and rescanning it. Relation-only ways can transfer their coordinate storage rather than cloning it before discovering that they emit nothing themselves. These are medium-value local changes.

- **Replace byte-wise rank accumulation with word-wise rank operations.** [`count_bits_before`](/home/folk/Programs/elivagar/crates/lib/src/node_index.rs:375) scans preceding bytes and counts their bits. Four machine words with small prefixes offer a cheaper rank operation. This is below the architectural changes in priority.

Two additional findings affect the assessment. First, the way-index reader is not allocation-free: `decode_way` allocates an E7 vector, and relation preparation allocates a projected vector. Second, way admission accounts for block and plan bytes, while member-coordinate clones accumulate in task results and can remain queued after admission tickets are released. The replacement design should carry a byte charge with retained geometry through its actual lifetime.

The observable-contract constraints are precise: retain coordinate conversion, the sequence of surviving references, missing counts per occurrence, existing closure and pin rules, comprehensive extent, wrapped source geometry, and deterministic feature ordering. The first three architectural rewrites can meet those constraints without changing tile bytes. The external join needs semantic adaptation; early integer geometry needs a separate equivalence argument.
