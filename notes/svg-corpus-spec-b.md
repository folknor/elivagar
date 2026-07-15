# Spec B: the canonical render core and the SVG corpus

Written against `reference/technical-implementation-spec.md` (the contract
this document must satisfy). Spawned from `notes/svg-corpus-plan.md` (the
hardened corpus plan, reviews R1+R2 folded), which is the source naming the
problem; this spec implements its "Spec B - canonical render core + corpus"
item: tier 2 (the human-viewable SVG corpus) and tier 3 (the overlay
attribution emitter), plus the calibration instruments both require.

Status of the siblings: Spec A (the digest gate: `elivagar corpus
check|bless|mutate`, `src/corpus.rs`, `corpus/denmark/`) is landed. Spec C
(the bless-machinery teardown) lands strictly after this spec calibrates and
is out of scope here. Nothing in this spec removes or weakens `brokkr
regress`, `brokkr bless`, or `data/blessed/` - the standing gate stays intact
at every landing boundary.

Failure-history note (spec contract item 8): this is geometry-adjacent work,
and the governing ledger entry is R23 in `notes/rendering-postmortem.md`. Its
lesson is load-bearing for this spec twice over: (a) the render core decodes
MVT the consumer way (ClosePath does not move the cursor - already correct in
every in-tree decoder), and (b) a ring-grouping implementation is only
trustworthy against an INDEPENDENT oracle, which is why the classifyRings
port gets a differential gate against maplibre's verbatim implementation
before any SVG is blessed. No R01-R20/S01-S09 approach is re-proposed here;
this spec renders decoded geometry and never re-derives it.

## 1. Goal

Four deliverables, one coherent structure:

1. **The canonical render core** (`src/corpus/render.rs`): a pure,
   byte-deterministic function from (decoded MVT tile, committed style) to
   SVG text. Canonical order via the regress detail comparators, MapLibre
   ring grouping via a verified classifyRings port, integer coordinates
   verbatim, style from `corpus/style.toml`.
2. **The SVG corpus** (tier 2): `corpus/<dataset>/manifest.toml` +
   `corpus/<dataset>/tiles/*.svg`, rendered at bless time, byte-compared at
   check time. The human layer of the corpus, NOT an independent detection
   gate (plan framing R1): the digest remains the sole exhaustive detector;
   the corpus's unique value is human-diffability at rotation time.
3. **The overlay attribution emitter** (tier 3): `elivagar regress <cur>
   --against <cmp> --overlay <DIR>` renders one diff SVG per differing tile
   up to `--overlay-max` (default 64) - a SAMPLED emitter, not exhaustive
   (review note SBR2 9: section 4.6 caps output at the first N differing tiles
   in `differing_ranges` order; raising `--overlay-max` widens the sample),
   drawing all four classes - unchanged, current-only, comparand-only, and
   the changed-PAIR category with BOTH geometries drawn (plan finding R2 3);
   attribute-only changes get visible old->new text, not just `<title>`.
4. **The verification instruments** the plan demands before any of the above
   is trusted: the classifyRings differential oracle
   (`elivagar corpus rings` + `scripts/validate/ring-grouping-oracle.mjs`,
   plan finding R1 2) and the render-determinism acceptance test (plan
   finding R1 3).

## 2. Survey of the ground

Verified in-tree 2026-07-15 at the current head (`340215a` + dirty roadmap
note). Everything below was re-read for this spec, not inherited from the
plan. (Review note SBR2 9: an earlier draft cited head `420534e`, which
predates Spec A's landing at `a8c4f84` - so its corpus.rs citations could
not have held at that hash. Every line/symbol citation below was re-read at
`340215a` and holds; the corpus code lives from `a8c4f84`, the denmark
baseline and calibration from `340215a`.)

### What exists and is reused

- **`src/regress.rs`** (~3280 lines) contains the canonicalization and diff
  machinery, all currently private:
  - `decode_detail_tile` -> `DetailTile { layers: Vec<DetailLayer> }`,
    layers sorted by name; `DetailLayer { name, extent, version, features }`
    with features sorted by `compare_detail_features` (id, geom_type, attrs,
    components). This IS the canonical order tier 2 needs; it exists and is
    tested (differentially, against the streaming hash, in
    `src/regress/tests.rs`).
  - `DetailFeature` carries decoded, sorted attrs
    (`Vec<(Arc<str>, DetailAttr)>`) - the styling input tier 2 needs.
  - `decode_detail_polygons` extracts rings in wire order, then groups them
    with a FIXED convention: `signed_area(ring) > 0` = outer, else hole,
    holes attached to the preceding outer. This is canonical-identity
    machinery, NOT MapLibre semantics: maplibre's classifyRings
    self-calibrates winding off the first nonzero-area ring, skips zero-area
    rings, and clamps at 500 rings. The render core must NOT reuse this
    grouping for rendering (section 3.2).
  - `compare_detail_layer` / `compare_id_group` / `compare_anonymous_group`
    / `pair_detail_features` / `classify_detail_geometry`: the pairing and
    classification engine tier 3 needs. Today it records into a concrete
    `DetailOutcome` (counts + capped events) and drops the pair identities -
    the overlay refactor (section 4.1) threads a sink trait through instead.
  - `RegressReport::dump_svg_examples` (regress.rs:324): side-by-side
    current/blessed SVG pairs for structural examples via
    `crate::svg::render_tile_svg`. Superseded by the overlay emitter and
    deleted in landing 4 (its `--svg-dump` flag with it).
- **`src/corpus.rs`** (Spec A, landed): digest compute/check/bless/mutate,
  contract guard via `provenance::extract_contract` + `contract_diff`,
  `format_guard` (MVT + gzip only), atomic writes, exit-code plumbing in
  `main.rs` (`Pass`=0, `ContentMismatch`=1, `Refused`=2). Tier 2 extends
  `check`/`bless` and adds subcommands; the digest machinery is untouched.
- **`corpus/denmark/`**: committed `digest` (leaves mode), `leaves`,
  `contract.json` (schema 1: input/config/build; blessed from the `a8c4f84`
  denmark locations archive, ocean policy_version 2).
- **`src/provenance.rs`**: `extract_contract` reads only
  `schema`/`input`/`config`/`build` from the `elivagar` object and ignores
  sibling keys - so `contract.json` can gain a top-level `"style"` key that
  corpus code reads directly without touching the provenance schema.
- **`src/svg.rs`** (441 lines): the ad-hoc viewer (`elivagar svg`, `brokkr
  svg`, consumed by `scripts/validate/svg-roi.mjs`). Arbitrary palette by
  layer encounter order, `%.1f` floats, no attrs, no ring classification,
  feature order as-encoded. Confirmed as the plan says: usable precedent,
  not extensible into the render core. It STAYS - the `svg` subcommand and
  its consumers are out of scope - but loses its one library caller when
  `dump_svg_examples` dies.
- **`scripts/validate/earcut-oracle.mjs`**: verbatim maplibre
  `classifyRings` (lines 78-106) including the two quirks the port must
  replicate exactly: the `len <= 1` early return (single-ring features skip
  BOTH the zero-area filter and calibration) and the in-place
  sort-by-|area|-descending clamp at maxRings. `calculateSignedArea` runs
  the full wraparound shoelace (j = len-1 start), so closed rings (our
  decoders append the first point on ClosePath, matching @mapbox
  loadGeometry) give identical values with or without wraparound.
- **tilepeek's style** (`research/tilepeek/src/main.rs` ~736-861): the
  MapLibre style JSON whose layer order and color/match tables seed
  `corpus/style.toml` (section 5.4). Adoption-time snapshot; drift is
  accepted and one-directional per the plan.
- **Preserved calibrands**: `data/tilegen/denmark-bc71cf1.pmtiles` (the
  stale-ocean-artifact archive; its survival is a standing prerequisite per
  the plan) and per-commit archives `denmark-{a8c4f84,d8b5147,...}.pmtiles`.
  The hard-tile ledger: z5/16/9 and z5/17/9 (2026-07-15 spikes,
  `notes/planet-30gb-roadmap.md` H5 incident), z2/2/1 (R23 ClosePath
  multi-ring ocean tile, feat id 539, `notes/rendering-postmortem.md`).
