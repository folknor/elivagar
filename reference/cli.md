# The elivagar CLI

The complete command-line surface. This document is the reference for what
elivagar accepts; `AGENTS.md` summarises the commands a session reaches for
most, and defers here for the full set.

Note that most work in this repo does not invoke `elivagar` with hand-written
flags. Measured runs go through the `tilegen` script bench (`brokkr bench
[--hotpath|--alloc] tilegen ...`, registered in `brokkr.d/benches.toml`),
which records results that a raw invocation does not; plain dataset builds go
through the in-tree devkit (`brokkr run --with elivagar elivagar-devkit --
tilegen ...`). Both expand the host's tilegen block into the same flags; the
bench differs only in writing its archive and `--tmp-dir` inside its
iteration directory under the host scratch. Archive
inspection is the binary itself on a path
(`brokkr run elivagar -- inspect <path>`).

## The rule that shapes this surface

**Either it is explicit, or it is not set.** Nothing is inferred from the
filesystem. A run's behaviour is a function of its arguments and its input, and
of nothing else.

This is not a style preference; it was bought with a specific failure. The
ocean inputs used to be auto-detected from `data/`: the full shapefile, the
simplified shapefile, and `data/ocean-tiles.pmtiles`. Two consequences
followed, and both bit.

The run's meaning lived in the filesystem rather than the invocation. Two runs
of the same binary on the same PBF produced different ocean geometry depending
on whether a file happened to exist, with nothing in the recorded command line
saying which. `.brokkr/results.db` stores the literal subprocess invocation in
`cli_args`, so no bench row could be classified after the fact as
artifact-active or computed - the comparison simply could not be made from the
record.

And absence was silent. On 2026-07-14 a denmark archive was built, verified and
blessed as the regress baseline while `data/ocean-tiles.pmtiles` was missing,
so it took the computed path throughout while every gate passed. The baseline
was wrong about the thing it was baselining, and nothing said so.

Hence: no auto-detection, `--ocean` names every ocean input, omitting it means
no ocean, and a named input that cannot be honoured is an error rather than a
fallback.

## Exit status

Every subcommand exits under one table. A code names a broad failure class,
not a recovery to run, and a class is claimed only where the code raising it
has evidence for it: a failure that has not been classified exits 2, never a
guessed class. Archive header, directory and payload reads, and the MVT
message/detail decoders, establish byte rejection through `DecodeError`.
`inspect`, `svg` and `diag` carry that evidence to exit 65; OS failures exit
74 and opaque failures stay 2. Invalid diag coordinates exit 64. Individual
feature failures in diag are printed and the report continues. Verify aborts on operational or opaque tile read failures; rejected payload
bytes remain findings in a completed report.

| code | meaning |
|---|---|
| 0 | success, including `--help` and `--version` |
| 1 | `verify` ran and its report fails (see `verify` below) |
| 2 | unclassified failure; it may follow substantial work or partial output |
| 3 | `--skip-to` resume refused: the resume records under `--tmp-dir` are absent or do not match this run |
| 4 | the named ocean artifact cannot serve this run because of its own bytes or identity |
| 64 | invalid invocation: a clap usage error, or a value or combination the library refuses before work (including environment knobs, and an artifact named for a tile contract or zoom window it cannot serve) |
| 65 | input bytes read and rejected, e.g. an unsorted PBF without an opt-in, node IDs the node store refuses (negative, not increasing under `--force-sorted`, at or above 2^40 in the compact store, over the flat index safety cap), a node block after the first way block when the node store is in use, a PBF block holding more than one element kind, malformed injected data, a malformed ocean shapefile index or selected record (in `run` and `ocean-build`), or a header that refuses `--locations-on-ways` |
| 70 | an internal invariant failed, e.g. a polygon over the 500-ring cap reaching a clip rect too small to split, which the ring-cap partition proves cannot happen |
| 74 | an I/O failure the OS reported |

A panic is outside the table: release builds use `panic = "abort"`, so the
process ends by signal. Library callers read the same class from
`PipelineError::kind()` (`ErrorKind::exit_code()` gives the code), and a
refused resume names its reason through `PipelineError::resume_refusal()`.
The public `node_index::NodeStoreError` names the refused node ID; growth
and remap failures of the flat index keep their `io::Error` and exit 74 with
an OS errno. Sort chunk and way index write failures in phase12 and the
ocean phase return their `io::Error` the same way: 74 with an OS errno, 2
for a failure the OS did not report (a short write). A chunk write failure
is reported when the phase collects its chunks, unless another failure of
the same run is returned first. A sort record that does not decode during assembly fails the
run unclassified (exit 2): the bytes were read back from chunk files, so
they cannot tell a program fault from a damaged chunk.
Chunk-store rejections, including deferred reads during assembly, carry
`ResumeRefusal::ChunkStore { kind }` on a resumed run (exit 3). The public
`sort::ChunkStoreError` and `ChunkStoreErrorKind` distinguish compression
mismatch, stale names, missing chunks, count mismatch, corruption and I/O.
The same read-back rejection during a full run stays unclassified (exit 2);
operational OS failures retain exit 74 and their causes in either case.
The `resume_refusal_*` sidecar counters are diagnostics only; their absence
means the reason is unavailable, not that there was none.

