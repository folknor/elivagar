# E1: input-shaped fast path on the single-contour normalize

Implementation specification.

## Standing references

- Contract: `reference/technical-implementation-spec.md` (this document is
  written against it; every brick names its gate, its keep/revert path, and
  its concrete artifact).
- Source item: `notes/planet-30gb-roadmap.md`, the H6 post-port engine
  surface, bullet **E1: input-shaped fast paths on the 11.4M-call
  normalize** (and the two framing paragraphs above it, "Why engine CPU is
  planet-relevant" and "The gate insight that reshapes what forbidden
  meant"). The kill-list context and the tol-0 / differential-oracle gate
  regime E1 lands under are stated there.
- Measurement record: `reference/performance.md` plus `.brokkr/results.db`.
  The alloc call-shape figure E1 is priced against (denmark: 11.41M
  `normalize_into` calls, alloc `546b9d58`) is quoted in the roadmap's
  framing paragraph.
- Geometry failure ledger: `notes/rendering-postmortem.md` (the R23 ClosePath
  and land-layer material lives here; there is no `rendering-fix-log.md`). The
  convex-ring soundness argument reused here (condition-1-alone is unsound for
  a self-lapping spiral) is the land-layer defect recorded there and encoded
  in `is_convex_ring`'s doc comment in `src/geometry/pyramid.rs` (the doc
  comment at the top of `is_convex_ring` is the durable in-code statement of
  it; cite that, it will outlive the note).

## Problem statement

`normalize_into` (`src/geometry/int_ocean.rs`) is rank 4 in the norway
hotpath (129.2 thread-s) and runs inside ranks 1-2 (the coastal/relation
stack). On denmark it is called 11.41M times. Its single-contour arm
(`shape.len() == 1`) delegates to `BoolOverlay::simplify_contour_into`,
which forwards to the port engine's `Overlay::simplify_contour_into`
(`src/geometry/overlay/port/simplify.rs`).

That function already has a "perfect input" verdict: `find_intersections`
builds segments, runs the split solver, and tests for repeated vertices;
when the input is already simple, non-collinear, and correctly wound it
returns `false` and the caller keeps its own allocation (no output rebuilt).
But reaching that verdict still pays the full segment build
(`append_path_iter`) + split solver (`split_segments`) + loop test
(`test_contour_for_loops`) on every call. On a ring that is already simple and
correctly wound - tile-space rings are typically tiny and usually simple -
that entire pipeline is spent to confirm the ring was already fine.

Where the win actually lives (corrected after review R2). `emit_cell`
(`src/geometry/pyramid.rs`) already proves the pattern pays: its
`is_convex_single_ring` screen skips `normalize_into` entirely for convex
single rings before the call is ever made - but ONLY when `dp_tol > 0`
(`pyramid.rs`, the `if dp_tol > 0 && is_convex_single_ring(...)` guard).
Crucially, `is_convex_single_ring`/`is_convex_ring` accepts a SUPERSET of what
E1's screen accepts: it tolerates collinear vertices and is winding-agnostic,
whereas E1's screen is strictly convex and CCW-only. So on the `dp_tol > 0`
emit path E1's fast path is UNREACHABLE - every ring E1 would accept has
already been peeled at that guard. The spec's earlier claim that `emit_cell`
peels a "strict subset" was backwards; it peels a superset there.

E1's reachable production opportunity is therefore the calls that do NOT hit
that guard:
- the pyramid root normalization (`pyramid.rs` ~line 178, `dp_tol` 0 at the
  root), and
- `dp_tol == 0` cell emission (the guard requires `dp_tol > 0`, so at tol 0
  every single-contour ring, convex or not, reaches `normalize_into`), and
- the ocean / `int_ocean` production callers of `normalize_into` that do not
  route through `emit_cell`'s screen at all.
E1 still also catches non-convex-but-simple rings on any path (those the
convex screen never peels), and CCW-simple rings on the `dp_tol == 0` path.
The instrument in Brick 2 is what confirms the surviving volume is worth the
landing; the "which callers dominate" question is now an empirical one the
Brick 2 counters (broken out by the buckets below) answer, not an assumed one.

## Survey of the ground

### The single-contour arm and its verdict

`normalize_into` (`src/geometry/int_ocean.rs`; `fn` header ~line 535, the
single-contour arm shown below at ~line 547). The `simplify_contour_into` in
this snippet is the 2-arg `BoolOverlay` wrapper (`overlay/mod.rs:63`), which
forwards to the 3-arg port function the wiring section targets - two layers,
same name:

```rust
if shape.len() == 1 {
    if scratch.overlay.simplify_contour_into(&shape[0], out) {
        scratch.overlay.recycle_owned_shape(shape);
        clean_shapes_in_place(scratch, out, min_area);
        return;
    }
    clean_shape_in_place(scratch, &mut shape, min_area);
    if shape.first().is_some_and(|outer| outer.len() >= 3) {
        out.push(shape);
    } else {
        scratch.overlay.recycle_owned_shape(shape);
    }
    return;
}
```

`simplify_contour_into` returns a bool:
- `true` -> the engine rebuilt the geometry into `out` (either the reversed
  perfect contour, or the fully re-noded shapes). Caller cleans `out`.
- `false` -> the input was already perfect AND already wound CCW. `out` is
  untouched; caller cleans and pushes its own `shape`.

The bool is exactly the None/Some verdict the differential oracle asserts
(`src/geometry/overlay/mod.rs`, `differential_oracle_reused_engine_recycles`,
the `port_rebuilt` assertion at line ~254 vs `oracle_simplify`). Any screen
that changes when `false` is returned is caught there, point-for-point
against dev-dep i_overlay.

### What "perfect + correct winding" means, exactly

`Overlay::simplify_contour_into` (`src/geometry/overlay/port/simplify.rs`):

1. `find_intersections(contour)` returns `is_perfect`, true iff ALL of:
   - `append_path_iter(..)` returned `false`: no point was dropped. With
     `preserve_input_collinear = false` (the elivagar option, set in
     `overlay_options`, `src/geometry/overlay/mod.rs`), the filter is
     `DropCollinear`, which drops any vertex whose incoming/outgoing edges
     are collinear (`cross_product == 0`), including consecutive duplicates.
     So "not modified" means: no cyclic triple is collinear and no
     consecutive vertices coincide.
   - `split_segments(..)` returned `false`: no segment pair crossed or
     overlapped (no self-intersection).
   - `segments` non-empty.
   - `test_contour_for_loops(..)` returned `false`: no vertex is repeated
     anywhere in the ring (catches vertex-touching figure-eights that do not
     cross).
2. If `is_perfect`, `contour_direction` decides:
   - contour CCW (`unsafe_area > 0`, i.e. `is_clockwise_ordered()` false),
     output direction CCW -> `Correct` -> **return `false`, `out`
     untouched**. This is the case E1 fast-paths.
   - contour CW -> `Reverse` -> push the reversed contour, return `true`.
3. If not perfect, run the full boolean overlay (`build_boolean_overlay` +
   `extract_shapes_into`), return `true`.

### The existing convex screen and why E1 needs a stricter one

`is_convex_ring` (`src/geometry/pyramid.rs`, around line 1024) proves a ring
is convex and winds exactly once, using two conditions:
1. no reflex vertex (every non-collinear cyclic triple shares one cross
   sign), and
2. exactly two x-direction sign flips and two y-direction sign flips (winds
   once, not a self-lapping spiral - condition 1 alone is unsound, the
   land-layer defect in `notes/rendering-postmortem.md` and in
   `is_convex_ring`'s doc comment).

It is NOT sufficient for E1 as-is, for two reasons:
- It SKIPS collinear triples (`if cross == 0 { continue; }`), so a ring with
  a collinear vertex passes `is_convex_ring` but is NOT perfect to the engine
  (`append_path_iter` would drop the collinear point, `is_perfect == false`).
- It is sign-agnostic, so it passes both CW and CCW convex rings, but only
  CCW yields the `Correct`/`false` verdict.

E1 needs a strictly-convex, sign-fixed screen: every cyclic triple cross
product strictly positive (proves no collinear, no consecutive dup, convex,
AND CCW winding in one pass), plus the two-x-flip / two-y-flip revolution
check (proves simple, not lapping). This exactly reproduces "perfect +
Correct(CCW)". A strictly-convex simple polygon has all-distinct vertices, so
`test_contour_for_loops` is subsumed - argued below and enforced by the
differential test.

### Contour representation

Contours here are NOT explicitly closed (no repeated first == last vertex);
`is_convex_ring` and `emit_cell`'s rescale output both use modular indexing
`(i+1) % n`, `(i+2) % n`. The screen matches that convention. An
accidentally explicitly-closed ring (first == last) produces a zero cross at
the seam, so the screen rejects it and the call falls through - safe.

### Counter/flush infrastructure

`src/debug.rs` provides the `counter_group!` macro (accumulate into
process-global `AtomicU64` fields, flush once at end of run). `WAIT` and
`BUSY` use it; `emit_wait_counters()` flushes both and is called once from
`src/pipeline/mod.rs` (around line 734, after `total_ms`). Per-call FIFO
writes (`emit_counter`) are NOT viable at 11.4M calls - accumulate in atomics
and flush once, exactly as the wait/busy counters do.

### Callers of the single-contour arm

`normalize_into`'s single-contour arm is at `int_ocean.rs:547-548` (the
`shape.len() == 1` block; the "~535" the survey used to cite is the `fn`
header, and the arm itself is a dozen lines below). It calls the 2-arg
`BoolOverlay::simplify_contour_into` wrapper (`overlay/mod.rs:63`), which
`sync_options()` (forcing `output_direction = CounterClockwise`,
`fill_rule = NonZero`) and forwards to the 3-arg port
`Overlay::simplify_contour_into(contour, FillRule::NonZero, out)`
(`port/simplify.rs:21`). The screen lands in the port function, so it covers
every caller without touching call sites.