- **Determinism substrate**: std `HashMap`/`HashSet` are lint-banned
  (`disallowed_types`, clippy.toml, commit `76492b8`); `Fx` containers are
  allowed but the render core must not ITERATE any hash container into
  output (section 5.6 pins the rule; the acceptance test enforces the
  outcome).
- **Sizing** (plan measurement, denmark `d8b5147`, %.1f-float renderer):
  z14 Copenhagen full-layer 1.2 MB, z10 1.5 MB, z5 127 KB. Integer emission
  only shrinks these. Manifest budget: low tens of MB committed.
- **Dependencies**: no TOML parser in tree; `serde` derive is
  dev-dependencies only. Landing 2 adds `toml` and promotes `serde`
  (section 4.2).

### What depends on the touched surface

- `src/corpus.rs` is consumed only by `main.rs` (the `corpus` subcommand)
  and its own tests. Converting it to `src/corpus/mod.rs` changes no import
  paths (`crate::corpus::*` throughout).
- `regress::regress()` is consumed by `main.rs::run_regress`. The sink
  refactor keeps its signature; `dump_svg_examples` + `--svg-dump` is the
  only externally visible removal, replaced by `--overlay` in the same
  landing so the attribution capability never has a gap.
- `regress::{DecodeScratch, semantic_hash, next_zoom_boundary}` are already
  `pub(crate)` for corpus.rs; widening more items to `pub(crate)` follows
  the established pattern.

## 3. Decisions resolved inline

Obstacles the plan left open, each closed here so implementation discovers
nothing.

### 3.1 Paint order: style-order, not name-order (deviation from plan wording)

The plan says "layers sorted by name". Alphabetical z-order paints water
fills over streets and boundaries under everything, which defeats the
"does this render look right" human gate the corpus exists for. The
requirement behind the plan's wording is DETERMINISM, and a committed,
explicit draw order satisfies it strictly better: `corpus/style.toml`'s
`[[layer]]` table order IS the paint order (bottom first), exactly like a
MapLibre style's layer array, and it is hashed into the corpus contract. A
decoded layer absent from the style renders ON TOP of all styled layers, in
name order, with the fallback style, and is reported as a warning by
`check`/`bless`/`render-manifest` ("unstyled layer <name>") - loud, never
silent, and still deterministic. Within a layer, features stay in canonical
`compare_detail_features` order; within a feature, polygons in classifyRings
order, rings in wire order. Nothing else in the plan's canonical form
changes.

### 3.2 Ring grouping: verbatim classifyRings over wire-order rings

