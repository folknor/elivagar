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

## CON-017 - How is an MVT message (not its command stream) walked?

The command stream is settled: every in-crate reader decodes through
`mvt::decode`. What remains is the protobuf layer around it, walked
independently and with different error policies:
`mvt::layers::decode_mvt_layers` (tag-read errors end the loop silently),
verify's four per-tile parses (`decode_mvt_layers`, `validate_mvt_geometry`,
`validate_ocean_rings`, `collect_ocean_geometry_stats`, the last two
skipping unreadable fields silently), `svg::decode_mvt_geometry`/
`decode_layer`/`decode_feature` (read errors on length-delimited fields
dropped), `diag::diag_layer` (all read errors dropped), and the artifact
consumer's `ocean_layer_field`/`read_varint` (`assemble/artifact.rs`). Only
`tile_detail`
reads the tile/layer/feature messages with a stated policy.

Ring grouping (first nonzero-area ring sets the exterior winding,
zero-area rings skipped) is also spelled twice, in `tile_detail`
(`group_detail_rings`) and `mlt::group_rings_to_polygons`.

`MvtLayer`/`decode_mvt_layers` already left the container reader for the
crate-private `mvt::layers`. Owner proposed: verify, svg
and diag walk messages through one borrowed message reader with
`tile_detail`'s strictness. MLT and the line merger stop decoding at all
once the tile model is typed geometry (STR-013).

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

Land and street kinds are typed (`LandKind`, `StreetClass`, `LinkClass`,
`StreetKind`), each owning wire name and paint rank, and a wire_format test
checks they are interned. What remains is the other families: water
(`water.rs` polygon, line, label, dam and pier kinds), transport
(`aerialway_kind`, ferry, `public_transport_match`), place labels
(`place_label_info`) and sites (`sites_kind`) still emit `&'static str`
literals from their row tables, so nothing ties them to
`wire_format::KIND_VALUES`. A new kind in one of them silently misses the
dictionary (costing bytes, not correctness - the table is a compression
dictionary with an inline-string fallback, and that fallback stays). The
gap: enums for those families, or at least a coverage test enumerating
their row tables against `KIND_VALUES`. Deriving `KIND_VALUES` from enums
would renumber ids and needs a `CHECKPOINT_VERSION` bump.

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