PRODUCTION callers of the single-contour arm (verified against source, review
R2):
- `emit_cell` (`pyramid.rs` ~line 413), reached only AFTER the `dp_tol > 0 &&
  is_convex_single_ring` guard has peeled the convex-single-ring case. As
  established above, that guard peels a superset of E1's acceptance, so on the
  `dp_tol > 0` path E1 gets no hits here; the reachable hits from this caller
  are the `dp_tol == 0` emissions (guard not taken) and non-convex-but-simple
  rings.
- the pyramid root (`pyramid.rs` ~line 178, `dp_tol` 0 at the root).
- the ocean / `int_ocean` production paths that build single-contour shapes.

NOT production callers (do not count these toward the opportunity):
- `int_ocean.rs` `normalize()` (~line 527) is `#[cfg(test)]`.
- the pyramid quad path (`pyramid.rs` ~line 1259) is inside the `#[cfg(test)]`
  tests module.

`emit_cell`'s own `is_convex_single_ring` short-circuit is left in place: on
the `dp_tol > 0` path it additionally skips `encode`-side setup and the
`normalize_into` call frame itself, so it still pays off above this screen;
the two do not conflict (a ring `emit_cell` already peeled never reaches
`simplify_contour_into`).

## Target artifact

### The screen

A free function in `src/geometry/overlay/port/simplify.rs`:

```rust
/// True iff `contour` is a strictly convex, CCW-wound, non-self-intersecting
/// ring with no collinear or duplicate vertices - exactly the inputs for
/// which `Overlay::simplify_contour_into` returns `false` (the engine's
/// "perfect input, correct winding" verdict, `out` untouched). Conservative:
/// any doubt returns false and the caller runs the full solver.
///
/// Reproduces the engine verdict via three necessary+sufficient conditions
/// on the implicitly-closed ring (modular indexing, no repeated first==last):
///  1. n >= 3.
///  2. every cyclic triple cross product is strictly > 0. Strictly positive
///     gives, in one pass: no collinear vertex and no consecutive duplicate
///     (both would be cross == 0, which `append_path_iter`/DropCollinear
///     drops -> not perfect), convexity (all left turns), and CCW winding
///     (positive signed area -> the Correct branch, not Reverse).
///  3. exactly two x-edge sign flips and two y-edge sign flips (winds exactly
///     once). Condition 2 alone is unsound: a self-lapping spiral turns left
///     at every vertex yet is non-simple. A strictly convex ring that winds
///     once is simple, and its vertices are all distinct, so
///     `test_contour_for_loops` (repeated-vertex test) is subsumed.
fn is_perfect_ccw_convex(contour: &[IntPoint]) -> bool
```

Implementation: one O(n) pass computing per-triple cross (reject on any
`cross <= 0`), plus the two flip counts, inlined here on `&[IntPoint]` to keep
the port module self-contained - do not add a pyramid dependency to the
engine. Uses `i128` cross products (matching `is_convex_ring`) to avoid
overflow at max-zoom pixel magnitudes.

ALLOCATION-FREE flip counting is mandatory (review R1). Do NOT port
`direction_flips` verbatim: that function does `Vec::with_capacity(n)` per
call, which at 11.4M single-contour calls would allocate on every pass and
directly contradict this spec's own "alloc profile unchanged in shape, cheap
O(n) add" claim - it would surface as an alloc regression. Count flips with
running state instead: for each axis track `first_sign`, `prev_sign`, and a
`flips` counter, skipping zero-delta edges, and close the wrap-around by
comparing the last nonzero sign against `first_sign` at the end. Fuse both
axis counts and the cross-product convexity check into the single O(n) pass so
the screen allocates nothing.

