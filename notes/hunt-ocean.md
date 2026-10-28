# Design hunt: ocean

Scope: `src/ocean.rs`, the ocean wiring in `src/pipeline/mod.rs`
(`resolved_ocean_mode`, `ocean_pass_max_zooms`, the ocean phase, `ocean_build`,
`retain_outgoing_ocean_artifact`, checkpoint ocean mode), the artifact half of
`src/pipeline/assemble.rs` (run copies, splice, provenance writer), the
`--ocean` parser in `src/main.rs`, and `OceanContract` in `src/provenance.rs`.

Headline: the ocean subsystem has one real concept that the code never names -
**an ocean plan**: which shapefile feeds which zoom pass, which grid owns which
zoom, whether an artifact serves the interior, and what identity the whole
thing has. Today that plan is re-derived from `Option<PathBuf>` fields and
`min_zoom`/`max_zoom` in at least six places, and the derivations already
disagree in their documented meaning (finding 2.1). Most of the type and
structure findings below collapse into "build `OceanPlan` once, in one
module, and hand it to everything".

---

## 1. Axes that should be types

### 1.1 The ocean configuration is three loose `Option<PathBuf>`s plus two side-channel fields

`TilegenConfig` carries `ocean_shapefile`, `ocean_simplified_shapefile`,
`ocean_tiles`, `ocean_artifact_key: Option<OceanArtifactKey>` and
`ocean_only_metadata: bool`. States this admits that the run never enters:

- simplified set, full unset (silently "no ocean" - the provenance writer has
  a comment explaining this, i.e. the type forces prose to cover the hole);
- `ocean_tiles` set with no shapefile (rejected by `main.rs`, then rejected
  again by `resolved_ocean_mode` with a different message - library callers
  only get the second);
- `ocean_artifact_key: Some` together with `ocean_tiles: Some` (builder and
  consumer at once);
- `ocean_only_metadata: true` with OSM layers present.

`main.rs` already has the right shape (`OceanInputs { low, high, artifact }`,
validated partition) and then flattens it back into the loose fields. The
validated value should be the library type:

```
enum OceanSource {
    None,
    Shapefiles { passes: OceanPasses, artifact: Option<PathBuf> },
}
enum OceanPasses { Single(ShapefilePair), Split { low: ShapefilePair, high: ShapefilePair } }
struct ShapefilePair { shp: PathBuf, shx: PathBuf }   // shx derived once
```

and the artifact *builder* should not be a `TilegenConfig` at all (see 3.2):
`ocean_artifact_key` and `ocean_only_metadata` are builder-only knobs riding
on the run config, which is why `ocean_build` has to fill in `pbf_path:
PathBuf::new()`, `fanout_caps: [0; ..]`, `locations_on_ways: false` and so on.

### 1.2 `.shx` path is re-derived by `with_extension("shx")` at five sites

`OceanSourceKey::from_inputs`, `OceanArtifactKey` callers in
`resolved_ocean_mode` and `ocean_build`, and `process_ocean_shapefile` each
derive the index path independently. `OceanArtifactKey::from_inputs` even
takes the shx paths as parameters, so a caller could pass a mismatched pair
and get a key that hashes a different index than the one the pass reads. A
`ShapefilePair` value (1.1) makes the pair one thing.

### 1.3 Two near-identical key types with two hand-written codecs

`OceanSourceKey` (resume identity) and `OceanArtifactKey` (artifact identity)
share seven of eight fields. Their JSON codecs are hand-written and differ:

- `OceanSourceKey::to_json` uses `json!`; `OceanArtifactKey::json` uses
  `format!` with a raw-string template and returns a `String`.
- `OceanArtifactKey::from_json` requires exactly 32 lowercase hex chars and
  requires the optional members to be present (null); `OceanSourceKey::from_json`
  accepts any hex length, uppercase, and treats a missing optional as `None`.
