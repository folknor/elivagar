# Ocean coastline spike: problem statement + root cause

Written 2026-07-12. This is the ACTUAL bug the "Norway spike" report was about.
It is NOT the boundary-line defect chased in
`notes/norway-boundary-spike-investigation.md` - that was a real but separate
defect (thousands of merger palindromes / duplicate borders, now fixed) that was
misidentified as this one. See "The false trail" below.

Root cause is now RESOLVED (dual investigation by codex + a Fable agent,
reconciled and confirmed by direct tile decode). See "Root cause" below.

## Symptom

On mainland Norway's coastline, thin spikes shoot out at low zoom (z1-5, easiest
at z4), described originally as "an SVG path node being flipped." They are
**exclusively land-colored** (the beige land background), poking out into the sea.
They are stable - "always been there" - and were long dismissed as "the PBF only
containing denmark/local data," which is wrong (see the proof below).

## The base-layer model (the framing that was the historical source of confusion)

In a MapLibre/PMTiles viewer the first paint layer is a style `background` layer -
a solid fill in the **land** color - not in the PMTiles at all. Land is therefore
**never encoded as geometry**; at low zoom the only area layer present is `ocean`
(the `land` layer starts at z7).

- The PMTiles stores the **water** (`ocean`, from `water-polygons-split-3857`).
  **Land is the complement** - the background showing through wherever an ocean
  polygon is not drawn.
- The **coastline is the edge of the ocean polygon** against the background. There
  is no "coastline" or "land" layer.

So a "spike coming out of Norway" has no land/coastline layer to live in; it can
only be a defect in the **ocean polygon's boundary**. And because it is
**land-colored**, it is a **coverage gap**: the ocean polygon fails to cover water
it should, and the background bleeds through as a thin concave notch.

## The false trail (recorded so it is not repeated)

The investigation first found a striking, real defect in `boundaries` - out-and-back
palindrome spikes fabricated by the MVT line merger from duplicated boundary edges -
and assumed it WAS the reported spike without confirming against what the viewer
shows. It was fixed (palindromes/duplicates 0 at the reported tiles), but the
coastline spikes remained, because they were never boundaries. Lesson: confirm the
visible defect's **layer and color** against the base-layer model before committing
to a fix.

## Proof it is the ocean, and provably PBF-independent

The **denmark** extract PBF contains **zero Norway OSM data**, yet the identical
Norway coastline spike is present on Norway in the denmark-built archive. The only
thing that can draw geometry on Norway in a denmark extract is the **global ocean
data**. The same data feeds the norway build. So the spike is sourced entirely
from the ocean geometry and is independent of the extract, the PBF, and everything
OSM-derived. The denmark build is the proof.

## Which ocean, and which build

The low-zoom ocean is fed from the **pre-simplified** `simplified_water_polygons.shp`
for **z0-7** and the full-resolution shapefile for **z8-14**. Two consumers exist:
the runtime shapefile path (`elivagar run` -> `ocean.rs` -> `geometry/int_ocean.rs`)
and the precomputed artifact `data/ocean-tiles.pmtiles` (built once by
`elivagar ocean-build`, normally merged into every extract). The artifact was moved
aside to `.disabled`, so the recent `norway-3f4ca38` / `denmark-3f4ca38` builds
used the shapefile path; the artifact (`.disabled`) was inspected and has the same
spike. Both consumers funnel the same shapefile through the same `int_ocean` code,
so the defect is in that shared engine.

## Root cause (RESOLVED)

**Per-zoom Douglas-Peucker over-generalization of a single ocean polygon, at a
fixed tolerance `OCEAN_DP_TOL_PX = 16` (one rendered pixel).** Stage:
`emit_cell` -> `rescale_shape_pinned_into` -> `simplify_shape_dp`
(`geometry/pyramid.rs:324`, `geometry/int_ocean.rs:20`).

