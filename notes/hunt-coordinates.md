# Hunt: coordinate spaces and units

Scope: every coordinate space and unit the crate uses, which of them travel
as bare primitives, every site that converts between them or decides which
space, extent or zoom applies, and whether those sites agree.

No code was edited. The source line numbers below are a snapshot from when
this note was written. They will drift.

## The spaces as they exist today

| # | Space | Representation today | Where it is minted |
|---|---|---|---|
| S1 | WGS84 fixed-point (e7 degrees) | bare `i32` pairs, usually ordered `(lat, lon)` but not always | PBF decode, node store, way index |
| S2 | WGS84 float degrees | bare `f64` and the tuples `(f64,f64,f64,f64)` / `(f64,f64,u8)` | `PmtilesConfig.bounds/center`, `inspect.rs` e7 decode |
| S3 | EPSG:3857 metres | bare `f64` x/y read straight out of the shapefile | `ocean.rs` record reader |
| S4 | Normalized Web Mercator [0,1] (unwrapped: x may be < 0 or > 1) | `geometry::Point { x: f64, y: f64 }`, `MercBbox`, `ClipRect` | `project_e7`, `from_epsg3857` |
| S5 | Max-zoom pixel integers ("base space": 2^(maxz+12) units across the world) | `IntPoint` / `Contour` / `Shape` / `IntRect` (all `i32`), with `maxz: u8` passed alongside | `quantize_*` in `int_ocean.rs` |
| S6 | Per-zoom pixel integers ("zoom space": 2^(z+12)) | the same `Shape` / `IntRect` types | `rescale_shape*` (shift-round) |
| S7 | Tile-local MVT units (0..4096, plus or minus a 128 buffer) | `(i32, i32)` tuples, then delta-zigzag `Vec<u32>` command streams | `to_tile_coords_into`, `merc_to_tile_px`, `encode_tile_shape` |
| S8 | Tile address | bare `(u8, u32, u32)`, `PyramidCell { z, tx, ty }` in one module only | everywhere |
| S9 | PMTiles Hilbert tile id | bare `u64` | `pmtiles_writer::xy_to_tile_id` |
| S10 | Sort key | `type SortKey = u64` (a transparent alias) | `sort::make_sort_key` |
| S11 | Sort partition id | bare `usize` | `sort::partition_from_key` |
| S12 | Vertex-identity hash spaces | `(i64, i64)` at 1e12 (`emit::merc_point_key`), `(i64, i64)` at 1e9 (`multipolygon::quantize`), `(i32, i32)` base-space pins | as listed |
| S13 | Tolerance and area units | bare `i64` / `u64` / `i128` / `f64` | constants in `geometry/mod.rs`, `int_ocean.rs`, `ocean.rs` |

S5, S6 and S7 all share one Rust type (`(i32, i32)` or `IntPoint`). S9, S10
and S11 are all bare integers. S4 has a type, but the type does not record
whether a value has been wrapped into [0,1] or is unwrapped across the
antimeridian. No document in `reference/` names or defines these spaces;
the only description of each one is in comments at the sites that use it.

---

## 1. Axes that should be types

### 1.1 One `i32` point type covers three spaces (S5, S6 and S7). This is the biggest finding.

`Shape`, `Contour`, `IntPoint` and `IntRect` carry base-space coordinates,
zoom-space coordinates, and (once `append_translated_ring` subtracts
`tile_origin`) tile-local coordinates. The space is implied by which
function you are in:

- In `pyramid.rs`, `buffered_cell_rect_base`, `buffered_range_rect_base`,
  `cell_tile_size_base` and `build_edge_flags` work in base space. They
  shift both the tile size and the buffer by `maxz - z`.
- `thin_window_runs` and `int_ocean::buffered_tile_rect` / `tile_origin` work
  in zoom space. They use `TILE_EXTENT_I32` and `TILE_BUFFER_I32` with no
  shift.
- `encode_tile_shape` turns zoom space into tile-local space.

