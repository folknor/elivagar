# Design reconnaissance: tile encoding and the sort record format

Scope: `src/mvt/` (mod.rs, merge.rs, tests.rs), `src/mlt.rs`, `src/wire_format.rs`.
Followed out of scope where the question led: `src/pipeline/assemble.rs`
(the only consumer of the decode side), `src/pipeline/emit.rs` and
`src/ocean.rs` (the producers), `src/geometry/mvt_decode.rs`,
`src/shortbread/mod.rs` (`Attr`, `AttrValue`).

## Headline

The real intermediate representation of this pipeline is "an MVT geometry
command stream (`Vec<u32>`) plus a per-layer interning table". It is chosen
at emission, survives the sort as raw bytes, is decoded into
`mvt::Feature`, post-processed by decoding it *again* (line merger, MLT),
and re-encoded. Everything downstream of emission therefore speaks MVT
wire encoding as its data model. That is the root of most findings below:
MLT is a second-class re-decoder of MVT commands, the line merger is a
decode-transform-reencode loop, at least eight independent MVT command
decoders exist in the crate, and the per-tile post-processing policy has
no home of its own, so the MVT and MLT paths each answer it separately -
and they already disagree.

The big move I would make: introduce a typed tile-local geometry
(`TileGeom { kind, points: Vec<IPoint>, parts: Vec<Range> }` with
polygon parts tagged outer/hole) as the sort payload and the assembly
model, make "MVT command stream" an encoder *output* only, and give tile
post-processing (merge-same-attrs, line merging, ocean exemption, zoom
gate) a single format-independent owner that both encoders consume.

---

## 1. Axes that should be types

### 1.1 Attribute keys are `&'static str`, resolved by linear search with a runtime panic

`shortbread::Attr = (&'static str, AttrValue, u8)`. `wire_format::key_to_id`
does `KEY_NAMES.iter().position(..)` per attribute per zoom per feature and
`panic!`s on an unknown key. Any typo in a shortbread rule is a runtime
crash in phase12, discovered only when a matching OSM object is hit.

Should be: `enum AttrKey { Kind, Name, NameEn, ... }` with `#[repr(u8)]`,
`AttrKey::name()` for the MVT key string. Shortbread constructs
`AttrKey::Name` instead of `"name"`; the wire format writes `key as u8`;
decode is a checked `TryFrom<u8>`. The "append only, never reorder"
comment then becomes unnecessary for this table because the record is
ephemeral anyway (see 1.4), and the panic disappears as a category.

`KEY_NAMES` also carries two entries annotated test-only (`way_area`,
`height`) in the production table.

### 1.2 `kind` values are strings with an opportunistic side table

`KIND_VALUES` is a second hand-maintained list of shortbread `kind`
strings, consulted by linear search (up to 129 string compares per kind
attribute per zoom). Misses fall back to raw strings silently, so the
table can drift from shortbread with no failure, only a size/speed
regression. Shortbread already knows the closed set per layer; `kind`
should be an enum (or `&'static str` interned by pointer into one table
owned by shortbread), not re-recognised from text at the wire boundary.
See 2.4 for the duplicated-decision side of this.

### 1.3 The attribute tuple: `(key, value, min_zoom)` as a bare tuple

The `u8` third field is a minimum zoom. It sits beside the tile zoom `z:
u8` that `encode_attrs_bytes` takes and is compared with `zoom >= *az` /
`zoom < *attr_zoom` in two spellings in the same function. A `Zoom`
newtype (0..=14, constructed checked) and a named struct
`Attr { key: AttrKey, value: AttrValue, min_zoom: Zoom }` would make
argument swaps impossible and the filter a method (`attr.visible_at(z)`).
Zoom-as-`u8` is crate-wide; this is just where it is most exposed.

### 1.4 The sort record is an untyped byte layout parsed by hand at two ends

`encode_*` and `add_feature_to_layer` agree on the layout only by
mirroring. Specifics:

- The format comment says `u16 geometry command count`; the code writes
  and reads `u32`. Stale doc on the one place the layout is described.
- Magic numbers: header size 13 appears as `13 +` (twice) and `< 13`;
  value-type tags 0..4 are literals on both sides with no enum.
