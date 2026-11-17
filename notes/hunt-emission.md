**The biggest opportunity is to stop making a self-contained, tile-specific record the unit of external processing.** I would rewrite the emission-to-assembly seam around spatial feature packets, shared attribute sets, and deferred tile expansion. Smaller encodings would help, but they preserve the multiplication that causes the cost.

This assessment comes from source inspection only. I read emission, serialization, sort writers and readers, assembly, geometry producers, and relevant pbfhogg internals. I made no changes and ran no workloads. Payoff rankings below are architectural hypotheses, not measured speedups.

| Priority | Change | Classification | Conviction |
|---|---|---|---|
| 1 | Spatial feature packets; expand into tiles inside assembly workers | Full emission, sort, and assembly rewrite | High |
| 2 | Output-sensitive line routing with a shared simplification hierarchy | Coherent line-emission algorithm rewrite | High |
| 3 | Group geometry before constructing MVT command streams | Coordinated producer and consumer rewrite | High on eliminating work; payoff depends on layer mix |
| 4 | Direct node emission, reusable attribute buffers, narrower encodings | Local changes | Medium |

The first and third belong together in the strongest implementation. Their benefits should not be counted as independent speedups.

**The current representation pays repeatedly for information whose lifetime is one source feature.** Each record contains an eight-byte feature ID, geometry type, command count, four bytes per MVT command word, and a complete visible attribute set. The chunk format adds an eight-byte sort key and four-byte payload length. For command count \(C\) and encoded attribute length \(A\), that is:

\[
25 + 4C + A
\]

uncompressed bytes per record, excluding chunk headers. An attribute-free point costs 38 bytes; an attribute-free full-tile rectangle costs 70 bytes. In the producer arena, every record additionally has a 24-byte `PayloadRef`. See [wire serialization](/home/folk/Programs/elivagar/crates/lib/src/wire_format.rs:315), [chunk serialization](/home/folk/Programs/elivagar/crates/lib/src/sort/format.rs:298), and [record descriptors](/home/folk/Programs/elivagar/crates/lib/src/sort/mod.rs:73).

There are already useful amortizations: ways and polygons use arena-backed payloads, and attributes are encoded once per emitted zoom. But `append_feature_data_with_attrs` still copies those attribute bytes into every fragment. Each payload then passes through the shared coalescer, disk serialization, a merge-source buffer, the assembly batch slab, and an owned geometry vector. Assembly interns attributes again and subsequently merges many of those individually materialized features. The arena removes allocations; it does not remove replication.

Not all geometry is redundant. Distinct clipped boundary fragments often carry genuinely different coordinates. The strongest redesign must distinguish those from repeated metadata, repeated source geometry, and entirely symbolic interiors.

**1. Replace flat tile records with spatial feature packets.**

The structural problem is that emission finishes too much tile-specific work before establishing spatial ownership. It produces complete records for all tiles and zooms, then asks an external sort to transport and reorder their full contents.

I would instead have emission produce a feature packet containing:

- Source identity and layer presentations, including paint rank.
- Typed attributes with their visibility thresholds.
- Geometry stored once at the packet's spatial scope.
- Polygon frontier fragments, line geometry, or point coordinates.
- Symbolic full-fill subtrees where applicable.

Packets would be collected into spatial pages covering bounded tile subtrees. A page worker would gather all contributions, generate its tile fragments, order compact descriptors locally, merge geometry, and encode the final tiles. Large pages would split recursively. Low zooms would have separate ownership so continent-wide features could not force one enormous working set.

The important distinction is **spatially colocated storage**, rather than a global feature store with tiny references scattered through a tile sort. A global store could reduce logical bytes while replacing sequential reads with millions of random geometry fetches. Packets should carry the geometry and attribute dictionaries needed by their owning pages. Replication across page boundaries is acceptable; replication into every tile is the expensive part to remove.

For polygons, the existing implementation supplies a particularly strong handoff point. Descent clips a base-grid fragment into children; each cell's output is derived separately from that fragment. The code already exposes [frontier splitting](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:203) and [resumption from a cell](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:189). OSM emission currently calls the complete serial traversal for each match. A redesigned pyramid should hand emission an owned frontier cell, its exact base-grid fragment, required pin information, and its zoom policy. Assembly can continue the same descent after spatial collection.