Nothing stops a caller from intersecting a base-space fragment with a
zoom-space `buffered_tile_rect`. The one thing that tells the reader which
space a rect is in is the `_base` suffix on some function names. The descent
"cut identity" (a child's buffered rect is a subset of its parent's, and the
base and zoom rects agree after `>> s`) holds only because 4096 and 128 are
both exact under every shift. That is an invariant held in step by hand,
not a type.

What should change: introduce `BasePt<const MAXZ>`, or a runtime-tagged
`Grid { maxz }` plus `BaseRect` / `ZoomRect { z }` / `TileLocal`. Then make
`rescale` the only `BaseRect -> ZoomRect` conversion and `to_tile_local`
the only `ZoomRect -> TileLocal` conversion. A const generic is awkward
because ocean uses two different `maxz` values (7 and 14). A `Grid` value
that owns `maxz` and hands out typed rects is the practical form, and it
is also the natural owner for the arithmetic duplicated in section 2.

### 1.2 `maxz` travels as a bare `u8` next to the data it describes

Every quantized `Shape` was produced at some `maxz`, yet the shape does not
carry that value. `PyramidParams.maxz`, `OceanPassGrid.max_zoom`, the
`maxz` argument of `quantize_polygon*`, and `OSM_POLYGON_MAX_Z` are all
separate copies of the answer to "what grid is this shape on". Section 2.4
shows two of those copies already disagreeing. A shape quantized at z7 and
a shape quantized at z14 can be passed to the same `intersect_shapes_into`
call, and nothing will object.

### 1.3 The zoom domain has no type: the literals 14 and 15 appear in about 80 places

`TilegenConfig.min_zoom` and `max_zoom` exist, `run` rejects
`max_zoom > 14`, and the CLI hardcodes `min_zoom: 0, max_zoom: 14`
(`main.rs`). So zoom is a library-only axis that no operator can set. Even
so, the code treats "the last zoom" as a literal 14 in places that should
mean `config.max_zoom`:

- `emit.rs`: `z >= 14` in the skip-size filter, `z < 14` in the line pin
  path, and `OSM_POLYGON_MAX_Z = 14`, which both picks the quantization
  grid and switches off DP and min-area.
- `simplify.rs`: `if z < 14` (two sites).
- `assemble.rs`: `z == 14` in `artifact_run_copy_items`, `13..=14` in the
  compression-level table, and `if z < 14`.
- `sort.rs`: `zoom_from_tile_id` starts its scan at a hard 14.
- `[_; 15]` arrays, and the `layer * 15 + zoom` index arithmetic, appear in
  `stats.rs`, `emit.rs`, `sort.rs`, `assemble.rs`, `ocean.rs`,
  `relations.rs`, `phase12.rs` and `verify.rs`. That is a hand-rolled 2-D
  array in eight files, decoded by `idx / 15` and `idx % 15` in three of
  them.

If `max_zoom = 12` were ever set (the library allows it), z12 would get DP
simplification and min-area filtering that the real last zoom never gets.
OSM polygons would still quantize at z14. In effect, "max zoom" means two
different things.

What should change: add `struct Zoom(u8)` with `Zoom::MAX` and
`ZoomRange { lo, hi }`, plus `PerZoom<T>([T; Zoom::COUNT])` and
`PerLayerZoom<T>`. Indexing should be typed, so `idx / 15` disappears.
Then decide explicitly whether "max zoom" means "the configured last zoom"
(overzoom detail, no simplification) or "z14". The alternative is to drop
`min_zoom` and `max_zoom` from `TilegenConfig` entirely: no operator can
reach them, and the ocean artifact already refuses anything other than
z0-14. Either outcome beats the current half-axis.

### 1.4 Tile address and tile id are bare integers

`(z, x, y)` is `(u8, u32, u32)` everywhere except `pyramid::PyramidCell`,
which is crate-private. The tile id is a bare `u64` and `SortKey` is a
transparent alias for `u64`, so `make_sort_key(tile_id, ...)` will accept a
partition id, an artifact offset, or a key. Nothing validates
`x, y < 2^z`:

- `xy_to_tile_id` produces a wrong id for out-of-range x/y without failing
  (in `hilbert_xy2d` the high bits are reflected away).
