# Spec 5: Output regression diffing (elivagar regress)

Written against `reference/technical-implementation-spec.md`. Spawned from
the standing gap that no gate diffs pipeline output against a known-good
archive - property oracles (earcut, verify) prove invariants, not "nothing
moved that should not have." Immediate motivation:
`notes/spec-4-tile-pyramid-descent.md` changes every polygon tile with
explicitly non-byte-identical output, and its "counts within 1%" bounds are
the coarsest gate in that spec. Per the contract, the instrument lands
BEFORE the brick it gates: this spec precedes Spec 4 Landing 1.

Scope boundary (decided): elivagar owns decoding, canonicalization,
comparison, classification, and reporting - a new `elivagar regress`
subcommand. brokkr is PRESUMED to grow (a) a `brokkr regress` wrapper in the
`pmtiles-inspect`/`diag`/`svg` style, resolving the current build's archive
and the blessed archive from `brokkr.toml`, and (b) a blessing workflow that
copies a gate-passing archive to `data/blessed/<dataset>-<commit>.pmtiles`
and registers it (path, source commit, xxhash) in `brokkr.toml`. If brokkr
deviates, that is the brokkr side's problem; nothing in this spec depends on
the wrapper existing - `elivagar regress` takes two explicit paths.

## 1. What it must catch (and what it cannot)

Catches: silently dropped/added tiles; features that vanish or appear;
attribute changes; geometry drift beyond a stated tolerance; layer/zoom
coverage shifts - all relative to a BLESSED archive that passed every gate
including human QA. A snapshot mechanism only catches regressions FROM a
verified-good state; it cannot find longstanding bugs (R23 survived three
months precisely because every reference was equally wrong -
`notes/rendering-postmortem.md`). The earcut oracle remains the independent
correctness gate; regress is the change-accountability gate. They are
complementary, not substitutes.

Non-goal: bitwise archive comparison. Compression settings, PMTiles
clustering, and feature ORDER within a tile are not semantic content.

## 2. Survey of the ground

