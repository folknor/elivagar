# Structural

Shape and placement findings from the 2026-10-28 design hunt: axes that
should be types, and the moves, splits and rewrites, each with the payoff a
hunter claimed for it. Where a type proposal is the owner of a duplicated
decision it lives with that decision in `notes/consolidations.md`, not here.

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

# Run configuration and orchestration

## STR-001 - Replace the flat `TilegenConfig` with a typed run plan

`TilegenConfig` is one flat bag of primitives holding inputs (pbf, three
ocean `Option<PathBuf>`s), the producer contract (zooms, fanout caps,
simplify factor), the tile contract (format, compression, level), resources
(budgets, threads, dedup cap, `in_memory`), execution mode (`skip_to`,
`tmp_dir`, `force_sorted`, `allow_unsafe_flat_index`, `locations_on_ways`,
`output_path`), and artifact-builder-only flags (`ocean_artifact_key`,
`ocean_only_metadata`). Library callers can construct states the CLI never
would: simplified-without-full (silently no ocean), `ocean_tiles` without
shapefiles, artifact key together with `ocean_tiles` (builder and consumer at
once), `ocean_only_metadata` with OSM layers, MLT with brotli,
`polygon_simplify_factor = NaN` (which would serialize into the contract),
`min_zoom > max_zoom`. Some are rejected late in `run()`, some only in
`main.rs`, some never. `main.rs` already has the right shape for ocean
(`OceanInputs { low, high, artifact }`, validated) and then flattens it back.

Proposed by all four hunts that touched it, in compatible shapes:
`RunPlan { input, ocean: OceanPlan, producer: ProducerContract, tile:
TileContract, chunks: ChunkFormat, resources: Resources, exec: Execution,
output: Output }` (pipeline hunt) or
`RunSpec { input, contract: ContractConfig }` + `Resources` +
`Action { output, skip_to }` (run-contract hunt), built by fallible
constructors that hold every validation now spread across `main.rs`, `run()`
and `resolved_ocean_mode`. `ProducerContract` derives Serialize + PartialEq
and is the checkpoint comparison key, so the pairwise `producer_config_*`
tests go away. `TilegenConfig` disappears or becomes a thin serde DTO.
Payoff: a new contract-relevant setting cannot be added without landing in
provenance (today it silently defaults to "not in the contract").

Reported by the pipeline, run-contract, ocean and storage hunts.

## STR-002 - Make `run()` a phase state machine with typed hand-offs

`run()` is a roughly 620-line function doing resume and contract validation,
phase sequencing with `Option` juggling (`ocean_elapsed`, `phase12_rss`,
nested-option writer states, the artifact opened in one of two places
depending on the skip), stderr reporting, and about 300 lines of sidecar
counter emission that knows the internals of `Phase12Stats`, `FanoutStats`,
`DedupStats` and `TileSizeDiagnostics`. Assemble returns a 6-tuple
`(u64, u64, u64, usize, DedupStats, TileSizeDiagnostics)` with three adjacent
`u64`s (features, tiles, unique tiles) and the writer thread another; the
drain thread returns `(WayIndex, SortWriter, u64, FanoutStats)`.

Proposed: `RunPlan::resolve(config)` performs every refusal before work;
`enum Start { Full, ResumeOcean(ChunkSet), ResumeSort(ChunkSet),
ResumeAssemble(ChunkSet) }`, each phase producing the next typed artifact
(`ChunkSet` -> `SortReader` -> `AssembleReport`); the artifact opens in
exactly one place; phase12 returns a `ChunkSet { bounds, chunk_count, .. }`
that a resume loads as the same type. Counter emission moves to
`impl Phase12Stats { fn emit }` and friends, so a struct's owner owns its
counter names.

Reported by the pipeline hunt.

## STR-003 - Typed errors with a defined exit-code mapping

Every failure is prose. `PipelineError(String)` carries resume refusals,
contract refusals, unreadable input and mid-assemble I/O, all exiting 1;
`main.rs` exits 2 for some invocation errors (`--ocean`, MLT without the
feature) and 1 for others (`--fanout-cap` syntax, unknown layer, simplify
factor range). brokkr - which links the crate and already gives exit codes
1/2/3 meaning in the corpus gate - cannot tell "your invocation is wrong" from
"your checkpoint is stale" from "disk failed", except by substring. The same
shape recurs per subsystem:

- ocean: about a dozen artifact refusals as `io::Error::other` strings; the
  key-mismatch message does not say which field differs, though "policy
  version 4 vs 5" (rebuild) and "full shapefile hash differs" (re-download)
  are different operator actions;
- sort: `io::Error::new(InvalidData, format!(..))` for compression mismatch,
  stale chunks, count mismatch, duplicate ids, partition out of range;
- node store: a panic on the flat-index safety cap;
- multipolygon: unclosed chains dropped (`join_ways` returns them, both
  callers bind `_`), orphan inners promoted, degenerate rings dropped - all
  invisible;
- archive: `tile_detail` returns `Result<_, String>` everywhere,
  `pmtiles_reader` uses `io::Error::other(String)` (its own tests assert with
  `err.to_string().contains("overflow")`), `verify` collects
  `tile_errors: Vec<String>` and tests assert on prefixes,
  `VerifyError::{Container(String), Metadata(String)}`;
- encoding: `MltEncodeError::Encode(String)`, `GeometryDecode { message }`,
  `check_merge -> Result<(), String>` whose tests match prose fragments;
- profile: `validate_paint_order` returns `String`s;
- geometry: `partition_shape_to_ring_cap` `expect`s on an indivisible rect,
  aborting a planet run; the `.expect("... fits i32")` overflow invariants
  rest on `maxz + 12 <= 30`, which no type states; `enforce_ring_cap`
  returns a bool its only caller discards;
- the sort-record decoder silently returns or breaks on every malformed case,
  so a code bug yields a quietly wrong tile rather than a failure;
- prose-only telemetry brokkr never reads under `--bench`: "input PBF not
  identified - omitting provenance", "no sort checkpoint found - cannot
  verify chunk integrity", the relation re-read spill notice.

Proposed enums (from the hunts): `PipelineError { InvalidConfig,
ResumeRefused { reason }, ArtifactRefused, InputUnreadable, CorruptInput, Io }`
with one exit-code mapping in `main`; `RunRefusal` /
`ArtifactRefusal::KeyMismatch { field, declared, expected }`;
`ChunkStoreError { CompressionMismatch, StaleChunks, MissingChunk, Corrupt }`;
`NodeStoreError::FlatCapExceeded`; `AssemblyReport { unclosed_chains,
promoted_orphans, degenerate }` beside the missing-ref counters;
`DecodeError { locus, kind: UnknownField | Truncated | Utf8 | IndexOutOfRange
| CoordinateOverflow | BadCommand }`; `ArchiveError`; `TileFinding { coord,
layer, feature, kind }` making `MAX_TILE_ERRORS` a presentation choice;
`PaintOrderViolation`; `MergeViolation`; `EmitError::RingCapIndivisible`;
a typed or counted `RecordError` in the sort decoder.