Library callers can inspect `PipelineError::artifact_refusal()` for a typed
`ArtifactRefusal`. `KeyMismatch::fields` names every differing identity field
(policy version, shapefile hashes, zooms, or compression level). Artifact
admission and directory refusals exit 4; OS read failures exit 74. Shapefile
hashing failures retain their own failure class.

## `elivagar run <INPUT> -o <OUTPUT> [OPTIONS]`

Generate a PMTiles archive from an OSM PBF.

### Input and output

| flag | meaning |
|---|---|
| `<INPUT>` | input OSM PBF (positional, required) |
| `-o`, `--output <PATH>` | output PMTiles path (required) |
| `--tmp-dir <PATH>` | sort chunks and intermediates. Default `data/tilegen_tmp` |

### Ocean

`--ocean <SPEC>` is repeatable and is the only ocean input. **Omit it entirely
for no ocean.** Each value is one of:

| spec | meaning |
|---|---|
| `z0-z14:<file.shp>` | one shapefile serves every zoom |
| `z0-z7:<file.shp>` | low-zoom shapefile (the pre-generalized one) |
| `z8-z14:<file.shp>` | full-resolution shapefile |
| `<file.pmtiles>` | precomputed world-ocean artifact |

Shapefile entries must partition z0-z14 exactly, and the only accepted
partitions are a single `z0-z14` or the `z0-z7` + `z8-z14` pair. That is not
arbitrary: the ocean plan (`ocean::plan`) has exactly two passes, z0-z7 on a
z7 grid and z8-z14 on a z14 grid, and splits nowhere else. A `z0-z5` request is rejected rather than accepted and quietly
served at z7, because the alternative is a false statement in the recorded
invocation - which is the failure this flag exists to prevent.

The two shapefiles are the OSM standard datasets from
osmdata.openstreetmap.de - same coastlines, same projection (EPSG:3857),
differing in vertex density. At low zooms, full-resolution coastline vertices
are overwhelmingly sub-pixel:
the split exists to avoid paying for detail that cannot be seen. It is an
optimization, not a correctness requirement, and `z0-z14:<full>` alone is a
legal and slower way to say the same thing - internally it still runs as
two passes, with the full shapefile serving both, so the low-zoom behavior
below and the artifact band ownership are identical under either spelling.
The provenance contract still records the spelling's roles: the pair records
`low_zoom_source: simplified` and simplified hashes even when both entries
name the same file, and `z0-z14` alone records `full` and none.

Invalid index entries and truncated shapefile records fail the run. Selected
polygon geometry must have finite coordinates and closed rings with at least
four points including closure, inside normalized Mercator `[-1, 2]` and
inside the record's declared bounds; malformed geometry fails rather than
being silently skipped. Records are selected by their declared bounds
without reading their points, so a record whose bounds understate its points
is caught only when it is selected. Null shape records are ignored. These
refusals exit 65; a shapefile read that fails to deliver its bytes, including
a short read from a file that changed after it was mapped, keeps the I/O rule
(74 with an OS errno, 2 without), and inputs replaced after source-key
resolution stay unclassified (2).

The low-zoom (z0-z7) pass unions its source pieces before pyramid descent
(introduced at `OCEAN_POLICY_VERSION` v3; the ring-cap partition below
came at v4). The osmdata shapefiles arrive pre-split into
grid cells comparable to low-zoom tile sizes; descending each cell
independently simplified shared coastlines into disagreeing shapes (seam
wedges on every cell edge) and applied the per-zoom min-area drop per cell
fragment (half-peninsula spikes and dropped slivers), which is exactly what
the corpus z2 tile showed on 2026-07-19. The union is measured per run:
stderr prints one `Union:` line (pieces/vertices in and out, duration) and
the sidecar carries `ocean_union_ns`, `ocean_union_input_pieces`,
`ocean_union_input_vertices`, `ocean_union_shapes`,
`ocean_union_output_vertices`. The z8-z14 pass keeps per-piece descent: its
seam mismatch is sub-pixel at fixed tolerances, and a world-scale union of
the full polygons would dominate `ocean-build`.

No emitted polygon exceeds MapLibre's 500-ring clamp (`OCEAN_POLICY_VERSION`
v4). classifyRings applies `EARCUT_MAX_RINGS = 500` to each classified polygon
- an outer plus its following opposite-wound holes - and silently drops all
but the 500 largest rings of any polygon past it, so three many-holed ocean
polygons were losing their smallest islands in MapLibre and nowhere else.
Emission now bisects such a shape's clip rect until every piece fits under the
cap; the halves share their integer cut coordinate, so the pieces abut exactly
under nonzero fill. The sidecar carries `ring_cap_partitions` and
`ring_cap_pieces`; the gate is `scripts/validate/ring-cap-census.mjs` at zero
over the cap. An artifact-active extract reads its interior tiles from the
artifact, so these counters cover only its boundary-band computation.
Partitioning already performed during `ocean-build` is not counted again.

