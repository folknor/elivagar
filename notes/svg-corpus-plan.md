# SVG regression corpus - plan

Transient planning note. Captures the discussion behind building an SVG-based
regression corpus for elivagar output. Not a spec.

## Goal

A regression corpus of per-tile SVG artifacts. For a given {z}/{x}/{y} elivagar
emits an SVG that faithfully represents what the user sees in the tilepeek
viewer, so a human can open it in Inkscape (or any SVG viewer) and judge
good/bad, and so builds can be diffed against a blessed baseline across commits.

Two reasons SVG wins over rasterized (PNG) snapshots:

1. Size. SVG paths are tiny versus PNGs, so thousands of corpus files can live
   in git, diffable as text with full history.
2. Diff highlighting is trivial. elivagar already decodes MVT to geometry, so
   when a diff is detected it can re-emit an SVG with the differing features
   colored pink. No raster diffing, no browser, and the highlight is exact
   because the diff is geometry.

## Viewer landscape (what we are matching)

tilepeek (research/tilepeek) is the actual viewer. It is a standalone Rust HTTP
server that serves the pmtiles blobs raw and a viewer page. It now offers two
render paths side by side:

- MapLibre GL JS v5 - WebGL, always rasterized. Polygons go classifyRings ->
  earcut -> GL triangles. No vector/SVG intermediate exists; you cannot extract
  SVG from its canvas, and a screenshot is a fuzzy PNG.
- OpenLayers Canvas VectorTile - keeps features vector-side, style-driven,
  fills via the HTML5 Canvas nonzero winding rule. Closer to what our SVG
  emitter does than MapLibre's GL path, so it is the right calibration
  reference: if elivagar SVG matches the OL canvas, the SVG is faithful.

All three (MapLibre, OL, elivagar SVG) decode the SAME MVT bytes. They only
diverge in how they interpret and paint that geometry. The SVG is a valid
stand-in ONLY if its geometry interpretation matches the viewers'.

## Geometry interpretation - the findings

Verified two ways: against the in-tree earcut oracle
(scripts/validate/earcut-oracle.mjs, which runs maplibre's classifyRings
verbatim as sanctioned tooling), and via a clean-room codex-review session
pointed at a fresh clone of OpenLayers (research/openlayers) that answered
abstractly without exposing source. Findings:

- Both viewers fill with NONZERO winding, not evenodd. This is the crux. evenodd
  happens to match nonzero for well-formed polygons with correctly alternating
  outers/holes, but it is not the viewers' semantics and disagrees on
  same-winding nesting and other malformed topology - exactly the ring-role bugs
  the corpus must catch. evenodd papers them over.
- OpenLayers' default Canvas path does not classify rings at all. It submits all
  of a feature's rings as subpaths of one path and lets the canvas nonzero
  winding-number rule cut the holes. Ring order is irrelevant; only relative
  winding matters (reverse every ring in a feature together and the visible fill
  is unchanged). So "all rings, one path, nonzero" is literally what OL does.
- classifyRings (maplibre) self-calibrates: the first nonzero-area ring defines
  the outer winding; rings matching it start a new polygon, opposite-wound rings
  become holes of the current one; zero-area rings are skipped; a maxRings=500
  clamp keeps the largest-area rings when a polygon exceeds 500. It is ~20 lines,
  derivable from the MVT polygon spec (exterior positive area, holes negative in
  screen space). No port of any viewer's rendering engine is required.
- The one valid-input divergence between the two viewers: MapLibre's 500-ring
  cap. OL keeps every ring; MapLibre drops the smallest holes past 500. A valid
  polygon with more than 500 rings genuinely renders differently in the two
  viewers, and no single SVG can match both. Everything else (antialiasing, line
  joins/caps, label and symbol collision) is raster-only, lives where no
  geometry bug does, and SVG neither can nor should reproduce it.

