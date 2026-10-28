# Design reconnaissance: the archive surface

Scope: `src/pmtiles_writer.rs` (+ tests), `src/pmtiles_reader.rs`,
`src/provenance.rs`, `src/tile_detail.rs`, `src/inspect.rs`, `src/verify.rs`,
`src/svg.rs`. Followed out into `pipeline/assemble.rs`, `pipeline/mod.rs`,
`ocean.rs`, `geometry/mvt_decode.rs` and `main.rs` (`diag`) where the
questions led.

Headline: this scope has no model of a PMTiles archive and no model of an
MVT tile. It has seven modules that each re-derive both from raw bytes. The
header is a `[u8; 127]` indexed by magic offsets in five files; the MVT wire
format is walked by six independent hand-written decoders with six
different error policies; the provenance block that the corpus gate treats
as a typed contract is built as typed Rust, flattened to `serde_json::Value`,
and then read back as untyped `Value` by two consumers that disagree about
what a complete contract is. The right move is a rewrite into three
layers - `archive` (container), `mvt::decode` (one decoder), `provenance`
(one typed schema with serde both ways) - with inspect/verify/svg/diag as
thin consumers. Details and evidence below.

---

## 1. Axes that should be types

### 1.1 The PMTiles header is a byte array, not a type

`PmtilesReader::header() -> &[u8; 127]`, `ArchiveView::header()`, and the
writer's `build_header` all work in raw offsets. The offsets 8/16/24/32/40/
48/56/64/72/80/88/96/97/98/99/100/101/102..127 are spelled independently in:

- `pmtiles_writer.rs` `build_header`, `write_header_counts`, `write_header_bounds`
- `pmtiles_reader.rs` `PmtilesReader::open` + ~15 accessors, and again in
  `ArchiveView::open` + its accessors (two parallel field sets)
- `inspect.rs` (`h[7]`, `h[96]`, `h[118]`, `read_i32_le(h, 102..123)`, with
  its own private `read_i32_le` copy)
- inspect tests (`set_header_u64(path, 32, 0)`)
- writer tests (own `read_u64_le` copy)

Nothing ties the writer's offsets to the reader's except round-trip tests.
Replace with one `Header` struct (`#[derive]` plain fields, `Header::encode
-> [u8;127]`, `Header::decode(&[u8;127]) -> Result<Header, HeaderError>`)
owned by an `archive` module, used by writer, both readers and inspect.

Inside it, the enumerated bytes become enums:

- `TileType { Unknown, Mvt, Png, Jpeg, Webp, Avif, Mlt? }` - today `u8`
  compared to literal `1` in `verify.rs`, `ocean.rs`, `ArchiveView::open`;
  named in `inspect::tile_type_name`; produced by
  `TileDataFormat::header_tile_type`.
- `Compression { Unknown, None, Gzip, Brotli, Zstd }` - one enum for both the
  internal and the tile compression fields. Today `u8` compared to `2` in
  `verify`, `ocean`, `ArchiveView`, `PmtilesReader::read_metadata`,
  `read_directory`; named in `inspect::compression_name`; produced by
  `TileDataCompression::header_value`.
- `bounds`/`center` as a `LngLatBbox` / `Center { lng, lat, zoom }` instead
  of `(f64, f64, f64, f64)` and `(f64, f64, u8)` tuples in `PmtilesConfig`.
  The four-tuple order (min_lon, min_lat, max_lon, max_lat) is exactly the
  sibling-swap a tuple invites. `f64_to_e7` truncates rather than rounds;
  a typed E7 conversion would own that choice once.
- `clustered: bool` (writer hardcodes `h[96]=1`, inspect reads `h[96]==1`).

### 1.2 Tile format and compression: four vocabularies, five name tables

The "what is in a tile payload" axis exists as:

1. `TilePayloadFormat { Mvt, Mlt }` + `TileCompression { Gzip, Brotli }` in
   `lib.rs` (config).