- `svg` and `diag` take raw CLI `-z/-x/-y` (`main.rs` declares `z: u8`,
  `x: u32`, `y: u32` with no range parser). `xy_to_tile_id` computes
  `1u64 << z`, which overflows for `z >= 64`. For `z = 32`,
  `n as u32` truncates to 0. A typo in an operator command produces garbage
  or a panic, never a clean refusal.
- `tile_id_to_zxy` loops up to z31. `sort::zoom_from_tile_id` caps at z14
  and returns 14 for any id beyond z14 without saying so.

What should change: add `TileCoord { z: Zoom, x, y }`, whose constructor
validates the range, and a newtype `TileId(u64)`. Then
`TileCoord <-> TileId` should be the only pair of conversions. Add
`SortKey { tile: TileId, layer: Layer, rank: u8 }`, with packing as an
implementation detail.

### 1.5 Mercator `Point` does not say whether it is wrapped

`Point` is documented as "Mercator [0,1] space". But
`unwrap_antimeridian_path` deliberately produces x outside [0,1],
`antimeridian_shifts_for_bbox` adds plus or minus 1.0, and `wrap_unit_x`
folds back. The quantizer is documented as "unclamped, antimeridian-safe",
while the ocean code clamps to `[0, scale]`. Two of the decisions in
section 2 hinge on which variant is in hand. An `UnwrappedMerc` versus
`Merc` distinction, or at minimum a debug assertion at the `project_e7`
boundary, would make the antimeridian flow readable.

### 1.6 Tolerance and area units are bare, and some names mislabel the unit

- `OCEAN_DP_TOL_PX = 16` and `OSM_DP_TOL_PX = 16` are named PX but are in
  extent units (16 units = 1 rendered pixel).
- `OCEAN_VW_AREA_2X = 256` is a doubled area, so the effective VW threshold
  is 128 units squared. `ocean_min_area` returns a literal `256` of true
  area, and `geometry::MIN_POLY_AREA` (also 256, computed as `PX*PX`) is a
  third spelling of the min-area constant. The VW and min-area thresholds
  sit next to each other in `PyramidParams` with a factor of 2 between them
  that only the `_2X` suffix discloses.
- Float-side tolerances (`simplify_tolerance`,
  `merc_bbox_is_subpixel`) express "1 pixel" as `1/(256*2^z)` in Merc
  units. The integer side expresses the same "1 pixel" as 16. These are two
  spellings of one fact (section 2.6).

What should change: add `ExtentUnits(i64)`, `Area2x(i128)` and
`Area(u64)`, plus one `const RENDER_PX: ExtentUnits = 16`, all defined in
one module.

### 1.7 Header bounds and center are untyped tuples

`PmtilesConfig.bounds: (f64, f64, f64, f64)` is ordered
`(min_lon, min_lat, max_lon, max_lat)` only by a doc example.
`center: (f64, f64, u8)` is ordered `(lon, lat, zoom)`. Swapping lat and lon
type-checks. Use `LonLatBbox` and `LonLat`.

### 1.8 Vertex identity lives in float space at two different resolutions

`emit::merc_point_key` rounds Merc x/y times 1e12. `multipolygon::quantize`
rounds times 1e9, which is about 4 cm at the equator. Ring assembly
(`multipolygon.rs`) therefore joins endpoints the pin machinery
(`relation_shared_vertex_keys`, `fill_pin_keys_from_mask`) treats as
distinct. Each of them is trying to recover node identity that was exact
in S1, as an `(i32, i32)` e7 pair, before projection. The right key is the
e7 pair, or the node id, carried alongside the Merc point. That would be
exact, collision-free, and need no tuning constant.

### 1.9 Failures that reach the caller only as prose

`pipeline::PipelineError(String)` carries every coordinate and zoom
refusal: `max_zoom` above 14, `min > max`, the artifact z0-14 contract,
compression level, and a missing shapefile. Elsewhere,
`std::io::Error::other("ocean artifact must cover z0-14")` and the
`.expect("... fits i32")` panics in the pyramid, ocean and int_ocean
conversion helpers are the only reports of an out-of-grid coordinate.
brokkr links this crate, but it cannot branch on, for example, "the
artifact cannot serve this zoom contract" versus "the artifact key is
stale". It can only string-match. A `ContractRefusal` enum (zoom range,
format, compression, key mismatch) belongs next to the zoom and artifact
types proposed above.