The renderer groups each polygon feature's rings with a Rust port of
maplibre's classifyRings run over the rings IN WIRE ORDER - not over
regress's canonically-grouped components. Rationale: classifyRings assigns
holes by encounter order and calibrates winding off the first nonzero ring,
so only wire order reproduces what MapLibre actually draws; wire ring order
within a feature is part of the geometry (the canonicalization tier erases
intra-layer FEATURE order and nothing else), so it is stable across
legitimate rebuilds and safe for byte-determinism. The port replicates the
verbatim semantics exactly: `len <= 1` early return (no filter, no
calibration), zero-area rings skipped (full-wraparound integer shoelace,
i128), first nonzero ring
defines outer winding, matching-wound rings start polygons, opposite-wound
append to the current polygon, maxRings=500 clamp keeping the largest rings
by |area| via a STABLE sort descending (JS sort is stable per ES2019). A
clamped feature is flagged: `data-clamped="N"` on the emitted paths and a
warning line in every corpus command that renders it ("cannot match both
viewers", per the plan). The port is gated by the differential oracle
(section 6, landing 2) before any SVG is blessed - plan finding R1 2.

Review note SBR1 1 (the i128-vs-f64 equivalence is bounded, not universal):
maplibre's `calculateSignedArea` sums `(p2.x - p1.x) * (p1.y + p2.y)`. A
single product of arbitrary i32 coordinates reaches ~2^64 and the running
sum can exceed 2^53, above which f64 rounds and i128 does not. The
node-side (f64) and Rust-side (i128) shoelace therefore agree bit-for-bit
only while the running sum stays under 2^53 - true for real in-extent
denmark tile coordinates (bounded to a few thousand plus buffer), so the
denmark differential gate is sound. It is NOT true for the arbitrary large
i32 coordinates section 5.1 permits ("negatives legal; outside-viewBox
content is clipped"): a pathological tile could flip a near-zero area sign
(and with it outer/hole calibration) or the clamp-sort magnitude between the
two sides. The gate's "any divergence is a port bug by definition" (section
6, landing 2) rests on denmark coordinates staying within the 2^53 bound,
not on universal bit-identity; a divergence on out-of-bound coordinates is
an f64/i128 magnitude artifact, not a port bug, and denmark never exercises
it.

Review note SBR1 3 (the clamp reorders rings, so "wire order" is not
unconditional): the maxRings clamp runs `polygons[j].sort((a,b)=>b.area-a.area)`
BEFORE slicing (earcut-oracle.mjs:101-102), so a clamped polygon's surviving
rings are in |area|-descending (then truncated) order, NOT wire order. The
Rust `classify_rings` must reproduce that exact stable reorder-and-truncate.
The "rings in wire order within the polygon" wording in sections 5.1 and 5.5
holds only for UNCLAMPED polygons (>500 rings in one polygon is required to
clamp, and such a feature already carries `data-clamped`); for a clamped
polygon the emitted and dumped ring order is the area-sorted survivor set.

### 3.3 The overlay draws from DetailFeature components, not the render decode

Tier 3's job is attribution, not viewer fidelity. The diff engine hands the
sink `&DetailFeature` pairs; their `components` (rings + roles) are exactly
the geometry that was compared, and drawing them directly avoids any
index-mapping between the detail decode and a second render decode. The
overlay shares the render core's low-level SVG path-data emitter
(`path_data`, section 5.1) and the style file's background/frame, nothing
else. classifyRings is not involved in overlays.

### 3.4 Style file location and the explicitness rule

One style file for all datasets: `corpus/style.toml`, committed. Commands
that touch it take `--style <PATH>` with default `corpus/style.toml`. This
default does not violate "either it is explicit, or it is not set": the
recorded contract carries the style file's xxh3-128, and `check` REFUSES on
any mismatch between the on-disk style and the blessed hash, so the default
can never silently mean two different things - the failure mode the rule
exists to prevent (a run whose meaning lived in the filesystem) is
structurally impossible here. `contract.json` gains a top-level key:

```json
"style": { "path": "corpus/style.toml", "xxh3_128": "<32 hex>" }
```

written by `bless`/`render-manifest` whenever a manifest exists, ignored by
`provenance::extract_contract` (verified: it reads only
schema/input/config/build), read and enforced by corpus code. A style edit
is: edit `corpus/style.toml`, run `corpus render-manifest`, commit the
re-render (zero digest delta) - exactly the plan's workflow.

Review note SBR1 2 (the style-key write/read path is NOT free plumbing): the
on-disk `contract.json` is produced by `contract_text` (corpus.rs:306),
which rebuilds the WHOLE document from a `ContractDoc` -
`json!({"schema":1,"input":..,"config":..,"build":..})` - and is read back
through `contract_from_file` -> `extract_contract`, which returns only
input/config/build and discards every other key. So "other keys untouched"
is misleading: a naive rewrite through today's serializer would DROP a style
key on every pass. The landing must (a) extend the contract writer to carry
and re-emit the `style` object alongside the four existing keys (a
surgical-edit-in-place is not what happens; the document is rebuilt from a
struct, so the struct/serializer gain the field), and (b) read the style
hash back through a SEPARATE parse of the raw JSON, since `extract_contract`
will not surface it. `extract_contract` ignoring the key remains the correct
safety argument (the provenance schema is untouched); it just does not do
the reading for us.

### 3.5 Manifest accretion without bless rotation

`corpus bless` refuses to overwrite an existing digest without `--rotate`,
which would block the plan's append-only manifest accretion. Resolution: a
separate `corpus render-manifest` subcommand renders every manifest entry
from an archive and updates the contract's `style` key, but FIRST runs the
full digest check against the committed baseline and refuses unless it
passes - so corpus SVGs can only ever be (re)rendered from content that
matches the blessed digest, and accretion/style edits never touch the
digest files. `bless` calls the same internal path after writing the
digest, so a rotation re-renders everything in one command.

### 3.6 Remaining small closures

- **Extent**: viewBox is `0 0 4096 4096`. A layer with extent E != 4096 is
  wrapped in `<g transform="scale(Q)">` where Q = 4096/E, emitted only when
  integral; a non-integral scale is a categorical refusal naming the tile
  and layer (our encoder always emits 4096; this is a correctness statement,
  not an expected path). No floats anywhere.
- **Absent tile**: a manifest tile not addressed by the archive renders the
  background-only SVG. Deterministic; a tile that disappears diffs loudly.
- **Orphans**: a file in `corpus/<dataset>/tiles/` not derived from the
  manifest is a `check` content mismatch (named), and `render-manifest`
  deletes it. The tiles directory is exactly the manifest render, always.
- **Attrs in corpus SVGs**: not emitted. The digest owns attribute
  bit-exactness; tier 2 is the human layer, and style-relevant attr changes
  already surface as paint changes. (Overlays DO render attr text - that is
  attribution, tier 3.)
- **Empty-manifest bless**: legal; the tier-2 machinery engages only when
  `manifest.toml` exists in the corpus dir.
- **Mixed geometry in a styled layer**: rendering is driven by the feature's
  geom_type, not a layer "type": polygons fill, lines stroke, points become
  circles. Label layers therefore render their (point or line) geometry -
  the corpus checks geometry fidelity, not cartography.

## 4. Target structure

### 4.1 Module and code moves

```
src/corpus.rs            -> src/corpus/mod.rs      (digest machinery, unchanged;
                                                    check/bless extended, sec 5.7)
src/corpus/style.rs      NEW  style load/parse/hash/resolve
src/corpus/manifest.rs   NEW  manifest load/parse/validate
src/corpus/render.rs     NEW  render core: decode, classifyRings port,
                              canonical SVG emission, rings dump
src/corpus/overlay.rs    NEW  tier-3 overlay SVG emission
```

`src/regress.rs` changes (all internal, output-neutral):

- Widen to `pub(crate)`: `DetailTile`, `DetailLayer`, `DetailFeature`,
  `DetailAttr`, `DetailComponent`, `DetailRing`, `OutcomeClass`,
  `decode_detail_tile`, `compare_detail_features`, `compare_detail_layer`,
  `RegressConfig` stays pub. **Widen the FIELDS too, not just the types**
  (review note SBR2 2): `DetailFeature`, `DetailComponent`, `DetailRing`,
  `DetailLayer`, `DetailTile` currently have private fields (regress.rs:1193
  onward), so a bare `pub(crate)` on the type leaves `corpus::overlay` and
  `corpus::render` unable to read the geometry/attrs they must clone and
  emit. Every field these two modules read (`DetailFeature.id`/`geom_type`/
  `attrs`/`components`, `DetailComponent.rings`, `DetailRing.role`/`points`,
  `DetailLayer.name`/`extent`/`features`, `DetailTile.layers`) is widened to
  `pub(crate)`, or the types grow `pub(crate)` accessors. Fields carrying
  private helper types not needed downstream (`attrs_digest`, `bbox`,
  `structure`, `digest`) stay private.
- Factor the feature-message parse out of `decode_detail_feature`:

```rust
pub(crate) struct RawFeature {           // owns its bytes; NO lifetime param
    pub id: Option<u64>,
    pub tag_bytes: Vec<u8>,
    pub geom_type: u8,
    pub geometry: Option<Vec<u8>>,
}
pub(crate) fn parse_feature_message(data: &[u8]) -> Result<RawFeature, String>;
```

  (review note SBR2 2: the earlier `RawFeature<'a>` / `-> RawFeature<'_>` is a
  compile error - the struct owns all four fields and never borrows `data`,
  so a lifetime parameter is unused, E0392. Drop it.)

  and the wire-order geometry walkers out of the detail decoders:

```rust
pub(crate) fn decode_polygon_rings(data: &[u8]) -> Result<Vec<Vec<(i32, i32)>>, String>;
pub(crate) fn decode_line_paths(data: &[u8])   -> Result<Vec<Vec<(i32, i32)>>, String>;
pub(crate) fn decode_point_runs(data: &[u8])   -> Result<Vec<(i32, i32)>, String>;
```

  `decode_detail_polygons` becomes `decode_polygon_rings` + the existing
  fixed-convention grouping; byte-for-byte identical results (the
  differential hash tests in `src/regress/tests.rs` gate this).
- Thread a sink trait through the diff engine in place of the concrete
  `DetailOutcome` parameter:

```rust
pub(crate) trait DiffSink {
    fn record(&mut self, layer: &Arc<str>, class: OutcomeClass, displacement: i32,
              current: Option<&DetailFeature>, blessed: Option<&DetailFeature>);
    fn matched(&mut self, _layer: &Arc<str>, _current: &DetailFeature,
               _blessed: &DetailFeature) {}
    // Pair-aware layer event: BOTH sides optional, so the collector can
    // retain each side's extent/version (review note SBR2 3).
    fn layer_event(&mut self, _class: OutcomeClass,
                   _current: Option<&DetailLayer>, _blessed: Option<&DetailLayer>) {}
}
```

  `DetailOutcome` implements it by reproducing today's behavior exactly
  (record -> counts + events, deriving the event id from
  `current.or(blessed)`; `matched` no-op; `layer_event` -> today's single
  LayerAdded/LayerRemoved/ExtentMismatch event, taking the name from
  whichever side is present). `classify_detail_geometry`
  calls `sink.matched(...)` on its distance==0 early return. Call sites
  (`compare_detail_layer`, `compare_id_group`, `compare_anonymous_group`)
  become generic over `&mut impl DiffSink`; monomorphization keeps the hot
  path zero-cost.

Review note SBR2 3 (the layer event MUST be pair-aware, and the extent path
skips comparison): today's top-level layer walk records a SINGLE
`ExtentMismatch` event carrying only `cur.name` and then advances BOTH
indices WITHOUT calling `compare_detail_layer` (regress.rs:2116-2120) - so a
version/extent difference stops the per-feature comparison for that layer
entirely. A `layer_event(class, &DetailLayer)` that receives one side cannot
retain both extents/versions, and an overlay driver that then only runs
`compare_detail_layer` (which assumes compatible layers) would emit an empty
or mis-scaled overlay for a real extent regression. Hence `layer_event`
carries both optional layers, and the overlay collector pins per-side extent
normalization (each side wrapped in its own `scale(4096/extent)` group,
section 3.6) so mismatched-extent layers draw at a common 4096 space.

Review note SBR1 4 (the grey-vs-both split is real and routes through the
existing class): the overlay renders matched-unchanged in grey and
tolerance/structural pairs with both geometries drawn. That distinction is
already carried by the diff engine: `classify_detail_geometry` returns
without recording on its distance==0 early return (regress.rs:2748 - this is
where `sink.matched()` is added), and otherwise records
`OutcomeClass::ToleranceMoved` for `distance <= cfg.tol` else
`StructuralMoved` (regress.rs:2751-2756). So the collector classifies grey
(via `matched`) vs tolerance-moved vs structural-moved cleanly; no lumping.

Review note SBR2 8 (AttrChanged can also hide a geometry change): ID matching
records `AttrChanged` and STOPS - it never classifies geometry when attrs
differ (regress.rs:2290-2294, the `if cur.attrs != bl.attrs` short-circuit).
So an `AttrChanged` pair may ALSO have moved geometry. The overlay must not
draw such a pair's geometry only once in orange (that hides the geometry
regression): it draws BOTH sides (current pink, comparand blue) whenever the
two geometries differ, and additionally emits the orange attr panel text.
Geometry-equality for this decision is the `geometry_digest` the detail
feature already carries, so no extra comparison pass is needed.
- Delete `RegressReport::dump_svg_examples` (landing 4, together with its
  `--svg-dump` flag and the `--overlay` replacement).

### 4.2 Dependencies (`Cargo.toml`)

- Add `toml = "0.9"` to `[dependencies]`.
- Move `serde = { version = "1", features = ["derive"] }` from
  dev-dependencies to `[dependencies]` (style/manifest structs derive
  `Deserialize`).
- `Cargo.lock` changes ride with the landing commit.

### 4.3 Render core types (`src/corpus/render.rs`)

