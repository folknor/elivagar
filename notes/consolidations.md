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

---

# Geometry and coordinates

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

---

# Emission dispatch and per-layer policy

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

---

# Geometry, continued

## CON-041 - Ring orientation and shoelace area inside the engine

`geometry::ring_role` now decides orientation for `tile_detail`, `mlt`,
`multipolygon.rs`, `diag` and the ocean shapefile reader. Still deciding it
by hand inside the polygon engine: `orient_ring` by slot,
`clip_shape_rect_fast` re-orienting, and `append_translated_ring` re-orienting
by slot. Shoelace area is still implemented separately in
`geometry::signed_area` (f64), `int_ocean::signed_area_2x` (i128),
`signed_area_tile_2x`, the overlay test's `ring_area`, and inline in
`vw_refresh` and `is_convex_ring`. The convention is also named three ways:
`geometry::signed_area` says positive is counter-clockwise,
`encode_tile_shape` calls the outer clockwise, and the overlay is configured
`ContourDirection::CounterClockwise` (the same convention, since positive
shoelace in y-down renders clockwise). Better still, the engine emits roles in
typed geometry so consumers never re-derive them (see `notes/structural.md`).

Residue of an earlier entry, narrowed 2026-11-04 after `ring_role` landed.