- Both callers that want a `Value` from the artifact key round-trip it through
  text: `serde_json::from_str(&key.json())` - in `run()` with `.ok()` (a parse
  failure silently drops the key from provenance, turning an artifact-active
  archive's contract into "no key"), in `write_provenance` with `.ok()` again,
  in `save_checkpoint` with `map_err`.

The comment on `OceanSourceKey` explains why compression level is excluded,
which is correct, but the right encoding of that is composition, not a copy:

```
struct OceanSourceKey { shapefiles: ShapefileHashes, zooms: ZoomRange, policy: OceanPolicyVersion }
struct OceanArtifactKey { source: OceanSourceKey, compression_level: CompressionLevel }
```

with one serde derive (or one codec) and a `Xxh128` newtype that owns the
"32 lowercase hex" rule. The artifact key's `min_zoom`/`max_zoom` are
always 0/14 at both construction sites; they are a restatement of the
artifact contract (checked again in `OceanTiles::open` from the header), so
they should either be derived from a single `ARTIFACT_ZOOMS` constant or
dropped from the key (dropping changes artifact bytes - a policy bump).

### 1.4 `OCEAN_POLICY_VERSION` is a bare `u32` on a promise

The doc comment says any change to `ocean_params`, the pyramid, the
simplifiers, the overlay engine or the MVT encoder requires a bump, and the
v2 history is a three-day silent-staleness incident caused by forgetting.
Nothing mechanical ties the constant to the code it versions. Options, in
increasing strength:

- a golden test: build a small fixed synthetic world through
  `process_ocean_shapefile` + assemble and pin the digest of the output
  together with `OCEAN_POLICY_VERSION`; any output change fails the test until
  someone updates both (a "change detector" - exactly the failure mode the
  bump exists to catch, now caught at `brokkr check` time);
- additionally fold a hash of `ocean_params`' resolved values (tolerance,
  min area, simplifier enum) into the key, so the tunable half cannot drift
  without changing the key.

The golden test is the important one: the policy version is the only
staleness defence the artifact has and it is currently honour-system.

### 1.5 Ocean mode is a `&'static str` in two vocabularies

`OceanMode` is a proper enum, but it is rendered as `"none"/"computed"/"band"`
in the checkpoint and `"none"/"shapefile"/"artifact"` in provenance
(`RunProvenance::ocean_mode: &'static str`, `OceanContract::mode: &'static str`).
`low_zoom_source` is likewise a `&'static str` (`"none"/"simplified"/"full"`).
`write_provenance` then branches on `mode == "none"` - a string compare on a
value that was an enum two calls earlier. Carry `OceanMode` (or the plan,
1.1/3.1) into `RunProvenance` and give it one `contract_name()`.

### 1.6 Artifact refusals are prose `std::io::Error::other`

`OceanTiles::open` and `resolved_ocean_mode` produce about a dozen distinct
refusals (wrong format, wrong zooms, not world bounds, not exactly one ocean
layer, key mismatch, compression mismatch, run outside z0-14, missing file,
missing metadata), all as strings. brokkr links this crate and the corpus
gate already has the notion "contract refusal, exit 2"; "artifact key does
not match current shapefile inputs" is exactly a contract refusal but can only
be recognised by substring. Also the key-mismatch message does not say which
field differs - for an operator, "policy_version 4 vs 5" versus
"full_shp_xxh128 differs" are different actions (rebuild vs re-download).
A typed `ArtifactRefusal` enum with a `KeyMismatch { field, declared, expected }`
variant fixes both.

### 1.7 Coordinate spaces are all `i32`/`f64`

`OceanPassGrid` stores `IntRect`s in "max-zoom pixel space of this pass";
`pass_owns_root_range` takes four `f64` Mercator-unit coordinates; the
shapefile is EPSG:3857 metres converted by `from_epsg3857`;
`ocean_band_tile` takes tile `(z, tx, ty)` and does `4096 << shift`.
The `scale = 1 << (max_zoom + 12)` formula is written out in
`OceanPassGrid::for_bounds`, `ocean_band_tile`, `pass_owns_root_range` and
`data_bounds_rect`. A grid rect from a z7 pass and one from a z14 pass are
the same type and silently comparable. This matters concretely: in the
single-shapefile + artifact case the low (z0-z7) pass is handed the z14
grid (see 2.1) and nothing in the types says the pass and its grid are in
different pixel spaces; it works only because `ocean_band_tile` re-derives
`shift` from `pass.max_zoom`. A `PassGrid` that owns its scale and exposes
`tile_rect(z, x, y)` / `snap(merc)` methods would remove the four copies
of the formula. The magic `128` buffer in `ocean_band_tile` is the MVT
tile buffer and should be the same constant the pyramid uses for its
buffered clip - if those two ever differ the band/artifact boundary is wrong.