**The `.pmtiles` artifact is a cache over the shapefiles, not a substitute for
them**, and is rejected on its own. Two things force this. An extract still
computes its boundary band near the bbox edge from the shapefiles and takes
only the interior from the artifact (the band is empty only at world bounds).
And the artifact's key is validated by re-hashing the shapefiles it claims to
have been built from, so both sides must be present for the check to mean
anything. Build one with `ocean-build`.

When a run computes ocean geometry, it checks the opened shapefile and index
buffers against the source key resolved before phase12, before emitting any
records from that pass. Replacing an input between key resolution and reading
it fails the run instead of recording the earlier key for different geometry.

A named artifact that cannot serve the run is an **error**. It applies only to
the exact MVT + gzip + z0-14 contract at a matching compression level, and a
mismatch on any of those four fails the run rather than quietly computing the
ocean instead. This used to print `Ocean artifact inactive` to stderr and carry
on - and brokkr reads nothing from tilegen's stderr under `--bench`, so a
measured run could switch ocean paths on four separate conditions leaving no
trace in the results row, the sidecar, or the archive. If you want the computed
path, say so by dropping the artifact from `--ocean`.

Example, the production shape:

```
--ocean z0-z7:data/simplified-water-polygons-split-3857/simplified_water_polygons.shp
--ocean z8-z14:data/water-polygons-split-3857/water_polygons.shp
--ocean data/ocean-tiles.pmtiles
```

### Pipeline

| flag | meaning |
|---|---|
| `--skip-to <ocean\|sort\|assemble>` | resume from a checkpoint |
| `--in-memory` | keep the tile blob in RAM. Faster for small extracts |
| `--compression-level <0-10>` | MVT base compression level. Default 6; library callers are also restricted to 0-10. Ignored for MLT. |
| `--tile-format <mvt\|mlt>` | payload format. Default `mvt` |
| `--tile-compression <gzip\|brotli>` | MVT only. Default `gzip`; MLT ignores this default and stores uncompressed payloads. MLT with brotli is rejected by the library. |
| `--compress-sort-chunks <lz4\|snappy>` | compress sort chunks: less disk I/O, more CPU. Wanted at planet scale, where scratch exceeds RAM |
| `-j`, `--threads <N>` | worker sizing input for Rayon, decode, phase12 and assembly; not a process-wide cap. Default: logical CPUs. Zero is rejected |

Library callers use `TilegenConfig::ocean: Option<OceanInputs>`: `full` is
required, while `simplified` and `artifact` are optional. `OceanInputs::parse_specs`
applies the same partition rule as `--ocean`; `parse_build_specs` additionally
rejects artifacts and requires ocean. No parser consults the filesystem.
`SimplifyFactor::new` checks a finite value in 0.1..=10 and preserves the
caller's f64; its default is 1. `FanoutCaps::parse_specs(default, overrides)`
applies a default only to supported layers, trims whitespace, and applies the
last override for each layer. Zero is uncapped. `FanoutCaps::set` also rejects
unsupported layers; `Layer` implements `FromStr` for wire names. Missing MLT
build support is refused by `run` before phase work or scratch mutation.

The CLI sizes Rayon from `--threads`; library callers size Rayon separately.
Phase12 derives decode workers with `max(threads / 3, 1)` and, in locations
mode, node workers with `(threads / 4).clamp(2, 6)`. Way admission allows
`threads.max(8)` blocks in flight. Assembly derives partition workers from
this value. Relation prepass, way dispatch and result drain use separate OS
threads, so these counts do not divide a single process-wide allowance.

A resume is checked against two records, each written by the producer whose
chunks it describes. The checkpoint (`checkpoint.txt`), written after the PBF
phase, records the input identity, producer config and sort chunk codec, and
every resume refuses a mismatch in any of them, so `--skip-to` cannot silently
blend two contracts into one archive. It records no ocean: the PBF phase reads
none. Inline way coordinates contribute to the
data extent in locations-on-ways mode, including ways without tags. The
`--compress-sort-chunks` setting must match the run that wrote the chunks,
including leaving compression unset on both runs.
Resume also refuses obsolete chunk filenames and incompatible partition
layouts, including chunks written with a different partition split zoom.
Regenerate those chunks with a full run; they are not converted on resume.
Checkpoints from an earlier format version are refused; re-run a full
tilegen. The version moves whenever chunks built under unchanged recorded
identity would differ, as when computed ocean moved to canonical pass grids.

Resuming at `sort` or `assemble` also requires the chunk completion record
(`chunks_complete.json`), written after the ocean phase and final chunk flush.
It carries the total chunk count and the identity of the ocean chunks: none,
computed from a shapefile source, or the boundary band of an artifact-served
run, each with the source's content hashes, zoom span and ocean policy
version. A missing, unreadable, or invalid record refuses the resume because
the chunk set may be partial, and an ocean identity that differs from this
run's refuses it because those resumes reuse the ocean chunks.