That preserves the actual clipping ancestry. Reconstructing fragments later by clipping the original polygon directly to arbitrary assembly partitions would risk different rounding and topology.

The clearest waste is polygon interiors. [The pyramid recognizes full buffered cells](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:391), but [recursively emits every descendant](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:690). Both OSM polygon sinks then discard the `PyramidEmitKind` argument and serialize full geometry normally: [polygon sink](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:1090), [multipolygon sink](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:1204).

A full subtree should remain one operation:

```text
FullFill(feature, spatial_cell, emitted_zoom_range, attributes)
```

For a full cell with six descendant levels remaining, the current traversal emits 5,461 records, including the root. One subtree descriptor can represent those contributions until the assembly owner needs them. Hilbert descendants form contiguous intervals at each target zoom, making interval-based delivery practical.

This requires changing the pyramid before descendant expansion. Merely encoding each existing `FullFill` callback as a shorter record leaves the record count, key construction, sorting, and merge work intact.

The payoff is plausible because this removes work from several stages simultaneously: attribute copying, geometry serialization, external sorting, compression and decompression of intermediates, and merge scheduling. It also removes repeated tile-key computation for symbolic descendants.

The principal risks are page skew, repeated geometry across page boundaries, and moving too much work into assembly. Page size must follow actual geometry cost, with bounded expansion and subdivision. Input completion also remains necessary: PBF order provides no spatial watermark, and later relations can still contribute to an earlier tile. This design does not imply that final tiles can safely stream out immediately during PBF reading.

There is also an output-sensitive memory problem today. Way and relation flush checks happen after processing a whole feature or relation, so a large expansion can exceed the nominal accumulator threshold substantially. Frontier packets and bounded page expansion address that directly. See [way processing and flush placement](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/ways.rs:318) and [relation accumulation](/home/folk/Programs/elivagar/crates/lib/src/pipeline/relations.rs:270).

**The packet handoff should transfer ownership, not copy bytes under one global lock.** Currently, [SpillCoalescer::append](/home/folk/Programs/elivagar/crates/lib/src/sort/write.rs:392) copies every producer payload into one shared arena while holding its mutex, and rebases every descriptor. The actual sort and write occur outside that lock, which is good, but all emitted bytes still pass through its serialized memory-copy section.

The replacement should accept sealed producer pages by ownership. Large logical runs can contain multiple immutable slabs, with descriptors addressing slab-local offsets. Writers can combine those into substantial partition sections without copying all payload bytes into one contiguous arena. This preserves useful I/O aggregation while removing the global payload-copy funnel. Emitting thousands of tiny independent files would sacrifice that benefit.

**2. Rewrite line emission around the segments that actually intersect tiles.**

[Line emission](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:872) simplifies a line for a zoom, enumerates every tile in its bounding box, and invokes the clipper separately for each tile. [The clipper](/home/folk/Programs/elivagar/crates/lib/src/geometry/clip.rs:82) can reject some tiles quickly, but surviving candidates scan the line's segments again.

For \(V_z\) simplified vertices and \(B_z\) bounding-box tiles, the expensive path approaches \(O(V_z B_z)\). A diagonal crossing an \(n \times n\) tile rectangle intersects only order \(n\) tiles, while its bounding box contains order \(n^2\). Long boundaries, ferries, and winding waterways are the concerning cases. Adding threads does not remove that work.

The replacement should traverse each simplified segment through the tile grid, accounting for buffered clip rectangles, and append references to the affected tiles. Each tile receives source-ordered segment references or contiguous source ranges. Exact clipping then operates only on those candidates.

This is a full algorithm replacement, not moving the current bbox loop into assembly. Its work should scale with source segments and actual segment-to-tile intersections rather than rectangle area times source length. Inside spatial pages, it also avoids transporting already encoded copies of all resulting line pieces.

