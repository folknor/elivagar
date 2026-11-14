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
moot only because artifact admission (`OceanArtifact::admit`) already refuses a level mismatch - the
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

The zoom-constant half is settled: "max zoom" is `tiling::MAX_ZOOM` (the
Shortbread top, z14), every last-zoom policy asks about it, and the
`TilegenConfig` zoom bounds are a truncation window (OSM construction runs
over each layer's own range; emission is cut). What remains:

- The pyramid's grid arithmetic: `maxz`, `z_top` and `z_bottom` travel as
  three bare `u8`s in `PyramidParams`, and the shift `maxz - z` is written out
  at each use in `geometry/pyramid.rs` plus `OceanBand::artifact_tile_rect`
  in `ocean/band.rs`. The proposed `ZoomSpan { maxz, top, bottom }` with
  `shift_for(z)` belongs with the coordinate-space `Grid` in
  `notes/structural.md`.
- The window is still two bare `u8`s checked in `run` (`min <= max <=
  MAX_ZOOM`); no `ZoomRange` type carries the validation, and `EmitCtx`,
  `relations.rs` and `phase12.rs` thread the pair separately. Ocean's
  `PassWindow` (`ocean/plan.rs`) is a pass-local intersection of it, not a
  validated window type.

Reported by the polygon-engine, pipeline, coordinate-spaces, layer-identity,
storage and archive-surface hunts.

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

## CON-034 - What is the sort budget a budget of?

`--sort-budget` is charged independently by the `SortWriter` buffer, the
`SpillCoalescer` buffer (which keeps filling while a swapped-out full buffer
is sorted and written; its doc concedes two producers can trip it
concurrently), the drain thread's writer, and the 2x pre-serialization on
compressed writes. Real peak sort-side memory is a small multiple of the flag,
stated nowhere. Owner proposed: one budget shared through the chunk store,
buffers drawing from it.

Reported by the storage hunt.