- `src/pmtiles_reader.rs`: complete reader - header accessors,
  `read_all_entries()` (root + leaf directories, run-length expanded to
  `TileEntry { tile_id, offset, length }`), `read_tile()`, `read_tile_raw()`.
  CORRECTION (was wrong in this spec's first draft): `read_tile()` always
  gzip-decompresses regardless of the header's tile_compression field, and
  `verify` likewise rejects non-gzip archives. regress is therefore
  MVT+gzip only, erroring hard on any other tile_compression byte -
  matching verify's actual support, not the header's vocabulary.
  Dedup is visible here: denmark has 1.32M addressed tiles over 175K
  unique data blobs; entries sharing (offset, length) share content, and
  run-length entries expand to consecutive tile_ids sharing one pair.
- Tile decoding is fragmented across three private decoders, none
  full-fidelity: `svg.rs` `decode_mvt_geometry` -> `SvgLayer`/`SvgFeature`
  (geometry as f64, attrs not modeled), `verify.rs`
  `validate_mvt_*` (validation walkers, no output model),
  `geometry/mvt_decode.rs` `decode_mvt_polygon` (commands -> rings, used by
  tests/merge). All are R23-spec-compliant (ClosePath does not move the
  cursor; enforced by `mvt/tests.rs`). There is NO decoder that yields
  layers with feature ids, attrs, and geometry together - Brick 2 builds
  it and it becomes the canonical one.
- Feature identity on the wire, load-bearing and subtle:
  `merge_same_attr_geometries` (`mvt/merge.rs:141`) merges same-attr
  non-point geometries within a layer and sets the merged feature's id to
  NONE (the encoder omits absent ids, `mvt/mod.rs:329`). So a tile layer
  holds a mix of id-carrying features and ANONYMOUS merged features whose
  geometry is a concatenation of the merged components in pre-merge
  feature order. (layer, id, attrs) is a valid identity only for
  unmerged features; anonymous features are identified by (attrs) group +
  geometry, and their COMPONENT ORDER is itself nondeterministic (see
  next bullet). `CanonFeature.id` is `Option<u64>` and matching handles
  the anonymous case explicitly (3.2).
- Determinism is NOT established - the exact nondeterminism sources,
  verified in code, dictate what canonicalization must erase:
  (a) ocean chunk ids come from an `AtomicUsize` raced by rayon workers
  (`ocean.rs:243`), so equal-key records land in differently-numbered
  chunks run-to-run; (b) the k-way merge DOES tie-break equal keys by
  chunk index (`sort.rs:543`) - deterministic given chunk assignment, but
  (a) makes chunk assignment itself nondeterministic; (c) within one
  chunk, equal keys are ordered by `sort_unstable_by_key`
  (`sort.rs:363`, `sort.rs:427`) - unstable, so same-chunk equal-key
  order varies; (d) the sort key `(tile_id, layer, 0)` carries no
  sequence, so (a)+(c) surface as intra-layer feature order AND as
  component order inside merged features (merge concatenates in feature
  order). Canonicalization must therefore sort features AND components
  within merged features; Brick 1 measured the effect (below).
- Archive locations (reconciled with the brokkr side): brokkr renames run
  outputs to `<scratch>/<dataset>-<commit>.pmtiles` where `<scratch>` is
  brokkr.toml's per-host scratch dir. On plantasjen scratch is currently
  `data/tilegen_tmp` - the SAME directory elivagar wipes at every run
  start, so these archives are transient and one probe pair was already
  lost to exactly this. Consequences baked into this spec: every archive
  needed later is copied OUT immediately after the producing run
  (`data/probes/` for probe archives, `data/blessed/` for blessed ones),
  and no gate ever names a scratch path as a stable input.
- Noop probe pair that should be semantically IDENTICAL: denmark built at
  `9b51e46` vs at `661cd1c`. Precisely: `661cd1c` relative to `9b51e46`
  changes only `.brokkr/results.db` and `reference/performance.md` -
  nothing under src/ (the prepass-overlap threading change IS `9b51e46`
  itself, and threading changes do not alter emitted geometry anyway,
  only ordering - which canonicalization erases).
  The pair is REGENERATED for Brick 4 (the original archives were wiped):
  `brokkr tilegen --commit 9b51e46 --dataset denmark` (brokkr builds the
  old commit), copy out to `data/probes/`, then a plain HEAD run, copy
  out. Any semantic diff between them is a bug in regress itself (or a
  real nondeterminism finding worth knowing either way).
- CLI: clap derive, `enum Command` in `src/main.rs:29`; `inspect`, `verify`,
  `svg`, `diag` show the subcommand pattern to follow.

## 3. Target structure

### 3.1 Canonical tile model (`src/regress.rs`, new)

```rust
/// Full-fidelity decoded tile, canonicalized: layers sorted by name,
/// features within a layer sorted by (id, geom_type, attrs_key, geom_key).
/// This ordering erases the pipeline's intra-layer order nondeterminism
/// and NOTHING else - two tiles canonicalize equal iff they have the same
/// layers, features, attributes, and exact geometry commands.
pub struct CanonTile {
    pub layers: Vec<CanonLayer>,
}
pub struct CanonLayer {
    pub name: String,
    pub extent: u32,
    pub features: Vec<CanonFeature>,
}
pub struct CanonFeature {
    /// None for merged same-attr features (the merger drops ids).
    pub id: Option<u64>,
    pub geom_type: u8,
    /// (key, value) pairs resolved from the layer key/value tables,
    /// sorted by key.
    pub attrs: Vec<(String, AttrVal)>,
    /// Geometry as canonical COMPONENTS. For polygons a component is one
    /// outer ring plus its holes (grouped by winding as MVT semantics
    /// dictate; within-component ring order preserved - it is
    /// semantic); for lines a component is one path; for points one
    /// component of all points. Components are sorted by their canonical
    /// byte encoding: merge concatenation order is nondeterministic
    /// (survey) and must not affect equality. Vertex order within a
    /// ring/path is NEVER reordered.
    pub components: Vec<CanonComponent>,
}
/// Attr values preserve the wire variant and raw bits - Float(u32 bits)
/// and Double(u64 bits) stay distinct variants and compare bitwise
/// (NaN payloads and negative zero included); Int/UInt/Sint/Bool/String
/// compare exactly. A variant change with equal numeric value IS a diff.
pub enum AttrVal { ... }

pub fn decode_canonical(tile_data: &[u8]) -> Result<CanonTile, String>;
/// xxh3-128 over the canonical serialization; the cheap equality tier.
pub fn canon_hash(tile: &CanonTile) -> u128;
```

`decode_canonical` is built on one shared low-level decoder; `svg.rs` is
NOT rewritten onto it in this spec (stopping rule) - but the new decoder
lives in `geometry/mvt_decode.rs` alongside the existing polygon decoder so
later consolidation is natural.

### 3.2 Comparison engine

```rust
pub struct RegressConfig {
    /// Geometry tolerance in the layer's own extent units. A matched
    /// component pair within tol (discrete symmetric Hausdorff) is a
    /// TOLERANCE diff; beyond it, STRUCTURAL. Tolerance applies ONLY
    /// after structure agrees (see flow: component counts, ring roles).
    pub tol: i32,
    /// Bound on total tolerance_moved before exit 1. Tolerance moves are
    /// NOT free by default: a systematic sub-tol shift of thousands of
    /// features is a real regression. Default 0 (any move fails); gates
    /// that expect drift (Spec 4) pass an explicit budget and read the
    /// printed displacement percentiles besides.
    pub max_moved: u64,
    /// Cap on per-class example collection for reporting/SVG dump.
    pub max_examples: usize,
}

pub enum TileDiff {
    /// Tile id present in exactly one archive.
    OnlyInCurrent(u64), OnlyInBlessed(u64),
    /// Same tile id, canonical content differs.
    Content {
        tile_id: u64,
        layers_added: u32,       // layer present (even empty) in one side only
        layers_removed: u32,
        extent_mismatch: u32,    // same layer, different extent: structural,
                                 // geometry comparison skipped for the layer
        missing_features: u32,   // in blessed, unmatched in current
        added_features: u32,
        attr_changed: u32,       // matched by (layer,id) but attrs differ
        tolerance_moved: u32,    // matched, geometry within tol
        structural_moved: u32,   // matched, geometry beyond tol
    },
}

pub struct RegressReport {
    pub identical_tiles: u64,
    pub diffs: Vec<TileDiff>,          // capped collection + full counters
    pub per_zoom_layer: ...,           // counters keyed (z, layer, class)
}

pub fn regress(current: &Path, blessed: &Path, cfg: &RegressConfig)
    -> io::Result<RegressReport>;
```

Flow: read both directories -> expand entries -> two sorted tile_id lists
-> merge-walk. Set differences report directly. For shared tile_ids,
compare content: a per-archive memo keyed by (offset, length) caches
`canon_hash` so deduped tiles (86% of denmark's addressed tiles) decode
once. Hash-equal -> identical. Hash-unequal -> per-layer comparison.
Decode work is parallelized with rayon over the shared-id list (batched;
the report is deterministic because counters are merged and examples are
selected by tile_id order, not completion order).

Per-layer comparison, in order (each step short-circuits the next):

1. Layer presence: a layer present on one side only (even with zero
   features) is layers_added/removed - structural, exit 1.
2. Extent: mismatched extents are extent_mismatch - structural; geometry
   comparison for that layer is skipped (tolerance units would be
   incomparable).
3. Feature matching, multiset with counts: id-carrying features match by
   (id, attrs); anonymous merged features (id None - see survey) match
   within their (attrs) group by canonical geometry: sort each side's
   group members by canonical encoding, pair equal encodings first
   (identical), then pair remainders greedily by minimal Hausdorff.
   Same-id different-attrs pairs are attr_changed. Unpaired features are
   missing/added - this is also where a merge GROUPING change (one
   anonymous feature becoming two) surfaces, as a count mismatch in the
   (attrs) group; correct, because merge behavior IS semantic output.
4. Geometry classification for matched pairs, structure BEFORE tolerance:
   component counts must match, and for polygons each paired component's
   ring roles must agree (hole count, winding, hole-in-outer containment) -
   any mismatch is structural regardless of distance (a hole that
   detached at distance 0 is exactly the earcut-relevant defect class).
   Components pair like anonymous features: canonical-equal first, then
   greedy minimal-Hausdorff; leftover components are structural. Only
   then: all paired components within tol -> tolerance_moved, else
   structural_moved.

Layer-specific identity: ocean feature ids are synthetic piece indices,
unstable across pipeline changes BY DESIGN (Spec 4 changes them
wholesale). For the ocean layer, matching treats ALL features as
anonymous (id ignored, attrs are empty there anyway) - pure geometry
matching per step 3.

### 3.3 CLI

```
elivagar regress <CURRENT.pmtiles> --against <BLESSED.pmtiles>
    [--tol N]            geometry tolerance in the layer's extent units
                         (default 0)
    [--max-moved N]      tolerance_moved budget before exit 1 (default 0;
                         no budget flag means any move fails)
    [--max-examples N]   per-class example cap (default 20)
    [--svg-dump DIR]     side-by-side SVG pairs for the worst structural
                         diffs (reuses render_tile_svg on both archives)
    [--json]             machine-readable report to stdout
```

Both archives must be MVT + gzip (the reader's actual support); anything
else is a hard error before comparison starts.

Exit code: 0 iff zero set-differences, zero layers added/removed, zero
extent mismatches, zero missing/added features, zero attr changes, zero
structural moves, AND tolerance_moved <= max_moved. Everything else exits
1. Residual blindness, stated plainly: a systematic sub-tol drift within
an explicitly granted --max-moved budget passes - which is why the report
always prints per-layer/per-zoom displacement percentiles (p50/p95/max of
matched-pair Hausdorff) for the human reading the gate, and why the earcut
oracle and visual QA remain independent gates. Text report: summary line,
per-zoom/per-layer table of nonzero counters, displacement percentiles,
then examples as `z/x/y layer id class` lines - each example line
copy-pasteable into `elivagar svg`/`diag`/`feature-probe.mjs`.

### 3.4 Presumed brokkr integration (informative, not this spec's bricks)

- `brokkr regress [--dataset D] [--tol N] [--file P]` - resolves the
  current archive (default: the last run's
  `<scratch>/<dataset>-<commit>.pmtiles` - valid only until the next run
  wipes scratch; brokkr should consider renaming archives to a non-wiped
  destination, which is its call to make) and the blessed archive
  from a `[plantasjen.datasets.<D>.blessed]` brokkr.toml entry
  (`file`, `commit`, `xxhash`), verifies the xxhash, then execs
  `elivagar regress <current> --against <blessed> --tol N`.
- `brokkr bless [--dataset D]` - copies the named archive from the scratch
  dir to `data/blessed/<dataset>-<commit>.pmtiles` and writes a singular
  `[<host>.datasets.<D>.blessed]` entry (`file`, `commit`, `xxhash`) into
  brokkr.toml - toml_edit, comment-preserving (per the brokkr dev: the one
  genuinely new piece of machinery on that side). Keyed singular, not
  by-variant: one pmtiles variant exists; a map waits for a second one.
  `commit` stays explicit in the entry (derivable from the filename, kept
  for provenance). bless REFUSES a dirty tree (results.db and *.md
  excluded, matching bench discipline) - blessing from a dirty tree would
  record a hash that does not reproduce.
- Blessing is manual and deliberate: only after a landing's full gate
  battery (including human QA) passes.
- Blessed archives live under `data/blessed/` (gitignored, NOT under the
  scratch dir - scratch is wiped by elivagar runs on hosts where it
  coincides with the tmp dir); the repo carries only the toml
  registration. Cross-host: each host blesses its own.

Spec-4 consequence (amendment applied when BOTH specs' first landings are
in): each Spec 4 landing gate gains
`elivagar regress <landing output> --against <blessed 661cd1c archive>
--tol 16 --max-moved <explicit budget>` (dp_tol is 16, so legitimate
DP-anchor drift is tolerance-class; the budget is stated per landing and
is expected to be a large fraction of coastline features - the gate is
read together with the displacement percentiles, where legitimate DP
re-anchoring shows as p50 near zero with a sub-16 tail, and a systematic
shift shows as an elevated p50). Keep bounds: zero set-differences at
z <= 8, zero missing/added features outside the ocean layer's anonymous
regrouping, zero attr changes, structural moves individually reviewed via
--svg-dump (expected: only full-tile-boundary reshuffles; any lake or
island disappearance is a revert).

## 4. Bricks

**Brick 1 - determinism probe (instrument pricing, contract clause 5).**
DONE (2026-07-07, plantasjen, commit `661cd1c`): two plain denmark runs,
archives preserved as `data/probes/denmark-661cd1c-runB.pmtiles` /
`-runC.pmtiles`. Outcome: NOT byte-identical - first divergence at header
byte 49, total sizes 350,947,496 vs 350,950,371 (2,875 bytes apart). This
confirms the sort-tie analysis: intra-layer feature order varies with
chunk assignment, changing tile bytes, therefore dedup hits and directory
sizes. The canonical form is load-bearing - raw byte or hash comparison of
archives can never be a gate, and Brick 4's noop probe MUST pass through
canonicalization. (Lesson folded into the survey: the run that produced
runB also wiped the previous probe pair, because scratch archives live in
elivagar's tmp dir on this host.) Commands, for the record:
`brokkr tilegen --dataset denmark`; `mv <scratch>/denmark-661cd1c.pmtiles
data/probes/denmark-661cd1c-runB.pmtiles`; repeat for runC;
`cmp data/probes/denmark-661cd1c-runB.pmtiles data/probes/denmark-661cd1c-runC.pmtiles`.

**Brick 2 - canonical decoder.** `decode_canonical` + `canon_hash` in
`geometry/mvt_decode.rs` + `src/regress.rs` model types. Unit tests:
round-trip a synthetic two-layer tile built with the production encoder
(`mvt` module) - attrs resolve, ids survive, ClosePath semantics match
`decode_mvt_polygon` on the polygon layer; canonicalization erases feature
order (encode features in two orders, canon forms equal); canon_hash
differs on: one moved vertex, one changed attr value, one dropped feature,
a Float vs Double variant swap of the same numeric value, NaN-payload and
negative-zero bit differences; canonicalization erases component order
inside a merged multi-line feature (encode two component orders, canon
forms equal) but NOT ring order within a polygon component.
Gate: `brokkr check`.

**Brick 3 - comparison engine + CLI.** `regress()` per 3.2, subcommand per
3.3, wired into `main.rs`. Unit tests: synthetic archive pairs built with
`pmtiles_writer` in-test (tiny, single-tile-per-zoom): identical ->
exit 0 / all-identical report; one tile removed -> OnlyInBlessed; vertex
moved by 3 with tol 4 -> tolerance_moved, with tol 2 -> structural;
attr change -> attr_changed; layer present-but-empty on one side ->
layers_removed; extent mismatch -> extent_mismatch with geometry skipped;
polygon hole reassigned to a different outer at distance 0 -> structural;
run-length-encoded directory entries expand and compare correctly (a
run of N tiles sharing one blob); dedup memo hit counted (two tile_ids sharing
one blob decode once). Gate: `brokkr check`.

**Brick 4 - self-validation on real archives.** Commands, in order:
1. Same-run self-diff:
   `elivagar regress data/probes/denmark-661cd1c-runB.pmtiles --against data/probes/denmark-661cd1c-runB.pmtiles`
   - exit 0, all tiles identical.
2. Same-commit cross-run (the Brick 1 pair, byte-DIFFERENT archives):
   `elivagar regress data/probes/denmark-661cd1c-runC.pmtiles --against data/probes/denmark-661cd1c-runB.pmtiles`
   - expected exit 0 with zero diffs of any class at tol 0. This is the
   canonicalization proof: Brick 1 established these archives differ in
   bytes; regress must prove them semantically identical. A nonzero
   result is a regress bug or a deeper nondeterminism finding - resolved
   inline before landing, not deferred.
3. Noop-commit probe: `brokkr tilegen --commit 9b51e46 --dataset denmark`,
   copy the archive out of scratch to
   `data/probes/denmark-9b51e46.pmtiles` IMMEDIATELY (scratch is wiped by
   the next run), then
   `elivagar regress data/probes/denmark-661cd1c-runB.pmtiles --against data/probes/denmark-9b51e46.pmtiles`
   - expected exit 0, zero diffs at tol 0 (geometry-identical commits).
Gate: the three exit codes; runtime on denmark recorded (budget: under
60s single-threaded is acceptable, under 15s with rayon expected - if it
blows past, fix before landing; the tool must be cheap enough to run per
landing on three datasets).

Measured (2026-07-07, plantasjen, pre-landing build): steps 1 and 2 PASS -
the byte-different runB/runC pair proves semantically identical
(identical_tiles=1323406, zero diffs of every class), and a bonus
cross-build check (dirty-HEAD-with-regress-code archive vs runB) is also
zero-diff. Runtime 49s user / 99% single CPU on denmark: codex did NOT
implement the rayon parallelization named in 3.2 - within the accepted
single-threaded budget, so the landing proceeds; parallelize only if the
per-landing battery cost starts to chafe. Step 3 (noop-commit probe via
--commit 9b51e46) requires a clean tree and runs right after the landing
commit.

**Brick 5 - landing.** One commit: decoder, engine, CLI, tests, plus:
AGENTS.md CLI section gains `elivagar regress` and the Verification list
in `reference/technical-implementation-spec.md` clause 5 gains regress as
a named gate class ("output-diff regression vs the blessed archive for
changes intended to be output-neutral, or with stated tolerance for
geometry-changing landings"); Spec 4's gate amendment per 3.4. Then bless
the current state (manual, presumed brokkr flow; until `brokkr bless`
exists, the blessed archive is copied by hand and the path passed
explicitly). Gate: `brokkr check` + Brick 4's commands rerun on the
committed build.

Keep/revert: this is additive tooling off every measured path; the
keep/revert verdict is Brick 4 - a tool that cannot prove the noop pair
identical does not land. No bench required (no pipeline code is touched);
the neutrality statement per contract clause 10: nothing under
`src/pipeline/`, `src/geometry/` (except the new decoder fn),
`src/ocean.rs`, `src/sort.rs`, or the encoders changes, and
`brokkr check` green witnesses it.

## 5. Stopping rule

Out of scope: the brokkr wrapper and blessing commands (presumed per 3.4,
owned outside this repo); rewriting svg.rs/verify.rs onto the canonical
decoder (later consolidation, not this spec); raster/pixel diffing of SVG
renders (the SVG dump is for human eyes, the classification is geometric);
intermediate-stage snapshots (sort chunks, way index - the archive is the
contract surface; stage-level diffing has no consumer until a stage rewrite
wants it and can add it then); non-gzip / non-MVT archives - the reader
is gzip-only today (survey correction) and regress errors hard on
anything else rather than half-supporting it; any change to what the
pipeline emits.

## 6. References

- `reference/technical-implementation-spec.md` - contract (clause 5's
  instrument-first rule is why this spec exists and why it precedes Spec 4).
- `notes/spec-4-tile-pyramid-descent.md` - the first consumer; gate
  amendment in 3.4.
- `notes/rendering-postmortem.md` - why regress complements but never
  replaces the earcut oracle (the R23 blindness argument).
- `reference/performance.md` - measurement record; this spec touches no
  measured path and records no benchmark.
