# Design reconnaissance: pipeline orchestrator and config surface

Scope: `src/pipeline/` (mod, phase12, relations, boundary_prescan, emit,
assemble, stats), `src/pipeline_tests.rs`, `src/lib.rs`, `src/main.rs`.
I followed the threads into `provenance.rs`, `ocean.rs`
(`OceanTiles`, `OceanPassGrid`, `selected_pass_grid`), `sort.rs` and
`shortbread/boundaries.rs` where the decisions lead there.

Headline: the orchestrator's real problem is that `TilegenConfig` is one flat
bag of primitives that several independent sites have to interpret. The ocean
pass structure, the tile contract, the "producer vs assemble" split and the
effective-path vocabulary are each decided in 3 to 7 places. At least two of
those sites already disagree with the others (findings 2.1 and 2.2). I
recommend a rewrite of the config and orchestration layer, not incremental
fixes. Section 4 gives the shape.

---

## 0. Things that look like bugs (verify first)

1. **MLT runs record `gzip` in provenance and in the sidecar.**
   `assemble.rs` maps `TilePayloadFormat::Mlt` to
   `TileDataCompression::None` and ignores `config.tile_compression`.
   `provenance::config_json` writes
   `tile_compression_name(config.tile_compression)`, which defaults to
   `gzip` from clap, and `run()` emits `tile_compression=0` (gzip). So the
   comparability contract of every MLT archive names a compression the
   archive does not use. Section 2.2 covers the root cause.

2. **Single-shapefile ocean plus an artifact uses the wrong pass grid at
   z0-z7.** `run()` always runs two ocean passes. Since OCEAN_POLICY_VERSION
   v3 the low pass is a separate z0-z7 pass even when only `z0-z14:<shp>` is
   named, and `ocean_build` does the same. However, `ocean_pass_max_zooms`
   still returns `[max_zoom]` (one grid, z14) when no simplified shapefile is
   named. `OceanTiles::open` therefore builds one z14 grid:
   - The low pass receives `grids().first()`, which is that z14 grid.
   - `selected_pass_grid` (`len()==2 && z<=7`) serves z0-z7 band ownership
     from it.

   `OceanPassGrid::for_bounds` rounds the inner rect with ceil/floor at the
   grid's own scale, so z0-z7 band ownership is decided at z14 resolution
   while the low pass descends from z7. `write_provenance` also derives
   `low_zoom_source` with a comment saying it "mirrors
   ocean_pass_max_zooms". Production always names the simplified shapefile,
   so this has not bitten. The `main.rs` doc on `OceanInputs` repeats the
   pre-v3 claim that "a single z0-z14 entry ... serves every zoom", which
   contradicts `run()`. I have not verified that this changes output bytes.
   It is a disagreement either way.

3. **Sort-chunk compression is a property of the chunks, but a resume takes
   it from the current invocation.** `compress_sort_chunks` is in neither
   `producer_config` nor the checkpoint. A `--skip-to sort|assemble` with a
   different `--compress-sort-chunks` passes the config to
   `SortReader::from_dir` / `SortWriter::resume`. The multi-chunk files carry
   a per-compression magic, so this probably fails at read time rather than
   misdecoding. That is the best case, and it is a late opaque I/O error
   instead of a checkpoint refusal. Single-section chunk paths
   (`write_chunk_records_presorted`) were not checked for a magic.

4. **`parse_byte_size` overflows.** `v * 1024 * 1024 * 1024` on
   user-supplied `usize` panics in debug and wraps in release. For example,
   `--sort-budget 99999999999G` wraps to a small number, which then passes
   or fails the `>= 64M` check arbitrarily. Use `checked_mul`.

5. **`--locations-on-ways` can be forced on a PBF without locations.**
   `locations_on_ways = config.locations_on_ways || header...`. If the
   header lacks the feature, ways resolve coordinates from
   `way.node_locations()`, which is presumably empty. Every way is then
   dropped silently, and provenance says `coordinate_source: inline`. Either
   refuse the flag when the header disagrees, or delete the flag. Header
   detection already covers the legitimate case.

