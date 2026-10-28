# Consolidations

Domain questions the 2026-10-28 design hunt found answered in more than one
place. The unit of an entry is the question: every site answering it belongs
to the one entry, with where its single owner should live. Where a hunter
found the sites already disagreeing and that disagreement is a defect, the
account lives in `notes/bugs.md` instead.

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

# Run contract, provenance and resume

## CON-001 - Which settings form the comparability contract?

Sites: `provenance::config_json` and `provenance::Input::to_json` (each a
hand-picked `json!` subset of `TilegenConfig`), brokkr's own rule that
`input.name` warns but does not gate (lives only in brokkr and
`reference/corpus.md`), and the `reference/corpus.md` prose. A new
contract-relevant field added to `TilegenConfig` defaults to "not in the
contract" silently - the run-contract hunt calls that the dangerous default.

Already disagreeing: `reference/corpus.md` lists a "seam" member that does not
exist; the contract omits computed-ocean source identity (in
`notes/bugs.md`); MLT compression is misrecorded (in `notes/bugs.md`).

Unrecorded choice: `compression_level` is in the contract while the corpus
digest is over decoded content, so a level change refuses a comparison whose
content is guaranteed equal. Defensible as strictness; nobody has written it
down.

Owner proposed: a typed `Contract { input, config: ContractConfig }` in
`provenance.rs` with serde both ways and a `gated_eq` / `diff` /
`gated()` projection that brokkr calls instead of applying its own
exceptions, so the "name never gates" rule documented on `provenance::Input`
is enforced by the type stating it. With `TilegenConfig` split so the contract
is its own struct (see `notes/structural.md`), `config_json` becomes
`serde_json::to_value(&spec.contract)`.

Reported by the run-contract and archive-surface hunts.

## CON-002 - Is a provenance block a complete contract?