DP bounds *perpendicular* deviation, not the *length* of a removed narrow feature,
and 16 units is constant at every zoom. So at z4 (16 units ~= 9.8 km projected) a
long thin sub-pixel-wide fjord / peninsula / coastal zigzag collapses into a
straight chord tens of km long. That chord cuts ocean coverage on the water side;
land is the background, so the removed water reads as a thin land-colored notch.
The result is a valid, non-self-intersecting ring - exactly why earcut passed
(earcut flags self-intersections, not a vertex in the wrong place).

**Confirmed by direct decode, not agent report.** In `z4/8/3` the conspicuous
notch is one emitted feature (`id=551`), and its outer ring holds the vertices
`(3926,2205) (3914,2275) (3899,2232) (3857,2272) (3882,2295) (3911,2285)
(3910,2329)` as a **consecutive run within a single path** - verified in the
rendered ocean SVG (path `p18`), not spanning two abutting pieces. codex traced
those emitted vertices back to source feature 3877's outer ring at indices
566/593/603/655/680/686/701 - i.e. the emitted ring skips 26/9/51/24/5/14
intermediate source vertices that DO exist in `simplified_water_polygons.shp`. So
the coverage-cutting chords are DP deletions, absent from the source. The source
shapefile passes GDAL `ST_IsValid`/`ST_IsSimple` for all Norway-area features.

**Zoom confinement.** The 16-unit tolerance is half the projected distance at each
higher zoom; a narrow feature below the one-pixel threshold at z1-5 becomes
representable again around z6-7 - so the spikes thin out by ~z6, *before* the z8
full-res transition, not because of it.

## Second, separate defect: latent cross-piece seam (Fable)

Ocean sets `pins: None` in `ocean_params` (`ocean.rs:1172`); the only DP pins come
from `build_edge_flags` (`pyramid.rs:937`), which pins only the *current cell's*
tile-edge window plus `params.pins` junctions. So a boundary genuinely **shared**
between two source pieces, away from tile edges, is simplified independently on
each side and can open a seam. The ocean shapefile path is the one polygon producer
with shared edges and no pin source - contrast the OSM path's `SharedNodePins-v1`
(`notes/simplify-then-reconcile-design.md`). This is NOT the observed Norway spikes
(those are within one feature), but it is a real latent bug that should get its own
guard. The fix machinery already exists unused: `quantize_polygon_pinned_into`
(`int_ocean.rs:132`) and `PyramidParams.pins`.

## Ruled-out stages (both agents agree)

- **Shapefile read** - verbatim ring coordinates after bbox filter; no repair,
  stitch, or simplify (`ocean.rs`).
- **Initial quantization** - `quantize_polygon` is a pure function of the source
  double; shared coordinates get identical integers. The world artifact takes the
  identity branch (`ocean.rs:1122`) yet still has the spike, so the data-bounds
  boolean is not the cause.
- **Shift-round rescale** - symmetric nearest rounding, bounded to <1 z4 unit;
  cannot produce long chords, and the spike vertices map back to real source
  vertices, not new rounded intersections.
- **Root bisection / cell clipping** - buffered rectangles at the pinned cell
  edges; the inspected notch is 170+ units from the tile edge, not a clip crossing.
- **Artifact merge** - both artifact and runtime builds show it; assemble does no
  geometry.

## Competitor precedent (fix directions)

- **Planetiler** consumes the *full* `water-polygons-split-3857` source (not a
  separately simplified shapefile) with a below-max-zoom tolerance of **0.1 tile
  pixel** - 10x tighter than our 1px. Its Natural Earth low-zoom path swaps in
  scale-appropriate ocean (110m z0-1, 50m z2-4, 10m z5) rather than hard-simplifying
  a fjord coast.
- **Tilemaker** uses the full source with **Visvalingam** (area-based) for ocean,
  not DP (`config-openmaptiles.json`).
- **Tippecanoe** keeps geometry within one tile unit and offers
  `--no-simplification-of-shared-nodes` (relevant to the latent seam).
- **Stedsplakat** merges coastline lines, nodes them with the viewport boundary,
  and polygonizes - a different low-zoom model, not directly comparable.

