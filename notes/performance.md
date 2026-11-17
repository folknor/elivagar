# Performance

Findings from the 2026-11-17 performance hunt: twelve codex hunters
(gpt-6.1-sol), one per stage of the tilegen hot path, each reading code only
(no documents, no history, nothing built or run). Every claim below is the
hunters' source reading, not a measurement; payoffs are hypotheses for a
benchmark to settle. The raw reports are in the commit preceding the one that
added this file. Where two hunters proposed different redesigns for one
bottleneck, the entry records both. Where several scopes reported the same
mechanism, that is noted as a fact about the reports. Defects the hunters
found on the way are in `notes/bugs.md`; the encode-side materialization
chain (tile model built from owned features, MVT commands as the
intermediate representation) is filed with the existing entry on that
mechanism in `notes/structural.md`. Several entries touch items the
standing backlog `notes/performance-backlog.md` parks; those are named
where they apply.

1. An entry is removed entirely when completely resolved. No historical record
   stays here.
2. Stable IDs never change and are never reused; removal leaves a gap.
3. An entry adjudicated against, verified incorrect, or whose outcome is that no
   action is taken owes comments at the code sites it names - and, where the
   claim touches a documented contract, the relevant `reference/` or `docs/`
   page - before the entry is removed, so the finding is not hunted again.
4. Once all findings are resolved, the file gets deleted.
5. Finding IDs are never written into the code or other documents. They are
   stable only until this document is drained; the next hunt writes new ones,
   and they are never deduplicated through git history. Carry the context
   inline instead.

---

# Ingest: phase12, nodes, ways, relations

## PERF-001 - Relations are discovered, discarded and rediscovered

Rewrite. Reported by the phase12-actors, relations, shortbread-matching and
node-way-resolution hunters.

Mechanism. Every run starts a relation prepass (`phase12/prepass.rs`) that
reads relation tags, runs full matching, walks members and folds boundary
metadata, then keeps only a needed-way set and boundary metadata and throws
the matched jobs away. `BlobFilter::only_relations()` runs inside pbfhogg's
decode tasks after `BlobReader` has read and allocated the whole blob body, so
indexed input still pays a second compressed-body read and unindexed input a
second decompression and block build of every data blob. The main pass then
decodes relations again and buffers decompressed relation blocks; past the
buffer cap it discards that prefix and recovers it through replay, which on
raw unindexed input revisits every data blob. `prepare_relation` then matches
tags again. The background provenance hash is another full input read. Under
injected membership the exact `needed_ways` set is computed and then discarded
for the broader injected bitmap (`start_way_phase`), and the prepass builds a
full `superset_ways` hash set only for a count.

Redesign. Make the first relation visit produce the execution plan: compact
owned jobs with relation id, matched attributes, pending area policy, member
ids, roles and original occurrence order, member-way requirements and boundary
contributions; spill a compact representation if needed instead of retaining
whole PBF blocks. For indexed input, schedule relation bodies first alongside
node work using pbfhogg's header walker, split kind schedules and worker-local
positional decode, then process node and way bodies without revisiting
relations. Extend pbfhogg's validator projections (relation projections today
keep member-array locations but not roles or tags) so jobs come out of
validation without another element walk. The prepass reductions (needed-way
union, min admin level, disputed-parent OR) do not need file order; shard them
by way id instead of one serial hash-map merge. `par_map_reduce` is not a
substitute: it ignores `BlobFilter`.

Payoff. Removes complete passes and repeated interpretation, not just
synchronization; on unindexed input the prepass duplicates work across the
whole extract.

Risks and limits. Unindexed input cannot learn relation membership from frame
headers; abolishing the prepass there means keeping an external partitioned
way representation for later membership and boundary joins, which can grow
intermediate geometry substantially. The phase12 hunter would benchmark that
design against the relation-first indexed one rather than assume single-pass
wins. The relations hunter would keep a complete metadata pass, because
boundary metadata applies to relations outside the multipolygon/boundary type
gate and can make an untagged way emit a boundary, and must be complete before
member ways emit. Preserve member order, duplicate occurrences, missing-way
handling and block-local pin semantics.

## PERF-002 - Member-way geometry goes through a serial drain, a global offset sort and a barrier

Rewrite. Reported by the phase12-actors, node-way-resolution and relations
hunters; the node-way hunter ranks it highest across both input shapes.

Mechanism. A relation member's coordinates are resolved into a new E7 vector,
cloned into `way_puts` (`phase12/ways.rs`), sent to one drain thread,
delta-varint encoded with a 16-byte offset entry (`way_index.rs`), and after
every way task finishes `finish_writing` serially sorts the offsets, writes
another file and maps and populates both. Then, per relation-member occurrence,
the reader binary-searches, allocates and decodes an E7 vector, and allocates
and projects a Mercator vector. No relation can start until the last way task
and the finalization are done, even when its own members were ready long
before. Shared ways are decoded and projected once per relation occurrence.
Way admission tickets cover block and plan bytes and release on result send,
so member-coordinate clones queued in results are not charged.

Redesign. Retain the dependency plan from PERF-001: dense handles for distinct
required ways and an inverse table from way handle to consuming relations.
Way workers resolve member geometry once and publish an immutable span under
its handle into worker-owned slabs (bounded resident bytes, sequential spill,
cold slabs compressible at coarse granularity; compression and spill stay
parallel producer work). A relation runs when its last dependency publishes;
only relations with unresolved members wait for the way-input seal to
establish absence. Release geometry after its last consumer. The relations
hunter: deduplicate geometry storage and lookup, not member occurrences. The
assembler should accept oriented span references rather than owned
`Vec<Point>` members (see PERF-006). A partitioned directory of per-task
segments replaces the single offset file; tiny per-task files would trade
serialization for fragmentation.

Payoff. Removes a whole write-sort-read cycle and two serial ownership
points, overlaps relations with ways, and stops re-decoding shared ways.

Risks. Retention must be bounded explicitly (a giant relation can pin early
geometry until late); spilling must let producers continue while consumers
hold earlier slabs; readiness accounting and missing-member completion need
one bounded dependency scheduler, not workers parked waiting on each other;
published geometry stays in original wrapped coordinates and consumers must
not unwrap shared storage in place; do not replace compressed storage with an
unlimited cache of 16-byte projected points. The design hunt's sorter
consolidation in `notes/structural.md` covers the offset sort from the other
direction.

## PERF-003 - Relation preparation runs on the serial side of `par_bridge`

Local change (the medium step short of PERF-001/002). Reported by the
node-way-resolution, relations and shortbread-matching hunters.

`process_relation_blocks` (`pipeline/relations.rs`) `flat_map`s every matching
relation in a block through `prepare_relation` before yielding to
`par_bridge`, so member lookup, coordinate decode, allocation, projection and
tag matching all run under the bridge's pull lock. One huge relation
monopolizes preparation, and one block can expand into a large amount of
prepared geometry before any assembly starts. Yield lightweight jobs or
block-owned relation references and resolve geometry on the workers. Leaves
deferred processing, repeated member decode and the last-relation straggler in
place. The relations hunter adds that the tail's wall time is bounded below by
serial preparation, total work over workers, and its slowest indivisible
relation; this fixes the first, PERF-028 the third. Backlog item 18 landed a
streamed relation tail; the hunters read the current code as still preparing
on the iterator side.

## PERF-004 - The raw node store has one owner, and lookups decode whole chunks

Rewrite. Reported by the phase12-actors and node-way-resolution hunters.
Applies to raw (non-locations) input only.

