I would make two substantial rewrites: replace the owned-feature encoder with a compiler over borrowed record spans, and replace the per-tile archive sink with content jobs plus deterministic placement. Those address repeated work and serialized ownership directly.

Compression also has two concrete local opportunities: reusable compressor state and hardware dispatch that the dependency configuration currently disables.

This is a source-only assessment. I read the requested paths, their producers and scheduling interfaces, pbfhogg's relevant writer internals, and the installed dependency implementations. No files were changed and no builds, tests or workloads were run. The ranking reflects structural evidence, not measured speedups.

| Priority | Opportunity | Classification | Conviction |
|---|---|---|---|
| 1 | Borrowed records through grouping to final protobuf | Full encoder rewrite | High, broad applicability |
| 2 | Content jobs, compression sharing and deterministic archive placement | Full output pipeline rewrite | High for removing serial work; dedup savings depend on workload |
| 3 | Resettable compression contexts and hardware dispatch | Local changes | High |
| 4 | Feed MLT's existing column writer directly | Focused backend replacement | High for MLT workloads |
| 5 | Build directories incrementally | Partial writer rewrite | Medium, strongest at very large tile counts |

1. **Replace `LayerBuilder` assembly with a compiler over borrowed record spans.**

   The current representation makes ownership and materialization prerequisites for operations that mostly need references.

   `RawBatch` already holds records in a flat slab. Nevertheless, [record decoding](/home/folk/Programs/elivagar/crates/lib/src/wire_format.rs:446) allocates and copies a geometry vector for every feature, allocates its tags, and interns attributes into owned tables. Every distinct string value gets two owned strings, and keys that arrived as schema IDs become strings and pass through another hash table. These objects remain alive across preparation of all the tile's layers.

   Same-attribute merging then builds a stable sorted index of non-point features, compares tag vectors, walks every merged geometry through the command decoder, constructs another command vector, and clones that vector into the surviving feature. Below z14, line features are subsequently decoded into absolute-coordinate segments, merged, and encoded into command words again. See [same-attribute merging and its line-merger call](/home/folk/Programs/elivagar/crates/lib/src/mvt/merge.rs:76).

   Finally, serialization creates another copying chain:

   `geometry words -> packed bytes -> feature buffer -> layer buffer -> tile buffer`

   This is explicit in [the MVT serializer](/home/folk/Programs/elivagar/crates/lib/src/mvt/mod.rs:335) and `protohoggr::encode_packed_uint32`. A serialized geometry byte passes through three full buffer copies after its initial varint encoding.

   The stronger design is to process one layer at a time from borrowed records:

   - Validate record envelopes and retain geometry spans, borrowed strings and typed scalar values.
   - Use the existing `AttrKey` IDs directly. Produce the present-key table by filtering a precomputed alphabetical schema order.
   - Intern ordered attribute tuples into tile-local group IDs. Group non-point features by geometry type and attribute-group ID, retaining the first feature's position and each member's original order.
   - Represent merged polygon and z14 line geometry as lists of source spans. Concatenation should describe the output, rather than construct another owned command stream.
   - For lines below z14, decode those source spans directly into the line merger's input. Encode its final coordinates directly into protobuf geometry bytes.
   - Calculate message lengths during preparation, then write features directly into their final positions in the tile buffer. Remove the packed, feature and layer staging buffers.

   Expected grouping work becomes linear in features and attributes, rather than sorting features by tag vectors. More importantly, this removes per-feature geometry ownership, merged command-vector copies and unnecessary geometry round trips. It also makes memory proportional to the active layer, actual line-merger work and final payload, rather than all layer builders plus their scratch.

   **This is my highest-conviction architectural opportunity.** It applies to unique tiles too, including the high zoom tiles where archive deduplication may save nothing.

   There are precise compatibility requirements. Preserve point singletons, ocean's current exclusion from same-attribute merging, group placement at the first member, singleton IDs, removal of IDs only for groups that currently merge, and the exact segment order supplied to the line merger. Preserve bitwise floating-point equality, including signed zero and NaN representations. Hash tables can index groups without determining output order.

   The initial rewrite can preserve uncompressed MVT bytes exactly. It need not change topology, feature grouping or protobuf field order.

   The record-format sibling could strengthen this further. I would ask it for borrowed geometry and attribute views, plus first-point, final-cursor and encoded-length summaries. For geometry that only needs concatenation, packed command bytes with cursor summaries allow rebasing just the first coordinate pair and copying the remainder. For lines entering the line merger, coordinate spans avoid encoding commands upstream merely to decode them later. These can be different record variants; a universal intermediate geometry representation is unnecessary.

   Any record-format change must address [the existing payload-byte tie breaker](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:153). Changing serialization can change feature order even when every geometry is unchanged. An explicit logical ordering must replace that accidental dependency.