---

## 2. Decisions made in more than one place

### 2.1 What is the tile extent and the buffer? Ten or more independent sources

The question has two answers: 4096 units per tile, and 128 units (8
rendered px) of buffer.

| Site | Spelling |
|---|---|
| `geometry/mod.rs` | `EXTENT: f64 = 4096.0`, `BUFFER_FRACTION = 8.0/256.0`, `PX = EXTENT/256` |
| `int_ocean.rs` | `TILE_EXTENT_I32 = 4096`, `TILE_BUFFER_I32 = 128`; also `1 << (maxz + 12)` (12 = log2 4096) in three quantizers |
| `ocean.rs` | `ocean_band_tile`: literal `4096_i64 << shift`, `128_i64 << shift`, `1 << (max_zoom + 12)`; `pass_owns_root_range`: `4096 << shift`, `+ 12`; `OceanPassGrid::for_bounds` and `data_bounds_rect`: `+ 12` |
| `mvt/merge.rs` | `BUFFER_UNITS` derived as `EXTENT * BUFFER_FRACTION` (a const-assert that it is whole) |
| `mvt/mod.rs` | literal `4096` written as the layer extent field |
| `mlt.rs` | literal `4096` passed to `TileLayer::builder` |
| `verify.rs` | its own `MVT_EXTENT = 4096` and limits derived from it; `is_full_tile_rect` uses `geometry::EXTENT` |
| `svg.rs` | its own `EXTENT: f64 = 4096.0`; it never reads the layer's declared extent |
| `tile_detail.rs` | defaults `extent = 4096` and does read the declared value |

They agree today. The float buffer (`EXTENT * BUFFER_FRACTION`) and the
integer buffer (`TILE_BUFFER_I32`) are separate definitions with no
compile-time link: `merge.rs` checks that the float form is whole, but not
that it equals 128. Line clipping uses the float form and polygon clipping
uses the integer form, and the line merger's `CLIP_BOUNDARY_*` assumes
every line was cut at the float buffer. If one is changed and not the
other, line and polygon buffers diverge without any error. Ocean's literal
`4096` and `128` would also go stale, and band ownership would stop
matching the pyramid's cell rects. That mismatch would show up as
artifact seams that no gate is calibrated for.

Owner: one `tile_grid` module that defines
`EXTENT_LOG2 = 12`, `EXTENT = 1 << 12`, `BUFFER_UNITS = 128` and
`BUFFER_FRACTION = BUFFER_UNITS / EXTENT`. Everything else should derive
from it, including the MVT and MLT extent fields and the decoders' default.

### 2.2 Which tiles does a base-space rect touch, and what is its buffered cell rect? Four copies

| Site | Function |
|---|---|
| `pyramid.rs` | `tile_range_for_base_rect` + `base_tile_index` + `cell_tile_size_base` + `buffered_cell_rect_base` + `buffered_range_rect_base` |
| `emit.rs` | `tile_count_for_base_rect` + `base_tile_index`, a verbatim copy used for the polygon fanout cap |
| `ocean.rs` | `pass_owns_root_range` (inline `index` closure + `cell`), `ocean_band_tile` (inline buffered rect, clamped to the world) |
| `int_ocean.rs` | `tile_range_for_rect`, `tile_index`, `tile_count_for_rect`, `rect_intersection`, all `#[allow(dead_code)]` and zoom-space only |

`pass_owns_root_range` says outright that it "deliberately follows
pyramid::root_fragments". That is a copy kept correct by a comment. It
already differs in method:

- Ocean snaps the raw float bbox (`round`, clamped to `[0, scale]`).
- The pyramid takes the bbox of the normalized quantized shape (unclamped,
  possibly smaller after normalize).

The ocean answer is a conservative superset, so today it errs toward
"owned". But any change to root selection in the pyramid (for example,
buffering the root range) silently breaks the "follows" claim.