Mechanism. One node worker performs coordinate insertion, compact-store
construction, extent tracking, tag collection, matching and node emission
(`phase12/nodes.rs`); the ordered source exists largely to feed it. The
sorted store's builder advances mutable chunk and group state per insert
(`node_index.rs`). Lookup is a dependent chain: group lookup, presence/rank,
cache search, variable-length chunk navigation on a miss, full chunk
decompression (up to 256 pairs to answer one query) and another rank. The FOR
encoding does not require that: lat and lon are independently bit-packed
offsets, so one coordinate can be extracted at its bit position once rank and
count are known.

Redesign. Build immutable ID-range segments in parallel on workers (256-node
chunks and 64K-id groups are natural units), publish completed groups straight
into the final directory, merge only fragments crossing block boundaries,
validate order from intra-block checks plus adjacent blocks' first/last ids.
Coordination becomes proportional to blocks, not nodes. Builder accepts
independently completed chunks and yields an immutable reader; a mutex around
`put` would keep the bottleneck. Resolve way references in batches grouped by
node chunk, deduplicated within a block, scattered back to occurrence
positions; sparse queries extract one exact E7 pair, dense ones decode a chunk
once. Move tagged-node matching and emission off the store owner (the
shortbread hunter makes the same point: ordered insertion needs sequencing,
classification does not).

Payoff. Removes the one-core ceiling on raw extracts; more decode workers
cannot outrun a worker that touches every node.

Risks. Groups split across blocks, preserving order rejection, transient
duplication during publication; the unsafe flat backend needs an explicit
policy for conflicting writes (today file order wins). Batching adds request
metadata and scatter work, large batches delay emission, a direct-access
format needs more location metadata, hot repeated access may still favour
whole-chunk decode. The node-way hunter would not store nodes as projected
doubles (16 bytes vs 8) nor assume an uncompressed rank store wins.
pbfhogg's rank-indexed sparse store is a relevant alternative but its
referenced-node set and rank index are hot build costs. The backlog lists the
node-store chunk decode as a non-target because production runs
locations-on-ways; this entry is raw-path only.

## PERF-005 - Resolve raw node references by external join instead of a node index

Alternative rewrite, high upside, medium confidence it wins on wall time.
Reported by the node-way-resolution hunter.

Sorted node ids do not make way references arrive in node order, so random
access dominates when the index leaves cache. Invert resolution: read ways,
emit (node id, destination occurrence) requests, partition by node-id range,
join against sorted nodes, partition results back into bounded way batches
that feed emission and PERF-002 publication. Unreferenced nodes need no store
(all nodes still contribute to extent and tagged-node emission). pbfhogg's
external ALTW join already implements requests, merge resolution,
destination-slot permutation and consumption before all payloads finish;
integrate it with elivagar's consumers rather than writing and rereading an
enriched PBF. Costs: request and result streams scale with reference count,
and a compact resident index can beat them; favour this only when random
access and residency dominate. Compatibility traps: pbfhogg's ALTW fills
unresolved slots with zero coordinates while elivagar drops and counts them,
so the result needs explicit validity; pbfhogg's raw coordinate scanner uses
checked scaling while the element accessors wrap and narrow, so reuse can
change accepted coordinates on pathological input.

## PERF-006 - Multipolygon assembly copies chains and pairs rings exhaustively

Rewrite. Reported by the relations hunter.

Mechanism. `multipolygon.rs` joins chains as coordinate vectors: prepend
allocates and copies the existing chain (quadratic coordinate movement over
repeated prepends), reversing allocates, and the second pass rebuilds the
endpoint map, performs one merge and restarts (quadratic bookkeeping). Ring
role assignment tests every unclassified ring against every explicit and
unclassified ring, and hole pairing tests every hole against every outer, each
comparison rescanning coordinates for bboxes, boundary membership,
point-in-polygon and shell area, with no spatial rejection first.
`normalize_ring_start` compares rotations by rescanning common prefixes
(quadratic worst case). Bounding boxes are recomputed inside sort comparisons;
centroid work repeats.

Redesign. A topology compiler: chains are ordered lists of oriented fragment
handles with endpoint metadata and counts; joining edits links and
orientation bits; accepted rings gather vertices once, skipping the same
junction and closure vertices as today. Degree-two endpoint components walk
linearly; independent components run in parallel within a relation. Ring
descriptors carry cached bounds, area, ordering keys and an indexed edge
representation; index candidate containers spatially, query in the right world
copy, run the existing containment predicate only on candidates, reuse
containment results for depth and hole pairing, choose the smallest containing
original outer with today's tie rule. Linear minimum-rotation for canonical
start, preserving its tie choice.

Payoff. Large for long fragmented rings (country boundaries) and for
relations with many separate rings.

Risks. The endpoint map today keeps one chain per endpoint, overwrites and
later recovers; a graph walk that finds more valid cycles is not equivalent,
so branch choices and salvage must be reproduced. Endpoint equivalence is
projected coordinates rounded at 1e9; pins use 1e12; tile geometry the z14
grid; collapsing them changes output. The containment predicate uses the first
off-boundary vertex or edge midpoint rather than proving full containment, so
pruning must be conservative under that predicate, not full-bbox containment.
Behaviours to keep explicit: partial salvage of broken relations; orphan
inners promoted only when an outer exists; pins counted over member
occurrences including members whose chains are later discarded;
`ClosedRings::push` checks vertex count, not distinct vertices. pbfhogg's ring
assembler is not a drop-in (full backtracking, exponential without its budget,
different semantics). The design hunt's entry on assembling multipolygons in
integer space (`notes/structural.md`) is a different proposal over the same
module.

## PERF-007 - Locations-on-ways blocks are walked and decoded several times

Rewrite of the decoder-to-consumer path (medium value alone, more as the
ingress to PERF-012/PERF-022). Reported by the phase12-actors,
node-way-resolution and line-kernels hunters.

Construction validates way arrays; injected-membership validation walks ways
to count them; injected-pin validation walks again counting refs; plan
construction (`phase12/plans.rs`) materializes every way's reference ids and
decodes every inline location for extent; processing (`ways.rs`) decodes
locations again, clones the plan's refs into `resolved_node_refs`, allocates
E7 coordinates and a pin mask, then fills the projected vector. pbfhogg's
geometry validation already counts refs and locations. With injected pins and
membership, reference values have no geometry purpose (membership is
positional, pins arrive as a bitmap); counts are still needed for validation
and missing-reference accounting.

