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