The fanout-cap copy in `emit.rs` decides "how many tiles would this
polygon emit at z" independently of the pyramid that then emits. A drift
there produces a fanout counter that is wrong and never noticed.

Owner: the `Grid` value from 1.1, with `grid.tiles_touching(rect, z)`,
`grid.cell_rect(cell)` and `grid.buffered_cell_rect(cell)`. Ocean's band
test should call the pyramid's own root selection rather than re-derive
it. Delete the dead int_ocean copies.

### 2.3 Mercator to tile-local pixels, float path: two copies, plus a third rounding path for polygons

- `projection::merc_to_tile_px` (points) and `tiles::to_tile_coords_into`
  (lines) each compute `round((p * 2^z - t) * EXTENT)`.
- Polygons instead go `round(p * 2^(14+12))` and then `shift_round` by
  `14 - z`, minus `tile_origin`.

Double rounding means a shared OSM node can land one unit apart in a
polygon and in a line or point at z < 14 (the unit test
`rescale_shape_s_gt_zero_within_one_of_direct_quantization` asserts exactly
this ±1). Examples are a landuse edge against a street, a boundary line
against a boundary polygon, and a POI centroid against its building. This
is a real, already-present disagreement between the two halves of the
crate about where a given coordinate goes. It is benign at 1/16 px, but it
is the reason "lines and polygons share vertices" cannot be stated as an
invariant.

Owner: quantize every geometry kind once into base space (S5) and derive
every per-zoom tile coordinate by the same `shift_round`. Lines and points
would then go through the same integer pipeline as polygons. That is also
the structural move in 3.2.

### 2.4 Which ocean passes exist, and on which grid does each pass's ownership live? Seven sites, two already disagree

The question has two parts: is z7/z8 a pass boundary, and which `maxz`
grid does a pass use?

| Site | Answer |
|---|---|
| `main.rs` `resolve_ocean_inputs` | accepts only `z0-z14` or `z0-z7` + `z8-z14` |
| `pipeline/mod.rs` run loop | always two passes when `max_zoom >= 8`: low (simplified or full, `max_zoom.min(7)`), high (8..max) |
| `pipeline/mod.rs` `ocean_pass_max_zooms` | two grids only if a simplified shapefile is named; otherwise one grid at `max_zoom` |
| `pipeline/mod.rs` `ocean_build` | literal `0, 7` and `8, 14` passes |
| `assemble.rs` `write_provenance` | "Mirrors ocean_pass_max_zooms" (`simplified && min_zoom <= 7`) |
| `ocean.rs` `selected_pass_grid` | `grids.len() == 2 && z <= 7` |
| `ocean.rs` `LOW_ZOOM_UNION_MAX_ZOOM = 7` | the union fires on `max_zoom <= 7` |