Resuming at `ocean` rebuilds the ocean, so it may name a different ocean
input than the run that built the chunks, or none. It clears any previous
completion record before changing the chunk set, so a failed ocean pass cannot
look complete, and writes a new one when the pass completes.

The ocean chunk identity excludes the compression level and the artifact's key
as a whole: chunks hold geometry, not encoded tiles, so neither changes them.
An artifact-backed run is still refused at entry unless the named artifact
matches its compression level and was built from the named shapefiles. Tile
compression is independent of the sort chunk codec, which must match as
described above. A full run clears the tmp dir and fails if it cannot, so no
earlier run's records survive beside its chunks.

### Node and way handling

| flag | meaning |
|---|---|
| `--locations-on-ways` | Assert that the PBF header declares embedded way coordinates. Header detection enables the mode automatically; this flag cannot force it on for a raw PBF |
| `--force-sorted` | use the compact node store without the PBF header flag. Errors if nodes are not monotonic |
| `--allow-unsafe-flat-index` | bypass flat-index guardrails. Expert debugging only; may cause severe IO/RSS degradation |

Locations-on-ways mode is detected from the PBF header, not the filesystem,
and is a property of the input file, which is itself named explicitly and
hashed into provenance. The optional `--locations-on-ways` flag is an
assertion: if set, the header must declare `LocationsOnWays`.

When a node store is needed, input without the `Sort.Type_then_ID` header
flag is refused at every file size unless `--force-sorted` or
`--allow-unsafe-flat-index` is explicit. `--force-sorted` selects the compact
store and checks strictly increasing node IDs, refusing the input (exit 65)
on the first that is not, and refuses node IDs at or above 2^40 (its group
directory is dense by ID); `--allow-unsafe-flat-index` selects the
flat index and bypasses its size guardrails. A small PBF can still contain
large, sparse node IDs. Either store needs every node block before the first
way block, because the store is finalized there; a later node block refuses
the input (exit 65). Inline way coordinates (`--locations-on-ways`,
including header detection) avoid the node store and this guard entirely.

Every input reads one element kind per PBF block, as `pbfhogg sort` and
pbfhogg's writer produce. A block holding nodes, ways or relations together
refuses the input (exit 65), whether its groups say so or a group hides a
second kind behind its first element; dense and plain node groups count as
one kind. A block holding no node, way or relation (empty, or changeset and
unknown groups only) is skipped. Detection is conservative where a group's
first element mislabels it: an empty dense-node group in front of ways is
refused rather than read as ways. A way that carries node refs but no
inline coordinates in a locations-on-ways input is counted with the
missing node refs, like an unresolved ref on the raw path.

### Memory budgets

| flag | default | meaning |
|---|---|---|
| `--sort-budget <SIZE>` | 1G | per-chunk sort buffer. Min 64M. Lower cuts phase12 RSS, costs merge chunks |
| `--way-budget <SIZE>` | 128M, or 4G under `--locations-on-ways` | in-flight way processing, in raw bytes (decompressed block size plus measured plan bytes). Min 1M. The count ceiling (`threads.max(8)` blocks in flight) is the primary admission control; this budget is a safety net against individually huge blocks and never binds on normal input, whatever the blob packing. See the constant's comment in `crates/lib/src/resources.rs` |
| `--dedup-cap <N>` | 1000000 when omitted; 0 is rejected | positive entry cap for the PMTiles payload dedup map. Tile content is identical under any cap; past it duplicate payloads are stored instead of referenced, costing archive bytes, and the map costs ~56 bytes/entry of RAM. Deliberately outside the provenance contract, like the budgets: it cannot change what a consumer decodes |
| `--assemble-budget <SIZE>` | 32M | tile assembly batch. Min 1M |

Sizes accept `256M`, `1G`, or raw bytes. Values whose scaled size overflows
the platform byte-size integer are rejected.

Omitting a byte budget uses its documented default. An explicit value must
meet the listed minimum, so zero is rejected. Fanout caps use a separate rule:
zero or omission means uncapped. A zero dedup cap is rejected; omit the flag to
use the default. The standalone `PmtilesWriter::set_dedup_cap(0)` API prevents
new dedup map entries. Existing entries remain eligible for lookups, so set
zero before writing to disable tile-to-tile deduplication from the start. Run
entries still share one payload across their tile range.

Library callers state the same choices in types rather than with a zero
sentinel. `TilegenConfig`'s byte budgets are `Option<ByteBudget>` and its dedup
cap is `Option<NonZeroUsize>`, with `None` selecting the default above; fanout
caps are `Option<NonZeroU32>` per layer, `None` meaning uncapped; `threads` is
a required `NonZeroUsize`, and `elivagar::resources::default_threads()` is the
CLI's default. The minimums are enforced by the library too, so a library
caller cannot run a smaller budget than the CLI accepts. The run resolves every
budget, worker count and environment knob once, in `crates/lib/src/resources.rs`, and
emits the resolved values as `res_*` sidecar counters.

### Geometry