6. **Data bounds come from node extent only.** In locations mode, if the
   producer stripped untagged nodes, extent tracking sees only tagged nodes.
   The ocean band and the artifact inner rect are then computed from a bbox
   smaller than the ways' real extent. I don't know whether pbfhogg's
   add-locations-to-ways keeps untagged nodes. Worth a one-line check. The
   comment "~1 degree buffer (... ≈ 0.003)" is also wrong for `buf = 0.01`,
   which is about 3.6 degrees.

7. **Ocean-artifact archives carry no `elivagar` provenance block.**
   `ocean_build` passes `RunProvenance::default()` and an empty `pbf_path`.
   `write_provenance` prints "no PBF file name - omitting provenance" and
   returns. Assemble also attempts `ElementReader::from_path("")`. Separately,
   `RunProvenance::default().ocean_mode` is `""`, a value outside the
   `none|shapefile|artifact` vocabulary.

---

## 1. Axes that should be types

### 1.1 `TilegenConfig` is one flat struct holding five different things
It mixes:
- **inputs**: pbf, three ocean `Option<PathBuf>`s;
- **the producer contract**: zooms, fanout caps, simplify factor;
- **the tile contract**: format, compression, level;
- **resources**: budgets, threads, dedup cap, `in_memory`;
- **execution mode**: `skip_to`, `tmp_dir`, `force_sorted`,
  `allow_unsafe_flat_index`, `locations_on_ways`;
- **artifact-builder-only flags**: `ocean_artifact_key`,
  `ocean_only_metadata`.

Consequences:
- Which fields affect chunks is decided by hand-listing them in
  `provenance::producer_config`. That list is guarded by two pairwise tests
  (`producer_config_excludes_assemble_side_settings`,
  `producer_config_covers_chunk_affecting_settings`), which catch only the
  fields they name. `compress_sort_chunks` (finding 0.3) is exactly the
  field that slipped through.
- `ocean_build` has to invent a full `TilegenConfig` with an empty
  `pbf_path`, `ocean_shapefile: None` while the run is all ocean, and so on.
- Library callers can construct states the CLI never would: simplified
  without full (silently no ocean), `ocean_tiles` without shapefiles, MLT
  with brotli, `polygon_simplify_factor = NaN`, `min_zoom > max_zoom`. Some
  are rejected late in `run()`, some only in `main.rs`, and some never.

Proposed types:
- `ProducerContract { zooms: ZoomRange, fanout_caps: LayerMap<FanoutCap>,
  polygon_simplify: SimplifyFactor }`. It derives Serialize and PartialEq
  and is the checkpoint's comparison key. The type is the classification,
  so the pairwise tests go away.
- `TileContract` as an enum: `Mvt { compression: MvtCompression, level:
  CompressionLevel } | Mlt`. MLT plus gzip becomes unconstructable, which
  fixes 0.1 by construction.
- `ChunkFormat { compression }`, recorded in the checkpoint (fixes 0.3).
- `OceanPlan` (see 2.1) replaces the three `Option<PathBuf>`s plus the
  artifact key plus `ocean_only_metadata`.
- `Resources { sort_chunk: ByteBudget, way_inflight: Option<ByteBudget>,
  assemble_batch: ByteBudget, dedup_cap: NonZeroUsize, threads:
  NonZeroUsize }`, with defaults resolved once in a constructor. Today
  `0 = default` is a sentinel resolved in four places:
  - sort chunk size twice. `DEFAULT_SORT_CHUNK_SIZE` is defined twice, in
    `mod.rs` and `phase12.rs`.
  - assemble budget twice. `32 * 1024 * 1024` appears literally in both
    `phase_assemble_with_ocean` and `phase_assemble_partitions`.
  - way budget in phase12.
  - dedup cap in the writer.