Fix space: tighten the ocean tolerance (16 is very loose vs 0.1px), and/or switch
to a coverage-safe / area-based simplifier (Visvalingam), and/or a
coverage-preserving rule so simplification cannot cut across water; plus,
separately, pin shared boundaries for the latent seam.

## Lateral findings

- `ocean.rs` header and AGENTS.md still say "scanline fill"; no scanline fill
  remains in the file.
- `ocean_dp_tol` and `ocean_min_area` are per-zoom closures that return constants
  (16, 256) at every zoom - the per-zoom hook exists but is unused; worth
  revisiting when tuning the fix.
- No oracle catches one-sided ocean coverage loss / thin concave notches (earcut
  cannot). The existing cell-seam XOR test
  (`seam_window_dp_tol_16_xor_empty_for_shared_window`, `pyramid.rs:1322`) shows
  cell seams were budgeted but *piece* seams and coverage loss were never modeled.

## Prior Visvalingam attempt (failure reconciled)

Visvalingam-Whyatt was tried before and reverted (2026-02-26). The full writeup
`notes/vw-simplification-experiment.md` was deleted in commit `8e0d77f`; recover it
with `git show 8e0d77f~1:notes/vw-simplification-experiment.md`. What actually
happened:

- It was a **global** swap of `for_each_zoom_simplified` - the **OSM** per-feature
  DP cascade - to VW, motivated purely by **performance** (that function was the #1
  CPU consumer). It did NOT touch the ocean path (`simplify_shape_dp` in the
  pyramid).
- Reverted because it was **slower** (+0.6s), for two reasons: (1) VW's per-call
  allocation (five vecs plus a heap) is dwarfed by DP's zero-alloc recursive scan
  for the **~10-vertex average OSM geometry**; (2) VW as implemented was
  **non-cascading** (filters the original at each zoom), so it produced **more**
  low-zoom output (+7 MB, +112K features) than DP's cascade, which double-simplifies
  z14 down to z0 and so yields less low-zoom data and less downstream work. A hybrid
  (small to DP, large to VW at 24 vertices) also failed to help.
- Crucially the failure was **perf plus output size, never correctness** - the
  writeup notes VW produced "more detailed (higher quality)" geometry. Its own
  conclusion: VW "would only help if the geometry distribution were different -
  fewer, larger geometries where O(n log n) vs O(n squared) actually matters."

That conclusion points straight at the ocean case, and none of the failure reasons
transfer:

- Ocean coastline rings are **large** (the located spike's source ring had 700+
  vertices) - exactly the "fewer, larger geometries" VW is for; the alloc overhead
  amortizes.
- Ocean is a **tiny fraction** of features and uses a **different** simplify
  function (`simplify_shape_dp`, untouched by the prior attempt), so the global OSM
  perf regression does not arise.
- VW's **area-based** metric is inherently more coverage-preserving than DP's
  perpendicular-distance metric - which is the exact cause of the long
  coverage-cutting chords here.
- VW producing "more low-zoom detail" is the **desired** direction for a coverage
  mask (the bug is too little coverage, not too much).

So VW is a well-motivated candidate. The Phase B spec must still (a) scope it to
ocean only, (b) measure the low-zoom ocean output-size increase against the perf
budget, and (c) decide cascading vs non-cascading - but it is applying VW to the
geometry class the prior experiment itself named as where VW wins, not re-proposing
the logged failure.

## Still open (for the fix, not the diagnosis)

- The **fix approach is Visvalingam**, scoped to the ocean simplify path.
  Tighter DP tolerance and a global DP-to-VW swap were both tried before and did
  not pan out, so the path is a targeted ocean-only VW, not a tolerance tweak.
  Remaining spec work: exact scope, cascading vs non-cascading, and the low-zoom
  ocean output-size measurement against the perf budget.
- A **coverage / notch oracle** to gate any fix (earcut is blind here).
- The **latent seam** guard - a separate item.

## VW fix attempt: gate results (2026-07-12) - FAILED, oracle in question