- `add_feature_to_layer` comment references "the `as u8` encode on line
  69" (wrong line, and the doc rule forbids line refs) and
  `LayerBuilder::reclaim_features` (does not exist; it is
  `prepare_for_reuse`).
- The decoder silently returns or `break`s on every malformed case. The
  stated rationale is "corruption means a code bug, caught by tests", but
  the effect is that a code bug produces a quietly wrong tile (a feature
  missing, or a feature with a truncated tag list) rather than a failure.
  A `debug_assert!` on each early exit, or a typed `RecordError` counted
  into a sidecar counter, would cost nothing on the hot path.

Recommended shape: one `SortRecord` codec module with
`struct FeatureRecord<'a> { id: FeatureId, geom: GeomView<'a>, attrs: AttrIter<'a> }`,
a `ValueTag` enum, `write` and `read -> Result`. The two directions live
next to each other with a round-trip property test; today the encoder is
three entry points (`encode_feature_data`, `encode_feature_data_with_attrs`,
`append_feature_data_with_attrs`), one of them `#[allow(dead_code)]` and
test-only.

### 1.5 `osm_id: u64` carries no element type

The record carries a bare `u64` that becomes the MVT feature id. Nodes,
ways, relations and synthetic ocean ids share the space with nothing in
the type saying which. I did not trace emission to check whether ids are
pre-namespaced; if they are, the namespacing rule is an implicit
convention at each emit site, if not, ids collide across element types.
Either way a `FeatureId` (enum or tagged newtype) minted at emission makes
it explicit. Worth a look by whoever owns emission.

### 1.6 Geometry as `Vec<u32>` command streams everywhere

`mvt::Feature.geometry: Vec<u32>` plus `geom_type: GeomType` is a
stringly-typed geometry: nothing stops a Point-typed feature from carrying
LineTo commands, nor a polygon with a 2-vertex ring. Consequences visible
in scope:

- `merge_connected_lines` decodes the commands to `Vec<Vec<(i32,i32)>>`,
  merges, and re-encodes (plus, in debug, decodes once more).
- MLT `decode_feature_geometry` re-parses every feature's commands into
  `geo_types` and can fail per feature (`GeometryDecode`), turning a
  representational impossibility into a runtime error path that aborts the
  whole run.
- `append_geometry` re-deltas command streams to concatenate them.
- `merge_same_attr_geometries` uses "empty geometry" as a tombstone,
  justified by "all emitted features have non-empty geometry (enforced at
  all emit call sites)" - an invariant held by N call sites, not a type.

A typed `TileGeom` (absolute i32 points + part ranges, polygon parts
carrying role) removes the re-decode loops, makes concatenation a vector
append, and makes MVT delta/zigzag a pure encoder concern. Sort payload
size would be similar or smaller (fixed-width absolute i32 vs. u32
zigzag-delta words are the same 4 bytes; a varint delta encoding inside
the record is an independent optimisation if desired).

### 1.7 Interning indices: `u16` that saturates silently

`intern_key`, `intern_value`, `intern_string_value` compute
`len().min(u16::MAX)`. On the 65,536th distinct value in one layer of one
tile, every further distinct value gets index 65535 - which is a real,
different value. Result: silently wrong attributes, no error. Unlikely in
practice (dense z14 name layers are the only candidate), but this is a
correctness hole dressed as overflow protection. Either use `u32` (MVT
tags are uint32) or return a `Result`. Also `(u16, u16)` tag pairs should
be `(KeyIdx, ValIdx)` newtypes: `Feature.tags` is exactly the "two
siblings of the same primitive" case, and the encoder remaps both through
different tables (`key_remap`, `val_remap`) by position in the tuple.

### 1.8 `Value` has seven variants; the pipeline can produce four

The wire format produces only `String`, `Int`, `Bool`, `Double`. `Float`,
`UInt`, `SInt` exist under `#[allow(dead_code)]`, and MLT carries a full
seven-way `MltColumnType` machine, a type-bit mask, and a `Mixed` column
that stringifies values - all exercised only by tests that hand-build
`LayerBuilder`s. Either shrink `Value` to what shortbread can emit (and
let MLT columns be typed from the schema - see 2.5), or document why the
dead variants are kept.

