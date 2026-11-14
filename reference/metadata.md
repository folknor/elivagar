# PMTiles archive metadata: the `elivagar` provenance block

Ordinary extract archives carry their run contract as a top-level `elivagar`
member of the PMTiles metadata JSON. The world-ocean artifact instead carries
its source and policy identity in the separate `ocean_artifact` member; it has
no input PBF or run provenance.

Read it with `scripts/dump-pmtiles-metadata.py <archive> --key elivagar`.
`elivagar inspect` renders the input/config contract and the available build,
effective-path and resume diagnostics. It uses the provenance extractor to
distinguish absent, invalid, incomplete and unsupported-schema blocks, and
reports metadata read failures separately.

## Why it exists

A prose rule already said not to compare across dataset variants.
`reference/technical-implementation-spec.md` states it plainly, and names the
2026-07-09 false alarm that "burned a session tail proving the code innocent".

On 2026-07-14 the same thing happened again. A denmark archive blessed from a
`--variant locations` build was compared against a default-variant (raw) build.
`brokkr regress` reported 363,620 structural moves across every layer, which
was read as a code regression and investigated as one, at length, in the wrong
subsystem. The two builds were both correct.

The rule was five days old and written down. It failed twice because it asks a
human to remember an invariant at exactly the moment they are focused on
something else, and because nothing in the artifacts could contradict them.

That is what this block changes. The archive states its own contract, so a
consumer can refuse an incomparable diff instead of reporting it as a finding.
A rule that only exists as prose is a rule that gets broken on a busy
afternoon; the same rule enforced against recorded facts is not.

## The split, which is the whole design

Groups get opposite treatment, and getting this backwards inverts the gate.

**`input` and `config` are the comparability contract.** They describe the work
requested. Two archives whose contract differs describe different work, so a
geometry diff between them says nothing about the code. A consumer compares
these first and refuses the diff on mismatch.

**`effective`, `build` and `execution` are diagnostic and must never be
equality-gated.** Given identical `input` and `config`, `effective` is a pure
function of the code, and `build` *is* the code. Regression testing exists to
compare revisions, so gating on either would refuse precisely the comparisons
the gate is for. Change how the pipeline selects a path and a contract-gated
consumer would refuse the diff exactly when it matters most.

These groups are not ranked by importance. `effective` is the most
*explanatory* thing in the block - it is what tells you why a diff exists once
the contract matches - and it is still not part of the gate.

## Reproducibility constraint

Nothing in this block may vary between two builds of the same commit on the
same input.

No wall-clock, no hostname, no absolute paths, no thread counts, no timings, no
measured durations, no environment-order-dependent values. Same-commit builds
are byte-identical (AGENTS.md) and the corpus digest and contract are
recomputed from archive content; a per-run value here would break both.

Anything that fails this test does not belong in the archive. It belongs in
`.brokkr/sidecar.db`, which exists for exactly that class of data.

## Labels versus identities

The governing distinction. A label describes; an identity pins.

- `input.name` is a **label**. Never gate on it. Two files named for the same
  region and built at the same commit can be entirely different contracts,
  which is how 2026-07-14 happened.
- `input.xxh3_128` is the **identity**. Gate on this.
- `input.features` are **observed**, read from the PBF header rather than
  inferred from a name, and they determine which paths the pipeline takes.
- `build.*.commit` is only an identity when `dirty` is `false`. A dirty tree
  means the commit names the nearest ancestor of the code that ran, not the
  code that ran.

The same lesson applied to dependencies. protohoggr was a path dependency
labelled `0.4.0` while a *second*, content-checksummed `0.4.0` came from the
registry via pbfhogg - two crates, one label, no assertion that they matched.
Pinning it to the registry version made the lockfile checksum its identity.

## Schema

`schema` is the version. Bump it when the meaning of an existing field changes.
Adding a member does not require a bump: readers ignore unknown members, and
the contract comparison is defined over named fields, not the whole object.

```json
{
  "elivagar": {
    "schema": 1,

    "input": {
      "name": "denmark-20260220-seq4704-locations-prepass.osm.pbf",
      "xxh3_128": "58c47f32d3a55b04a56813565efc78ac",
      "bytes": 531544047,
      "replication_timestamp": 1771622445,
      "features": {
        "sort_type_then_id": true,
        "locations_on_ways": true,
        "way_members_v1": true,
        "shared_node_pins_v1": true
      }
    },

    "config": {
      "profile": "shortbread",
      "min_zoom": 0,
      "max_zoom": 14,
      "tile": {
        "format": "mvt",
        "compression": "gzip",
        "base_compression_level": 6,
        "compression_policy": "zoom-v1"
      },
      "fanout_caps": {},
      "polygon_simplify_factor": 1.0,
      "ocean": {
        "mode": "artifact",
        "runtime_simplification": true,
        "low_zoom_source": "simplified",
        "artifact_key": { }
      }
    },

    "effective": {
      "coordinate_source": "inline",
      "way_members": "injected_v1",
      "shared_node_pins": "injected_v1"
    },

    "build": {
      "elivagar":       { "commit": "ec5bd11...", "dirty": true },
      "pbfhogg_reader": { "version": "0.5.0" },
      "cargo_lock_xxh3_128": "f850fa82608190a904500b37810e3def",
      "cargo_features": ["default"]
    },

    "execution": { "resumed_from": null }
  }
}
```