Redesign. An elivagar-shaped validated block result: way descriptors with id,
tags, membership, validated counts, pin positions and coordinate spans. Decode
inline coordinates once, accumulating extent in the same pass, and project
selected geometry straight into its consumer; keep E7 arrays only for
consumers that need them (relation members). For locations input without
injected pins, decode refs once for block-local sharing and produce positional
pin masks directly. Build on pbfhogg's `BlockBuffer::project`,
`GroupProjection`, `WaySpans` and fused transforms (crate-private today;
changing pbfhogg's boundary is warranted). The projection hook runs after
array validation, so removing the validator's packed scan too needs a
deliberately fused validate-and-decode.

Risks. Extent must stay comprehensive (unselected and untagged ways count,
because locations input can omit untagged nodes) and keep both ordinary and
shifted longitude extrema for the dateline decision (`phase12/extent.rs`).
Keep full malformed-array detection; missing location arrays keep today's
missing-reference outcome; block-local pins count across the same ways as
today, including ways later rejected by matching. Publish nothing from a block
that later fails validation.

## PERF-008 - Decoded blocks cross fixed pools instead of one scheduler

Rewrite of the execution model; its wall-time payoff depends on PERF-001,
002 and 004. Reported by the phase12-actors, sort and shortbread-matching
hunters.

Locations input crosses a raw channel, a decode pool (`threads / 3`), a
decoded channel, the router, a one-slot way channel, a dispatcher and a Rayon
task; node emission has its own pool capped at six; the prepass another
decoder pool (`phase12/input.rs`, `resources.rs`, `ways.rs`). The router does
little work, so removing it alone is not a convincing speedup; the cost is
blocks crossing ownership boundaries and independently sized pools that cannot
lend capacity, and a blocked node or way send stalls routing of unrelated
kinds.

Redesign. Queue compressed work or frame descriptors; a worker decompresses,
validates, plans, processes and publishes before releasing the block; ready
work shares capacity; dependencies are pending work items, not parked
workers. Admission follows retained ownership through completion (today a
way ticket does not cover queued `way_puts`, emission scratch or coalescer
buffers being written). pbfhogg already shows fused transforms on decode
workers, worker-resident bounded decode and reduction, and
`parallel_classify_accumulate`/`parallel_project_accumulate`; its fused ordered
reader would still serialize consumption, and element-level `par_map_reduce`
lacks block context for pin planning, so build the block-execution shape
phase12 needs and generalize afterwards. The sort hunter: the goal is a
worker that decodes, matches, builds geometry and publishes owned bucket pages
(PERF-010), with only accounting and dependency messages leaving it; workers
blocked on spill or relation readiness must not stall unrelated work. Raw
input still needs the node store before ways; relations still need members.
Determinism holds: the sort orders by key and payload independent of
scheduling. Three hunters note that elivagar's stated reason for its custom
unordered source (an unbounded ordered decode window in pbfhogg) is stale;
pbfhogg's admission permit now survives reorder and consumption, so the
remaining ordered-reader costs are bounded retention, head-of-line blocking
and extra handoffs.

## PERF-009 - Every emitted byte is copied into one arena under one mutex

Rewrite (the producer half of PERF-010/011). Reported by the
phase12-actors, relations, emission, line-kernels and sort hunters.

`SpillCoalescer::append` (`sort/write.rs`) copies every producer payload into
one shared arena while holding its mutex and rebases every descriptor; the
producer that crosses the threshold then sorts and writes the whole chunk
before returning, so producers double as spill workers. Sort and write run
outside the lock, but all record bytes pass through the serialized copy.
Replace with ownership transfer of sealed producer pages: logical runs made of
multiple immutable slabs, descriptors addressing slab-local offsets, writers
combining them into large partition sections without a contiguous copy.
Writing many small independent chunks instead would fragment the merge. The
relations hunter expects added relation parallelism to hit this next.

---

# The sort seam

## PERF-010 - Replace sorted runs and the k-way merge with a tile-bucket store

Rewrite; the sort hunter's highest conviction. Related proposals from the
emission and line-kernels hunters are PERF-011.

Mechanism. Runs are ordered by packed key then payload bytes, written, and
reconstructed in assemble by a per-partition heap merge that picks a winner
per record and copies its payload into `RawBatch` (`sort/key.rs`,
`sort/merge.rs`). The integer key compare is cheap; the cost is ordering
across unrelated tiles, per-record heap maintenance, readback and
decompression, and payload movement. The fixed z7 split defines 136,533
possible partitions by geography while work follows record density.

Redesign. Producers transfer owned pages (PERF-009) to bucket owners;
records are distributed by zoom and tile-id prefix with radix or counting
partitioning, no comparison sort before spill. Buckets stay resident while the
shared budget allows; spill workers write substantial bucket blocks into a
bounded number of extent files with an explicit directory. When all producers
finish, a worker gathers a bucket, groups descriptors by tile, orders each
tile's features and encodes directly; assembly receives complete tiles or
borrowed tile views instead of a record iterator. Tile order comes from
enumerating occupied buckets. Route by prefix, then subdivide dense buckets by
recorded byte and record distributions; keep the directory sparse. Emission
should hand over owned pages with `(tile_id, layer, rank, payload_handle)`
descriptors; assembly should request complete tiles or bounded ranges with
occupancy and size. The final PMTiles drain stays Hilbert-ordered including
artifact-only ranges.

Payoff. Removes the coalescer copy, cross-tile comparison sorting, the heap
and the merge-to-batch copy; resident buckets also skip readback.

Risks. Scatter cost and fragmented reads with poor page sizing; buckets
outgrowing memory need recursive repartitioning and extra passes; a single
enormous tile needs an explicit strategy; descriptors, payloads and encode
scratch must share one budget. Tile bytes need not change if order inside
each tile stays `(tile_id, layer, rank, legacy payload bytes)`. Tile
completion needs evidence: PBF order is not tile order, relations and ocean
arrive later, so encoding waits for all producers or an exact watermark. Resume
semantics: consuming resident buckets while also persisting them removes
readback but keeps writes; skipping the writes means a replay-backed checkpoint
or another stated change to resume behaviour. The backlog lists the sort phase
as a non-target because its own time is near zero; this entry is about the
merge and copies that run inside assemble and phase12.

## PERF-011 - Records are expanded per tile before spatial ownership is established

Rewrite; the emission hunter's first priority and the sort hunter's
highest-upside follow-up. Also the storage half of the line-kernels hunter's
first priority.

Mechanism. A record carries an 8-byte feature id, geometry type, command
count, 4 bytes per command word and a full attribute set; the chunk adds an
8-byte key and 4-byte length, so 25 + 4C + A bytes uncompressed (38 for an
attribute-free point, 70 for a full-tile rectangle), plus a 24-byte
`PayloadRef` in the producer arena. Attributes are encoded once per zoom but
`append_feature_data_with_attrs` copies them into every fragment. Each payload
then goes through the coalescer, disk, a merge-source buffer, the assembly
slab and an owned geometry vector. The arena removes allocations, not
replication. Not all geometry is redundant: distinct clipped fragments carry
distinct coordinates.

Redesign (emission hunter). Emission produces feature packets: source
identity and layer presentations with paint rank, typed attributes with
visibility thresholds, geometry stored once at the packet's spatial scope,
polygon frontier fragments, line geometry, points, and symbolic full-fill
subtrees (PERF-013). Packets collect into spatial pages covering bounded tile
subtrees; a page worker generates its tile fragments, orders compact
descriptors, merges and encodes. Large pages split recursively; low zooms get
separate ownership so continent-wide features cannot force one huge working
set. Storage must be spatially colocated (packets carry the geometry and
dictionaries their pages need; replication across page boundaries is fine,
replication into every tile is the cost) rather than a global store with
random fetches. For polygons, hand off an owned frontier cell with its exact
base-grid fragment, pins and zoom policy, and continue the same descent in the
page worker (`pyramid.rs` already exposes frontier splitting and
`emit_shape_pyramid_cell` resumption), preserving clip ancestry; reclipping
from source per partition risks different rounding and topology.

Line-kernels hunter's version of the storage half: replace per-tile geometry
payloads with compact associations backed by spatially bundled geometry,
designed for sequential regional reads; short z14-only lines terminate
directly as compact quantized spans rather than carrying a hierarchy.

Sort hunter's version: retain a feature or canonical fragment plus coverage
and let bucket workers generate tile records into tile-local storage, reusing
exact pyramid continuation state; points today become separate records at
every emitted zoom.

Payoff. Removes attribute copying, geometry serialization, external sorting,
intermediate compression and decompression, merge scheduling and repeated key
construction at once.

Risks. Page skew, cross-page replication, moving too much work into
assembly; page size must follow geometry cost with bounded expansion. Input
completion still gates final tiles. Geometry policy must survive the move
(canonical grids, inherited fragments, simplification, pins, buffered
clipping, fanout caps, ring-cap partitioning, antimeridian handling). The
backlog parks items 20, 21 and 24 (tile-owned polygon output, OSM polygon
feature planner, delaying multi-zoom fanout), which overlap this.

## PERF-012 - Partition-sized streams force an assemble prescan and compressed prefix replay

Storage rewrite; independent of PERF-010 if sorted runs stay. Reported by
the sort and assemble-schedule hunters.

`plan_pieces` (`assemble/schedule.rs`) scans every source of a hot partition
for bucket boundaries before any assembly worker starts, and discarded
payloads still go through `io::copy` into a sink
(`next_header_discard_payload`), so plain chunks are read in full rather than
seeked past. Compressed pieces keep the frame start and a record count to
skip (`SourceSeek::slice`, `open_slices`); each piece restarts the decoder and
decompresses its whole prefix again, roughly (q+1)/2 section volumes for q
equal pieces plus the planning scan. The merge-byte counter excludes scanned
and skipped records. Compressed multi-partition writing calls
`stream_position()` before every section, forcing the `BufWriter` through, and
builds a fresh frame encoder per section, so the 1 MiB buffer does not
coalesce small sections. Each source has a 256 KiB `BufReader` whose reads are
not bounded by the section extent, so small sections read neighbours' bytes,
and reader memory grows with fan-in. Exhausted sources keep their readers and
high-water payload buffers until the partition reader drops.

Redesign. Write a block directory while serializing: compressed and
uncompressed lengths, record counts, first/last keys and tile bounds,
independent restart points, tile-respecting boundaries with explicit handling
for tiles larger than a block, plus uncompressed work estimates. Assembly picks
task boundaries from metadata and starts merging at once, balancing by record
count and uncompressed volume (compressed size and fixed Hilbert buckets with
a 32-piece ceiling are weak proxies). Exact block-range reads through shared
handles; compression and decode as block tasks. The assemble hunter notes
there is no materialized final merged stream to delete; the merge already
happens in assemble, so the useful fusion is merge consumption with tile
construction (PERF-014). Codec choice is secondary to framing and indexing.
Risks: smaller blocks compress worse; index overhead; too-fine tasks
reinitialize heaps and cursors; excessive I/O concurrency.

Local steps if runs stay (sort hunter): loser tree, cached comparison
prefixes, winner-run draining, a direct single-source path; seek past plain
payloads; bound section reads; release exhausted sources; 16-byte arena
descriptors (page-local u32 offset and length together); radix sort for large
batches.

## PERF-013 - Full-fill subtrees are expanded into one record per descendant tile

Rewrite across pyramid, sort and assembly. Reported by the pyramid, ocean and
emission hunters.

`emit_full_subtree` (`pyramid.rs`) recursively visits every descendant of a
full buffered cell and calls `emit_full_tile` (`int_ocean.rs`), which rebuilds
and encodes the same tile-local rectangle each time. Both OSM polygon sinks in
`emit.rs` discard the `PyramidEmitKind` argument and serialize full geometry
normally; `ocean_sink` does the same. A full cell with d remaining levels
emits 1 + 4 + ... + 4^d records (5,461 for a z8 cell through z14), which are
then coalesced, sorted, spilled, merged, decoded, encoded, compressed and
deduplicated back into runs.

Redesign. Keep the subtree symbolic: a descriptor with subtree, zoom span,
feature identity, layer, paint rank, attributes and multiplicity
(`FullFill(feature, spatial_cell, emitted_zoom_range, attributes)`).
Descendants are one contiguous Hilbert interval per zoom (seven spans for the
z8 example). Merge intervals with ordinary fragment events; where other
content exists, inject the fill feature into the tile's deterministic stream;
where whole tile contents are identical across an interval, encode and
compress once and write a PMTiles run via `PmtilesWriter::add_run`. The
artifact consumer's interval run copies and its Hilbert range traversal of
fully covered quadtree blocks already show the machinery. The change has to
land in the pyramid before descendant expansion; shortening each record leaves
record count, key work, sort and merge intact. Caching the rectangle command
buffer alone captures little (backlog item 11).

Risks. Exact feature identity and multiplicity; overlapping fills cannot be
collapsed; a full fill does not license discarding another piece's fragment in
the same tile; attributes and compression level vary by zoom; mixed tiles still
need normal assembly; order must follow the existing payload-byte order to
preserve output. Artifact-served ocean interiors already get runs, so their
savings are not counted again. Decoded tile contents can be preserved
exactly.

---

# Assembly and output

## PERF-014 - Assemble hands records across reader/encoder pairs in whole-batch barriers

Rewrite; the assemble-schedule hunter's highest conviction.

Each partition worker runs one serial merge reader that fills a `RawBatch`,
hands it to a newly spawned encoder thread through a three-batch recycling
pool, and the encoder runs parallel tile work but publishes nothing until the
whole batch finishes (`read_encode_partition`, `encode_tile_batch_mvt`). Merge
concurrency is tied to the partition-worker count (eight by default),
independent of encode parallelism; every record is copied from the merge's
head buffer into the slab (`next_into`) and later rebuilt into owned geometry
and attributes; one expensive tile delays its whole batch. Defaults allow 24
circulating raw batches, nominally about 768 MiB of batch budgets before merge
buffers, encode state and output (an illustration, not a measured RSS).

Redesign. A persistent pool over cost-balanced contiguous tile ranges
(PERF-012 supplies the boundaries): a worker owns its merge cursors, consumes
records straight into the current tile's builders via a borrowed
consume-before-advance merge operation, encodes and compresses the tile and
publishes it through small output chunks with range-completion markers. Empty
tiles still advance completion because they decide where artifact fallback
applies. Deletes `RawBatch` slabs and feature-reference tables, reader-encoder
transfers, encoder thread spawn and join per partition, and the batch
barrier.

Risks. More concurrent cursors multiply descriptors, decompression state and
256 KiB buffers; needs bounded block residency and shared file access. A
single enormous tile is indivisible at tile level; independent layer
preparation is the escape hatch; geometry merging within a layer still needs
its full input.

## PERF-015 - The ordered writer thread hashes, deduplicates and writes every payload

Rewrite. Reported by the assemble-schedule and encode-write hunters, who
propose two overlapping designs.

Mechanism. The writer thread receives batches, maintains two levels of
`BTreeMap`, drains them in partition and batch order, runs two full
`DefaultHasher` (SipHash) passes over each compressed payload in
`dedup_hashes` (`archive/writer.rs`) even after the dedup insertion cap is
reached, does dedup lookup and insert (a separate `Vec` per hash bucket,
normally holding one entry), copies accepted bytes through its buffered file
writer and records directory entries. That is a serial ceiling proportional to
output bytes. Every computed tile is fully compressed before dedup sees it, so
compression is spent on payloads that are then discarded.

Assemble-schedule hunter's design. Workers publish descriptors (tile id or
run, length, precomputed fingerprints, owned compressed storage or an artifact
payload identity, completion info proving the next ordered prefix). A small
planner consumes the prefix, resolves dedup with today's first-seen policy,
assigns deterministic offsets and emits directory descriptors; bounded I/O
workers write at those offsets and may finish out of order. pbfhogg's parallel
writer already assigns offsets in ordered delivery and dispatches positional
writes and range copies (its round-robin queues may not suit uneven tiles).
Moving the hashes to encode workers is a worthwhile local change on its own.
Offset assignment still waits on preceding lengths; disk parallelism may add
little if the buffered writer already saturates storage.

