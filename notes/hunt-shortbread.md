# Hunt: the Shortbread profile

Scope: `src/shortbread/` (mod, boundaries, land, streets, transport, water,
paint_order), `src/pois.rs`, `src/shortbread_tests.rs`, `tests/fixtures/`.
Followed out of scope where the profile's decisions leak: `src/verify.rs`
(paint-order validation), `src/pipeline/phase12.rs` (`resolve_boundary_match`,
the `possible_feature` probe), `src/pipeline/emit.rs`
(`enrich_polygon_matches`, size/bbox filter exemptions),
`src/pipeline/boundary_prescan.rs`, `src/pmtiles_writer.rs`
(`build_metadata`).

Headline: the profile is a hand-translation of a declarative document
(`tests/fixtures/shortbread.yml`, which is checked in and read by nothing) into
~40 imperative matcher functions. Every per-layer fact (zoom floor, kinds,
rank, which tag wins) is spread across a matcher, a free-standing table
somewhere else, and sometimes a pipeline stage. The recommended end state is
one typed `LayerSpec`/kind-enum table per layer that the matchers, the paint
ranks, the metadata writer, and the verifier all read. Details below.

---

## Bugs and surprises found on the way

1. **public_transport drops valid matches when an unrelated value is present
   on an earlier key.** `transport.rs::public_transport_match` returns `None`
   as soon as a key is *present* with a non-listed value: `amenity=shelter +
   highway=bus_stop` yields nothing, as do `aeroway=gate + railway=halt` and
   `amenity=<anything> + railway=station`. The Shortbread definition
   (`shortbread.yml`, `include_when` for public_transport) is an OR over
   specific values, so these should match. Shelter-plus-stop on one node is a
   real OSM tagging pattern. The same early-return-on-key-presence shape exists
   in `streets.rs::street_match` (`highway=<unknown> + railway=rail` -> no
   street), `street_polygon_match` (`area:aeroway=<other> + highway=pedestrian`)
   and `land.rs::sites_kind` (amenity last, so harmless today). `land_match`
   and `water_polygon_match` deliberately fall through instead (tests B7). So
   the question "does an unlisted value on key A block key B?" gets a
   different answer in each matcher. That is a scattered decision (see D5). It
   also leaves bugs unseen, because the spec YAML has no multi-key cases for
   these layers.

2. **Advertised layer minzoom disagrees with what is emitted.**
   `Layer::min_zoom()` (only consumer: `pmtiles_writer::build_metadata`, the
   TileJSON `vector_layers`) says `water_polygons_labels` = 4 and
   `water_lines_labels` = 9. The matchers emit 14 and 12/14. The claim is
   valid only as a lower bound and is not the truth. Nothing ties the two. All
   other layers happen to agree today (checked one by one).

3. **`match_boundaries_line` is dead on the way path.** `phase12.rs::
   resolve_boundary_match` runs on every way's matches and *overwrites* the
   Boundaries match's `min_zoom` and `attrs`, re-deriving `maritime` with
   copied logic. The profile matcher's output survives only as the
   `possible_feature` truthiness probe. Two implementations of the boundary
   attribute set exist, and the visible one lives in the pipeline.

4. **Supported admin levels are decided twice, with different parsers, and a
   panic couples them.** `boundaries.rs::boundary_min_zoom` (2 -> z0, 4 -> z7,
   reached via `parse_i64`, which strips commas) and
   `boundary_prescan.rs::fold_relation` (`parse::<u8>` then
   `matches!(level, 2 | 4)`). `resolve_boundary_match` then
   `expect("boundary metadata only contains supported levels")`. If someone
   adds admin_level 6 to the prescan without adding it to the profile, the
   program panics mid-phase12. The parsers also disagree on input like `"0,2"`
   (parse_i64 gives 2, u8 parse fails) and `" 2"`.

5. **Two `normalize_numeric_token` and two metres parsers.**
   `land.rs::parse_building_measurement_meters` and
   `pois.rs::parse_ele_meters` are character-identical in their unit-stripping
   prelude and both carry a private copy of `normalize_numeric_token`. This is
   a duplicated *decision* ("how is an OSM length interpreted"), not just
   duplicated code: a fix to one (for example `"12 m"` with an interior
   space, or `"5'6\""`) will not reach the other. `parse_ele_meters` also does
   `rounded.to_string().parse::<i64>()` to convert, which silently drops
   values beyond i64 range and is an odd round-trip where a checked cast
   would do.

