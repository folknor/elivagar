# Spec A: the corpus digest gate

Implementation specification for tier 1 of the SVG regression corpus: a
git-committed, exhaustive, categorical content digest over every tile of a
dataset build, checked against any explicitly named PMTiles archive.

Written against `reference/technical-implementation-spec.md` (the contract
this document must satisfy). Spawned from `notes/svg-corpus-plan.md` (the
hardened plan; its "Spec A - digest gate" item and the tier-1 "open from
review" block are the originating TODO). The teardown of the bless machinery
is spec C, the render corpus is spec B; neither is in scope here, and until
spec C lands `brokkr regress` against the blessed archive remains THE standing
gate. Everything this spec builds is advisory until the calibration readings
in landing 4 are recorded.

## Resolutions of the open review items

The plan left five tier-1 questions open for this spec. Each is resolved
here; the reasoning is in the body.

1. **Granularity: committed exact leaf hashes for denmark, a fixed
   z7-ancestor bucket grid for planet-scale datasets.** One digest
   definition, two committed representations (`mode leaves` /
   `mode buckets`), chosen per dataset by measured file size against an
   explicit threshold. The three irreconcilable descriptors in the old draft
   ("z7 partition granularity", "~hundreds of tiles", "tens of KB") are
   replaced: denmark localizes to the exact tile run (leaves), planet
   localizes to a (zoom, z7-cell) bucket of at most 4^(z-7) tiles and hands
   attribution to tier 3. The `PARTITION_SPLIT_Z` -reuse rationale is
   dropped; z7 is chosen because the fixed global z7 grid (16,384 cells) is
   the coarsest grid whose full per-zoom expansion (136,533 bucket rows,
   ~10-11 MB text) is committable at planet scale, not because sort.rs uses it.
2. **The deduped leaf representation is adopted, measured, not refused.**
   The 21 MB rejection premise was wrong; the measured denmark archive
   (survey below) has 225,129 directory entries covering 1,296,999 addressed
   tiles, so a canonical run-level leaf file is ~11 MB of text, and it is
   committed. This directly closes the R2 rotation-reviewability hole for
   the standing-gate dataset: a rotation's git diff names every changed
   tile run.
3. **Planet-scale sizing is stated with arithmetic** (sizing section):
   planet leaves ~3.5-4 GB, uncommittable, therefore bucket mode; planet
   bucket file ~10-11 MB, committable.
4. **The semantic surface is pinned and mutation-tested.** The streaming
   hash is extended to cover the MVT layer version (field 15) and made
   strict on unknown wire fields, the detail decoder mirrors both so the
   two continue to induce the same equivalence relation, and a table-driven
   mutation suite covers every claimed component in both directions.
5. **Calibration uses a same-contract known-good/known-bad pair** built by
   a new `corpus mutate` instrument (direct tile mutation of a fresh clean
   build), so the actual `check` path - contract guard included - is
   exercised. The preserved incident archive
   `data/tilegen/denmark-bc71cf1.pmtiles` (confirmed on disk 2026-07-15)
   calibrates the contract guard's FIRES direction; it cannot calibrate the
   digest because the guard correctly refuses it first.

One deliberate deviation from the plan's lettering: the contract file is
`contract.json`, not `contract.toml`. The contract already exists as JSON
(the `elivagar` provenance member, `src/provenance.rs`); re-encoding it as
TOML invents a second schema and an unrepresentable-null problem
(`ocean.artifact_key` is null for computed-ocean archives, and TOML has no
null). Canonical sorted-key pretty JSON is exactly as git-diffable.

## Survey of the ground

### Measured input (2026-07-15, this spec's survey)

`brokkr pmtiles-inspect --file data/blessed/denmark-d8b5147.pmtiles`:

- 1,296,999 tiles addressed; 166,365 unique payloads (87.2% deduplicated);
  **225,129 directory entries**; 360.3 MB file; z0-14; MVT + gzip.
- Provenance: schema 1, input `denmark-20260220-seq4704-locations-prepass`
  xxh3 `58c47f32d3a55b04a56813565efc78ac`, ocean artifact-active with key
  `shp c10be1c7843c simplified b3417e31c287 level 6 policy 2`, build
  `elivagar 420534ea953b (dirty)`.
- The dirty build flag confirms the plan's claim: the current blessed
  archive cannot be reproduced from source, so the corpus baseline MUST
  come from a fresh clean build, never from promoting this file.

On-disk comparands (confirmed via `print -l data/tilegen/*.pmtiles`):
`denmark-bc71cf1.pmtiles` (the stale-artifact incident archive, ocean
policy 1 - the contract-guard calibrand), plus denmark archives at
`420534e`, `bdf87fc`, `d8b5147`, `eb1e36e`. These live in gitignored
`data/`; the bc71cf1 file's survival until landing 4 is a prerequisite.

Planet-scale reference numbers (`notes/planet-30gb-roadmap.md`): NA
101.8M addressed tiles with 20.3M directory entries (ratio ~0.20 entries
per tile), NA 18.0M unique tiles, planet unique tiles estimated 60-70M,
world ocean artifact 212.4M addressed / 9.2M unique.

### Code the spec builds on

- `src/regress.rs` (3280 lines) contains tier 1's primitive, currently
  private:
  - `streaming_tile_hash` (line ~689) -> sorted-by-name layer hashes under
    domain `elivagar-stream-tile-v1`; `streaming_layer_hash` (~721) hashes
    MVT layer fields 1-5 (name, features, keys, values, extent) and SKIPS
    field 15 (version) - the R2 finding; `streaming_feature_hash` (~782)
    covers id presence/value, geom type, sorted bit-exact attrs
    (`DetailAttr` hashed via `to_le_bytes`, so float bit patterns
    including -0.0 and NaN payloads are covered), geometry;
    `multiset_hash` (~854) is the order-insensitive combine (sorted child
    digests, not additive); `streaming_geometry_hash` retains ring point
    order and rotation, multisets components.
  - `semantic_hash(raw, &mut DecodeScratch)` (~679): gzip decompress +
    streaming hash - the per-blob entry point `fingerprint_blobs` (~622)
    already parallelizes with rayon exactly the way corpus compute will.
  - `archive_runs` / `next_zoom_boundary` (~376/~439): directory runs and
    zoom-boundary splitting.
  - `HashSink`/`CanonSink`/`write_string`/`write_len` (~1905-1997), the
    detail decoder (`decode_detail_tile` ~1319, `DetailTile`/`DetailLayer`
    /`DetailFeature`/`DetailAttr` ~1180-1230, `decode_detail_attr[s]`).
  - The comment at ~708 pins the invariant this spec must preserve: the
    streaming hash and the detail decoder induce the same equivalence
    relation.
  - Protobuf primitives (`Cursor`, `WIRE_LEN`, `WIRE_VARINT`) come from the
    `protohoggr` crate, so extraction carries no parser with it.
- `src/pmtiles_reader.rs`: `ArchiveView::open/read_all_runs/raw_blob/
  num_addressed/tile_type/tile_compression/metadata` - everything compute
  and the contract reader need; `RawDirEntry {tile_id, run_length, offset,
  length}`; `BlobRef`.
- `src/provenance.rs`: `SCHEMA_VERSION = 1`, `Input` (doc comment: `name`
  must never decide comparability), `OceanContract` (artifact key inside
  `config.ocean.artifact_key`), `config_json`, and `producer_config_diff`
  (~314) - an existing JSON field-path diff walker to generalize for the
  contract guard.
- `src/inspect.rs`: `MetadataState` and `print_provenance` enumerate every
  way of having no contract (absent / unavailable / invalid / unknown
  schema / incomplete). The corpus contract reader must produce the same
  taxonomy as values, not prose.
- `src/pmtiles_writer.rs`: `add_run(tile_id, run_length, data)` accepts
  already-gzip-compressed payloads in tile_id order with dedup -
  `corpus mutate`'s rewrite loop; `write_to` composes metadata via
  `build_metadata` (~1125), so mutate needs a verbatim-metadata bypass
  (new brick). `tile_id_to_zxy` (~1259) and `xy_to_tile_id` (~1246) are
  public.
- `src/main.rs`: clap `Command` enum (~34) - `Corpus(CorpusArgs)` slots in
  beside `Regress`.
- Datasets and the tilegen contract: `brokkr.toml` `[plantasjen.tilegen
  .default]` is artifact-active; blessed discipline says locations variant.

### What is NOT here

No rendering, no style, no manifest, no overlay emitter (spec B). No
removal of `brokkr bless` / `brokkr regress` / `data/blessed/` /
`datasets.<D>.blessed`, no gate-reference rewiring in AGENTS.md or
reference/performance.md's discipline sections (spec C). No brokkr-repo
changes: `elivagar corpus` is invoked directly, like `elivagar verify`;
`brokkr corpus` wrappers are spec C's cross-repo step 1.

## The target, concretely

### On-disk layout (committed)

```
corpus/
  denmark/
    contract.json      the comparability contract, written at bless time
    digest             small human summary: mode, root, per-zoom rollups
    leaves             mode leaves only: one line per canonical tile run
```

One directory per dataset. Only denmark is created by this spec.

### The digest definition (representation-independent)

All hashes are XXH3-128, lowercase 32-hex in files, with versioned domain
strings. The digest is a pure function of the mapping
`tile_id -> semantic tile content` for every addressed tile:

- **pair hash** = `xxh3_128("elivagar-corpus-pair-v1" || tile_id_le_u64 ||
  semantic_hash_le_u128)` - binds addressing to content, so a readdressed
  tile fails even when its payload exists elsewhere.
- **zoom hash** (one per populated zoom z) = `multiset_hash(b"corpus-zoom",
  pair hashes of every addressed tile at z)`.
- **bucket hash** (mode buckets) = `multiset_hash(b"corpus-bucket", pair
  hashes of every addressed tile at zoom z whose ancestor at
  zoom min(z, 7) is cell c)` - key (z, c). For z <= 7 this is per-tile.
- **root hash** = `HashSink` over `b"corpus-root-v1"`, the zoom count, then
  each `(z_u8, zoom_hash)` in ascending z.
- **bucket root hash** (mode buckets only) = `HashSink` over
  `b"corpus-bucket-root-v1"`, the bucket-row count, then each
  `(z_u8, cell_u64, bucket_hash)` in ascending (z, cell). The plain root is
  computed identically in both modes, but in bucket mode it does NOT
  determine the bucket rows: `multiset_hash` is non-homomorphic, so bucket
  hashes cannot be recombined into zoom hashes (R1/R2). A bucket-mode
  baseline therefore commits this second root over the bucket rows, without
  which a bucket line hand-edited or merge-damaged after bless is undetectable
  - and bucket mode is exactly the planet-scale mode whose diffs are already
  opaque, so it needs the STRONGER integrity guard, not the weaker one.

`semantic_hash` is the (extended, v2 - see below) streaming tile hash:
invariant to gzip bytes/level, layer order, feature order, attr order,
key/value table permutation, and multi-geometry component order; sensitive
to everything else in the decoded tile including layer version, extent,
float bit patterns, ring point order and rotation, and ring order within a
component. Deduplication is exploited exactly as `fingerprint_blobs` does:
one decode per unique `BlobRef` (166,365 for denmark, not 1,296,999) - but
that BlobRef count is a decode-loop optimization, NOT a committed number.
The `unique` field in `digest` is the count of DISTINCT SEMANTIC HASHES,
which is a pure function of the tile_id -> semantic-content mapping. Raw
BlobRef count is a writer/dedup/gzip artifact: two byte-different blobs can
share a semantic hash and one canonical build can dedup differently from
another while staying semantically identical, so committing the BlobRef
count would let a semantically-neutral rebuild rewrite the `digest` file
and break the byte-equality verdict (R2). Every committed count - `tiles`,
`entries`, `unique` - is therefore a function of the semantic map alone;
the 166,365 shown in the survey is the BlobRef figure and the committed
`unique` is computed and recorded at landing 2.

### File formats (exact)

All files LF-only, ASCII, single-space separated, written via temp file +
rename. `digest`:

```
elivagar-corpus-digest v1
mode leaves
root 9f0c...32hex
tiles 1296999 entries 225129 unique 166365
zoom 0 tiles 1 hash <32hex>
zoom 1 tiles 4 hash <32hex>
...
zoom 14 tiles <n> hash <32hex>
```

In `mode buckets` the header additionally carries a `broot <32hex>` line
(the bucket root, immediately after `root`), and the zoom lines are followed
by bucket lines, sorted by (z, cell tile_id), populated cells only:

```
broot 4a71...32hex
...
bucket z=9 cell=7/66/38 tiles=58 hash <32hex>
```

`leaves` (mode leaves only): header line `elivagar-corpus-leaves v1`, then
one line per **canonical run**, sorted by tile_id:

```
<z> <x> <y> <run_length> <semantic_hash_32hex>
```

A canonical run is a maximal span of consecutive tile_ids within one zoom
sharing one semantic hash. It is derived from directory runs by splitting
at zoom boundaries (`next_zoom_boundary`) and then merging adjacent runs
whose semantic hashes are equal. This makes the leaves file a pure function
of the tile_id -> semantic-content mapping: a writer-side dedup or
run-splitting change CANNOT rewrite a single leaf line, so leaf-file byte
equality is itself a semantic verdict, and a rotation's git diff over
`leaves` names exactly the changed tile runs and nothing else. z/x/y is the
run's first tile.

`contract.json`: canonical JSON (keys sorted recursively, 2-space indent,
trailing LF) of

```json
{
  "schema": 1,
  "input": { ...the archive provenance "input" subtree verbatim... },
  "config": { ...the archive provenance "config" subtree verbatim... },
  "build": { ...the archive provenance "build" subtree verbatim: elivagar
             and pbfhogg_reader, each with commit + dirty... }
}
```

`config.ocean.artifact_key` (shapefile hashes, level, OCEAN_POLICY_VERSION)
rides inside `config`, so the ocean key is gated without a separate field.
The `build` subtree is committed so the baseline NAMES the exact producer
commits (both repos) that made it - closing the R2 gap that the corpus
stored no producer commits at all - but it is DIAGNOSTIC and never gated
(see contract semantics). The style hash slot is spec B's addition; its
absence here is deliberate.

### Contract semantics

- **Gated:** the whole of `config` by value, and the whole of `input` by
  value EXCEPT `input.name` - the provenance doc comment is explicit that
  the name is a human label that must never decide comparability. A name
  mismatch prints a warning line, never a refusal.
- **Never gated:** the whole of `build`. Comparing archives built from
  different revisions is the entire point of a gate, so `contract_diff`
  ignores the `build` subtree exactly as it ignores `input.name`; it is
  committed for diagnosis and read by bless's door check alone.
- **Refused with the mismatching JSON paths named** (generalizing
  `producer_config_diff`): any other difference. Never a silent
  cross-contract diff - this is the dm6-vs-plantasjen and policy-1-vs-2
  guard, and it runs BEFORE any content comparison.
- **Every way of having no contract names itself**, with the same taxonomy
  as inspect: metadata absent / unreadable / not JSON / no elivagar member /
  unknown schema / incomplete (missing input or config). All refuse.
- A config-schema evolution (a new knob appearing in `config`) is a
  contract mismatch by construction and forces a rotation commit; when the
  knob is output-neutral that commit shows contract.json changing while
  digest and leaves do not - which is itself the proof of neutrality, so
  the strictness is a feature, not friction to engineer away.

### Semantic surface v2 (the R2 layer-version fix)

Landing 1 changes the shared canonical machinery so the literal claim "any
semantic output change fails" holds over the full MVT wire surface:

- `streaming_layer_hash` additionally reads field 15 (varint, the layer
  version; default 1 when absent, per vector_tile.proto) and hashes it,
  under the bumped domain `elivagar-stream-layer-v2`.
- Unknown fields become hard errors instead of skips, at all FOUR message
  levels (tile, layer, feature, AND the nested `Value` message), naming the
  field number and level. The original R2 finding is that `decode_detail_attr`
  (the shared Value decoder, regress.rs ~1404) skips unknown Value fields, so
  a three-level fix would leave the claim "any semantic output change fails"
  false inside `Value`; v2 makes the Value decoder strict too. Our encoder
  emits only known fields, so on elivagar archives this is a no-op; on a
  corrupted or foreign archive it is a named refusal instead of a silent
  hash over a partial read. `regress` inherits the strictness (it shares
  the decoder); regress is documented as an elivagar-archive tool and
  `compare-tiles` remains the cross-producer instrument, so this is
  accepted and noted in cli.md.
- Repeated packed fields are CONCATENATED, not overwritten. MVT feature
  `tags` (field 2) and `geometry` (field 4) are packed-repeated and a
  conformant producer MAY split either across multiple wire occurrences;
  both the streaming decoder (regress.rs ~804/~820, `attrs =`/`geometry =
  Some(...)`) and the detail decoder currently ASSIGN on each occurrence,
  keeping only the last and silently dropping the earlier parameters. v2
  appends successive occurrences at both decoders so the split form hashes
  identically to the single-run form and no parameters are lost. Without
  this the claimed "full MVT wire surface" is overstated (R2): a
  split-packed archive would mis-hash. On elivagar archives (one occurrence
  of each field) it is a no-op; it closes the claim rather than narrowing it.
- The detail decoder mirrors both: `DetailLayer` gains `version: u32`
  (default 1), `compare_detail_tiles` reports a version mismatch as a
  layer-level difference, and `decode_detail_tile` gets the same strict
  unknown-field errors. This preserves the load-bearing invariant that the
  streaming hash and the detail decoder induce the same equivalence
  relation (regress.rs ~708) - without it, regress's canonical tier and
  detail tier would disagree about version-differing tiles.

The claimed surface, exhaustively: tile addressing (pair hash), layer set
and names, layer version, extent, feature id presence and value, geometry
type, attributes bit-exactly (all seven `DetailAttr` kinds), geometry
vertices, ring point order and rotation, ring order within a component,
ring roles (via geometry structure). The accepted (absorbed) reorderings,
exhaustively: gzip bytes, layer order, feature order, attr pair order,
key/value table permutation with remapped indices, component order within
multi-geometries, and a producer splitting packed field 2 (tags) or field 4
(geometry) across multiple wire occurrences (concatenated on decode).
Anything not in either list (archive metadata JSON,
header bounds/center, directory run structure, internal compression) is
explicitly OUTSIDE the digest: metadata format/compression are gated by the
contract, container integrity stays `elivagar verify`'s job, and the claim
in cli.md is worded to say exactly this.

### Module and type layout

**New module `src/mvt_canon.rs`** (pub(crate)): the canonical-form
machinery moves here from regress.rs, unchanged except the v2 extensions -
`DetailAttr`, `decode_detail_attr[s]`, `detail_attrs_hash`, `HashSink`,
`CanonSink`, `write_string`/`write_len`, `unzigzag`/`unzigzag64`, the
five `streaming_*` functions, `multiset_hash`, `DecodeScratch` +
`gzip_decompress_into`, `semantic_hash`. regress.rs imports them; nothing
about regress's three-pass engine, report, or detail comparison moves.
This is a structural cut, not reuse-for-its-own-sake: two consumers
(regress, corpus) of one canonical-form definition, and the mutation suite
lives beside the definition it pins.

**New module `src/corpus.rs`** (pub):

```rust
pub enum DigestMode { Leaves, Buckets }

pub struct ZoomDigest { pub z: u8, pub tiles: u64, pub hash: u128 }
pub struct BucketDigest { pub z: u8, pub cell: u64, pub tiles: u64, pub hash: u128 }
pub struct Digest {
    pub mode: DigestMode,
    pub root: u128,
    pub tiles: u64,
    pub entries: u64,   // canonical run count (pure function of the semantic map)
    pub unique: u64,    // count of DISTINCT semantic hashes, NOT raw BlobRef count
    pub zooms: Vec<ZoomDigest>,
    pub buckets: Vec<BucketDigest>, // empty in Leaves mode
}
pub struct LeafRun { pub tile_id: u64, pub run_length: u32, pub hash: u128 }

pub fn compute(archive: &ArchiveView, mode: DigestMode)
    -> io::Result<(Digest, Vec<LeafRun>)>;

pub struct CheckReport { /* per-zoom added/removed/changed counts,
    changed leaf runs or buckets, contract diff, refusal reason */ }
pub enum CorpusVerdict { Pass, ContentMismatch, Refused }

pub fn check(archive: &Path, corpus_dir: &Path) -> io::Result<(CorpusVerdict, CheckReport)>;
pub fn bless(archive: &Path, corpus_dir: &Path, mode: DigestMode, rotate: bool)
    -> io::Result<(CorpusVerdict, CheckReport)>;  // Refused unless clean or rotate
```

plus private parse/serialize for the three files. `compute` mirrors
`fingerprint_blobs`: collect unique `BlobRef`s from `read_all_runs`,
par-decode to semantic hashes, expand runs (zoom-split, then equal-hash
merge) into `LeafRun`s, then fold pair hashes into zoom/bucket/root. The
`unique` count is the cardinality of the set of distinct semantic hashes
seen across addressed tiles (a `FxHashSet<u128>` fold), never the BlobRef
map length.

**`src/provenance.rs` additions:**

```rust
pub enum ContractState {
    Contract(ContractDoc),
    Absent, Unavailable(String), Invalid, UnknownSchema(u64),
    Incomplete(&'static str),
}
pub struct ContractDoc {
    pub input: serde_json::Value,
    pub config: serde_json::Value,
    pub build: serde_json::Value,    // the WHOLE provenance "build" subtree
                                     // verbatim: elivagar AND the path-dep
                                     // pbfhogg_reader, each carrying commit +
                                     // dirty (src/provenance.rs). Diagnostic,
                                     // NEVER gated - comparing two revisions
                                     // is the entire point of a gate. Read
                                     // only by bless's door check, which
                                     // inspects both repos' dirty flags.
}
pub fn extract_contract(metadata_json: &str) -> ContractState;
pub fn contract_diff(baseline: &ContractDoc, candidate: &ContractDoc) -> Vec<String>;
    // gated JSON paths under `config` and `input`; `input.name` and the
    // entire `build` subtree are excluded (name -> separate warning, build
    // -> ignored, since comparing revisions is the point of a gate)
```

`inspect.rs` keeps its print path untouched (it prints diagnostics the
contract reader deliberately drops); only the member-location and schema
check logic is shared if the extraction falls out naturally, otherwise
duplication of ~20 lines is accepted over entangling a printer with a gate.

**`src/pmtiles_writer.rs` addition:** `pub fn set_metadata_verbatim(&mut
self, json: String)` - `write_to` emits this string (gzip-compressed) as
the metadata section instead of composing `build_metadata`. Used only by
mutate; documented as such.

**CLI (`src/main.rs`):**

```
elivagar corpus check <archive.pmtiles> --corpus <dir>
elivagar corpus bless <archive.pmtiles> --corpus <dir> [--mode leaves|buckets] [--rotate]
elivagar corpus mutate <in.pmtiles> -o <out.pmtiles> [--tile <z/x/y>]
                        --op drop-tile|nudge-geometry|layer-version|regzip
```

Exit codes: 0 pass / blessed clean; 1 content mismatch (check), or any
detected difference at bless without `--rotate` (content OR contract - an
unadjudicated rotation); 2 refusal. A contract difference is asymmetric
between the two commands, and this is the R2 contradiction resolved: in
`check` a gated contract diff is a REFUSAL (exit 2, before any content
comparison); in `bless` the same diff against the committed baseline is the
ROTATION TRIGGER (exit 1 without `--rotate`, written with it), never exit 2.
bless is the only path that may legitimately cross a contract boundary, so
it must not inherit check's refuse-before-content guard. The remaining exit-2
refusals apply to BOTH commands: no interpretable contract on the candidate
archive, a non-MVT or non-gzip archive, missing or internally inconsistent
corpus files, and (bless only) a dirty-build candidate. Every refusal prints
its reason; nothing is inferred from the filesystem - the archive path and
the corpus dir are both explicit, per the CLI rule.

### Command flows (every brick visible)

`check`:
1. Read corpus dir: `digest` + `contract.json` required, `leaves` required
   iff mode leaves. Missing/unparseable -> exit 2, named.
2. Baseline self-consistency (both modes): recompute the plain `root` from
   the committed `zoom` lines and, in mode buckets, recompute `broot` from
   the committed `bucket` lines; either mismatch with `digest` -> exit 2
   "baseline internally inconsistent". In mode leaves additionally recompute
   the zoom hashes AND root from the committed leaves (the strongest check,
   since leaves determine everything). Catches hand-edits and merge damage in
   microseconds - and closes the R1/R2 gap that left bucket-mode baselines,
   whose diffs are opaque, with no internal integrity guard at all.
3. Open archive; guard MVT + gzip (header and metadata contract); extract
   contract; `contract_diff` vs contract.json -> any gated diff: exit 2
   listing paths; name-only diff: warning.
4. `compute`; PASS requires `root` equal in mode leaves, and BOTH `root`
   and `broot` equal in mode buckets. Root alone catches any single changed
   tile (it flips one bucket hash and the containing zoom hash), but it does
   NOT certify that a committed bucket row was not damaged after bless -
   `broot` closes that (R2). On PASS print `corpus check: PASS` with
   tile/unique counts and wall time, exit 0.
5. Else print per-zoom tile-count deltas (derivable in both modes: `zoom`
   lines store the per-zoom tile count, so net add/remove and a changed-hash
   flag come straight from the recomputed vs committed zoom rows). Mode
   leaves: the changed/added/removed canonical runs as `z x y n old->new`
   lines, capped at 100 per class with totals - EXACT per-tile naming. Mode
   buckets: changed buckets as `z cell-z/x/y` lines with the per-bucket net
   tile-count delta (from the stored `tiles=` field), same cap. A bucket
   hash is a multiset digest, so it CANNOT name individual tiles or split a
   same-count in-place change into per-tile counts (R2); bucket-mode
   diagnostics are restricted to facts derivable from the stored rows and
   hand exact attribution to tier 3. Exit 1.

`bless`:
1. Steps 3-4 of check for the candidate (contract extracted, format
   guarded). Additionally read the whole `build` subtree and refuse if
   EITHER repo's `dirty` is `true` - `build.elivagar.dirty` OR
   `build.pbfhogg_reader.dirty`. A baseline must be reproducible from source,
   and pbfhogg is a path dependency that drifts independently, so a clean
   elivagar over a dirty pbfhogg is still irreproducible (R2): the whole
   build closure must be clean, not just elivagar. This is the 07-14/d8b5147
   lesson enforced at the door. A `dirty` of `null` (git absent at build
   time) means reproducibility is UNPROVEN, not disproven: warn loudly,
   record the unknown, and proceed. Also refuse (exit 2) if
   `input.features.locations_on_ways` is not `true`: a blessed baseline is
   ALWAYS locations-generated (the standing bless-machinery rule), and while
   the committed contract makes a raw baseline visible in the diff, bless
   gets the same door-level refusal the dirty check has rather than relying
   on a human to notice the variant in review (R1).
2. If the corpus dir has an existing digest: run the ROTATION COMPARISON
   against it - NOT `check`. Unlike `check`, this path never refuses on a
   contract difference; it computes the contract diff AND the content deltas
   together and prints one rotation report (contract diffs named by path,
   per-zoom changed counts, changed runs capped as above). Without
   `--rotate`: exit 1, nothing written, whether the difference is contract,
   content, or both. With `--rotate`: proceed past both to step 3. This is
   the R2 hole closed at the mechanism level: bless can never silently
   convert an unexplained failure into the new baseline, yet it is the one
   command allowed to cross a contract boundary (the whole purpose of a
   rotation), so it deliberately does not reuse check's refuse-before-content
   guard. With leaves committed the subsequent git diff names every changed
   run for review - the commit IS the bless and carries the adjudication.
3. Write contract.json + digest + leaves (temp + rename; leaves removed in
   bucket mode). Print sizes (bytes per file) - this printout is the
   sizing instrument the thresholds below read.

`mutate` (calibration instrument, not a user tool - documented as such):
1. Open input, `read_all_runs`, locate the run containing the target
   tile_id. If that run has length > 1 (a deduped blob shared across a span
   of consecutive tile_ids), SPLIT the target tile_id into its own
   single-tile run so ONLY the target tile changes - the remaining span
   keeps the original shared blob untouched. This split applies to every
   payload-editing op, not just `drop-tile`; editing a shared blob in place
   would silently mutate every tile in the run and defeat the isolated-defect
   proof (R1/R2). `regzip` is the exception: it touches every tile and needs
   no split or `--tile`.
2. Apply the op (payload ops: decompress, edit, recompress gzip level 6):
   - `drop-tile`: omit the split-out target tile_id; no payload edit.
   - `nudge-geometry`: first feature with a geometry, first MoveTo command;
     DECODE its parameter (MVT geometry command parameters are
     zigzag-encoded), add 1 to the decoded x delta, RE-ENCODE (zigzag). A
     raw +1 on the encoded varint does NOT move x by one - zigzag maps
     encoded n to alternating +/- magnitudes, so it must be a decoded-delta
     mutation. Changing the decoded delta can change its zigzag varint byte
     width, which cascades through the field-4 (geometry) length prefix, the
     field-2 (feature) length prefix, and the field-3 (layer) length prefix;
     all three are recomputed on re-emit (tile-level layers are top-level
     fields, so nothing above the layer needs fixing).
   - `layer-version`: replace field 15 with 3 on the first layer. Production
     layers already carry version 2 (the encoder emits it explicitly), so
     this is a 1-byte-to-1-byte varint swap that does NOT change the layer
     length; the length-fixup code stays general but this specific op does
     not exercise a length change (the version-2-vs-3 length invariance is
     what makes it safe as the minimal op).
   - `regzip` (positive control, not a defect): decompress and recompress
     EVERY tile at gzip level 9 (the pipeline writes level 6), no payload
     edit, no split. Yields a byte-different, semantically-IDENTICAL archive
     - the known-good byte-difference calibration reading 1b needs.
3. Reconstruct the writer config from the SOURCE HEADER, not just metadata:
   `PmtilesWriter` requires a `PmtilesConfig` (`min_zoom`, `max_zoom`,
   `bounds`, `center`), and a naive rewrite would recompute those and drift,
   so mutate reads all four from the source archive header and rebuilds the
   config verbatim. Stream every run to the new writer via `add_run` in
   tile_id order (untouched runs copy raw compressed bytes), with
   `set_metadata_verbatim(original metadata JSON)` so the contract is
   preserved bit-for-bit - the whole point of the instrument is a
   same-contract known-bad (or, for `regzip`, a same-contract known-good).

## Sizing (measured base, arithmetic stated, thresholds pinned)

Denmark (from the measured 225,129 directory entries; canonical runs land
within a few percent of that - zoom splits add at most 14, equal-hash
merges subtract):

- `leaves`: ~225k lines x ~48 bytes (z/x/y up to 5+5 digits, run length,
  32-hex hash) ~= **11 MB** raw text; the 16-byte/run hash entropy bounds
  the git-packed size at ~4-7 MB per rotation.
- `digest`: header + 15 zoom lines, **< 1 KB**.
- `contract.json`: **< 2 KB**.

Planet (from NA's 0.20 entries-per-addressed-tile and the 60-70M unique /
~350-400M addressed estimates): leaves would be ~70-80M lines ~= 3.5-4 GB -
uncommittable, so planet blesses `--mode buckets`. Bucket file: fixed
population ceiling of 136,533 rows (21,845 per-tile rows for z0-7 plus
7 x 16,384 z7-cell rows for z8-14) x ~77 bytes (a full z14 line
`bucket z=14 cell=7/127/127 tiles=16384 hash <32hex>` measures ~77 bytes,
not the ~44 the first draft assumed - R2) ~= **~10-11 MB** - committable.
Localization in bucket mode is one (zoom, z7-cell) pair, i.e. at most
4^(z-7) tiles (16,384 at z14); naming individual tiles then requires tier
3 against an on-disk comparand archive. That trade is accepted and stated
here rather than hidden: exhaustive detection everywhere, exact naming
where leaves fit.

**Thresholds (proceed/close, read at landing 2):**

- Committed-leaves decision: denmark's measured `leaves` file <= 32 MB raw
  -> commit it (expected ~11 MB). Over 32 MB -> the leaf question closes
  as mispriced, denmark falls back to `--mode buckets`, and the
  rotation-naming duty moves wholly to tier 3 (recorded in this note and
  the plan).
- Cost observation (recorded, not gated): `corpus check` wall time on
  denmark, expected in the seconds range (same work shape as regress's
  canonical tier: 166k parallel blob decodes). Landing 2 records the
  actual number here and in `reference/performance.md`'s corpus section
  alongside the calibration readings.

## Landings

Four landings, in order; `brokkr check` and `elivagar verify` stay green at
every boundary. No landing changes pipeline output, so the standing gates
double as neutrality proofs. Benchmark discipline: the pipeline (tilegen)
bench is NOT owed - no landing touches a measured tilegen path, and the
zero-diff regress read at landing 1 is the neutrality gate. The one
exception is landing 3: `set_metadata_verbatim` adds a branch to
`PmtilesWriter::write_to`, which is `#[hotpath::measure]` (pmtiles_writer.rs
~568) and priced by `brokkr pmtiles-writer`, a measured path SEPARATE from
tilegen (R2). The branch is a single once-per-archive check off the per-tile
loop, but the governing contract prices any measured path, so landing 3
records a `brokkr pmtiles-writer --bench` reading against the pre-change and
post-change commit and confirms neutrality there rather than asserting an
exemption. Other new-tool costs are recorded as observations only.

### Landing 1: mvt_canon extraction + semantic surface v2

Move the canonical machinery to `src/mvt_canon.rs`; add version-field
hashing (domain bump to `elivagar-stream-layer-v2`), strict unknown-field
errors, `DetailLayer::version` + comparison; port `write_detail_*` test
helpers along. Add the mutation suite as table-driven unit tests in
mvt_canon.rs over hand-built MVT byte tiles:

MUST CHANGE the hash: layer added/removed/renamed; version 2 -> 3; absent
version vs explicit 1 stays EQUAL but absent vs 2 differs; extent 4096 ->
8192; feature id added/removed/changed; geom type change; attr key change;
attr value change for each of the seven DetailAttr kinds; float 0.0 ->
-0.0 and NaN-payload change; one vertex +1; ring rotated one step; ring
order within a component swapped; unknown field present at tile, layer,
feature, OR Value level -> ERROR (not a hash).

MUST NOT change the hash: layer reorder; feature reorder; attr pair
reorder; key/value table permuted with indices remapped; multipolygon /
multiline / multipoint component reorder; feature tags (field 2) split
across two occurrences vs one; feature geometry (field 4) split across two
occurrences vs one.

Each MUST-CHANGE case also asserts the detail decoder reports a
difference and each MUST-NOT asserts it reports none - the mirror
invariant, tested as such.

Gates (exact):
```
brokkr check
brokkr tilegen --dataset denmark --variant locations
brokkr regress --dataset denmark --file data/tilegen/denmark-<commit>.pmtiles   # zero diffs: tool neutrality
elivagar verify data/tilegen/denmark-<commit>.pmtiles  # zero errors
```
(`<commit>` = the landing-1 commit; brokkr prints the resolved output
path.) `brokkr regress` has NO `--variant` flag (verified against
`brokkr regress --help`: only `--dataset`, `--commit`, `--file`,
`--against`, `--tol`, ...), so the tech-spec's variant-pinning requirement
is met one step upstream - the `--variant locations` tilegen produces the
locations output and regress consumes THAT exact file via `--file`, against
the always-locations blessed archive. Never pass `--variant` to regress; it
is rejected. Keep if all green; revert is a mechanical move-back plus
dropping the v2 additions - no persisted state depends on the domain
strings yet.

### Landing 2: contract reader + corpus module + CLI + docs

`provenance::extract_contract`/`contract_diff`; `src/corpus.rs`
(compute/check/bless, three file formats, canonical-run derivation);
`Command::Corpus` wiring; `reference/cli.md` gains the `elivagar corpus`
section (formats, exit codes, the semantic-surface claim worded per this
spec, and the ADVISORY status until spec C's cutover). Unit tests: file
format round-trips; canonical-run merging (dedup-structure invariance:
two synthetic archives, same tile contents, different run splits ->
byte-identical leaves); contract taxonomy (each ContractState variant from
doctored metadata); bucket grouping (z<=7 per-tile, z>7 by ancestor);
baseline self-consistency refusal.

Gates (exact):
```
brokkr check
brokkr tilegen --dataset denmark --variant locations
elivagar corpus bless data/tilegen/denmark-<commit>.pmtiles --corpus corpus/denmark
elivagar corpus check data/tilegen/denmark-<commit>.pmtiles --corpus corpus/denmark   # exit 0
```
Record the printed sizes against the 32 MB threshold (the proceed/close
read) and the check wall time. The blessed files exist in the tree but the
baseline COMMIT is landing 4's, after calibration proves the tool.

### Landing 3: mutate instrument + writer verbatim-metadata

`set_metadata_verbatim`; `corpus mutate` with the four ops (three defect
ops plus the `regzip` positive control). There is no true no-op mutate mode
- an instrument that changes NOTHING invites a vacuous calibration - and
`regzip` is not one: it perturbs bytes (cmp differs) while preserving
semantics, which is exactly the known-good byte-difference control reading
1b consumes. Coverage is an integration test that builds a small archive
via PmtilesWriter and asserts, per op, contract extraction equal
before/after, plus:
- the three defect ops: digest root UNEQUAL, `check` naming exactly the
  target tile, and every NON-target semantic leaf byte-identical to the
  source (the isolated-defect proof the run-split guarantees);
- `regzip`: digest root and leaves byte-IDENTICAL, yet the archive bytes
  differ (`cmp`), and `check` exits 0.

Gates (exact; `<commit>` = the landing-2 archive brokkr printed):
```
brokkr check
brokkr pmtiles-writer --bench    # write_to is measured; confirm neutral vs the pre-change commit
elivagar corpus mutate data/tilegen/denmark-<commit>.pmtiles -o data/tilegen_tmp/denmark-mut-nudge-geometry.pmtiles --tile 5/16/9 --op nudge-geometry
elivagar verify data/tilegen_tmp/denmark-mut-nudge-geometry.pmtiles    # container stays structurally valid
```

### Landing 4: calibration + baseline commit

The oracle-discipline readings, all recorded (commands verbatim, exit
codes, named tiles) in `reference/performance.md`'s corpus-calibration
record and summarized in this note:

Placeholder resolution for the readings below: `<fresh>` is
`data/tilegen/denmark-<landing-4 commit>.pmtiles` (brokkr prints the
resolved path); `<op>` iterates the three literal defect op names
`drop-tile`, `nudge-geometry`, `layer-version`. Landing 4 adds no code (only
`corpus/denmark/` files plus performance.md), so its build is byte-for-byte
landing 3's build - that is why the same `<fresh>` archive serves the
known-good, mutation, and neutrality readings.

1. **CLEARS on known-good:** rebuild the landing-3 commit's archive from
   scratch (`brokkr clean`, then
   `brokkr tilegen --dataset denmark --variant locations`);
   `elivagar corpus check <fresh> --corpus corpus/denmark` -> exit 0.
   Additionally `cmp -s <fresh>` against the landing-2 archive - expected
   identical. Read this as a NEUTRALITY cross-check, not a determinism law:
   AGENTS.md's byte-determinism guarantee is WITHIN one commit, and landing 2
   and landing 3 are different commits, so the byte-identity relied on here
   is the spec's separate landing-to-landing output-neutrality assertion. A
   `cmp` mismatch is therefore NON-BLOCKING relative to the digest verdict -
   if landings 2->3 ever perturbed byte layout benignly while staying
   semantically neutral, reading 1b (not this cmp) is the authority, and the
   digest PASS is the gate. Record both readings and label the cmp as the
   neutrality cross-check it is.
1b. **CLEARS on a byte-DIFFERENT known-good (the real absorption proof):**
   `elivagar corpus mutate <fresh> -o data/tilegen_tmp/denmark-regzip.pmtiles --op regzip`
   then `cmp <fresh> data/tilegen_tmp/denmark-regzip.pmtiles` MUST differ
   (bytes changed) while
   `elivagar corpus check data/tilegen_tmp/denmark-regzip.pmtiles --corpus corpus/denmark`
   -> exit 0. This is the reading that would catch an accidental digest
   dependency on gzip bytes, writer dedup, or run splitting - the failure
   mode reading 1's byte-identical cmp is blind to, and the exact class of
   bug the `unique`-count fix (R2) removed. Record it.
2. **FIRES on known-bad, same contract:** for each op,
   `elivagar corpus mutate <fresh> -o data/tilegen_tmp/denmark-mut-<op>.pmtiles --tile 5/16/9 --op <op>`
   then check -> exit 1 naming z5 and the run containing 5/16/9
   (5/16/9 is a hard-tiles-ledger spike site; drop-tile additionally
   flips a tile count). Contract guard must PASS on these - that is the
   proof the content path, not the guard, produced the verdict.
3. **Contract guard FIRES:**
   `elivagar corpus check data/tilegen/denmark-bc71cf1.pmtiles --corpus corpus/denmark`
   -> exit 2 naming `config.ocean.artifact_key.policy_version` among the
   paths (policy 1 vs 2). Prerequisite: the preserved file (on disk
   today); if it has been cleaned by landing 4, the substitute is a
   doctored-contract corpus copy under `data/tilegen_tmp/` exercising the
   same guard path, and the loss of the historical calibrand is recorded.
4. **Bless-refusal readings:** bless the mutated archive over the existing
   corpus WITHOUT `--rotate` -> exit 1, nothing rewritten; bless a
   dirty-build archive (the preserved `data/blessed/denmark-d8b5147.pmtiles`)
   -> exit 2 dirty refusal. Both recorded.

Then the baseline commit: `corpus/denmark/{contract.json,digest,leaves}`
from the landing-4 fresh clean build, with the calibration record bundled
(performance.md rides along per the markdown rule). This commit is the
first corpus bless; the user adjudicates it like any bless rotation.

Keep/revert: the corpus is advisory, so every landing's revert is a plain
code revert; landing 4's revert additionally deletes `corpus/denmark/`.
Nothing else in the system consumes corpus output yet (spec C is the
consumer switch), which is precisely why the blast radius stays bounded.

## Oracle discipline compliance

Categorical: pass = byte-equal digest (and leaves; and `broot` in bucket
mode), 0 differing runs - achievable, demonstrated by calibration readings
1 and 1b. Both directions calibrated on the REAL check path with a
same-contract pair: known-good on both a byte-IDENTICAL rebuild (reading 1)
AND a byte-DIFFERENT but semantically-equal archive (reading 1b, the
absorption proof that no gzip/dedup/run-split artifact leaks into the
verdict), known-bad on the three defect ops (reading 2), plus the guard
calibrated independently in both directions (reading 3 fires; readings 1-2
double as guard-clears). No continuous quantity is thresholded anywhere in
the gate. Until all readings are recorded, corpus
check is advisory and `brokkr regress` remains the standing gate; the
promotion itself is spec C, conditional on these readings.

## Stopping rule

This spec stops at: a calibrated, advisory `elivagar corpus
check`/`bless`/`mutate` on an explicit archive path, a committed denmark
baseline under `corpus/denmark/`, the v2 semantic surface shared with
regress, the cli.md section, and the calibration record. Explicitly out of
scope: SVG rendering, styles, manifests, overlays (spec B); bless-machinery
teardown, gate rewiring, brokkr wrappers, `blessed` terminology decisions
(spec C); germany/norway/planet corpus directories (cheap to add later via
the same commands; planet additionally waits on the roadmap's planet
archive existing at all); any consumer that reads the corpus in CI or
brokkr. The digest deliberately does not cover archive metadata JSON,
header bounds, or directory structure - contract and `elivagar verify` own
those, and the cli.md wording states the boundary.