Encode-write hunter's design. Encoding emits content candidates (tile
ordinal, final uncompressed payload, effective compression contract); a
bounded sharded index compares exact uncompressed bytes before compression and
publishes an in-flight handle, so equal candidates under the same compression
contract share one compression and late duplicates attach to it. Workers
compress unique content into bounded slabs holding many payloads and return
descriptors; the coordinator only resolves references, picks first storage,
assigns offsets, extends runs and builds the directory, never scanning bytes.
XXH3 (already a dependency) as the fingerprint, as a candidate selector rather
than a proof. Admission must cover raw bytes, compression workspace, finished
blocks and pending I/O. Artifact runs keep their source blob identity (see
PERF-016). Duplicate-compression savings are workload-dependent:
artifact-only ocean tiles already arrive compressed. Cache admission and
eviction must follow logical tile order, not completion order, or dedup makes
archive bytes nondeterministic; bound retained representatives; effective
compression level is part of content identity.

Both keep Hilbert placement and the direct-to-final-data-section layout.
Changes to dedup retention or placement may change archive bytes while
keeping decoded tiles and determinism. Backlog item 26 (writer sharding,
faster fingerprint) is half landed and parks the rest.

## PERF-016 - Artifact splice rediscovers runs per gap and rehashes known blobs

Focused rewrite, medium overall, larger for artifact-heavy archives.
Reported by the assemble-schedule and encode-write hunters.