6. **`UNKNOWN_STREET_CLASS = 12` collides with `unclassified`'s rank.** An
   unknown kind sorts as unclassified. The verifier refuses unknown kinds, so
   the archive cannot carry one. Still, the sentinel is a real rank value, not
   an out-of-band marker. `Option<u8>` or an exhaustive enum would remove it.

7. **`shortbread.yml` is orphaned.** It is the upstream declarative profile
   and nothing reads it. The test harness reads only `shortbread.spec.yml`.
   Either load it in a test (see S4) or delete it.

8. **The spec harness is lenient in ways that hide drift**
   (`shortbread_tests.rs::shortbread_spec_yaml`):
   - Polygon inputs are dispatched only as `ClosedWay`. `match_multipolygon`
     is never exercised by the spec, although it is a separately maintained
     list of matcher calls (D3).
   - Every expectation carrying `at_zoom` skips the extra-attrs check, and
     per-attr `min_zoom` (the third tuple slot) is never checked. The
     zoom-gated attribute policy (`link`/`tunnel`/`bridge` at 11,
     `oneway`/`bicycle`/`horse` at 14) therefore has no spec coverage.
   - `attr_values_match` accepts Int vs Float within 1.0 and Int vs Str by
     formatting. A typed regression (`admin_level` emitted as a string) passes.
   - `matches.iter().find(|m| m.layer.name() == exp_layer)` takes the first
     match per layer. land-polygon plus land-line (cliff) on one closed way
     would hide a wrong second match.
   - `boundary_labels` min_zoom is skipped entirely, because the real answer
     is computed in `emit.rs` (D2).

9. **Hot-path allocation spent on predicates.** `attr_dyn` allocates a
   `String` per dynamic attr. `pois::would_match_poi` (called for every
   address candidate) builds the full POI attr vector and throws it away.
   `phase12`'s `possible_feature` runs `match_element` up to twice per way
   (ClosedWay, then OpenWay), building and discarding every attr, including
   `name*` copies. A separate `classify` step that returns kind and zoom
   without attrs (S1) makes these cheap and leaves one code path.

10. **`LayerMatch.max_zoom` is always 14.** All ~25 construction sites write
    the literal 14. The field carries no information, yet pipeline code
    min()s it against `config.max_zoom`. It should be removed, or set from one
    place if a layer ever really caps below 14.

---

## 1. Axes that should be types

**T1. `kind` is a `&'static str` everywhere.** Land, street, water, site,
pier, aerialway, place, public_transport and POI categories are all bare
strings. The consequences in code:
- `paint_order.rs` restates the land kinds three times (`is_known_land_kind`,
  `land_paint_rank`, the test lists) and the street kinds three times. The
  doc comment on `every_land_kind_has_rank` says the list "keeps the rank
  table and the matcher in lockstep". It does not: the list is a fourth copy
  checked against the third, and the matcher (`land_match_*`) is never
  consulted. A kind added to `land_match_landuse` without a rank falls into
  `_ => 0` silently, and `verify` then rejects the archive as an "unknown land
  kind". The failure therefore shows up at verify time, not at compile time.
- `street_paint_rank(kind, ...)` is fed both stripped (`streets`) and raw
  values. Only the call sites decide which, and `street_labels` keeps
  `_link` in its kind on purpose.
Proposal: `enum LandKind`, `enum StreetKind` (with `link: bool` separate,
`StreetKind::from_osm("motorway_link") -> (Motorway, link=true)`), and so on
for each kind-bearing layer, each with `as_str()` for the wire and a `rank()`
method. Paint rank then becomes a total `match` the compiler checks for
exhaustiveness. `is_known_*` becomes `LandKind::from_str(..).is_some()`,
used by `verify`. The four lockstep lists collapse to one `enum` declaration.

**T2. `Attr = (&'static str, AttrValue, u8)`.** This is a bare tuple with two
swappable small-integer-ish slots (the `u8` attr min-zoom against nothing
typed), and keys are free strings. The comment defends the tuple because a
struct "would bloat ~100 construction sites", but those sites already go
through `attr_*` helpers. Concretely:
- The key is stringly typed. `emit.rs::enrich_polygon_matches` reads
  `admin_level` back out of the attr list by string compare and pattern-match
  on `AttrValue::Int`, with `.unwrap_or(0)` as the fallback. If the key were
  renamed, label zooms would silently stop being overridden for admin 2.
  `phase12` tests do the same.