### 1.9 Extent and buffer as literals and floats

`4096` appears as: `geometry::EXTENT: f64`, `int_ocean::TILE_EXTENT_I32`,
`verify::MVT_EXTENT: i64`, `svg::EXTENT: f64`, literal `4096` in the MVT
layer encoder (field 5), literal in `mlt::TileLayer::builder(.., 4096)`,
default in `tile_detail`. The buffer is `geometry::BUFFER_FRACTION: f64`
and `int_ocean::TILE_BUFFER_I32 = 128`. The line merger reconstructs its
clip boundary from the float pair with a const assert. One
`TileSpace { EXTENT: i32, BUFFER: i32 }` owner (integer, since every
consumer that matters is integer) with derived float views would collapse
these. See 2.3.

### 1.10 Typed failure

- `MltEncodeError::Encode(String)` wraps every mlt_core error as prose;
  `GeometryDecode { message: String }` likewise. The pipeline turns it into
  `PipelineError(String)` and aborts. A caller cannot distinguish "our
  geometry was malformed" (a bug) from "mlt_core rejected a column" (a
  config/capability issue).
- MVT encoding is infallible by construction but `encode_tile_into` can
  produce an empty buffer which the caller checks (`s.mvt_buf.is_empty()`)
  as a second "no tile" signal alongside `non_empty.is_empty()`.
- The line merger's `check_merge` returns `Result<(), String>`; fine for a
  debug checker, but the tests match on prose fragments. An enum
  `MergeViolation { Consumption, Disconnected, BoundarySplice, Mismatch,
  Conservation }` would let the "checker fires on X" tests assert the
  category rather than any `Err`.

---

## 2. Decisions made in more than one place

### 2.1 "What post-processing does a tile get before encoding?" - two owners, ALREADY DISAGREE

Sites: `assemble.rs::encode_tile_batch_mvt` and
`assemble.rs::prepare_non_empty_layers` (MLT).

| step | MVT path | MLT path |
|---|---|---|
| `merge_same_attr_geometries` | every layer **except Ocean** | **every layer including Ocean** |
| `merge_connected_lines` | layers at `z < 14` | **never** |
| `prepare_for_reuse` | not called | called (no-op, see 3.4) |

So an MLT archive has merged ocean features (the MVT path deliberately
does not, presumably for the ring-cap / artifact-equivalence reasons) and
no line merging at all, meaning the boundary-line palindrome/duplicate
defects the merger and its clip-boundary rule exist to prevent are
handled differently per format. Nothing tests the two paths against each
other. Also the ocean exemption is spelled `li == Layer::Ocean as usize`
inline and the zoom gate `z < 14` is a bare literal - neither is named or
explained at the site (why not z14? because z14 is max zoom and lines are
not generalized there? the reason should be on a named constant or,
better, on a per-layer policy).

Owner: a single `TilePostprocess` (or a method on a `TileModel` that
owns all 26 `LayerBuilder`s) taking `(z, &LayerPolicy)` and returning the
finished layers; both encoders call it. The per-layer bits (merge
eligibility, line merging eligibility, max zoom) belong in shortbread's
layer definitions, beside `Layer::ALL`, not in assemble.

### 2.2 "How is an MVT command stream decoded?" - at least eight sites

- `mvt/merge.rs::append_geometry` (re-delta; truncates on short input,
  copies unknown commands verbatim)
- `mvt/merge.rs::decode_line_segments` (wrapping arithmetic; unknown
  command skips `count*2` words)
- `mlt.rs::parse_points` / `parse_paths` (checked, errors; plain `+=`
  which overflows/panics in debug)
- `geometry/mvt_decode.rs::decode_mvt_polygon` (unknown command skips
  *zero* words; ClosePath pushes the first vertex)
- `tile_detail.rs` (the "one decode surface" brokkr uses)
- `svg.rs`, `verify.rs`, `main.rs` diag (each with its own zigzag)
- `mvt/tests.rs` contains yet another full protobuf + command parser
  (`parse_fixture_*`)