`plan_artifact_items` (`assemble/artifact.rs`) runs gap planning between each
pair of surviving encoded tiles, rediscovering intersecting source runs and
ownership ranges each time; surviving tiles look up their splice run
separately; run copies lose source identity at `add_run`, where the same
source blob can be hashed again for every fragment. Give each tile-range task
a monotone cursor over artifact runs pre-intersected with ownership intervals
and merge it with computed outcomes (surviving tile: combined payload; empty
tile: leave artifact coverage; uncovered interval: run referencing the
artifact payload identity). Translate `(artifact, BlobRef)` to an output
payload handle once and reuse it, including past the dedup cap; the first
encounter can still dedup against computed payloads. OSM survival must be
decided before artifact fallback; the current code does this correctly. A
task-local cache of gunzipped splice tiles is possible but not shown to
matter. The ocean hunter adds that `OceanArtifact::into_tiles` reads every
world directory run and `read_all_runs` flattens them before `TileRuns` sorts
them; an extract should keep directory state for its relevant ranges only (if
whole-directory validation stays part of admission, stream it while retaining
only relevant runs).

## PERF-017 - Gzip builds a fresh compressor per tile, and zlib-rs runs without CPU dispatch

Local changes, high conviction. Reported by the encode-write hunter.

The gzip path builds a `GzEncoder<Vec<u8>>` per tile: a fresh DEFLATE context,
a header allocation, and a 32 KiB intermediate buffer in the installed
flate2, then copies output into the destination. flate2's low-level
`Compress::reset`, `compress_vec` and `new_gzip` support persistent
per-worker contexts per effective level compressing into caller-owned slab
capacity; pbfhogg's PBF framing already retains a reset compressor. Keep the
z0-z8 level 9, z9-z12 level 6, z13-z14 level 3 mapping. `crates/lib/Cargo.toml`
disables flate2 defaults and enables only `zlib-rs`; without flate2's
`runtime_detection`, zlib-rs's `std` is not enabled on this path and its
hardware CRC detection returns false, and generic builds also lose AVX2 match
comparison (compile-time AVX2/BMI features recover the latter). The
dependency fix may also help PBF decompression; its effect is unmeasured.
Lower-level framing can change compressed bytes but not uncompressed tiles;
builds must stay repeatable. Brotli's state API has no simple reset; treat it
separately. Also: release builders and merge scratch before compression
(`assemble/encode.rs` keeps them in scope); flatten the dedup index.

## PERF-018 - MLT converts through an owned object model instead of `LayerWriter`

Focused backend replacement, high for MLT workloads. Reported by the
encode-write hunter.

`mlt.rs` starts from MVT `LayerBuilder`s, validates properties into another
tile model, decodes commands into nested owned `geo_types`, builds an owned
`TileLayer`, and for every feature loops over every column scanning the
feature's tags for that column's value; mlt-core then stages it back into
columns. mlt-core 0.22's `LayerWriter` already takes borrowed strings, typed
property columns, flat coordinate and part storage, feature ids, ring writers
and buffer reset. Map schema key ids to property handles once, write each
feature's properties once, decode geometry straight to the flat form. Keep
column order, feature order, id handling, polygon grouping and encoder profile.
The line-kernels hunter notes `group_rings` is used in generation only by this
path; carrying polygon structure through assembly would avoid rediscovering
it.

## PERF-019 - Directory entries are spilled raw and replayed at close

Partial writer rewrite, medium, strongest at very large tile counts.
Reported by the encode-write hunter.

The streaming writer spills each completed run as a 24-byte entry; finalize
rereads them, chunks, encodes and compresses leaves (already parallel and
batched) and accumulates all compressed leaves in one `leaf_blob`. 50 million
entries means 1.2 GB written and 1.2 GB read before encoding. Accumulate
entries into leaf-sized blocks during assembly, compress completed leaves into
a spool with root summaries, append at finalize; if the root overflows its
reserve, compact adjacent leaves deterministically. Risks: root-size
handling, changed leaf boundaries, codec work competing during assembly.

---

# Matching and emission

## PERF-020 - Shortbread matching rescans the tag list per layer and per row

Rewrite; the shortbread-matching hunter's first priority.

`Tags` lookups are linear scans and `match_element` invokes each
geometry-compatible layer independently. `street_match` has 31 ordered rows,
each a separate `first_tag_match` scan, so an element with no street class
answers 31 negative queries, and named or referenced elements hit another
31-row label table; buildings and other non-street ways pay this too. Sites and
public transport add nine-row searches. Water geometry and water labels
classify the same tags separately; street attributes and paint rank recompute
bridge and tunnel; `match_addresses` calls `would_match_poi`, which builds and
discards category attributes before checking whether an address exists, and
the POI layer then repeats classification; name lookups repeat across
layers. Tagged nodes and ways collect a fresh `Vec<(&str, &str)>` before
matching.

Redesign. Per PBF block, a dictionary over string-table ids classifies
referenced keys into a fixed Shortbread vocabulary and relevant values into
tokens (unknown values keep their string-table reference); each element makes
one pass over integer pairs accumulating presence and first-value refs,
recognized predicates, precedence-ranked candidates, shared flags (bridge,
tunnel, oneway) and dynamic-value refs; a compiled profile-specific decision
program (direct code or static tables, not an interpreter) yields every
output. pbfhogg already exposes `raw_tags()` for all element kinds and
`string_table_entry`. Classify an id lazily when it is used in a relevant
role; keep caches block- or worker-local. The strongest form runs on decode
workers (PERF-008) so the raw-node owner and relation producer stop matching
(PERF-004, PERF-003), and way processing gets classification facts before
geometry work instead of probing closed then open matching and matching again
(`process_planned_way_into`). Dense-node tags are walked at validation, again
to locate slices and again through the iterator; removing all three needs a
decoder change, not just the projection hook.

Semantics to preserve. `get` takes a key's first occurrence while `has_value`
and `has_any` look at any occurrence; row priority decides winners
independent of tag order, and an unrecognized higher-priority value lets a
later candidate win; street geometry and street labels have different
precedence tables; present-empty, unknown and absent are distinct; outputs
are a sequence (a closed way can emit a polygon and a cliff line in `Land`);
closure stays decided on resolved coordinates. The backlog lists
micro-tuning `match_element` as a non-target; this proposal is a structural
replacement.