- The attr min-zoom `u8` is not a `Zoom`, and nothing enforces that it is at
  least the layer min zoom or at most 14.
Proposal: `struct Attr { key: AttrKey, value: AttrValue, from_zoom: Zoom }`.
Better still, give label/boundary matches a typed payload
(`admin_level: AdminLevel`) that is lowered to attrs only at encode time.
Then enrich reads a field, not a string.

**T3. Zooms are `u8`.** `min_zoom`, `max_zoom`, the attr zoom, and
`Layer::min_zoom` are all plain `u8`, as is `paint_rank`. A `Zoom` newtype
(0..=14, constructed checked) plus a `PaintRank` newtype would stop the
`(min_zoom, max_zoom, paint_rank)` triple of adjacent `u8` fields in
`LayerMatch` from being transposed. They are constructed positionally in
`phase12::resolve_boundary_match` and in many tests.

**T4. `AdminLevel`.** Today it is `i64` in the match (via `parse_i64`), `u8`
in `BoundaryMeta`, `u8::try_from(..).expect(..)` in phase12, and `Int` in
attrs. `enum AdminLevel { Country = 2, State = 4 }` with one
`AdminLevel::parse(&str) -> Option<Self>` and a `min_zoom()` method removes
bug 4 by construction. The prescan stores `Option<AdminLevel>`, and the
`expect` disappears because an unsupported level cannot be represented.