| flag | meaning |
|---|---|
| `--fanout-cap-default <N>` | default fanout cap for OSM polygon layers that support caps. 0 or omitted = uncapped |
| `--fanout-cap <layer=N,...>` | per-layer fanout caps; takes precedence over the default. Features whose buffered bbox tile count exceeds the cap are skipped at that zoom. Layers that do not support caps are rejected |
| `--polygon-simplify-factor <F>` | simplification multiplier for polygon layers. Default 1.0 (same as lines). Higher simplifies harder, cutting sort volume; fills tolerate this better than stroked lines do |

Per-layer fanout caps are supported by `water_polygons`, `dam_polygons`,
`pier_polygons`, `land`, `sites`, `buildings`, `street_polygons`, and
`bridges`. The default applies to these layers only. For `land`, the cap
affects polygon features; line features in the same layer are unaffected.
The tile count includes the 128-unit MVT buffer (8 rendered pixels) on each
tile edge. It is a conservative upper bound from the bounding box, including
possible boundary-only contacts, rather than a count of nonempty clipped
polygons. Ocean does not support caps and is unaffected by the default.

## `elivagar ocean-build --ocean <SPEC> -o <OUTPUT>`

Build the durable world-ocean PMTiles artifact. One shot per shapefile
release, not per run.

| flag | meaning |
|---|---|
| `--ocean <SPEC>` | repeatable, same spelling as `run --ocean`, shapefiles only. Required |
| `-o`, `--output <PATH>` | output artifact path (required) |
| `--tmp-dir <PATH>` | default `data/ocean-build_tmp` |
| `--compression-level <0-10>` | default 6; the library rejects values above 10 before accessing inputs or retaining the outgoing artifact |
| `-j`, `--threads <N>` | default: logical CPUs. Zero is rejected |

The `--ocean` spelling is shared with `run` deliberately: the shapefiles and
the zoom split are part of the artifact's identity, so the key it records and
the key a `run` recomputes are derived from the same statement. An artifact is
valid only for a run naming the same shapefiles at the same compression level
over z0-14.

Rotating the artifact is an output-changing event. The corpus contract records
the artifact key, so the next `elivagar-corpus check` refuses with the
key mismatch named until the corpus is re-blessed (`brokkr run
elivagar-corpus -- bless --rotate`) from a build carrying the new artifact - and that rotation
commit is the review.

`OCEAN_POLICY_VERSION` (`crates/lib/src/ocean/policy.rs`) is the code half of the artifact key:
the shapefile hashes catch input rotations, the version stands in for "the
geometry pipeline changed". Any change that alters emitted ocean geometry or
encoding must bump it, which makes every existing artifact fail the key check
loudly instead of silently serving pre-change geometry - the 2026-07-15
stale-artifact incident (the VW landing shipped without a bump; DP-era
coastline spikes served world-wide for three days with all gates green) is
the motivating case.

Policy v6 discards a polygon if its original outer ring collapses during
rescaling, simplification or cleanup, so a surviving hole cannot become a
shell. Older artifacts must be rebuilt, followed by rotation of the
artifact-active corpus baseline. Policy v7 additionally orders rect-clip
endpoints by the clipped axis before rounding the displacement from the
smaller coordinate, so opposite traversals of shared edges no longer leave
one-unit fill gaps. This also requires an artifact rebuild and an adjudicated
corpus rotation. Policy v8 reserves `u64::MAX` for full-tile fill feature IDs,
keeping them distinct from zero-based source-piece fragment IDs while
preserving identical full-fill payloads for deduplication. Rebuild the artifact
and adjudicate the new output before rotating the artifact-active corpus.
Artifact builds hash the opened shapefile and index buffers
used by each pass, so replacing an input path during the build cannot attach
another file's hash to the geometry.

Retention: when the output path already holds an artifact, `ocean-build`
keeps the outgoing generation as
`<stem>-v<policy_version>-<key_prefix>-<build_date>.pmtiles` next to it. The
eight-character lowercase hex `key_prefix` is the start of the XXH3-128 hash
of the artifact key, including its source hashes and compression level.
Version comes from the artifact's own metadata, `unknown` if unreadable; an
unreadable key also uses `unknown` for the prefix. The date comes from the
artifact's mtime. Retention stays one generation deep: the previous retained
copy is removed after the outgoing artifact's link is created. Cleanup also
recognizes the earlier date-only name
`<stem>-v<policy_version>-<build_date>.pmtiles`; other names are left alone.
The keep is a hard link made before the build starts: a failed rebuild leaves
the active artifact unchanged, but retention cleanup has already replaced the
previous retained generation with a link to that active artifact. The active
path is never empty, and the extra ~1 GB materializes only when the finalize
rename swaps the inode out. The retained generation is the
comparand for the archive-bracketing idiom (`elivagar svg ... -o` the same tile
from each generation, diff one ROI with `scripts/validate/svg-roi.mjs`) -
without it, every policy bump destroys the only artifact of the version
it replaces.

## `elivagar inspect <FILE>`

Header, tile statistics, section layout, provenance, and metadata (layer list
with zoom ranges) for a PMTiles archive. In-session spelling: `brokkr run
elivagar -- inspect <FILE>`; `brokkr run elivagar-devkit -- archive
[--dataset D] [--variant V] [--commit H]` prints the stored archive path to
hand it. The same pairing serves `diag`, `svg` and `verify`.