Local steps (subsumed by the rewrite): one POI category decision shared by
addresses and POIs (the strongest isolated fix); one lookup per key with
direct value dispatch; allocation-free numeric parsing in `osm_value.rs`
(`parse_length_m` lowercases into a `String`, decimal normalization allocates
without commas; integer, count and length parsing differ deliberately);
transform street refs once (`replace` then `attr_dyn` copies again; keep
Unicode counts, newline and empty-ref behaviour).

## PERF-021 - Matches materialize owned attribute trees that emission re-serializes

Rewrite coupled to emission. Reported by the shortbread-matching and
emission hunters.

`LayerMatch` is asserted at 280 bytes with inline room for eight generic
attributes, even for a single constant kind; `Hits` reserves four plus
pending-rule state; `Hits::finish` collects another vector; `attr_dyn`
allocates a `String` per dynamic attribute, a relation-lifetime requirement
applied to every node and way although they emit while the block is alive.
Replace with an element-level plan: static recipes for constant kinds and
categorical values, flags for booleans, block-dictionary refs for dynamic
strings while the block lives, compact typed numbers, output descriptors with
layer, geometry policy, zoom and rank, and a separate pending boundary-area
operation. Emission serializes straight from recipes into its attribute
buffers. Relations copy their selected attributes into a compact owned arena
once. An adapter that rebuilds `LayerMatch` would forfeit the gain. Serialize
attributes once per distinct visibility interval and share across zooms and
multipolygon components: point and line emission call `encode_attrs_bytes` per
zoom; polygon emission caches per zoom and `clear_attrs_cache` sets entries to
`None` per geometry, so later callbacks allocate fresh vectors (the emission
hunter's local step: keep capacities, track validity). Record bytes can stay
the same if attribute order, types, visibility and string treatment are kept;
a wire-format change needs a semantic tie-break. Area measurement in
`Hits::finish` is already lazy and only boundary labels trigger it; replacing
`area_sq_meters` (cosh per edge) changes `way_area`.

## PERF-022 - Line clipping scans the whole line for every tile in its bbox

Rewrite. Reported by the emission and line-kernels hunters.

`emit_line_feature` (`emit.rs`) simplifies per zoom, enumerates every tile in
the line's axis-aligned bbox and clips the whole line per tile; `clip.rs`
rejects some tiles by outcode but walks all segments otherwise. Roughly N x B
for N vertices and B bbox tiles; a diagonal through an n x n range meets
order n tiles but scans order n^2. Long boundaries, ferries and winding rivers
are the bad cases; threads do not remove it.

Redesign. Walk each simplified segment through the tile grid (with buffer)
and append ordered segment references per touched tile; clip only those runs,
with a segment-bounds hierarchy to accept or reject contiguous runs; interior
runs stay borrowed spans. Work scales with segments, actual incidences and
output vertices. Operate on each zoom's selected geometry with the original
segment endpoints and today's intersection arithmetic; clipping first and
simplifying pieces, or reclipping parent pieces, changes output. Keep the
single-tile fast path. Preserve reentry boundaries, source order, `points_near`
behaviour and per-piece subpixel filtering; buffer edges, corners and world
edges need care. Today's candidate set is the unbuffered bbox (see the bug
document); a router that finds every buffered overlap would change output, so
either preserve the restriction or fix it as a declared behaviour change.

## PERF-023 - Douglas-Peucker reruns from source at every zoom and for every match

Kernel rewrite. Reported by the emission and line-kernels hunters.

Every zoom below z14 reruns DP on the original source (keep flags reset,
splits rediscovered, output compacted); closed lines repeat canonical
rotation, two farthest-point sweeps and allocations (`simplify_closed`).
`ways.rs` dispatches matches separately, so a road and its label, or a
waterway and its label, repeat the work at overlapping zooms. Source,
required vertices and closed-line anchors do not depend on tolerance, so these
are traversals of one subdivision tree at different thresholds. Build it once
per source geometry down to the finest requested tolerance and record which
zooms keep each vertex; a vertex is visible only if every ancestor split
passed (traverse with active zooms, or use minimum deviation along the
ancestor path). Required vertices partition into independent intervals. Keep
the strict `>` comparison, farthest-point ties, source order and the current
distance arithmetic (reassociation changes borderline decisions). The same
traversal yields per-zoom bboxes. Limits: many roads start at z13 and streams
at z14, so the saving is concentrated on major roads, rail, rivers and
boundaries. Antimeridian copies, closed-line start rules, coincident
vertices, closing duplicates and narrowed zoom windows need identical
treatment. Backlog item 25 parks line DP cost on older profiles.

## PERF-024 - The line merger allocates per segment and per endpoint

Medium-value subsystem rewrite, natural part of the encoder rewrite.
Reported by the line-kernels hunter.

`dedup_parallel_segments` (`line_merge.rs`) allocates a reversed copy of
every segment and sometimes a forward copy, keeping full coordinate vectors
as hash keys; endpoint construction allocates a `Vec<SegEnd>` per endpoint;
decoding clears a `Vec<Vec<IntPoint>>`, dropping inner allocations; chain
output allocates again. Use a flat coordinate slab with segment ranges and
orientation flags, hash canonical oriented spans without reversed copies
(exact compare on collision), store endpoint degree and first two incidences
inline (higher degree stops traversal), and traverse spans straight into
output. The degree-2 traversal itself is right. Merging must stay tile-local
and after quantization. The witness checker is compiled out of release, but
witness and chain-boundary records are still populated in release; compiling
that bookkeeping out is a smaller local win.

---

# The polygon engine

## PERF-025 - Pyramid descent copies whole fragments at every level

Rewrite; the pyramid hunter's first priority, and the overlay hunter's second
(batched rectangle clipping) is the same design from the engine side.

Mechanism. Each child scans the parent's shapes and computes their bboxes
independently; a contained child still copies every coordinate into a new
owned shape; a crossing child goes to `clip_shape_rect_fast`, which clones the
outer, runs four successive half-plane passes materializing chains, endpoint
lists, partner and visited arrays and rebuilt rings, repeats for each hole and
reassigns holes, and may abandon all of it for the boolean fallback;
emission copies the fragment into zoom coordinates, simplifies, normalizes
and encodes, then discards the simplified result while descendants continue
from the unsimplified base fragment. Every cut yields another
`Vec<Vec<Vec<IntPoint>>>`. The topology interface clears and uploads the
subject per call (`intersect_rect_into`, `overlay_into_nested`); its retained
buffers reuse storage, not a prepared subject. Pooling cuts allocation, not
these scans and copies.

Redesign. An immutable normalized source boundary: shared coordinate chains
with explicit generated intersection vertices, ring and component identity,
junction info, cached bounds, and explicit empty, full and boundary-bearing
coverage. Cell fragments are chain references, cut endpoints and clip-boundary
connections; a contained child is another reference; only affected chains
get new storage; hole ownership follows the representation. Batched clip of a
prepared parent against its four buffered child rectangles in one operation,
classifying parent edges against all cut lines, sharing untouched runs,
returning separate (overlapping) outputs per label, never a union of the four.

Payoff. Fewer coordinate copies per level, shared sibling preparation, less
reconstruction of unaffected holes, earlier full/empty classification, lower
live memory.