Zigzag decode alone: `mvt::decode_zigzag`, `merge::unzigzag` (in the
same module that imports `decode_zigzag`!), `mlt::decode_zigzag`,
`mvt_decode::unzigzag`, `tile_detail::unzigzag`, `svg::zigzag_decode`,
`verify::zigzag_decode_u32`, `main::unzigzag_diag`, plus `way_index`'s
own pair.

These disagree on unknown-command handling (skip 0 words vs skip
`2*count` vs copy vs error) and overflow (wrapping vs panic vs error).
They agree on well-formed streams, which is all we emit - but the
ClosePath cursor bug (R23) is exactly the class of defect that lives in a
symmetric encoder/decoder pair, and the cure for it is one decoder whose
semantics are pinned to the spec and to MapLibre, used everywhere. The
internal consumers (merge, MLT) should not decode at all (1.6); the
external ones (verify, svg, diag, tests) should all go through
`tile_detail`'s command decoder with a strictness parameter.

### 2.3 "Where is the clip boundary?" - two derivations

`merge.rs` derives `CLIP_BOUNDARY_LO/HI` from
`geometry::EXTENT * geometry::BUFFER_FRACTION` (float); the polygon engine
uses `int_ocean::TILE_BUFFER_I32 = 128`. The line clipper (emit.rs,
`ClipRect::for_tile(.., BUFFER_FRACTION)`) clips in float Mercator and
rounds afterwards. The merger's correctness depends on the clipper's
rounded output landing *exactly* on ±128/4224; that is a cross-module
contract asserted by one test (`clip_boundary_matches_the_line_clip_contract`)
and a const assert on the float product, not by construction. If the line
clipper ever moves to the integer pipeline with a different buffer, or a
per-layer buffer is introduced (planetiler and tilemaker both have them),
the merger silently starts joining through clip points again. Owner: the
clipper should *mark* clip-created endpoints (a bit in the typed geometry
of 1.6) rather than the merger inferring "created by clipping" from
position - the code comment itself calls position "a conservative
proxy". With typed geometry this proxy and its false positives go away.

### 2.4 "Which strings are shortbread keys and kind values?" - shortbread and wire_format

Shortbread rules write string literals; `KEY_NAMES` and `KIND_VALUES` in
wire_format independently enumerate them. Drift in keys panics (1.1);
drift in kinds is silent (1.2). One owner: shortbread defines the key and
kind enums; wire_format only serializes their discriminants.

### 2.5 "What type is attribute X?" - decided per tile by MLT, statically known by shortbread

`mlt::build_tile_model` re-infers each column's type per tile by scanning
values, and falls back to `Mixed` (stringify) when a key carries two
types in one tile. The schema is static: shortbread knows `population` is
an int, `name` a string. Inferring per tile means the same key can be an
i64 column in one tile and a string column in the next, which is a
consumer-visible inconsistency in MLT output. The column type should come
from the shortbread schema (same owner as 2.4), and a value of the wrong
type is then a bug, not a `Mixed` column.

### 2.6 "How are polygon rings grouped into polygons?" - MLT vs MapLibre vs pyramid

`mlt::group_rings_to_polygons` uses first-ring-sign classification
(`sign >= 0` grouped with positive, so zero-area rings join whichever side
`>= 0` lands on). MapLibre's `classifyRings` skips zero-area rings. The
emission side (pyramid) knows exactly which ring is an outer and which a
hole and throws that away into a command stream. A third reconstruction
(`decode_mvt_polygon`) does no grouping at all. With part roles carried
in the typed geometry (1.6), MLT never has to guess.

### 2.7 "Does an encoding change require an OCEAN_POLICY_VERSION bump?" - comment-only

`mvt/mod.rs` header says any encoding change alters durable ocean
artifact bytes and must bump `OCEAN_POLICY_VERSION` in `ocean.rs`.
Nothing enforces it. A cheap structural fix: an `MVT_ENCODER_VERSION`
constant in `mvt` that is folded into the artifact key alongside the
ocean policy version, plus a golden-bytes test of a fixed ocean tile so
an encoder change fails a test that names the constant to bump.

### 2.8 Attribute zoom filtering: one site, two spellings