The VW fix (Landings 1-2, `OCEAN_VW_AREA_2X = 256`, committed at `41d0227` as a
spec + implemented uncommitted) was gated on norway via the shapefile path
(artifact moved to `.disabled`). Three archives: `<DP>` = `norway-3f4ca38`
(pre-fix DP), `<VW>` = `norway-41d0227-vw`, `<REF>` = `norway-41d0227-ref`
(`--no-ocean-simplify` verbatim baseline).

### Passed
- `brokkr check` green (VW + the discrimination unit test + the parser rewrite).
- earcut oracle on the VW ocean layer: exit 0, no tessellation regression.
- Coverage oracle DISCRIMINATION - the instrument prices the bug: `<DP>` vs `<REF>`
  at z1-6 shows large one-sided coverage loss concentrated at the exact spike
  tiles - z4/8/4 **188,793**, z3/4/2 **130,746**, z4/8/3 **80,436** 2x-px^2, all
  hundreds of times over the 512 threshold. The oracle genuinely detects the
  coverage-cutting DP chords where we found the spikes.

### FAILED - VW does not clear
- Coverage oracle `<VW>` vs `<REF>` at z1-6: STILL loses 100K+ at the worst tiles
  (z4/8/4 **146,342**, z3/4/2 **86,108**, z4/8/3 **65,120**). VW reduces the loss
  only **~20-40%** vs DP and does NOT clear the 512 threshold anywhere in z1-6.

### Geometry (z4/8/3, feature id=551, path p18)
- DP: 8 vertices, a sharp zigzag reaching the notch apex x=3857.
- VW: ~14 vertices, smoother and more faithful to the real coastline, but STILL
  reaching x=3857. VW improves fidelity; the notch region's outer extent is
  essentially unchanged.

### How to reproduce, and what to look for

All three archives are shapefile-path builds (artifact `.disabled`). The coverage
oracle runs via `scripts/ocean-coverage.sh` (builds a fresh elivagar, caches the
`--no-ocean-simplify` verbatim REF, runs `elivagar ocean-coverage`), because
brokkr has no wrapper.

- **earcut (RAN, passed):**
  `node scripts/validate/earcut-oracle.mjs data/tilegen/norway-41d0227-vw.pmtiles ocean`
  -> `0 over_thresh, 0 misattached` at every zoom 0-14, worst deviation 1.349e-4
  (z11), "No polygon exceeded the deviation threshold." VW ocean does not
  self-intersect.

- **Discrimination - DP must FIRE (RAN, confirmed):**
  `scripts/ocean-coverage.sh data/tilegen/norway-3f4ca38.pmtiles --zmin 1 --zmax 6`
  Output: one `zN/x/y: lost <N> 2x-pixel^2` line per offender tile, then a per-zoom
  `zN: max=.. p99=.. worst=x/y` summary. FIRES = large losses. Recorded worst per
  zoom: z1 74,043 / z2 57,054 / z3 130,746 (worst 4/2) / z4 188,793 (worst 8/4) /
  z5 234,073 / z6 192,119; the worst tiles are the spike tiles - z4/8/4, z3/4/2,
  and z4/8/3 = 80,436 - all hundreds of times over the 512 threshold.

- **VW must CLEAR - it does NOT (RAN, confirmed):**
  `scripts/ocean-coverage.sh data/tilegen/norway-41d0227-vw.pmtiles --zmin 1 --zmax 6`
  Recorded worst per zoom: z1 29,393 / z2 41,373 / z3 86,108 / z4 146,342
  (worst 8/4) / z5 141,523 / z6 117,131 - every zoom still far over 512, only
  ~20-40% below DP. This is the failed gate.

- **Geometry - the notch reaches the same extent (RAN, confirmed):**
  `brokkr svg --file data/tilegen/norway-3f4ca38.pmtiles -z 4 -x 8 -y 3 -l ocean`
  vs the same with `--file data/tilegen/norway-41d0227-vw.pmtiles`. In both,
  feature `id=551` path `p18` reaches apex x=3857: DP
  `...3899,2232 -> 3857,2272 -> 3882,2295...` (8-vertex sharp zigzag); VW
  `...3899,2232 -> 3877,2241 -> 3857,2261 -> 3869,2286 -> 3892,2295...` (~14
  vertices, smoother, same outer reach).