Risks. Clipping is path-dependent: generated integer intersections take part
in later cuts, so ancestry must be kept as a cut recipe where it affects
output; do not clip every descendant directly from source. `crossing_point`
rounds differently from the overlay solver and the overlay snaps and repairs
iteratively, so swapping the fast clipper for an exact rectangle overlay is
not byte-preserving; the batch must reproduce today's half-plane sequence,
rounding, fallback decisions and result order per child. A cell with no local
boundary still needs a reliable fill classification (a hole can sit wholly
inside it). Normalization cannot be skipped just because the source was
normalized once (PERF-026).

Local steps (pyramid and ocean hunters): test the child ownership filter
before clipping it (descent, parallel split and root bisection all build
geometry that is then rejected); cache bounds and vertex counts; encode from
global-coordinate ring views plus origin instead of `tile_points`; run the
ring-cap bisection's two rectangle intersections as one shared cut (keep the
longer-axis split, midpoint, traversal order and zero area threshold; only
matters above 500 contours).

## PERF-026 - Normalization rebuilds topology that is already known

Rewrite spanning engine and caller; the overlay hunter's first priority.
Reported by the overlay, pyramid and ocean hunters.

The single-contour fast path builds segments, sorts, merges duplicates, runs
intersection finding and checks repeated vertices (pairwise below 64
vertices, copy-and-sort above) even when the answer is "unchanged"
(`overlay/port/simplify.rs` `find_intersections`). Below 4,000 segments the
split strategy is a nested candidate scan (`list_split`), quadratic when
bounding ranges overlap with no crossings. Multi-contour shapes have no
unchanged path: `normalize_into` runs fill, graph construction, extraction and
hole binding. `root_fragments` normalizes the source and `emit_cell`
normalizes again after rescale and simplify unless the convex single-ring
bypass applies. At OSM z14 rescale is identity and tolerance and min area are
zero, yet emission copies and normalizes again. Ocean union and exact clip
outputs are normalized, and `root_fragments` normalizes them again.

Redesign. A polygon representation carrying ring roles, containment and
contacts, whether boundaries are fully noded, stable edge ids and spatial
index, which vertices and edges a transformation changed, and what is needed
to reproduce output traversal. DP and VW decisions stay; they report what they
changed; validate against retained topology and run arrangement construction
only when repair is needed. Small single contours get a direct exact
classifier; larger ones an indexed classifier; conservative rejection falls
into the existing algorithm. For multiple rings, recognize an unchanged simple
outer with strictly contained, mutually disjoint holes. The caller passes
topology and mutation info instead of bare `Vec<Vec<Point>>`. Local step
(pyramid and ocean hunters): carry normalization provenance and consume the
terminal fragment at the last level instead of copying it.

Risks. "Valid" does not mean "return verbatim": the perfect single-ring path
keeps its start vertex while rebuilt output follows graph traversal order;
multi-ring rebuild sets hole order and rotation; collinear cleanup and area
filtering must match. Quantization, DP and VW can create contacts, collapse
channels or change containment, so exact post-transformation validation is
needed, not an unchecked validity flag; touching boundaries keep today's
non-OGC traversal.

## PERF-027 - One large arrangement runs on one core

Engine rewrite. Reported by the overlay hunter.

`union_pieces` (`ocean/emit.rs`) submits one contour soup to one normalizer,
and the large splitter builds spatial fragment groups but `process`
(`overlay/port/split.rs`) handles every group sequentially into one marks
vector. Keep the global snap-pass sequence and parallelize inside it:
candidate pairs in parallel, crossings with the current arithmetic,
orientation and snap state, deterministic merge-sort-dedup of marks, apply
splits, advance snap state as today; parallel large sorts and segment
reconstruction with prefix counts and disjoint output ranges. Upstream
i_overlay has a `parallel_split` showing parallel discovery plus deterministic
reduction. Later, replace column-replicated fragments (long segments copied
into many columns, pairs reaching the solver through several groups) with an
adaptive index with explicit pair ownership. Risks: a dense group straggles;
per-task marks raise peak memory; fill and extraction stay serial. Unioning
batches and then unioning results is a different algorithm (intermediate
snapping changes later intersections), so not equivalent. Benefit is limited
to large arrangements actually executed (ocean-build, artifact-absent runs,
large relations).

## PERF-028 - Large polygons stay one serial unit of work

Scheduling rewrite with payoff in long tails. Reported by the pyramid and
relations hunters.

`root_fragments` materializes all roots before descent; `split_for_parallel`
expands fragments and emits expanded cells synchronously; ocean preparation
collects all prepared items before processing; OSM polygon descent stays in
the worker handling that way or relation. `split_for_parallel` and
`emit_shape_pyramid_cell` exist and ocean uses them; relation emission does
not. Use a bounded queue of polygon-region jobs over shared geometry
(PERF-025): roots yield jobs incrementally, workers spill expensive
descendants as jobs and keep small ones, full coverage yields PERF-013
descriptors. Task-local output must avoid a shared mutable sink; jobs bounded
by retained geometry and estimated work. Determinism holds through the sort's
total order. Does not parallelize one enormous normalization. The relations
hunter's emission handoff: an immutable prepared polygon per component and
world copy (wrapped source geometry where labels or measurements need it, one
checked integer base shape, aligned pins, bounds and cached measurements, the
layer-specific requests), built once and borrowed by every polygon emission
instead of cloning and unwrapping rings per match and component
(`relations.rs`) and requantizing per match (`emit.rs`); oriented float rings
do not let integer orientation checks be skipped. The pyramid hunter's
version of the same sharing: prepare wrapped, quantized source geometry once
across compatible layer matches, sharing emitted geometry only where the full
policy (root zoom, caps, area, tolerance) matches.

## PERF-029 - Overlay backend keeps nine representations of one boundary

Representation rewrite, high for memory, medium for wall time. Reported by
the overlay hunter.

A general call holds segments and sort scratch, spatial fragments and split
marks, fill bytes, links with endpoint coordinates and node ids, endpoint
records and their sort scratch, CSR adjacency, a walked contour buffer, copied
contour vectors and shape shells, and a second segment set for hole binding
(`fill.rs` `GraphBuilder`, `graph.rs`, `extract.rs` `extract_into`). Use
compact vertex storage, indexed boundary edges and a contour arena with
ranges; extraction writes into the arena, hole binding attaches contour ids,
collinear cleanup runs after binding; the pyramid consumes ranges and views.
Use retained input connectivity for successors where unambiguous; angular
routing only at real junctions. Hole ownership must still be decided on raw
noded geometry before collinear removal; extraction order must be explicit,
not arena allocation order; a general half-edge structure could cost more
memory than today.

Local steps: fuse fill consumption with boundary selection via the existing
`FillHandler` (extraction re-evaluates the inclusion predicate on filtered
links; keep angular order at equal origins); size the single-contour buffer
to the contour, not the whole graph's visited-link count; size-class the
contour pool instead of two linear scans in `take_vec_with_capacity`; stop
`fragment_split` allocating `FragmentBuffer::empty()` each pass. Workspace
reservation defects are in the bug document.

---

# Ocean

## PERF-030 - The world artifact does not carry what the band needs

Rewrite; high for the architecture, medium for replacing the low union
exactly. Reported by the ocean hunter.