- `Output` as an enum: `Tileset | OceanArtifact { key }`. This replaces the
  `ocean_artifact_key` / `ocean_only_metadata` pair, which today can
  disagree (key set with full metadata, or the reverse).

### 1.2 `Effective` is three `&'static str` fields with an allowlist duplicated at load
`phase12.rs` mints `"inline"/"node_store"`, `"injected_v1"/"relation_scan"`
and `"injected_v1"/"block_local"`. `load_checkpoint` keeps its own allowlist
(`path_name(..., ["inline", "node_store"])`) to "intern back".
`PinSource` and `NodeStoreMode` already exist as enums in phase12, so the
strings are a lossy re-encoding of types that already exist.

Fix: make `CoordinateSource`, `MembershipSource` and `PinScope` enums with
`as_str`/serde, stored in `Effective`. phase12's `PinSource` and
`MemberSource` should simply be these, and the checkpoint allowlist
disappears.

### 1.3 Ocean mode has three vocabularies
`OceanMode::{None, Computed, Band}` is serialized three ways:
- as `"none" / {"computed"} / {"band"}` in the checkpoint;
- as `"none" / "shapefile" / "artifact"` in provenance, via a `match` in
  `run()` passed as `&'static str` through `RunProvenance.ocean_mode`;
- `write_provenance` then string-compares `mode == "none"`.

Make `OceanMode` carry its provenance name and pass the enum, not the
string. `OceanMode` is also `pub` from `lib.rs` although nothing outside
the crate uses it.

### 1.4 `resumed_from: Option<&'static str>`
`SkipTo` is mapped to strings in `run()`. Pass `Option<SkipTo>` and give
`SkipTo` an `as_str`. `SkipTo` and `SkipToArg` are also duplicate enums
with a hand mapping in `main.rs`. The same goes for `TileFormatArg`,
`TileCompressionArg` and `SortChunkCompressionArg`. Derive `ValueEnum` on
the library enums behind a `cli` feature, or keep the mapping in one `cli`
module.

### 1.5 Layer x zoom index as raw `usize` arithmetic
`layer * 15 + z` is computed in:
- `emit.rs` (`polygon_z_bottom_after_caps`, packed into a `u16`);
- `phase12.rs` (`harvest_way_cap_events`: `idx / 15`, `idx % 15`);
- `relations.rs` (same unpack);
- `stats.rs` (5 sites);
- `mod.rs` (`i * 15 + z`).

Arrays are sized `32 * 15` while `Layer::count()` is 26, and `layer >= 32`
guards appear next to them. `cap_events: Vec<(u16, u64, u64)>` and
`top_capped: Vec<(u64, u8, u8, u64)>` are bare tuples whose fields
(`bbox_tiles`, `osm_id`) are the same primitive and can be swapped.

Fix:
- `LayerZoom(Layer, Zoom)` with `fn index()`.
- `LayerZoomTable<T>` sized `Layer::count() * ZOOM_COUNT`.
- `CapEvent { at: LayerZoom, bbox_tiles, osm_id }` as a struct.

### 1.6 Zoom as `u8` with 14 and 15 scattered
The maximum zoom is a domain constant that appears as:
- `config.max_zoom > 14` (run);
- `OSM_POLYGON_MAX_Z = 14` (emit);
- the literal `z >= 14` (line emitter, twice);
- `max_zoom: 14` (resolve_boundary_match);
- `0, 14` (artifact contract in `resolved_ocean_mode`, `ocean_build`,
  `OceanTiles::open`);
- `[u64; 15]` (assemble per-zoom arrays);
- `0..15u8` (counters);
- `zoom >= 15` (stats).

A `Zoom` newtype, a `MAX_ZOOM` const, and a `ZoomRange` that validates
`min <= max <= MAX` at construction would remove the run-time check and
every literal. The artifact's zoom contract belongs as a const on
`OceanArtifactKey`.