```rust
pub struct RenderTile { pub layers: Vec<RenderLayer> }
pub struct RenderLayer {
    pub name: Arc<str>,
    pub extent: u32,
    pub features: Vec<RenderFeature>,      // canonical order (compare_detail_features)
}
pub struct RenderFeature {
    pub id: Option<u64>,
    pub geom_type: u8,                     // 1 point, 2 line, 3 polygon
    pub attrs: Vec<(Arc<str>, DetailAttr)>,// sorted (detail decode)
    pub wire_paths: Vec<Vec<(i32, i32)>>,  // rings/paths in WIRE order,
                                           // ClosePath appends first point
}

pub fn decode_render_tile(data: &[u8]) -> Result<RenderTile, String>;
// per feature: parse_feature_message once; decode_detail_attrs; wire walkers
// for wire_paths; the detail component grouping ONLY as the sort key, then
// dropped. Features sorted by compare_detail_features (stable sort; ties
// keep wire order, which both this and DetailLayer share, so the two
// decodes of one payload always agree on order).

/// Verbatim maplibre classifyRings, section 3.2 semantics.
/// Returns polygons as index groups into `rings`, plus rings_clamped count
/// (0 when no clamp fired).
pub fn classify_rings(rings: &[Vec<(i32, i32)>]) -> (Vec<Vec<usize>>, u32);

pub struct RenderedSvg { pub bytes: Vec<u8>, pub warnings: Vec<String> }
// z/x/y are required, not optional (review note SBR2 2): the canonical form
// emits `<title>{z}/{x}/{y}</title>` (section 5.1) and a non-integral extent
// is a refusal that NAMES the tile and layer (section 3.6). A RenderTile
// carries no coordinates, so they are passed in.
pub fn render_svg(tile: &RenderTile, z: u8, x: u32, y: u32, style: &Style,
                  layers: Option<&[String]>) -> Result<RenderedSvg, String>;

/// Shared low-level emitter (also used by overlay.rs):
/// "M{x} {y} L{x} {y} ..." with " Z" per closed ring; integers only.
pub(crate) fn path_data(paths: &[&[(i32, i32)]], close: bool) -> String;

/// Grouping dump for the differential oracle (section 5.8).
pub fn dump_ring_grouping(archive: &ArchiveView, out: &mut dyn io::Write)
    -> io::Result<()>;
```

### 4.4 Style types (`src/corpus/style.rs`)

```rust
#[derive(Deserialize)]
pub struct StyleFile { pub background: String, pub layer: Vec<StyleLayer> }
#[derive(Deserialize)]
pub struct StyleLayer {
    pub name: String,
    #[serde(default)] pub r#match: Vec<StyleMatch>,
    #[serde(flatten)] pub paint: Paint,
}
#[derive(Deserialize)]
pub struct StyleMatch {
    pub key: String,
    pub value: toml::Value,                // string / integer / boolean
    #[serde(flatten)] pub paint: Paint,
}
#[derive(Deserialize, Default, Clone)]
pub struct Paint {                          // every field verbatim-emitted
    pub fill: Option<String>,
    pub fill_opacity: Option<String>,
    pub stroke: Option<String>,
    pub stroke_width: Option<String>,
    pub stroke_dasharray: Option<String>,
    pub stroke_opacity: Option<String>,
    pub point_radius: Option<u32>,          // default 4 at resolution time
}

pub struct Style { /* parsed file + file hash */ }
impl Style {
    pub fn load(path: &Path) -> io::Result<Style>;       // parse + xxh3_128
    pub fn hash_hex(&self) -> &str;
    /// paint order = file order; None for a layer not in the file
    pub fn position(&self, layer: &str) -> Option<usize>;
    /// base paint overlaid by the FIRST matching rule, file order.
    /// Match: attr string == value string; Int/UInt/SInt == value integer;
    /// Bool == value boolean. Float/Double never match.
    pub fn resolve(&self, layer: &str,
                   attrs: &[(Arc<str>, DetailAttr)]) -> Paint;
    pub fn fallback() -> Paint;              // magenta #ff00ff, the unstyled flag
}
```

All emitted paint values are verbatim strings from the TOML - no numeric
formatting of style values exists anywhere in the render core.

### 4.5 Manifest types (`src/corpus/manifest.rs`)

```rust
#[derive(Deserialize)]
pub struct ManifestFile { pub tile: Vec<ManifestTile> }
#[derive(Deserialize)]
pub struct ManifestTile {
    pub z: u8, pub x: u32, pub y: u32,
    #[serde(default)] pub layers: Vec<String>,   // empty = all layers
    #[serde(default)] pub note: String,          // provenance for humans
}
pub fn load(path: &Path) -> io::Result<ManifestFile>;
// validation: z <= 14, x/y in range for z, layers strictly ascending
// (enforced so the derived filename is canonical), no duplicate
// (z, x, y, layers) entries.
// Filename-safety validation (review note SBR2 6): each layer name must match
// a restricted grammar [a-z0-9_]+ (the Shortbread layer names all do). This
// rejects, categorically:
//   - path separators and traversal ("../outside" escaping the tiles dir),
//   - the '+' join ambiguity, where ["a+b"] and ["a","b"] would collide.
// AND `load` checks that the FINAL derived filenames are unique across all
// entries - the (z,x,y,layers)-tuple duplicate check is not sufficient,
// since distinct tuples could still map to one filename; the render target
// is the filename, so the filename is what must not collide.
pub fn file_name(t: &ManifestTile) -> String;
// no layers: "z{z}-x{x}-y{y}.svg"
// layers:    "z{z}-x{x}-y{y}-{layers joined by '+'}.svg"
// e.g. z5-x16-y9-boundaries+ocean.svg
// ('+' is safe as a join only because layer names are validated to exclude
// it and every path/traversal character; see load's grammar above.)
```

### 4.6 Overlay emitter (`src/corpus/overlay.rs`)

```rust
pub struct OverlayCollector { /* implements regress::DiffSink; owns clones of
    the geometry + attrs it needs, grouped by class */ }
pub fn render_overlay(collector: &OverlayCollector, style_background: &str)
    -> Vec<u8>;
```

Driver in `regress.rs`:

```rust
pub fn dump_overlays(current: &Path, against: &Path, report: &RegressReport,
                     cfg: &RegressConfig, dir: &Path, max: usize)
    -> io::Result<u64>;   // returns overlays written
```

iterates `report.differing_ranges` in order, expands tile ids, takes the
first `max` (default 64, `--overlay-max`), and for each tile: reads both
blobs (absent side = empty tile), decodes both `DetailTile`s, matches layers
by name (unmatched -> `layer_event`), runs `compare_detail_layer` with an
`OverlayCollector`, writes `dir/z{z}-x{x}-y{y}.svg`.

Overlay SVG form (canonical, like the corpus renders):

- viewBox `0 0 4096 5120`: the 4096-square tile area plus a 1024-tall text
  panel below.
- Classes and colors (each geometry gets `data-class` and a `<title>`):
  - matched-unchanged: `#999999`, opacity `0.25` (fills and strokes alike)
  - current-only (added, incl. layer-added layers): `#e91e63`
  - comparand-only (removed, incl. layer-removed layers): `#2196f3`
  - changed pair (tolerance-moved, structural-moved, geom-type change):
    BOTH geometries drawn - current `#e91e63`, comparand `#2196f3`,
    opacity `0.7`, `data-class="tolerance-moved"` /
    `"structural-moved"` and the displacement in the `<title>`
  - attr-changed pair: if the two geometries are equal (`geometry_digest`
    match) the geometry draws once in `#ff9800`; if they ALSO differ, both
    sides draw (current `#e91e63`, comparand `#2196f3`) as for a changed
    pair, so a simultaneous geometry regression is never hidden (review note
    R2 8). Either way, one text line per changed key in the panel:
    `layer id=<id> key: <old> -> <new>` (canonical attr formatting,
    `DetailAttr` debug-stable form pinned in code), capped
    at 24 lines + a `(+N more)` line
- Panel also carries a fixed legend line naming the four colors. Monospace,
  `font-size="20"`, `line-height` 24px, all text content deterministic. The
  panel is 1024px tall (viewBox `0 0 4096 5120`); at 24px/line a full panel
  is 24 attr lines + `(+N more)` + legend = 26 lines ~= 624px, comfortably
  inside 1024 (review note SBR1 5: `font-size="40"` at ~44px/line overflowed
  the panel by ~120px - SVG does not clip unless told, so it bled visually).