The `MVT validation` line reports whether the archive's declared tile type and
compression are supported by the MVT validation and archive-view workflows.
The current supported pair is MVT with gzip. Header inspection still works
when that pair is unsupported.

An unreadable metadata block is still described in the header report, then
inspection returns its typed failure status. Unsupported tile formats remain
reportable without reading tiles.

### Provenance

The `elivagar` metadata member (`reference/metadata.md`, schema at
`crates/lib/src/provenance.rs`), contract first:

```
  Provenance:  schema 1
    Input:      north-america-seq4710-locations.osm.pbf
                xxh3 8122bcc83873ef95349e6a3522827fd9  17.8 GB
                features: sorted, locations-on-ways
    Config:     shortbread, z0-z14, polygon simplify x1
                tile: mvt gzip, base level 6, policy zoom-v1
                fanout: none
    Ocean:      artifact, low zoom simplified, simplifier on
                key: shp c10be1c7843c simplified b3417e31c287 level 6 policy 1
    Build:      elivagar b833fc8730cd, pbfhogg 0.5.0
    Effective:  coords inline, way members relation_scan, pins block_local
```

`Input` and `Config` together are the comparability contract, and the **whole**
of both is printed - not a selection. That matters: two archives differing only
in `polygon_simplify_factor` or one fanout cap would otherwise display
identically, and a reader told these lines are the contract would conclude a
geometry diff between them says something about the code. It is cheap to print
in full because the fanout map omits defaults, so it is a line at most.

`Build`, `Effective` and `Resumed` (only on a `--skip-to` run) are diagnostic.
They explain a diff once the contract matches and must never be
equality-gated - given identical input and config they are a function of the
code, and a regression gate exists to compare revisions.

For the standing gate, `elivagar-corpus check` enforces the contract
before reading content, decoding this block through the library.
`elivagar-corpus regress` does not read the block, so for an ad-hoc comparison these lines let
a human refuse; they do not refuse it. See `reference/metadata.md`.

`Ocean` answers "was this artifact-active or computed", which before this
existed could only be recovered by dumping the raw metadata - and on
2026-07-14 was not recovered at all, which is how a computed-path archive was
blessed as the baseline for an artifact-active pipeline. Its `key` line
carries the shapefile identities, which appear nowhere else in the block: an
artifact-active archive's contract is incomplete without them.

`features` reports the PBF header bits, not the filename. A name ending
`-locations-prepass` is a label and can lie; the bits decide which coordinate,
membership and pin paths the run took, and `Effective` says which it then
used. Unset features are omitted, so `features: none` means a plain PBF - but
a *missing* flag reports `unknown` rather than being read as false, since
`none` is a positive claim and must not be derived from absent data.

Commits are abbreviated to 12 characters for reading; the block stores them in
full and that remains the identity. `(dirty)` means the commit names the
nearest ancestor of the code that ran, not the code that ran.

**Every way of having no contract names itself**, because "no block",
"unreadable metadata" and "not JSON" are different facts and reporting any of
them as silence would read as nothing to report:

```
  Provenance:  absent - archive predates the elivagar metadata block
  Provenance:  unavailable - archive stores no metadata
  Provenance:  unavailable - metadata could not be read or decompressed
  Provenance:  invalid - metadata is not JSON
  Provenance:  schema 99 - this build understands 1; not interpreted
  Contract:    INCOMPLETE - block is missing input and config
```

A schema this build does not know is refused rather than summarised: a bump
means an existing field changed meaning, so interpreting it with these
meanings would report confident nonsense. Adding members does not bump, so
unknown members at a known schema are ignored silently.

For the raw block use `scripts/dump-pmtiles-metadata.py`.

## `elivagar verify <FILE> [--geometry-stats] [--unique-payloads]`

End-to-end archive validation: container integrity, metadata schema, tile
decompression, MVT payload structure, geometry commands, layer coverage, and
ocean ring self-intersection. Exit 0 on pass, 1 when the report fails;
retains the first 100 typed tile findings while counting all findings and
completing traversal. Malformed payloads fail the report. Operational tile
read failures abort with 74, opaque failures with 2. Container and metadata
rejections exit 65. A verification that cannot complete exits under the table in
"Exit status" at the top of this document.

Payload structure is checked under the strict MVT policy `tile_detail`
uses: a field MVT does not name, a duplicate singular field, a feature
without a geometry field, an unreadable tag or field, a value message that
does not decode (referenced or not) and a tag index outside the layer's
tables each fail the tile. `verify` is a check on elivagar's own output, so
a foreign archive carrying extension fields fails it by design.
Observed attribute keys must be allowed for their Shortbread layer, and
values must match the schema, including integer versus floating-point types.
Each metadata `vector_layers` entry must carry the complete canonical
`fields` map. TileJSON advertises both numeric types as `Number`.

| flag | meaning |
|---|---|
| `--geometry-stats` | per-zoom ocean statistics: ring counts, max/p99 ring vertices, consecutive duplicates, full-tile fills |
| `--unique-payloads` | validate each distinct compressed payload once, keeping addressed-tile accounting. For very large run-heavy archives such as the ocean artifact |