Minor, but `encode_attrs_bytes` counts with `zoom >= az` and writes with
`zoom < attr_zoom -> continue`. These are the same predicate written
twice; they would diverge if one were edited. A method on the attr type
(1.3) fixes it. This function also has a real bug, see Bugs.

---

## 3. Structure

### 3.1 `mvt` owns the tile model, which is not an MVT concept

`LayerBuilder`, `Feature`, `Value`, the interning tables, the merge
passes, and the scratch types live in `mvt/`. MLT consumes them through
`#[cfg_attr(not(feature = "mlt"), allow(dead_code))]` accessors that
exist only for it. The format-independent model (layers, features,
attributes, interning, post-processing) should be its own module
(`tile_model/`), with `mvt` and `mlt` as two sibling sinks. Today the
dependency edge `mlt -> mvt` points the wrong way and the MVT module
carries cfg noise for its sibling.

### 3.2 `wire_format` knows the tile model

`wire_format::add_feature_to_layer` decodes a sort record *directly into*
a `LayerBuilder`, interning as it goes, and handles the per-worker Vec
pools. So the sort-record codec depends on the MVT tile model and on the
assembly pooling strategy. Split: `wire_format` yields a borrowed
`FeatureRecord` view; assembly owns "push record into tile model". This
also makes the codec testable without `LayerBuilder` test accessors.

### 3.3 Line merging is a geometry algorithm living in the encoder

`merge.rs` holds two different things: multi-geometry concatenation
(`merge_same_attr_geometries`, which is a tile-model operation) and a
line-graph merger with its witness-based invariant checker (a geometry
algorithm with its own clip-boundary semantics). The line merger should
live with geometry/line code (beside the line clipper whose contract it
depends on, 2.3), operate on typed segments, and be called by the
post-processing owner. The invariant checker is good work and should
move with it.

### 3.4 The pooling design is dead; its documentation is not

- `MergeScratch` doc: "Created once per thread via `thread_local!`,
  reused across all tiles on that worker."
- `LayerBuilder::prepare_for_reuse` doc: "Used by assembly `thread_local!`
  to reuse `LayerBuilder` across tiles."
- `AssemblyScratch` doc: "persisted across batches via `thread_local!`."
- `merge_same_attr_geometries` comment: "keeps scratch.geom at high-water
  capacity forever", citing a 3.8 GB churn saving.

Reality: both `encode_tile_batch_mvt` and `encode_tile_batch_mlt` call
`AssemblyScratch::new()` **per tile** inside the rayon closure. There is
no `thread_local!` in assemble. Every pool starts empty for every tile,
so `geom_pool`/`tags_pool` reuse only within one tile, `prepare_for_reuse`
is a no-op on fresh builders, and the high-water claim is false. Either
the thread-local was removed deliberately (then delete the pool plumbing,
which threads `&mut Vec<Vec<u32>>` through three public signatures) or
it was lost in a refactor (then the alloc profile that justified it has
silently regressed). `brokkr tilegen --alloc` would settle which. This is
the strongest "state split from the behaviour acting on it" case in
scope: the pools are passed as loose `&mut Vec` arguments rather than
owned by the object that knows their lifetime.

### 3.5 Tests

- `mvt/tests.rs` (1.6k lines) mixes encoder unit tests, a private
  protobuf fixture parser, `append_geometry` tests, same-attr merge tests,
  and line-merger/checker tests. Its layout mirrors the accident that all
  of these live under `mvt/`. If 3.1/3.3 happen, the tests split with
  their code; the private parser should be replaced by `tile_detail`.
- `wire_format` tests exercise the 255-attr cap but only assert decoded
  count, which is why the trailing-bytes bug below passes.
- Merge tests reach into internals through `test_*` re-export shims
  (`test_append_geometry`, `test_merge`, `TestMergeRun` tuple type,
  `TEST_CLIP_BOUNDARY`); `LayerBuilder` grows `test_feature`/`test_key`
  accessors that duplicate the `pub(crate)` `features()`/`key()` MLT
  already uses. Symptoms of visibility being shaped around file layout.
- No test compares MVT and MLT output of the same tile semantically,
  which is how 2.1 went unnoticed.