### 4.7 CLI surface (`src/main.rs`)

```
elivagar corpus check  <ARCHIVE> --corpus <DIR> [--style <PATH>]      (extended)
elivagar corpus bless  <ARCHIVE> --corpus <DIR> [--style <PATH>] ...  (extended)
elivagar corpus render-manifest <ARCHIVE> --corpus <DIR> [--style <PATH>]  (new)
elivagar corpus render <ARCHIVE> -z Z -x X -y Y [--layers a,b]
                       [--style <PATH>] [-o OUT]                      (new)
elivagar corpus rings  <ARCHIVE> -o <OUT>                             (new)
elivagar regress <CUR> --against <CMP> [--overlay <DIR>] [--overlay-max N]
                                                    (replaces --svg-dump)
```

`--style` defaults to `corpus/style.toml` (section 3.4). Exit codes
unchanged: 0/1/2. `corpus render` and `corpus rings` are contract-free
(format guard only) - `render` exists for manifest seeding and for the
human calibration against archives whose contract differs (the bc71cf1
reading, section 6). Limitation (review note SBR1 7): the non-integral-extent
refusal (section 3.6) means `corpus render` on a FOREIGN archive whose
extent E does not divide 4096 refuses rather than renders (Q = 4096/E must
be an integer: E=2048 gives Q=2 and renders; the common E=8192 gives Q=0.5
and is refused). Our encoder always emits 4096, so
in-project archives are unaffected; this bounds only the human-calibration
use against arbitrary outside archives, and is acceptable under the
same-project scope.

## 5. The canonical artifacts, pinned

### 5.1 Corpus SVG canonical form

Exact emission, in order (no XML declaration; `\n` line ends; two-space
indent; UTF-8):

```
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 4096 4096" width="512" height="512">
  <title>{z}/{x}/{y}</title>
  <rect width="4096" height="4096" fill="{style.background}"/>
  <g id="{layer}">                          <!-- one per rendered layer,
                                                 style paint order -->
    ...feature elements...
  </g>
</svg>
```

No commit hash, dataset name, timestamp, or any other volatile content -
per-SVG files stay contract-free so an input rotation does not rewrite
unchanged renders (plan, contract section).

Feature elements, by geom_type, with `i` = canonical feature index in the
layer and `j` = polygon index:

- polygon (3): one `<path>` PER classifyRings polygon:
  `<path id="{layer}-f{i}-p{j}" d="{rings as M/L/Z subpaths, wire ring
  order within the polygon}" fill="{paint.fill}" fill-rule="nonzero"
  [fill-opacity=...] [stroke=... stroke-width=...] [data-clamped="N"]/>`
- line (2): one `<path>` per feature, sub-lines as M/L runs, no Z:
  `<path id="{layer}-f{i}" d="..." fill="none" stroke="{paint.stroke or
  paint.fill}" stroke-width="{paint.stroke_width or 1}" [dasharray,
  opacity]/>`
- point (1): one `<circle id="{layer}-f{i}-p{k}" cx=".." cy=".." r="{
  point_radius}" fill="{paint.fill or paint.stroke}"/>` per point.

All coordinates are the decoded i32 values verbatim (negatives legal;
outside-viewBox content is clipped by the viewer, matching tile buffer
semantics). A paint with no applicable color uses `Style::fallback()`
magenta - visible, never silent.

XML escaping (review note SBR2 7): every value interpolated into the SVG that
is not a pinned literal or a formatted integer - style paint strings, layer
names, feature ids in `id=`/`<title>`, the `{z}/{x}/{y}` title, and (in
overlays) the old->new attribute text - is escaped, because a `&`, `<`, `>`,
`"`, or `'` in any of them would otherwise produce malformed SVG or inject
markup/attributes. Two pinned helpers do this: `xml_text` (escapes `&`,`<`,
`>` for element text and `<title>` bodies) and `xml_attr` (adds `"` and `'`
for attribute values). Both are deterministic. Layer/attr keys come from the
tile and are attacker-influenceable in principle, so the escaping is
enforced, not assumed, and unit-tested against hostile style/layer/attr
values carrying each metacharacter.

### 5.2 Manifest format

Section 4.5's schema. Layer lists strictly ascending; the derived filename
is the only name a render is ever written to or compared against.

### 5.3 Seeded manifest (`corpus/denmark/manifest.toml`)

Representative classes plus the hard-tiles ledger, per the plan. The x/y
values below are computed from the denmark bbox and MUST be verified at
seeding time by rendering each candidate (`elivagar corpus render ...`) and
eyeballing that it exhibits its class; adjusting a coordinate by +-1 during
seeding is within this spec. Accretion afterward is append-only.

| z | x | y | layers | class / provenance |
|---|---|---|---|---|
| 5 | 16 | 9 | boundaries, ocean | 2026-07-15 stale-artifact spike (H5 incident) |
| 5 | 17 | 9 | boundaries, ocean | 2026-07-15 stale-artifact spike (H5 incident) |
| 2 | 2 | 1 | ocean | R23 ClosePath multi-ring class (feat id 539) |
| 7 | 68 | 39 | (all) | open water, Kattegat - empty-ocean class |
| 10 | 535 | 318 | boundaries, land, ocean | west Jutland coast |
| 10 | 538 | 324 | boundaries | DK-DE boundary junction |
| 11 | 1084 | 647 | land, ocean, water_polygons | South Funen island cluster |
| 12 | 2150 | 1275 | (all) | rural mid-Jutland |
| 14 | 8764 | 5132 | (all) | dense Copenhagen z14; the one budgeted ~1.2 MB entry |

Every future fixed visual bug drops its tile in (append-only), with `note`
naming the incident.

### 5.4 `corpus/style.toml` (committed, complete)

Full content to commit (paint order bottom-to-top; colors from tilepeek's
embedded style where it has an opinion, neutral picks elsewhere; label
layers render their raw geometry as small marks - geometry fidelity, not
cartography):