2. `TileDataFormat { Mvt, Mlt }` + `TileDataCompression { None, Gzip,
   Brotli, Zstd }` in `pmtiles_writer.rs`.
3. Header bytes (`u8`).
4. Metadata strings: `tile_payload_format`/`tile_compression` in the writer's
   metadata, `config.tile.format`/`config.tile.compression` in provenance.

Name tables: `TileDataFormat::metadata_format` ("pbf"/"mlt"),
`TileDataFormat::metadata_payload` ("mvt"/"mlt"),
`TileDataCompression::metadata_value`, `provenance::tile_format_name`,
`provenance::tile_compression_name`, `inspect::compression_name`,
`inspect::tile_type_name`, `inspect::infer_payload_format_from_header`.

The config-to-writer mapping lives in `pipeline/assemble.rs` (the match
around `set_tile_contract`), which also hard-codes the domain rule "MLT
payloads are stored uncompressed". That rule is a fact about the tile
contract, not about assembly. One `TileContract { format, compression }`
type, constructed only through a validating constructor (`Mlt` implies
`None`), with `header_tile_type()`, `header_compression()`,
`metadata_name()` methods, would delete both the second enum pair and every
name table.

Latent disagreement already present: for an MLT run, the writer stamps
`tile_compression: "none"` in the top-level metadata and header, while
`provenance::config_json` records `tile.compression` from
`config.tile_compression`, which is whatever the user passed (default
gzip). The same archive therefore claims two different tile compressions
in one metadata document. The provenance value describes the request, the
header the output; nothing says which is the contract. Worth verifying
against an MLT archive.

Also: `PmtilesWriter::add_tile` documents "`data` must already be
gzip-compressed" while the writer happily accepts a brotli or none
contract. The contract is held by the writer but the payload is not
checked against it - a typed `CompressedTile { contract, bytes }` from the
encoder would make the mismatch unconstructible.

### 1.3 Tile identity: `(u8, u32, u32)` and bare `u64`

`xy_to_tile_id(z, x, y) -> u64` and `tile_id_to_zxy(u64) -> (u8,u32,u32)`
live in the writer but are imported by verify, svg, main (`diag`), ocean,
assemble, emit, phase12 and the pipeline tests. Tile IDs travel as `u64`
next to offsets (`u64`) and lengths (`u32`) in `DirEntry`, `RawDirEntry`,
`TileEntry`, `BlobRef`, the dedup bucket tuple `(u64, u32, u64)`, and
verify's `(u64, u32, u8, bool)` key. Swapping `tile_id` and `offset` in any
of these compiles. Wanted:

- `TileId(u64)` (Hilbert, archive order) and `TileCoord { z, x, y }` with
  `From` both ways, in their own module (`tile_id.rs` / `archive::tile_id`),
  not in the writer. The writer is the wrong owner: the reader, sort
  partitioning and emit depend on it too.
- `BlobRef { offset, length }` already exists in the reader; the writer's
  `DirEntry` and dedup tuple should use it. `DirEntry`/`RawDirEntry` are
  the same struct defined twice (writer private, reader public).
- `TileEntry` is `RawDirEntry` minus `run_length`; it's `(TileId, BlobRef)`.
- `run_length == 0` meaning "leaf pointer" is a sentinel in both the writer
  and reader; a `DirEntry::{Run{..}, Leaf{..}}` enum makes it explicit.

### 1.4 Zoom range as array length `15`

`VerifyReport::addressed_per_zoom: [u64; 15]` and assemble's
`tiles_per_zoom/unique_per_zoom/bytes_per_zoom: &mut [u64; 15]`, verify
silently drops z>14 (`get_mut` returns None), assemble indexes
`[z as usize]` and would panic. `pipeline/mod.rs` hard-codes `max_zoom != 14`
for the artifact. A `Zoom` newtype bounded by a single `MAX_ZOOM` constant
plus a `PerZoom<T>` container would make the bound one decision.