2. **Replace “compressed tile handed to writer” with content jobs and deterministic placement.**

   Currently, [each computed tile is fully compressed](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/encode.rs:183), collected into an encoded batch, and eventually passed to the ordered writer. Only there does [deduplication inspect it](/home/folk/Programs/elivagar/crates/lib/src/archive/writer.rs:884).

   The ordered thread also computes two complete SipHash passes over every compressed payload, performs dedup lookups and insertions, copies accepted bytes into a buffered file sink, and records directory entries. The hash passes are visible in [the dedup implementation](/home/folk/Programs/elivagar/crates/lib/src/archive/writer.rs:251). Its map stores a separate `Vec` per hash bucket, normally to hold one entry.

   This creates a serial ceiling proportional to output bytes, alongside serial per-tile bookkeeping. A many-core encoder cannot overcome that ceiling. It also spends compression CPU on computed payloads that the writer subsequently discards.

   I would redesign this as one coherent output system:

   **Encoding produces content candidates.** Each candidate carries its logical tile ordinal, final uncompressed payload and effective compression contract. Artifact-only runs retain their source blob identity instead of becoming ordinary payload jobs.

   **Content resolution precedes compression.** A bounded, sharded index compares exact uncompressed bytes and publishes an in-flight content handle. Equal candidates under the same effective compression contract share one compression result. Duplicate candidates arriving while compression is running attach to that result.

   **Workers produce owned output blocks.** Compress unique content into bounded slabs containing multiple payloads and return descriptors containing offsets, lengths and precomputed fingerprints. This replaces one separately allocated `Vec` per queued tile.

   **The ordered coordinator places content.** Its job is resolving tile references, selecting deterministic first storage, assigning archive offsets, extending runs and producing directory entries. It should not scan payload bytes or copy them. An I/O backend consumes owned blocks or vectors of accepted slices at assigned offsets, with buffers retained until completion.

   Deterministic archive placement remains in Hilbert order. Physical writes can complete independently once offsets are assigned. The current direct-to-final-data-section layout is already appropriate and should remain; [streaming construction](/home/folk/Programs/elivagar/crates/lib/src/archive/writer.rs:834) does not need a second full tile-data file.

   The scheduling handoff I want is therefore an owned, bounded block plus tile-to-content references, rather than a batch of independently owned compressed tiles. Admission must account for raw bytes, compression workspace, finished blocks and pending I/O until their actual release. Otherwise the redesign merely moves memory into another queue.

   There are two distinct payoff mechanisms:

   **Removing byte scans and copies from the coordinator has broad value.** Parallel fingerprints alone can preserve current placement decisions while removing both SipHash passes from the serial thread. The existing XXH3 dependency supplies a plausible faster fingerprint implementation, although fingerprints must remain candidate selectors rather than proofs of equality.

   **Avoiding duplicate compression is workload-dependent.** Artifact-only ocean tiles already arrive as compressed runs, so their large addressed-tile count is not evidence of wasted compression. The relevant quantity is compression CPU spent on duplicate *computed* payloads. This design saves that work where it exists, but I would not claim an enormous compression win from ocean coverage alone.

   The risks are manageable but architectural. Cache admission and eviction must follow logical tile order, not completion timing, or dedup decisions can make archive bytes nondeterministic. Raw representatives consume memory, so bound their retained bytes. A cold candidate can safely become a miss instead of requiring an expensive archive reread. Include effective compression level in content identity: identical raw tiles at different zoom policies must not silently share a result produced at another level.

   Prebuilt artifact blobs should have stable content handles keyed by source blob identity. Repeated runs referencing the same source blob then avoid repeated compressed-byte hashing.

3. **Reuse compressor state and restore hardware dispatch. These are local changes with potentially substantial payoff.**

   The gzip path constructs a fresh `GzEncoder<Vec<u8>>` for every tile. That constructs a fresh DEFLATE context, a gzip header allocation and, in the installed flate2 implementation, a 32 KiB intermediate output buffer. Its output is then copied into the destination vector.

   The low-level API already supports the required alternative: [`Compress::reset`, `compress_vec` and `new_gzip`](/home/folk/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/flate2-1.1.11/src/mem.rs:245). pbfhogg already uses [a retained compressor with reset](/home/folk/Programs/elivagar/research/pbfhogg/src/write/framing.rs:569) for PBF framing.

   Give compression workers persistent contexts for the effective levels they encounter, and compress into caller-owned slab capacity. Keep variable-size geometry scratch separate from this fixed compressor state. Reusing a compressor does not require retaining each worker's largest tile representation.

   The current default effective levels are 9 at z0-z8, 6 at z9-z12 and 3 at z13-z14. Retain that mapping. The opportunity is removing construction and buffering costs around the configured compression work.

   There is also a concrete dependency issue. [Elivagar's manifest](/home/folk/Programs/elivagar/crates/lib/Cargo.toml:24) disables flate2's defaults and enables only `zlib-rs`. The installed flate2 exposes `runtime_detection` separately; without it, zlib-rs's `std` feature is not enabled through this path. In [zlib-rs's dispatch code](/home/folk/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/zlib-rs-0.6.8/src/cpu_features.rs:73), hardware CRC detection then returns false. Generic builds also lose AVX2 match-comparison dispatch; compile-time AVX2/BMI features can recover that latter path.

   Enabling runtime detection is a production dependency fix worth making. It can also benefit PBF decompression outside this scope. Its throughput effect remains unmeasured.

   Lower-level gzip framing or a different finish sequence can change compressed bytes while preserving the uncompressed tile exactly. Fixed headers, fully reset state and deterministic compression must preserve repeatability of builds.

   Brotli deserves a separate implementation. Its public state API supports caller-owned output and custom allocation, but does not offer the same straightforward reset interface. I would remove wrapper buffering where useful without assuming gzip's context-reuse design transfers unchanged.