Correctness is manageable if candidate discovery changes first while exact clipping keeps the original simplified segment endpoints and arithmetic. Repeatedly clipping parent-clipped lines into children could introduce different intersections. Reentry boundaries, source order, the `points_near` behavior, and per-piece subpixel filtering also need to survive the redesign.

There is a second reusable structure here: the simplification hierarchy. Every zoom below z14 currently reruns Douglas-Peucker from the original source. Closed lines also repeat canonical rotation and anchor selection. See [simplification dispatch](/home/folk/Programs/elivagar/crates/lib/src/geometry/simplify.rs:105), [closed-line setup](/home/folk/Programs/elivagar/crates/lib/src/geometry/simplify.rs:204), and [recursive splitting](/home/folk/Programs/elivagar/crates/lib/src/geometry/simplify.rs:313).

Build the required-vertex partitions and DP subdivision tree once, down to the finest tolerance that any simplified zoom needs. Derive each coarser zoom by pruning that tree; z14 continues to use the original source. Preserve exact distance comparisons and tie rules. Vertex activation must include ancestor decisions, rather than independently thresholding each vertex's own error.

Compatible layer presentations can share this geometry work. Their visibility and size-filter policies still remain distinct. The current loop invokes emission independently for every match, even where presentations use the same source and simplification.

I rank grid-based routing above shared DP: it removes the more dangerous multiplicative behavior. Both belong in the same coherent line rewrite.

**3. Make the intermediate representation serve geometry grouping, then encode MVT once.**

The current format commits to MVT commands before assembly knows which pieces will become one output feature.

The resulting path is:

1. Emit an independent MVT command stream.
2. Serialize its words as fixed-width integers.
3. Allocate and copy them back into a `Feature`.
4. Decode commands while concatenating same-attribute geometry.
5. For lower-zoom lines, decode that geometry into absolute segments again.
6. Merge connected lines and re-encode.
7. Convert command words into protobuf varints.

These are actual paths in [record decoding](/home/folk/Programs/elivagar/crates/lib/src/wire_format.rs:424), [geometry concatenation](/home/folk/Programs/elivagar/crates/lib/src/mvt/merge.rs:15), [connected-line merging](/home/folk/Programs/elivagar/crates/lib/src/mvt/merge.rs:161), and [final encoding](/home/folk/Programs/elivagar/crates/lib/src/mvt/mod.rs:299).

I would have the pyramid supply normalized, oriented ring spans with explicit polygon grouping. Line emission should supply absolute-coordinate subline spans. Tile assembly should group these spans using shared attribute-set identities, run the existing connected-line operation where required, and generate MVT commands after the final geometry grouping is known.

Attributes should be stored once with `from_zoom`, or once per distinct visibility interval. Fifteen zooms do not imply fifteen distinct attribute sets. Typed keys already declare value types, so every fragment need not repeat both a key discriminator and value-type discriminator. Dynamic strings should belong to packet or page dictionaries. Tile dictionaries can be built from the attribute sets actually used, retaining the current canonical output ordering.

This removes per-fragment owned geometry vectors and tag vectors, repeated attribute decoding and hashing, and geometry encode/decode cycles. Compact immutable command spans can still be useful where geometry truly passes through unchanged. The point is to choose representation according to the consumer's operations.

The risks are observable feature behavior. [Same-attribute merging](/home/folk/Programs/elivagar/crates/lib/src/mvt/merge.rs:76) retains the first feature's position and removes IDs from merged groups. Points remain separate. Ocean is excluded from that merge. A replacement must reproduce those decisions, rather than opportunistically unioning polygons or merging all identical-looking content.

For initial correctness, retain the existing fragment sequence and merge grouping. Geometry spans then reproduce the same coordinates and polygon boundaries while changing only internal storage. Further changes to geometry ordering can be evaluated separately.

**Deterministic order needs its own representation.** Today, equal sort keys are ordered by the entire serialized payload: [record comparison](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:153). Consequently, changing string dictionaries or geometry compression can change feature order even when decoded content is equivalent.

Do not use page-local handles, dictionary insertion order, or worker completion order as tie-breakers. Carry stable source and fragment identity, and define ordering independently of storage encoding.