The disagreement: with a `z0-z14` single shapefile plus an artifact, the
run loop executes two passes. The low pass quantizes at `maxz = 7`. But
`ocean_pass_max_zooms` returns only `[14]`, so `grids().first()` and
`.last()` are the same z14 grid. The low pass's `pass_owns_root_range` and
`ocean_band_tile` then snap at 2^26 resolution while its pyramid works at
2^19. That directly contradicts `pass_owns_root_range`'s own "deliberately
follows pyramid::root_fragments" claim. Producer and consumer still agree
with each other (both use `selected_pass_grid` on the same list), so it is
probably benign. But the comment on `ocean_pass_max_zooms` ("a simplified
shapefile only serves z0-7") and the `main.rs` comment ("a single z0-z14
entry ... serves every zoom") both describe a pass structure the run loop
abandoned when the low-zoom union landed. Production always names the
simplified shapefile, so this arm is never exercised.

`selected_pass_grid` keys on `grids.len() == 2` as a proxy for "a split
exists", which is a decision inferred from a collection's size.

Owner: an `OceanPlan` value built once from the resolved inputs and zoom
range. It would be a `Vec<OceanPass { source, zooms: ZoomRange, grid: Grid }>`
from which the run loop, `ocean_build`, artifact grids, provenance's
`low_zoom_source` and `selected_pass_grid` all read. `ocean_build` would
build the world plan through the same constructor.

### 2.5 Tile id to zoom and zoom bounds: five sites

The base formula `(4^z - 1) / 3` is spelled in:

- `sort::TILE_ID_BASES` (and `zoom_from_tile_id`, capped at 14)
- `pmtiles_writer::xy_to_tile_id` (inline)
- `pmtiles_writer::tile_id_to_zxy` (inline, loops to 31)
- `assemble.rs` `artifact_run_copy_items` `zoom_end` (inline, plus `z == 14`)
- `ocean.rs` `OceanTiles::open`, `z14_end = ((1 << 30) - 1) / 3 + 1`

`zoom_from_tile_id` and `tile_id_to_zxy` give different answers for any id
at or beyond `base(15)`: the first returns 14 without any signal, the
second returns z15+. Owner: `TileId::zoom()`, `TileId::zoom_start(z)` and
`TileId::zoom_end(z)` in the tile-id module.

### 2.6 What is "one rendered pixel" at zoom z? Float and integer spellings

- Float: `simplify_tolerance` (`1/(256*2^z)`), `merc_bbox_is_subpixel`
  (`1/(256*2^z)`), `line_is_subpixel` (`MIN_LINE_EXTENT_SQ = PX*PX` in
  tile-local units).
- Integer: `*_DP_TOL_PX = 16`, `MIN_POLY_AREA`, ocean's literal `256`,
  `OCEAN_VW_AREA_2X = 256`.

They agree today. `polygon_z_start` (`emit.rs`) decides the first emitted
zoom of an integer-pipeline polygon using the float subpixel test, so a
single decision about one polygon is made in two spaces.

### 2.7 Is this the last zoom (no simplification, no size filter)?

This is decided at `emit.rs` (`polygon_min_area`, `polygon_dp_tol` via
`OSM_POLYGON_MAX_Z`, plus `z >= 14` and `z < 14` in the line path) and in
`simplify.rs` (`z < 14`, twice). It is never decided from
`config.max_zoom`. See 1.3. Owner: `ZoomRange::is_last(z)`, or the policy
object proposed in 3.3.

### 2.8 Does a line or polygon get a copy across the antimeridian?

`antimeridian_shifts_for_bbox` is shared, but the
shift-and-re-emit loop is open-coded four times: `phase12.rs` twice and
`relations.rs` twice. Each copy allocates the shifted `Vec<Point>` and
calls the emitter. Each copy also uses the unbuffered bbox (`min_x < 0`
or `max_x > 1`), so a feature ending just inside x = 1 contributes nothing
to the buffer of tile x = 0. The same "buffer carries only features that
reach the tile proper" policy is applied by `for_each_tile_in_bbox` (line
and point tiles from the unbuffered bbox) and by the pyramid's root
selection (unbuffered shape bbox). These three agree today, but the
policy is written nowhere. If one site starts enumerating tiles from the
buffered bbox, lines and polygons will disagree about what sits in the
buffer. Owner: one `emit_wrapped(feature, emitter)` helper, plus a single
documented sentence about buffer population in a reference document.

### 2.9 MVT command-stream decoding (the ClosePath cursor rule): eight production copies

The decoders that turn commands back into tile-local coordinates are:

- `geometry::mvt_decode::decode_mvt_polygon`
- `main.rs` `diag_decode_polygon`, a verbatim copy of the above, including
  its own `unzigzag_diag`
- `svg.rs` `decode_geometry_commands`
- `verify.rs` `validate_geometry_commands`
- `tile_detail.rs` (points, lines and polygons)
- `mlt.rs` `parse_points` / `parse_paths`
- `mvt/merge.rs` (two walkers)

plus four or more test-only decoders. Each one independently re-decides
the R23 rule that ClosePath does not move the cursor. All the production
copies seen get it right today. But this is exactly the decision that hid
for three months, and it now has eight owners. Owner: `tile_detail`
(already the out-of-crate decode surface) or a single command iterator in
`mvt/` that yields `MoveTo` / `LineTo` / `Close` with absolute
coordinates. Every other site should consume that iterator.