### 1.5 MVT geometry type as `u8`/`u64`

`DetailFeature::geom_type: u8` (public API to brokkr), svg's `geom_type:
u64`, verify's `geom_type: u64`, `diag_geom_type -> u64`, all matched on
`1/2/3`. `crate::mvt::GeomType` already exists on the encode side; the
decode side should use it (with an `Unknown(u64)` arm for tolerant
decoding). `DetailFeature` carries both `geom_type` and per-ring
`CanonRingRole` (Point/Path/Outer/Hole) that encode the same fact twice -
a feature of `geom_type 1` with a `Path` ring is constructible.

`DetailAttr::Float(u32)` / `Double(u64)` store bits, which is right for
bit-exact hashing, but deserve a comment or a `F32Bits` newtype so a
consumer doesn't treat them as integers.

### 1.6 Errors as prose

- `tile_detail` returns `Result<_, String>` from every function. This is
  the decode surface the gate consumes; brokkr can only print it. A
  `DecodeError { context: Locus { layer, feature }, kind: DecodeErrorKind }`
  with kinds like `UnknownField { level, field }` (the one the Strict mode
  exists to produce), `Truncated`, `Utf8`, `IndexOutOfRange`,
  `CoordinateOverflow`, `BadCommand` lets brokkr distinguish "foreign
  structure" from "corrupt payload" without substring matching.
- `pmtiles_reader` uses `io::Error::other(String)` for every container
  defect (overlap, overflow, zero-offset sentinel, unsupported
  compression). The reader's own tests assert with
  `err.to_string().contains("overflow")`, which is the symptom. An
  `ArchiveError` enum (`NotPmtiles`, `UnsupportedVersion`,
  `Unsupported{field, value}`, `SectionOutOfBounds{section}`,
  `RunOverlap{..}`, `Directory(DirError)`, `Io`) belongs here.
- `verify` collects `tile_errors: Vec<String>` formatted as
  `"z{z}/{x}/{y}: [layer] feat N: message"`. Tests assert on the prefix
  string. A `TileFinding { coord, layer: Option, feature: Option, kind }`
  makes the report machine-readable (brokkr could consume verify as a
  library and gate on categories) and makes the `MAX_TILE_ERRORS` cap a
  presentation choice instead of a data loss.
- `VerifyError::{Container(String), Metadata(String)}` - same.
- `provenance::ContractState::Unavailable(String)` is never constructed in
  this crate; dead variant or brokkr-only - either way, prose.

### 1.7 Provenance: typed on the way in, `Value` on the way out

`provenance.rs` has `Input`, `PbfFeatures`, `OceanContract`, `Effective`
structs, each with a hand-written `to_json`. There is no `from_json`.
`ContractDoc { input: Value, config: Value, build: Value }` is what the
library hands brokkr - so the "typed provenance in full" claimed in
AGENTS.md is typed only on the write side. Every reader (`inspect`
`print_contract`/`print_diagnostics`/`pbf_feature_list`/`repo_display`,
brokkr's contract gate) re-learns the key names as string literals.

Stringly-typed enums inside it:

- `OceanContract::mode: &'static str` ("none"/"shapefile"/"artifact"),
  set from `run.ocean_mode` and compared to `"none"` in
  `assemble.rs`.
- `OceanContract::low_zoom_source: &'static str`
  ("simplified"/"full"/"none") - and "none" must agree with mode "none",
  which nothing enforces; an `enum OceanContract { None, Shapefile {
  low_zoom }, Artifact { low_zoom, key } }` makes the invalid pairings
  (mode none + low_zoom simplified, artifact without key) unconstructible.
- `Effective::{coordinate_source, way_members, shared_node_pins}:
  &'static str` - three two-valued enums.
- `artifact_key: Option<Value>` - the `OceanArtifactKey` type already
  exists in `ocean.rs`; assemble round-trips it through
  `serde_json::from_str(&key.json()).ok()` (silently dropping it on parse
  failure, which would turn an artifact contract into "no key" without a
  word).
