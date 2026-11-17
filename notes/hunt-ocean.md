The biggest opportunity is a rewrite: make ocean a prepared spatial source feeding partition-owned tile production, and keep full-water regions symbolic until the PMTiles writer.

The current structure repeatedly expands compact information into expensive representations: named sources become repeated whole-file hashes, the band becomes a whole-pass geometry collection, source cells become a global contour soup, and full-water subtrees become individual geometry records that eventually deduplicate back into runs.

These are my priorities, based on code structure rather than measurements. I made no changes and ran no builds, tests, or benchmarks.

| Priority | Opportunity | Kind | Conviction |
|---|---|---|---|
| 1 | Put indexed boundary-generation data in the world artifact | Architectural rewrite | High for the architecture; medium for replacing the low union exactly |
| 2 | Produce ocean by tile partition, with symbolic full-water spans | Architectural rewrite | High |
| 3 | Validate each distinct source once and retain its identity through consumption | Focused ownership redesign | High |
| 4 | Replace global low-zoom contour-soup processing with reusable mosaic topology | Algorithm rewrite | Medium |

**1. The artifact should contain the prepared source needed to compute the band.**

Today, the artifact accelerates finished interior tiles, while boundary computation returns to the original shapefiles.

The record filter does not make that computation proportional to the final band. It examines source bboxes at the pass's first zoom: z0 for low, z8 for high. For a regional extract, the low pass's z0 root is computed-owned, so every overlapping record survives. The high pass rejects interior z8 root ranges, but accepted records are still parsed and clipped whole before deeper traversal discovers the narrower band. See [band.rs:217](/home/folk/Programs/elivagar/crates/lib/src/ocean/band.rs:217) and [emit.rs:387](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:387).

The stronger design is a self-contained artifact containing both its existing PMTiles ocean tiles and a versioned auxiliary spatial representation for boundary generation:

- Source rings already grouped, projected, and quantized on their canonical grids.
- Spatial indexes over records, components, and coastline edges.
- Prepared low-grid topology with artificial source-cell seams resolved.
- Enough source identity and lineage to reproduce clipping, feature identity, and deterministic output.

This should live inside the explicitly named artifact. There is no reason to introduce sibling-file discovery or change the CLI's explicit-or-unset behavior. Its existing tile contract and source key can remain intact.

Once the extract bounds are known, ocean asks this representation for the geometry needed by computed-owned cells. Full interior cells come from the finished artifact; partial cells query prepared topology. Geometry production can happen as assembly requests each partition.

That removes recurring shapefile decoding, projection, quantization, coarse source scanning, and potentially low-grid dissolution. It moves work into the amortized producer where it belongs.

There are two important limits.

First, low-zoom band work is not necessarily perimeter-only. A shallow computed-owned tile can cover most or all of the extract. The index must support those broad queries efficiently; it cannot assume every request is a thin strip.

Second, **cropping finished world tiles is not an equivalent implementation**. The current order is source quantization, extract clipping, low union, tile construction, simplification, and area filtering. Cropping an already simplified world tile changes that order. Slivers, disconnected components, and simplification near the extract boundary can differ.

Prepared unsimplified source geometry is the high-confidence part. Moving low union before extract clipping is a separate geometric question: union and intersection commute over exact sets, but the implementation uses integer rounding, cleanup, and ring lineage. That transformation needs equivalence evidence. Keeping indexed original source contributions in the prepared representation gives the redesign a way to reproduce difficult boundary cases without returning to shapefile parsing.

The artifact consumer also materializes too much. [OceanArtifact::into_tiles](/home/folk/Programs/elivagar/crates/lib/src/ocean/artifact.rs:235) reads every world directory run, and [read_all_runs](/home/folk/Programs/elivagar/crates/lib/src/archive/reader.rs:352) flattens them before `TileRuns` sorts them. An extract should retain directory state for its relevant ranges, not a world-sized flat vector.

There is a correctness distinction here: lazy range reads alone would stop validating unused directories. If whole-directory rejection remains part of admission, stream that validation while retaining only relevant runs. That still removes global resident storage and sorting; eliminating the validation traversal requires a trustworthy immutable-generation mechanism.

**2. Ocean should reach tiles directly, with full-water spans carried through the pipeline.**

This is the strongest rewrite for artifact-absent runs and full-world computation.

The pyramid already recognizes full buffered cells. However, [emit_full_subtree](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:690) recursively visits every descendant and emits a full polygon for each one. [ocean_sink](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:674) turns each polygon into a generic sort payload. Those payloads are copied into the spill coalescer, sorted, written, merged, decoded into features, encoded, compressed, and finally deduplicated.