`verify` is a standing gate, but it is a structural one: it cannot see a
tessellation fault that decodes cleanly. That is what the earcut oracle
(`scripts/validate/earcut-oracle.mjs`) is for.

## The corpus gate and `regress` - the `elivagar-corpus` binary

The `corpus` namespace (`check`, `bless`, `render`, `render-manifest`,
`rings`, `mutate`), `regress` and `compare-tiles` are not `elivagar`
subcommands. They are the separate `elivagar-corpus` binary in
`crates/corpus`, which links the library by path and decodes archives
in-process: the canonical semantic tile hash, the digest fold, the contract
gating policy, the canonical SVG render core, the calibration instruments,
and every baseline verdict. The spelling is `brokkr run elivagar-corpus --
check|bless|render|render-manifest|rings|mutate|regress|stats|compare-tiles ...`.
`stats` prints per-zoom tile counts, distinct payloads and payload sizes
(exit 0, or 4 if unreadable). `--file` conflicts with `--commit` and
`--variant`; `regress --json` keeps stdout pure JSON. `bless` writes digest,
contract and leaves atomically through a roll-forward journal,
`<corpus>/.bless-journal`: while one exists `check` and `render-manifest`
refuse with exit 3 and the next `bless` replays it. (From the 2026-07-24 corpus redesign until the workspace split this layer
was native brokkr code, spelled `brokkr pmtiles-corpus ...`, `brokkr regress`
and `brokkr compare-tiles`; the port keeps the flags and exit codes.)
`reference/corpus.md` remains the methodology. The baseline state is
committed in this repo at `corpus/<dataset>/` + `corpus/style.toml`, read
and written only by `elivagar-corpus`, so rotations stay atomic with their
landing commits.

Plain dataset builds and the ocean artifact build likewise have an in-tree
driver, `elivagar-devkit` in `crates/devkit`, which reads brokkr.toml and its
`brokkr.d/` includes and owns the durable archive store
`data/tilegen/<dataset>-<variant>-<commit>.pmtiles`:
`brokkr run --with elivagar elivagar-devkit -- [--dry-run] tilegen|ocean-build ...`.
tilegen names the archive from the commit recorded in the archive's
provenance, so a `--with-commit` build lands under its own commit; the dry
run shows that slot as COMMIT. `archive [--dataset D] [--variant V]
[--commit H | --file P]` prints a resolved archive path, and `clean
[--archives [--keep N]]` removes `data/tilegen_tmp`, `data/corpus-calibrands`
and corpus-sort spill dirs whose owner pid is gone, optionally pruning the
store (global `--dry-run` lists only).

SIGTERM and SIGINT make `elivagar` write its hotpath or alloc report on a
signal thread and exit `128 + signo`, so a run stopped early still yields
its report.

What elivagar exposes instead is a library surface: the pmtiles reader
(`archive::Archive`, including run-level raw payload access over validated
directory runs, and the losslessly decoded `archive::Header`), the
`tile_detail` wire-order MVT
decoder with selectable unknown-field strictness (strict is the gate's mode -
foreign wire fields error; tolerant skips them, for cross-producer
comparison), typed provenance in full, and the writer's `add_run` plus the
mutate-only `set_metadata_verbatim`. The comparability *policy* went with
the gate: the provenance schema lives in the library, the decision of which
fields gate lives in `crates/corpus`.

`compare-tiles`, the lenient cross-producer sampling comparison, was
historically an elivagar cargo example and never an `elivagar` subcommand.

## `elivagar svg <FILE> -z <Z> -x <X> -y <Y> [OPTIONS]`

Render tiles as SVG. Single tile or an NxM grid.

Zooms must be in the PMTiles range 0 through 31. X and Y must be within the
zoom's tile matrix, and the whole requested grid must fit inside it. Grid
width and height must both be nonzero. The `--layers` names must match
Shortbread layer names exactly. Each layer has a fixed color across tiles,
and SVG groups use a fixed layer order with ocean painted first so it stays
below the other layers.

| flag | default | meaning |
|---|---|---|
| `-z`, `-x`, `-y` | | tile coordinates (top-left of the grid) |
| `-W`, `--width <N>` | 1 | grid width in tiles |
| `-H`, `--height <N>` | 1 | grid height in tiles |
| `-l`, `--layers <LIST>` | all | comma-separated layer filter, e.g. `ocean,boundaries` |
| `-o`, `--output <PATH>` | stdout | output path |

Polygons fill with `nonzero`, matching MapLibre; points are circles, lines are
stroked paths, background is land-coloured. Grid lines are drawn when width or
height exceeds 1. Command sequencing is not validated, so a mis-sequenced
stream still draws; a wire error (unknown command, truncated parameters)
fails the render.

## `elivagar diag <FILE> -z <Z> -x <X> -y <Y>`