If retaining current tile bytes, preserve the existing logical comparator, including its surprising little-endian byte ordering of feature IDs, with exact fallback ordering for multiple fragments sharing an ID. If choosing a new canonical order, say explicitly that tile bytes and some geometry sequences change. Preserve feature membership, attributes, polygon grouping, line connectivity, and deterministic output; visual similarity alone is insufficient evidence.

**The useful pbfhogg changes are supporting changes, not the primary performance thesis.**

There are concrete internal facilities Elivagar could exploit:

- `raw_tags()` and `string_table_entry()` allow matching and selected-string extraction using block-local IDs before copying strings into packets.
- `BlockBuffer::project` can collect information during validation and subsequently construct a block without another validation.
- Fused reader transforms execute work on decode workers, before a separate consumer handoff.
- Dense-node columns support batch coordinate processing.

See [raw way tags](/home/folk/Programs/elivagar/research/pbfhogg/src/read/elements.rs:288), [string-table access](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:682), [validation projections](/home/folk/Programs/elivagar/research/pbfhogg/src/read/block.rs:1097), and [fused transforms](/home/folk/Programs/elivagar/research/pbfhogg/src/read/reader.rs:364).

These require ownership changes, not just exposing methods. `AttrValue::Str` currently uses `Cow<'static, str>`, and dynamic attributes are copied into owned strings. Immediate way processing could instead borrow until packet sealing; relations that outlive a block still need ownership.

The locations path already uses an unordered reader, so replacing it with the existing ordered fused pipeline would reintroduce a constraint it has removed. A useful pbfhogg extension would combine worker-side projection or transformation with unordered delivery and the metadata Elivagar needs. Raw-input node-store dependencies still apply.

I would pursue that after choosing the packet representation. A general reader API is an afterwards-if-this-works follow-up, not a prerequisite for the redesign.

**Several local changes are worthwhile, but they do not replace the rewrites.**

- **Emit nodes directly into the arena.** Production node processing currently allocates a boxed payload per record, drains those boxes into `RecordSink`, and frees them. It also constructs point scratch per matched node. Pass the sink directly and keep scratch in the node worker. This is the clearest local improvement: [node processing](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/nodes.rs:244).
- **Retain polygon attribute-buffer capacities.** `clear_attrs_cache` replaces every entry with `None`; later callbacks allocate fresh vectors. Reuse buffers and track validity, preferably per distinct visibility interval: [cache reset](/home/folk/Programs/elivagar/crates/lib/src/pipeline/emit.rs:520).
- **Use a narrower fallback wire format.** Packed command words, typed attributes, and per-block dictionaries can substantially reduce uncompressed bytes. LZ4 already compresses redundancy, so raw-width savings will not translate directly into equal disk savings. Dictionary references alone also retain the record-count cost.
- **Bulk-copy native command words if fixed-width storage remains.** The encoder appends each word separately, while the decoder already uses a bulk copy. This is low priority relative to eliminating whole passes.

**Two findings deserve separate correctness or measurement attention.**

First, point and line tile enumeration uses an unbuffered bbox, although line clipping uses buffered rectangles. Polygon coverage explicitly includes the buffer. A point near a tile edge therefore does not reach the neighboring tile through this emitter, and some buffer-only line contributions are never considered. That is a concrete behavioral inconsistency deserving review, not a change to introduce silently during optimization. Compare [bbox enumeration](/home/folk/Programs/elivagar/crates/lib/src/geometry/tiles.rs:54) with [polygon coverage](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:1121).

Second, the fan-out statistics count records per layer and zoom, without deduplicating tile IDs. Multiple clipped line pieces or polygon components in one tile therefore count as multiple “tiles touched.” This can exaggerate tile fan-out precisely where fragmentation is expensive. See [fan-out accounting](/home/folk/Programs/elivagar/crates/lib/src/pipeline/stats.rs:309).

My implementation priority would be the spatial packet rewrite, including symbolic full subtrees and ownership-transfer pages, followed by output-sensitive line routing. Those changes attack the repeated work and serialized handoffs across the whole pipeline. The final geometry representation should be designed with assembly at the same time; preserving today's opaque payload boundary would leave much of the opportunity behind.