---

## 3. Structure

### 3.1 There is no `coords` module, so geometry spaces are spread across five homes

- Projection is in `geometry/projection.rs`.
- Tile enumeration is in `geometry/tiles.rs`, float only.
- Integer grid arithmetic is split between `int_ocean.rs` (constants,
  quantize, `tile_origin`, `buffered_tile_rect`), `pyramid.rs` (base-space
  rects) and `ocean.rs` (band rects).
- The Hilbert tile id lives in `pmtiles_writer.rs`, and its zoom inverse
  lives in `sort.rs`.
- The vertex keys live in `emit.rs` and `multipolygon.rs`.

The dependency edges point the wrong way. `emit`, `ocean`, `svg`, `verify`,
`main` and `inspect` all import `pmtiles_writer` just to name a tile, and
`ocean` imports `sort` to learn a zoom.

What should change: create `src/coords/` (or `src/grid/`) holding
`Zoom`/`ZoomRange`, `TileCoord`/`TileId`, the `Grid` base/zoom/tile-local
types and conversions, the extent and buffer constants, and the
projection. `pmtiles_writer` and `sort` would depend on it, not the other
way round. This is the module that would make 2.1, 2.2, 2.5 and half of
2.4 single-owner.

### 3.2 `int_ocean.rs` is not ocean-specific, and its name is misleading

It is the integer geometry engine for all polygons (AGENTS.md says so). It
holds the quantizer, the rescale, DP and VW simplification, normalize and
intersect wrappers, point-in-contour, grid helpers, MVT tile encoding
(`encode_tile_shape`, `emit_full_tile`), the dead-code tile-range copies,
and the ocean-artifact-policy constants. That is at least five jobs.

Split it into:

- the grid (move to `coords`),
- `int_simplify` (DP, VW and their scratch),
- the boolean-op facade over `overlay`,
- the MVT polygon encoding (move next to `mvt::encode_polygon_ranges`).

Rename what remains. `OCEAN_DP_TOL_PX`, `OCEAN_VW_AREA_2X` and the
artifact-policy comment belong in `ocean.rs` next to
`OCEAN_POLICY_VERSION`.

The bolder version, recommended: move lines and points onto the same
quantize-early integer pipeline. Then there is exactly one
Merc-to-integer conversion (at quantize), one per-zoom conversion
(shift-round), and one tile-local conversion. The float S-H clipper, the
float `to_tile_coords_into`, the float DP with Merc tolerances and
`merc_to_tile_px` all go away, and 2.3 and 2.6 disappear by construction.
This is a large rewrite of `emit_line_feature` and the line merger's
assumptions. It is also the only way to make "a shared node lands on the
same tile-local coordinate in every layer" true, and it removes the second
geometry pipeline that every geometry change currently has to reason
about.

### 3.3 Zoom policy is scattered through the emitters

"Which zooms, what tolerance, what min area, is the size filter skipped,
is this the last zoom" is answered inside `emit.rs` closures
(`polygon_dp_tol`, `polygon_min_area`, `skip_size_filter`,
`skip_bbox_check`), in `simplify.rs`, and in `ocean.rs` (`ocean_dp_tol`,
`ocean_min_area`). Layer-specific exceptions (`StreetPolygons`,
`Bridges`, `Boundaries`, `Streets`) are spelled as `m.layer == ...` in the
middle of geometry code. This would be cleaner as a per-layer
`GeneralizationPolicy` in `shortbread/` handing the emitters
`(ZoomRange, tol(z), min_area(z), size_filter(z))`. The geometry engine
should not know layer names.

### 3.4 `pipeline/mod.rs` owns ocean pass planning, artifact gating and checkpoint serialization

`select_ocean_mode`, `ocean_pass_max_zooms`, the two-pass run loop and
`ocean_build` live in the orchestrator, while the half that consumes them
(`OceanTiles`, `selected_pass_grid`) lives in `ocean.rs`. That split is
why 2.4 has seven sites. Move all of it into `ocean::plan`.

### 3.5 Tests mirror the duplication