The artifact serves finished interior tiles but the band goes back to the
shapefiles. The record filter checks source bboxes at the pass's first zoom
(z0 low, z8 high, `band.rs`), so for a regional extract the low pass's z0 root
is computed-owned and every overlapping record survives, and high-pass
accepted records are parsed and clipped whole before deeper traversal finds
the narrow band. Put a versioned auxiliary representation inside the
explicitly named artifact (no sibling-file discovery, no CLI change): source
rings grouped, projected and quantized on their canonical grids, spatial
indexes over records, components and coastline edges, prepared low-grid
topology with source-cell seams resolved, and lineage for clipping, feature
identity and order. The band queries it per computed-owned cell; production
can happen as assembly requests each partition. Removes recurring shapefile
decode, projection, quantization, coarse scanning and possibly low-grid
dissolution. Limits: low-zoom band cells can cover most of the extract, so
broad queries must be efficient; cropping finished world tiles is not
equivalent (today's order is quantize, extract clip, low union, tile build,
simplify, area filter). Moving low union before extract clipping needs
equivalence evidence; keeping original contributions indexed lets hard cases
be reproduced. The artifact directory flattening is in PERF-016.

## PERF-031 - Ocean records go through the generic sort

Rewrite; strongest for artifact-absent runs and `ocean-build`. Reported by
the ocean hunter; overlaps PERF-013 and backlog item 19.

Beyond the full-fill expansion (PERF-013), each ocean polygon becomes a
generic sort payload via `ocean_sink`. The high pass collects every parsed
piece, then every prepared frontier, then another work vector before
processing, keeping all source geometry alive (`ocean/emit.rs`). Replace with
a producer owned by output partition: the partition gets its prepared source
geometry, partial cells produce deterministic ocean features or encoded layer
fields, full subtrees produce spans referencing a canonical full-water layer,
assembly combines them with its PBF tile stream, ocean-only spans go to
`add_run` and mixed tiles append the ocean layer before their single
compression. A bounded producer assigns deterministic piece identities through
an ordered coordinator without retaining all geometry. Writing a temporary
ocean archive and rereading it keeps the seam. Requests to siblings: the
pyramid yields full subtrees symbolically, takes owned normalized fragments,
generates tasks lazily with ownership checks before clipping; assembly gets a
partition-local ocean provider returning sparse layer data and runs.
Correctness: gather all contributions per tile, keep separate ocean features
(`LayerBuilder::add_feature` appends them) and full-fill multiplicity, follow
the payload-byte order. When both grids use the same full shapefile, decode
once and build both grids from original coordinates (deriving z7 from z14
coordinates double-rounds).

## PERF-032 - Ocean shapefiles are hashed two or three times per run

Focused ownership change. Reported by the ocean hunter.

`resolve_ocean` hashes the named shapefile pairs at entry (`key.rs`), then each
checked pass reopens its source and hashes the whole mapping again
(`emit.rs`): distinct full and simplified sources twice each, a full-only
setup three times, more if roles alias one file. Open each distinct file pair
once, hash once, compare, and have consumers read that generation (pbfhogg's
`PinnedInput` and `ElementReader::open_with` show the model); provenance keeps
roles distinct. Hash with bounded buffers: `Shapefile::hashes` touches the
whole `.shp` mapping, which stays resident while parsed geometry accumulates.
An arbitrary mutable file cannot be validated without reading it; skipping the
last scan needs sources published as trusted immutable generations bound to
their XXH128 identity, and a metadata cache is not equivalent validation. This
cost and the artifact directory load sit outside `ocean_ms` (admission precedes
the ocean section; the directory load is between `SORT_END` and
`ASSEMBLE_START`), so phase timings omit them. Local step: `read_ring_points`
(`shapefile.rs`) allocates a fresh 64 KiB buffer per ring; read records or
bounded windows instead (backlog item 8).

## PERF-033 - The low-zoom union dissolves source-cell seams from scratch every time

Algorithm rewrite, medium conviction. Reported by the ocean and overlay
hunters.

`union_pieces` gathers every contour into one `Shape` and runs serial NonZero
normalization; root construction then copies and normalizes each result again.
Source-cell boundaries are rediscovered and dissolved globally on each run.
The ocean hunter: spatially organized mosaic topology that reconciles shared
chains, dissolves artificial seams, keeps component and hole relations and
answers spatial queries; built once into the artifact (PERF-030), or in
bounded spatial groups with deterministic boundary reconciliation for full
computation. The overlay hunter: a specialized mosaic assembler that keeps
piece identity and source-border relations, normalizes local defects, splits
differently segmented shared borders into common intervals, cancels interior
intervals and stitches the surviving boundary in parallel, judged against the
parallel general engine (PERF-027). Both note that `SplitSolver::split_segments`
already sorts and merges coincident (including opposite) segments, so a
cancellation table feeding the same global machinery would duplicate work; the
redesign must avoid global arrangement, reuse topology across queries, or
shrink the edge set. Risks: partial shared chains, different segmentation,
integer snapping, point contacts, holes crossing cells, contour order;
dissolving arbitrary batches and descending them separately reintroduces the
seams. Replacing low topology may change contour decomposition or feature
identity at equal coverage, which is observable and needs separate
adjudication.

---

# Labels

## PERF-034 - Boundary-label relations pay for polygon machinery they do not use

Targeted algorithm changes. Reported by the relations hunter.

Admin boundary relations often need assembled geometry only for
`boundary_labels`; their lines emit from member ways. `process_prepared_relation_into`
builds the shared-vertex pin map (scanning all member vertices into
relation-wide hash tables, `emit.rs`) before checking that assembly produced
anything and whether any polygon output needs pins; the relation plan should
name its consumers. Surface-point aggregation (`multipolygon.rs`) computes a
surface point for every component then picks the largest successful one;
compute areas first, visit in descending area (original order on ties), stop
at the first success. In `surface.rs`, every outer scanline interval rescans
and sorts every hole's intersections and interval subtraction reallocates
segment lists; compute each ring's intersections once per scanline, combine
hole intervals once and subtract with an ordered sweep, keeping scanlines,
candidate validation and ties so label coordinates do not change. Payoff
depends on how much of the relation tail labels occupy.

---

# Small local changes

## PERF-035 - Node emission still boxes every record

Local. Reported by the emission and sort hunters. Production node processing
(`phase12/nodes.rs`) allocates a boxed `SortRecord` payload per record, drains
the boxes into `RecordSink` and frees them, and builds point scratch per
matched node; pass the arena sink directly and keep scratch in the worker. The
emission hunter also notes the encoder appends command words one at a time
while the decoder bulk-copies.

## PERF-036 - Node iteration decodes dense-node metadata nobody reads

Local. Reported by the phase12-actors, node-way-resolution and
shortbread-matching hunters. Node workers use `for_each_element`, whose dense
iterator advances metadata elivagar never consumes; pbfhogg has
`elements_skip_metadata`. The shortbread hunter warns that the two pbfhogg
iteration APIs visit plain and dense nodes in different orders within a group,
and the sorted store requires increasing ids, so switching needs that settled
first (see the bug document).

## PERF-037 - Node cache swaps 2 KiB entries by value

Local, potentially substantial on the raw path. Reported by the
node-way-resolution hunter. `CacheEntry` holds an inline 2,048-byte coordinate
array and promotion and replacement swap whole entries (`node_index.rs`),
despite comments describing pointer swaps; the swap-to-front policy also is
not true LRU (replacement moves the previous front into the next eviction
slot). Keep recency in small slot indices. Separately,
`count_bits_before` sums bytes; word-wise rank with small prefixes is
cheaper.

## PERF-038 - Remaining copies in way processing

Local. Reported by the node-way-resolution and phase12-actors hunters.
Inline `resolved_node_refs` need not clone the plan's refs; raw resolution can
build positional pins with the coordinates instead of another ref vector and a
rescan; relation-only ways can move their coordinate storage instead of
cloning before learning they emit nothing; skip building `needed_ways` when
injected membership makes it unused.