DIRECTION GUARD is mandatory (review R1/R2). The screen hardcodes "perfect CCW
contour -> Correct verdict -> return `false`," which is only valid when
`self.options.output_direction == CounterClockwise` and `fill_rule ==
FillRule::NonZero`. The port `Overlay::simplify_contour_into`
(`port/simplify.rs:35`) computes the fill direction from
`self.options.output_direction`; under `output_direction = Clockwise` a
strictly-convex CCW ring must instead be REVERSED and return `true`. Production
never hits that arm - `overlay_options` (`overlay/mod.rs:118`) pins
`output_direction = CounterClockwise` and `FillRule` has only `NonZero` - but
the fast path sits in the generic port function, below that pin, so the fast
path MUST gate on `self.options.output_direction == ContourDirection::
CounterClockwise` (and match/assert the `NonZero` fill rule). When either
condition does not hold, skip the fast path and fall through to the existing
body. Add a `debug_assert!` documenting the assumption at the fast-path site.

### The counters

New `counter_group!` in `src/debug.rs`:

```rust
counter_group!(ScreenCounters {
    normalize_screen_pass           => "normalize_screen_pass",
    normalize_screen_reject         => "normalize_screen_reject",
    // slow-path calls the engine itself found perfect (returned false):
    // the ceiling a looser screen could ever reach.
    normalize_screen_perfect_return => "normalize_screen_perfect_return",
    // size buckets (n = outer vertex count), pass/reject split, for
    // solver-cost-weighting the pass fraction (see threshold derivation).
    // ... _n3 / _n4_8 / _n9_32 / _n33p per pass|reject ...
});
pub static SCREEN: ScreenCounters = ScreenCounters::new();
```

`pass + reject` == every `simplify_contour_into` call == every
single-contour `normalize_into` call, so the pair is self-normalizing (pass
fraction = pass / (pass + reject)). Flushed alongside the wait/busy counters:
extend `emit_wait_counters()` to also call `SCREEN.emit()` (or add
`emit_screen_counters()` called at the same site in `src/pipeline/mod.rs`).

Contention (review R1). `WAIT`/`BUSY` reuse `counter_group!` but fire
per-blocking-span (rare); bumping a process-global `AtomicU64` on every one of
11.4M calls across all rayon workers is a different regime - cache-line
ping-pong on the shared line could both muddy the Landing-1 "is the screen
cheap" reading and persist as a small tax into Brick 3, where the counters
stay. Accumulate thread-locally (per-worker cell, folded into the atomic once
at end of run) rather than a shared atomic per call. If a shared atomic is
kept instead, that is a decision to record with a measured "contention cost
acceptable" note, not an assumption.

Richer buckets for a correct pricing (review R2). A bare pass/reject pair
weights a passed triangle and a rejected large contour equally, and a
single-contour `normalize_into` call is not the same as
`simplify_contour_into` returning the perfect verdict. To price the landing
honestly the instrument should additionally record, at least behind the same
end-of-run flush:
- `normalize_screen_perfect_return`: how many slow-path (screen-rejected)
  calls the engine ITSELF found perfect (`simplify_contour_into` returned
  `false`) - the ceiling of what a looser screen could ever capture, and the
  measure of how much the strict screen leaves on the table.
- a small set of contour-size buckets (e.g. n in 3, 4-8, 9-32, >32) split by
  pass/reject, so the pass fraction can be re-weighted by solver cost rather
  than call count.
These are cheap counters on the same flush path; they are what turn Brick 2
from a single ratio into an actual cost estimate. See the revised threshold
below.

### The wiring

At the top of `Overlay::simplify_contour_into`
(`src/geometry/overlay/port/simplify.rs`), before `self.clear()`:

- Landing 1 (instrument): compute `hit = is_perfect_ccw_convex(contour)`,
  increment `SCREEN.normalize_screen_pass` or `..reject`, then fall through
  to the existing body unchanged. Output identical; only the counters and a
  cheap O(n) screen are added.
