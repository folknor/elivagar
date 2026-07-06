# Performance hunt brief: integer polygon emission engine

Find the biggest remaining performance opportunities for this code
specifically. Optimize for the per-zoom polygon emission cost: every
polygon is quantized once to an integer base grid, and per zoom level is
rescaled, simplified, topology-normalized, and resolved into per-tile
clipped MVT geometry - multiplied across 15 zoom levels, millions of OSM
features, and ~5000 large ocean shapefile pieces that dominate the cost.
Output parity stays exactly: `elivagar verify` PASS, the earcut oracle at
zero deviant polygons and zero misattached holes on every polygon layer,
and per-layer/zoom feature counts within noise. That is the ONLY
observable contract; if it holds, a change is legal no matter what
internal shape it breaks. The Shape/Shapes data structures, the row-band
cutting, the per-tile boolean resolution strategy, and the scanline/PIP
tile-membership model that produce today's output are NOT sacred. You own
everything from a quantized base polygon onward - per-zoom derivation,
simplification, topology normalization, tile membership, row cutting,
per-tile clipping, interior-tile fast paths, and sort-record emission - at
the cheapest correct per-tile cost. The upstream PBF parsing and tag
matching that produce the polygons are out of scope.

## Very important framing

- This is NOT primarily a generic writer/API cleanup exercise.
- This is NOT primarily about preserving existing stage boundaries.
- This is NOT primarily about preserving existing internal APIs.
- If a later generic cleanup or generalization becomes obvious, that is a
  follow-up, not a constraint now.

## Constraints that DO apply

- Prefer structural / architectural opportunities over micro-optimizations.
- Focus on opportunities that could materially improve real throughput.
- Be explicit about which ideas are "full coherent rewrites" versus local
  changes.
- Assume pre-1.0: breaking internal API is acceptable.
- Assume we are willing to rewrite internals aggressively if the payoff is
  real.
- We do care about correctness and maintainability, but not about
  preserving old abstractions just because they already exist.
- We do care about codebase cleanliness: avoid proposing env-var-heavy
  experiment scaffolding as the default way forward.
- If proposing an experiment, prefer "make the full intrusive and complete
  change, benchmark, keep/revert" over tiny gated probes.

## Constraints that DO NOT apply

- Do not preserve structure just because it exists today.
- Do not preserve writer abstractions just because they are shared.
- Do not optimize for generic reuse first.
- Do not optimize for minimal or least-invasive change by default.
- Do not assume public/internal API stability matters pre-1.0.
- Do not center the analysis on hidden env vars, benchmark knobs, or
  temporary routing switches.
- Do not assume engineering time or resource constraints. We have
  unlimited resources.

## What I want from you

1. Read the relevant code deeply, along with other potential internal APIs
   the code is not currently taking advantage of (the i_overlay 7.0.2
   crate's full API surface is at
   `/home/folk/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/i_overlay-7.0.2/`
   - the code today uses only Simplify and Overlay::Intersect; buffer
   reuse, graph reuse, batch operations, and alternative extraction modes
   are unexplored).
2. Identify the biggest remaining optimization opportunities.
3. Prioritize opportunities that remove structural bottlenecks, stage
   seams, intermediate materialization, or serialized ownership/handoffs.
4. For each major opportunity, explain:
   - what the bottleneck is
   - why the current structure causes it
   - what the stronger end-to-end redesign would look like
   - what makes it plausibly high-payoff
   - what risks it carries
5. Distinguish clearly between:
   - high-conviction architectural rewrites
   - medium-value local changes

## Important style requirements

- Be opinionated.
- Prefer strong hypotheses over exhaustive laundry lists.
- Do not default to conservative or minimal suggestions.
- If you think the right move is a big rewrite, say so directly.
- If you mention generic improvements, frame them as "afterwards, if this
  works".

Please refrain from reading other Markdown documents (this file is the
one exception), git log, or previous optimization attempts, because we
have probably not attempted them properly. Read CODE, not history.

Do NOT build, run, test, or benchmark anything - analysis only; the main
conversation owns all gates and measurements.

## Ground truth (measured, host: 24-thread Ryzen 5900X)

Full Denmark pipeline bench: 73.9s wall total; the ocean phase is 50.2s of
it. Function-level profile (hotpath, inclusive CPU across threads; wall
~78s, so >100% totals = nested/parallel):

```
Function                                   Calls        Avg       P50        P95        P99     Total
pipeline::phase12::process_raw_way       6616526   20.94 us   5.21 us   94.08 us  273.41 us  138.57 s
int_ocean::intersect_rect                2069501   63.39 us   4.62 us  100.09 us  509.18 us  131.18 s
int_ocean::emit_shape_for_zoom             24921    3.66 ms   1.60 us    2.22 ms   61.87 ms   91.33 s
ocean::emit_ocean_polygon                   3449   26.44 ms  48.29 us   33.85 ms  660.60 ms   91.18 s
ocean::process_ocean_shapefile                 2    25.26 s  115.4 ms    50.40 s    50.40 s   50.51 s
int_ocean::cut_row_bands                    8527    5.65 ms  12.78 us   10.03 ms  128.45 ms   48.16 s
pipeline::emit::emit_polygon_feature     4548851    8.93 us   1.26 us   33.41 us  145.41 us   40.60 s
int_ocean::simplify_shape_dp            14722118     641 ns     10 ns    1.31 us    3.13 us    9.45 s
int_ocean::normalize                      586252   12.58 us   1.87 us   14.39 us   40.77 us    7.38 s
```

Reading hints from the numbers (verify against code, do not trust me):
`intersect_rect` is the headline - 2.07M calls, 131s CPU, with a heavy
tail (P95 100us, P99 509us: the large-input calls dominate). It is called
from three places: cut_row_bands internal bisection cuts, per-tile clips
of boundary/gap tiles, and Tier-2 OSM feature clipping. `normalize` and
`simplify_shape_dp` are NOT the problem (17s combined). The per-zoom loop
re-derives everything from the base shape at every zoom independently.
Every intersect_rect call constructs a fresh i_overlay Overlay (allocation
+ sweep setup) and materializes owned output Shapes.

## Scope pointers (code to read deeply)

- `src/geometry/int_ocean.rs` - the whole engine (quantize, rescale, DP,
  normalize, intersect_rect, cut_row_bands, emit_shape_for_zoom,
  encode_tile_shape, tiering helpers).
- `src/ocean.rs` - ocean orchestration (pre-split, scanline, rasterize,
  boundary/gap/fast-path emission, rayon fold + chunk flushing).
- `src/pipeline/emit.rs` - OSM polygon paths (three tiers) + their zoom
  loops; line emission is out of scope.
- `src/mvt/mod.rs` encode_polygon + `src/sort.rs` SortRecord push (the
  emission sink - included in "you own everything onward").

## Report

Reply with a full report per "What I want from you". Target length: the
substance, no padding; every claim tied to a file/function you actually
read.