### 1.7 Input identity is `(String, u64)`
`input_identity: Option<(String, u64)>` is threaded through `run()`,
`RunProvenance` and the checkpoint, with `CheckpointProvenance.input_xxh3_128:
String` beside it. Use `InputIdentity { xxh3_128: Xxh3Hex, bytes: u64 }`.

### 1.8 `MercBbox` bounds through the checkpoint as four bare floats
`[min_x, min_y, max_x, max_y]` is positional JSON. The rest of the
checkpoint is hand-rolled `serde_json::json!` plus `doc["field"]` lookups.
A `#[derive(Serialize, Deserialize)] struct Checkpoint` would remove about
130 lines of hand parsing and the `stale()` plumbing. The version stays an
explicit field.

### 1.9 Errors reach callers only as prose
`PipelineError(String)`. The doc says "no caller inspects variants", but
brokkr is a machine caller through the exit code, and every failure exits 1:
- a resume refusal (checkpoint for another PBF, producer drift, ocean-mode
  drift);
- a contract refusal (artifact cannot serve this run, compression-level
  mismatch);
- an unreadable input;
- an I/O failure mid-assemble.

Meanwhile, `main.rs` exits 2 for some invocation errors (`--ocean`, MLT
without the feature) and 1 for others (`--fanout-cap` syntax, unknown layer,
`--polygon-simplify-factor` out of range). brokkr cannot tell "your
invocation is wrong" from "your checkpoint is stale" from "disk failed".

Fix: an enum with `InvalidConfig`, `ResumeRefused { reason }`,
`ArtifactRefused`, `InputUnreadable`, `CorruptInput` and `Io`, with one
mapping to exit codes in `main`. That is the same pattern the corpus gate
uses: 1, 2 and 3 have meanings.

Prose-only telemetry is a related problem. Some outcomes are visible only
on stderr, which brokkr does not read under `--bench`:
- "warning: input PBF not identified - omitting provenance";
- "no sort checkpoint found - cannot verify chunk integrity";
- the relation re-read spill notice (that one also has a counter).

The provenance omission in particular should be a counter or a hard error.

### 1.10 Assemble returns a 6-tuple; the writer thread returns another 6-tuple
`(u64, u64, u64, usize, DedupStats, TileSizeDiagnostics)` holds three
adjacent `u64`s: features, tiles and unique tiles. Swapping two of them
compiles. Return a struct, `AssembleReport`.

---

## 2. Decisions made in more than one place

### 2.1 What are the ocean passes, and which shapefile and grid serve each zoom?
Sites:
1. `main.rs resolve_ocean_inputs`: the accepted partitions (z0-z14 alone, or
   z0-z7 + z8-z14).
2. `run()` ocean block: always a low pass `min..=min(max,7)`, sourced from
   simplified-or-full, plus a high pass `max(min,8)..=max`.