### 1.8 Pass identity is positional

Producers pick a grid with `grids().first()` / `grids().last()`; the consumer
uses `selected_pass_grid(grids, z)` which keys on `grids.len() == 2 && z <= 7`.
"Low pass" vs "high pass" is encoded as vector index and length. A
`PassGrids { low: Option<Grid>, high: Grid }` (or a `grid_for_zoom(z)` method
on the plan) removes the positional convention.

---

## 2. Decisions made in more than one place

### 2.1 "Which pass covers zoom z, and from which shapefile" - six sites, already inconsistent

Sites:

1. `main.rs::resolve_ocean_inputs` - accepts only `z0-z14` or `z0-z7`+`z8-z14`.
2. `pipeline::run` ocean phase - ALWAYS runs a z0-min(7) pass and a z8+ pass;
   low source is simplified if named, else full.
3. `pipeline::ocean_build` - hardcodes the same two passes again (`0, 7` and
   `8, 14`), with a "mirror the run pipeline" comment.
4. `pipeline::ocean_pass_max_zooms` - says a low (z7) grid exists only when a
   simplified shapefile is named; otherwise a single grid at `max_zoom`.
5. `ocean::selected_pass_grid` - "two grids and z <= 7 means first".
6. `ocean::LOW_ZOOM_UNION_MAX_ZOOM = 7` - union fires per pass by
   `max_zoom <= 7`.
7. `assemble::write_provenance` - `low_zoom_source` re-derives "simplified iff
   simplified set and min_zoom <= 7" with a comment "Mirrors
   ocean_pass_max_zooms".

Disagreements that exist today:

- Sites 2/3 split a single `z0-z14:full.shp` into two passes; site 4 says it is
  one pass. With an artifact, the low pass is therefore handed
  `grids().first()`, which is the z14 grid. It produces correct output only
  because the consumer (site 5) also picks the single grid, and
  `ocean_band_tile` is zoom-generic. Nothing pins that agreement.
- Doc comments contradict the code: `OceanInputs` in `main.rs` says "a single
  z0-z14 entry ... serves every zoom, matching `ocean::selected_pass_grid`"; the
  `LOW_ZOOM_UNION_MAX_ZOOM` comment says "A single z0-z14 full-shapefile pass
  keeps the old per-piece behavior at low zoom". Both are stale: since the
  pass split was made unconditional, a single `z0-z14` spelling DOES union at
  low zoom. These are the "comment states a fact" kind of stale and should be
  reworded when the owner below lands.
- `--ocean z0-z14:full.shp` and `--ocean z0-z7:full.shp --ocean z8-z14:full.shp`
  run identical work, but record different `low_zoom_source` ("full" vs
  "simplified") and different `OceanSourceKey`s (simplified hash `None` vs
  `Some(full)`), so they are contract-incomparable while byte-identical.
  The contract is describing the spelling, not the work.

Owner: an `OceanPlan` built once (in `ocean/plan.rs`, see 3.1) from the
validated input, exposing `passes() -> [Pass { source, zooms, union: bool,
grid }]`, `grid_for_zoom(z)`, and `contract()`. `run`, `ocean_build`,
`OceanTiles::open`, assemble's band test and the provenance writer all read
it. Then the "z7/z8 split" lives in one constant, and the union decision is
a property of the pass, not a re-test of `max_zoom`.

### 2.2 "Which tiles does the computed band own" - producer filter vs consumer copy/splice