### Field notes

`input.xxh3_128` matches the `xxhash` brokkr records per dataset variant in
`brokkr.toml`, byte for byte, with no coordination between the two - the same
XXH3-128 over the same bytes. It is serialized as exactly 32 lowercase hex
characters and can be compared directly.

`config.tile.base_compression_level` is a **base**, not a uniform setting: the
encoder clamps low zooms up and caps z13/z14 down. Recording the level alone
would misdescribe the output, so `compression_policy` names and versions the
mapping from base to per-zoom level. MLT stores uncompressed payloads and
records `compression: "none"`; its tile contract omits the inapplicable base
level and compression policy. The sidecar compression counter uses 0 for
gzip, 1 for brotli, and 2 for none.

The compression policy does not enter the ocean artifact key: changing
compression effort preserves decoded MVT bytes. Artifact run copies may use
a different compression level policy; an encoded tile that takes the
artifact's ocean layer is compressed under the current policy. Decoded content changes require an ocean policy version
change.

`config.ocean.mode` is the **resolved** mode, taken from `OceanMode` rather
than inferred from which config paths are set. Those differ: the ocean phase
runs only when the full shapefile is present, so a simplified-only config
produces no ocean and correctly reports `none`. It distinguishes `artifact`
from `shapefile` and is part of the contract: artifact-served tiles are not
geometry-identical to extract-computed ones (descent seams depend on each
piece's clip extent - see the extract-consumption note in
`src/pipeline/mod.rs`). Comparing an artifact-active archive against an
artifact-absent one is a contract mismatch, not a regression.

`config.ocean.low_zoom_source` is independent of `runtime_simplification`.
Source selection (a separate simplified shapefile serving z0-7) and runtime
simplification are different things; a single boolean conflates them.
Runtime simplification is always enabled; the flag that disabled it was removed.

`config.ocean.source_key` appears only when `mode` is `shapefile`. It records
the full and optional simplified `.shp` and `.shx` hashes, the zoom span, and
`OCEAN_POLICY_VERSION`. Its XXH3-128 values use the same 32-character lowercase
hex form as `input.xxh3_128`. The simplified-input hash fields must be present,
with explicit nulls when those inputs are absent. Artifact mode keeps its existing flat
`artifact_key`, including compression level; artifact and no-ocean contracts
do not gain a `source_key` member.

`effective.shared_node_pins` is the largest of the three effective paths in
blast radius. Pin scope changes which vertices survive simplification, so
`injected_v1` and `block_local` produce materially different geometry from the
same OSM data.

`build.pbfhogg_reader` is the pbfhogg **linked as our reader**, not the
pbfhogg that produced the input PBF. Those can differ. The producer is not
recorded (see gaps); the input hash is authoritative regardless. It is a
pinned registry dependency now, so it records its locked semver `version`
rather than a `commit`/`dirty` pair: a registry dependency has no git tree to
be dirty against, and `cargo_lock_xxh3_128` already pins its exact checksum -
the version is the reader-legible half of that pin.

`build` has no protohoggr entry by design. It is a pinned registry dependency,
identified exactly by the content-addressed checksum in `cargo_lock_xxh3_128`,
and cannot drift. pbfhogg is likewise pinned there; it keeps a named entry only
to surface its human-readable version. Only path dependencies would need a
commit/dirty pair, and there are none.

`execution.resumed_from` names the phase a `--skip-to` run began at, or is
`null` for a full run.

## Feature ids and polygon label points

OSM feature ids are scoped by layer. Layers accepting more than one OSM
source type use Planetiler's convention: `id = osm_id * 10 + type_code`,
with node = 1, way = 2, relation = 3. Open and closed ways are one source
type. Decode these ids with `floor(id / 10)` for the OSM id and `id % 10`
for the type code. Single-source layers keep raw ids. Synthetic ocean ids
retain their separate existing scheme.

The mixed-source layers are `water_polygons`, `water_polygons_labels`,
`dam_polygons`, `pier_polygons`, `boundary_labels`, `land`, `sites`,
`buildings`, `addresses`, `street_polygons`, `streets_polygons_labels`,
`bridges`, `public_transport`, and `pois`. Every OSM feature in these layers
changes its numeric id.