Ring winding for one tile: per-ring vertex count, signed area, and direction
(CW outer, CCW hole) for every polygon feature across all layers. Prints
first/last 3 vertices for large rings, all vertices for rings of 6 or fewer.
Rings are raw (zero-area ones included). Polygons decode under the strict MVT
grammar; a feature that fails prints its decode error instead of rings.
Zooms must be 0 through 31, and X and Y must fit that zoom's tile matrix.

## Environment variables

None of these duplicates a flag or outranks one. A knob is a flag or an env
var, never both, because two ways to say one thing means the quieter one wins
silently.

They are recordable on measured runs: brokkr's `capture_env` in brokkr.toml matches `ELIVAGAR*`
and `MALLOC*` and stores what it finds with the run, so an env-configured run
can be reconstructed from the results row. That is what makes them legitimate
rather than ambient - before it, the knob below that moves NA assemble by 26s
was tunable from any shell and recorded nowhere.

| var | effect |
|---|---|
| `ELIVAGAR_ASSEMBLE_WORKERS` | positive decimal integer; assemble worker cap, default 8. Measured on NA locations: 4 workers left the writer idle 68% of assemble; 8 cut assemble 186.4 to 160.7s; 12 was WORSE at 176.1s and +3 GB RSS - encode CPU saturates near 8 |
| `ELIVAGAR_ASSEMBLE_PARK_BUDGET` | positive decimal integer; bytes of encoded-but-unwritten tiles workers may run ahead of the writer, default 2 GiB. The RAM ceiling on assemble, and the term the planet ledger's 6-9 GB assemble estimate is derived from |
| `ELIVAGAR_ASSEMBLE_SPLIT_TARGET` | positive decimal integer; piece size in record bytes for hot-partition splitting, default 64 MiB; a partition over twice this splits into contiguous tile-range pieces read by independent workers (H8b - one dense z14-block partition otherwise puts hundreds of encoded MB behind one ordered slot). Output is byte-identical split or unsplit; set a tiny value to force splits on a small extract for that gate |
| `ELIVAGAR_REL_BLOCKS_CAP` | non-negative decimal integer; relation-block buffer cap in bytes. Past it the tail re-reads relation blobs instead of holding them; zero forces the spill path |
| `ELIVAGAR_LAYER_STATS=1` | per-layer per-zoom sort-stats firehose (~800 counters). Emitted at end of run, so it never affects timing. The per-layer totals are always emitted |
| `BROKKR_MARKER_FIFO` | set by brokkr. Phase markers and counters go here; unset means every counter in `crates/lib/src/debug.rs` is a silent no-op |

For the four integer knobs above, only an unset variable selects the default.
Set values must contain only ASCII decimal digits and fit the target integer:
`ELIVAGAR_ASSEMBLE_SPLIT_TARGET` uses `u64`; the other three use `usize`.
Empty, malformed, non-UTF-8 or overflowing values fail the run instead of
falling back. Signs, whitespace and size suffixes are rejected: `1G` fails;
write `1073741824` for 1 GiB. Zero fails for the three assemble knobs.
All four are read once when the run starts, with the budgets, so a malformed
value fails the run even when the phase that would use it is skipped by
`--skip-to`.

`BROKKR_MARKER_FIFO` is why a bare `elivagar run` emits no metrics at all: the
sidecar numbers exist only for runs brokkr started with a measurement flag.

The harness cannot express "allow brokkr with any env assignments", so
`VAR=x brokkr ...` is blocked in-session; `scripts/envrun.sh` exists only to
carry that, and gates nothing.

## What is not on this surface, and why

The `corpus` and `regress` subcommands were removed on 2026-07-24 by the
corpus redesign - see the `elivagar-corpus` section above. The gate machinery
was development tooling in an end-user binary; it now lives in its own
workspace binary, which links the library.

`--no-ocean`, `--no-ocean-simplify`, `--ocean-simplified` and `--ocean-tiles`
were removed on 2026-07-14 along with the auto-detection.

`--no-ocean` existed only to defeat the auto-detection, so it died with it:
omitting `--ocean` now means what `--no-ocean` used to.

`--ocean-simplified` and `--ocean-tiles` folded into `--ocean`'s zoom-range
spelling. Both named an ocean input; there is now one flag for that, and its
value states the coverage it claims rather than leaving it to the flag's name.

`--no-ocean-simplify` skipped the ocean VW simplifier to emit verbatim polygons
for an `ocean-coverage` baseline. It was a production-binary flag whose only
purpose was producing deliberately non-production tiles for a diagnostic that
had already been demoted from gate to triage. The `ocean-coverage` subcommand
followed it out on 2026-07-24: with no way left to produce a verbatim
baseline it was unfeedable, and its triage role is covered baseline-free by
`scripts/validate/zoom-overlay.mjs`, `svg-roi.mjs`, and `regress --overlay`
(a confirmed false negative on the 2026-07-12 VW landing is why it could
never be a gate; the authoritative ocean gates remain the earcut oracle and
the human visual check). `provenance.rs` keeps the
`runtime_simplification` field, invariably true on archives built after the
removal and possibly false on older ones - dropping the field would make those
archives incomparable to these, which is the opposite of what the block is for.