A single full z8 cell represents 5,461 tiles through z14. Its descendants occupy one contiguous Hilbert interval at each zoom. That information can be represented by seven spans instead of 5,461 geometry records.

The destination already understands this representation: [PmtilesWriter::add_run](/home/folk/Programs/elivagar/crates/lib/src/archive/writer.rs:916) accepts one payload covering consecutive tile IDs. The existing artifact splice also demonstrates that assembly can combine ordinary tile content with an independently supplied ocean layer.

I would replace piece-driven ocean record emission with a producer owned by output partition:

1. A partition obtains the relevant prepared source geometry.
2. Partial cells produce deterministic ocean features or encoded layer fields.
3. Full subtrees produce spans referencing a canonical full-water layer.
4. Assembly combines those results with its sorted PBF tile stream.
5. Ocean-only spans go directly to `add_run`; mixed tiles append the ocean layer before their single compression.

This removes ocean's generic external-sort round trip. Only fragments requiring actual geometry work remain expensive. Full-water payloads are encoded and compressed once per applicable encoding/compression policy.

This needs real ownership changes. Merely changing the sink to write another intermediate format leaves most of the amplification intact. Producing a temporary ocean PMTiles archive and immediately reading it back would also preserve an unnecessary seam.

Correctness requires gathering all contributions to a tile. A full-fill contribution does not authorize discarding another source piece's fragment. The current output preserves separate ocean features, and `LayerBuilder::add_feature` simply appends them. Overlapping full fills may also have observable multiplicity. Symbolic spans therefore need contribution semantics, with partial tiles and overlap changes splitting spans.

Feature ordering must follow the existing total payload order when preserving output exactly. It is not enough to sort numeric piece IDs: [record_cmp](/home/folk/Programs/elivagar/crates/lib/src/sort/key.rs:153) compares serialized payload bytes after the key.

This rewrite should also remove the current whole-pass barriers. The high pass first collects every parsed piece, then collects every prepared frontier, then builds another work vector before processing it. Source geometry remains alive throughout. The relevant barriers are at [emit.rs:387](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:387) and [emit.rs:528](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:528).

A bounded producer can parse records, assign deterministic piece identities through an ordered coordinator, and hand owned geometry to downstream tasks immediately. Deterministic identity needs ordering and counts; it does not require retaining all geometry.

For the pyramid sibling, I would request three concrete capabilities:

- Yield a full subtree as a symbolic result.
- Transfer owned, already normalized fragments into traversal.
- Generate further tasks lazily, checking ownership before constructing rejected children.

For assemble, I would request a partition-local ocean provider returning sparse layer data and runs alongside the PBF merge stream. Those are requirements of this redesign, not a general API exercise.

**3. Source validation should happen once per distinct input generation.**

This is smaller than the two rewrites above, but it directly attacks a cost paid on every artifact-backed run.

At entry, `resolve_ocean` obtains the expected key by hashing the named shapefile pairs. Later, each checked pass opens its source and hashes the entire mapping again before parsing. See [key.rs:197](/home/folk/Programs/elivagar/crates/lib/src/ocean/key.rs:197) and [emit.rs:350](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:350).

For a normal complete run with a nonempty band:

- Distinct full and simplified sources are each hashed twice.
- Without a simplified source, the full pair is hashed three times: entry, low pass, high pass.
- Explicitly aliasing source roles can cause further redundant hashing because identity is organized around roles rather than distinct opened files.

The stronger ownership model is one retained source generation per distinct file pair. Open it once, hash it once, compare the resulting key, and make consumers read that same generation. Source roles still remain distinct in provenance even when they share storage.

The vendored PBF reader already contains a useful implementation model: [PinnedInput](/home/folk/Programs/elivagar/research/pbfhogg/src/read/input.rs:218) retains file identity across opens, and [ElementReader::open_with](/home/folk/Programs/elivagar/research/pbfhogg/src/read/reader.rs:1257) accepts it. Ocean does not need a general abstraction exercise first; it needs the same ownership guarantee for its own sources.

A retained descriptor protects against pathname replacement. It does not make an in-place-mutated file immutable. That distinction matters, but the current shapefile mapping already assumes its backing inode is not rewritten or truncated.

Hashing should use bounded buffers rather than making the entire mapping resident. Currently, `Shapefile::hashes` touches the whole `.shp` mapping, which remains present while parsed integer geometry accumulates. This combines file-backed residency with live geometry at precisely the wrong point.

There is a hard limit: **an arbitrary mutable file cannot be content-validated without reading its contents**. An inode, size, or timestamp cache does not preserve the artifact-key guarantee.

