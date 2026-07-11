# Spec: deterministic within-layer feature order with a deliberate paint-order key

Written against `reference/technical-implementation-spec.md` (the contract this
document must satisfy). Spawned from the first item under "Tile output
optimizations" in `TODO.md`. History anchor: the old regress spec
(`git show 9ee474a:notes/spec-5-output-regression.md`, "Determinism is NOT
established") documents the nondeterminism as a discovered fact the comparison
tool was built to tolerate, never a ratified design decision. The measurement
record is `reference/performance.md` plus `.brokkr/results.db`.

Competitor evidence: `notes/paint-order-competitors.md` (2026-07-11 survey of
planetiler, tilemaker, and tippecanoe, file/function pointers into
`research/`). Headlines this spec should be checked against: all three
physically order features within layers and none emits an order attribute;
planetiler packs an explicit sort key into its global merge key and
tie-breaks the k-way merge by comparing full encoded value bytes (the model
closest to this spec's total order); renderers draw fills/lines in
within-layer order (later = on top) and place symbols in order (earlier =
wins collisions); tippecanoe demoted compression-motivated reordering to
opt-in after it scrambled paint order; one u8 of priority covers the
realistic worst case (~170 distinct values in tilemaker's shipped recipe).

## The problem

Feature order within a tile layer is nondeterministic run to run. Visually
confirmed 2026-07-11: parks inside cities vanish under the residential landuse
fill when the order flips, because MapLibre paints features in stream order
and same-layer fills are opaque. Two same-commit denmark builds differ in
byte-level dedup (~28 tiles/build leak) and only 1.22M of 1.30M tile pairs hit
the regress engine's raw byte-equality tier on an identical pair.

Two distinct defects hide under one symptom:

1. **No deliberate order.** The sort key has carried a `priority` byte since
   the first commit, reserved for paint order and never wired - every call
   site passes 0. Within a layer, paint order is whatever the merge emits.
2. **No total order.** Records with equal sort keys are ordered by chunk
   assignment, and chunk assignment is raced.

Fixing (2) alone gives reproducible builds with an arbitrary-but-stable paint
order. Fixing (1) alone gives a paint order that still shuffles within equal
priorities. Both land here, as two ordered landings.

## Survey of the ground

All verified in code 2026-07-11 at `e2284ec` (+ dirty CLAUDE.md/scripts).

### The sort key and its unused byte

`src/sort.rs`: `pub type SortKey = u64`, packed by
`make_sort_key(tile_id, layer, priority)` as
`(tile_id << 16) | (layer << 8) | priority`. Accessors `tile_id_from_key`,
`layer_from_key` exist; there is no `priority_from_key`. `partition_from_key`
and `partition_next_key` operate on the tile_id field only, so a populated
priority byte cannot move a record across partitions; the existing partition
boundary test already exercises `make_sort_key(..., u8::MAX, u8::MAX)` against
`partition_next_key`.

Producers of keys:
- `src/pipeline/emit.rs` `push_sort_record(tile_id, osm_id, layer, geom_type,
  geom_buf, attrs_buf, records)` - the single funnel for ALL OSM feature
  emission (point, line, polygon, multipolygon). Calls
  `make_sort_key(tile_id, layer as u8, 0)`. Its seven callers
  (`emit_point_or_centroid`, `emit_line_feature` twice,
  `emit_polygon_feature`, `emit_multipolygon_feature`, and
  `process_node` in `src/pipeline/phase12.rs`) all have `m: &LayerMatch`
  in scope at the call.
- `src/ocean.rs` ocean sink: `sort::make_sort_key(tile_id, layer_idx, 0)`,
  empty attrs, synthetic `feature_id = piece_idx` (deterministic - pieces come
  from the shapefile in read order).

### The nondeterminism chain

- **Racy chunk assignment.** `src/ocean.rs` allocates chunk ids from an
  `Arc<AtomicUsize>` raced by rayon workers; `src/pipeline/phase12.rs` does
  the same via `attach_chunk_counter` plus a shared `SpillCoalescer` that way
  tasks, relation workers, and phase tails all `append` into under a mutex in
  arrival order. Which records land in which chunk, and in what buffer order,
  varies run to run.
- **Unstable within-chunk sort by key only.** Four funnels sort records
  before hitting disk, all by bare key:
  `SortWriter::flush_chunk` (`self.buffer.sort_unstable_by_key(|r| r.key)`),
  `write_sorted_chunk`, `write_sorted_payload_chunk`, and
  `write_partitioned_payload_chunks` (which `SpillCoalescer::write` calls).
  Equal keys keep racy arrival order.
- **Merge tie-break by chunk index.** `HeapEntry::Ord` in `src/sort.rs`
  compares `(key, chunk_idx)`. Equal-key records from different chunks come
  out in chunk-id order, and chunk ids are the raced atomics above.

### How order becomes pixels

`src/pipeline/assemble.rs` consumes the k-way merge in key order and pushes
`(layer_idx, data)` pairs per tile; `encode_tile_batch_mvt` adds features to
each `LayerBuilder` in exactly that order. MVT feature order in the layer is
merge output order. Two in-place transforms run after:

- `LayerBuilder::merge_same_attr_geometries` (`src/mvt/merge.rs`): stable
  sort of indices by `(geom_type, tags)`, merges each run into the FIRST
  occurrence's position, tombstones the rest. First-occurrence order is
  preserved; deterministic input order gives deterministic output. Merging
  can only collapse features with identical tags.
- `LayerBuilder::merge_connected_lines`: per-feature segment rejoining,
  deterministic given the feature's own geometry.

Downstream, everything is order-faithful: rayon `par_iter().collect()`
preserves batch order, tiles are written in Hilbert order, PMTiles dedup
hashes tile bytes, gzip/brotli are deterministic at fixed level, and the
metadata JSON is assembled with `format!` from deterministic aggregates. No
timestamp lands in the archive (`osmosis_replication_timestamp` comes from
the input PBF, same every run). Conclusion: the record stream order is the
only nondeterminism source; make it total and the archive is byte-stable.

### The MLT path re-sorts and is explicitly out of scope

The above holds for MVT, which appends features to the layer in stream order
and never reorders. The MLT encoder does NOT. `src/mlt.rs`
`encode_layer` calls `layer.encode(EncoderConfig::default())`, and
mlt-core 0.12.3's `EncoderConfig::default()` sets
`attempt_spatial_morton_sort`, `attempt_spatial_hilbert_sort`, and
`attempt_id_sort` all `true` - the encoder tries each feature reordering and
keeps whichever encodes smallest (mlt-core `encoder/sort.rs`). Any paint rank
we bake into the record stream is discarded in MLT output. Planetiler hit the
same wall and exposes `mlt_reorder_features` (default off) precisely to keep
MLT paint order intact.

Both landings therefore scope their paint-order guarantee to **MVT + gzip**
(the same envelope the regress engine and earcut oracle already restrict
themselves to). Landing 1's byte-determinism claim likewise holds only for
MVT; whether MLT is even run-to-run deterministic under the smallest-encoding
selection is a separate question this spec does not answer. Making MLT honor
paint rank means either passing an `Unsorted` `SortStrategy` (disabling MLT's
spatial/id reordering, at a tile-size cost) or a dedicated MLT order gate -
both deferred (stopping rule). The determinism gate below runs against the
default MVT output, so it never exercises the MLT path.

### The record payload as a tiebreak

`src/wire_format.rs`: payload = `u64 osm_id (LE), u8 geom_type,
u32 cmd_count, u32 x cmds, attr bytes`. The payload is a pure function of the
feature (id, geometry, attrs at that zoom) - no producer-dependent bytes.
Lexicographic byte comparison of payloads is therefore a chunk-independent
total order. It is not numeric-osm_id order (LE bytes); determinism needs
totality, not any particular order, and equal payloads are identical records
whose mutual order is unobservable. osm_id alone is NOT a sufficient tiebreak:
`for_each_clipped_segment` pushes multiple records for one way in one tile
(same key, same osm_id, different geometry).

### What the paint rank can be computed from

`src/shortbread/mod.rs` `LayerMatch { layer, min_zoom, max_zoom, geom_expect,
attrs }` (const size assert 408) is built at tag-match time, where the kind
string and the bridge/tunnel/link facts are in scope as plain values -
`src/shortbread/land.rs` `land_match` returns the kind; `streets.rs`
`match_streets_line` has `kind_raw` (pre `_link` strip), `is_link`,
`is_tunnel(tags)`, `is_bridge(tags)`. Attrs are pre-encoded per zoom later, so
match time is the only place the rank can be computed once. LayerMatch is
constructed at ~27 sites (streets.rs 6, water.rs 8, land.rs 5, boundaries.rs
3, transport.rs 3, pois.rs 1, mod.rs 1, pipeline_tests.rs 7); all but land and
streets get rank 0. `enrich_polygon_matches` mutates existing matches in
place and constructs none.

Attr visibility for a decoded-archive checker: land emits `kind` ungated
(zoom 0) - full rank recomputable at every zoom. Streets emit `kind` (post
`_link` strip) and `rail` ungated, but `link`/`tunnel`/`bridge` are gated to
z>=11 (`attr_bool_z(.., 11)`). StreetPolygons emit `bridge`/`tunnel` ungated.
So a monotonicity check on decoded tiles can verify the full street rank only
at z>=11; below that only the kind-derived component is recomputable. This
forces the rank encoding to be kind-major (below).

### Competitor convention

`research/tilemaker/resources/process-openmaptiles.lua` `SetZOrder` implements
imposm's `wayzorder`: highway class 3..9 plus bridge +10 / tunnel -10 / OSM
`layer` tag x10 ("upstream context #323" in the TODO refers to this lineage).
We adopt the road-importance core. We do NOT adopt elevation-major or the OSM
`layer` tag: elevation-major would make the z<11 archive check vacuous (the
major key would be undecodable below z11), and we do not currently read the
`layer` tag at all (out of scope, noted in the stopping rule).

### Gate tooling facts that shape the plan

- The blessed denmark archive (`c9362c4`) predates the injected-pins geometry
  change; the bare `brokkr regress` gate is stale until a user-gated
  `brokkr bless` (reference/performance.md). All regress gates below therefore
  use explicit `--file`/`--against` archives, never the blessed default.
- `brokkr regress` accepts `--file <CURRENT>` and `--against <BLESSED>`
  (verified via `--help`).
- `brokkr verify pmtiles --dataset denmark --tiles locations` exists, but
  reference/performance.md records a resolution gap ("resolves only
  brokkr.toml-pinned pmtiles entries and this project pins none"). Fallback
  pinned below: run the built binary directly,
  `target/release/elivagar verify <archive>`.
- `brokkr svg` colors per LAYER (`LAYER_COLORS[li]` in `src/svg.rs`), so
  within-layer paint order is invisible in SVG. The old in-repo
  `.brokkr/preview/debug-viewer.html` was single-color-per-layer too (same
  blind spot) and has been removed. The human rendering gate uses the user's
  external kind-aware viewer (Landing 2 gate 8), the only instrument that
  renders within-layer paint order.
- Regress canonicalization sorts features and merged-feature components - it
  is deliberately order-blind. That is exactly why it works as the semantic
  no-change gate across these landings, and exactly why it can never be the
  paint-order gate. The order gate is built in Landing 2.
- Outputs land in `target/` (convention from notes/injected-prepass-spec.md
  and `data/blessed/denmark-c9362c4.pmtiles` naming); each `brokkr tilegen`
  run prints its resolved output path. `<OUT>` below means that printed path.

## Target structure

### Landing 1: a total record order

One rule, applied everywhere records are ordered:

```rust
// src/sort.rs
/// Total order on sort records: key, then payload bytes. The payload is a
/// pure function of the feature, so this order is independent of chunk
/// assignment and producer scheduling - the k-way merge of any chunking of
/// the same record multiset yields the same sequence.
#[inline]
fn record_cmp(a_key: SortKey, a_data: &[u8], b_key: SortKey, b_data: &[u8]) -> Ordering {
    a_key.cmp(&b_key).then_with(|| a_data.cmp(b_data))
}
```

Applied at all five ordering sites in `src/sort.rs`:

1. `SortWriter::flush_chunk`: `self.buffer.sort_unstable_by(...)` over
   `SortRecord { key, data }`.
2. `write_sorted_chunk`: same, over the `&mut [SortRecord]` argument.
3. `write_sorted_payload_chunk`: `records.sort_unstable_by(...)` where the
   comparator resolves `(key, offset, len)` payload slices from the `payload`
   argument.
4. `write_partitioned_payload_chunks`: same as (3). This covers
   `SpillCoalescer::write`, its only production caller.
5. `HeapEntry`: add no fields (keep the `size_of == 32` assert only if it
   still holds; `data` is already in the entry). `Ord` becomes reverse
   `(key, data, chunk_idx)` - `chunk_idx` stays as the FINAL component purely
   so the heap order is total when two chunks hold byte-identical records
   (interchangeable, unobservable in output). **`PartialEq` must move in
   lockstep.** Today `impl PartialEq for HeapEntry` compares
   `(key, chunk_idx)` only (`src/sort.rs`); leaving it while `Ord` gains
   `data` breaks the `BinaryHeap` requirement that `a == b` iff
   `a.cmp(b) == Equal` - a silent correctness trap, not a style nit. It is
   currently Ord/Eq-consistent only because at most one live entry exists per
   chunk (k-way merge), making `chunk_idx` a unique tiebreak; that invariant
   is not obvious and must not be relied on across an `Ord` change. Rewrite
   `PartialEq` to `(key, data, chunk_idx)` too (or, cheapest, derive both from
   the single `record_cmp`-plus-`chunk_idx` rule), so equality and ordering
   share one definition.

No change to chunk-id allocation, chunk naming, `--skip-to` resume, or the
raced `AtomicUsize` allocators: once the order is total, chunk assignment is
provably irrelevant to output (a k-way merge of sorted chunks emits the
sorted multiset regardless of how the multiset was split). The race stays;
its consequences end.

Also add the missing accessor (tests and future debugging):

```rust
// src/sort.rs
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub fn priority_from_key(key: SortKey) -> u8 {
    (key & 0xFF) as u8
}
```

Update the sort.rs module comments that currently describe the merge as
"tie-broken by chunk index".

### Landing 2: the paint-order key plus its gate instrument

**New module `src/shortbread/paint_order.rs`** - the single source of truth
for ranks, consumed by both the matchers (encode side) and `verify.rs`
(check side), so the instrument cannot drift from the emitter:

```rust
/// Paint rank for the land layer, by kind. Background-first: the renderer
/// paints features in stream order, so smaller = painted earlier = below.
pub fn land_paint_rank(kind: &str) -> u8

/// Kind-importance class for streets/street_polygons kinds AS EMITTED
/// (post `_link` strip). Total: unknown kinds get UNKNOWN_STREET_CLASS.
pub fn street_class_rank(kind: &str) -> u8

/// Full street paint rank. Kind-major so the archive checker can verify the
/// major component below z11 where link/tunnel/bridge attrs are not emitted.
/// elev: tunnel=0, surface=1, bridge=2. Links sit just below their parent.
pub fn street_paint_rank(kind: &str, link: bool, tunnel: bool, bridge: bool) -> u8 {
    let elev: u8 = if tunnel { 0 } else if bridge { 2 } else { 1 };
    street_class_rank(kind) * 6 + elev * 2 + u8::from(!link)
}
```

**Pinned land table** (`land_paint_rank`), covering every kind `land.rs` can
emit. Bands, ascending (painted later = above):

| rank | kinds |
|---|---|
| 0 | residential, commercial, retail, industrial, garages, railway, brownfield, greenfield, landfill, quarry, farmyard |
| 1 | farmland, meadow, orchard, vineyard, allotments, plant_nursery, greenhouse_horticulture |
| 2 | park, golf_course, recreation_ground, village_green, cemetery, grave_yard |
| 3 | bare_rock, scree, shingle, sand, beach, grassland, heath, scrub, bog, marsh, string_bog, swamp, wet_meadow |
| 4 | grass, garden, playground, miniature_golf |
| 5 | forest |
| 6 | cliff |

Band logic follows typical containment direction: broad developed fills at
the bottom (the TODO's named defect: parks above residential); broad
recreation below the natural covers and fine green they contain (golf bunkers
tagged natural=sand paint above the course; groves paint above the park);
forest on top of the fills it punctuates; the cliff line above all fills.
Unknown kind = 0 (background). Nesting against the typical direction (a park
inside a forest) is not resolvable by a kind table and is out of scope
(stopping rule).

**Pinned street class table** (`street_class_rank`), ascending importance,
on emitted kinds:

```
track 0, footway 1, steps 2, path 3, cycleway 4, pedestrian 5,
living_street 6, service 7, busway 8, bus_guideway 9, taxiway 10,
runway 11, unclassified 12, residential 13, tertiary 14, secondary 15,
primary 16, trunk 17, motorway 18, funicular 19, monorail 20, tram 21,
light_rail 22, subway 23, narrow_gauge 24, rail 25
```

`UNKNOWN_STREET_CLASS = 12` (unclassified level). Max street paint rank =
25\*6+5 = 155; both tables fit the u8 with headroom.

**LayerMatch gains the field:**

```rust
pub struct LayerMatch {
    pub layer: Layer,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub geom_expect: GeomExpect,
    pub paint_rank: u8,      // NEW - key priority byte, 0 = background/none
    pub attrs: SmallVec<[Attr; 8]>,
}
```

Update the `size_of` const assert to the new compiled value (expected 408 -
the u8 fits existing padding - or 416; read the compile error, pin what the
compiler says). Setters:

- `land.rs` `match_land`: `paint_rank: paint_order::land_paint_rank(kind)`;
  `match_land_lines` (cliff): rank 6 via the same function.
- `streets.rs` `match_streets_line`:
  `paint_order::street_paint_rank(kind, is_link, is_tunnel(tags), is_bridge(tags))`
  (note: `kind` post-strip, matching what verify can decode).
- `streets.rs` `match_street_polygons`: same function with `link: false` and
  that site's bridge/tunnel booleans.
- Every other construction site: `paint_rank: 0` (explicit, mechanical; the
  compiler enumerates them).

**Key plumbing.** `push_sort_record` gains the byte and every caller passes
`m.paint_rank`:

```rust
pub(super) fn push_sort_record<T: FeatureRecordSink + ?Sized>(
    tile_id: u64,
    osm_id: u64,
    layer: Layer,
    paint_rank: u8,
    geom_type: GeomType,
    geom_buf: &[u32],
    attrs_buf: &[u8],
    records: &mut T,
) {
    let key = sort::make_sort_key(tile_id, layer as u8, paint_rank);
    records.push_feature(key, osm_id, geom_type, geom_buf, attrs_buf);
}
```

Callers: the four emitters in `src/pipeline/emit.rs` plus `process_node` in
`src/pipeline/phase12.rs`. `src/ocean.rs` keeps its literal 0 (single
full-coverage layer, no internal order). Nothing in assemble changes: the
merge already emits key order, which is now (tile, layer, paint_rank,
payload) order, and `add_feature_to_layer` appends in that order.

**The verification instrument** (this is the brick the reference doc demands:
no existing gate can see paint order - regress is order-blind by design, svg
is one-color-per-layer). This is NOT a light extension of existing decoding.
`src/verify.rs` today iterates layers and features but `validate_mvt_feature_geometry`
reads only the feature `type` (field 3) and `geometry` (field 4), `skip_field`s
everything else, and never touches the feature `tags` (field 2) or the layer
`keys`/`values` tables. Recomputing a rank needs the tag payload, so this
landing must add a **new MVT tag decoder**: parse the layer `keys[]` and
`values[]` tables, resolve each feature's tag-pair indices, and extract the
`Value`-message string/bool for `kind`/`link`/`tunnel`/`bridge`. That is the
exact decoder-convention surface that hid the R23 ClosePath bug for three
months, so it is new machinery that carries its own unit coverage (below),
not a one-liner. Extend `src/verify.rs`, which already iterates every tile's
layers and features:

- New check, run for layers `land`, `streets`, `street_polygons` on every
  tile: walk features in stream order, decode `kind` plus the
  `link`/`tunnel`/`bridge` bools when present, recompute the rank via
  `shortbread::paint_order`, and assert the sequence is non-decreasing.
  - z >= 11 (and land at every zoom, and street_polygons at every zoom -
    their flags are ungated): check the full rank
    (`land_paint_rank` / `street_paint_rank` with absent flag = false).
  - streets below z11: check `street_class_rank(kind)` only (the major
    component; link/tunnel/bridge are not in the tile there). Because the
    encoding is kind-major, full-rank order implies class-rank order, so
    this is a sound necessary condition - and it is immune to
    `merge_same_attr_geometries` relocating same-tag features (same tags at
    a zoom imply the same checked component at that zoom).
- Violations are verify errors, reported and counted like existing tile
  errors (subject to the same 100-error stop), failing the exit code.
- **Fail closed.** A targeted-layer feature whose `kind` is missing, whose
  attr is present but the wrong wire type, or that the checker otherwise
  cannot decode into a rank is itself a verify error - never a silent skip.
  A skipped feature is an unchecked feature, and the checker's whole job is
  that no feature slips past it.

Common-mode caveat, called out because it bounds what this gate proves: the
emitter and the checker both call `shortbread::paint_order`, so a *wrong but
self-consistent* table entry (a kind typed into the wrong band) produces an
out-of-order stream AND a checker that blesses it - the monotonicity check
catches ordering that disagrees with the table, not a table that disagrees
with intent. The `every_*_kind_has_rank` membership tests do not close this:
membership is not the *value*. So the paint_order tests below carry an
**independent expected-rank fixture** - a hand-written `[(kind, expected_u8),
...]` table, transcribed from the pinned tables in this spec by a second
pair of eyes, asserted against the functions - so the mapping is pinned in
two independently-authored places, not derived from the code under test.

**Tests** (named per the reference doc's "named unit tests for behavior no
oracle reaches"):

- `src/sort.rs` `merge_order_is_chunk_assignment_independent`: one record
  multiset with equal-key groups, written as chunks under two different
  splits/orders, merged; assert byte-identical output sequences. (Extends
  the existing external-chunk test fixtures.)
- `src/sort.rs`: extend the existing `make_sort_key` roundtrip test with
  `priority_from_key` over nonzero priorities.
- `paint_order` tests: `every_land_kind_has_rank` (iterate every kind string
  `land_match`/`match_land_lines` can return; assert table membership - keeps
  the table and matcher in lockstep), `every_street_kind_has_rank` (same for
  the streets/street_polygons kind sets), `street_link_ranks_below_parent`,
  `street_bridge_above_tunnel_within_kind`,
  `land_bands_background_first` (spot: residential < park < sand < grass <
  forest < cliff), and `ranks_match_independent_fixture` - the
  common-mode guard: a hand-transcribed `[(kind, expected_u8), ...]` table
  covering every pinned row of both tables, asserted equal to the function
  output. Membership tests prove totality; only this value fixture catches a
  kind filed into the wrong band, which the shared emitter/checker functions
  cannot self-detect.
- `src/pipeline_tests.rs` `tile_features_ordered_by_paint_rank`: this must
  exercise the SORT+MERGE path, not the encoder alone. `PendingTile.features`
  is `Vec<(u8 layer_idx, Box<[u8]> data)>` - the sort key is dropped when
  records are grouped into the tile, and `encode_tile_batch_mvt` appends that
  input order verbatim without re-sorting. Feeding the encoder adversarially
  ordered records would therefore just encode them out of order and prove
  nothing. Instead: push mixed-kind records for one tile in adversarial order
  through the chunk writer, run them through `SortReader` / the k-way merge,
  and assert the tile-grouped stream (or the decoded MVT after the full
  assemble path) is rank-sorted. Alternatively extract the sort-through-group
  helper so the test can target it with keys still attached.
- `src/verify.rs` `order_check_flags_out_of_order_land_kinds`: hand-built
  layer with forest before residential -> error surfaces. Add
  `order_check_fails_closed_on_undecodable_kind`: a targeted-layer feature
  with a missing/mistyped `kind` attr -> error, never a silent skip.

LayerMatch construction sites in tests get `paint_rank: 0` or a table value
where the test asserts ordering. These are `src/pipeline_tests.rs` (7 sites)
AND `src/pipeline/emit.rs` `landing2_tests` (the `zoom_gated_attr_parity`
fixture) - the latter is easy to miss since the spec's survey counted only
pipeline_tests.rs; the compiler's missing-field error enumerates both, and
emit.rs is already in the blast radius.

## Landing plan

Two landings, each one commit, keep/revert read on its own gates.
`brokkr check` and `elivagar verify` are green at both boundaries. Benchmark
discipline: commit first, then measure, then record numbers against the hash.
Never two elivagar processes at once - every command below runs sequentially.

Measurement-record commit sequence (resolving the ordering trap): benches run
AFTER a landing's commit, so a landing's own numbers are hash-anchored to a
commit that already exists and CANNOT be folded back into it - amending would
change the very hash the numbers describe. So each landing's
`reference/performance.md` numbers land in a LATER commit: Landing 1's numbers
ride with the Landing 2 code commit (bundled, satisfies no-markdown-alone);
Landing 2's numbers, having no following code commit here, land as a standalone
measurement-record commit - permitted because a hash-anchored bench record is
substantive markdown, the exception the markdown rule carves out. Never amend a
benched commit to insert its own numbers.

### Pre-work (before Landing 1 lands): bank the reference archive

```
brokkr tilegen --dataset denmark --variant locations
cp <OUT> data/scratch/denmark-pre-landing.pmtiles
```

(`<OUT>` = the output path the run prints, under `target/`. If Landing 1 is
already committed, produce the same archive with
`brokkr tilegen --dataset denmark --variant locations --commit <pre-hash>`.)

### Landing 1 - total record order (determinism)

Changes: the five ordering sites + `priority_from_key` + comment updates +
`merge_order_is_chunk_assignment_independent`.

Gates, in order:

1. `brokkr check` - zero clippy, all tests including the new merge-order
   test.
2. Commit (bundle any dirty markdown per repo rules).
3. **Determinism gate** (the payoff instrument for the TODO's dedup-leak and
   raw-tier claims - byte identity subsumes both):
   ```
   brokkr tilegen --dataset denmark --variant locations
   cp <OUT> data/scratch/denmark-det-run1.pmtiles
   brokkr tilegen --dataset denmark --variant locations
   cmp data/scratch/denmark-det-run1.pmtiles <OUT>
   ```
   Pass = `cmp` exits 0 silently. Contingency, resolved inline: if `cmp`
   fails, first run
   `brokkr regress --dataset denmark --file <OUT> --against data/scratch/denmark-det-run1.pmtiles`.
   Note the causal dichotomy is not clean: zero regress diffs does NOT prove
   the residue is pure byte-order. Regress has a geometry tolerance tier, so
   record-*content* nondeterminism that stays inside that tolerance also
   passes regress while failing `cmp`. So on zero regress diffs, run TWO
   checks before blaming ordering: (a) confirm the differing tiles carry the
   same feature *count* and same per-feature payload bytes (a byte-level tile
   diff, not the tolerant regress) - equal payloads with different sequence is
   the ordering-miss signature; unequal payloads at equal count is
   content nondeterminism (audit the producers, e.g. any non-positional
   synthetic id, not the sort sites). Only for the equal-payload case:
   re-audit the five ordering sites plus any new
   `sort_unstable_by_key(|r| r.key)` introduced since this survey
   (`grep sort_unstable src/sort.rs`). Nonzero regress diffs mean a semantic
   race the survey rules out; stop-and-investigate.
4. **Semantic neutrality vs pre-landing:**
   ```
   brokkr regress --dataset denmark --file <OUT> --against data/scratch/denmark-pre-landing.pmtiles
   ```
   Pass = exit 0, zero structural, zero attr diffs, zero added/missing
   features (tol 0). Order changes are invisible to regress by design.
5. **Bench neutrality** (this change is on the measured path: comparator in
   every chunk sort and merge pop):
   ```
   brokkr tilegen --bench 3 --dataset denmark --variant locations
   brokkr results --compare-last
   ```
   Baseline: denmark locations bench-3 12.0s, plantasjen, commit `acbe400`,
   results uuid `acf5ea76` (reference/performance.md). Keep bound: <= +5%
   wall (~0.60s), the noise floor performance.md sets for best-of-3 deltas
   ("deltas under ~5% are still suspect", line 220) - a +3% reading would sit
   inside that floor and could not distinguish a real regression from bench
   variance, so 3% is not a usable trip wire. Anchor the verdict to the two
   explicit UUIDs (baseline `acf5ea76` vs this landing's run), not just
   `--compare-last`, and if the delta lands in the 3-5% grey zone, re-bench
   once more before acting on it. Cost note: swapping
   `sort_unstable_by_key(|r| r.key)` for `sort_unstable_by(record_cmp)`
   forfeits the key-extraction/hoisting the by-key form gets on *every*
   comparison, not only on equal-key ties - the added indirection is broad,
   though still cheap since unequal keys settle in the u64 compare. If the
   bound is exceeded: the accepted follow-up inside this landing (not a knob,
   a code change) is comparing an inlined u64 prefix of the payload before the
   full slice compare on equal keys - still the same total order, cheaper
   equal-key path. Revert only if the bound still fails.
6. Record the bench uuid + verdict in `reference/performance.md` (rides with
   the Landing 2 commit if no other code change intervenes; markdown never
   commits alone).

### Landing 2 - paint-order key + verify instrument

Changes: `paint_order.rs`, `LayerMatch.paint_rank` + all construction sites,
matcher wiring, `push_sort_record` signature + callers, verify order check,
all named tests, TODO.md item flipped to done (rides with this commit).

Gates, in order:

1. `brokkr check` - includes the paint_order lockstep tests, the pipeline
   order test, the verify negative test, and the untouched Shortbread spec
   cases (attrs are unchanged by this landing; `paint_rank` is key-only).
2. Commit.
3. Build + archive checks:
   ```
   brokkr tilegen --dataset denmark --variant locations
   brokkr verify pmtiles --dataset denmark --tiles locations
   ```
   Pass = zero errors, INCLUDING the new order check across all tiles. Known
   gap (survey): if brokkr's verify resolution fails on this project's
   unpinned pmtiles entries, run the equivalent directly:
   `target/release/elivagar verify <OUT>`.
4. **Determinism holds with the populated byte** - repeat the Landing 1
   determinism gate verbatim (two builds, `cp`, `cmp`). Pass = byte
   identical.
5. **Semantic neutrality:**
   ```
   brokkr regress --dataset denmark --file <OUT> --against data/scratch/denmark-det-run1.pmtiles
   ```
   Pass = exit 0, zero structural/attr/feature diffs. (Reference is the
   Landing 1 archive; order is the only intended delta and regress cannot
   see it - that blindness is now load-bearing in our favor.)
6. **Earcut oracle** (standing gate for anything touching MVT feature
   streams; reordering changes which features `merge_same_attr_geometries`
   concatenates first):
   ```
   node /home/folk/Programs/elivagar/scripts/validate/earcut-oracle.mjs <OUT-absolute>
   ```
   (run with cwd `scripts/validate/` so node_modules resolves). Pass = 0
   polygons over threshold, 0 misattached holes, every polygon layer.
7. **Bench neutrality:** same invocation and bound as Landing 1 gate 5
   (key packing cost is unchanged; the verify check is off the tilegen
   path entirely).
8. **Human rendering gate** (the one gate that needs eyes, tiles named).
   The instrument is the user's OWN kind-aware tile viewer, maintained
   outside this repo - it is what surfaced the parks paint-order bug, and it
   colors `land`/`streets` features by kind, which is exactly what this gate
   needs. The user runs this gate personally (as they run the `brokkr bless`
   that follows). The former in-repo `.brokkr/preview/debug-viewer.html` is
   NOT the instrument and has been deleted from the tree (it, plus its
   `pmtiles.js`, drew every `land` feature one beige and every `streets`
   feature one gray - within-layer paint order was invisible in it, same
   blind spot as `brokkr svg`). Load `<OUT>` in the external viewer, navigate
   to central Copenhagen, tile z13/4382/2563 (12.57 E, 55.68 N) and its z14
   children. Correct looks like: Kongens Have, Orstedsparken, and
   Faelledparken render as green park fills ABOVE the residential/commercial
   landuse fill; forest patches inside Faelledparken visible above the park
   green; no park disappears when re-rendering after a fresh same-commit
   rebuild (the pre-landing failure mode). Spot-check one motorway junction
   (e.g. the Amagermotorvejen interchange, z14 around 12.52 E, 55.63 N):
   link ramps paint below the through carriageways.
9. Update `reference/performance.md` with both landings' bench uuids and the
   determinism payoff line (same-commit denmark pair: raw byte-equal tiles =
   all tiles; the ~28-tile/build dedup leak closed by construction). Per the
   measurement-record sequence above, this lands as a standalone
   substantive measurement-record commit (Landing 2's numbers describe
   Landing 2's already-created commit; do NOT amend it). Land the payoff line
   scoped to MVT+gzip - it is the format the byte-determinism claim covers.

### After both landings

The blessed archive rotation (`brokkr bless`) that would un-stale the bare
`brokkr regress` gate remains a user decision, now more attractive since
future re-blessings are byte-reproducible. Named, not performed here.

## Stopping rule and exclusions

Blast radius: `src/sort.rs` (ordering + accessor), `src/shortbread/`
(paint_order module, LayerMatch field, matcher wiring),
`src/pipeline/emit.rs` + `src/pipeline/phase12.rs` (signature threading),
`src/verify.rs` (order check), tests. Nothing else moves. Explicitly out:

- **The OSM `layer` tag** (imposm multiplies it into z_order). We do not
  read it anywhere today; consuming it is a separate TODO if wanted.
- **Area-ordered rendering within a band** (osm-carto sorts landcover by
  way_area descending; solves park-inside-forest). A kind table cannot
  express it and the u8 has room for an area bucket later - separate item.
- **Ranks for other layers.** water_polygons, buildings, sites, pois,
  boundaries, transport all stay at 0; the byte-order tiebreak makes them
  deterministic, and no defect is on record for their internal order.
  Assigning them later is a table-plus-one-matcher-line change.
- **Chunk allocation / naming / resume.** The raced allocators stay; the
  total order makes them output-irrelevant (proven by the determinism gate).
- **Wire format.** No byte changes; the payload-lex tiebreak deliberately
  uses it as-is (LE osm_id first).
- **Regress engine.** Stays order-blind; the order gate lives in verify.
- **The MLT output path.** `EncoderConfig::default()` re-sorts features
  (Morton/Hilbert/id, smallest-encoding wins), discarding paint rank, and its
  run-to-run determinism under that selection is unestablished. The
  paint-order and byte-determinism guarantees here are MVT+gzip only, matching
  the regress and earcut oracles. Making MLT honor rank (an `Unsorted`
  `SortStrategy`, or an MLT order gate) is a separate item. The TODO item this
  spec closes is scoped to MVT accordingly.
- **Feature dropping / tile size budgets** - the neighboring TODO items this
  spec does not touch.

## Measurement record obligations

This change is on the measured path (chunk sort + merge comparators), so:
baseline pinned above (12.0s denmark locations bench-3, plantasjen,
`acbe400`, uuid `acf5ea76`); each landing benches AFTER its commit with
`brokkr tilegen --bench 3 --dataset denmark --variant locations`, verdict
anchored to the two explicit UUIDs (baseline vs the landing run), not just
`brokkr results --compare-last`, under the +5% keep bound - performance.md's
own noise floor, since sub-5% best-of-3 deltas are suspect and a tighter bound
cannot separate signal from variance; a reading in the 3-5% grey zone is
re-benched before it is acted on. Hash-anchored numbers land in
`reference/performance.md` per its reading rules (best-of-3, same host, no
cross-variant reads), MVT+gzip only (the MLT path re-sorts and is out of
scope). No claims are made on norway/germany/NA
and none of their stresses (coastal fanout, way volume) intersect a
comparator whose cost scales with equal-key density, which denmark's dense
urban tiles already exercise; denmark carries the verdict.

## Review consolidation (2026-07-11)

Two reviews (R1 Opus, R2 codex gpt-5.6-sol) were validated against the tree at
`e2284ec` and folded above. Folded findings, and where:

- **MLT re-sorts, defeating the guarantee** (R2.2). New "The MLT path re-sorts"
  subsection + exclusions entry + measurement-scope note. Verified:
  `mlt.rs` `encode_layer` passes `EncoderConfig::default()`, whose
  Morton/Hilbert/id sort attempts are all `true` in mlt-core 0.12.3.
- **verify.rs decodes no tags today** (R1.1). Verify instrument reframed as a
  new MVT tag decoder. Verified: `validate_mvt_feature_geometry` reads only
  fields 3/4 and skips tags + the keys/values tables.
- **Common-mode oracle hole + fail-closed** (R2.4). Fail-closed rule + the
  independent expected-rank fixture, in the verify section and the tests.
- **Pipeline order test can't use the encoder alone** (R2.3). Test rewritten to
  go through sort+merge. Verified: `PendingTile.features` is
  `(layer_idx, data)` with the key dropped; `encode_tile_batch_mvt` appends
  input order without sorting.
- **HeapEntry PartialEq must track Ord** (R1.5 = R2.5, merged). Landing 1 site
  5. Verified: `PartialEq` compares `(key, chunk_idx)` only.
- **cmp-fail contingency dichotomy is incomplete** (R1.2). Landing 1 gate 3
  broadened to separate content-within-tolerance nondeterminism from ordering
  misses.
- **Bench framing / noise floor** (R1.6 + R2.7). +3% bound raised to the +5%
  performance.md noise floor, UUID-anchored, grey-zone re-bench; the
  key-extraction cost is noted as per-comparison, not per-tie.
- **Measurement-record commit sequence** (R2.8). Explicit hash-preserving
  strategy in the landing-plan preamble + Landing 2 gate 9.
- **Test-fixture accounting undercounts** (R1.4). `emit.rs` `landing2_tests`
  fixture added to the sites-to-update list. Verified: `emit.rs` has a
  `LayerMatch` fixture the survey's "pipeline_tests.rs 7" count omitted.
- **Human rendering gate unobservable** (R2.1 + orchestrator addendum). Gate 8
  and the survey note rewritten to name the user's external kind-aware viewer;
  the removed in-repo single-color page recorded as deleted.

Rejected:

- **R1.3 "phantom `grave_yard`"** - rejected, factually wrong. `land.rs`
  `land_match` emits `grave_yard` from `amenity=grave_yard` (the very first
  branch of the matcher), distinct from the `landuse=cemetery -> cemetery`
  entry. Band 2 listing both is correct; nothing to drop.
- **R2.6 "gates carry manual placeholders"** - rejected as already resolved.
  `<OUT>` is defined in the survey as the path each `brokkr tilegen` prints
  (brokkr computes it; there is no fixed path to hardcode), and the one
  genuinely unresolved command (`brokkr verify` on unpinned pmtiles entries)
  is already flagged with a concrete `target/release/elivagar verify <OUT>`
  fallback. No dangling placeholder remains that the spec could pin further.