- **NOT yet run (would finish the gate):** the discrimination false-positive floor
  (`scripts/ocean-coverage.sh data/tilegen/norway-3f4ca38.pmtiles --zmin 12 --zmax 14`,
  expected near-zero, must PASS) and the VW high-zoom floor (same on
  `norway-41d0227-vw.pmtiles --zmin 7 --zmax 14`). These matter only if the oracle
  design is retained - see interpretation.

### Interpretation - three hypotheses (resolved by the visual gate below)
1. **The oracle conflates legitimate simplification with the spike (likely a
   design flaw).** The baseline is verbatim - every fjord and skerry. At z4
   (~10 km/px), correctly removing sub-pixel coastline detail loses large area vs
   verbatim, so ANY simplifier shows big "lost area" at low zoom, and the 512
   threshold ("a good simplifier loses ~0") is probably unachievable. The
   discrimination "worked" only because DP loses MORE than VW; both lose a lot. If
   this is right, the oracle measures total simplification loss, not the spike, and
   cannot gate the fix. The codex+Fable review validated this
   same-source/same-zoom-vs-verbatim design - it may have traded the cross-source
   confound for a legitimate-simplification confound.
2. **`OCEAN_VW_AREA_2X = 256` is too aggressive** - codex committed it without the
   deferred Landing-3 sweep; a lower value keeps more vertices. But even the
   smallest sweep value still loses SOME area vs verbatim, so this alone may not
   clear the gate.
3. **The fix is genuinely insufficient** - VW at the ocean tolerance still cuts
   coverage.

### The unanswered question
Whether the VISIBLE spike is actually gone needs the human visual gate; the
coverage oracle (aggregate lost area) cannot answer it, and the z4/8/3 geometry
shows VW smoother but reaching the same extent.

### Instrument gaps found while gating
- `brokkr tilegen` has no `--no-ocean-simplify` passthrough (it is an `elivagar
  run` flag); `brokkr ocean-coverage` and `brokkr ocean-build` wrappers do not
  exist. The spec's gate commands are not runnable via `brokkr` as written - the
  REF build and every `ocean-coverage` run had to invoke the `elivagar` binary
  directly. The brokkr wrappers are unbuilt bricks (brokkr is external to this
  repo).

### RESOLUTION - the fix works; the oracle is the flaw (visual gate, 2026-07-12)

The human visual gate settled it. Loaded in the viewer:
- `norway-41d0227-vw.pmtiles` (the VW fix): **looks perfect - the spike is gone.**
- `norway-3f4ca38.pmtiles` (DP, before): **has the spikes** (bug confirmed).
- `norway-41d0227-ref.pmtiles` (verbatim baseline): also looks good - expected,
  since it is the un-simplified full-detail coastline (no simplification, no
  artifact). It confirms the spike is purely a DP-simplification artifact that
  both VW (good simplification) and verbatim (none) avoid.

So **hypothesis 1 is confirmed and hypothesis 3 is rejected**: the VW fix
eliminates the visible spike, and the coverage oracle's failure to clear was a
FALSE NEGATIVE. The oracle measures one-sided lost area vs a verbatim baseline,
but at low zoom ANY correct simplification removes large sub-pixel coastline
detail vs verbatim, so the oracle cannot separate legitimate detail removal from
the spike. The 100K+ lost area on the VW build is legitimate low-zoom
generalization, not a defect; the `--threshold-2x 512` gate is unachievable by
design.

### Status
The VW fix is CORRECT (visual gate perfect, earcut clean); the L1 coverage oracle
is BROKEN as a gate. To land, the authoritative signals are the human visual gate
(passed), earcut (passed), and an `elivagar regress` ocean-only diff (pending).
The coverage oracle must either be redesigned to isolate the spike (e.g. compare
VW-vs-DP, or net one-sided loss beyond what the same simplifier removes on a
smooth control coast) or dropped as a gate and replaced by the visual gate on the
record. Do NOT tune `OCEAN_VW_AREA_2X` against the current oracle - it measures the
wrong thing. LANDED at 31b8298, gated on the human visual check plus earcut, with
the coverage oracle demoted to a diagnostic and the now-implemented spec removed;
the `elivagar regress` diff was skipped as redundant with the enum-level
ocean-only scoping. See "Why every automated verdict was wrong" below for the
validation reckoning this near-miss prompted.