- `ocean.rs` has unit tests for `ocean_band_tile` and `selected_pass_grid`
  that hard-code `max_zoom: 14` and z7/z8. They exercise the copy, not the
  agreement with the pyramid.
- There is no test asserting `EXTENT * BUFFER_FRACTION == TILE_BUFFER_I32`.
- `pipeline_tests.rs` and `mvt/tests.rs` carry their own command decoders
  (`decode_commands_to_abs_coords`, `decode_commands_to_abs`,
  `decode_segments`). So the encoder is tested against decoders written
  by the same hand, which is the symmetric-convention trap the earcut
  oracle exists for. Pointing them at the single decoder from 2.9 would at
  least make every in-crate test use one convention, and that convention is
  the one brokkr's gate uses.

### 3.6 There is no reference document for coordinate spaces

Nothing in `reference/` names S1 to S13 or the conversions between them.
The `int_ocean` doc comments, `pyramid` comments and `merge.rs` comments
each describe a piece. A short `reference/coordinates.md` (the table at
the top of this note, plus the buffer-population policy from 2.8 and the
rounding rule from 2.3) would give future comments something durable to
cite.

---

## Lateral findings (bugs, smells, surprises)

1. **The CLI accepts invalid tile coordinates.** `svg` and `diag` accept
   any `u8` z and any `u32` x/y. `xy_to_tile_id` overflows `1u64 << z` for
   `z >= 64` and silently truncates `n as u32` at `z = 32`. Out-of-range
   x/y produce a valid-looking but wrong tile id, so the command renders
   an empty or wrong tile instead of refusing. See 1.4.
2. **The `data_bounds` comment does not match the code.**
   `phase12.rs` says "~1 degree buffer (in Mercator space, roughly 1/360
   ≈ 0.003)" but `buf = 0.01`, which is about 3.6 degrees in x and varies
   in y. Either the comment or the constant is wrong. Because ocean band
   ownership depends on this bbox, it matters for artifact interplay.
   Per the documentation rule, rewording it to not hard-code a figure is
   safer than "fixing" it to 3.6.
3. **The PMTiles header bounds and center are always the world.**
   `assemble.rs` writes `(-180, -85.05, 180, 85.05)` and center
   `(0, 0, 2)` for every extract, even though `data_bounds` is known.
   Clients that fit to bounds open a denmark archive at the whole world.
   There is also no production Merc-to-lon/lat inverse
   (`merc_y_to_lat` is `#[cfg(test)]`). `85.05` is yet another spelling of
   `MAX_LATITUDE` (85.051129).
4. **`f64_to_e7` truncates instead of rounding** (`pmtiles_writer.rs`).
   `85.05 * 1e7` can truncate to `850499999`. This is harmless for display,
   but it is a non-symmetric conversion next to `inspect.rs`'s `/ 1e7`.
5. **`main.rs` `diag_decode_polygon` is a verbatim copy** of the public
   `geometry::decode_mvt_polygon`, including the zigzag helper. Delete it.
6. **Dead code that can drift:** `int_ocean::tile_range_for_rect`,
   `tile_index`, `tile_count_for_rect` and `rect_intersection` are all
   `#[allow(dead_code)]` duplicates of pyramid and emit logic in a
   different space. Dead copies of a live decision are where the next
   drift starts.
7. **`svg.rs` and `verify.rs` assume extent 4096** without reading the
   layer's extent field. That is correct for elivagar output, but wrong
   for any foreign MVT (and `tile_detail` was written to be tolerant of
   foreign MVT).
8. **The two vertex-identity resolutions (1e9 and 1e12) conflict**
   (1.8). Ring assembly can treat two nodes about 4 cm apart as one
   endpoint, while pinning treats them as two. The intended identity is
   the OSM node, which the e7 pair carries exactly.
9. **`selected_pass_grid` infers the pass split from
   `grids.len() == 2`** (2.4). It is fragile, and stale against the
   current run loop.
10. **`OCEAN_VW_AREA_2X` versus `ocean_min_area`:** the factor of 2 between
    the VW kill threshold (128 true units squared) and the min-area drop
    (256) may be intended. Nothing records whether it is.