Eliminating that last scan requires sources published as trustworthy immutable generations, with a previously verified binding to their existing XXH128 identities. That is a legitimate end-to-end redesign. Without it, keep one full validation scan and remove the duplicates. Do not describe a metadata cache as equivalent validation.

This cost also sits outside `ocean_ms`: entry-time admission precedes the measured ocean section. The full artifact-directory load occurs after `SORT_END` and before `ASSEMBLE_START`. Phase timings alone therefore omit material parts of the recurring ocean cost.

**4. The low union should become prepared topology, rather than another global contour soup.**

[union_pieces](/home/folk/Programs/elivagar/crates/lib/src/ocean/emit.rs:154) gathers every contour into one `Shape`, then performs serial NonZero normalization. After that, pyramid root construction copies and normalizes each resulting shape again.

The architectural problem is larger than that repeated normalization. Source-cell boundaries are being rediscovered and dissolved from scratch, globally, every time. The representation preserves the source's arbitrary splitting until the most expensive point.

I would replace this with spatially organized mosaic topology: reconcile shared chains, dissolve artificial seams, retain component and hole relationships, and expose geometry by spatial query. For artifact-backed runs, build it once in the artifact. For full computation, construct it in bounded spatial groups with deterministic boundary reconciliation.

The payoff would be fewer simultaneously live representations, no giant soup barrier, and less work in general intersection and fill machinery.

However, this is a medium-confidence algorithm rewrite, not an obvious edge-hashing optimization. The overlay engine already sorts and merges coincident segments before intersection processing in [SplitSolver::split_segments](/home/folk/Programs/elivagar/crates/lib/src/geometry/overlay/port/split.rs:40). A new cancellation table that merely feeds the surviving edges back into the same global machinery could duplicate existing work.

To earn its complexity, the redesign must avoid global arrangement construction, reuse topology across queries, or substantially reduce the materialized edge set.

Partial shared chains, differing segmentation, integer snapping, touching components, and hole reconstruction are the risks. Dissolving arbitrary batches and descending them independently would reintroduce the very source-cell boundaries that union removes.

I would retain the z7 and z14 grids as separate geometric products. They use different quantization and may use different source files. What should disappear is the requirement that each product owns a separate complete read, collection, preparation, and emission lifecycle.

When both products use the same full source, decode it once and construct both grids directly from the original coordinates. Deriving z7 by rounding already quantized z14 coordinates introduces a double-rounding change.

Several local improvements are worthwhile, but secondary to those rewrites:

- **Enforce output budgets inside emission.** The 4 MiB threshold is checked after `split_for_parallel` or an entire work item returns. One item can emit far beyond it. Collected preparation results also retain many residual accumulators simultaneously. This is a real memory-bound weakness, not merely an inefficient constant.
- **Read records or bounded windows, rather than allocate and pread per ring.** [read_ring_points](/home/folk/Programs/elivagar/crates/lib/src/ocean/shapefile.rs:439) allocates a fresh 64 KiB buffer for every ring. Record-level buffering can remove allocations and small reads while preserving the existing checks.
- **Skip redundant normalization for proven normalized inputs.** Union and rectangular intersection already produce normalized shapes; [root_fragments](/home/folk/Programs/elivagar/crates/lib/src/geometry/pyramid.rs:274) normalizes them again. Raw quantized inputs still require normalization.
- **Reject unowned cells before clipping them.** Both descent and parallel splitting construct child fragments before the child's ownership check. Root bisection likewise constructs roots that later pruning may reject.

Two correctness observations deserve explicit attention.

The low-union implementation relies on winding correctness that the reader does not establish: it groups rings by orientation and record order, without proving hole containment. Also, its comment makes disjoint interiors sound necessary for NonZero composition to equal union. Correctly nested, consistently oriented polygons can overlap safely; the actual danger is contributions with negative winding. I found no evidence that the shipped sources violate the needed condition, but a specialized dissolver should establish that condition rather than inherit the comment's stronger assumption.

Outside ocean, PBF identity is also split across independently opened paths: the background hash, relation prepass, main reader, and assembly's provenance-header read. A pathname replacement can make those observations describe different generations. The vendored pinning support is relevant here too. The PBF header bbox is not a safe shortcut for starting ocean early: current bounds include inline way locations and antimeridian handling, as [CoordinateExtent](/home/folk/Programs/elivagar/crates/lib/src/pipeline/phase12/extent.rs:93) requires.

The storage, scheduling, identity, and symbolic-span changes can preserve decoded tile contents exactly. Replacing low topology may change contour decomposition or feature identities even when water coverage stays equal. Those are observable changes and must be declared and adjudicated separately. The strongest performance case does not require merging ocean features or changing simplification policy.