- `provenance::extract_contract` (brokkr's entry point) requires `schema == 1`,
  `input`, `config` AND `build`, reporting `Incomplete("build")` if build is
  missing.
- `inspect::print_provenance` hand-walks the same JSON, requires `schema`,
  `input`, `config`, and treats `build` as optional ("absence is not an
  error"). Its "not JSON" / "absent" / "schema N not interpreted" /
  "INCOMPLETE" ladder is a second copy of `ContractState`'s variants with its
  own wording, and the two also differ on what counts as invalid.

Already disagree: an archive with input+config but no build prints a full
contract in `elivagar inspect` and is refused as `Incomplete("build")` by
`brokkr pmtiles-corpus check`. AGENTS.md says build is diagnostic and must
never be equality-gated; extract_contract does not gate its value but does
gate its presence. Also `ContractState::Unavailable(String)` is never
constructed in this crate.

Owner proposed: `provenance` with a typed decode; inspect calls
`extract_contract` and renders the `ContractState`.

Reported by the run-contract and archive-surface hunts.

## CON-003 - What must a resume match?

Five carriers, each covering a different subset:

- the checkpoint's `producer_config` (zooms, fanout caps, simplify factor),
  hand-listed and guarded by two pairwise tests
  (`producer_config_excludes_assemble_side_settings`,
  `producer_config_covers_chunk_affecting_settings`) that catch only the
  fields they name;
- the checkpoint's `OceanMode` and input hash, compared separately in `run()`
  - ocean mode twice on `--skip-to ocean` (once in the up-front resume block,
  again in the arm, with different prose);
- the chunk filename (`_z7` encodes `PARTITION_SPLIT_Z`; a different split
  zoom silently reclassifies files as `Legacy` and falls back to the global
  merge);
- the multi-chunk magic (compression, multi files only);
- the checkpoint chunk count versus a directory scan.

`effective` is inherited, never compared. Gaps already present
(compression, phase completion) are in `notes/bugs.md`.

Open decision: `--skip-to ocean` refuses an ocean-mode change, yet re-running
the ocean is the point of that resume; ocean producer identity matters for
`sort|assemble` resumes, not `ocean` ones (pipeline hunt). And the checkpoint's
ocean identity for artifact-active runs is the artifact key, which includes
`compression_level`; the `OceanSourceKey` doc says compression level must not
gate a resume because chunks hold geometry not encoded tiles. The refusal is
moot only because `resolved_ocean_mode` already refuses a level mismatch - the
band chunks are a function of the source key plus band geometry, not of the
artifact's encoding (run-contract hunt).

Related: `load_checkpoint` returns a 4-tuple and is called up to four times
per run (resume guard, `--skip-to ocean` arm, data bounds, late artifact
open), each re-parsing the file; on a full run it reads back what phase12
returned a moment earlier (`bounds_out` is discarded and re-read).

Owner proposed: a typed `Checkpoint` (own module) holding the producer
contract, a chunk-store manifest (format version, compression, split zoom,
chunk list with partition tables inlined), the ocean identity, input identity,
effective enums and bounds; loaded once; one
`Checkpoint::admits(&RunPlan) -> Result<(), ResumeRefusal>`. `ProducerConfig`
embedded in `ContractConfig` so it is a subset by construction.

Reported by the pipeline, run-contract, ocean and storage hunts.

## CON-004 - What is the tile contract (format, compression, per-zoom level, policy version)?

- `assemble.rs`: MLT forces compression None; the config-to-writer mapping
  around `set_tile_contract` hard-codes "MLT payloads are stored
  uncompressed".
- `provenance::config_json` and `run()` counters record `config.tile_compression`
  raw.
- `resolved_ocean_mode` checks format and compression independently.
- `OceanTiles::open`, `verify`, `ArchiveView::open` compare raw header ints
  `1`/`2` (a re-check against the file - fine, but magic numbers).
- `compression_level_for_zoom` (z0-8 clamp to 9-10, z13-14 cap at 3) is the
  policy; the label `"compression_policy": "zoom-v1"` in `provenance` is a
  string literal nothing ties to it. Change the function without the string
  and the contract reads as unchanged.
- The ocean artifact key carries `compression_level` but no compression-policy
  version: an artifact built under an older per-zoom policy still key-matches,
  and assemble splices fresh-policy tiles next to old-policy run copies. The
  ocean hunt notes `OCEAN_POLICY_VERSION`'s documented scope lists the MVT
  encoder but not the gzip policy.
- Vocabularies: `TilePayloadFormat`+`TileCompression` (lib.rs),
  `TileDataFormat`+`TileDataCompression` (writer), header bytes, metadata
  strings; name tables in `TileDataFormat::metadata_format`/
  `metadata_payload`, `TileDataCompression::metadata_value`,
  `provenance::tile_format_name`/`tile_compression_name`,
  `inspect::compression_name`/`tile_type_name`/
  `infer_payload_format_from_header`.
- `PmtilesWriter::add_tile` documents "data must already be gzip-compressed"
  while accepting a brotli or none contract and never checking the payload.

Not a duplicate (run-contract hunt): the compression-level check in both
`resolved_ocean_mode` (`declared_key`) and `OceanTiles::open` (full key
equality) is one answer confirmed twice for a better message.

Owner proposed: `enum TileContract { Mvt { compression, level }, Mlt }` with
`header_tile_type()`, `header_compression()`, `metadata_name()`,
`provenance_json()`; the per-zoom policy function and a
`const COMPRESSION_POLICY` version next to each other, referenced by the
provenance block and folded into the artifact key; a typed
`CompressedTile { contract, bytes }` from the encoder.

Reported by the pipeline, run-contract, ocean and archive-surface hunts.

## CON-005 - How do ocean mode and effective-path facts get their names?

- `OceanMode::{None, Computed, Band}` is rendered `"none"/{"computed"}/{"band"}`
  in the checkpoint and `"none"/"shapefile"/"artifact"` in provenance via a
  `match` in `run()` passed as `&'static str` through `RunProvenance`;
  `write_provenance` then compares `mode == "none"`. `low_zoom_source` is a
  third `&'static str` (`"none"/"simplified"/"full"`) whose `none` must agree
  with mode `none`, enforced by nothing.
- phase12 mints `"inline"/"node_store"`, `"injected_v1"/"relation_scan"`,
  `"injected_v1"/"block_local"`; `load_checkpoint` keeps its own allow-list
  (`path_name(..., ["inline", "node_store"])`) to intern back. `PinSource` and
  `NodeStoreMode` already exist as enums in phase12.
- `RunProvenance::resumed_from: Option<&'static str>` is `SkipTo` mapped to
  strings in `run()`.

Owner proposed: enums with serde renames shared by checkpoint and provenance
(`CoordinateSource`, `MembershipSource`, `PinScope`, `Option<SkipTo>`), and
`enum OceanContract { None, Shapefile { low_zoom }, Artifact { low_zoom, key } }`
so invalid pairings are unconstructible. `OceanMode` is also `pub` from
`lib.rs` though nothing outside the crate uses it.

Reported by the pipeline, run-contract, ocean and archive-surface hunts.

## CON-006 - How are content hashes represented and validated?

`u128` in the ocean keys, lowercase-hex `String` in `provenance::Input`,
`String` in `CheckpointProvenance.input_xxh3_128`, `(String, u64)` as
`input_identity` threaded through `run()`, `RunProvenance` and the checkpoint.
Two `hash_file`s (provenance: 8 MiB buffer, returns `(String, u64)`; ocean:
1 MiB, returns `u128`). Two hex validators: `OceanArtifactKey::from_json`
requires exactly 32 lowercase hex and present-but-null optionals;
`OceanSourceKey::from_json` accepts any radix-16 length, uppercase, and
missing optionals. The two keys share seven of eight fields with two
hand-written codecs (`json!` vs a `format!` raw-string template).

Owner proposed: one `Xxh128` newtype with one `Display`/`FromStr`;
`InputIdentity { xxh3_128, bytes }`; `OceanArtifactKey { source:
OceanSourceKey, compression_level }` by composition with one serde codec. The
artifact key's `min_zoom`/`max_zoom` are always 0/14; derive from one
constant or drop at the next policy bump. A `ShapefilePair { shp, shx }`
removes the five independent `with_extension("shx")` derivations and the
`OceanArtifactKey::from_inputs` parameters that let a caller pass a mismatched
pair.

Reported by the run-contract, ocean and archive-surface hunts.

---

# Ocean

## CON-007 - May the artifact serve this run?

1. `main.rs` - artifact without shapefile is an error.
2. `resolved_ocean_mode` - format, compression and zoom contract against
   config; existence; declared compression level vs config; full shapefile
   present (again, with different prose than `main.rs`, so library callers
   get only this one); recompute key. It opens the archive once just to read
   the declared key.
3. `OceanTiles::open` - format/compression/zoom against the header (magic
   ints), world bounds read by raw header offsets, single ocean layer
   (by the string `"ocean"`), full key equality, run range. It is reached at
   two sites in `run()` (opened verbatim twice depending on the skip).
4. `ocean_build` - what it writes.

The artifact zoom contract `0/14` is hard-coded in four places
(`resolved_ocean_mode`, `ocean_build`, `OceanTiles::open` including a
`z14_end = ((1 << 30) - 1) / 3 + 1` Hilbert formula that belongs to the tile-id
module, and `artifact_run_copy_items`' `z == 14`).

Agree today, held by hand. `OceanMode::Band { key }` does not carry the
artifact path, so every use re-pairs it with `config.ocean_tiles` via
`if let (OceanMode::Band { key }, Some(path)) = ...` (three times in `run`);
Band with a None path silently proceeds without the artifact.

Owner proposed: one `ARTIFACT_CONTRACT` constant (format, compression, zoom
range) in an artifact module, and one
`OceanArtifact::admit(path, &plan, &run_contract) -> Result<OceanArtifact,
ArtifactRefusal>` called once before phase12, with Band owning the admitted
view.

Reported by the pipeline, run-contract and ocean hunts.

## CON-008 - Is the ocean policy version still honest?

`OCEAN_POLICY_VERSION` is a bare `u32` whose doc says any change to
`ocean_params`, the pyramid, the simplifiers, the overlay engine or the MVT
encoder requires a bump. Three further "remember to bump" comments live in
`int_ocean.rs`, `pyramid.rs`, `overlay/mod.rs`, and the `mvt/mod.rs` header
says the same of encoding changes. Nothing mechanical ties the constant to the
code it versions; the v2 history is a three-day silent-staleness incident.

Owners proposed (the hunts suggest overlapping options):
- a golden test building a small fixed synthetic world through ocean emission
  and assemble, pinning the output digest together with the version (ocean
  hunt; encoding hunt suggests a golden-bytes ocean tile naming the constant);
- an `MVT_ENCODER_VERSION` folded into the artifact key (encoding hunt);
- the ocean policy as a single hashable value (`OceanPolicy` implementing a
  `PolygonPolicy` trait) whose hash is folded into the key or checked by a
  test that fails when the hash changes without a bump (polygon-engine and
  ocean hunts).

Reported by the polygon-engine, encoding and ocean hunts.

---

# Geometry and coordinates

## CON-009 - What is the tile extent and the buffer?

Two answers (4096 units, 128 units of buffer), ten or more sources:
`geometry/mod.rs` (`EXTENT: f64`, `BUFFER_FRACTION = 8/256`, `PX`),
`int_ocean.rs` (`TILE_EXTENT_I32`, `TILE_BUFFER_I32`, plus `+ 12` as log2 in
three quantizers), `ocean.rs` (literal `4096 << shift`, `128 << shift`,
`1 << (max_zoom + 12)` in `ocean_band_tile`, `pass_owns_root_range`,
`OceanPassGrid::for_bounds`, `data_bounds_rect`), `mvt/merge.rs`
(`BUFFER_UNITS` from the float pair with a const-assert it is whole, not that
it equals 128), `mvt/mod.rs` and `mlt.rs` (literal 4096 in the layer extent),
`verify::MVT_EXTENT`, `svg::EXTENT`, `tile_detail` default. The int and float
buffers agree only because 4096 * 8/256 = 128; a geometry test pins
`BUFFER_FRACTION`'s value (the pairwise-agreement pattern).

Consequence chain: lines clip with the float buffer, polygons with the
integer one, and the line merger's `CLIP_BOUNDARY_LO/HI` assumes the line
clipper's rounded output lands exactly on +-128 / 4224 - a cross-module
contract asserted by one test
(`clip_boundary_matches_the_line_clip_contract`) and the const assert. The
code comment calls position "a conservative proxy" for "created by clipping".
Ocean's literal 128 in `ocean_band_tile` must equal the pyramid's buffered
clip or band/artifact ownership is wrong - artifact seams no gate is
calibrated for. `svg` and `verify` assume 4096 without reading the layer's
extent field (right for elivagar output, wrong for foreign MVT).

Owner proposed: one integer tile-grid module (`EXTENT_LOG2 = 12`,
`EXTENT`, `BUFFER_UNITS = 128`, derived `BUFFER_FRACTION`) everything derives
from, including the MVT/MLT extent fields and decoder defaults. The encoding
hunt adds that the clipper should mark clip-created endpoints in typed
geometry rather than the merger inferring them from position. The brotli
`CompressorWriter::new(&mut buf, 4096, ...)` buffer size is a grep hazard next
to all the extents; name it.

Reported by the polygon-engine, encoding, ocean and coordinate-spaces hunts.

## CON-010 - Which tiles does a base-space rect touch, and what is a cell's buffered rect?

- `pyramid.rs`: `tile_range_for_base_rect`, `base_tile_index`,
  `cell_tile_size_base`, `buffered_cell_rect_base`, `buffered_range_rect_base`.
- `emit.rs`: `tile_count_for_base_rect` + its own `base_tile_index`, a
  verbatim copy used for the fanout cap (the undercount it causes is in
  `notes/bugs.md`).
- `ocean.rs`: `pass_owns_root_range` (inline index closure, "deliberately
  follows pyramid::root_fragments"), `ocean_band_tile` (inline buffered rect,
  clamped to the world).
- `int_ocean.rs`: `tile_range_for_rect`, `tile_index`, `tile_count_for_rect`,
  `rect_intersection`, all `#[allow(dead_code)]`, zoom-space only, used only by
  pyramid's test reference `collect_direct`.
- Float `tiles::clamp_tile` / `for_each_tile_in_bbox` / `is_single_tile` for
  lines and points.

Owner proposed: a `Grid` value (or `PyramidCell::cover(rect, span)`) with
`tiles_touching(rect, z)`, `cell_rect`, `buffered_cell_rect`, used by emit.rs
and by ocean's band test (which should call the pyramid's own root
selection). Delete the dead int_ocean copies.

Reported by the polygon-engine and coordinate-spaces hunts.

## CON-011 - Which vertices are in a cell's edge window?

`build_edge_flags` (base space, buffer shifted by `maxz - z`, also flags
segments crossing an edge line) and `thin_window_runs` (zoom space, unshifted
buffer, no crossing test) both decide "is this vertex within 128 units of the
cell's tile edges" with separately written left/right/top/bottom arithmetic.
Shift-rounding moves vertices up to half a unit, so a vertex exactly at the
window boundary can be in under one and out under the other. Owner proposed: a
`CellWindow` built once per cell, in one space, applied after rescale.

Reported by the polygon-engine hunt.

## CON-012 - Where does a Mercator coordinate land in tile pixels?

- Polygons: `int_ocean::quantize_coord` `round(v * 2^(z+12))` at `maxz = 14`,
  then `shift_round` by `14 - z`, minus `tile_origin`.
- Points: `projection::merc_to_tile_px`; lines: `tiles::to_tile_coords_into`;
  both `round((v * 2^z - tx) * 4096)` - the same function twice.

Algebraically equal, evaluated differently: a shared OSM node can land one
unit apart in a polygon and in a line or point at z < 14 (the unit test
`rescale_shape_s_gt_zero_within_one_of_direct_quantization` asserts exactly
this +-1). Examples: landuse edge against a street, boundary line against a
boundary polygon, POI centroid against its building. Benign at 1/16 px, but it
is why "lines and polygons share vertices" cannot be stated as an invariant.

Owner proposed: one quantizer into base space and the same `shift_round` for
every geometry kind - i.e. lines and points onto the integer pipeline (see
`notes/structural.md`).

Reported by the polygon-engine and coordinate-spaces hunts.

## CON-013 - Is this the last zoom, and what is the maximum zoom?

"The last zoom" (no simplification, no size filter) is decided from a literal
14 rather than `config.max_zoom`: `emit.rs` (`z >= 14` in the skip-size
filter, `z < 14` in the line pin path, `OSM_POLYGON_MAX_Z = 14` gating
`polygon_dp_tol`, `polygon_min_area`, `polygon_z_start` and the quantization
grid), `simplify.rs` (`z < 14` twice), `assemble.rs` (`z == 14`,
`13..=14` in the compression table, `z < 14` for line merging),
`resolve_boundary_match` (`max_zoom: 14`), `LayerMatch.max_zoom` (literal 14
at about 40 construction sites, never varies, yet min()'d against config).
The maximum is restated as `config.max_zoom > 14` in `run`, `0, 14` in the
artifact sites, `[u64; 15]`, `0..15u8`, `zoom >= 15`.

`TilegenConfig.min_zoom`/`max_zoom` exist, `run` rejects `max_zoom > 14`, and
the CLI hardcodes 0/14 with no flag, so zoom is a library-only half-axis. With
`max_zoom = 12` the top zoom would get DP and min-area filtering and lines the
size filter, while OSM polygons still quantize at z14; `build_metadata` also
writes `minzoom = layer.min_zoom()` without clamping to `config.min_zoom`.

Owner proposed: `Zoom`, `ZoomRange` (validated `min <= max <= MAX`),
`ZoomRange::is_last(z)`, a `ZoomSpan { maxz, top, bottom }` with
`shift_for(z)` for the pyramid (removing six `maxz - z` subtractions), and an
explicit decision whether "max zoom" means the configured last zoom or z14 -
or drop the two config fields since no operator can reach them and the
artifact refuses anything but z0-14.

Reported by the polygon-engine, pipeline, coordinate-spaces, layer-identity,
storage and archive-surface hunts.

## CON-014 - What zoom is a tile id, and where does a zoom's id range end?

The base formula `(4^z - 1) / 3` is spelled in `sort::TILE_ID_BASES`
(`zoom_from_tile_id` scans down from a hard 14), `pmtiles_writer::xy_to_tile_id`
and `tile_id_to_zxy` (inline, loops to z31), `assemble.rs`
`artifact_run_copy_items` `zoom_end`, and `ocean.rs` `z14_end`.
`zoom_from_tile_id` returns 14 silently for any id at or beyond base(15) while
`tile_id_to_zxy` returns z15+; `partition_from_key` clamps out-of-range ids
into the last partition (`.min(TILE_ID_LIMIT_EXCLUSIVE - 1)`) silently. If the
max zoom ever changes the sort misfiles z15 records into the last partition.

Owner proposed: `TileId(u64)` with `zoom()`, `zxy()`, `from_zxy()`,
`zoom_start(z)`, `zoom_end(z)` in one tiling module (see the coordinates
module in `notes/structural.md`).

Reported by the storage, coordinate-spaces and archive-surface hunts.

## CON-015 - What is one rendered pixel, and what are the area and tolerance thresholds?

- Integer: `OCEAN_DP_TOL_PX = 16`, `OSM_DP_TOL_PX = 16` (named PX, in extent
  units; neither derived from `PX`), `MIN_POLY_AREA = 256` (`PX*PX`),
  `ocean_min_area` returning a literal `256` not tied to `MIN_POLY_AREA`,
  `OCEAN_VW_AREA_2X = 256` (a doubled area, so a true 128).
- Float: `simplify_tolerance` and `merc_bbox_is_subpixel` as `1/(256*2^z)` in
  Merc units, `line_is_subpixel` (`MIN_LINE_EXTENT_SQ = PX*PX`).
- Mixed widths: `min_area: u64` in the pyramid, `true_area -> u128`,
  `signed_area_2x -> i128`, overlay `min_output_area: u64`.
- `polygon_z_start` decides a polygon's first zoom with the float subpixel
  test on the unquantized input, then survival is decided on integer area
  after quantize and simplify - one polygon, two spaces.
- Inclusivity: `clean_shape_in_place` keeps `area >= min`,
  `contour_area_is_below` drops `min > 0 && area < min`, the port's extractor
  applies its own comparison, and the single-contour fast path applies no
  filter so `normalize_into` re-filters (a deliberate re-check of a comparison
  written three times).

Agree today. Unrecorded: whether the factor of 2 between the VW kill
threshold (128 true units squared) and the min-area drop (256) is intended.

Owner proposed: `ExtentUnits`, `Area`, `Area2x` newtypes and one
`RENDER_PX = ExtentUnits(16)` with every tolerance derived from it and
`area.meets(min)` as the one comparison.

Reported by the polygon-engine and coordinate-spaces hunts.

## CON-016 - Which ring is an outer?

- `tile_detail::decode_polygon` (the gate's hash): `area > 0` outer, zero
  area is a hole, a leading hole opens a component.
- `mlt::group_rings_to_polygons`: self-calibrating, first ring's sign defines
  exterior (classifyRings-like), zero area counts as positive (`>= 0`).
  MapLibre's classifyRings skips zero-area rings.
- `ocean.rs` shapefile reader: `w == 0 || signed_area >= 0.0`.
- `multipolygon.rs`: `signed_area < 0.0` / `> 0.0` in two places, plus its
  own orientation and deterministic ring order, which the integer engine then
  normalizes again.
- In-engine: `orient_ring` by slot, `clip_shape_rect_fast` re-orients by
  hand, `append_translated_ring` re-orients by slot.
- `main.rs` diag prints "CW = outer" by its own sign; `geometry::signed_area`
  says "positive = counter-clockwise"; `encode_tile_shape` calls the outer
  `clockwise`; the overlay is configured `ContourDirection::CounterClockwise`.
  Same convention (positive shoelace in y-down renders clockwise), named
  three ways.

`tile_detail` and `mlt` already disagree on a zero-area ring and on a
geometry whose first ring is negative - latent on engine output, but they are
exactly the two sites (gate decoder, product encoder) that must agree on what
the engine emitted. For tolerant cross-producer comparison with a producer
that winds the other way, `tile_detail` would make every polygon an all-Hole
component.

Shoelace itself is implemented in `geometry::signed_area` (f64),
`int_ocean::signed_area_2x` (i128), `signed_area_tile_2x`,
`tile_detail::signed_area`, `mlt::ring_signed_area` (i64), the overlay test's
`ring_area`, inline in `vw_refresh` and `is_convex_ring`;
`geometry::ring_is_simple` is duplicated verbatim as
`main.rs::diag_ring_is_simple`.

Owner proposed: one `ring_role(area2x) -> Orientation` (zero case decided
explicitly, classifyRings semantics) in a `geometry::ring` module; better, the
engine emits roles in typed geometry so MLT and others never re-derive them
(see `notes/structural.md`).

Reported by the polygon-engine, encoding, storage and archive-surface hunts.

## CON-017 - How is an MVT command stream decoded?

Production decoders: `geometry::mvt_decode::decode_mvt_polygon` (lenient,
used by verify, assemble, tests; unknown command skips zero words),
`main.rs::diag_decode_polygon` (a verbatim copy with its own
`unzigzag_diag`; its comment says it exists "to avoid needing pub access to
geometry module"), `svg.rs` `decode_geometry_commands`, `verify.rs`
`validate_geometry_commands`, `tile_detail.rs` (strict/tolerant, checked),
`mlt.rs` `parse_points`/`parse_paths` (Result, plain `+=`),
`mvt/merge.rs::append_geometry` (re-delta; truncates on short input, copies
unknown commands) and `decode_line_segments` (wrapping; unknown command skips
`count*2` words). Protobuf walkers: `pmtiles_reader::decode_mvt_layers`
(silently stops on error), `verify::validate_mvt_*`,
`verify::collect_ocean_*`/`validate_ocean_*`, `svg::decode_*`,
`main::diag_*`, assemble's `ocean_layer_field`/`read_varint`/
`raw_contains_ocean`. Test-only: `mvt/tests.rs` `parse_fixture_*`,
`pipeline_tests.rs` `decode_commands_to_abs_coords`/`decode_commands_to_abs`,
`decode_segments`. Zigzag alone: `mvt::decode_zigzag`, `merge::unzigzag` (in
a module that imports `decode_zigzag`), `mlt::decode_zigzag`,
`mvt_decode::unzigzag`, `tile_detail::unzigzag`, `svg::zigzag_decode`,
`verify::zigzag_decode_u32`, `main::unzigzag_diag`, `way_index`'s pair.
`verify::decode_packed_varints` and `svg::decode_packed_varints` are
byte-identical.

They disagree on unknown-command handling (skip 0, skip `2*count`, copy,
error) and overflow (wrapping, panic, error). Each re-decides the R23 rule
that ClosePath does not move the cursor; all production copies get it right
today, but that is the decision that hid for three months and it now has
eight owners. In-crate tests check the encoder against decoders written by
the same hand - the symmetric-convention trap the earcut oracle exists for.
The defects already present (verify's lenient decoder, tile_detail
permissiveness) are in `notes/bugs.md`.

Owner proposed (all five hunts agree on the direction): one decoder - a
command iterator in `mvt/` or `tile_detail` with a strictness parameter and a
typed error - used by verify, svg, diag and tests; internal consumers (line
merger, MLT) should not decode at all once the tile model is typed geometry.
`pmtiles_reader::MvtLayer`/`decode_mvt_layers` should leave the container
reader.

Reported by the polygon-engine, encoding, archive-surface, coordinate-spaces
and pipeline hunts.

## CON-018 - What identifies a vertex across features?

`emit::merc_point_key` rounds Merc x/y times 1e12 (`FxHashSet<(i64, i64)>`
pin keys); `multipolygon::quantize` rounds times 1e9 (about 4 cm); base-space
pins are `FxHashSet<(i32, i32)>`; per-ring `Vec<Vec<bool>>` flags must stay
index-parallel to the shape. Ring assembly can treat two nodes about 4 cm
apart as one endpoint while pinning treats them as two, and points straddling
a 1e9 rounding boundary fail to join. Each is recovering node identity that
was exact as an E7 pair before projection. Owner proposed: carry the E7 pair
(or node id) alongside the Merc point and key on it. The pin-flag side is in
`notes/structural.md` (pinned rings).

Reported by the coordinate-spaces, storage and polygon-engine hunts.

## CON-019 - Does a feature get a copy across the antimeridian, and which tiles carry it in the buffer?

`antimeridian_shifts_for_bbox` is shared, but the shift-and-re-emit loop is
open-coded four times (`phase12.rs` twice, `relations.rs` twice), each
allocating the shifted `Vec<Point>` and using the unbuffered bbox
(`min_x < 0 || max_x > 1`), so a feature ending just inside x = 1 contributes
nothing to the buffer of tile x = 0. The same "buffer carries only features
that reach the tile proper" policy is applied by `for_each_tile_in_bbox` and
by the pyramid's root selection. They agree today; the policy is written
nowhere. Owner proposed: one `emit_wrapped(feature, emitter)` helper (or the
shared `emit_match`, next entry) and one documented sentence about buffer
population in `reference/`.

Reported by the coordinate-spaces and pipeline hunts.

---

# Emission dispatch and per-layer policy

## CON-020 - How does a `LayerMatch` become sort records?

The way path (`process_planned_way_into`), the relation path
(`process_prepared_relation_into`) and phase12 node emission each: `match
m.geom_expect`; clamp `z_lo = m.min_zoom.max(min_zoom)` /
`z_hi = m.max_zoom.min(max_zoom)`; fetch the cap with
`fanout_caps.get(m.layer as usize).copied().unwrap_or(0)` (hiding an
impossible case); run their own antimeridian loop; harvest cap events with an
identical `/15 %15` loop and a duplicated "clear after harvest or
double-count" comment. `emit_polygon_feature` and `emit_multipolygon_feature`
are about 95% the same function. Three to four positional arguments (zooms,
caps, simplify) travel separately through `process_relation_blocks`,
`process_planned_way_into` and every emitter. The silent drop in the relation
arm is in `notes/bugs.md`.

Owner proposed: one `emit_match(osm_id, FeatureGeometry::{Way, Area{outer,
inners}}, &LayerMatch, &EmitCtx, &mut Acc)` in `emit.rs` with an exhaustive
match, owning the shift loop, cap lookup and cap-event harvest;
`EmitCtx { zooms, fanout_caps, simplify }`.

Reported by the pipeline, layer-identity and coordinate-spaces hunts.

## CON-021 - Which layers get which geometry policy?

- Lines: `emit_line_feature` decides "connectivity-sensitive line layer"
  twice in one function (`skip_size_filter = z >= 14 || layer == Boundaries
  || layer == Streets`; `skip_bbox_check = layer == Boundaries ||
  layer == Streets`).
- Polygons: `polygon_min_area` exempts `StreetPolygons | Bridges`, but
  `polygon_z_start` (drop until the bbox stops being subpixel) does not, so a
  tiny bridge polygon is kept by min-area but can have its first zoom delayed.
  Whether intended is not recorded.
- OSM vs ocean polygon policy is one question ("what is layer L's
  simplification at z?") answered in two modules with two constant sets:
  emit.rs (`polygon_dp_tol` = `OSM_DP_TOL_PX * factor`, 0 at z14;
  `polygon_min_area` = `MIN_POLY_AREA`; DP; `maxz = 14`) and ocean.rs
  (`ocean_dp_tol`, `ocean_min_area` literal 256, VW, factor deliberately not
  applied). Only the ocean side is versioned.
- Per-zoom policy reaches the engine as two boxed closures in `PyramidParams`
  (`dp_tol`, `min_area`) plus a `Simplifier` enum, and `emit_cell` hard-codes
  more policy on the closure results (`dp_tol > 2` decides window thinning,
  `dp_tol > 0` gates the convex fast path). In VW mode the tolerance's value is
  ignored except its sign, yet `dp_tol > 2` stays live - so the ocean,
  nominally VW-only, also runs DP at tol 2 on window runs because its
  `dp_tol` (16) is greater than 2. Possibly intended; nothing says so.

Owner proposed: per-layer policy in the profile's layer table
(`keep_subpixel`, `min_area: MinAreaPolicy`), and a
`trait PolygonPolicy { fn at(&self, z) -> ZoomPolicy }` with
`ZoomPolicy { simplify: Simplify, min_area: Area, thin_window: bool }` and
`enum Simplify { None, Dp { tol }, Vw { area2x } }`, implemented by
`OsmPolicy { layer, tol_scale }` and `OceanPolicy`. The engine stops knowing
layer names or which producer it serves; ocean-policy constants move beside
`OCEAN_POLICY_VERSION`.

Reported by the pipeline, Shortbread, layer-identity, coordinate-spaces and
polygon-engine hunts.

## CON-022 - What is a layer's zoom floor?

- `Layer::min_zoom()` - hand-written; only consumer is
  `pmtiles_writer::build_metadata` (TileJSON `vector_layers` minzoom).
- Each matcher's literal `min_zoom` (about 40), `boundaries::boundary_min_zoom`,
  `emit.rs::enrich_polygon_matches` (boundary_labels z2/z3/z4 by area
  thresholds 2M/700k/100k km2 with an admin-2 special case, re-reading
  `admin_level` out of the attr vector by string key with `.unwrap_or(0)`),
  `boundaries.rs` default 5 for boundary_labels with a comment that overrides
  happen "later in tilegen", and `water_*_labels` hard-coding 14/12.
- The per-attribute min zoom (third tuple slot) is a third concept.

The hunts disagree on whether these already disagree. The Shortbread hunt
reports `Layer::min_zoom()` says `water_polygons_labels` = 4 and
`water_lines_labels` = 9 while the matchers emit 14 and 12/14 - valid only as
a lower bound. The layer-identity hunt spot-checked and reports agreement
(land 7, streets 5, place_labels 4, boundary_labels 2 via the override,
water_polygons 4, water_lines 9, public_transport 11). Both readings stand.
Neither side is checked: nothing asserts `Layer::min_zoom()` is a lower bound
of what matchers emit, and the spec harness skips boundary_labels' min zoom.

Owner proposed: the profile, with the floor derived as the min over the
layer's kind table, and `boundary_label_min_zoom(level, area_km2)` as a
profile function emit calls with the area it measured.

Reported by the Shortbread and layer-identity hunts.

## CON-023 - What is a boundary, and what does a boundary line match look like?

Admin levels:
- `boundaries::boundary_min_zoom` (`2 => 0, 4 => 7`, via `parse_i64`, which
  strips commas);
- `boundary_prescan::fold_relation` (`parse::<u8>` then `matches!(level, 2 | 4)`);
- `phase12::resolve_boundary_match`
  `.expect("boundary metadata only contains supported levels")` - the drift
  detector: adding admin_level 6 to the prescan without the profile panics
  mid-phase12. The parsers disagree on input like `"0,2"` and `" 2"`.
- `AdminLevel` is `i64` in the match, `u8` in `BoundaryMeta`,
  `u8::try_from(..).expect(..)` in phase12, `Int` in attrs.

The match: `boundaries::match_boundaries_line` and
`phase12::resolve_boundary_match` both build the Boundaries `LayerMatch`
(attrs `admin_level`, `maritime`, `disputed`; maritime =
`maritime=yes || natural=coastline` copied verbatim; `max_zoom: 14`,
`paint_rank: 0`, `GeomExpect::Line`). phase12 runs on every way's matches and
overwrites `min_zoom` and `attrs`, so the profile copy survives only as the
`possible_feature` truthiness probe - the visible implementation lives in the
pipeline. Disputed is decided three ways (way `disputed=yes` twice, relation
`boundary=disputed` in the prescan).

Owner proposed: `enum AdminLevel { Country = 2, State = 4 }` with one
`parse` and `min_zoom()`; `shortbread::boundaries` exposes
`boundary_line(tags, Option<BoundaryMeta>)` and owns `BoundaryMeta`, its fold,
the resolution and the label-zoom policy; phase12 supplies only the relation
fold and area. `place_labels` moves out of `boundaries.rs` to its own module.

Reported by the pipeline, Shortbread and layer-identity hunts.

## CON-024 - Which relations produce area geometry?

`prepass_relation_plan` (phase12) and `prepare_relation` (relations.rs) each
re-implement the `type in {multipolygon, boundary}` gate (identical
hand-rolled loops) and `match_element(MultiPolygon)`. The prepass decides
which ways enter `way_index`; the tail decides which relations read them back.
Drift shows up as `missing_relation_way_refs`, which reads as input damage
rather than a code bug. A third answer comes from outside: the injected
WayMembers-v1 bitmap has superset semantics (quantified by
`superset_ways_count`). Owner proposed: one
`area_relation_matches(tags) -> Option<SmallVec<LayerMatch>>` in shortbread.

Reported by the pipeline hunt.

## CON-025 - Which OSM geometries does each layer accept?

`match_closed_way` and `match_multipolygon` are two hand-kept lists that must
agree on every polygon and point layer; ClosedWay also repeats the whole
`match_open_way` list. They agree today; nothing checks it, and the spec only
exercises ClosedWay. Separately, `GeomExpect` lives on each `LayerMatch`, so
"is `land` a polygon layer" has no answer (land also emits `cliff` lines;
three label layers are point layers fed from polygons); the oracles discover
polygon layers from data because the crate cannot say. `PolygonCentroid` vs
`PolygonPointOnSurface` is chosen per call site (`water_polygons_labels` and
`addresses` use Centroid, everything else PointOnSurface) and the spec harness
accepts either; the comments do not say whether that is deliberate.

Owner proposed: each layer spec declares `accepts: &[OsmGeomType]` and its
allowed output geometry kinds; dispatch iterates the table; the centroid
choice becomes a layer property or is collapsed.

Reported by the Shortbread and layer-identity hunts.

## CON-026 - What kind values exist per layer, and at what zoom?

- The matchers (land.rs `land_match_*`, streets.rs `highway_zoom`,
  `aeroway_zoom`, `railway_zoom`, `street_label_zoom_highway` plus inline
  arms in `street_label_zoom`, `street_label_kind_raw` re-calling the line
  zoom functions as a membership test, `street_polygon_match`).
- `paint_order::is_known_land_kind` / `is_known_street_kind` and
  `land_paint_rank` / `street_class_rank` (fallbacks `_ => 0` and the
  `UNKNOWN_STREET_CLASS` sentinel), plus the paint_order test lists - a fourth
  copy checked against the third while the matcher is never consulted.
- `wire_format::KIND_VALUES`, a linear-search interning table (up to 129
  string compares per kind per zoom) where a miss silently falls back to an
  inline string; includes `motorway_link` etc., which are dead if streets.rs
  strips `_link` into the `link` attr.
- `corpus/style.toml`, whose comment says every kind `is_known_land_kind()`
  accepts has a match - a hand-held agreement across repos.
- `street_paint_rank(kind, ...)` is fed both stripped and raw values;
  `street_labels` keeps `_link` on purpose.

A kind added to `land_match_landuse` without a rank silently gets rank 0, and
verify then rejects the archive as "unknown land kind" - at verify time, only
on an archive containing one. The key-order mismatch already live between
street label zoom and kind is in `notes/bugs.md`.

Owner proposed: `LandKind`, `StreetKind` (with `link` separate,
`from_osm("motorway_link") -> (Motorway, link=true)`) and so on per family,
each carrying `(name, min_zoom, paint_rank)` in one table with `as_str()`
for the wire; `is_known_*` becomes `from_str`; `KIND_VALUES` derives from the
enums; brokkr's style test can enumerate them.

Reported by the Shortbread, encoding and layer-identity hunts.

## CON-027 - What attribute keys exist, and what type is each?

- `shortbread::Attr = (&'static str, AttrValue, u8)` - string key, bare `u8`
  min zoom (unchecked against the layer floor or 14). The comment defends the
  tuple by construction-site count; the sites already go through `attr_*`
  helpers.
- `wire_format::KEY_NAMES` + `key_to_id`: `position(..)` per attribute per zoom
  per feature, `panic!` on an unknown key - a typo in a Shortbread rule is a
  runtime crash in phase12 found only when a matching object is hit.
- `mlt::build_tile_model` re-infers each column's type per tile by scanning
  values and falls back to a stringifying `Mixed` column when a key carries
  two types in one tile, so the same key can be an i64 column in one tile and
  a string column in the next - a consumer-visible inconsistency. The schema
  is static in Shortbread.
- TileJSON `vector_layers` entries carry no `fields` because no attribute
  schema exists in code, and verify checks only layer names.
- `attr_dyn` allocates a `String` per dynamic attr; `pois::would_match_poi`
  builds and discards the full POI attr vector; phase12's `possible_feature`
  runs `match_element` up to twice per way building and discarding every
  attr (hot-path allocation spent on predicates).

Owner proposed: `#[repr(u8)] enum AttrKey` owned by Shortbread (the
discriminant is the wire id), `struct Attr { key, value, from_zoom: Zoom }`
with `attr.visible_at(z)`, per-key value types from the schema (MLT columns
typed statically; a wrong-typed value is a bug), and a classify step that
returns kind and zoom without building attrs. Typed label payloads
(`admin_level: AdminLevel`) lowered to attrs only at encode time.

Reported by the encoding, Shortbread and layer-identity hunts.

## CON-028 - Which layers have a paint order, and how is rank recovered?

Producers: land.rs (`land_paint_rank`, including `"cliff"`) and streets.rs
(`street_paint_rank` for streets and street_polygons); every other matcher
passes `paint_rank: 0`. `verify::validate_paint_order` re-decides the set by
name (`matches!(layer, "land" | "streets" | "street_polygons")`) and
re-decides how rank is derived at low zoom (`"streets" if z < 11 =>
street_class_rank`). That 11 exists only because `link`/`tunnel`/`bridge`
carry attr min zoom 11 in streets.rs (written three times there); move one
and verify false-fires or weakens. Verify re-deriving rank from the same
function is a deliberate re-check and not itself the finding; verify choosing
its own layer list and zoom threshold is.

Owner proposed: a layer-spec `paint_order: Option<fn>` that verify iterates,
rank recovery applied to the attributes actually present at z (or
`shortbread::paint_order::rank_for(layer, z, attrs)`), and a named
`STREET_DETAIL_ZOOM` if a constant survives. `PaintRank` as a newtype only
paint_order can mint.

Reported by the Shortbread, layer-identity and archive-surface hunts.

## CON-029 - How is a length or numeric OSM tag parsed?

`land.rs::parse_building_measurement_meters` and `pois.rs::parse_ele_meters`
have character-identical unit-stripping preludes, each with a private copy of
`normalize_numeric_token` - a duplicated decision ("how is an OSM length
interpreted"): a fix for `"12 m"` or `"5'6\""` in one will not reach the
other. `parse_i64` (comma-stripping, used for population and admin_level) is a
third policy and disagrees with `normalize_numeric_token` on `"1,5"`
(15 vs 1.5). Owner proposed: one `osm_value` module with `parse_length_m`,
`parse_count`, `parse_int`, each documenting its locale policy.

Reported by the Shortbread hunt.

## CON-030 - What is a POI, for the address layer?

Decided once (`pois_match`) and reused by addresses through
`would_match_poi`. The Shortbread hunt judges this acceptable as long as it
stays one call, but notes the address layer's "POI" has silently become "POI
layer membership": if POIs gain a zoom or geometry restriction, addresses
follow without anyone deciding that. A one-line comment in `land.rs`
acknowledges the coupling.

Reported by the Shortbread hunt.

## CON-031 - How is the ocean layer identified?

By `"ocean"` string literals in `assemble.rs` (`raw_contains_ocean`,
`ocean_layer_field`), `ocean.rs` (artifact vector_layers check) and
`verify.rs` (twice, for geometry stats and ring checks); elsewhere by
`Layer::Ocean`. `Layer::Ocean` itself lives in the profile enum although no
matcher produces it - it exists so fanout caps and sort keys can index it, so
"Shortbread layer" and "output layer" are the same type. Owner proposed:
`Layer::Ocean.name()` everywhere; in a layer table, Ocean is the one row with
no `classify`.

Reported by the layer-identity and Shortbread hunts.

## CON-032 - What does the `Layer` enum's discriminant decide?

The discriminant alone decides the wire id in the sort key, order inside a
tile, MVT layer order, `vector_layers` order, and the stats index. `ALL`,
`name()`, `min_zoom()` and the `#[repr(u8)]` discriminants are four parallel
listings, so a reordered `ALL` would silently shift array indices used by
`fanout_caps` and the provenance contract. Owner proposed: one declarative
source (macro or the layer table) generating the enum, `ALL`, `name`,
`from_name`, with tile order as an explicit field.

Reported by the layer-identity and Shortbread hunts.

---

# Storage, sort and resources

## CON-033 - What is this run's effective budget and thread count?

| Question | Sites |
|---|---|
| sort chunk default | `DEFAULT_SORT_CHUNK_SIZE` in `pipeline/mod.rs` (private) and `phase12.rs` (`pub(super)`), "0 means default" resolved in both `run` and `phase_read_and_process` |
| assemble batch default | literal 32 MiB in both `phase_assemble_with_ocean` and `phase_assemble_partitions` |
| way budget default | inside phase12's way-block arm, keyed on effective locations mode; restated as `128M`/`4G` in `--way-budget` help |
| dedup default | `MAX_DEDUP_ENTRIES` in the writer plus assemble's "if > 0 then set"; restated as "1000000" in help |
| thread default | `available_parallelism().unwrap_or(4)` twice in `main.rs` (run, ocean-build) |
| per-actor thread split | phase12 (decode /3, node workers /4 clamp, in-flight max 8), assemble (clamp 8 + env), `plan_pieces` (unbounded) |
| ocean per-fold flush | `min(sort budget, OCEAN_CHUNK_SIZE_LIMIT)` in ocean.rs |
| way-offset sort memory | `way_index::OFFSETS_SORT_BUDGET` (256 MB, no knob) |
| relation block buffer | `REL_BLOCKS_BUFFER_CAP` + env override in phase12 |
| layer-stats firehose | `ELIVAGAR_LAYER_STATS` read by `pipeline::layer_stats_enabled` (every call) and `ocean::ocean_layer_stats_enabled` (OnceLock), agreeing only because both test `var_os(..).is_some()` |

They do not disagree yet beyond the defects in `notes/bugs.md` (thread doc,
env fail-open, zero sentinel). Budgets rightly stay out of the contract, but
the effective values never reach provenance or the sidecar as one record, so
"what memory shape did this run have" can only be reconstructed by re-deriving
defaults from source at that commit. Validation (the 64M/1M minimums) lives
only in clap `value_parser`s, so a library caller (brokkr; `lib.rs`'s doc
example passes zeros) can pass `sort_chunk_size: 1`. `dedup_cap` and
`sort_chunk_size` are the same `usize`, adjacent and swappable.

Owner proposed: a `resources` module with
`Resources::resolve(&config, &InputFacts) -> ResolvedResources`, called once
after header detection (the locations-dependent way default needs it),
holding every budget, thread split and env override (validated, failing
loudly), emitted once as `res_*` sidecar counters and passed by reference.
Budget types per the zero-sentinel bug entry.

Reported by the storage and pipeline hunts.

## CON-034 - What is the sort budget a budget of?

`--sort-budget` is charged independently by the `SortWriter` buffer, the
`SpillCoalescer` buffer (which keeps filling while a swapped-out full buffer
is sorted and written; its doc concedes two producers can trip it
concurrently), the drain thread's writer, and the 2x pre-serialization on
compressed writes. Real peak sort-side memory is a small multiple of the flag,
stated nowhere. Owner proposed: one budget shared through the chunk store,
buffers drawing from it.

Reported by the storage hunt.

## CON-035 - Per-layer, per-zoom record tally

`SortWriter::push` (sort.rs) and `RecordTally::record` (emit.rs) make the
identical classification (layer from key, `if layer < 32`, zoom from tile id,
`if zoom < 15`, index `layer * 15 + zoom`), and `SortWriter::merge_tally`
re-implements `RecordTally::merge`. `push_untracked` exists purely to stop
the two copies double-counting. `FanoutStats` in stats.rs is a third parallel
`32 * 15` structure. The `32` is `Layer::count()` (26) rounded up, so a 33rd
layer silently falls out of stats. Owner proposed: `SortWriter` holds a
`RecordTally` (or the tally leaves the writer), on a typed
`PerLayerZoom<T>`; `RecordTally` belongs in `sort`, not `pipeline::emit`.

Reported by the storage, layer-identity and pipeline hunts.

## CON-036 - How are chunk ids allocated?

The chunk-number allocator is an `Arc<AtomicUsize>` created in phase12,
attached to `SortWriter` (`attach_chunk_counter`), passed separately to
`SpillCoalescer::new`, used by `write_partitioned_payload_chunks` callers,
reconciled on `detach_chunk_counter`. Its doc warns the counter "MUST be
initialized to the current `chunk_count()` by the caller" - an invariant
enforced by prose. The count is reconstructed from three sources
(`chunk_count`, the atomic, adopted path lists) and resume validates it
against a directory scan. Owner proposed: the chunk store (manifest owner)
allocates; writers and coalescers get a cloneable `ChunkSink`.

Reported by the storage hunt.

---

# Archive surface

## CON-037 - Can this archive be read, and as what?

- `ArchiveView::open` rejects non-MVT or non-gzip.
- `verify::verify_opts_unique` repeats the check with different text.
- `ocean.rs` re-checks `tile_type() != 1 || tile_compression() != 2` right
  after `ArchiveView::open` did.
- `PmtilesReader::read_tile` decides by not deciding (always gunzips).
- The artifact-contract cousin in `pipeline/mod.rs` is a stricter version of
  the same predicate.
- Metadata readability: `PmtilesReader::read_metadata` lossy,
  `ArchiveView::metadata` strict.

The reader defects are in `notes/bugs.md`. Owner proposed: a typed `Header`
plus `TileContract`, with `reader.tile_decoder() -> Result<Decompressor,
Unsupported>` deciding once.

Reported by the archive-surface hunt.

## CON-038 - Which payloads are equal (dedup)?

`add_tile` and `add_run` each compute two SipHash fingerprints, probe the
bucket, push and maintain `DedupStats`, and already differ: `add_tile` counts
`reject_len_mismatch`/`reject_fp_mismatch`, `add_run` counts neither;
`add_tile` is `#[hotpath::measure]`, `add_run` is not; `add_run` has no run
ordering check. The test helper `compute_dedup_hashes` is a third copy of the
hash. `add_run` checks `data.len() > u32::MAX` and then also `u32::try_from`;
`add_tile` casts with `as` after the check. Owner proposed: a
`DedupIndex { lookup_or_insert(&mut self, data) -> (BlobRef, bool) }`. The
hunter also notes `DefaultHasher` output is release-dependent (fine
in-memory) and one xxh3-128 would halve hashing on the assemble hot path.

Reported by the archive-surface hunt.

## CON-039 - What is the PMTiles directory format?

`encode_directory` (writer, private) and `decode_directory` (reader, public)
are two halves of one format with no shared definition of the column order or
the `offset + 1` / `0 = contiguous` convention. `DirEntry` and `RawDirEntry`
are the same struct defined twice; `run_length == 0` meaning "leaf pointer"
is a sentinel in both. Writer tests hand-decode with a Cursor and reader tests
hand-encode (`encode_directory_raw`), so the pair is tested symmetrically only
through full archives. Header offsets likewise are spelled independently in
`build_header`/`write_header_counts`/`write_header_bounds`, in
`PmtilesReader::open` plus about 15 accessors, again in `ArchiveView::open`,
in inspect (with a private `read_i32_le`), in inspect tests
(`set_header_u64(path, 32, 0)`) and in writer tests (own `read_u64_le`).
Owner proposed: one `archive` module with `Header` encode/decode,
`DirEntry::{Run, Leaf}`, `directory` encode/decode with a
`decode(encode(x)) == x` property test (see `notes/structural.md`).

Reported by the archive-surface hunt.

## CON-040 - Is this tile a seam tile (in verify)?

`verify.rs` computes `x == 0 || x == (1 << z) - 1` at the unique-payload
grouping key, again before the geometry-outcome cache lookup, and again inside
`validate_geometry_commands` to pick the delta limit. Cache correctness
(outcome keyed by `(z, seam)`) depends on all three agreeing and on the
paint-order check depending only on `z`; an x- or y-dependent check added to
`validate_mvt_geometry` would make the cache replay the wrong outcome. The
seam definition (antimeridian columns) is also presumably owned by the
geometry engine's buffer logic. Owner proposed: a
`ValidationClass { z, seam }` computed once and passed in, so the validator
cannot see `x`.

Reported by the archive-surface hunt.