```toml
# corpus/style.toml - canonical corpus render style.
# [[layer]] order IS the paint order, bottom first. Hashed into
# corpus/<dataset>/contract.json; edits require `corpus render-manifest`.
background = "#f2efe9"

[[layer]]
name = "ocean"
fill = "#aad3df"

[[layer]]
name = "water_polygons"
fill = "#aad3df"
[[layer.match]]
key = "kind"
value = "glacier"
fill = "#f8fbfb"

[[layer]]
name = "land"
fill = "#f2efe9"
[[layer.match]]
key = "kind"
value = "forest"
fill = "#add19e"
[[layer.match]]
key = "kind"
value = "farmland"
fill = "#eef0d5"
[[layer.match]]
key = "kind"
value = "grass"
fill = "#cdebb0"
[[layer.match]]
key = "kind"
value = "meadow"
fill = "#cdebb0"
[[layer.match]]
key = "kind"
value = "park"
fill = "#c8facc"
[[layer.match]]
key = "kind"
value = "garden"
fill = "#cdebb0"
[[layer.match]]
key = "kind"
value = "residential"
fill = "#e0dfdf"
[[layer.match]]
key = "kind"
value = "industrial"
fill = "#ebdbe8"
[[layer.match]]
key = "kind"
value = "commercial"
fill = "#ebdbe8"
[[layer.match]]
key = "kind"
value = "retail"
fill = "#f0d9d9"
[[layer.match]]
key = "kind"
value = "cemetery"
fill = "#aacbaf"
[[layer.match]]
key = "kind"
value = "wetland"
fill = "#d4e6c8"
[[layer.match]]
key = "kind"
value = "sand"
fill = "#f5e9c6"
[[layer.match]]
key = "kind"
value = "heath"
fill = "#d6d99f"
[[layer.match]]
key = "kind"
value = "scrub"
fill = "#c8d7ab"
[[layer.match]]
key = "kind"
value = "orchard"
fill = "#aedfa3"
[[layer.match]]
key = "kind"
value = "vineyard"
fill = "#aedfa3"
[[layer.match]]
key = "kind"
value = "allotments"
fill = "#c9e1bf"

[[layer]]
name = "sites"
fill = "#e0dfcc"
fill_opacity = "0.3"

[[layer]]
name = "buildings"
fill = "#d9d0c9"
fill_opacity = "0.6"

[[layer]]
name = "water_lines"
stroke = "#aad3df"
stroke_width = "1"

[[layer]]
name = "ferries"
stroke = "#7db3c9"
stroke_width = "1"
stroke_dasharray = "6 3"

[[layer]]
name = "dam_polygons"
fill = "#cccccc"

[[layer]]
name = "dam_lines"
stroke = "#888888"
stroke_width = "2"

[[layer]]
name = "pier_polygons"
fill = "#d9d0c9"

[[layer]]
name = "pier_lines"
stroke = "#d9d0c9"
stroke_width = "1"

[[layer]]
name = "bridges"
fill = "#e8e4df"
fill_opacity = "0.5"

[[layer]]
name = "public_transport"
fill = "#b8b8b8"
fill_opacity = "0.3"

[[layer]]
name = "street_polygons"
fill = "#cccccc"

[[layer]]
name = "streets"
stroke = "#dddddd"
stroke_width = "1"
[[layer.match]]
key = "kind"
value = "motorway"
stroke = "#e892a2"
stroke_width = "6"
[[layer.match]]
key = "kind"
value = "trunk"
stroke = "#f9b29c"
stroke_width = "5"
[[layer.match]]
key = "kind"
value = "primary"
stroke = "#fcd6a4"
stroke_width = "4"
[[layer.match]]
key = "kind"
value = "secondary"
stroke = "#f7fabf"
stroke_width = "3"
[[layer.match]]
key = "kind"
value = "tertiary"
stroke = "#ffffff"
stroke_width = "2.5"
[[layer.match]]
key = "kind"
value = "residential"
stroke = "#ffffff"
stroke_width = "2"
[[layer.match]]
key = "kind"
value = "living_street"
stroke = "#ededed"
stroke_width = "2"
[[layer.match]]
key = "kind"
value = "service"
stroke = "#ededed"
stroke_width = "1"
[[layer.match]]
key = "kind"
value = "track"
stroke = "#ccbbaa"
stroke_width = "1"
[[layer.match]]
key = "kind"
value = "path"
stroke = "#ccbbaa"
stroke_width = "1"
[[layer.match]]
key = "kind"
value = "footway"
stroke = "#fa8072"
stroke_width = "1"
[[layer.match]]
key = "kind"
value = "cycleway"
stroke = "#0000ff"
stroke_width = "1"

[[layer]]
name = "aerialways"
stroke = "#888888"
stroke_width = "1"

[[layer]]
name = "boundaries"
stroke = "#9e7bba"
stroke_width = "1"
stroke_dasharray = "4 2"
[[layer.match]]
key = "admin_level"
value = 2
stroke_width = "2"
[[layer.match]]
key = "admin_level"
value = 4
stroke_width = "1.5"

[[layer]]
name = "addresses"
fill = "#b6b6b6"
point_radius = 2

[[layer]]
name = "streets_polygons_labels"
fill = "#333333"
point_radius = 3

[[layer]]
name = "street_labels_points"
fill = "#333333"
point_radius = 3

[[layer]]
name = "street_labels"
stroke = "#888888"
stroke_width = "1"
fill = "#888888"
point_radius = 3

[[layer]]
name = "water_polygons_labels"
fill = "#4a80a0"
point_radius = 3

[[layer]]
name = "water_lines_labels"
stroke = "#4a80a0"
stroke_width = "1"
fill = "#4a80a0"
point_radius = 3

[[layer]]
name = "boundary_labels"
fill = "#9e7bba"
point_radius = 3

[[layer]]
name = "place_labels"
fill = "#d63333"
point_radius = 4

[[layer]]
name = "pois"
fill = "#666666"
point_radius = 2
```

All 26 Shortbread layers appear; the fallback path exists only for schema
drift, and firing it is a warning.

### 5.5 Ring-grouping dump format (both sides byte-identical)

One line per polygon feature (geom_type 3), unique payloads only:

```
{z}/{x}/{y} {layer} {featIdx} {groups}
```

`featIdx` indexes ALL features in the layer in WIRE order (matching
`layer.feature(i)` on the node side). Note this is a DIFFERENT index space
from the corpus SVG's feature id `i`, which is the canonical
`compare_detail_features` index (section 5.1) - the dump is a raw wire-order
walk for a byte-`cmp` against verbatim maplibre, the SVG is canonical-order
for human diffing; the two indices are intentionally not the same number and
must not be conflated (review note SBR1 6). `groups`: polygons joined by `|`,
each polygon its ring vertex counts joined by `+`, in grouping order (which
for a clamped polygon is the area-descending survivor order, section 3.2
review note SBR1 3, not wire order); a feature whose grouping is empty (all
rings zero-area) emits `-`. Example: `5/16/9 ocean 0 34+5|12`.

Explicit sort before compare (review note SBR2 5): the two dump readers use
DIFFERENT PMTiles implementations (Rust elivagar vs the JS PMTiles the
oracle copies, whose leaf traversal is a LIFO stack, earcut-oracle.mjs:49-53,
and can reverse leaf-directory order). "Identical iteration because both read
the same directory" does NOT hold across two libraries. Both dump sides
therefore SORT their emitted lines by the tuple (tile id, layer wire index,
feature wire index) before writing, so `cmp` compares a canonical order on
each side rather than relying on incidental traversal parity.

### 5.6 Determinism rules (enforced, then tested)

- No hash-container iteration reaches any emitted byte (style lookup is
  file-order Vec scan; layers/features are sorted Vecs).
- No float formatting: coordinates i32, areas i128, paint values verbatim
  TOML strings, panel text from pinned canonical formatting.
- Acceptance test (landing 2, `brokkr check`): render a synthetic
  multi-layer tile (points + lines + multi-ring polygons + a >500-ring
  polygon + a zero-area ring + an unstyled layer) twice through separate
  `Style::load` calls; assert byte equality. Plus the whole-corpus reading
  in landing 3's gates (render-manifest twice, empty git diff), which also
  covers the "unchanged tile re-renders byte-identical" rotation property.
  Cross-machine byte-identity is asserted the first time another host runs
  `corpus check` against the committed corpus; it needs no dedicated
  instrument here.

### 5.7 `check` / `bless` / `render-manifest` semantics (tier 2 integration)