The band/artifact ownership answer is computed by `ocean_band_tile` (one
function, good), but which grid is fed to it is chosen independently by the
producer (positional `first()/last()` in `run`) and the consumer
(`selected_pass_grid` in `OceanTiles::band_tile`), and the producer also
applies a second, coarser prefilter (`pass_owns_root_range`, documented as
"deliberately follows pyramid::root_fragments" - a re-implementation of the
pyramid's root snapping that must stay in step with it by hand).
Additionally `band_is_empty` is evaluated per pass in
`process_ocean_shapefile` and as `all(grids)` in assemble.

The runtime check "non-band OSM tile unexpectedly contains computed ocean"
in `splice_artifact_ocean` is a genuine re-check of the agreement, but it only
fires on tiles that also have OSM content. An ocean-only tile where the
producer emitted (thinks band) but the consumer thinks non-band is passed
through as an `Encoded` item (run copies exclude only that tile id), so it is
served computed with no error. A disagreement the other way (producer filtered, consumer thinks band) drops
the tile's ocean entirely with no error. Neither direction has a test that
builds both sides from the same plan.

Owner: the plan's `grid_for_zoom` (2.1) plus one `owns(z, x, y)` method used
by both the pyramid `tile_filter` and assemble; `pass_owns_root_range` should
call a pyramid-exported "root cells for bbox" function instead of copying its
snapping.

### 2.3 "May the artifact serve this run" - three sites

1. `main.rs` - artifact without shapefile is an error.
2. `resolved_ocean_mode` - format/compression/zoom contract against config,
   existence, declared compression level vs config, full shapefile present
   (again), recompute key.
3. `OceanTiles::open` - format/compression/zoom against the archive header,
   world bounds, single ocean layer, full key equality, run range.

Step 2 opens the archive once just to read the declared key
(`declared_key`), then step 3 opens it again (and, on `--skip-to` paths, at
a different point in `run`, with the open duplicated verbatim at two sites in
`run`). The compression-level comparison in step 2 is a strict subset of the
key equality in step 3 - kept only for a better message, which 1.6's typed
mismatch would give for free. Owner: one `OceanArtifact::admit(path, &plan,
&run_contract) -> Result<OceanArtifact, ArtifactRefusal>` called once, before
phase12, with the opened view kept.

### 2.4 "What zoom range does the artifact contract cover" - hardcoded 0/14 in four places

`resolved_ocean_mode` (`min_zoom != 0 || max_zoom != 14`, then `0, 14` into
the key), `ocean_build` (`0, 14` into the key and a config with `0..14`),
`OceanTiles::open` (`min_zoom() != 0 || max_zoom() != 14`, and
`z14_end = ((1 << 30) - 1) / 3 + 1` - a Hilbert tile-id formula that belongs
to `pmtiles_writer`), and `artifact_run_copy_items` (`if z == 14`). One
`ARTIFACT_CONTRACT` constant (format, compression, zoom range) in the
artifact module.

### 2.5 Per-zoom compression policy - one function, but a second consumer that must agree

`compression_level_for_zoom` is shared by batch encoding and
`splice_ocean_layer`, good. But the artifact's own run-copy bytes were
compressed by the *builder* at its level under the same policy, and the
splice re-compresses at the *run's* level. Admission compares base levels
only; the policy name `zoom-v1` lives as a string in provenance and is not in
the artifact key. If the policy table changes, existing artifacts still
key-validate and run copies (artifact bytes) and spliced tiles (fresh bytes)
diverge in compression within one archive. Put the compression policy
version into `OceanArtifactKey` (or make it part of `OCEAN_POLICY_VERSION`'s
documented scope, which today lists the MVT encoder but not the gzip policy).

### 2.6 Shapefile identity is hashed twice per artifact-active resume, never for computed provenance

`OceanSourceKey` (computed mode) and `OceanArtifactKey` (band mode) each hash
the inputs; neither identity reaches the archive contract in computed mode.
`OceanContract` records `mode`, `runtime_simplification`, `low_zoom_source`
and `artifact_key` - so two computed-mode archives built from different
shapefile releases, or under different `OCEAN_POLICY_VERSION`s, present
identical ocean contracts. The artifact-active baseline happens to be
protected (the key carries hashes and the policy version); the computed path
is not. That is the same decision ("what identifies the ocean inputs of this
archive") answered differently by mode. Owner: the plan's `contract()` emits
the source key in all non-none modes, and the artifact key when one is served.

---

## 3. Structure

### 3.1 `ocean.rs` is four modules, and the fifth lives in `pipeline/`

`src/ocean.rs` (2290 lines, ~1000 code) holds:

- the artifact identity (`OCEAN_POLICY_VERSION`, both keys, `hash_file`);
- the artifact reader and the band ownership geometry (`OceanTiles`,
  `OceanPassGrid`, `ocean_band_tile`, `selected_pass_grid`);
- a shapefile reader (shx parsing, mmap, `read_exact_at` ring reads, ring
  role inference by signed area);
- the emission engine glue (union, `ocean_params`, `OceanAcc`, sink, rayon
  work splitting, stats atomics).

Meanwhile ocean *orchestration* (pass split, artifact admission, artifact
retention, `ocean_build`) lives in `pipeline/mod.rs`, and artifact *merging*
(run copies, splicing, a hand-rolled protobuf field scanner `ocean_layer_field`
+ `read_varint`) lives in `pipeline/assemble.rs`. Suggested layout:

```
src/ocean/
  mod.rs        OceanPlan (validated inputs, passes, grids, contract)
  policy.rs     OCEAN_POLICY_VERSION, ocean_params, union threshold, golden test
  key.rs        Xxh128, ShapefileHashes, OceanSourceKey, OceanArtifactKey (one codec)
  shapefile.rs  .shp/.shx reader yielding projected polygons; no sort/pyramid deps
  emit.rs       union + pyramid emission into SortWriter, stats
  artifact/
    mod.rs      ARTIFACT_CONTRACT, admit() -> OceanArtifact, ArtifactRefusal
    band.rs     PassGrid, owns(z,x,y), root-range prefilter
    merge.rs    run-copy planning and splice (moved out of assemble.rs)
    build.rs    ocean_build + retention (moved out of pipeline/mod.rs)
```

Gains: the shapefile reader becomes testable without a `SortWriter` and
reusable (it is generic ESRI Polygon reading); the artifact merge code stops
inflating `assemble.rs`, which today interleaves partition scheduling with
MVT field surgery; `pipeline/mod.rs` stops knowing the z7 split.

### 3.2 The artifact builder masquerades as a tilegen run

`ocean_build` constructs a full `TilegenConfig` with dummy PBF fields and two
builder-only flags so it can call `assemble::phase_assemble`. That pushes
builder concerns into the run config (1.1) and into `assemble`
(`set_ocean_only_metadata`, `ocean_artifact_key` metadata member,
`write_provenance`'s "the artifact builder supplies its own key via config"
branch). Better: split assemble's entry into an `AssembleSpec { format,
compression, zooms, metadata: MetadataSpec }` that both `run` and the
builder construct, with `MetadataSpec::Tilegen{..}` vs
`MetadataSpec::OceanArtifact{ key }`. The builder then never sees
`fanout_caps` or `locations_on_ways`.

Related: the artifact is identified by a top-level `ocean_artifact` metadata
member which `provenance.rs` explains it cannot move "because it would
invalidate every artifact already built". Pre-1.0 and with a policy bump
already the normal way to rotate the artifact, that constraint is cheap to
break: fold it into a typed, versioned `elivagar.ocean_artifact` member in
the same provenance schema the run archives use, so one reader parses both.

### 3.3 Ocean splice reaches into the MVT wire format from assemble

`ocean_layer_field` / `read_varint` / `raw_contains_ocean` re-implement
protobuf field scanning in `assemble.rs`, next to `pmtiles_reader::decode_mvt_layers`
and the `tile_detail` decoder. And the splice decompresses the freshly encoded
OSM tile to append the ocean field, then recompresses: the tile was just
encoded in-process. Structurally the cleaner design is to inject the
artifact's ocean layer bytes *before* compression (pass the raw layer field
into `encode_tile_batch_mvt` for non-band tiles), which removes a
gunzip+gzip per spliced tile and the "unexpectedly contains computed ocean"
decode. The field scanner should live with the other MVT decoders.

Side effect worth noting: spliced tiles carry the ocean layer last, computed
tiles carry it in Shortbread layer order. Harmless for style-driven
renderers, but it is a second, unrecorded difference between artifact-served
and computed tiles.

### 3.4 State split from behaviour: `OceanMode::Band` without its path

`OceanMode::Band { key }` does not carry the artifact path; every use
re-pairs it with `config.ocean_tiles` via `if let (OceanMode::Band { key },
Some(path)) = (&ocean_mode, config.ocean_tiles.as_deref())` (three times in
`run`). If the mode is Band and the path is None the code silently proceeds
without the artifact. Band should own the admitted `OceanArtifact` (2.3), so
the pairing cannot fail.

### 3.5 Global mutable stats

`OCEAN_STATS` is a process-global set of atomics accumulated across passes
and flushed by `emit_ocean_counters`. It is never reset, so a library caller
running two pipelines in one process (brokkr links this crate; tests run in
one process) double-counts. Return a `PassStats` from `process_ocean_shapefile`
and sum in the caller.

### 3.6 Test layout mirrors history

`ocean.rs`'s test module contains seven `point_in_polygon` tests
("pip_inside_square" etc.) under a header "point_in_polygon tests", for a
`geometry::` function the ocean module no longer uses (the module header
proudly says "no point-in-polygon"). They belong in `geometry`'s tests or
nowhere. Band-ownership tests sit beside shapefile-parsing tests; with 3.1
they split naturally. There is no test that runs producer filter and
consumer copy/splice from one plan and checks every tile is owned by exactly
one side (2.2) - that is the test this subsystem most lacks, more than any
of the current unit tests.

---

## 4. Other things noticed

- `OceanPassGrid::band_is_empty(min_zoom, max_zoom)` ignores both arguments
  (`let _ = ...`). Drop the parameters.
- `process_ocean_shapefile` skips shx records with negative offsets silently,
  skips records with bad part indices / short reads with an `eprintln!`
  warning, and keeps going. Under `--bench` stderr is not read, so a corrupted
  shapefile produces a silently thinner ocean whose key still validates (the
  hash is of the same corrupt bytes). For a durable world artifact, a
  malformed record should be an error, or at least a counter.
- Ring role inference: `is_outer = w == 0 || signed_area >= 0.0`; inner rings
  appearing before any outer are dropped (`else if current_outer.is_some()`),
  and a hole is attached to the most recent outer rather than its container.
  Correct for the OSMCD split data; worth a one-line invariant comment or a
  check, since `union_pieces`' disjoint-interior invariant also depends on it.
- `ocean_build` hashes the shapefiles for the key, then reads them again for
  geometry; a file replaced in between yields an artifact whose key names
  bytes it was not built from. Hash while reading, or hash after.
- `retain_outgoing_ocean_artifact` names the retained file by policy version
  and mtime date only; two rebuilds the same day at the same policy (e.g.
  shapefile refresh) replace the retained generation; keeping the most recent
  outgoing one is fine, but a same-version shapefile rotation is indistinguishable
  in the name. Including a short source-hash prefix would make it self-describing.
- `OceanTiles::open` reads PMTiles header bounds by raw offsets
  (`read_i32_le(header, 102)` etc.); those belong behind an `ArchiveView`
  accessor.
- `run` loads the checkpoint a second time just to get `data_bounds`
  (`let (data_bounds, _, _, _) = load_checkpoint(...)`), and a third time
  before assemble for the skip-to-assemble artifact open.
- `artifact_run_copy_items` walks the band case tile by tile over whole runs
  (`while cursor < run_end`), calling `tile_id_to_zxy` per tile. Artifact runs
  are long (the artifact addresses ~23x more tiles than it stores); for an
  extract that is fine because band-empty skips this, but a near-world
  extract with a non-empty band walks hundreds of millions of tile ids.
  Computing band rectangles per zoom and intersecting run ranges with them
  would make it proportional to band size.