- `"compression_policy": "zoom-v1"` and `"profile": "shortbread"` are
  literals; the policy string is a version of code living in the encoder
  and should be a constant next to that code.
- `repo_json(commit: &str, dirty: &str)` parses `"true"`/`"false"` strings
  from `env!` - fine at the build boundary but should be done once in
  `build.rs` into a typed `Option<bool>`.
- `xxh3_128: String` - a `Xxh128([u8;16])` with hex Display would make "32
  lowercase hex" a type invariant instead of a test assertion.
- `SCHEMA_VERSION: u32` compared against `as_u64` from JSON in two places.

Wanted: `#[derive(Serialize, Deserialize)] struct ProvenanceV1 { schema,
input: Input, config: ConfigContract, build: Build, effective:
Option<Effective>, execution: Execution }` with
`#[serde(deny_unknown_fields)]` off (additive members allowed) and enums
`#[serde(rename_all = "snake_case")]`. `extract_contract` returns
`Result<ProvenanceV1, ContractError>` with the existing no-contract cases
as error variants. brokkr can still compare `input`/`config` as `Value`
(re-serialize) if it wants field-named diffs, but the shape is owned here
once.

### 1.8 Archive metadata as concatenated strings

`PmtilesWriter::add_metadata_member(impl Into<String>)` takes raw
`"key":value` JSON fragments; `build_metadata` concatenates them by hand
with `format!`. The justification in the doc comment ("avoid a runtime
dependency for ~20 lines") is stale: `serde_json` is already a runtime
dependency (`provenance.rs`, `verify.rs`, `inspect.rs`). Consequences:

- Duplicate top-level keys are unconstructible-to-detect: a member named
  `"name"` or two `"elivagar"` members produce JSON with duplicate keys,
  and serde keeps the last. The doc comment acknowledges the hazard
  ("a mechanism that let one member displace another would silently
  invalidate every durable ocean artifact") and relies on discipline.
- `ocean_only_metadata: bool` is a mode flag that changes the
  `vector_layers` list; it's a property of the *layer set*, so the writer
  should take `layers: &[Layer]` (or the metadata document) rather than
  know Shortbread at all. Today the generic PMTiles writer imports
  `crate::shortbread::Layer` and hard-codes `"name":"Shortbread"`,
  `"type":"baselayer"` - a dependency edge pointing the wrong way.
- `set_metadata_verbatim(String)` bypasses everything, so the writer has
  two metadata modes held by an `Option` that silently overrides all
  other setters.

Wanted: the writer takes `metadata: serde_json::Map` (or `&str`, opaque),
built by a `metadata.rs` that owns the TileJSON-ish fields, the
`ocean_artifact` member and the `elivagar` member as typed fields of one
struct, serialized once. Key uniqueness becomes a struct property.

---

## 2. Decisions made in more than one place

### 2.1 "Is this a complete provenance contract?" - two owners, already disagree

- `provenance::extract_contract`: requires `schema == 1`, `input`,
  `config`, **and `build`**; reports `Incomplete("input")` before
  `"config"` before `"build"`.
- `inspect::print_provenance`: requires `schema == 1`, `input`, `config`;
  `build` is optional ("diagnostics ... absence is not an error").

An archive with input+config but no build prints a full contract in
`elivagar inspect` and is refused as `Incomplete("build")` by
`brokkr pmtiles-corpus check`. AGENTS.md says build is diagnostic and
"must never be equality-gated" - and extract_contract does not gate on its
value, but it does gate on its *presence*, which inspect says is not
required. Owner: `provenance` (typed decode, 1.7); inspect should call
`extract_contract` and render the `ContractState`, not re-classify.
Also, inspect's "metadata is not JSON" / "absent" / "schema N not
interpreted" / "INCOMPLETE" ladder is a second copy of
`ContractState`'s variants with its own wording.

### 2.2 "Which metadata bytes count as readable?" - two readers disagree

- `PmtilesReader::read_metadata`: `String::from_utf8_lossy` (invalid UTF-8
  silently becomes U+FFFD; the contract then parses as something it
  isn't).
- `ArchiveView::metadata`: strict `String::from_utf8`, error on invalid.

Also `PmtilesReader` treats any `internal_compression != 2` as "none"
(so a brotli- or zstd-compressed directory is decoded as raw varints and
yields garbage or a confusing varint error), while `ArchiveView::open`
rejects anything other than 1 or 2. Same question, two answers.

### 2.3 "Can this archive be read as MVT+gzip?" - four sites

- `ArchiveView::open` (rejects non-MVT or non-gzip).
- `verify::verify_opts_unique` (same check, different error text, and
  verify therefore cannot verify any MLT or brotli archive the writer
  produces - `elivagar verify` is silently MVT-gzip-only).
- `ocean.rs` (re-checks `view.tile_type() != 1 || view.tile_compression()
  != 2` right after `ArchiveView::open` already did).
- `PmtilesReader::read_tile` decides by **not deciding**: it always
  `gzip_decompress`es regardless of the header. svg and diag on a
  brotli-MVT archive fail with a gzip error rather than "unsupported tile
  compression".

And the artifact-contract cousin, "can this artifact serve this run" (MVT
+ gzip + z0-14) in `pipeline/mod.rs`, which is a stricter version of the
same predicate. Owner: `Header`/`TileContract` from 1.1/1.2, with
`reader.tile_decoder() -> Result<Decompressor, Unsupported>` deciding
once.

### 2.4 "Is this tile a seam tile?" - three sites in one file

`verify.rs` computes `x == 0 || x == (1 << z) - 1` at the unique-payload
grouping key, again before the geometry-outcome cache lookup, and again
inside `validate_geometry_commands` to pick the delta limit. The cache
correctness (outcome keyed by `(z, seam)`) depends on all three agreeing,
and on the fact that the paint-order check depends only on `z` (it
branches on `z < 11`). If someone adds an x- or y-dependent check to
`validate_mvt_geometry`, the cache silently replays the wrong outcome.
The validation key should be a type (`ValidationClass { z, seam }`)
computed once and passed in, so the validator *cannot* see `x`.

Note also: the seam test treats seams as the antimeridian columns; the
geometry engine's buffer/antimeridian logic presumably owns the real
definition. If the engine's notion widens, verify drifts.

### 2.5 "How is an MVT tile decoded?" - six decoders

Independent walkers of the same wire format:

| Site | Purpose | Unknown fields | Malformed input |
|---|---|---|---|
| `tile_detail` | gate/regress | Strict or Tolerant | error |
| `pmtiles_reader::decode_mvt_layers` | verify, assemble, tests | skip | **silently stops** (`while let Ok(Some..)`, `if let Ok(sub)`) |
| `verify::validate_mvt_*` | geometry + paint order | skip | mixed: some error, some stop |
| `verify::collect_ocean_*`, `validate_ocean_*` | ocean stats/rings | skip | silently stop / `continue` |
| `svg::decode_*` | render | skip | silently stops; truncated command stream `break`s |
| `main::diag_*` | ring winding | skip | silently stops |

Plus three packed-varint decoders (`verify::decode_packed_varints`,
`svg::decode_packed_varints` - byte-identical, `geometry::decode_mvt_polygon`
consuming the output) and four zigzag implementations (`tile_detail`
twice, `verify`, `svg`, `mvt_decode`).

The decision duplicated is not "how to parse a varint" but **what a valid
tile is** and **what ring role a ring has**:

- Ring role: `tile_detail` classifies Outer iff `signed_area > 0`;
  `diag` prints CW=outer/CCW=hole by its own sign; `geometry::decode_mvt_polygon`
  returns rings with no role; svg punts to nonzero fill. MapLibre's
  `classifyRings` is the authority and the earcut oracle uses it. Two
  in-crate sign conventions are a drift waiting to happen - the R23
  ClosePath bug in AGENTS.md is precisely a symmetric-convention bug.
- Geometry validity: `verify::validate_geometry_commands` is the only
  place spelling the MVT command-grammar rules; `tile_detail` accepts
  streams verify rejects (zero repeat count, ClosePath on lines is silently
  ignored, LineTo count with fewer than 2 points, polygon ring without
  ClosePath) - the gate's decoder is *more* permissive than the
  structural verifier on exactly the axis the gate exists to be strict on.
- Polygon grouping: `tile_detail` groups "outer opens a component,
  following holes attach"; a leading hole opens its own component. This is
  close to but not identical to classifyRings (which uses the first ring's
  sign as the outer sign, not a fixed sign). For elivagar's own output it
  agrees; for "tolerant cross-producer comparison" with a producer that
  winds the other way, every polygon becomes an all-Hole component.

Owner: one `mvt::decode` module producing `DetailTile` (or a borrowed,
zero-copy variant for verify's hot loop) with a typed error and a
strictness policy; a `geometry::ring` module owning signed area and the
classifyRings-equivalent role assignment; verify becomes rules over the
decoded tile; svg and diag become renderers over it.
`pmtiles_reader::MvtLayer` / `decode_mvt_layers` should not live in a
container reader at all.

### 2.6 "Which payloads are equal?" (dedup) - two copies

`add_tile` and `add_run` each compute the two SipHash fingerprints, probe
the bucket, push into it, and maintain `DedupStats`. They already differ:
`add_tile` counts `reject_len_mismatch`/`reject_fp_mismatch` per candidate,
`add_run` counts neither; `add_tile` is `#[hotpath::measure]`, `add_run`
not; `add_run` checks run ordering not at all (no `debug_assert`). The test
helper `compute_dedup_hashes` is a third copy of the hash. Extract a
`DedupIndex { fn lookup_or_insert(&mut self, data) -> (BlobRef, bool) }`.

Also: `DefaultHasher` is SipHash with unspecified, release-dependent
output ("the algorithm ... may change"); fine since the map is in-memory,
but an xxh3-128 (already a dependency) gives one 128-bit fingerprint
instead of two salted 64-bit passes over the same bytes - half the
hashing work on the assemble hot path.

### 2.7 "Where does the simplified shapefile serve?" - assemble mirrors ocean

`assemble.rs` decides `low_zoom_source` with the comment "Mirrors
ocean_pass_max_zooms". That's a self-declared duplicated decision; the
provenance value should be read from the ocean resolver's result, not
re-derived from config.

### 2.8 Directory encode/decode in two modules

`encode_directory` (writer, private) and `decode_directory` (reader,
public) are the two halves of one format with no shared definition of the
column order or the `offset + 1` / `0 = contiguous` convention. The writer
tests hand-decode with a Cursor instead of calling `decode_directory`, so
the pair is only tested symmetrically through full archive round trips.
Put both in `archive::directory` with a property test
`decode(encode(x)) == x` plus the existing hand-spelled vectors.

---

## 3. Structure

### 3.1 Proposed shape

```
archive/
  header.rs      Header, TileType, Compression, Bbox/Center; encode+decode
  tile_id.rs     TileId, TileCoord, Hilbert (moved out of the writer)
  directory.rs   DirEntry enum, encode/decode, run invariants, find_entry
  reader.rs      ONE reader (mmap), typed ArchiveError, tile decompression by header
  writer.rs      container only: blobs, dedup index, directories, layout
  metadata.rs    TileJSON-ish metadata document (layers, ocean_artifact, elivagar), serde
mvt/decode.rs    the one decoder (strictness, typed errors), absorbs tile_detail
geometry/ring.rs signed area, ring role (classifyRings semantics)
provenance.rs    serde types both directions; extract_contract over them
cli/inspect.rs, cli/verify.rs, cli/svg.rs, cli/diag.rs   consumers only
```

### 3.2 Two readers where one would do

`PmtilesReader` (seek+read, allocate per call) and `ArchiveView` (mmap,
borrow) duplicate open, header parsing, directory reading,
`read_all_runs`, and metadata, with divergent validation (2.2, 2.3). The
mmap view is strictly better for every consumer here (inspect, verify,
svg, diag, brokkr's corpus walk); the only reason to keep a file-backed
path would be archives larger than address space, which isn't a 64-bit
concern. Delete `PmtilesReader`, keep `ArchiveView` as `Archive`, add the
missing accessors. `read_u64_le`, `read_i32_le`, `gzip_decompress` should
stop being `pub` from the reader (assemble reaches in for `gzip_decompress`
and `decode_mvt_layers` today).

### 3.3 The writer knows Shortbread

`build_metadata` imports `crate::shortbread::Layer`, emits
`"name":"Shortbread"`, and owns the ocean-only layer list. The container
writer should not know the schema. Same for `PmtilesConfig` lacking a
metadata input: the composition belongs in a metadata module the pipeline
calls.

### 3.4 Writer storage modes as two enums with matching arms

`TileBlob::{Memory, File}` and `DirStore::{Memory, Streaming}` are chosen
together (`new` vs `new_streaming`) and matched separately in
`add_tile`, `add_run`, `flush_run`, `finalize_directories`, `write_to`.
Mixed combinations are representable but never constructed.
`finalize_directories` has three near-identical "try root, else double
leaf size until root fits" loops. Better: a `Sink: Write + Seek` for the
blob (a `Vec` cursor or the partial file) and a `DirSpill` trait, and one
`fn layout_directories(entries: impl Iterator<DirEntry>)` loop. The
in-memory mode then exists only as a test/small-run sink, not a parallel
code path that can diverge (it already writes the archive differently:
one-pass vs patch-and-rename).

### 3.5 verify.rs is four tools in one file

Container checks, MVT structural grammar, Shortbread paint-order rules
(`validate_paint_order` hard-codes `"land" | "streets" |
"street_polygons"` and the `z < 11` street-class switch - a decision that
belongs to `shortbread::paint_order`, which should expose
`rank_for(layer, z, attrs) -> Option<Result<rank>>`), and ocean-specific
geometry stats/self-intersection. Each walks the decoded tile
separately: per tile, the payload is protobuf-parsed four times
(`decode_mvt_layers`, `validate_mvt_geometry`, `validate_ocean_rings`,
`collect_ocean_geometry_stats`). Split into a `Check` trait over a
decoded tile, with the Shortbread rules owned by `shortbread/`.

### 3.6 inspect.rs parses JSON by substring

`extract_json_string(j, "\"tile_payload_format\":\"")`,
`extract_number`, and `print_metadata_json`'s `"id":"` scanner reimplement
JSON parsing by `find`, inside a module that also does `serde_json::from_str`
on the same document 60 lines later. They break on whitespace after the
colon (any non-elivagar producer, or a pretty-printed metadata). Parse
once with serde.

### 3.7 Tests mirror the file split, not the seams

- Writer tests test the encoder by hand-decoding; reader tests test the
  decoder by hand-encoding (`encode_directory_raw`). Neither tests
  `decode(encode)`; brokkr-facing round trip happens only via full
  archives.
- inspect tests build archives with the writer and patch header bytes by
  raw offset - they'd become `Header` mutations.
- verify tests assert on error string prefixes.
- `pmtiles_writer_tests.rs` houses Hilbert tests (`test_hilbert_*`) and
  varint tests (`test_varint_*` testing protohoggr, not this crate) -
  Hilbert tests belong with `tile_id.rs`.
- `tile_detail.rs` - the decoder the standing gate depends on - has **no
  tests in this crate**. Strict/Tolerant, ClosePath cursor handling (the
  R23 class), ring grouping, value decoding are all untested here; the
  only coverage is whatever brokkr exercises.

---

## 4. Bugs, smells and surprises

1. **Out-of-order tiles silently corrupt the directory in release
   builds.** `push_dir_entry` only `debug_assert!`s Hilbert order, and its
   comment claims out-of-order input "would degrade run-length compression
   but not produce an invalid archive - PMTiles readers binary-search the
   directory regardless". That is false: `encode_tile_id_column` uses
   `saturating_sub`, so an out-of-order tile ID encodes delta 0 and
   decodes as the *previous* entry's ID - the directory then addresses the
   wrong tile, or creates a duplicate ID. `add_run` has no order check at
   all. Should be a hard error (cheap: one compare per tile).
2. **inspect's section layout prints the header at the root dir's
   offset**: `"Header: ... offset {root_dir_offset}"` prints 127, not 0.
   It also lists sections in a fixed order (root, metadata, leaf, data)
   that no longer matches the writer's layout (data precedes metadata and
   leaves).
3. `verify` does not enforce "first command must be MoveTo":
   `expect_moveto` is written but never read for LineTo, so a line
   starting with LineTo passes. And a linestring's *first* MoveTo may
   have count > 1 (the check is gated on `!expect_moveto`), contrary to
   spec 4.3.4.3, which the error message itself cites.
4. In non-unique mode, verify caches only the geometry outcome per run;
   `decode_mvt_layers`, `validate_ocean_rings` and the stats collector are
   recomputed for every addressed tile in a run, on identical bytes. On
   ocean-heavy archives (runs of hundreds of thousands of full-tile ocean
   fills), that's the dominant cost of `verify` without
   `--unique-payloads`. Cache the whole per-`(blob, class)` outcome.
5. `tile_detail` accepts `geom_type` values other than 1/2/3 by returning
   no components even in Strict mode, and treats a feature with no
   geometry field as valid with empty components. For a strict gate, an
   unknown geometry type is "foreign structure".
6. `tile_detail` merges repeated `tags`/`geometry` fields by concatenation
   (protobuf packed-field semantics), but a repeated `id` or `type` field
   is last-wins silently; Strict should probably reject duplicates.
7. `PmtilesReader::read_directory`/`read_metadata`/`read_tile` allocate
   `vec![0; length as usize]` from untrusted header/directory values with
   no bound; `inspect` caps metadata at 10 MB but not directories, and
   `svg`/`diag` cap nothing. A corrupt header can request a multi-GB
   allocation. (Moot if the mmap reader replaces it.)
8. `assemble.rs` drops the artifact key to `None` if
   `serde_json::from_str(&key.json())` fails - an artifact build would
   then record `artifact_key: null`, indistinguishable from "computed".
   Should be infallible by construction (typed key).
9. `provenance::producer_config_diff` dedups by `starts_with(key)`, so
   a key that is a prefix of another (`min_zoom` vs a hypothetical
   `min_zoom_ocean`) would suppress the second diff line.
10. `add_run` checks `data.len() > u32::MAX` and then also
    `u32::try_from` - harmless duplication; `add_tile` does the cast with
    `as` after the check.
11. `inspect` doc comment cites "reference/metadata.md" - it exists, fine;
    but the module header says inspect reads "optional gzip-compressed
    metadata", while the reader decompresses only when internal
    compression is exactly gzip and lossily decodes otherwise (2.2).
12. svg's `LAYER_COLORS` assigns colors by first-appearance order in the
    rendered grid, so the same layer gets a different color in different
    tiles/grids - which works against the documented "compare the same
    ROI across archives" svg-roi idiom when eyeballing. Key colors by
    `Layer` instead.
13. `geometry::decode_mvt_polygon` uses unchecked `i32 +=` on deltas
    (debug-panics, release-wraps on hostile input) while `tile_detail`
    uses `checked_add`; verify feeds archive bytes into it via the ocean
    ring check, so a corrupt ocean tile can panic `elivagar verify` in a
    debug build.