- Landing 2 (fast path): when `hit`, `return false` immediately (skip
  `self.clear()`, `find_intersections`, the split solver, and the loop test).
  `out` is left untouched, matching the `Correct` branch. Also gate on the
  direction guard above (only take the fast path under CCW output +
  NonZero).

  Why skipping `self.clear()` is sound (corrected, review R1). The earlier
  justification - "`add_shape`/`add_contour`/`simplify_contour_into` clear
  first" - is false: `add_contour`/`add_contours`/`add_shape`
  (`overlay/mod.rs`, `port/mod.rs`) APPEND via `append_path_iter` and do NOT
  clear; `clear()` is a separate call the caller makes (the differential
  oracle does exactly `engine.clear(); engine.add_shape(...)`, and the
  multi-contour arm at `int_ocean.rs:565` calls `scratch.overlay.clear()`
  before `add_shape`). The correct argument: `simplify_contour_into` ALREADY
  leaves `self.segments` dirty on exit today - the existing perfect/`Correct`
  branch returns `false` with this contour's segments still populated (it
  never re-clears on the way out). So the codebase already relies on every
  reader of the segment buffers clearing before it reads. `Overlay::
  simplify_contour_into` unconditionally clears at entry (`port/simplify.rs`,
  first line); the multi-contour `add_shape` path clears at its call site. The
  fast path merely leaves DIFFERENT-but-equally-stale segments (those of some
  prior call rather than this contour's), and the next reader clears them
  regardless. No reader consumes segments without a preceding clear, so
  skipping the entry clear here changes nothing observable.

## Bricks

Ordered so `brokkr check` and `elivagar verify` stay green at every boundary.

### Brick 0 - capture the untouched baseline (reading a)

Before any E1 code lands, from clean `main`, capture the norway
`--variant locations` bench (best-of-3) and hotpath and record the commit hash
+ host under "reading a" in the Measurement baseline section. This is the
baseline Brick 3's keep/revert verdict is read against; capturing it after
Brick 2 would fold the instrument's cost into the baseline and hide a
regression. No code change, measurement only.

Commands:
```
brokkr tilegen --bench --dataset norway --variant locations
brokkr tilegen --hotpath --dataset norway --variant locations
```

### Brick 1 - the screen function + soundness test (no wiring)

Add `is_perfect_ccw_convex` to `src/geometry/overlay/port/simplify.rs`. Add a
unit test module in the same file that:
- asserts the screen FIRES on: CCW convex triangles/quads/octagons at varied
  scales; CLEARS on CW convex rings, concave rings, rings with a collinear
  vertex, rings with a consecutive duplicate, a self-intersecting bowtie, a
  self-lapping (double-wound) star, and n < 3.
- a reference slow body is required (review R2). Brick 1 must run the engine
  body with the screen NOT applied to get an independent `(verdict, out)`. No
  such helper exists today, and once Brick 3 lands, calling the public
  `simplify_contour_into` would exercise the fast path and no longer be
  independent. So Brick 1 introduces a `#[cfg(test)]` (or otherwise
  screen-bypassing) entry that runs the existing find_intersections + solver
  body unconditionally - e.g. a `simplify_contour_into_slow` used only by the
  test, or factor the current body into a private `fn` the fast path and the
  test both call. Pin this in Brick 1 so Brick 3 cannot silently turn the
  reference into the thing under test.
- differential corpus: generate many random small integer rings (reuse the
  `Lcg` generator pattern from `src/geometry/overlay/mod.rs` tests). For each,
  run the reference slow body to get `(verdict, out)`. Then assert: **whenever
  `is_perfect_ccw_convex(ring)` is true, the reference verdict is `false` AND
  reference `out` is empty (untouched).** Assert BOTH halves explicitly - the
  existing 2000-case oracle
  (`differential_oracle_reused_engine_recycles`) only checks `port_rebuilt ==
  false` for the perfect case and does NOT assert `out` stayed untouched
  (`overlay/mod.rs` ~line 254), so this test is the one that pins the
  untouched-output half. This is the soundness invariant - the screen may only
  fire where the engine returns `false` untouched. It never asserts the
  converse (the screen is allowed to miss perfect rings; it is conservative),
  so a too-strict screen is legal, a too-loose one fails here.
- deterministic fast-path cases (review R2): after Brick 3, add hand-built
  cases that exercise the short-circuit directly - verdict `false` + `out`
  untouched on an accepted ring; correct behavior on a warm/reused engine
  (call once so segments are dirty, then a screened call, and assert the
  result is still correct - this is the clear-skip soundness in a test); and
  the direction guard, by constructing an `Overlay` with `output_direction =
  Clockwise` and asserting a strictly-convex CCW ring is NOT fast-pathed
  (returns `true` with reversed output, matching the slow body).

Gate: `brokkr check`. This brick adds no behavior change (function unused in
production yet), so `check` passing is the whole bar.

Command: `brokkr check`

### Brick 2 - counters + instrument wiring (Landing 1, bit-identical)

Add the `ScreenCounters` group and `SCREEN` static to `src/debug.rs`, wire
`SCREEN.emit()` into the end-of-run flush (`emit_wait_counters` or a sibling
`emit_screen_counters`) called from `src/pipeline/mod.rs`. In
`Overlay::simplify_contour_into`, compute the screen, bump the counter, and
FALL THROUGH (no short-circuit). Output is byte-identical; only counters and
the O(n) screen cost are added.

Gates:
- `brokkr check` (differential oracle unaffected: verdict/output unchanged).
- Output neutrality: `brokkr regress --dataset denmark` (defaults denmark,
  resolves the locations-variant blessed archive itself). Expect tol 0, zero
  structural diffs, zero tolerance diffs - behavior is unchanged.
- Instrument read: measured norway builds (coastal normalize volume lives
  there) - BOTH a bench (the instrumented wall/CPU reading, reading b of the
  three below) and a hotpath (for the counters and function timing).
- Instrument neutrality: the Brick 2 norway bench is compared to the UNTOUCHED
  pre-change baseline (reading a). Brick 2 is supposed to be output-identical
  and near cost-neutral; a large regression here means the "cheap O(n) add"
  premise is already wrong (likely the alloc or atomic contention issue) and
  must be fixed before Brick 3, since Brick 3's verdict is read against
  reading a, not reading b (see Measurement baseline).

Commands:
```
brokkr check
brokkr tilegen --bench --dataset denmark --variant locations
brokkr regress --dataset denmark
brokkr tilegen --bench --dataset norway --variant locations
brokkr tilegen --hotpath --dataset norway --variant locations
brokkr sidecar <uuid> --counters
```
(Read `normalize_screen_pass`, `normalize_screen_reject`,
`normalize_screen_perfect_return`, and the size-bucket counters from the
`--counters` output of the norway HOTPATH run; read the norway BENCH
`elapsed_ms` for reading b.)

**Proceed/close threshold.** Let `f = pass / (pass + reject)` on norway. The
fast path removes the segment-build + split-solver + loop-test cost on the
`pass` fraction only, at the price of the O(n) screen paid on ALL calls.

Why 0.30 (derivation, review R1). The screen is a single O(n) scan (~n cross
products + 2n sign compares, no allocation). The body it replaces is the
segment build (`append_path_iter`, O(n) with per-vertex filter work) plus the
split solver (`split_segments`, superlinear) plus the loop test. Call the
per-call body cost `B` and the screen cost `S`; empirically `S` is a small
fraction of `B` (the screen is the cheap prefix of what the body already
does). Net saving per call = `f*(B - S) - (1-f)*S = f*B - S`. Break-even is
`f = S/B`; with `S/B` on the order of 0.1-0.15, any `f` above ~0.15 is already
net-positive in principle. 0.30 is set at roughly 2x break-even as a margin
against (a) the screen being more expensive than estimated, (b) the atomic /
counter tax, and (c) `B` being smaller on the tiny rings that dominate. It is
a guardrail, not the break-even point; if the size-bucketed instrument (above)
shows the passed rings carry a disproportionately small share of solver cost,
raise the effective bar accordingly - re-weight `f` by bucket rather than
trusting the flat ratio.

Below the bar the item is mispriced - close it and REMOVE Brick 2's added
cost, not just Brick 3 (review R2). Keep Brick 1's screen function and unit
test (cheap, documents the invariant), but the permanent scan + per-call
counter is only justified if it earns the fast path; on a close, delete the
per-call instrument (or demote it behind an off-by-default env gate like
`ELIVAGAR_LAYER_STATS`) so a mispriced item does not leave a standing tax that
pays the scan, the counter, AND the full solver on every call. Record the
measured `f`, the bucketed distribution, and the close decision in this spec
and `reference/performance.md`. The estimate (tile rings are "typically tiny
and usually simple") motivates the work; only this reading justifies the
landing.

### Brick 3 - the fast path (Landing 2, bit-identical, the win)

Conditional on Brick 2 clearing the threshold. Change the wiring in
`Overlay::simplify_contour_into`: when the screen fires, `return false`
immediately (skip `self.clear()`, `find_intersections`, the split solver, and
the loop test), leaving `out` untouched. Keep the counter increments.

Gates (all must pass; this is a bit-identical landing under the same regime
as Landing 2 of the port campaign, no re-blessing):
- `brokkr check` - the 2000-case differential oracle
  (`differential_oracle_reused_engine_recycles`) now exercises the
  short-circuit for any generated ring the screen accepts, asserting the
  `port_rebuilt == false` verdict against dev-dep i_overlay. Note (review R2):
  that oracle checks only `port_rebuilt == false`, NOT that `out` stayed
  untouched - the untouched-output half is pinned by Brick 1's soundness test,
  which is why Brick 1 asserts both halves. Both run under `brokkr check`.
- `brokkr regress --dataset denmark` - tol 0, zero structural diffs, zero
  tolerance diffs. The standing gate.
- Earcut tessellation oracle on a fresh build, every polygon layer, 0
  deviant polygons, 0 misattached holes - the standing geometry gate, and the
  layer where convex tile rings actually live. Invoked as
  `node earcut-oracle.mjs <archive.pmtiles>` from `scripts/validate/` (review
  R2: there is NO `pnpm start` script - `package.json` has no `start` entry;
  `pnpm install` there first if node_modules is cold, then run node directly).
- Archive verification - zero errors. `elivagar` is not on `PATH`; run the
  verify through brokkr against the concrete archive (`brokkr verify pmtiles
  --file <path>`), not a bare `elivagar verify`.
- The win: norway `--bench` (reading c), expect `normalize_into`'s hotpath
  rank-4 thread-time to drop (the screened calls no longer pay segment build +
  split). The verdict is read against reading a (untouched), NOT reading b
  (instrumented Brick 2): c-vs-a bench improvement must exceed ~5% (below that
  is within noise per `reference/performance.md`). Flat or regressed vs
  reading a = the pricing was wrong, revert.

Commands:
```
brokkr check
brokkr tilegen --bench --dataset denmark --variant locations
brokkr regress --dataset denmark
```
Then run the geometry gates against a concrete denmark archive. `brokkr
regress`/`--bench` resolve their own archives but do not leave one at a
stable, named path, and `<denmark-output>` is not a runnable token, so pin the
archive explicitly: produce a durable one with a plain `brokkr tilegen`
(user-authorized, since Brick 3 already runs the pipeline) writing a known
`-o` path under `data/`, or resolve the bench's output via `brokkr
pmtiles-inspect --dataset denmark --commit <brick3-hash>` to learn the path.
Call that resolved path `$ARCHIVE` and run:
```
brokkr verify pmtiles --file $ARCHIVE
```
```
node scripts/validate/earcut-oracle.mjs $ARCHIVE
```
(run the earcut oracle from `scripts/validate/` - `cd scripts/validate; node
earcut-oracle.mjs $ARCHIVE` - after `pnpm install` there; do NOT use `pnpm
start`, there is no such script. Replace `$ARCHIVE` with the real absolute
path before running - the copy-pasteable-command contract forbids leaving a
placeholder.)

The win measurement:
```
brokkr tilegen --bench --dataset norway --variant locations
brokkr results <uuid>
brokkr sidecar <uuid> --counters
brokkr tilegen --hotpath --dataset norway --variant locations
```

## Measurement baseline

THREE commit-anchored readings on the SAME host are required (corrected,
review R2). Comparing Brick 3 only against Brick 2 can show a Brick-3
improvement while the shipped code is still slower than the untouched original
(Brick 2 itself added a scan + atomic to every call). So:

- reading a - UNTOUCHED pre-change: norway `--variant locations` bench
  (best-of-3) and hotpath at the commit BEFORE Brick 1, i.e. current `main`.
  Capture this first, before any E1 code lands, and record the commit hash +
  host. This is the baseline the final keep/revert verdict is read against.
- reading b - INSTRUMENTED Brick 2: same norway bench + hotpath at the Brick 2
  commit. Its job is to price the instrument itself against reading a.
- reading c - FINAL Brick 3: same norway bench + hotpath at the Brick 3
  commit.

Keep bound. Ship Brick 3 only if reading c's norway bench is faster than
reading a's by more than measurement noise. Per `reference/performance.md`,
wall deltas under ~5% are suspect (within run-to-run variance), so treat a
c-vs-a improvement below ~5% as NOT demonstrated - flat or regressed against
reading a (not against b) means the pricing was wrong: revert Brick 3, and
reconsider whether Brick 2's instrument should stay (see the close path in
Brick 2). The `normalize_into` hotpath thread-time drop (c vs a) is the
corroborating signal; the bench wall (c vs a) is the gate.

Per `reference/technical-implementation-spec.md` brick 6 and
`reference/performance.md`: commit first, then measure, then write the
hash-anchored numbers into `reference/performance.md`. Denmark regress being
bit-identical means the alloc profile is unchanged in shape (assuming the
allocation-free flip count above - a Vec-per-call screen would break this);
the win is CPU (fewer solver invocations), not churn, so the alloc gate is not
the verdict here - the norway bench wall and `normalize_into` hotpath
thread-time are.

Fill on landing:
- reading a (untouched, commit `________`, host `________`): norway locations
  bench `____` ms best-of-3; `normalize_into` hotpath `____` thread-s.
- reading b (Brick 2 commit `________`): norway locations bench `____` ms;
  `normalize_into` hotpath `____` thread-s; `normalize_screen_pass` `____`,
  `normalize_screen_reject` `____`, `normalize_screen_perfect_return` `____`,
  `f = ____`, size buckets `____`.
- reading c (Brick 3 commit `________`): norway locations bench `____` ms;
  `normalize_into` hotpath `____` thread-s. Verdict (c vs a): `____`.

## Stopping rule

In scope: the single-contour arm of `normalize_into`, i.e. the screen inside
`Overlay::simplify_contour_into` and its counters. Out of scope, explicitly:
- The multi-contour arm of `normalize_into` (`shape.len() > 1`, the boolean
  overlay). Untouched.
- `intersect_rect_into` and every other overlay op. Untouched (E4b is the
  rect-specialized boolean; separate item).
- The E2 flat point+range output boundary, E3 clip pooling, E5 caller
  buffers. Named siblings in the roadmap; not this spec.
- The `emit_cell` `is_convex_single_ring` screen. Left exactly as-is; it sits
  above this one and (on the `dp_tol > 0` path) peels a SUPERSET of what E1
  accepts before the call - so E1's reachable hits are the `dp_tol == 0` and
  non-`emit_cell` paths, per the corrected framing above.
- Any change that alters noding or output values. This landing is
  bit-identical by construction; if any gate shows a diff, the screen is
  unsound (too loose) and the landing reverts - it is never re-blessed. Only
  E4c-class changes rotate the baseline, and this is not one.

The screen is conservative by design: it may only ADD false rejections
(rings the engine would also have found perfect but the screen was too strict
to prove), never false acceptances. A false rejection costs nothing but a
missed optimization; a false acceptance changes output and is caught by the
differential oracle in `brokkr check` before production. That asymmetry is
the safety argument for the whole item.

## Review reconciliation (R1 opus + R2 codex)

Both review reports were validated against source and consolidated here.

Folded in:
- Insertion-point unsoundness under configurable `output_direction` (R1
  "config coupling", R2 P1 #2): the fast path now gates on
  `output_direction == CounterClockwise` + `NonZero` with a `debug_assert`.
  Production is always CCW via `overlay_options`, so this is a guard for the
  generic port function, not a live production bug - but it is required.
- Allocation regression from a verbatim `direction_flips` port (R1 gap): the
  screen MUST count flips with running state, allocation-free; `Vec::
  with_capacity(n)` per call at 11.4M calls is forbidden.
- The `self.clear()`-skip justification was factually wrong (R1 bug): rewritten
  to the correct argument (the perfect branch already leaves segments dirty on
  exit; every reader clears before reading).
- Mislocated workload (R2 P1 #1): the `emit_cell` `dp_tol > 0` guard peels a
  SUPERSET of E1's acceptance (not a "strict subset"), so E1 is unreachable on
  that path; the reachable opportunity is root normalization, `dp_tol == 0`
  emission, and ocean/int_ocean callers. Caller survey corrected: `int_ocean`
  `normalize()` and the pyramid quad path are `#[cfg(test)]`, not production.
- Baseline uses the instrumented build (R2 P1 #3): three commit-anchored
  readings now required (untouched a / instrumented b / final c), verdict read
  c-vs-a with a ~5% keep bound; Brick 0 captures reading a first, Brick 2 now
  runs the norway bench.
- Threshold not priced (R1 nit, R2 P2 #4): 0.30 is now derived (~2x
  break-even `S/B`), plus `normalize_screen_perfect_return` and size-bucket
  counters to solver-cost-weight the ratio; on a close the per-call instrument
  is removed/gated, not left as a standing tax.
- Non-executable gate commands (R2 P2 #5): `pnpm start` replaced with `node
  earcut-oracle.mjs` (no `start` script exists), verify routed through `brokkr
  verify pmtiles --file`, placeholders flagged for concrete substitution.
- Soundness test underspecified (R2 P2 #6): a screen-bypassing slow-body
  reference helper is mandated; Brick 1 asserts BOTH the `false` verdict and
  untouched `out` (the existing 2000-case oracle asserts only the former);
  deterministic warm-reuse and Clockwise-direction cases added.
- Atomic contention at 11.4M calls (R1 smell, R2 P2 #4): thread-local
  accumulation recommended, or a measured "acceptable" note.
- Broken reference (R2 P2 #7): `notes/rendering-fix-log.md` does not exist;
  corrected to `notes/rendering-postmortem.md` + the `is_convex_ring` doc
  comment. Line-reference drift in the survey (the arm is at `int_ocean.rs`
  ~547, not ~535) corrected.

Rejected / down-scoped:
- R1 nit "the survey snippet conflates the wrapper and the port function": the
  snippet correctly shows the 2-arg `BoolOverlay` wrapper at the call site and
  the 3-arg port function at the wiring site - two genuinely distinct layers,
  each shown at the right place. Not a conflation; kept, only annotated to
  make the wrapper -> port forwarding explicit. (The accompanying line-number
  drift the same finding noted WAS valid and is fixed.)