4. **Replace MLT's owned conversion path with the dependency's existing `LayerWriter`.**

   [The current MLT adapter](/home/folk/Programs/elivagar/crates/lib/src/mlt.rs:173) starts with MVT-oriented `LayerBuilder`s, validates properties into another tile model, decodes commands into nested owned `geo_types` objects, and builds an owned `TileLayer`. For every feature, it loops over every column and scans the feature's tags to find that column's value. The library subsequently stages this back into columns.

   The installed dependency already has [`mlt_core::LayerWriter`](/home/folk/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/mlt-core-0.22.0/src/layer_writer.rs:151): borrowed string values, typed property columns, flat coordinate and part storage, feature IDs, ring-writing methods and buffer reset.

   Feed that directly from the final grouped features. Map schema key IDs to property handles once, then write each feature's actual properties once. Decode geometry directly into its flat representation instead of constructing `Geometry`, `MultiPolygon` and nested ring objects first.

   This removes an entire owned object model and the repeated column-to-tag searches using an API already available today. It is a focused backend replacement, with high confidence in reduced allocation and traversal for MLT workloads.

   Preserve the existing column order, feature order, ID handling, polygon grouping and encoder profile. A shared neutral encoder framework can be considered afterwards, if this works; it should not constrain either backend's redesign.

5. **Build directory leaves as runs close, rather than replaying raw entries at archive close.**

   The streaming writer spills each completed run as a 24-byte entry. Finalization rereads those entries, allocates leaf chunks, encodes and compresses them, and accumulates every compressed leaf into one `leaf_blob`.

   The leaf compression is [already parallel and batched](/home/folk/Programs/elivagar/crates/lib/src/archive/writer.rs:1283). The remaining opportunity is eliminating the raw spill/replay and final all-leaves materialization.

   Accumulate completed entries into leaf-sized blocks during assembly, compress completed leaves, and retain a compressed leaf spool plus root summaries. Finalization appends the compressed stream and writes the root. If the root exceeds its reserved budget, deterministically compact adjacent leaves into larger leaves.

   For scale, 50 million directory entries mean 1.2 GB of raw entry writes and another 1.2 GB of reads before encoding. Addressed tiles represented by runs do not each incur an entry.

   This is a medium-value structural change. It removes intermediate I/O and a finalization tail, but it does not reduce the dominant per-tile geometry or compression work. Its risks are root-size handling, changed leaf boundaries and extra codec competition during assembly.

Two additional local changes are worthwhile within the current structure. Release builders and merge scratch before starting compression, since [the current function](/home/folk/Programs/elivagar/crates/lib/src/pipeline/assemble/encode.rs:145) keeps them in scope throughout it. Flatten the dedup index so ordinary unique hashes do not require separate bucket allocations. These reduce memory and pointer chasing, but do not substitute for the two larger rewrites.

Three correctness observations also emerged:

- **Gzip level 10 is accepted but cannot be encoded by the selected backend.** [Validation permits 10](/home/folk/Programs/elivagar/crates/lib/src/pipeline/mod.rs:319), and the zoom mapping passes it through at z0-z12. The installed zlib-rs rejects levels outside 0-9, and its constructor asserts initialization success. This reaches a panic rather than a configuration error.
- **Archive deduplication currently treats fingerprints as equality.** Matching both SipHashes and length immediately reuses an offset without comparing bytes. That is an extremely unlikely collision risk, but the implementation is probabilistic. A redesigned content index should compare retained representative bytes and safely miss when it cannot.
- **Brotli completion errors cannot reach the caller.** Elivagar finishes by dropping `CompressorWriter`; the dependency suppresses errors from its final `FINISH` operation. The destination vector makes ordinary sink errors irrelevant here, so this is not evidence of observed corruption, but the path cannot report an encoder finalization failure.