## Why every automated verdict was wrong (validation reckoning, 2026-07-12)

The arc above has a worse story inside it: EVERY automated signal we built to
gate the VW fix reported FAILURE, and only a human loading the tiles and looking
confirmed the fix works. A Fable advisory agent was tasked with "what do we do
about that." Its findings, recorded here because they generalize past this bug.

### The oracle's real failure: an impossible pass criterion

The coverage oracle (`src/ocean_coverage.rs`) is a CORRECT instrument bolted to
an unachievable threshold. It thresholds one-sided lost water area at
`threshold_2x = 512` (one rendered pixel squared). But any correct 1-px-scale
generalization of a coastline legitimately loses ~ (coast length x sub-pixel
width). The recorded VW z4 worst of 146,342 2x-units backs out to roughly two
tile-widths of fjord coast - the "failure" magnitude is exactly what a CORRECT
simplifier must produce. The threshold sat two-plus orders of magnitude below
the legitimate floor. The gate was only ever calibrated on the BAD side (it
fires on DP at the spike tiles); nobody ran the null hypothesis - "what does a
known-good build score?" - which any measure merely correlated with
simplification aggressiveness would also have passed.

Structural insight: the oracle measured at 4096-unit tile precision a defect
that only exists at ~256-px raster precision. The sub-pixel legitimate loss that
sank it DOES NOT SURVIVE RASTERIZATION. Measuring in pixel space would make the
confound vanish structurally instead of needing a threshold to separate it.

### The trustworthy-oracle template (three properties, not one)

"Trust consumer-path oracles" is too weak: the coverage oracle IS consumer-path
(it decodes MVT correctly, ClosePath cursor rule included). The oracles that have
ever been authoritative here - earcut, boundary-line - share three properties;
the coverage oracle had only the first:

1. Consumer-path decode - reads geometry the way MapLibre does.
2. A CATEGORICAL defect with near-zero base rate on good output
   (self-intersection, misattached hole, palindrome, spur). Pass = count 0, and
   0 is actually achievable. The coverage oracle instead thresholded a
   CONTINUOUS quantity that is legitimately large on good output.
3. Calibration in BOTH directions before being trusted: fires on known-bad AND
   clears on known-good.

Every false verdict this project has produced missed property 2 or 3. The "false
trail" (the boundaries palindrome fix committed against the wrong bug) is the
same failure mirrored: a real signal acted on without confirming it was THE
rendered symptom. Unifying rule: the defect is defined at the renderer; a gate's
verdict counts only once its link to the rendered symptom is demonstrated, in
both directions.

### Recommended actions (ranked, not yet executed)

1. Formalize the visual gate as a blessed-render pixel diff. A new
   `scripts/validate/render-gate.mjs` in the existing oracle family: decode via
   `@mapbox/vector-tile`, rasterize the ocean layer to display resolution, diff
   against in-repo blessed PNGs on a curated tile set (the known-bad coords
   z4/8/3, z4/8/4, z3/4/2, z2/2/1 plus smooth-coast, archipelago, and full-ocean
   controls). Judge on CONNECTED-COMPONENT size, not aggregate pixel count - a
   1x50-px spike is one 50-px component, legitimate jitter is scattered 1-4-px
   specks. Re-bless flow mirrors `brokkr bless`; the human eye stays in the loop
   only at bless time. Cheapest change that would have flipped this episode, and
   it also catches the latent seam below.
2. Demote `ocean-coverage` from gate to diagnostic (its per-zoom worst-tile
   ranking genuinely located the spikes - keep it for triage). Strip the "must
   CLEAR" gate language from `scripts/ocean-coverage.sh`. Do NOT tune
   `OCEAN_VW_AREA_2X` against it.