### 3.6 `LayerBuilder` interning: three maps for one table

`key_map`, `value_map` (non-string values) and `string_value_map` (string
values) plus `values: Vec<Value>`. `value_map` never sees strings because
`intern_value` diverts them, so the comment's "keep consistent" is
maintained by routing discipline. Fine for speed, but the
string-in-`value_map` case is unreachable only by convention. With typed
`AttrKey`/kind enums (1.1/1.2) most strings become small integers and
keys become a fixed 52-entry array index, making the key `FxHashMap`
unnecessary.

### 3.7 Encoding constants

`encode_tile_into` writes `extent = 4096` and `version = 2` as literals in
`LayerBuilder::encode`; `MAX_FEATURE_RINGS` (a MapLibre consumer limit
that binds at emission in pyramid.rs) lives in `mvt/mod.rs`. It is a
consumer-compatibility constant used by geometry; it belongs with the
geometry emission policy or a `consumer_limits` module, not the encoder.

---

## Bugs and smells found along the way

1. **`encode_attrs_bytes` writes more attributes than its count byte
   says.** The count is `min(filtered, 255)` but the loop writes every
   filtered attribute. Over 255 attrs, the record carries trailing bytes
   that the decoder ignores. Latent (no shortbread feature has 255
   attrs), but the cap tests pass only because they check decoded count.
   Fix: stop the loop at the cap, or make the count `u16`, or reject.
2. **String truncation can split a UTF-8 sequence**, after which the
   decoder's `from_utf8(..).unwrap_or("")` turns the whole value into an
   empty string. Latent (OSM caps values at 255 chars), silent.
3. **Interning saturation at u16::MAX mis-tags features silently** (1.7).
4. **MVT and MLT post-processing disagree** (2.1): ocean merging and line
   merging differ by format.
5. **Per-tile `AssemblyScratch::new()` defeats the documented pooling**
   (3.4); four doc comments describe a `thread_local!` that does not exist.
6. Stale format doc: `u16 geometry command count` is `u32` in code.
7. Stale references: "line 69", `LayerBuilder::reclaim_features`.
8. `merge.rs` defines `unzigzag` while importing the identical
   `decode_zigzag` from its parent.
9. `merge_same_attr_geometries` sets `id = None` on merged features but
   keeps ids on singletons; MVT ids in one layer are therefore present or
   absent depending on whether a same-attr sibling happened to be in the
   tile. Probably intended, but it means feature ids are not a stable
   consumer contract (feature-state in MapLibre keys on them).
10. `merge_same_attr_geometries` sorts with `sort_by` on full tag vectors
    and compares them again in the scan; cheap, but with `(KeyIdx,
    ValIdx)` as a packed u32 per tag this becomes a slice compare of
    integers.
11. `dedup_parallel_segments` clones every segment twice (reverse + key)
    into a fresh `FxHashSet` per feature per tile; a hash of the
    canonical orientation without materialising it would avoid both.
    Micro, flagged only because it is on the z0-13 line path.
12. `build_tile_model` finds columns with `columns.iter().position` per
    tag (quadratic in column count per feature). Small n, but it is per
    feature per tile.
13. MLT `Mixed` columns stringify values, so a numeric attribute can turn
    into a string in some tiles (2.5).
14. `brotli::CompressorWriter::new(&mut compress_buf, 4096, ...)` - the
    4096 here is a buffer size, adjacent to all the extent 4096s; harmless
    but a grep hazard, name it.

## Suggested order if acted on

1. Fix 2.1 now (route MLT through the MVT post-processing, or extract a
   shared function) - it is a live output divergence.
2. Settle 3.4 with an `--alloc` run; then either restore the thread-local
   or delete the pool plumbing.
3. `AttrKey`/kind enums owned by shortbread (1.1, 1.2, 2.4, 2.5).
4. Extract `tile_model/` out of `mvt/`; make `wire_format` yield a view
   (3.1, 3.2).
5. Typed `TileGeom` as sort payload and assembly model, with clip-created
   endpoints marked and polygon part roles carried (1.6, 2.3, 2.6);
   collapse the command decoders to one (2.2).
