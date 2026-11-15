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

## STR-002 - Make `run()` a phase state machine with typed hand-offs

`run()` is a roughly 620-line function doing resume and contract validation,
phase sequencing with `Option` juggling (`ocean_elapsed`, `phase12_rss`,
nested-option writer states, the artifact opened in one of two places
depending on the skip), stderr reporting, and about 300 lines of sidecar
counter emission that knows the internals of `Phase12Stats`, `FanoutStats`,
`DedupStats` and `TileSizeDiagnostics`. Assemble returns a 6-tuple
`(u64, u64, u64, usize, DedupStats, TileSizeDiagnostics)` with three adjacent
`u64`s (features, tiles, unique tiles) and the writer thread another.

Proposed (the crate-private `RunPlan` already performs every refusal before
work): `enum Start { Full, ResumeOcean(ChunkSet), ResumeSort(ChunkSet),
ResumeAssemble(ChunkSet) }`, each phase producing the next typed artifact
(`ChunkSet` -> `SortReader` -> `AssembleReport`); the artifact opens in
exactly one place; phase12 returns a `ChunkSet { bounds, chunk_count, .. }`
that a resume loads as the same type. Counter emission moves to
`impl Phase12Stats { fn emit }` and friends, so a struct's owner owns its
counter names.

Reported by the pipeline hunt.

## STR-008 - Fix the dependency edges that point the wrong way

- `emit` is `pub(crate) mod` with `pub(super)` items, but it is the
  feature-to-sort-record layer shared by way and relation paths, not
  orchestration - it belongs top-level beside `geometry`.
- The geometry engine depends on the MVT encoder (`PyramidSink` receives
  encoded command streams; see STR-013) and on the pipeline's telemetry
  (`pyramid.rs` bumps `crate::debug::RING_CAP` atomics); return per-call
  stats and let the producer emit counters.
- `mlt -> mvt`: MLT consumes the tile model through
  `cfg_attr(not(feature = "mlt"), allow(dead_code))` accessors in `mvt`.
- `shortbread -> pois -> shortbread`: `pois.rs` reaches back into
  `crate::shortbread::{attr_*, name_attrs, ..}`.

Reported by the pipeline, polygon-engine, archive-surface, coordinate-spaces,
encoding and Shortbread hunts.

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

A grid-zoom newtype on its own was refused (reasons at `PyramidZooms`): the
`fits i32` arithmetic rests on three premises - grid at most z14, in-range
cell indices, and input coordinates - and a zoom type states only the first.
Coordinate representability is now checked at quantization against the
overlay engine's `2^30` interval (`int_ocean::Unrepresentable`), so the
`Grid` here is the place the remaining two premises would become types.

Reported by the polygon-engine, coordinate-spaces and ocean hunts.

## STR-012 - Make ring role a type

Role is "index 0 of a `Shape` is the outer, the rest are holes" plus "outer
has positive shoelace area"; every in-place compaction must preserve the
slot-0 invariant by hand, and the sites already use different rules. Proposed:
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
one per-zoom shift-round and one tile-local conversion. The float Cohen-Sutherland line
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

## STR-046 - Closed-line simplification depends on the source start vertex

`simplify_closed` now runs on the least rotation of the (y, x, required)
cycle and no longer forces the start, so closed lines are start-independent
on injected-pin input. Two residues remain start-dependent:

- Raw input (block-local pins) requires the closed way's source start on the
  line path (`line_start_required` in `FeatureGeometry::Way`), because a
  junction with a way in another block is unpinned. Closing it needs
  cross-block pins on raw input, i.e. the global prepass removed for RSS and
  serial wall cost.
- Rings crossing the antimeridian: `unwrap_antimeridian_path` walks from the
  source start, so a different start can give a different (not merely
  translated) lift. Closing it means choosing the canonical start on wrapped
  coordinates before unwrapping, with a defined policy for winding cycles and
  exact half-world edges.

---

# Profile and layers

## STR-021 - A declarative layer table as the profile's single owner

The profile is a hand translation of a declarative document
(`tests/fixtures/shortbread.yml`, checked in and read by nothing) into about
40 imperative matcher functions, and per-layer facts leak out of
`shortbread/` into emit.rs (min-area and subpixel exemptions), assemble (merge
policy), `pipeline::shortbread_tileset` (vector_layers) and ocean.rs (ocean
policy). The key schema (`attr_keys!`), the kind families and the area rules
(`Hits::finish`) already live in `shortbread/`; the layer table would also
own which keys each layer may carry.

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
benefit. (`vector_layers` already carries `fields` and verify already checks
observed attributes against the per-layer declarations in `define_layers!`.)

Reported by the Shortbread and layer-identity hunts.

---

# Encoding and the sort record

## STR-024 - Separate the tile model from MVT, and the sort-record codec from the tile model

`LayerBuilder`, `Feature`, `Value`, interning tables, merge passes and scratch
types live in `mvt/`, and MLT consumes them through accessors that exist only
for it. Proposed: a format-independent `tile_model/` (layers, features,
attributes, interning, post-processing) with `mvt` and `mlt` as sibling
sinks. `wire_format::add_feature_to_layer` decodes a sort record directly into
a `LayerBuilder`, interning as it goes, so the codec depends on the tile
model; proposed: `wire_format` yields a borrowed
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

---

# Storage, sort and resources

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

# Tests and documentation

## STR-035 - Test layouts mirror history and module privacy, not the design

- `pipeline_tests.rs` (about 3741 lines) is one `#[path]` file reaching
  private items through a block of `cfg(test)` re-exports in `mod.rs`; it
  mixes injected-PBF fixture builders (hand-written protobuf splicing,
  `splice_way_members`, `rebuild_blobheader_datasize`), emitter decode tests,
  assemble metadata tests, checkpoint tests and boundary-label tests, and builds `TilegenConfig` struct literals seven or
  eight times repeating every budget field as 0 (which is why the zero
  sentinel survives). Proposed: per-module tests after the splits, the PBF
  fixture builders as a shared `test_support` synthetic-PBF writer, and a
  `TilegenConfig::for_test()` / resolved-resources builder.
- `geometry_tests.rs` (loaded via `#[path]`) covers the float side; the integer engine's tests sit
  inline in int_ocean.rs under `tests` and `landing1_tests` (a landing name);
  `bounds_boolean_skipped_when_bbox_contained` asserts an ocean.rs property
  (`push_quantized_pieces` in ocean/emit.rs) from inside int_ocean; `landing_b_*` names encode project history. pyramid
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
- Several band-ownership tests in `ocean/band.rs` still hand-build a z14
  `OceanPassGrid` rather than going through `OceanBand`.
- `src/sort/merge.rs` tests mix format round-trips and resume semantics;
  resume tests belong beside `SortWriter::resume` and `SortReader::from_dir`
  (there is no persisted chunk-store manifest to host them).
- `archive/writer_tests.rs` hand-decodes directory varints and holds the
  directory codec tests that belong in `archive/directory.rs`, plus
  `test_varint_*` testing protohoggr, not this crate; verify tests assert on
  error-string prefixes.
- No test iterates `Layer::ALL` against a descriptor invariant.

Reported by every hunt.