Practical rule for the emitter: MapLibre-style sequential grouping (classifyRings)
+ skip zero-area rings + nonzero paths. This matches BOTH viewers whenever a
polygon has at most 500 rings and MVT winding/order conform. Add an explicit
>500-ring check that flags "cannot match both viewers" rather than silently
picking one. Note nonzero-all-rings alone already gives OL-correct fill;
classifyRings is needed to mirror MapLibre's cap and to drive hole-misattachment
diagnostics (same reason the earcut oracle runs it).

## Already done

- src/svg.rs polygon fill switched from fill-rule="evenodd" to
  fill-rule="nonzero". This is the actual correctness bug and is correct
  regardless of the CLI rework.

## Corpus emitter design

Context: the current CLI verb surface is being nuked, so do not fit the emitter
to the existing svg/regress/bless verbs. The 500-ring check and classifyRings
grouping travel together into the new emitter, not bolted onto the doomed CLI
path.

### Source-of-truth decision

Do not invent a new baseline. The blessed pmtiles already exists as the regress
reference. The corpus is a derived, committed artifact:

- Geometry truth = the blessed pmtiles.
- Corpus = deterministic SVG renders of a fixed tile manifest from that pmtiles,
  committed to git as the human-viewable, diffable layer.
- Blessing the pmtiles and re-rendering the corpus are one rotation. No second
  baseline to drift.

### Shared render core

A library function render_tile(pmtiles, z, x, y, style) -> String, four passes:

1. Decode MVT geometry (already exists in svg.rs).
2. Group rings: port classifyRings - signed-area self-calibration, skip
   zero-area rings, 500-ring clamp, flag when exceeded (where the >500
   "cannot match both viewers" warning lives).
3. Style: a table keyed by layer + kind -> fill/stroke/opacity, mirroring
   tilepeek. Emit grouped rings as nonzero paths.
4. Canonicalize: sort layers by name, features by a stable geometry key, fixed
   coordinate precision.

Pass 4 is the parked TODO and is a HARD prerequisite. Within a commit, elivagar
output is already byte-identical (within-run record order is total), but across
commits features can legitimately reorder (e.g. a paint-rank table change) with
no semantic difference. Without canonical ordering, that reordering
false-positives every cross-commit diff. Reuse brokkr regress's canonicalization
approach.

### Three modes on top

- bless: render manifest from blessed pmtiles -> write corpus SVGs -> commit.
  The git commit is the bless.
- check: render manifest from the current build -> text-compare against the
  committed corpus. Cheap, scales to thousands. Any byte diff = candidate
  regression tile.
- overlay: for each changed tile only, geometry-diff current vs blessed (reuse
  regress's feature-match) -> emit a diff SVG with current-only features pink,
  blessed-only another color, unchanged faint grey. Full geometry diff runs on
  only the handful of tiles that changed.

Key split: text-diff finds WHICH tiles changed (cheap detection over the whole
corpus); geometry-diff attributes WHAT changed (precise, only on changed tiles)
for the pink overlay.

### Two things still to decide

- Manifest: a committed per-dataset list of z/x/y. Seed with representative tiles
  (coast, city, rural, boundary junction) plus a growing "hard tiles" section -
  every fixed bug drops its tile in, so the corpus accretes regression coverage.
- Style single-source: to guarantee the SVG matches tilepeek, ideally both read
  one committed style file. v1 can be a Rust table mirroring tilepeek's inline
  style, but that drifts; extracting a shared style file is the clean version.
  Decide when.

## Guardrails

- Keep semantic brokkr regress and the earcut oracle as the authority. The SVG
  text-diff is a valid gate only because it is categorical (bytes match or not)
  on deterministic output. Do not let it drift into thresholding a continuous
  quantity - that is the ocean-coverage false-negative trap.
- Reproducing viewer geometry interpretation (classifyRings + nonzero) is the
  part that must match. AA, joins, labels, and symbol collision are raster-only
  and are deliberately left as a simpler approximation.