**T5. `GeomExpect` conflates the output geometry with the derivation.**
`PolygonCentroid` and `PolygonPointOnSurface` are both "point output from a
polygon". Which layer uses which is decided per call site (`water_polygons_
labels` and `addresses` use Centroid; everything else uses
PointOnSurface). The spec harness accepts either (`acceptable_geom_expects`).
If the choice is deliberate, it is a property of the layer and belongs in the
layer spec. If it is not deliberate, collapse it. The comments do not say
which.

**T6. `OsmGeomType` vs the dispatch lists.** Fine as a type, but see D3.

**T7. `Tags` lookup keys are bare strings.** Low value to type in general.
However, the tag predicates that encode domain meaning (`is_tunnel`,
`is_bridge`, oneway, `maritime`) should be the only sites that name those
keys. See D4.

**Prose-only failures.** `verify::validate_paint_order` returns `String`
errors ("unknown land kind", "paint rank N follows M"). A caller (brokkr) can
only grep. With kind enums, a typed `PaintOrderViolation { layer, feature,
kind: UnknownKind | Inversion{prev, rank} }` would let a caller branch.

---

## 2. Decisions made in more than one place

**D1. "What is layer L's zoom floor?"**
Sites: `Layer::min_zoom()` (mod.rs, feeds TileJSON metadata only), each
matcher's literal `min_zoom` (40-odd literals), `boundaries::boundary_min_zoom`,
`emit.rs::enrich_polygon_matches` (boundary_labels 2/3/4), and
`water_*_labels` hard-coding 14/12. **Already disagree** (bug 2). Owner: a
per-layer spec whose floor is *derived* as the min over the layer's kind
table, so the metadata cannot claim a zoom no rule produces. A `const` test
can assert it if derivation is awkward, but derivation is better.

**D2. "What is boundary_labels' min zoom?"** Three sites: `boundaries.rs`
(default 5), `emit.rs` (area thresholds 2M/700k/100k km2, admin-2 special
case, comment "else stays at 5 (default from match_boundary_labels)"),
`Layer::min_zoom` (2). The spec harness skips it. The area thresholds are
profile policy (Planetiler's) living in the pipeline's geometry module.
Owner: the profile, as a function `boundary_label_min_zoom(level:
AdminLevel, area_km2) -> Zoom` that emit calls with the area it computed.

**D3. "Which matchers apply to which OSM geometry?"** `match_closed_way` and
`match_multipolygon` are two hand-kept lists that must agree on every polygon
and point layer. ClosedWay also repeats the whole `match_open_way` list. They
agree today. Nothing checks it, and the spec only exercises ClosedWay (bug 8).
Owner: each layer spec declares which input geometries it accepts
(`accepts: &[OsmGeomType]`), and dispatch iterates the table. The
closed-way-as-line rule then lives in one place.

**D4. "Is this boundary maritime / disputed?" and "what are the boundary
attrs?"** `boundaries.rs::match_boundaries_line` and
`phase12::resolve_boundary_match` both compute `maritime` with identical
logic and both build the three-attr set. Disputed is decided in three
places with three tag tests: way `disputed=yes` in two of them, relation
`boundary=disputed` in the prescan. **Already divergent in effect**: the
phase12 copy wins, so the profile copy is dead (bug 3). Owner: the profile
exposes `boundary_line(tags, relation_meta: Option<BoundaryMeta>) ->
Option<LayerMatch>`. `BoundaryMeta` moves into the profile, because it is
profile semantics (min admin level wins, disputed parentage). phase12 just
supplies the relation fold.

**D5. "Does an unlisted value on an earlier key block later keys?"**
Answered per matcher. It blocks in public_transport, street_match,
street_polygon_match and sites. It falls through in land, water and POIs.
**Disagrees with the spec** for public_transport (bug 1). Owner: a single
table-walk helper with one documented semantics: first (key, value) row
that matches wins. That is the upstream `include_when` meaning.

**D6. "Which street kinds exist, and at what zoom?"** `highway_zoom`,
`aeroway_zoom` and `railway_zoom` (street lines), `street_label_zoom_highway`
plus the inline railway/aeroway arms in `street_label_zoom` (labels),
`street_label_kind_raw` (re-calls the line zoom functions as a membership
test), `street_polygon_match`, and `paint_order::is_known_street_kind` /
`street_class_rank`. The label zoom and label kind functions **check keys in
different orders** (zoom: highway, railway, aeroway; kind: highway, aeroway,
railway). With `aeroway=runway + railway=rail` on one way, the zoom comes
from railway (10) and the kind from aeroway ("runway"). That is a real if
rare mismatch. Owner: `StreetKind` (T1) carries line zoom, label zoom and
class rank as methods. One classifier returns `(StreetKind, link, source key)`
and both layers read it.

**D7. "At what zoom do street link/tunnel/bridge attrs appear?"**
`streets.rs` writes 11 three times. `verify.rs::validate_paint_order` has
`"streets" if z < 11` to choose the class-only comparison, because those attrs
are absent below 11. Two literals are held in step. If the streets attr floor
moves to 12, verify will report false inversions at z11. Owner: a named
`const STREET_DETAIL_ZOOM: Zoom` in the profile, read by both. Better: verify
asks the profile "which rank function applies at z for layer L".

**D8. "What is the paint rank of a feature?"** It is computed from tags in
the matchers and recomputed from decoded MVT attrs in `verify.rs`. This is a
deliberate re-check, so it is not itself a finding. But verify can only
re-derive it because the inputs (kind, link, tunnel, bridge) survive to the
wire. The layer list `"land" | "streets" | "street_polygons"` that carries
ranks is a third copy of "which layers have deliberate paint order", next to
the `paint_rank: 0` literals in every other matcher. Owner: a
`LayerSpec::paint_order: Option<fn>` field, which verify iterates.

**D9. "Is this element a POI?"** It is decided once (`pois_match`) and reused
by addresses through `would_match_poi`. Good. But the address layer's notion
of "POI" has silently become "POI layer membership". If POIs ever gain a
zoom or geometry restriction, addresses will follow it without anyone
deciding that. A one-line comment in `land.rs` acknowledges the coupling. It
is acceptable as long as it stays one call.

**D10. "How is a length/number tag parsed?"** See bug 5. There are two
copies. `parse_i64` (comma-stripping) is a third numeric policy, used for
population and admin_level, and it disagrees with `normalize_numeric_token`
on `"1,5"` (15 vs 1.5). Owner: one `osm_value` module with `parse_length_m`,
`parse_count` and `parse_int`, each documenting its locale policy.

**D11. "Which layers skip the size/bbox filters?"** `emit.rs` hard-codes
`Layer::Boundaries || Layer::Streets` twice (`skip_size_filter`,
`skip_bbox_check`). This is profile policy (lines that must stay continuous)
expressed in the geometry stage. Owner: a `LayerSpec` flag.

---

## 3. Structure

**S1. Replace imperative matchers with a typed declarative profile.** The
code already is a Shortbread table, written as `match` arms. Recommended
shape:

```
struct LayerSpec {
    layer: Layer,
    accepts: &'static [OsmGeomType],    // D3
    geom: GeomExpect,                   // T5
    classify: fn(&Tags) -> Option<Hit>, // kind + zoom, no attrs (bug 9)
    attrs: fn(&Tags, &Hit, &mut Attrs), // only on real emission
    paint: Option<fn(&Hit) -> PaintRank>,// D8
    keep_small: bool,                   // D11
}
static PROFILE: [LayerSpec; N];
```

The kind enums (T1) hold the per-kind zoom and rank, with simple layers
expressed as `&[(key, value, kind, zoom)]` rows walked by one helper (D5).
`Layer::min_zoom` is derived (D1). `match_element` becomes a loop over
`PROFILE` filtered by `accepts`. The split by domain file (water/land/...)
can stay as organisation of the spec rows, but the dispatch lists in mod.rs
disappear. This is a rewrite of ~1500 lines with no change to output, so it
can be gated byte-for-byte by the denmark corpus check. That makes it a
low-risk large change.

Going further, `build.rs` could generate the tables from `shortbread.yml`.
The gain is that "matches upstream" becomes mechanical. The cost is that
non-spec decisions (paint ranks, the cliff line, numeric parsing policy)
need an overlay. Hand-written Rust tables plus a test that loads
`shortbread.yml` and asserts every `include_when` value appears in the
corresponding kind table would get most of the benefit (S4).

**S2. Boundary logic is split across three modules and two stages.** It sits
in `shortbread/boundaries.rs`, `pipeline/boundary_prescan.rs` and
`pipeline/phase12.rs::resolve_boundary_match`, plus `emit.rs` for label
zooms. The profile owns the rule only nominally. Move `BoundaryMeta`, its
fold, the resolution and the label-zoom policy into `shortbread/boundaries.rs`.
The pipeline should hold only the plumbing (the per-way-id map and the area
computation). `boundaries.rs` also owns `place_labels`, which has nothing to
do with boundaries. Move it to its own `places.rs`.

**S3. `pois.rs` lives at the crate root but is a profile module.** mod.rs even
has a section header pointing at it, and it reaches back into
`crate::shortbread::{attr_*, name_attrs, ...}`, so the dependency runs both
ways (shortbread -> pois -> shortbread). Move it to `shortbread/pois.rs`.
Likewise `paint_order.rs` is a cross-cutting table keyed by strings. With
kind enums it dissolves into `LandKind::rank` / `StreetKind::rank` next to
their kinds, and `verify` imports the enums.

**S4. Test layout.** `shortbread_tests.rs` sits at `src/` root and is pulled
in via `#[path]` from `shortbread/mod.rs`. It is a single 1400-line file
mixing ad hoc cases, the spec-YAML harness, and a dated "2026-03-03 audit"
block (B1-B7). It should follow the design instead: per-layer tests next to
each layer's spec, plus a standalone conformance harness
(`shortbread/conformance_tests.rs`) that (a) runs `shortbread.spec.yml` for
both ClosedWay **and** MultiPolygon on polygon inputs, (b) checks attr zoom
gating via `at_zoom` instead of skipping it, (c) compares values strictly by
type, and (d) loads `shortbread.yml` to assert that every upstream
`include_when` value and `min_zoom` override is represented. Point (d) would
have caught bug 1 and bug 2. The `paint_order.rs` lockstep tests disappear
with the enums.

**S5. `LayerMatch` is a wide, mutable, post-processed record.** It is
mutated after construction by `resolve_boundary_match` (overwrite) and
`enrich_polygon_matches` (push attr, lower zoom). The profile's answer is
therefore provisional, and the final answer is assembled in the pipeline.
With S2's typed payloads, a match could be a `Hit` the pipeline completes by
calling a profile function with geometry facts (area, relation meta). The
profile then makes every decision and the pipeline supplies only measurements.
The `size_of == 408` const assert shows that size matters. Moving attr
construction to emission time (S1) also shrinks what travels through the
dispatch SmallVec.

**S6. `Layer` itself.** The `ALL` array, `name()`, `min_zoom()`, and the
`#[repr(u8)]` discriminants are four parallel listings. `Layer::Ocean` lives
in the profile enum although no matcher produces it. It exists so fanout
caps and sort keys can index it. That is fine, but it means "Shortbread
layer" and "output layer" are the same type. If S1 lands, `ALL` and `name`
come from the `PROFILE` table and `Ocean` is the one row with no
`classify`. A small declarative macro could generate the enum, `ALL`,
`name`, and `from_name` from one list, which removes the risk of a
reordered `ALL` silently shifting array indices used by `fanout_caps` and
the provenance contract.