3. `ocean_build`: hard-coded `0,7` and `8,14`, same source rule ("Mirror the
   run pipeline's pass structure" is a comment, not a shared function).
4. `ocean_pass_max_zooms`: one grid when no simplified shapefile.
   **Disagrees with sites 2 and 3** (finding 0.2).
5. `ocean::selected_pass_grid`: `len()==2 && z<=7`.
6. `write_provenance low_zoom_source`: explicitly "mirrors" site 4.
7. `OceanSourceKey::from_inputs` and `OceanArtifactKey::from_inputs`: take
   `(full, Option<simplified>, min, max)` and re-derive.
8. `resolved_ocean_mode`: "ocean runs only when the full shapefile is
   present" (simplified-only means no ocean). The comment in
   `write_provenance` restates the same rule.

Owner: an `OceanPlan` built once from the parsed `--ocean` specs. It holds
`passes: Vec<OceanPass { zooms: ZoomRange, source: PathBuf, grid_max_zoom }>`
and `artifact: Option<ArtifactRef>`. `run()`, `ocean_build`,
`OceanTiles::open` (grids from the plan's passes), `selected_pass_grid`
(lookup by zoom in the plan), the provenance `low_zoom_source`, and both
keys all read from it. The 7/8 split becomes one const on the plan.
`resolve_ocean_inputs` should live in the library as `OceanPlan::parse`, so
`main` stops owning a domain rule that `ocean.rs` comments refer back to.

### 2.2 What is the tile contract?
Sites:
- `assemble.rs`: MLT forces compression None.
- `provenance::config_json`: records `config.tile_compression` raw.
- `run()` counters: same raw value.
- `resolved_ocean_mode`: checks format and compression independently.
- `OceanTiles::open`: checks `tile_type()==1 && tile_compression()==2` as
  raw header integers, a re-check against the file. That one is fine.
- `compression_level_for_zoom` plus `"compression_policy": "zoom-v1"` in
  provenance: the per-zoom level policy lives in assemble, and its version
  string lives in provenance, kept in step by hand.

**Already disagree** (finding 0.1). Owner: the `TileContract` enum (1.1).
Its `provenance_json()` and `pmtiles_header()` are the only translators,
and the zoom policy lives next to its version string.

### 2.3 Which config fields does a resume have to match?
Sites:
- `provenance::producer_config`: zooms, caps, factor.
- `run()`: separately compares `OceanMode` (twice for `--skip-to ocean`,
  once at the top and again in the branch) and the input hash.
- Chunk compression: not compared anywhere (0.3).
- `effective` is inherited, never compared.

Owner: `Checkpoint::verify(&self, &RunPlan) -> Result<(), ResumeRefused>`
over the typed producer contract, the chunk format, the ocean plan identity
and the input identity.

Note that `--skip-to ocean` refuses an ocean-mode change, yet re-running the
ocean is the point of that resume. The ocean producer identity matters for
`sort|assemble` resumes, not for `ocean` ones. Worth deciding deliberately.

### 2.4 Which relations produce area geometry?
`prepass_relation_plan` (phase12) and `prepare_relation` (relations.rs) each
re-implement:
- the `type in {multipolygon, boundary}` gate, as identical hand-rolled
  loops;
- `match_element(MultiPolygon)`.

The prepass decides which ways enter `way_index`, and the tail decides which
relations read them back. If the two drift, members go missing and are
counted as `missing_relation_way_refs`, which reads as input damage rather
than a code bug. A third answer comes from outside: the injected
WayMembers-v1 bitmap uses superset semantics, which the
`superset_ways_count` counter quantifies.

Owner: one `fn area_relation_matches(tags) -> Option<SmallVec<LayerMatch>>`
in `shortbread`, called by both.

### 2.5 Which admin levels are boundaries, and what does a boundary line match look like?
Admin levels:
- `shortbread::boundaries::boundary_min_zoom` (`2 => 0, 4 => 7`);
- `boundary_prescan::fold_relation` (`matches!(level, 2 | 4)`, parsed as
  `u8` where `boundary_match` uses `parse_i64`);
- `resolve_boundary_match` `.expect("boundary metadata only contains
  supported levels")`. That expect is the drift detector. If the prescan's
  set gains a level that `boundary_min_zoom` lacks, phase12 panics.

The `LayerMatch` for a boundary line (max_zoom 14, paint_rank 0, attr order,
the maritime rule `maritime=yes || natural=coastline`) is built in both
`boundaries::match_boundaries_line` and `phase12::resolve_boundary_match`.

Owner: `shortbread::boundaries` exposes `supported_level(u8) ->
Option<MinZoom>` and `boundary_line_match(level, maritime, disputed)`. The
prescan and phase12 call these. `resolve_boundary_match` as a whole
(combining own-tag and relation meta) is Shortbread semantics and belongs in
`shortbread/boundaries.rs`, not in the PBF reader.

### 2.6 How does a `LayerMatch` become records? (geometry dispatch)
Way path (`process_planned_way_into`) and relation path
(`process_prepared_relation_into`) each:
- `match m.geom_expect`;
- clamp `z_lo/z_hi` against config;
- fetch the fanout cap with `fanout_caps.get(m.layer as usize)
  .copied().unwrap_or(0)`;
- run their own antimeridian `for shift in antimeridian_shifts_for_bbox`
  loop with a hand-built shifted copy;
- harvest cap events with an identical `/15 %15` loop and a "clear after
  harvest or double-count" comment, duplicated.

The relation dispatch ends in `_ => {}`, so `GeomExpect::Point` and `Line`
on a relation match are silently dropped. A new `GeomExpect` variant would
also be dropped silently rather than failing to compile.

Owner: one `emit_match(osm_id, FeatureGeometry::{Way{..}, Area{outer,
inners}}, &LayerMatch, &EmitCtx, &mut Acc)` in `emit.rs`, with an
exhaustive match, owning the shift loop, the cap lookup and the cap-event
harvest. `EmitCtx { zooms, fanout_caps, simplify }` replaces 3 to 4
positional arguments that currently travel separately through
`process_relation_blocks`, `process_planned_way_into` and every emitter.

### 2.7 Per-layer geometry policy inside the emitter
`emit.rs` decides layer exemptions by name:
- `polygon_min_area`: `StreetPolygons | Bridges` have no min area;
- `emit_line_feature`: `Boundaries | Streets` skip the subpixel filter,
  twice, as `skip_size_filter` and `skip_bbox_check`.

These are Shortbread profile facts. They should be attributes of the layer
definition (`Layer::geometry_policy()`) so the profile owns them and the
emitter has no layer names.

### 2.8 Where the default for each budget lives
See 1.1. There are two `DEFAULT_SORT_CHUNK_SIZE` consts and two literal
`32 MB` assemble defaults. The `--way-budget` help text in `main.rs`
restates `128M`/`4G` defaults that live in `phase12.rs`. The dedup default
"1000000" in help text restates `MAX_DEDUP_ENTRIES`.

### 2.9 Which MVT decoder is the authoritative one?
`main.rs` carries its own MVT polygon decoder (`diag_decode_polygon`,
`unzigzag_diag`, its own ClosePath handling, its own simplicity test). Its
comment says it exists "to avoid needing pub access to geometry module", but
`tile_detail` is now the public wire-order decoder brokkr consumes. A second
private copy of the ClosePath cursor rule is exactly the R23 bug class.
`diag` should decode with `tile_detail`. `svg.rs` presumably has a third
copy, which is worth checking.

### 2.10 Data bounds and checkpoint state
`run()` calls `load_checkpoint` up to four times in one run (resume guard,
the `--skip-to ocean` branch, data bounds, late artifact open), each
re-parsing the file. On a full run it reads back what phase12 returned a
moment earlier (`bounds_out` is discarded and re-read). This is one value
with several owners. Phase12 should return a typed `ChunkSet { bounds,
chunk_count, ... }`, a resume should load the same type once, and the rest
of `run()` should hold it.

---

## 3. Structure

### 3.1 `run()` is an 620-line function doing four jobs
It does:
1. Resume and contract validation.
2. Phase sequencing with `Option` juggling: `ocean_elapsed`, `phase12_rss`,
   `sort_writer: Option<Option<..>>`-like states, `active_ocean_artifact`
   opened in one of two places depending on the skip.
3. Stderr reporting.
4. About 300 lines of sidecar counter emission that knows the internals of
   `Phase12Stats`, `FanoutStats`, `DedupStats` and `TileSizeDiagnostics`.

Proposed shape:
- `RunPlan::resolve(config) -> Result<RunPlan, PipelineError>`: every
  refusal, the ocean plan and the resume verification, before any work.
- An explicit phase state machine: `enum Start { Full, ResumeOcean(ChunkSet),
  ResumeSort(ChunkSet), ResumeAssemble(ChunkSet) }`, each producing the next
  typed artifact (`ChunkSet` to `SortReader` to `AssembleReport`). The
  artifact opens in exactly one place.
- Counter emission as `impl Phase12Stats { fn emit(&self) }`, `impl
  AssembleReport { fn emit(&self) }` and so on in `stats.rs`. The owner of a
  struct's fields owns their counter names. The `layer_stats_enabled()`
  firehose gate goes with them. `ocean.rs` independently reads the same env
  var into its own `OnceLock`, which is a small duplicate of the same
  decision.

### 3.2 `phase12.rs` (2200 lines) holds at least six subsystems
- PBF block routing (two `macro_rules!` that mutably capture about 20
  locals, because no struct owns the phase state);
- the node worker;
- the way admission and dispatch actor with its condvar budget;
- way plans and shared-node pin detection;
- the relation prepass (which belongs with `relations.rs`, see 2.4);
- data-extent tracking (eight `min/max_*_e7` locals merged by hand in the
  join macro; should be an `Extent` struct with `merge`, which the node
  worker state would also use);
- boundary resolution (Shortbread logic, see 2.5).

The macros exist because the state has no owner. A `Phase12` struct holding
the channels, handles, tallies and extent turns `route_block!` and
`join_node_worker!` into methods. Split into
`phase12/{mod,router,node_worker,way_worker,plans,extent}.rs`, with the
prepass in `relations/`.

### 3.3 `assemble.rs` has two complete assemble engines
- `phase_assemble_with_ocean`: the legacy single-reader k-way path with
  reader, encoder and writer threads, about 300 lines.
- `phase_assemble_partitions`: the parallel partitioned path.

Which one runs depends on whether `sort_reader.take_partitions()` returns
`Some`. Both duplicate:
- the writer's per-zoom accounting;
- the batch budget default;
- `BATCH_SIZE`;
- the encode context.

If every `SortReader` produced by the pipeline now has partitions (writers
partition at write time per AGENTS.md), the legacy path is dead weight
carrying its own copy of the batching and accounting rules. Verify, then
delete it, or make the non-partitioned case a single-partition union.

`assemble.rs` also hosts:
- the ocean-layer MVT splicer (`ocean_layer_field`, `splice_ocean_layer`,
  `read_varint`, `raw_contains_ocean`): a hand-written protobuf walker, yet
  another decoder (see 2.9);
- provenance assembly (`write_provenance`);
- the zoom compression policy;
- MLT layer preparation.

Recommended split:
- `assemble/{schedule (union, split, claim window), encode (MVT/MLT,
  compression policy), artifact (run copy, ocean splice), report}`.
- `write_provenance` moves to `provenance.rs`, taking typed inputs.

### 3.4 Dependency edges point the wrong way
- `sort.rs` imports `crate::pipeline::emit::RecordTally`. The infrastructure
  module depends on the orchestrator's emitter. `RecordTally` describes
  sort-record counts and belongs in `sort.rs`.
- `provenance.rs` (public, linked by brokkr) depends on `TilegenConfig`
  (pipeline), while the pipeline depends on provenance. Provenance should
  take the typed contracts (`ProducerContract`, `TileContract`,
  `OceanPlan`), not the whole run config. Then the public provenance surface
  no longer drags the pipeline's config through brokkr's API.
- `TilegenConfig` exposes `sort::ChunkCompression`, which forces `pub mod
  sort`. Likewise `shortbread::Layer::count()` for the cap array. The public
  surface is wider than the API needs because the config leaks internal
  types.
- `emit` is `pub(crate) mod` with `pub(super)` items. It is the
  feature-to-sort-record layer (geometry plus MVT command encoding plus sort
  keys), shared by the way and relation paths, and is not
  orchestration. It sits more naturally as a top-level `emit` module beside
  `geometry`, with `pipeline` consuming it.

### 3.5 CLI and library validation are split
Validation that is domain logic lives only in `main.rs`:
- `--ocean` spec parsing and the partition rule;
- `--fanout-cap` parsing and layer-name resolution;
- the `polygon_simplify_factor` range check;
- the MLT feature check;
- `threads` defaulting.

Library callers (brokkr, tests, `ocean_build` itself) bypass all of it. Move
it into constructors on the typed config (1.1), so `main` is clap structs
plus one `TryFrom<RunArgs>`. `main.rs` also hosts the 270-line `diag`
implementation (2.9), which belongs in the library beside `svg`/`inspect`,
or should be reduced to a `tile_detail` printer.

`ocean_build` and `retain_outgoing_ocean_artifact` / `yyyymmdd_utc` /
`is_retained_artifact_name` sit in `pipeline/mod.rs`. They are artifact
lifecycle code and belong in an `ocean_artifact` module (or in `ocean.rs`
next to `OceanTiles`).

### 3.6 Test layout mirrors module privacy, not the design
`pipeline_tests.rs` (3741 lines) is one `#[path]` file that reaches private
items through a block of `#[cfg(test)] use` re-exports in `mod.rs`. Each
private helper a test touches must be re-exported from the orchestrator's
root. It mixes:
- injected-PBF fixture builders (hand-written protobuf splicing,
  `splice_way_members`, `rebuild_blobheader_datasize`);
- emitter decode tests;
- assemble metadata tests;
- checkpoint tests;
- artifact-retention tests;
- boundary-label tests (which test `shortbread` enrichment).

A test of `emit_polygon_feature` lives with tests of `yyyymmdd_utc` because
both happen to be under `pipeline`. Once the modules are split per 3.2 and
3.3, the tests should sit in each module's own `#[cfg(test)] mod tests` or a
sibling `*_tests.rs`. The PBF fixture builders belong in a shared
`test_support` module, since they are a reusable synthetic-PBF writer. The
pairwise `producer_config_*` tests disappear once the producer contract is a
type (1.1).

---

## 4. Recommended direction

Rewrite the config and orchestration layer. Don't patch it.

1. **Typed run plan.** `RunPlan { input: InputRef, ocean: OceanPlan,
   producer: ProducerContract, tile: TileContract, chunks: ChunkFormat,
   resources: Resources, exec: Execution, output: Output }`, built by
   fallible constructors that hold every validation now spread across
   `main.rs`, `run()` and `resolved_ocean_mode`. `TilegenConfig` as a public
   flat struct goes away, or becomes a thin serde DTO that only converts
   into `RunPlan`.
2. **Typed checkpoint.** A serde `Checkpoint` storing the producer
   contract, chunk format, ocean identity, input identity, effective enums
   and bounds. It is loaded once and verified by one function returning a
   typed `ResumeRefused`.
3. **Single owners** for the ocean pass plan (2.1), the tile contract
   (2.2), area-relation selection (2.4), boundary semantics (2.5, in
   `shortbread`), match-to-records dispatch (2.6, in `emit`) and layer
   geometry policy (2.7, in `shortbread`).
4. **Orchestrator as a phase state machine** with typed hand-offs. Counter
   emission moves to the stats owners, and `PipelineError` becomes an enum
   with a defined exit-code mapping.
5. **Split phase12 and assemble** along their real seams (3.2, 3.3). Delete
   the legacy single-reader assemble engine if it is unreachable.
6. **Fix the dependency inversions** (3.4): `RecordTally` to `sort`,
   provenance over contracts not the run config, `emit` out of `pipeline`.

Fix findings 0.1, 0.2 and 0.3 regardless of whether the rewrite happens.
Each is a live disagreement, not a hypothetical one.
