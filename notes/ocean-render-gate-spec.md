# Ocean render gate: implementation spec

Written 2026-07-13, restructured 2026-07-14 to be CALIBRATION-FIRST after two
independent reviews (`notes/render-gate-review-r1-opus.md`,
`notes/render-gate-review-r2-codex.md`) showed the gate's core premise is
empirically uncertain and must be proven before any durable machinery is worth
building. Written against `reference/technical-implementation-spec.md` (the
contract) and spawned from `notes/ocean-coastline-spike-problem.md`
("Recommended actions", item 1 - "Formalize the visual gate as a blessed-render
pixel diff") and its "Why every automated verdict was wrong" reckoning. The
rasterization semantics are pinned by `notes/svg-corpus-plan.md` (classifyRings +
nonzero winding is what both viewers do); the calibration discipline is pinned by
the "Oracle discipline" section of `AGENTS.md`.

## The honest premise (read this first)

The proposed instrument is a render-level, connected-component ocean gate:
rasterize the ocean layer to a binary mask, XOR test against reference, and gate
on the size of the largest CONNECTED COMPONENT of the pixel disagreement rather
than the aggregate pixel count. The hypothesis is that this separates two things
the `ocean-coverage` diagnostic could not:

- the DEFECT - a low-zoom land-colored coastline spike (a compact 2D filled
  coverage notch), and
- LEGITIMATE simplification loss - sub-pixel coastline detail that any correct
  simplifier removes.

The hypothesis is that legitimate loss rasterizes to a scatter of tiny
disconnected specks (small components) while the spike is one large component, so
component SIZE is categorical where aggregate area was continuous.

THIS HYPOTHESIS MAY BE FALSE, and the whole item is structured so that we find
out cheaply BEFORE building anything durable. The specific reason it may be
false (Opus R1, major): a fjord removed by simplification is sub-pixel in WIDTH
but LONG. XORed against the verbatim reference (calibration pair B, which MUST
CLEAR), that removed water channel is not a scatter of specks - it is a long,
thin, fully CONNECTED run of `ref_only` pixels, potentially tens of pixels. The
source note's own numbers point the same way: VW still loses ~146K 2x-px^2 at
z4/8/4, which the note backs out to "roughly two tile-widths of fjord coast"
(`notes/ocean-coastline-spike-problem.md`) - spatially extended loss, not
noise. That is the SAME confound that sank the coverage oracle, measured
differently: connectivity does not automatically dissolve it. If a long thin
fjord streak in pair B is as large as the spike component in pair A, the gate is
not categorical and cannot ship.

Two consequences drive the structure:

1. Calibration is not a formality that follows construction - it is the GO/NO-GO
   for the entire item. We build a minimal, throwaway-friendly harness FIRST,
   run the two-direction calibration, and only proceed to durable machinery if
   the separation is real and wide.
2. The proceed bar is quantitative and stronger than "strictly between." The
   threshold math (geometric mean AND >= 2x the pair-B floor) secretly requires
   `A >= 4B`: `sqrt(A*B) >= 2B` iff `A >= 4B` (Opus R1). So the real proceed
   condition is: the spike component (pair A) must be at least 4x the
   legitimate-loss floor (pair B), with the chosen threshold clearing pair B by
   at least 2x. State this up front so the implementer knows the true bar.

If calibration does NOT separate at any swept RES/connectivity, the item CLOSES
as "render-gate approach refuted" - a recorded negative that mirrors the
coverage oracle's demotion - and no further machinery is built. That is a clean,
successful outcome for this item, not a failure to deliver.

## Why this exists (the defect and the near-miss)

The ocean-VW landing (`31b8298`) fixed a real, visible defect - low-zoom
land-colored coastline spikes caused by Douglas-Peucker over-generalization of
the ocean polygon - and EVERY automated signal we had reported the fix as
FAILED. Only a human loading the tiles and looking confirmed it worked. The
`ocean-coverage` diagnostic failed because it thresholds a CONTINUOUS quantity
(one-sided lost water area vs a verbatim baseline) that is legitimately large on
correct output: at z4 (~10 km/px) any correct simplifier removes large sub-pixel
coastline detail, so a correct build scores hundreds of times over the threshold.
The threshold sat two-plus orders of magnitude below the legitimate floor, and
nobody ran the null hypothesis ("what does a known-good build score?").

The reckoning named the fix: the trustworthy oracles here (earcut,
boundary-line) share three properties, and the coverage oracle had only the
first. (1) Consumer-path decode - read geometry the way MapLibre does.
(2) A CATEGORICAL defect with near-zero base rate on good output; pass = count 0
and 0 is achievable. (3) Calibration in BOTH directions - fires on known-bad AND
clears on known-good - before it may gate.

The structural insight the render gate BETS ON: the sub-pixel legitimate loss
that sank the coverage oracle may not survive rasterization as a large connected
component. A thin fjord removed by simplification is long but sub-pixel WIDE; if
it rasterizes to a scatter of 1-few-px specks along the coast rather than a
filled streak, component size is categorical (good = small components, defect =
one large component) in the way aggregate pixel count is not. The whole point of
calibration-first is that we do NOT know this holds - see "The honest premise"
above. Brick 1 tests it; it does not assume it.

## Brick 1 - the calibration harness IS the go/no-go

Brick 1 builds a MINIMAL, throwaway-friendly script whose only job is to run the
two-direction calibration and return a GO/NO-GO for the whole item. It does not
need the polished CLI, `--json`, the union-scan robustness path, or the durable
reference format - those belong to the later bricks and are built ONLY if brick 1
says GO. Keep brick 1 cheap so that a NO-GO costs little.

The script lives at `scripts/validate/render-gate.mjs` (grown, not replaced, by
the later bricks) under the existing `validate` pnpm workspace - no new deps
(`pmtiles`, `@mapbox/vector-tile`, `pbf`, node `zlib`; NOT `earcut`).

### The mechanism (pinned so two implementers converge)

Per tile, per archive: decode -> classify rings -> rasterize to a binary ocean
mask. Then XOR the two masks and label connected components.

- Decode: reuse the earcut oracle's proven scaffolding (copy, do not import):
  `BufferSource`, `allTiles()`, `tileIdToZxy`, the
  `header.tileDataOffset + offset` slice, `gunzipSync`/raw fallback. Build a
  `Map<tileId, Uint8Array>` of decompressed MVT bytes for BOTH archives; iterate
  a chosen tile set (calibration uses `--tiles`, see below).
- Geometry to polygons (consumer-path): for the target layer, for each
  `type === 3` feature, `feat.loadGeometry()` -> rings in tile coordinates (read
  `layer.extent`, do not hardcode 4096). Group with `classifyRings(rings, 500)`.
  NOTE the >500-ring divergence codex flagged (finding 8) - the earcut oracle's
  copy sorts the whole polygon before truncating, while installed MapLibre
  preserves the outer ring and quickselects only after it, so they disagree for
  polygons with >500 rings. For CALIBRATION this does not bite (ocean tiles here
  are well under 500 rings per polygon), so brick 1 may use the earcut copy
  as-is; the verbatim fix is a later-brick requirement (see "Durable gate"). If
  brick 1 hits a >500-ring polygon it must WARN, not silently diverge.
- Rasterize (nonzero winding scanline fill): one `Uint8Array(res*res)` per tile
  per archive, 1 = ocean. Scale `s = res/extent`; tile `(x,y)` -> pixel
  `(x*s, y*s)`. Fill is nonzero winding over ALL rings of a classified polygon
  together (holes cut by winding, not by ring role - `notes/svg-corpus-plan.md`),
  then OR the polygon into the tile mask. Standard scanline: for scanline `py`,
  sample world-y `wy = (py+0.5)/s`; for every ring edge `(a,b)` with
  `wy in [min(a.y,b.y), max(a.y,b.y))` (half-open) compute crossing `wx` and a
  `+1`/`-1` winding contribution by edge direction; sort crossings by `wx`, walk
  accumulating winding, set pixels `px` from `ceil(wx0*s-0.5)` to `floor(wx1*s-0.5)`
  where accumulated winding is non-zero. Clamp indices to `[0,res)`. No
  anti-aliasing - a hard binary mask is what keeps the disagreement categorical.
  (Scanline, not per-pixel PIP: O(scanlines x edges + pixels) vs
  O(pixels x edges); identical binary result for the nonzero rule.)
- Diff + components: `diff[i] = mask_test[i] XOR mask_ref[i]`. Label connected
  components under the chosen connectivity (flood fill or union-find). Per
  component record pixel count and the signed split (`test_only` where
  `mask_test==1`, `ref_only` where `mask_test==0`). The verdict quantity is the
  max component px.

### Cross-tile stitching is REQUIRED for calibration validity

Codex finding 6, and it is load-bearing here, not a nicety: the historical spike
straddles the z4 y=3 / y=4 tile seam (the calibration tiles include BOTH
`z4/8/3` and `z4/8/4`). Tile-local component labeling splits one physical defect
into two smaller components, each of which can fall below threshold - so
tile-local labeling would UNDERSTATE pair A's FIRE component and could sink a
real separation. For the calibration to measure the defect it targets, brick 1
MUST stitch components across the shared tile edge (or rasterize a padded
neighborhood spanning the adjacent tiles). Concretely: for the calibration tile
set, assemble the four historical spike tiles into a single common pixel canvas
(they are a contiguous 2x2-ish block at their zooms) and label components on the
stitched canvas, so a seam-crossing notch is one component. Minimum viable form:
stitch the specific `z4/8/3`+`z4/8/4` vertical pair. Full cross-tile stitching
for arbitrary tile sets is a later-brick concern; calibration only needs the
apex block stitched.

### Pin the apex ROI so pair A's FIRE is the historical spike (not some other diff)

Codex finding 5: DP and VW differ along the ENTIRE coastline, so "a large
component appears somewhere in one of four tiles" does NOT prove the measured
component is the notch. Brick 1 MUST pin a pixel-space ROI (bbox) around the
known apex and assert that (a) the largest component OVERLAPS the ROI and (b) it
is predominantly `ref_only` (ocean in the VW reference, land in the DP test - the
coverage gap). The apex is documented in `notes/ocean-coastline-spike-problem.md`:
feature id=551 at z4/8/3, notch apex x=3857, shoulders near x~3910, vertical
extent y~2205..2329 in 4096-extent tile units. Convert to pixels at the swept RES
(`px = tileUnit * res/4096`) and use a small margin. If the largest component is
elsewhere in the tile, pair A has NOT fired on the defect and calibration is
INVALID regardless of component size.

### Sweep RES and connectivity in BOTH directions

Do NOT fix RES=512 by fiat. Opus R1 (major smell): the note's core insight is
that the confound "does not survive rasterization" at ~256px display precision,
but 512 is 2x FINER than the 256px display grid where the defect is DEFINED. A
thin fjord that vanishes at 256 (a pixel center misses a sub-display-pixel
channel) can become a 1-2px-wide surviving connected streak at 512 - inflating
pair B, the binding constraint, exactly the wrong way. And raising RES does NOT
keep a speck a speck: for a thin-but-long feature, more resolution lengthens the
streak in pixel count while its width crosses the 1px threshold. So the original
spec's "a speck stays a speck at any resolution" is FALSE for long thin features.

Therefore brick 1 sweeps:
- RES in {256, 512} - 256 matches the display grid where the defect is defined
  and collapses thin fjords MORE; 512 gives finer notch resolution but risks
  reifying fjord streaks. Try both; do not assume 512.
- connectivity in {4, 8} - 8 keeps a diagonal spike as one component (helps pair
  A FIRE) but also merges nearby specks/jitter into larger components (hurts pair
  B CLEAR, the binding constraint). Since CLEAR binds, 4-connectivity may be the
  safer default. Try both; do not default to 8 on FIRE-side reasoning alone.

The winning (RES, connectivity) is the pair that MAXIMIZES separation
(pair A component / pair B component). Record the full 2x2 sweep, not just the
winner.

### The calibration itself - pinned reproduction

The three calibration archives are ABSENT from disk and MUST be regenerated
(norway `locations` extract). All commands below are copy-pasteable; the script
runs from `scripts/validate/`, so archive paths begin `../../data/` (codex
finding 3).

PIN THE OCEAN-INPUT STATE (codex 2, plus AGENTS.md ocean-artifact note): ocean
output depends on whether the durable `data/ocean-tiles.pmtiles` artifact is
active (artifact-served interior tiles differ benignly from extract-computed
ones). All three archives MUST be built in the SAME artifact-vs-shapefile state,
or the pair-B diff will show artifact-vs-computed seam differences that are not
simplification loss and will corrupt the null score. Pin ONE state for all three:
prefer the direct-shapefile path (pass `--ocean`/`--ocean-simplified`, no
artifact) so every archive computes ocean the same way and pair B isolates
simplification alone. Record which state was used in the Calibration record.

Regenerate (only if absent; the one place a pipeline run is required):

- Known-BAD (DP, pre-VW). Build elivagar at a pre-VW commit - the VW simplifier
  landed at `31b8298`, so any earlier commit produces DP ocean; use `3f4ca38`
  (the commit the archive is named for):
  `brokkr tilegen --commit 3f4ca38 --dataset norway --variant locations -o data/tilegen/norway-3f4ca38.pmtiles`
  (produces `data/tilegen/norway-3f4ca38.pmtiles`).
- Known-GOOD (VW). The known-good IMPLEMENTATION is `31b8298`, NOT `41d0227` -
  `41d0227` is the SPEC commit only (touches `notes/*.md`; verified: no `src/`
  changes), so it cannot have produced a VW archive (codex finding 2):
  `brokkr tilegen --commit 31b8298 --dataset norway --variant locations -o data/tilegen/norway-31b8298-vw.pmtiles`
  (produces `data/tilegen/norway-31b8298-vw.pmtiles`).
- Verbatim REFERENCE (`--no-ocean-simplify`). There is NO `brokkr` passthrough
  for `--no-ocean-simplify`, so this needs a DIRECT `elivagar` run with no brokkr
  wrapper (as `scripts/ocean-coverage.sh` documents). Build the same
  known-good code (31b8298) with the simplifier disabled. Resolve the norway
  `locations` PBF path from brokkr.toml
  (`norway-20260225-seq4709-locations-prepass.osm.pbf`) and the ocean shapefile
  from `data/`, then:
  `elivagar run data/norway-20260225-seq4709-locations-prepass.osm.pbf -o data/tilegen/norway-31b8298-ref.pmtiles --locations-on-ways --no-ocean-simplify --ocean <ocean.shp> --ocean-simplified <ocean-simplified.shp>`
  (produces `data/tilegen/norway-31b8298-ref.pmtiles`). Pin the exact `--ocean`
  paths used, into the Calibration record.

### Pair A - MUST FIRE

```
node render-gate.mjs ../../data/tilegen/norway-3f4ca38.pmtiles \
     --reference ../../data/tilegen/norway-31b8298-vw.pmtiles \
     --tiles z4/8/3,z4/8/4,z3/4/2,z2/2/1 --res <R> --connectivity <C>
```

Diffs known-bad (DP) against known-good (VW). PASS of calibration = the largest
stitched component OVERLAPS the pinned apex ROI, is predominantly `ref_only`, and
is large. Record its px per (RES, connectivity) cell.

### Pair B - MUST CLEAR

```
node render-gate.mjs ../../data/tilegen/norway-31b8298-vw.pmtiles \
     --reference ../../data/tilegen/norway-31b8298-ref.pmtiles \
     --tiles z4/8/3,z4/8/4,z3/4/2,z2/2/1 --res <R> --connectivity <C>
```

Diffs known-good (VW) against the verbatim full-detail baseline - the NULL
HYPOTHESIS the coverage oracle skipped. Aggregate disagreement is LARGE (this is
what scored 100K+ 2x-px^2), but the largest connected COMPONENT is the base-rate
floor a correct build produces. THIS is where the long-thin-fjord confound (Opus
major) will show up if it is going to. Record max component px per cell.

### The proceed decision (the go/no-go)

For each (RES, connectivity) cell compute separation = A_component / B_component.
Pick the best cell.

- GO iff `A_component >= 4 * B_component` in that cell (the true bar the
  threshold math implies - see "The honest premise") AND pair A's component
  overlaps the apex ROI and is predominantly `ref_only`. Then set
  `--threshold-px` = round(geometric mean of A and B) and CONFIRM it clears B by
  >= 2x. Record A, B, the chosen (RES, connectivity), and the threshold in the
  Calibration record. Proceed to the durable-gate bricks.
- NO-GO if no cell reaches `A >= 4B`, or pair A never fires on the apex ROI.
  Then the render-gate approach is REFUTED for this defect: record the full 2x2
  sweep and the negative verdict in the Calibration record, add a one-line
  pointer from `notes/ocean-coastline-spike-problem.md` noting the render gate
  was calibration-tested and did not separate (mirroring the coverage-oracle
  demotion), and CLOSE the item. Build no durable machinery. The throwaway
  harness may be kept as a diagnostic or deleted; it is explicitly not a gate.

Determinism: re-running any cell yields identical component numbers (decode +
scanline fill are pure functions of the bytes) - that determinism is what makes
the byte-of-verdict categorical.

## Durable current-vs-blessed gate - LATER bricks (only after GO)

These bricks exist ONLY if brick 1 returns GO. This is instrument-first
sequencing, not deferral (codex P0): the durable gate codex's P0 wants IS in
scope, but there is no point building a bless flow and reference format around a
measure we have not proven separates. Once GO is recorded, build all of the
following. Only the human baseline rotation / initial reference-set APPROVAL is
user-gated; the machinery itself is built autonomously.

- Brick D1 - polished CLI + `--reference` PNG mode. Full CLI:
  `node render-gate.mjs <test.pmtiles> --reference <ref.pmtiles|dir> [--layer ocean] [--res R] [--threshold-px N] [--connectivity C] [--tiles ...] [--json]`.
  `--reference <dir>` diffs against a blessed PNG directory (the durable
  baseline) instead of a second archive - this is the current-vs-blessed standing
  gate the originating action asked for
  (`notes/ocean-coastline-spike-problem.md`). Exit 0 = no component reaches
  threshold, 1 = a component reaches it, 2 = usage/decode error. Robustness:
  when scanning the union of both archives, a tileId in one and absent in the
  other rasterizes to an empty (all-land) mask on the missing side, yielding a
  full-tile disagreement and a large component (correct FIRE for a dropped tile).
- Brick D2 - bless flow (built autonomously; only the baseline rotation is
  user-gated). Mirror `brokkr bless`: a subcommand/flow that renders the current
  ocean masks to a blessed PNG set under version control and records provenance.
  The tool and flow are built without asking; ROTATING the blessed baseline or
  approving the INITIAL reference set is the user's call, exactly as
  `brokkr bless` rotates the regress baseline. Do NOT bless a production
  reference set autonomously.
- Brick D3 - config-bound thresholds (codex 4). The calibrated threshold is valid
  ONLY at the calibrated (layer, res, connectivity). Component area scales ~res^2
  and 4-vs-8 connectivity changes component membership, so reusing the calibrated
  default at other knobs yields false verdicts. Bind the threshold to its
  (layer, res, connectivity) in config; any non-calibrated combination MUST
  either carry its own explicit `--threshold-px` or run advisory (no pass/fail).
  Until a threshold is set for the active knobs, the tool prints per-component
  measurements only and refuses a verdict - an uncalibrated threshold is exactly
  the failure this whole spec exists to prevent.
- Brick D4 - cheap `--tiles` path (codex 7). Restricted `--tiles` mode MUST call
  `PMTiles.getZxy()` per requested tile, NOT decompress-and-retain every tile of
  both archives (a norway archive is ~1.17 GB / ~600K tiles). Full-scan mode
  should merge directory streams and process one tile pair at a time rather than
  building two full `Map<tileId, bytes>`. Also memoize by payload offset so RLE'd
  low-zoom full-ocean tiles are not rasterized redundantly (earcut's `--unique`
  trick; Opus nit).
- Brick D5 - verbatim MapLibre classifyRings (codex 8). Replace the earcut-copy
  classifier with one that matches installed MapLibre's fill bucket exactly:
  preserve the outer ring and quickselect only indices AFTER it, rather than
  sorting the whole polygon before truncating. Report the >500-ring case
  explicitly per `notes/svg-corpus-plan.md`.
- Brick D6 - focused tests (codex 9). Cover what calibration cannot reach:
  missing-tile-on-one-side, raw (non-gzip) payloads, 4-connectivity, alternate
  resolution, threshold-equality boundary, malformed arguments, `--json` output,
  and scanline boundary conventions (half-open interval, pixel-center rounding).

## Findings folded (provenance)

Every valid finding from both reviews is folded above. Map:

- Opus major (specks-vs-blob may be geometrically false; long thin fjord = long
  thin connected component in pair B) -> "The honest premise"; drives the whole
  calibration-first restructure.
- Opus major (RES=512 finer than the 256 display grid, pushes the wrong way)
  -> "Sweep RES and connectivity"; RES swept in {256,512}.
- Opus GAP (pair A magnitude computed against wrong referent - DP and VW share
  apex, so the diff is a thin sliver, may not be "tens of px") -> "The honest
  premise" (A may be small) + apex-ROI pin.
- Opus SMELL (8-connectivity cuts both ways, only justified from FIRE side)
  -> connectivity swept in {4,8}, CLEAR-binds reasoning noted.
- Opus GAP (threshold math hides A >= 4B) -> stated as the true proceed bar in
  "The honest premise" and the go/no-go.
- Opus nit (example-table sign backwards: pair-A spike is `ref_only`, not
  `test_only`) -> corrected; pair A FIRE component is specified as `ref_only`.
- Opus nit (RLE redundant rasterization) -> Brick D4 payload-offset memoize.
- Codex P0 (defers the durable gate) -> reframed as instrument-first sequencing;
  the durable current-vs-blessed gate is Bricks D1-D6, built after GO.
- Codex P1 (reproduction unpinned; 41d0227 is spec-only, known-good impl is
  31b8298; archives absent) -> "Pinned reproduction"; commits, paths, and the
  verbatim-reference direct-binary command all pinned.
- Codex P1 (commands not copy-pasteable from scripts/validate) -> all commands
  use `../../data/` paths.
- Codex P1 (calibrated threshold invalid when knobs change) -> Brick D3
  config-bound thresholds.
- Codex P1 (pair A does not prove the component is the spike) -> apex-ROI pin in
  brick 1.
- Codex P2 (tile-local labeling undercounts border-crossing defects; spike
  straddles the seam) -> "Cross-tile stitching is REQUIRED"; mandatory for the
  calibration, not just the later gate.
- Codex P2 (`--tiles` not cheap under the map-everything flow) -> Brick D4
  getZxy path.
- Codex P2 (classifyRings not verbatim, >500-ring divergence) -> Brick D5;
  brick 1 may use the copy but must WARN on >500 rings.
- Codex P2 (verification omits most behavior; brick ordering contradictory)
  -> Brick D6 focused tests; brick ordering rationalized (calibration harness
  first, full CLI in D1).

### Findings REJECTED

None rejected outright. The only reframing (not a rejection) is codex P0: the
spec does not "defer" the durable gate in the contract-violating sense, because
building bless machinery around an unproven measure is the exact mistake the
oracle discipline forbids. The durable gate stays fully in scope as Bricks
D1-D6, sequenced after a real go/no-go that closes the item cleanly if the
premise fails - which is instrument-first discipline, the same shape the earcut
and boundary-line oracles earned before they gated.

## Survey of the ground

- `scripts/validate/earcut-oracle.mjs` - the structural template.
  `BufferSource`, `allTiles()`, tile-slice/gunzip decode, `calculateSignedArea`,
  `classifyRings(rings, 500)` are copied verbatim into brick 1 (with the >500-ring
  caveat above). Third oracle in the `scripts/validate/` family (earcut,
  boundary-line, render-gate) if it graduates; follows their CLI/exit conventions.
- `notes/svg-corpus-plan.md` - both viewers fill with NONZERO winding over all
  rings of a feature and classifyRings is the sanctioned grouping;
  `src/svg.rs` already uses `fill-rule="nonzero"`. The pass-3 scanline fill
  implements the same semantics. The corpus plan's guardrail ("do not drift into
  thresholding a continuous quantity - the coverage false-negative trap") is
  honored by gating on component size (categorical), never aggregate pixels.
- `src/ocean_coverage.rs` / `scripts/ocean-coverage.sh` - the demoted diagnostic
  this gate would REPLACE as the standing ocean gate IF calibration says GO. Not
  torn out either way (keeps its triage role). No Rust changes in any brick.
- `notes/ocean-coastline-spike-problem.md` - the failure history: the defect (DP
  over-generalization), the exact spike geometry (feature 551, apex x=3857), the
  three calibration builds, and why aggregate-area gating fails. This spec
  re-proposes nothing that note logged as failed: no `OCEAN_VW_AREA_2X` tuning,
  no VW-vs-DP or smooth-control normalization as a standing gate, no reuse of the
  coverage measure as a gate.
- Latent cross-piece seam (same note): a render gate that survives calibration
  would catch an opened seam as a visible pixel component no current gate sees -
  a beneficiary, not a target, of this spec, and only real if brick 1 says GO.

## Standing references

- Contract: `reference/technical-implementation-spec.md`.
- Source problem: `notes/ocean-coastline-spike-problem.md` (recommended-action
  item 1 and the "Why every automated verdict was wrong" reckoning).
- Mechanism: `notes/svg-corpus-plan.md` (classifyRings + nonzero winding).
- Calibration discipline: `AGENTS.md` "Oracle discipline".
- Template instrument: `scripts/validate/earcut-oracle.mjs`.
- Reviews folded: `notes/render-gate-review-r1-opus.md`,
  `notes/render-gate-review-r2-codex.md`.
- This spec touches no measured path (`reference/performance.md` /
  `.brokkr/results.db`): it adds a Node script only, so it owes no benchmark; the
  neutrality gate is that `brokkr check` and `elivagar verify` are unaffected
  because no Rust changes.

## Calibration record

To be filled by the implementer when brick 1 runs (leave TODO until then; an
empty record means the tool is ADVISORY and may not gate, and the item's go/no-go
is undecided):

- Ocean-input state pinned for all three builds (artifact vs direct shapefile):
  TODO   `--ocean` path used: TODO
- Sweep (max stitched component px per cell):
  - res=256 conn=4: A=TODO B=TODO sep=TODO
  - res=256 conn=8: A=TODO B=TODO sep=TODO
  - res=512 conn=4: A=TODO B=TODO sep=TODO
  - res=512 conn=8: A=TODO B=TODO sep=TODO
- Pair A component overlaps apex ROI: TODO   predominantly `ref_only`: TODO
- Best cell (max separation): TODO   separation A/B: TODO   (GO requires >= 4)
- Verdict: GO / NO-GO: TODO
- If GO - chosen (res, connectivity): TODO   `--threshold-px`: TODO
  (geometric mean, confirmed >= 2x pair-B floor)   dual exit-code check:
  pair A exit 1 = TODO, pair B exit 0 = TODO
- If NO-GO - render-gate approach refuted; pointer added to
  `notes/ocean-coastline-spike-problem.md`: TODO