The raw OSM id limit for mixed-source layers is 900,719,925,474,098,
`floor((Number.MAX_SAFE_INTEGER - 3) / 10)`. This shared bound keeps every
type code within JavaScript's exact integer range, at most `2^53 - 1`;
the largest encoded id is 9,007,199,254,740,983. Exceeding the raw limit
panics during emission and fails the run; ids are never rounded, truncated,
or saturated. Single-source raw ids are unchanged and are exact in
JavaScript only through `2^53 - 1`.

Tile-local merging removes ids from merged features while singletons retain
them. The namespaced singleton ids are exact MapLibre feature-state keys,
but merging can change with tile, zoom, or rebuild, so these ids do not
provide persistent identity for merged output.

`water_polygons_labels` uses a planar area centroid in normalized Mercator,
subtracting holes. `addresses` uses that centroid only for convex
polygons without holes and otherwise uses point-on-surface. Other polygon
point layers keep point-on-surface placement. Area centroids can lie outside
a polygon or inside a hole.

A relation emits one aggregate label point per layer per eligible zoom in
`water_polygons_labels`, `boundary_labels`, `addresses`,
`streets_polygons_labels`, `public_transport`, and `pois`. Its water label
centroid combines all valid components, weighted by planar area after
subtracting holes; components are aligned to the largest component's world
copy before averaging across the antimeridian. Relation addresses with one
convex component and no holes use its centroid. Other relation addresses and
the remaining four layers use point-on-surface from the largest valid
component. A relation without a usable label point emits none. Polygon
geometry continues to emit each assembled component separately.

## Checkpoint provenance

`--skip-to` reuses sort chunks from a previous run, and nothing in a later
invocation reveals what produced them. The checkpoint (JSON, written after
the PBF phase) records the input hash, producer config, effective paths, sort
chunk codec and data extent. A resume validates all except the effective paths
and extent, which it inherits. The ocean chunks' identity lives in the chunk
completion record written after the ocean phase (below). The checkpoint and
archive provenance use the same path enums.

**Input hash.** A mismatch is a hard error: resuming would mix two inputs into
one archive and stamp it with provenance describing only the second.

**Producer config.** The subset of settings that decide what phase12 writes
into chunks - zoom range, fanout caps on supported OSM polygon layers,
`polygon_simplify_factor`. Unsupported layers' caps are ignored by the library
and omitted from the contract; the CLI rejects them.
Chunks cannot be reinterpreted under different values,
so a mismatch is a hard error naming the field. It deliberately excludes the
assemble-side settings (tile format, tile compression, compression level,
memory budgets), which are applied after the chunks are read and may
legitimately change on a resume. An artifact-backed run still has to satisfy
the named artifact's tile format, compression and compression-level contract.

**Effective paths.** A matching resume inherits them rather than re-deriving,
because re-derivation would describe what *this* invocation would have done,
not what produced the chunks on disk.

**Sort chunk codec.** Recorded separately from producer config. The
`--compress-sort-chunks` setting must match, including uncompressed chunks. It
is a decoding requirement, independent of tile compression applied later
during assembly.

**Chunk completion and ocean identity.** Resuming at `sort` or `assemble`
also requires the chunk completion record, written after the ocean pass and
final chunk flush. It records the total chunk count and what produced the
ocean chunks: no ocean, computed ocean, or the boundary band of an
artifact-served run, the last two with the shapefile source key (content
hashes, zoom span, ocean policy version). A missing, unreadable, or invalid
record refuses the resume, and so does an ocean identity differing from the
run's. The artifact's compression level is not part of it: chunks hold
geometry, and artifact admission at run entry already binds the artifact to
the run's level. Resuming at `ocean` admits any ocean input, since it rebuilds
the ocean; it clears the previous record before modifying chunks and writes a
fresh one only after completion.

**Data extent.** It includes the inline coordinates of every way in
locations-on-ways mode, independently of tags. This is needed because enriched
inputs may omit untagged node elements. The checkpoint's Mercator extent
includes the ocean overlap buffer and supplies both ocean selection and the
extract archive's WGS84 header bounds and center. The world-ocean artifact
keeps world bounds. Nonempty extents use this buffer even for a single point
or a single latitude; empty input falls back to world bounds.
Header coordinates are rounded to E7; inspect decodes them
by dividing the signed integers by 10,000,000.

Both records carry the format version, and a record from any other version is
rejected; re-run a full tilegen. The version moves whenever chunks built under
unchanged recorded identity would differ.

## Where the input hash is taken