3. Reject both earlier sketched reframings as STANDING gates. VW-vs-DP compares
   against a known-broken referent that vanishes once DP leaves the ocean path
   and can only certify "not worse than last time." Smooth-control-coast
   normalization is a geography-dependent fudge factor - a fjord coast
   legitimately loses far more per tile than any smooth control. If a
   geometry-level gate is still wanted, the right shape is a BASELINE-FREE needle
   detector on the emitted ring, analogous to the boundary oracle's spur
   detector: flag point pairs a sub-pixel straight-line distance apart but a long
   path-length apart, excursion on the land side. Categorical, no REF build - but
   the render gate covers the same defect class with less new machinery.
4. Codify an oracle-calibration protocol in AGENTS.md. Before any validator gates
   work: (a) it fires on the known-bad artifact, (b) it clears on a known-good
   artifact or defect-free control region, and (c) it measures the defect itself,
   or the correlation is demonstrated - including the null-score arithmetic before
   choosing any threshold. Advisory until all three. Nearly free, and it prevents
   both the false negative (this episode) and the false trail.

### Lateral findings (line refs unverified; symbols given so they survive drift)

- `src/ocean_coverage.rs` smell: `shape_area` treats every ring after the first
  as a hole regardless of winding. Holds today only because `encode_tile_shape`
  emits one feature per Shape (outer first, holes after); if ocean emission ever
  packs multiple outers into one MVT feature, `shape_area` silently subtracts
  outer areas and the numbers go garbage. If the tool survives as a diagnostic,
  classify rings by winding like the earcut oracle does.
- `src/ocean_coverage.rs` smell: `lost_area` sums pairwise intersections over all
  reference/low shape pairs - correct only while the low shapes are mutually
  disjoint (true for ocean pieces today, unstated in the code).
- Latent cross-piece seam (also flagged above): VW inherits `pins: None` exactly
  as DP had it, so a boundary shared between two source pieces away from tile
  edges is still simplified independently per side. The render gate (item 1)
  would catch an opened seam as a visible pixel component; no current gate would.
  Fix machinery exists unused (`quantize_polygon_pinned_into`,
  `PyramidParams.pins`).
- The per-zoom `ocean_dp_tol` / `vw_area_threshold` hooks still return constants
  at every zoom (the VW threshold ignores `dp_tol`'s magnitude entirely). The "16
  units at every zoom" shape of the original bug is therefore structurally still
  available to a future tuning mistake - another argument for a renderer-level
  standing gate rather than a parameter-level one.
- Stale doc, still unfixed: `ocean.rs` header and AGENTS.md both say "scanline
  fill"; no scanline fill remains in the file.

## Reproduction

- Shapefile-built archives already on disk (both show it):
  `data/tilegen/norway-3f4ca38.pmtiles`, `data/tilegen/denmark-3f4ca38.pmtiles`.
- Artifact (currently disabled, also shows it): `data/ocean-tiles.pmtiles.disabled`.
- Spike tiles on the Norway coast: **z4/8/3, z4/8/4, z3/4/2, z2/2/1**.
- Render the ocean layer of a tile:
  `brokkr svg --file <archive> -z <Z> -x <X> -y <Y> -l ocean`
  (land-colored spike = a thin concave notch in an otherwise smooth ocean fill
  edge; e.g. z4/8/3 path `p18`, the `(3857,2272)` apex).
- A fresh shapefile build: `brokkr tilegen --dataset norway --variant locations`
  (falls back to the shapefile while the artifact is `.disabled`).

## Shipping implication (for when a fix exists)

A fix in `int_ocean` is reproducible/iterable via the shapefile path without
rebuilding the artifact. But shipping requires rebuilding the artifact
(`elivagar ocean-build`) and re-blessing, because the blessed regress baseline is
artifact-active and rotating `ocean-tiles.pmtiles` forces a bless rotation
(AGENTS.md; `notes/ocean-tile-stream-spec.md`).