`check` (extended; steps 1-4 are today's behavior, unchanged):

1. Parse + self-check baseline digest/leaves; 2. format guard; 3. contract
   guard (refusal on mismatch); 4. digest compare.
5. If `manifest.toml` exists in the corpus dir:
   - contract has no `style` key -> Refused: "corpus has a manifest but the
     contract records no style hash - run corpus render-manifest".
   - on-disk style hash != contract style hash -> Refused, named.
   - render every manifest entry from the archive; byte-compare against the
     committed file; collect mismatches, missing files, and orphans.
   - clamp warnings from the renderer -> report warnings.
6. Verdict: digest mismatch and/or SVG mismatches -> ContentMismatch, exit
   1, message naming every mismatched/missing/orphaned SVG path (capped at
   100 lines like the leaf diff). An SVG mismatch WITH a passing digest is
   labeled distinctly: "svg stale (digest unchanged): <file> - renderer or
   style changed without re-render".

`render-manifest`: format guard; full digest check against the committed
baseline (refuse on any mismatch or refusal - SVGs are only ever rendered
from blessed-equal content); render all entries; write files atomically;
delete orphans; rewrite `contract.json` with the current style hash (other
keys untouched). Prints one line per written/deleted file.

`bless`: after today's writes, if a manifest exists, run the
render-manifest path (the just-written digest trivially matches) and record
the style hash.

### 5.8 Differential oracle script

`scripts/validate/ring-grouping-oracle.mjs <file.pmtiles> -o <out.txt>`:
copies earcut-oracle's PMTiles/gunzip/VectorTile scaffolding and its
verbatim classifyRings; iterates unique payloads; for
each type-3 feature emits the section 5.5 line via `loadGeometry()` rings;
SORTS all emitted lines by (tile id, layer wire index, feature wire index)
before writing (per section 5.5's review note SBR2 5 - the LIFO leaf-stack it
inherits does not guarantee tile-id order). No threshold, no verdict - it is
a dump, compared with `cmp`. The Rust `corpus rings` side applies the same
sort. Documented in AGENTS.md's script list alongside the other validate
scripts.

## 6. Landings, gates, and calibration

Four landings, each one coherent keep/revert unit; `brokkr check` and
`elivagar verify` stay green at every boundary. Benchmark discipline:
nothing here touches the tilegen pipeline or any measured path (see
section 7), so no results.db numbers are owed; the named gates below are
the neutrality evidence. Commit first, then run gates, per the standing
discipline. `<commit>` in commands below is the short hash of the landing
commit under test, matching the `data/tilegen/<dataset>-<commit>.pmtiles`
naming.

### Landing 1 - regress internals refactor (output-neutral)

Content: section 4.1's regress.rs changes EXCEPT the dump_svg_examples
deletion (that waits for its replacement in landing 4): pub(crate)
widening, `parse_feature_message`, wire walkers, `DiffSink` +
`DetailOutcome` adaptation.

Gates (all must pass):

```
brokkr check
brokkr tilegen --dataset denmark --variant locations
brokkr regress --dataset denmark
```

`brokkr regress` must report zero diffs (tol 0) against the standing
blessed archive - the refactor is output-neutral by definition, and the
existing differential tests in `src/regress/tests.rs` (canonical hash vs
detail decode) run under `brokkr check` and pin the walker factoring.

### Landing 2 - render core, style, manifest machinery, oracle instrument

Content: `src/corpus/` split (mod/render/style/manifest), Cargo.toml deps,
`corpus render`, `corpus rings`, `ring-grouping-oracle.mjs`,
`corpus/style.toml` committed, unit tests (classifyRings port vectors:
single ring incl. zero-area quirk, outer+hole, two outers, reversed
calibration, zero-area skip, 501-ring clamp with area ties; style parse +
match resolution; determinism acceptance test per section 5.6). No corpus
baseline changes yet.

Gates:

```
brokkr check
brokkr tilegen --dataset denmark --variant locations
./target/release/elivagar corpus rings data/tilegen/denmark-<commit>.pmtiles -o data/scratch/rings-rust.txt
node ring-grouping-oracle.mjs ../../data/tilegen/denmark-<commit>.pmtiles -o ../../data/scratch/rings-node.txt
cmp data/scratch/rings-rust.txt data/scratch/rings-node.txt
```

(the `corpus rings` and `cmp` commands run from repo root; the `node`
command runs from `scripts/validate/`, so its script path is bare
`ring-grouping-oracle.mjs` and its archive/output paths are `../../data/...`
- review note SBR2 5: the earlier `node scripts/validate/ring-grouping-oracle.mjs`
with `../../data/...` args mixed a repo-root script path with
scripts/validate-relative data paths and resolved from neither directory.) `cmp` silence over every unique
polygon payload of a full denmark build is the classifyRings verification
the plan demands (R1 2) - the port is not trusted, and no SVG is blessed,
until this reads clean. Any divergence is a port bug by definition (the
node side is verbatim maplibre) and blocks landing 3.

### Landing 3 - tier-2 integration, seeding, and the corpus bless

Content: section 5.7's check/bless/render-manifest, contract style key,
`corpus/denmark/manifest.toml` seeded per section 5.3 (with the
render-and-eyeball verification of each candidate tile),
`corpus/denmark/tiles/*.svg` rendered and committed. The commit IS the
bless; its diff (digest untouched, manifest + SVGs + contract style key
added) is the human gate.

Gates - mechanical:

```
brokkr check
brokkr tilegen --dataset denmark --variant locations
./target/release/elivagar corpus render-manifest data/tilegen/denmark-<commit>.pmtiles --corpus corpus/denmark
git diff --stat corpus/
./target/release/elivagar corpus check data/tilegen/denmark-<commit>.pmtiles --corpus corpus/denmark
```

The `git diff --stat` after the second render-manifest run must be empty
(byte-determinism on real data, the R1 3 acceptance reading at corpus
scale); `check` must exit 0.

Calibration, oracle-discipline both directions (tier 2's own readings; the
digest gate's are already recorded at Spec A):

- FIRES (mechanical): mutate a manifest tile and check. The mutation target
  must be a tile whose manifest entry RENDERS the layer the mutator edits.
  `nudge-geometry` edits the first feature of the FIRST-ENCODED layer in the
  tile (corpus.rs:1107-1152), and layers encode in `Layer::ALL` enum order -
  `water_polygons` first (shortbread/mod.rs:34), `ocean` last (=25). The
  z5/16/9 entry renders only `boundaries,ocean`, so nudging it changes
  `water_polygons` (the first present layer): the digest moves but the
  filtered SVG does not, and the required SVG mismatch cannot occur (review
  note SBR2 1). So this calibration nudges an ALL-LAYERS manifest entry, where
  whichever layer is first-encoded is guaranteed to be in the render:

```
./target/release/elivagar corpus mutate data/tilegen/denmark-<commit>.pmtiles -o data/scratch/denmark-nudge.pmtiles --tile 12/2150/1275 --op nudge-geometry
./target/release/elivagar corpus check data/scratch/denmark-nudge.pmtiles --corpus corpus/denmark
```

  Required: exit 1 naming BOTH the changed leaf run and the all-layers
  render `z12-x2150-y1275.svg`. (Alternative, if a z5/16/9-specific FIRES is
  wanted: give `corpus mutate` a `--layer <name>` selector so
  `nudge-geometry` targets the encoded layer the filtered entry renders -
  a small, in-scope addition to the Spec-A mutator - and keep the
  `5/16/9 --layer ocean` target. The all-layers-tile route above needs no
  mutator change and is the default.)
- CLEARS (mechanical): the regzip control must pass end-to-end, proving the
  SVG compare reads decoded content, not bytes:

```
./target/release/elivagar corpus mutate data/tilegen/denmark-<commit>.pmtiles -o data/scratch/denmark-regzip.pmtiles --op regzip
./target/release/elivagar corpus check data/scratch/denmark-regzip.pmtiles --corpus corpus/denmark
```

  Required: exit 0.
- FIRES (human, the plan's named reading): the 2026-07-15 spikes must be
  humanly visible in a corpus render. The stale archive fails the contract
  guard by design, so this reading uses the contract-free single-tile
  renderer:

```
./target/release/elivagar corpus render data/tilegen/denmark-bc71cf1.pmtiles -z 5 -x 16 -y 9 --layers boundaries,ocean -o data/scratch/z5-16-9-stale.svg
./target/release/elivagar corpus render data/tilegen/denmark-<commit>.pmtiles -z 5 -x 16 -y 9 --layers boundaries,ocean -o data/scratch/z5-16-9-fresh.svg
cmp data/scratch/z5-16-9-stale.svg data/scratch/z5-16-9-fresh.svg
```

  Required: `cmp` differs, and eyeballing the stale render shows the
  coastline spike where the fresh one shows none. Repeat for `-x 17`.
  Record both adjudications in this document's landing record. What
  correct looks like: the fresh z5 coast is a smooth generalized coastline;
  the stale render shows the spike excursions the H5 incident describes.

Until these readings are recorded, tier 2 is advisory, exactly like the
plan says; `brokkr regress` remains the standing gate regardless until
Spec C.

### Landing 4 - overlay attribution emitter and docs

Content: `src/corpus/overlay.rs`, `dump_overlays`, `--overlay`/
`--overlay-max`, DELETION of `dump_svg_examples` + `--svg-dump`, doc
updates (section 8), unit tests for the attr panel and per-class color
emission (synthetic DetailFeature pairs; mutate has no attr op, so
attr-change rendering is pinned by unit test - a deliberate, stated
substitution, since building an attr-mutation instrument buys nothing the
unit test does not).

Gates (build this landing's archive first, and regenerate BOTH mutants in
this landing's scratch - review note SBR2 4: the gate cannot borrow
`denmark-<commit>.pmtiles` without building it for the landing-4 commit, nor
reuse `denmark-nudge.pmtiles` from landing 3's transient scratch):

```
brokkr check
brokkr tilegen --dataset denmark --variant locations
./target/release/elivagar corpus mutate data/tilegen/denmark-<commit>.pmtiles -o data/scratch/denmark-drop.pmtiles --tile 5/16/9 --op drop-tile
./target/release/elivagar corpus mutate data/tilegen/denmark-<commit>.pmtiles -o data/scratch/denmark-nudge.pmtiles --tile 12/2150/1275 --op nudge-geometry
./target/release/elivagar regress data/tilegen/denmark-<commit>.pmtiles --against data/scratch/denmark-drop.pmtiles --overlay data/scratch/overlay-drop
./target/release/elivagar regress data/tilegen/denmark-<commit>.pmtiles --against data/scratch/denmark-nudge.pmtiles --overlay data/scratch/overlay-nudge
```

Required, by eyeball of the emitted overlays: the drop overlay for z5/16/9
is ALL current-only pink over the background - NOT "pink over grey unchanged
context" (review note SBR2 4: `drop-tile` removes the entire comparand tile,
so there are no matched features to draw grey; the whole tile is
current-only). The nudge overlay for z12/2150/1275 shows the changed PAIR -
both geometries, pink and blue, at the nudged feature, grey unchanged
context elsewhere in the tile - proving the plan-finding-R2-3 changed-pair
category renders (this is
why the nudge target is an all-layers tile with abundant unchanged context,
not the fully-dropped z5/16/9). Both runs must exit nonzero (they are real
diffs) and write exactly the expected files. (If a drop overlay WITH grey
context is wanted as a distinct reading, add a feature-drop or layer-drop
mutation that removes one feature and leaves the rest of the tile matched -
noted as an option, not required for this gate.)

## 7. Performance statement

This spec is off every measured path: it adds subcommands and refactors
`regress`'s internal plumbing; `elivagar run`/tilegen is untouched, so no
benchmark baseline is claimed and no `reference/performance.md` update is
owed. Neutrality evidence: landing 1's `brokkr tilegen --dataset denmark
--variant locations` + `brokkr regress --dataset denmark` gate (unchanged
output, unchanged pipeline), and `brokkr check` timing staying ordinary.
The `DiffSink` genericization of the regress detail pass is monomorphized
and adds no allocation; regress is not a benchmarked command, and its
three-pass structure and parallelism are unchanged.

## 8. Documentation updates (bundled with their landings)

- `reference/cli.md`: `corpus render` / `render-manifest` / `rings`
  sections, the extended check/bless semantics (style hash, SVG compare,
  orphan rule, clamp warnings), `regress --overlay`/`--overlay-max`
  replacing `--svg-dump` (landing 3 and 4 respectively).
- `AGENTS.md`: add `ring-grouping-oracle.mjs` to the validate-script list
  (landing 2); one line in the corpus paragraph of the CLI section noting
  tier 2 exists and is advisory (landing 3).
- `notes/svg-corpus-plan.md`: mark Spec B landed with a pointer to this
  document (landing 4, riding the code commit).
- This document records the calibration adjudications as they are taken.

## 8b. Review resolution (SBR1 + SBR2)

Two reviews of THIS spec (`notes/svg-corpus-spec-b-R1.md`, Opus, cited below
as SBR1; `notes/svg-corpus-spec-b-R2.md`, codex gpt-5.6-sol xhigh, SBR2 -
prefixed "SB" to keep them distinct from the PLAN's R1/R2 that the goal
section cites as "plan finding R1/R2") were validated against the code at
head `340215a`. Every finding held against the source and is folded above at
its home section; NONE were rejected. Index (severity as the reviewer graded
it):

| # | src | sev | issue | folded into |
|---|-----|-----|-------|-------------|
| 1 | SBR2 | P1 | nudge hits first-encoded layer (`water_polygons`), filtered z5/16/9 SVG never mismatches | Landing 3 FIRES (now nudges an all-layers tile; `--layer` selector noted as alt) |
| 2 | SBR2 | P1 | `RawFeature<'a>` unused lifetime (E0392); `render_svg` lacks z/x/y; `pub(crate)` types keep private fields | 4.1 (drop lifetime, widen fields), 4.3 (add z/x/y) |
| 3 | SBR2 | P1 | `layer_event` one-sided can't carry both extents/versions; extent path skips `compare_detail_layer` | 4.1 (pair-aware `layer_event`, per-side extent normalization) |
| 4 | SBR2 | P1 | Landing 4 gate unbuilt archive + reused transient nudge; drop-tile leaves no grey context | Landing 4 (build + regen both mutants; expect all-pink) |
| 5 | SBR2 | P1 | oracle node command mixes repo-root script path with scripts/validate data paths; LIFO traversal not tile-id ordered | Landing 2 cmd, 5.5, 5.8 (bare script path; explicit sort both sides) |
| 6 | SBR2 | P1 | manifest layer names unsanitized: `+`-join collision, `../` traversal, filename non-uniqueness | 4.5 (restricted grammar, derived-filename uniqueness) |
| 7 | SBR2 | P1 | no XML escaping of interpolated style/layer/id/attr text | 5.1 (`xml_text`/`xml_attr`, hostile-value tests) |
| 8 | SBR2 | P2 | `AttrChanged` short-circuits before geometry classify; orange-once hides a co-occurring geom move | 4.1, 4.6 (draw both sides when `geometry_digest` differs) |
| 9 | SBR2 | P2 | survey head `420534e` predates Spec A (`a8c4f84`); goal says exhaustive, impl caps at 64 | 2 head note, 1 goal (sampled emitter) |
| 10 | SBR1 | gap | i128-vs-f64 shoelace "bit-for-bit on any input" overclaims past the 2^53 running-sum bound | 3.2 (bounded-equivalence note; gate rests on denmark coords) |
| 11 | SBR1 | gap | contract `style` key: `contract_text` rebuilds 4 keys and drops it; `extract_contract` won't read it back | 3.4 (extend writer, separate read-back parse) |
| 12 | SBR1 | smell | clamp `sort` reorders a clamped polygon's rings, so "wire order" is not unconditional | 3.2, 5.5 (clamped = area-descending survivor order) |
| 13 | SBR1 | gap | grey-vs-both split depends on `matched()` + tolerance/structural routing being confirmed | 4.1 (confirmed via regress.rs:2748-2756) |
| 14 | SBR1 | nit | overlay panel `font-size="40"` x 26 lines overflows the 1024px panel | 4.6 (font-size 20, 24px line-height) |
| 15 | SBR1 | nit | dump `featIdx` (wire) vs SVG `i` (canonical) are different index spaces | 5.5 (explicit disambiguation) |
| 16 | SBR1 | nit | contract-free `corpus render` refuses foreign non-integral-extent archives | 4.7 (acknowledged limitation) |

No finding was rejected. The two reviews did not overlap on any single
finding; SBR2 concentrated on buildability/gate-runnability (P1s), SBR1 on
correctness caveats and arithmetic (gaps/nits). The load-bearing pair to fix
before implementation are SBR2 1/2/3 (the calibration and the APIs cannot
compile or fire as originally written) and SBR1 10/11 (claims that
contradict the spec's own "implementation discovers nothing" bar).

## 9. Stopping rule and exclusions

Blast radius ends at:

- **No teardown.** `brokkr bless`/`brokkr regress`/`data/blessed/`/
  `datasets.<D>.blessed` and every doc paragraph about them are Spec C's,
  strictly after this spec's calibrations are recorded. The cross-repo
  `brokkr corpus` wrappers are likewise Spec C's named brokkr-repo task.
- **Denmark only.** No germany/norway/planet corpus SVGs; bucket-mode
  datasets keep their R2 2 reviewability hole open exactly as the plan
  records it (tier 3 + committed overlays are the answer there, not more
  manifest).
- **MVT + gzip only** (the existing format guard); no MLT, no brotli in the
  render core.
- **`src/svg.rs` untouched** beyond losing its `dump_svg_examples` caller;
  `elivagar svg`, `brokkr svg`, and `svg-roi.mjs` keep working unchanged.
- **No tilepeek convergence**; `corpus/style.toml` is authoritative for the
  corpus from adoption, drift accepted (plan ruling).
- **No baseline-free detectors**: the H5 seam-gap candidate and the earcut/
  boundary-line oracles are separate workstreams; the corpus gates CHANGE,
  not correctness-in-itself, and nothing here is argued to subsume them.
- **Tier 2 is not promoted to a detection gate**, now or later (R1 5): the
  digest is the exhaustive detector; the corpus check's SVG compare is the
  staleness guard for the human layer.