Identifying the input means reading all of it, so placement is a performance
decision, not a detail. A full run starts a background hash before phase12
and joins it after phase12, overlapping the hash with the PBF reader. Waiting
until phase12 ends to begin hashing would reread file pages that may already
have been evicted at large input sizes. A resume hashes up front because it
must validate before proceeding and skips the phase12 read entirely.

## Freshness, and why absent beats stale

Provenance that lies is worse than provenance that is absent, because it
invites a reader to attribute a diff to code that was never linked. Every value
is therefore either established or emitted as `unknown`, and nothing is
guessed. An ordinary run refuses assembly when the input cannot be identified
by filename and content hash; assembly also checks these facts defensively.
The ocean-only artifact builder has no PBF, reads no PBF header, and omits
run provenance deliberately. It retains the unchanged top-level
`ocean_artifact` identity and emits `ocean_artifact_no_run_provenance=1` on the
counter channel when measured.

The dirty flag observes the whole git status, including untracked files, so a
fixed source-file watch list cannot keep it fresh, and watching the repository
recursively would walk ignored datasets and build outputs. Rerunning
`build.rs` rebuilds the whole crate, so it reruns only when its git inputs
change: before every cargo call brokkr computes `ELV_BUILD_OBSERVATION`
(`[cargo_env]` in brokkr.toml), HEAD plus git's object id of the raw status
bytes, and `build.rs` declares `rerun-if-env-changed` on it. `build.rs`
computes the same token from the bytes it stamps from; when the variable is
absent (plain cargo, an old `brokkr --commit` worktree) or disagrees (the tree
moved mid-build, a failed read), it also watches an intentionally absent input
and so reruns on every build until the two agree - slower, never stale. Both
observers strip `GIT_*` from git's environment, so they read this checkout. The
guarantee is freshness equal to rerunning every build for any tree state that
holds from the trigger's evaluation through the build; an edit inside that
window, or git configuration visible to only one observer, is outside it.
The `build-observation-stable` script check fails the gate when the trigger
is stuck on its unique-per-call fallback. Git itself resolves
HEAD and repository metadata, including worktrees and submodules whose `.git`
is a file. Registry dependency identities come from the lockfile. A future
path dependency would need its own git inspection and commit/dirty pair.
The script writes the `build` values once, already typed, as generated Rust
that `provenance.rs`
includes, and a failed `git status` records `dirty` as unknown rather than
clean. Its rerun declarations are intended to keep build
provenance fresh; they are not independent proof of the code that ran.

`dirty` uses brokkr's definition, not git's: `*.md` and `.brokkr/results.db`
are ignored. A dirty `results.db` is this repo's expected state and markdown
never affects the binary, so counting either would mark nearly every build
dirty and make the flag meaningless. Status is read with untracked directories
expanded to files and rename detection off, so a new directory holding only
markdown is clean while a source file renamed to markdown is dirty (its
deletion counts).

## Consumer contract

A consumer comparing two archives:

1. Compare `input` and `config`. On mismatch, **refuse** the geometry
   comparison and report which field differs. Do not emit a diff.
2. Report `build` differences. Never gate on them.
3. Surface `effective` when explaining a diff that survived step 1.

An archive with no `elivagar` member may predate this schema, be a world-ocean
artifact, or come from an older producer that failed to identify its input.
It cannot pass the run provenance gate, and a consumer should say so rather
than assuming comparability.

## Known gaps

Recorded here so they are not rediscovered as bugs:

- **Enforcement landed for the standing gate only.** `brokkr pmtiles-corpus
  check` (native brokkr code since the 2026-07-24 corpus redesign, decoding
  this block through the linked crate) compares the committed `contract.json`
  against this block before reading content and refuses with the differing
  field named, including the calibrated stale-artifact mismatch at
  `config.ocean.artifact_key.policy_version`. `brokkr regress` still reads
  nothing here: an ad-hoc two-archive diff can be run across incomparable
  archives, so establish comparability with `elivagar inspect` first.
- `input.replication_sequence` is not recorded (the pbfhogg header API for it
  is unconfirmed). `replication_timestamp` is.
- `data_bounds` is not in the provenance comparability contract. It is derived
  from the input, stored in the checkpoint, and written as WGS84 header bounds
  and center. It also drives ocean selection, so it explains ocean-layer diffs
  between variants. It is **derived** and diagnostic.
- `pbf_generator` (the pbfhogg revision that produced an injected PBF) is not
  recorded. It could be stamped into the header by pbfhogg at write time.
- No `tileset_digest`. Adding provenance makes archives byte-different across
  commits even when every tile is identical; a digest over ordered tile IDs and
  uncompressed payloads, excluding metadata, would restore a cheap
  tiles-identical check. Regress already establishes tile equality per blob, so
  this is a convenience.