Reported by every hunt.

## STR-004 - The ocean artifact builder should not be a faked tilegen run

`ocean_build` constructs a full `TilegenConfig` with `pbf_path:
PathBuf::new()`, zeroed caps, `locations_on_ways: false` and two builder-only
flags (`ocean_artifact_key`, `ocean_only_metadata`, which can disagree with
each other) so it can call `assemble::phase_assemble`. That pushes builder
concerns into the run config and into assemble (`set_ocean_only_metadata`,
the `ocean_artifact` metadata member, `write_provenance`'s "the artifact
builder supplies its own key via config" branch), and it is why artifacts
carry no provenance block.

Proposed: factor "chunks to archive" out of assemble with an
`ArchiveRole { Run(RunProvenance), OceanArtifact(OceanArtifactKey) }` /
`Output::{Tileset, OceanArtifact { key }}` /
`AssembleSpec { format, compression, zooms, metadata: MetadataSpec }`
argument that both `run` and the builder construct; the builder never sees
`fanout_caps`. Pre-1.0 and with a policy bump the normal way to rotate the
artifact, the ocean hunt also proposes folding the top-level `ocean_artifact`
metadata member into a typed, versioned `elivagar.ocean_artifact` member of
the same provenance schema, so one reader parses both (`provenance.rs`
currently explains it cannot move "because it would invalidate every artifact
already built").

Reported by the pipeline, run-contract and ocean hunts.

## STR-005 - Move domain validation from `main.rs` into the library

Only `main.rs` holds: `--ocean` spec parsing and the partition rule,
`--fanout-cap` parsing and layer-name resolution, the
`polygon_simplify_factor` range check (`process::exit` from inside a struct
literal), the MLT feature check, and `threads` defaulting. Library callers
(brokkr, tests, `ocean_build` itself) bypass all of it, and brokkr's
`--dry-run` cannot reach the ocean resolver. `main.rs` tests the ocean
partition rules because that is where the parser lives. `SkipTo`/`SkipToArg`,
`TileFormatArg`, `TileCompressionArg`, `SortChunkCompressionArg` duplicate
library enums with hand mappings. `main.rs` also hosts the 270-line `diag`
implementation.

Proposed: constructors on the typed config (`OceanPlan::parse`,
`FromStr for Layer` / `LayerCapSpec`, a validated `SimplifyFactor` holding the
f64 bits); `main` becomes clap structs plus one `TryFrom<RunArgs>`; derive
`ValueEnum` on the library enums behind a `cli` feature or keep mappings in
one `cli` module; `diag` moves into the library beside svg/inspect or shrinks
to a `tile_detail` printer.

Reported by the pipeline, run-contract and layer-identity hunts.

## STR-006 - Typed provenance with serde in both directions

`provenance.rs` has `Input`, `PbfFeatures`, `OceanContract`, `Effective` with
hand-written `to_json` and no `from_json`; the library hands brokkr
`ContractDoc { input: Value, config: Value, build: Value }`, so "typed
provenance in full" is typed only on the write side and every reader
(inspect's `print_contract`/`print_diagnostics`/`pbf_feature_list`/
`repo_display`, brokkr's gate) re-learns key names as string literals.
`producer_config` and `config_json` are two hand-written `json!` literals
that must list the same fields; the producer config is compared as JSON "so
the float needs no float-equality dance". `"profile": "shortbread"` is a
literal; `repo_json(commit, dirty)` parses `"true"`/`"false"` from `env!`
(better once in `build.rs` into `Option<bool>`); `SCHEMA_VERSION: u32` is
compared against `as_u64` in two places.

Proposed: `#[derive(Serialize, Deserialize)] struct ProvenanceV1 { schema,
input, config, build, effective: Option<_>, execution }` with additive
members allowed and snake_case enums; `extract_contract` returns
`Result<ProvenanceV1, ContractError>` with the no-contract cases as error
variants; brokkr may still re-serialize for field-named diffs. Dependency
direction flips: provenance takes typed contract parts it owns rather than
picking fields out of `TilegenConfig` (see STR-008). `write_provenance` moves
from assemble into `provenance.rs` taking typed inputs.

Reported by the archive-surface, run-contract and pipeline hunts.

## STR-007 - Split `phase12.rs` and `assemble.rs` along their real seams

`phase12.rs` (about 2200 lines) holds PBF block routing (two `macro_rules!`
that mutably capture about 20 locals because no struct owns the phase state),
the node worker, the way admission and dispatch actor with its condvar
budget, way plans and shared-node pin detection, the relation prepass,
data-extent tracking (eight `min/max_*_e7` locals merged by hand in the join
macro), and boundary resolution. Proposed: a `Phase12` struct owning
channels, handles, tallies and extent so the macros become methods; an
`Extent` struct with `merge`; split into
`phase12/{mod,router,node_worker,way_worker,plans,extent}.rs` with the
prepass in `relations/`.

`assemble.rs` holds two complete engines - `phase_assemble_with_ocean` (the
legacy single-reader k-way path, about 300 lines) and
`phase_assemble_partitions` - selected by whether
`sort_reader.take_partitions()` returns `Some`, duplicating per-zoom writer
accounting, the batch budget default, `BATCH_SIZE` and the encode context. If
every `SortReader` the pipeline produces now has partitions, the legacy path
is dead weight: verify, then delete it or make it a single-partition union.
It also hosts the ocean-layer MVT splicer, provenance assembly, the zoom
compression policy and MLT layer prep. Proposed:
`assemble/{schedule, encode, artifact, report}`.

The ocean hunt adds: the splice decompresses the freshly encoded OSM tile to
append the artifact's ocean field and recompresses it. Injecting the
artifact's ocean layer bytes before compression (passing the raw layer field
into `encode_tile_batch_mvt` for non-band tiles) removes a gunzip+gzip per
spliced tile and the "unexpectedly contains computed ocean" decode.

Reported by the pipeline and ocean hunts.

## STR-008 - Fix the dependency edges that point the wrong way

- `sort.rs` imports `crate::pipeline::emit::RecordTally`: infrastructure
  depending on the orchestrator's emitter.
- `provenance.rs` (public, linked by brokkr) depends on `TilegenConfig` while
  the pipeline depends on provenance; the public surface drags the
  pipeline's config through brokkr's API.
- `TilegenConfig` exposes `sort::ChunkCompression` (forcing `pub mod sort`)
  and `shortbread::Layer::count()`; the public surface is wider than needed.
- `emit` is `pub(crate) mod` with `pub(super)` items, but it is the
  feature-to-sort-record layer shared by way and relation paths, not
  orchestration - it belongs top-level beside `geometry`.
- The geometry engine depends on the MVT encoder (`PyramidSink` receives
  encoded command streams; see STR-013) and on the pipeline's telemetry
  (`pyramid.rs` bumps `crate::debug::RING_CAP` atomics); return per-call
  stats and let the producer emit counters.
- `pmtiles_writer::build_metadata` imports `shortbread::Layer`, emits
  `"name":"Shortbread"` and `"type":"baselayer"`, and owns the ocean-only
  layer list; the container writer should not know the schema.
- `emit`, `ocean`, `svg`, `verify`, `main` and `inspect` import
  `pmtiles_writer` just to name a tile; `ocean` imports `sort` to learn a
  zoom (see STR-011).
- `mlt -> mvt`: MLT consumes the tile model through
  `cfg_attr(not(feature = "mlt"), allow(dead_code))` accessors in `mvt`.
- `shortbread -> pois -> shortbread`: `pois.rs` reaches back into
  `crate::shortbread::{attr_*, name_attrs, ..}`.

Reported by the pipeline, polygon-engine, archive-surface, coordinate-spaces,
encoding and Shortbread hunts.

---

# Ocean

## STR-009 - Give ocean one module tree

`ocean.rs` (about 2290 lines, about 1000 code) holds four jobs - artifact
identity (`OCEAN_POLICY_VERSION`, both keys, `hash_file`), the artifact
reader and band geometry (`OceanTiles`, `OceanPassGrid`, `ocean_band_tile`,
`selected_pass_grid`), a shapefile reader (shx parsing, mmap,
`read_exact_at`, ring role by signed area), and emission glue (union,
`ocean_params`, `OceanAcc`, sink, rayon splitting, stats atomics). Ocean
orchestration (pass split, artifact admission, retention, `ocean_build`,
`yyyymmdd_utc`, `is_retained_artifact_name`) lives in `pipeline/mod.rs`, and
artifact merging (run copies, splice, the hand-rolled protobuf field scanner)
in `pipeline/assemble.rs`. That split is why the pass decision has seven
sites.

Proposed:
```
src/ocean/
  mod.rs        OceanPlan (validated inputs, passes, grids, contract)
  policy.rs     OCEAN_POLICY_VERSION, ocean_params, union threshold, golden test
  key.rs        Xxh128, ShapefileHashes, OceanSourceKey, OceanArtifactKey
  shapefile.rs  .shp/.shx reader yielding projected polygons; no sort or pyramid deps
  emit.rs       union + pyramid emission into SortWriter, stats
  artifact/
    mod.rs      ARTIFACT_CONTRACT, admit() -> OceanArtifact, ArtifactRefusal
    band.rs     PassGrid, owns(z,x,y), root-range prefilter
    merge.rs    run-copy planning and splice (out of assemble.rs)
    build.rs    ocean_build + retention (out of pipeline/mod.rs)
```
Payoffs: the shapefile reader becomes testable without a `SortWriter` and
reusable; assemble stops interleaving partition scheduling with MVT field
surgery; `pipeline/mod.rs` stops knowing the z7 split. `OceanPassGrid` should
own its scale (`scale = 1 << (max_zoom + 12)` is written out in four places)
and expose `tile_rect(z, x, y)` / `snap(merc)`; pass identity should be a
`PassGrids { low: Option<Grid>, high: Grid }` or `grid_for_zoom(z)`, not a
vector index. `OceanTiles::open` reads PMTiles header bounds by raw offsets
(`read_i32_le(header, 102)`), which belong behind an archive accessor.
`OceanPassGrid::band_is_empty(min_zoom, max_zoom)` ignores both arguments.

Reported by the ocean and coordinate-spaces hunts.

---

# Geometry and coordinates

## STR-010 - Type the coordinate spaces

`IntPoint`, `Contour`, `Shape`, `Shapes` and `IntRect` are used unchanged in
base space (global pixels at `maxz`: `quantize_polygon*`, pyramid fragments,
`buffered_cell_rect_base`, `PyramidParams::pins`), zoom space (2^(z+12):
`rescale_shape_pinned_into` output, `buffered_tile_rect`, `thin_window_runs`,
`enforce_ring_cap`'s rect) and tile-local space (`(i32, i32)` after
`append_translated_ring` subtracts `tile_origin`). The only marker is the
`_base` suffix on some function names. Passing a base rect where a zoom rect
belongs type-checks and goes wrong only below `maxz`, where test geometry is
thinnest; `emit_cell` gets it right by hand. The descent "cut identity" holds
only because 4096 and 128 are exact under every shift - an invariant held by
hand. `maxz` travels as a bare `u8` beside each shape (a z7 and a z14 shape
can meet in one `intersect_shapes_into`). The float side has the same gap:
`Point` is "Mercator [0,1]" but `unwrap_antimeridian_path` and the
antimeridian shifts deliberately produce x outside [0,1] while ocean clamps
to `[0, scale]`; `ClipRect` and `MercBbox` are two float rects for one space;
`from_epsg3857`, `project_e7` and test-only `project` all mint `Point`.

Proposed: a `Grid { maxz }` value handing out typed `BaseRect`/`BaseShape`,
`ZoomRect { z }`/`ZoomShape`, `TileShape` (a const generic is awkward because
ocean uses maxz 7 and 14), with `quantize: Merc -> Base`,
`rescale(shift): Base -> Zoom`, `localize(tile): Zoom -> TileLocal` the only
tag-changing functions; the overlay engine stays generic. `UnwrappedMerc` vs
`Merc`, or at least a debug assertion at the `project_e7` boundary.

Reported by the polygon-engine, coordinate-spaces and ocean hunts.

## STR-011 - A coordinates module for zoom, tile ids and grids

There is no home for: `Zoom` (bare `u8` with 14 and 15 in about 80 places),
`ZoomRange`, `TileCoord` (`(u8, u32, u32)` everywhere except the
crate-private `PyramidCell`; `PyramidSink`, `tile_filter` and every sink take
bare triples), `TileId` (bare `u64` in sort, emit, assemble, ocean, verify,
svg, main and the writer, next to offsets and lengths of the same primitive in
`DirEntry`, `RawDirEntry`, `TileEntry`, `BlobRef`, the dedup tuple
`(u64, u32, u64)` and verify's `(u64, u32, u8, bool)` key), `SortKey`
(`type SortKey = u64`, so any u64 is a key; `make_sort_key(tile_id, layer:
u8, priority: u8)` takes two swappable u8s), `PartitionId` (bare `usize`, cast
to `u32` in section tables), per-zoom and per-layer-zoom arrays
(`[_; 15]` and `layer * 15 + z` hand-rolled in eight files, decoded by
`idx / 15`, `idx % 15` in three, packed into a `u16` for cap events; arrays
sized `32 * 15` with `layer >= 32` guards; `cap_events: Vec<(u16, u64, u64)>`
and `top_capped: Vec<(u64, u8, u8, u64)>` with swappable `bbox_tiles` and
`osm_id`), and `PayloadRecord = (SortKey, usize, usize)` destructured
positionally at about ten sites.

Proposed `src/coords/` (or `grid/`): `Zoom` with `MAX`/`COUNT`,
`ZoomRange`, `PerZoom<T>`, `LayerMap<T>`, `PerLayerZoom<T>`, `LayerZoom` with
`index()`, validated `TileCoord` and `TileId` with the only
conversions, `SortKey { tile, layer: Layer, rank: PaintRank }` with packing
internal, `PartitionId(u16)`, `PayloadRef { key, offset, len: u32 }` (also
shrinking the 24-byte record), `CapEvent { at: LayerZoom, bbox_tiles, osm_id }`,
the extent/buffer constants, the `Grid` of STR-010 and projection.
`pmtiles_writer` and `sort` depend on it, not the other way round. Hilbert
tests (`test_hilbert_*`) move from `pmtiles_writer_tests.rs` to it.

Reported by the coordinate-spaces, storage, archive-surface, layer-identity,
pipeline and polygon-engine hunts.

## STR-012 - Make ring role a type

Role is "index 0 of a `Shape` is the outer, the rest are holes" plus "outer
has positive shoelace area"; every in-place compaction must preserve the
slot-0 invariant by hand, and the sites already use different rules (the
defects are in `notes/bugs.md`). Proposed:
`struct Polygon<S> { outer: Ring<S>, holes: Vec<Ring<S>> }` with orientation
normalized at construction and one `Orientation::{Outer, Hole}`. If the
pooled `Vec<Vec<IntPoint>>` layout is load-bearing for allocation, keep it as
storage behind a type whose methods own the slot-0 rule. `multipolygon.rs`
has the same positional shape: `Polygon = (Vec<Point>, Vec<Vec<Point>>)` and
`RingGroups` a 3-tuple of same-typed Vecs (outer, inner, unclassified
swappable).

Reported by the polygon-engine and storage hunts.

## STR-013 - Typed tile-local geometry instead of MVT command streams as the internal representation

The engine's output is an MVT command stream: `PyramidSink` is
`FnMut(u8, u32, u32, &[u32], PyramidEmitKind)` and the engine calls
`mvt::encode_polygon_ranges` itself (`encode_tile_shape`, `emit_full_tile`).
The real intermediate representation of the pipeline is therefore "a
`Vec<u32>` command stream plus a per-layer interning table", chosen at
emission, carried through the sort as raw bytes, decoded into
`mvt::Feature`, decoded again by the line merger and MLT, and re-encoded.
Consequences: nothing stops a Point-typed feature carrying LineTo or a
polygon with a 2-vertex ring; `merge_connected_lines` decodes to
`Vec<Vec<(i32,i32)>>`, merges and re-encodes (plus a debug decode);
`append_geometry` re-deltas streams to concatenate; MLT re-parses every
feature into `geo_types` and can fail per feature, aborting the run;
`merge_same_attr_geometries` uses empty geometry as a tombstone justified by
an invariant held at N emit sites; ring roles are thrown away and
reconstructed with a different classifier; the line merger infers
"clip-created endpoint" from position.

Proposed (polygon-engine and encoding hunts converge): the engine emits a
`TilePolygon` with roles in tile-local space; the tile model is a typed
`TileGeom { kind, points: Vec<IPoint>, parts: Vec<Range> }` with polygon
parts tagged outer/hole and clip-created endpoints marked; MVT delta/zigzag
becomes a pure encoder concern; the sort payload is a format-neutral compact
encoding (fixed-width absolute i32 is the same 4 bytes as zigzag-delta u32;
varint delta is an independent optimisation). This subsumes the ring-role
and decoder consolidations for internal consumers.

Reported by the polygon-engine and encoding hunts.

## STR-014 - Move lines and points onto the integer pipeline

The coordinate-spaces hunt's "bolder version": quantize-early integer
geometry for every kind, so there is exactly one Merc-to-integer conversion,
one per-zoom shift-round and one tile-local conversion. The float S-H line
clipper, float `to_tile_coords_into`, float DP with Merc tolerances and
`merc_to_tile_px` go away, along with the +-1 shared-vertex disagreement and
the two spellings of "one pixel". A large rewrite of `emit_line_feature` and
the line merger's assumptions, and the only way to make "a shared node lands
on the same tile-local coordinate in every layer" true.

Reported by the coordinate-spaces hunt.

## STR-015 - Split `int_ocean.rs` and `pyramid.rs` by job

`int_ocean.rs` is misnamed (it serves all polygon emission) and holds
quantization, rescale, DP and VW simplifiers with scratch, ring hygiene, the
overlay facade (`normalize_into`, `intersect_rect_into`), point-in-contour,
tile arithmetic (`tile_origin`, `buffered_tile_rect`, dead
`tile_range_for_rect`), MVT encoding (`encode_tile_shape`, `emit_full_tile`)
and ocean-policy constants. `pyramid.rs` (about 1200 non-test lines) holds
descent and parallel split, ring-cap partition, a complete fast rect clipper
(`clip_shape_rect_fast`, `clip_ring_half_plane_multi`, `crossing_point`,
`round_div`) with its own fallback contract but tested only through the
pyramid, convexity testing, edge-flag building and window thinning.

Proposed `geometry/poly/`: `space.rs` (types, quantize, rescale, localize -
or into the coords module), `ring.rs` (hygiene, orientation, area),
`simplify.rs` (DP, VW, window thinning), `boolean.rs` (facade over
`overlay`), `clip_rect.rs` (with property tests against
`intersect_rect_into`), `pyramid.rs` (descent only); MVT polygon encoding
next to `mvt::encode_polygon_ranges`; `OCEAN_DP_TOL_PX`/`OCEAN_VW_AREA_2X` to
`ocean.rs` beside the policy version and `OSM_DP_TOL_PX` beside
`polygon_dp_tol`. The remaining float side of `geometry/` shrinks to
projection, line clipper, line DP and label placement - the real seam
`geometry::line` / `geometry::poly` / `geometry::overlay`.

Reported by the polygon-engine and coordinate-spaces hunts.

## STR-016 - Make pinned rings a type

Pins have three representations: Merc keys `FxHashSet<(i64, i64)>`
(`merc_point_key`), base-space `FxHashSet<(i32, i32)>`
(`PyramidParams::pins`), and per-ring `Vec<Vec<bool>>` flags (`flags_base`,
`edge_flags`, `flags_z`) that must stay index-parallel to the shape. The
parallel invariant is maintained by hand in `push_nonduplicate_pinned`,
`remove_closing_duplicate_pinned`, `orient_ring_pinned` (reverse both) and the
DP/VW truncate/resize repair, which silently pads missing flags with `false` -
hiding a desync rather than surfacing it. Proposed:
`PinnedRing { pts, pinned: BitVec }` or interleaved `(IntPoint, bool)`.

Reported by the polygon-engine hunt.

## STR-017 - Pool ownership in the polygon engine

`IntEmitScratch` owns the `BoolOverlay` and exposes its pool methods;
`PyramidScratch` owns an `IntEmitScratch` plus its own `frag_pool` and
reaches into `scratch.int.recycle_*`. "Return every `Shapes` you took" is
enforced by convention across about 20 call sites in pyramid.rs. Proposed: an
RAII `PooledShapes<'a>` that returns on drop (likely needing the pool behind
a `RefCell` or arena index), after the module split. The `PyramidParams` test
helper leaks a `Box` per call to get `'static` closures, a symptom of the
closure-typed policy.

Reported by the polygon-engine hunt.

## STR-018 - Drop the `geometry` glob re-exports

`geometry/mod.rs` re-exports everything with `pub use x::*` "so callers don't
change", and itself hosts constants, sub-pixel tests, float shoelace,
Mercator area, point-in-polygon and the integer self-intersection test. The
globs are why `geometry::EXTENT`, `geometry::MIN_POLY_AREA` and friends are
reached from everywhere without an owner. `mvt_decode.rs` does not belong in
geometry at all. Pre-1.0: make callers name the module.

Reported by the polygon-engine hunt.

## STR-019 - Assemble OSM multipolygons in integer space

`multipolygon.rs` receives member ways already projected to f64 Mercator
(`relations.rs` maps E7 through `project_e7`) and rebuilds identity by
quantizing to a 1e-9 grid, then decides orientation and ring order itself
before the integer engine decides them again - a float detour (E7 -> f64 ->
quantize-for-hash -> f64 -> quantize-early) the rest of the polygon pipeline
abandoned. Proposed: assemble in integer space keyed on the E7 pair or node
ids, producing integer rings the engine consumes directly, with role and
pairing from the engine's containment and NonZero normalization. Fixes the
inner-ring pairing defect by construction. The module then belongs under
`geometry/`; `WayRole::from_str` (an OSM tag interpretation, shadowing
`FromStr` naming without implementing the trait) belongs with relation member
parsing.

Reported by the storage hunt.

## STR-020 - Delete the dead float polygon machinery and other dead code

- `for_each_zoom_simplified_multi` and `SimplifyMultiScratch` have no
  production caller, but the scratch is threaded through relations.rs and
  emit.rs (`_simp_scratch`, including the unused parameter on
  `emit_multipolygon_feature`) and about 15 pipeline tests construct it; the
  doc keeps it "holding the path open for re-enabling".
- `clip_polygon_into` / `clip_polygon` (float S-H) have no production caller.
- `is_ccw`, `is_cw`, `reverse_ring` (test-only), `point_in_shape`,
  `recycle_shape`, `tile_range_for_rect`, `tile_count_for_rect`,
  `rect_intersection`, `intersect_shapes_with_rect`'s unused `_min_area`.
- `ring_is_simple`'s `j + 1 == ring.len() && i == 0` guard can never be true.
- `debug.rs` carries `#![allow(dead_code)]` over the module, hiding
  `malloc_trim`, `set_mmap_threshold`, `read_page_faults` with no callers.
- `mvt::Value` has seven variants; the pipeline produces four. `Float`,
  `UInt`, `SInt` sit under `allow(dead_code)` and MLT carries a seven-way
  `MltColumnType`, a type-bit mask and a `Mixed` column exercised only by
  hand-built tests.
- `wire_format::encode_feature_data` / `encode_feature_data_with_attrs` /
  `append_feature_data_with_attrs` - one of three entry points is
  `allow(dead_code)` and test-only.

Reported by the polygon-engine, coordinate-spaces, storage and encoding hunts.

---

# Profile and layers

## STR-021 - A declarative layer table as the profile's single owner

The profile is a hand translation of a declarative document
(`tests/fixtures/shortbread.yml`, checked in and read by nothing) into about
40 imperative matcher functions, and per-layer facts leak out of
`shortbread/` into emit.rs (min-area and subpixel exemptions, area-based zoom
overrides), phase12.rs (boundary match construction), assemble.rs (merge
policy), wire_format.rs (key and kind vocabularies), verify.rs (paint-order
layer set), pmtiles_writer.rs (vector_layers) and ocean.rs (ocean policy).

Proposed (Shortbread and layer-identity hunts):
```
struct LayerSpec {
    layer: Layer,
    tile_order, zooms,
    accepts: &'static [OsmGeomType],
    geom: GeomExpect,                      // allowed output kinds
    classify: fn(&Tags) -> Option<Hit>,    // kind + zoom, no attrs
    attrs: fn(&Tags, &Hit, &mut Attrs),    // only on real emission
    attr_schema,                           // typed keys, value types, per-key min zoom
    paint: Option<fn(&Hit) -> PaintRank>,
    keep_small / polygon & line policy,
    merge policy, fanout_cap_applies,
}
static PROFILE: [LayerSpec; N];
```
Kind enums hold per-kind zoom and rank; simple layers are
`&[(key, value, kind, zoom)]` rows walked by one helper; `Layer::min_zoom` is
derived; `match_element` loops over the table filtered by `accepts`. The
Shortbread hunt estimates a rewrite of about 1500 lines with no output change,
gated byte-for-byte by the denmark corpus check - "a low-risk large change".
Going further, `build.rs` could generate the tables from `shortbread.yml`, at
the cost of an overlay for non-spec decisions (paint ranks, the cliff line,
numeric parsing); hand-written tables plus a test asserting every upstream
`include_when` value and `min_zoom` override is represented gets most of the
benefit. The layer table also lets `vector_layers` carry `fields` and lets
verify check observed attributes against the schema.

Reported by the Shortbread and layer-identity hunts.

## STR-022 - `LayerMatch` is a wide, mutable, post-processed record

It is mutated after construction by `resolve_boundary_match` (overwrite) and
`enrich_polygon_matches` (push attr, lower zoom), so the profile's answer is
provisional and the final answer is assembled in the pipeline.
`LayerMatch.max_zoom` is always 14. `(min_zoom, max_zoom, paint_rank)` are
three adjacent `u8`s constructed positionally in
`phase12::resolve_boundary_match` and many tests. A
`const _: () = assert!(size_of::<LayerMatch>() == 408)` pins a struct whose
inline `SmallVec<[Attr; 8]>` of string-keyed tuples is most of the size.
Proposed: a `Hit` the pipeline completes by calling a profile function with
geometry facts (area, relation meta), so the profile makes every decision and
the pipeline supplies measurements; drop `max_zoom`; `Zoom` and `PaintRank`
newtypes; attr construction at emission time shrinks what travels through the
dispatch SmallVec.

Reported by the Shortbread and layer-identity hunts.

## STR-023 - Move `pois.rs` into `shortbread/` and dissolve `paint_order.rs`

`pois.rs` sits at the crate root but is a profile module (`Layer::Pois`);
mod.rs has a section header explaining it. `paint_order.rs` is a
cross-cutting table keyed by strings; with kind enums it dissolves into
`LandKind::rank` / `StreetKind::rank` beside their kinds, and verify imports
the enums. `boundaries.rs` also owns `place_labels`, which has nothing to do
with boundaries.

Reported by the Shortbread and layer-identity hunts.

---

# Encoding and the sort record

## STR-024 - Separate the tile model from MVT, and the sort-record codec from the tile model

`LayerBuilder`, `Feature`, `Value`, interning tables, merge passes and scratch
types live in `mvt/`, and MLT consumes them through accessors that exist only
for it. Proposed: a format-independent `tile_model/` (layers, features,
attributes, interning, post-processing) with `mvt` and `mlt` as sibling
sinks. `wire_format::add_feature_to_layer` decodes a sort record directly into
a `LayerBuilder`, interning as it goes and handling per-worker Vec pools, so
the codec depends on the tile model and the assembly pooling strategy;
proposed: `wire_format` yields a borrowed
`FeatureRecord<'a> { id, geom: GeomView<'a>, attrs: AttrIter<'a> }` with a
`ValueTag` enum and `read -> Result`, encoder and decoder side by side with a
round-trip property test (today the layout is mirrored by hand: header size
13 as `13 +` twice and `< 13`, value-type tags 0..4 as literals on both
sides). Assembly owns "push record into tile model".

`LayerBuilder` keeps three maps for one table (`key_map`, `value_map`,
`string_value_map`); strings never reach `value_map` only by routing
discipline. With typed keys and kinds, keys become a fixed array index and the
key `FxHashMap` goes. Encoding constants: `extent = 4096` and `version = 2`
are literals in `LayerBuilder::encode`; `MAX_FEATURE_RINGS`, a MapLibre
consumer limit binding at emission in pyramid.rs, lives in `mvt/mod.rs` and
belongs with geometry emission policy or a `consumer_limits` module.

Reported by the encoding hunt.

## STR-025 - Move the line merger to geometry

`merge.rs` holds two things: multi-geometry concatenation
(`merge_same_attr_geometries`, a tile-model operation) and a line-graph merger
with its witness-based invariant checker (a geometry algorithm with its own
clip-boundary semantics). The merger belongs with line geometry beside the
clipper whose contract it depends on, operating on typed segments and called
by the post-processing owner; the checker moves with it. Micro-optimizations
noted on the way: `dedup_parallel_segments` clones every segment twice into a
fresh `FxHashSet` per feature per tile on the z0-13 line path;
`merge_same_attr_geometries` sorts and re-compares full tag vectors (a packed
`(KeyIdx, ValIdx)` u32 per tag makes it an integer slice compare);
`build_tile_model` finds columns with `position` per tag.

Reported by the encoding hunt.

## STR-026 - Feature id namespacing

The sort record carries a bare `u64` that becomes the MVT feature id. Nodes,
ways, relations and synthetic ocean ids share the space with nothing saying
which. The encoding hunt did not trace emission to check whether ids are
pre-namespaced: if they are, the rule is an implicit convention at each emit
site; if not, ids collide across element types. Proposed: a `FeatureId`
(enum or tagged newtype) minted at emission.

Reported by the encoding hunt.

---

# Storage, sort and resources

## STR-027 - Split `sort.rs` and delete the legacy chunk modes

`sort.rs` (about 2070 non-test lines) is key and tiling algebra (`SortKey`
packing, `zoom_from_tile_id`, `PARTITION_SPLIT_Z`, `PARTITION_BASES`,
`partition_from_key`, `partition_tile_range` - public because an artifact run
may cross a partition boundary), a chunk file format (three magics, single and
multi layouts, compressed frames, a filename grammar including legacy names,
directory scan), a write side (`SortWriter`, `SpillCoalescer`, the chunk-id
atomic, and six functions writing the same bytes across
`SortRecord`/`PayloadRecord` x single/multi/compressed-multi/presorted - the
doc on `write_sorted_payload_chunk` says "the bytes on disk are identical"), a
read side (section parsing, `ChunkReader`, the merge heap, `SortPartition`,
the hot-partition piece planner), and stats.

Proposed: key algebra to the coords module; `sort/format.rs` with one
`RecordSource` trait implemented by both stagings and one writer generic over
it and `ChunkCompression` (removing four of six writers); `sort/write.rs`;
`sort/merge.rs`; and piece planning (`plan_pieces`,
`scan_source_buckets`, `choose_bucket_cuts`, `MAX_PIECES`, `MAX_BUCKETS`, the
`ELIVAGAR_ASSEMBLE_SPLIT_TARGET` target - all assemble scheduling policy)
moved next to the claim window it feeds, behind a small format
"source cursor/seek" API.

Legacy: `ChunkFileKind::Legacy`, the `_pNNNN` "first-cut P3" parser,
`legacy_chunk_path`, `SortReaderMode::Legacy`, `new_whole_paths`,
`SortReader::next`'s global iteration and the
`first_cut_partition_suffix_falls_back_to_legacy_merge` test exist to read
chunks from writers that no longer exist; checkpoint v6 already refuses every
older checkpoint. The only other route into Legacy is a filename with a
different split zoom, which should be an error. With legacy gone,
`SortReader` reduces to a manifest of partitions and `take_partitions()`
loses its None arm.

Reported by the storage hunt.

## STR-028 - One external sorter, or none for way offsets

Three hand-rolled external sorts: `sort.rs` records; `way_index.rs`
(`sort_offsets_file`, `merge_offset_chunks`, `OffsetChunkReader`,
`OffsetHeapEntry` - a complete second k-way merge with its own fixed 256 MB
budget outside the budget system); and the coalescer's in-memory sort. Way
ids arrive sorted in a Type_then_ID PBF (the property the sorted node store
relies on), also in locations mode, so the way index could write offsets in
arrival order and check monotonicity, making the external sort a fallback or
replacing it with the same sorted-only rule as nodes. Otherwise reuse one
generic sorter.

Reported by the storage hunt.

## STR-029 - Typestate and naming for the node and way indexes

`NodeIndex -> NodeIndexReader` and `SortedNodeStore -> SortedNodeStoreReader`
use consuming `into_reader`; `WayIndex` holds both phases in one struct with
`Option` writers and mmaps. Proposed: `WayIndexWriter -> finish() ->
WayIndexReader` (Sync), so the drain thread hands back a reader by type.
`NodeIndex` / `SortedNodeStore` / `NodeStore` / `NodeStoreReader` /
`NodeIndexReader` are five names for two backends; with the flat path dead in
practice, one `SortedNodeStore` plus a hard error for unsorted input, or a
`NodeLookup` trait with a backend-agnostic `stats()`. The bitpacking, bitmask
and FOR codec (about 450 lines) is a separable `node_index/codec.rs`.
Coordinates: node store `put(node_id, lat_e7, lon_e7)` / `get ->
Option<(i32, i32)>`, `WayIndex::put(way_id, &[(i32, i32)])`, `decode_way`,
`project_e7(lat, lon)` and phase12's extent locals agree on (lat, lon) order
by convention only; proposed `LatLonE7 { lat, lon }` (repr C, 8 bytes, owner
of the delta codec), `NodeId(i64)`, `WayId(i64)` (`WayIndex::get` accepts a
node id today).

Reported by the storage hunt.

## STR-030 - `debug.rs` is the telemetry channel and should be named and typed as one

`debug.rs` holds the sidecar FIFO protocol, stall and busy accounting, the
ring-cap tallies (a geometry statistic parked there), and glibc allocator
control. It is the always-on channel brokkr's tooling depends on. Counter
names are free strings, many `format!`-built (`fanout_capped_features_{name}_z{z}`,
`malloc_held_{boundary}`, `sort_layer_<name>_z<z>_...`), emitted from about
seven modules; brokkr parses them (`--stalls` keys on `_wait_ns`) and the
suffix protocol is enforced by convention. `emit_marker(&str)` lets anyone add
a fifth marker that floods the phase views, though AGENTS.md reserves markers
for phase boundaries. Process-global statics are why "nothing resets between
runs" is an assumption.

Proposed `telemetry/`: `fifo.rs` (protocol only), `counters.rs` (a declared
registry extending `counter_group!`, each counter declared once with a kind -
`Wait`, `Busy`, `Gauge`, `Total` - deciding its suffix; absorbing
`SORT_MERGE_*`, `OCEAN_LAYER_STATS` and the ring-cap pair), `alloc.rs`;
markers as `enum Phase { Phase12, Ocean, Sort, Assemble }` with start/end;
a run-owned `Telemetry` handle (or reset at `run` entry).

Reported by the storage, polygon-engine and pipeline hunts.

---

# Archive surface

## STR-031 - One `archive` module: typed header, one reader, directory, metadata

The scope has no model of a PMTiles archive: the header is a `[u8; 127]`
indexed by magic offsets in five files, and two readers (`PmtilesReader`,
seek+read allocating per call; `ArchiveView`, mmap borrowing) duplicate open,
header parsing, directory reading, `read_all_runs` and metadata with divergent
validation. Inside the header: tile type and compression as `u8` compared to
literals `1`/`2` (named only in inspect), `bounds: (f64, f64, f64, f64)` and
`center: (f64, f64, u8)` ordered only by a doc example (swapping lat and lon
type-checks), `clustered` as `h[96] = 1`.

Proposed:
```
archive/
  header.rs     Header, TileType, Compression, LonLatBbox, Center; encode + decode -> HeaderError
  tile_id.rs    (or the coords module)
  directory.rs  DirEntry::{Run, Leaf}, encode/decode, run invariants, find_entry
  reader.rs     one mmap reader, typed ArchiveError, tile decompression by header
  writer.rs     container only: blobs, dedup index, directories, layout
  metadata.rs   the TileJSON-ish document (layers, ocean_artifact, elivagar), serde
```
Delete `PmtilesReader` (archives larger than address space are not a 64-bit
concern); `read_u64_le`, `read_i32_le`, `gzip_decompress` stop being `pub`
(assemble reaches in for `gzip_decompress` and `decode_mvt_layers` today).
`TileEntry` is `(TileId, BlobRef)`; the writer's `DirEntry` and dedup tuple use
`BlobRef`.

Reported by the archive-surface and coordinate-spaces hunts.

## STR-032 - Build archive metadata as a typed document

`PmtilesWriter::add_metadata_member(impl Into<String>)` takes raw
`"key":value` fragments that `build_metadata` concatenates with `format!`; the
justification ("avoid a runtime dependency for ~20 lines") is stale since
`serde_json` is already a runtime dependency. Duplicate top-level keys are
undetectable (serde keeps the last), a hazard the doc acknowledges for durable
ocean artifacts and relies on discipline to avoid. `ocean_only_metadata` is a
mode flag changing `vector_layers`, which is a property of the layer set.
`set_metadata_verbatim(String)` silently overrides every other setter through
an `Option`. Proposed: the writer takes an opaque metadata document built by a
`metadata.rs` owning the TileJSON fields, `ocean_artifact` and `elivagar` as
typed fields of one struct serialized once.

Reported by the archive-surface hunt.

## STR-033 - Writer storage modes as one path

`TileBlob::{Memory, File}` and `DirStore::{Memory, Streaming}` are chosen
together (`new` vs `new_streaming`) and matched separately in `add_tile`,
`add_run`, `flush_run`, `finalize_directories` and `write_to`; mixed
combinations are representable but never constructed, and the in-memory mode
already writes differently (one-pass vs patch-and-rename).
`finalize_directories` has three near-identical "try root, else double leaf
size" loops. Proposed: a `Sink: Write + Seek` for the blob, a `DirSpill`
trait, one `layout_directories` loop; the in-memory mode becomes a test/small
sink, not a parallel path.

Reported by the archive-surface hunt.

## STR-034 - `verify.rs` is four tools in one file

Container checks, MVT structural grammar, Shortbread paint-order rules
(hard-coding the layer set and the `z < 11` switch), and ocean geometry stats
and self-intersection. Each walks the decoded tile separately - the payload
is protobuf-parsed four times per tile (`decode_mvt_layers`,
`validate_mvt_geometry`, `validate_ocean_rings`,
`collect_ocean_geometry_stats`). Proposed: a `Check` trait over one decoded
tile, with Shortbread rules owned by `shortbread/`; svg and diag become
renderers over the same decoded tile. Decode-side types: `DetailFeature`
carries both `geom_type: u8` and per-ring `CanonRingRole`
(Point/Path/Outer/Hole), encoding one fact twice (a type-1 feature with a
Path ring is constructible); svg and verify use `geom_type: u64`; the encode
side's `mvt::GeomType` (with an `Unknown(u64)` arm for tolerant decoding)
would serve all. `DetailAttr::Float(u32)` / `Double(u64)` store bits, which is
right for hashing but deserves a comment or `F32Bits` newtype.

Reported by the archive-surface hunt.

---

# Tests and documentation

## STR-035 - Test layouts mirror history and module privacy, not the design

- `pipeline_tests.rs` (about 3741 lines) is one `#[path]` file reaching
  private items through a block of `cfg(test)` re-exports in `mod.rs`; it
  mixes injected-PBF fixture builders (hand-written protobuf splicing,
  `splice_way_members`, `rebuild_blobheader_datasize`), emitter decode tests,
  assemble metadata tests, checkpoint tests, artifact-retention tests and
  boundary-label tests, and builds `TilegenConfig` struct literals seven or
  eight times repeating every budget field as 0 (which is why the zero
  sentinel survives). Proposed: per-module tests after the splits, the PBF
  fixture builders as a shared `test_support` synthetic-PBF writer, and a
  `TilegenConfig::for_test()` / resolved-resources builder.
- `geometry_tests.rs` (loaded via `#[path]`) covers the float side including
  `multi_simplify_*` for the parked cascade; the integer engine's tests sit
  inline in int_ocean.rs under `tests` and `landing1_tests` (a landing name);
  `bounds_boolean_skipped_when_bbox_contained` asserts an ocean.rs property
  from inside int_ocean; `landing_b_*` names encode project history. pyramid
  tests use a test-only `rescale_shape` and a `collect_direct` reference that
  relies on dead `tile_range_for_rect`, so the cover-function drift between
  emit.rs and pyramid.rs is not covered. Proposed: tests per module named by
  property.
- `shortbread_tests.rs` (about 1400 lines, `#[path]` from
  `shortbread/mod.rs`) mixes ad hoc cases, the spec harness and a dated
  "2026-03-03 audit" block (B1-B7). The harness is lenient in ways that hide
  drift: polygon inputs dispatch only as `ClosedWay`, so `match_multipolygon`
  is never exercised; any expectation with `at_zoom` skips the extra-attrs
  check and per-attr min zoom is never checked (the zoom-gated attribute
  policy - link/tunnel/bridge at 11, oneway/bicycle/horse at 14 - has no spec
  coverage); `attr_values_match` accepts Int vs Float within 1.0 and Int vs
  Str by formatting; `find` takes the first match per layer, hiding a wrong
  second match; boundary_labels min zoom is skipped. Proposed: per-layer tests
  beside layer specs plus a conformance harness running the spec for both
  ClosedWay and MultiPolygon, checking attr zoom gating, comparing values
  strictly, and loading `shortbread.yml` (currently orphaned) to assert every
  upstream `include_when` value and `min_zoom` override is represented. The
  Shortbread hunt says the last point would have caught both the
  public_transport and water-label findings.
- `mvt/tests.rs` (about 1.6k lines) mixes encoder tests, a private protobuf
  fixture parser, `append_geometry`, same-attr merge and line-merger/checker
  tests, reaching internals through `test_*` shims (`test_append_geometry`,
  `test_merge`, `TestMergeRun`, `TEST_CLIP_BOUNDARY`); `LayerBuilder` grows
  `test_feature`/`test_key` accessors duplicating the `pub(crate)` ones MLT
  uses.
- `ocean.rs` tests include seven `point_in_polygon` tests for a `geometry`
  function ocean no longer uses; band-ownership tests sit beside shapefile
  parsing; `ocean_band_tile` and `selected_pass_grid` tests hard-code
  `max_zoom: 14` and z7/z8, exercising the copy rather than agreement with the
  pyramid.
- `sort.rs` tests (about 1100 lines) mix format round-trips and resume
  semantics; resume tests belong with the chunk-store manifest.
- Writer tests hand-decode and reader tests hand-encode; inspect tests patch
  header bytes by raw offset; verify tests assert on error-string prefixes;
  `pmtiles_writer_tests.rs` houses Hilbert tests and `test_varint_*` testing
  protohoggr, not this crate.
- No test iterates `Layer::ALL` against a descriptor invariant.

Reported by every hunt.

## STR-036 - Missing tests the hunters named as most valuable

- A property suite running random shapes through quantize, pyramid and decode,
  checking outer positive, holes inside outers, at most 500 rings, and no
  hole-oriented ring in slot 0 (polygon-engine hunt).
- Full-range round-trip property tests for the way delta varint, node FOR
  bitpacking and chunk record layout (storage hunt).
- A test building the ocean producer filter and the consumer copy/splice from
  one plan and checking every tile is owned by exactly one side - "the test
  this subsystem most lacks" (ocean hunt).
- A semantic comparison of MVT and MLT output for the same tile (encoding
  hunt).
- In-crate tests for `tile_detail`: Strict/Tolerant, ClosePath cursor
  handling, ring grouping, value decoding (archive-surface hunt).
- `decode(encode(x)) == x` for the PMTiles directory (archive-surface hunt).
- An assertion that `EXTENT * BUFFER_FRACTION == TILE_BUFFER_I32` until the
  constants have one owner (coordinate-spaces hunt).
- A golden ocean digest pinned with `OCEAN_POLICY_VERSION` (ocean and encoding
  hunts).

## STR-037 - No reference document names the coordinate spaces

Nothing in `reference/` names the thirteen spaces the coordinate-spaces hunt
catalogued (e7 degrees, float degrees, EPSG:3857 metres, normalized Mercator
wrapped and unwrapped, base pixels, zoom pixels, tile-local units, tile
address, Hilbert id, sort key, partition id, vertex-identity hash spaces,
tolerance and area units) or the conversions between them; each is described
only by comments at its use sites. Proposed: a short
`reference/coordinates.md` holding that table, the buffer-population policy
(only features reaching the tile proper populate a neighbour's buffer) and the
rounding rule, giving future comments something durable to cite.

Reported by the coordinate-spaces hunt.
