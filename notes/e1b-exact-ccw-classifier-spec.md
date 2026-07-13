# E1b: exact perfect-CCW normalize classifier, priced by avoided solver cost

Written against `reference/technical-implementation-spec.md` (the contract for
what a spec must pin). Source item: the E1b bullet in
`notes/planet-30gb-roadmap.md` ("THE POST-PORT SURFACE", H6 engine surface),
the E1 close bullet directly above it, and the dated E1 evidence in
`reference/performance.md` ("E1 normalize fast-path close", commit `65ae629`).
This spec is the direct successor to E1 (CLOSED 2026-07-13, below threshold): it
does NOT re-derive the predicate or the soundness gate - it builds on the
`is_perfect_ccw_convex` predicate, the screen-bypassing slow-body reference
helper (`simplify_contour_into_slow`), and the soundness tests that E1 landed and
deliberately RETAINED under `cfg(test)` in
`src/geometry/overlay/port/simplify.rs` (commits `65ae629` then `edf1dac`).

## 0. One-paragraph statement

The single-contour arm of `normalize_into` calls `simplify_contour_into`, which
pays the full segment-build + split-solver body on every call only to discover,
0.974 of the time, that the input was already a perfect ring the engine returns
untouched (`return false`, `out` empty; the caller keeps its own allocation).
E1's strict-convex screen captured only 0.272 of calls - it proved the wrong
predicate (convexity), not the engine's exact perfect-input verdict, and the
expensive large rings sit in the rejected-but-perfect remainder. E1b replaces the
convexity predicate with an EXACT, conservatively-SOUND classifier that reproduces
the engine's "return false / untouched" verdict for non-convex simple rings too,
and - because a naive segment-pair simple-polygon test duplicates the split
solver's own work at large n - bounds it with a measured large-ring cutoff.
E1b is a CPU / solver-skip optimization: it is priced by AVOIDED SOLVER TIME
(hotpath), never by allocation volume, and it is instrument-first with an honest
close path. If the exact classifier is not cheaper than the solver body it
replaces, E1b closes exactly as E1 did.

## 1. Goal

Land a sound exact classifier `is_engine_perfect_ccw(contour, cutoff) -> bool` on
the single-contour arm of `simplify_contour_into` so that, when it returns true,
the call skips `find_intersections` + `contour_direction` entirely and returns
`false` with `out` untouched - the identical result the slow body would have
produced - saving the segment build, sort, split solver, and loop test on that
call. Success is measured, not assumed: the win is the net avoided solver
thread-time on the norway locations hotpath, and E1b proceeds to the flip only if
that net clears an explicit threshold (section 4, Landing 1).

Non-goals (named and excluded, not deferred):
- The reversed-winding perfect case (perfect ring, but CW where the engine wants
  CCW output; slow body reverses it into `out` and returns `true`). That path
  does real output work, is the smaller slice, and stays with the slow body.
  E1b's predicate requires correct CCW winding and returns only the `false`
  verdict.
- The multi-contour arm of `normalize_into` (`shape.len() != 1`) and
  `intersect_rect_into`. Out of scope; E1b touches only the single-contour
  `simplify_contour_into` path.
- E2 (flat point+range output), E3 (rect-clip pooling, already reverted), and
  E4* (noding changes). Separate roadmap items.

## 2. Survey of the ground

### 2.1 The exact verdict E1b must reproduce

`Overlay::simplify_contour_into_slow` (`src/geometry/overlay/port/simplify.rs`)
returns `false` with `out` untouched in EXACTLY one case: `find_intersections`
returned `true` (input already perfect) AND `contour_direction` returned
`Correct` (winding already matches the CCW output direction). The classifier is
sound iff it accepts a subset of the contours that produce this verdict. Decompose
`find_intersections` (same file) against the production options
(`preserve_input_collinear = false`, output `CounterClockwise`, `NonZero` -
`overlay_options` in `src/geometry/overlay/mod.rs`):

1. **append_modified == false.** `append_path_iter` builds segments through the
   `DropCollinear` filter (`preserve_input_collinear` is false), whose
   `include_point(p0,p1,p2)` is `a.cross_product(b) != 0`
   (`src/geometry/overlay/port/segment.rs`). It also drops consecutive duplicate
   points up front (`iter.find(|p| p0.ne(p))`). So `append_modified` is false iff
   NO cyclic triple is collinear and NO consecutive (cyclic) pair is equal -
   equivalently, every cyclic triple `(v[i], v[i+1], v[i+2])` has nonzero cross
   product. This is the SAME `cross != 0` test the retained `is_perfect_ccw_convex`
   already applies per triple; E1b keeps that test but drops the sign constraint
   (convex requires `cross > 0`, E1b allows `cross > 0` OR `cross < 0`, admitting
   reflex vertices).
2. **split_modified == false.** `split_segments`
   (`src/geometry/overlay/port/split.rs`) returns true on any merged overlap
   (`merge_if_needed`) or any pairwise intersection (`self.split`, via
   `CrossSolver::cross` under `snap_radius`). False means the ring is a SIMPLE
   polygon at the engine's rounding: no proper crossings, no collinear overlaps,
   no snap-radius T-junctions.
3. **segments not empty** (covered by n >= 3 with all triples non-degenerate).
4. **!has_loops.** `test_contour_for_loops` (`src/geometry/overlay/port/fill.rs`):
   true if ANY vertex value repeats anywhere in the ring (n<64 does the O(n^2)
   `contains` scan; n>=64 sorts). False means all vertices are distinct.
5. **contour_direction == Correct.** `contour.is_clockwise_ordered()` is
   `unsafe_area() <= 0` (`src/geometry/overlay/port/extract.rs`); for `NonZero` +
   CCW output, Correct means the ring is CCW, i.e. `unsafe_area() > 0`.

The strict-convex predicate `is_perfect_ccw_convex` already guarantees all five
(a strictly-convex CCW ring is simple, distinct-vertexed, and correctly wound).
E1b broadens acceptance to non-convex simple CCW rings by proving 2 and 4
directly instead of leaning on convexity.

### 2.2 The soundness trap in invariant 2, and the conservative resolution

Reproducing `split_modified == false` EXACTLY means matching the split solver's
snap-radius rounding, collinear-overlap merge, and T-junction detection. A naive
integer segment-intersection test that disagrees near `snap_radius` would be
UNSOUND (accept a contour the engine actually rewrites) - the one failure mode
E1b must never have. Resolution: E1b's classifier is CONSERVATIVELY sound. It
accepts only rings it can PROVE simple, and on any snap-radius ambiguity it
REJECTS (falls through to the slow body). A conservative reject is always sound:
the slow body then produces the correct result. Concretely the pairwise test is
built from the engine's own `CrossSolver::cross` primitive (same rounding as the
split solver) so a "clean" verdict means what the split solver means, and it
additionally rejects any non-adjacent edge pair whose bounding boxes come within
`snap_radius` even if `cross` reports no intersection. Soundness is not argued
into existence - it is GATED (section 5): the retained random-contour soundness
test is extended to the new predicate, and the 2,000-case differential oracle
plus artifact-level `cmp -s` (section 6) make an unsound accept observable.

**The pinned simple-polygon test (invariant 2), concretely.** This is the one
novel, soundness-critical algorithm and it is NOT left to implementation choice:

- *Primitive and radius.* Reuse `CrossSolver::cross(target: &XSegment, other:
  &XSegment, radius: i64) -> Option<CrossResult>` (`src/geometry/overlay/port/cross.rs`)
  verbatim - the same primitive `split_segments` drives - so a "clean" verdict
  means exactly what the split solver means. `cross` takes an explicit `radius`;
  that radius is `snap_radius`, which is solver/precision-dependent (derived from
  `self.solver` / the overlay precision, same source `split_segments` uses). It is
  NOT available from `(contour, cutoff)` alone. Therefore `is_engine_perfect_ccw`
  must be an `Overlay` METHOD (or take the resolved `radius`/solver by argument),
  not a free function; the section 3.1 signature is corrected accordingly. Build
  each edge as the engine builds it (the ordered `XSegment` form, low endpoint
  first, matching `append_path_iter`'s segment construction) so `cross`'s
  target/other ordering assumptions hold.
- *Adjacency exclusion.* Every adjacent edge pair shares a vertex, so `cross`
  returns `Some(TargetEnd)`/`Some(OtherEnd)` for them; those are legal and must be
  SKIPPED. Adjacency is cyclic: for a ring of n edges, edge `i` is adjacent to
  edges `i-1` and `i+1` mod n - and the closing pair (edge `n-1`, edge `0`) is
  adjacent too. Skip exactly those pairs and no more (a chord between vertices `i`
  and `i+2` is NON-adjacent and any touch there is a real reject). Getting this
  wrong in either direction is a bug: skip too much and a real self-touch slips
  through (unsound); skip too little and every polygon false-rejects (merely
  incomplete, still sound).
- *Reject classification.* For every NON-adjacent edge pair, reject the contour if
  `cross` returns `Some(_)` at all - `CrossType::Pure` (a proper interior
  crossing), `CrossType::Overlay` (a collinear overlap), and `CrossType::TargetEnd`
  /`OtherEnd` on a non-adjacent pair (an endpoint lying on another edge, a
  T-junction the split solver would snap). Any `Some` on a non-adjacent pair means
  the split solver would have set `split_modified`; reject.
- *Conservative proximity margin.* Even when `cross` returns `None`, reject any
  non-adjacent pair whose bounding boxes come within `snap_radius` (inclusive:
  box-to-box Chebyshev/Manhattan gap `<= snap_radius` on both axes rejects). This
  is the belt-and-braces margin that makes a near-touch the solver might snap fall
  to the slow body rather than be accepted. It is strictly conservative: it can
  only turn accepts into rejects, never the reverse, so it cannot introduce an
  unsound accept.

Only if every non-adjacent pair is clean under all four rules (and invariants 1,
4, 5 hold) does the classifier accept.

### 2.3 The large-n cutoff (why it is mandatory, not optional)

Invariants 2 and 4 are the O(n^2) part of the classifier (all non-adjacent edge
pairs; all vertex pairs). The split solver the classifier replaces is itself
roughly O(n log n) with heavy constants. So there is an n beyond which the
classifier's own pairwise scan costs more than the body it skips - "a segment-pair
test can duplicate the split solver's own work at large n" (roadmap E1b bullet).
E1b therefore carries a cutoff that bounds ONLY the O(n^2) extension (invariants
2 and 4). The precedence is pinned to remove any ambiguity: the O(n) work - the
cyclic-triple/winding pass and the strictly-convex fast-accept
(`is_perfect_ccw_convex`, itself O(n)) - runs on EVERY n, including large rings,
because it is cheap and a large strictly-convex ring is still a sound free accept.
The `n >= cutoff` guard sits AFTER the convex fast-accept and gates only the
O(n^2) distinct-vertex + pairwise-simplicity scan: a ring with `n >= cutoff` that
is not caught by the convex accept falls through to the slow body. So "no
classifier attempt beyond cutoff" is imprecise and is corrected here to "no O(n^2)
attempt beyond cutoff"; the O(n) screens always run. The cost accounting in 3.3
and the cutoff unit test in section 5 are aligned to this: the classifier-scan
counter must include the always-paid O(n) screen cost on large rings, not only the
`n < cutoff` O(n^2) scan, and the cutoff test uses a NON-convex simple ring (a
convex one would be accepted by the O(n) screen and never fall through). The
cutoff itself is NOT guessed - Landing 1 measures per-n avoided cost and per-n
classifier scan cost and picks the crossover (section 4).

### 2.4 The E1 evidence this spec is priced against

From `reference/performance.md` (E1 close, norway locations, `65ae629`),
19,400,462 single-contour calls:

| | value | reading |
|---|---|---|
| strict-convex pass | 5,282,144 | E1's captured slice, f=0.272 |
| perfect_return (engine false on rejects) | 13,610,939 | the missed remainder |
| pass + perfect_return | 18,893,083 | 0.974 ceiling E1b targets |

Size buckets (n = outer vertex count):

| bucket | pass | reject |
|---|---|---|
| n3 | 82,732 | - |
| n4_8 | 4,923,309 | 4,445,750 |
| n9_32 | 276,069 | 7,631,905 |
| n33p | 34 | 2,040,663 |

The load-bearing fact for E1b's design: the strict-convex passes cluster in cheap
n4_8, but the missed-but-perfect remainder is heavy in n9_32 (7.6M rejects,
mostly perfect_return) and n33p (2.0M rejects). The expensive perfect rings are
exactly the ones a broadened classifier would newly accept AND exactly the ones
the O(n^2) pairwise scan is most expensive on. That tension is the whole reason
E1b must be priced by avoided SOLVER cost per bucket and bounded by a measured
cutoff, not by the flat call ratio that mispriced E1.

### 2.5 Callers and the hotpath context

`simplify_contour_into` is called from `normalize_into`'s single-contour arm
(`src/geometry/int_ocean.rs`, `shape.len() == 1`) and from the differential-oracle
test in `src/geometry/overlay/mod.rs`. `normalize_into` carries `#[hotpath::measure]`
and is rank 4 in the norway hotpath (129.2 thread-s; roadmap H6 framing). On
denmark the call shape is 11.41M `normalize_into` calls (roadmap). The engine's
planet bill is millions of tiny per-feature normalizes, which is why shaving the
already-perfect fraction matters.

### 2.6 Failure history

E1 (this predicate as a strict-convex screen) is the direct logged failure:
mispriced against a flat 0.30 call-fraction threshold, captured 0.272, closed.
The lesson E1b must not repeat: price by solver work skipped, per size bucket,
with a threshold on avoided time - not by call count. No entry in
`notes/rendering-postmortem.md` (the durable R/S ledger; the former
`notes/rendering-fix-log.md` was deleted, full history in git) applies; E1b
changes no output geometry
and no MVT encoding, so the rendering-fix failure classes are out of frame. The
one geometry-adjacent risk (an unsound accept) is addressed by conservative
rejection (2.2) and gated by the oracle + `cmp -s`.

## 3. Target artifacts (concrete)

All in `src/geometry/overlay/port/simplify.rs` unless noted.

### 3.1 The exact predicate (promoted from `cfg(test)`, broadened)

Promote `is_perfect_ccw_convex` out of `cfg(test)` (it becomes a production
fast-path helper) and add the exact classifier beside it:

```rust
/// Immediate-accept fast path: strictly convex, CCW, distinct-vertexed,
/// non-self-intersecting. A sound subset of the engine's return-false verdict.
/// (Body unchanged from the E1-retained predicate; only the cfg(test) gate is
/// removed. Keep the FlipCounter revolution guard - left turns alone admit a
/// self-lapping {5/2} spiral, per the retained test.)
#[inline]
fn is_perfect_ccw_convex(contour: &[IntPoint]) -> bool { /* unchanged */ }

/// Conservatively-sound perfect-CCW screen. Returns true ONLY for contours the
/// engine's slow body returns `false`/untouched on with CCW-correct winding (see
/// section 2.1/2.2): every cyclic triple non-degenerate, all vertices distinct,
/// simple polygon at the split solver's rounding, CCW. Any doubt -> false (the
/// caller runs the full solver). Deliberately admits false NEGATIVES, so it is a
/// sound screen, not an "exact" reproduction - see the terminology note in 3.1.
/// `cutoff` bounds only the O(n^2) distinct-vertex + pairwise-simplicity scan;
/// the O(n) screens (triple/winding + convex fast-accept) always run.
///
/// It is a METHOD, not a free function: the pairwise simplicity test drives
/// `CrossSolver::cross(.., radius)` and needs `snap_radius`, which is
/// solver/precision-dependent and unavailable from `(contour, cutoff)` alone.
#[inline]
fn is_engine_perfect_ccw(&self, contour: &[IntPoint], cutoff: usize) -> bool {
    let n = contour.len();
    if n < 3 { return false; }
    // Invariant 1 + 5 (O(n), always run): every cyclic triple has nonzero cross
    // (no collinear/dup) AND CCW winding (unsafe_area() > 0). Reuse the triple
    // loop shape from is_perfect_ccw_convex but drop the sign constraint - accept
    // cross > 0 OR cross < 0 (admit reflex vertices).
    // Fast accept (O(n), always run): strictly convex rings are simple by
    // construction, sound at any n.
    if is_perfect_ccw_convex(contour) { return true; }
    if n >= cutoff { return false; }   // gate only the O(n^2) extension below
    // Invariant 4 (O(n^2)): all vertices distinct (mirror test_contour_for_loops).
    // Invariant 2 (O(n^2)): simple polygon under the pinned four-rule pairwise
    //   test of section 2.2 - CrossSolver::cross under snap_radius on every
    //   NON-adjacent (cyclic) edge pair, rejecting Pure/Overlay/endpoint touches
    //   and any pair whose boxes come within snap_radius.
    // Return true only if triples/winding/distinct/simple ALL hold.
    todo!("pinned in section 2.2: four-rule pairwise simplicity, radius from self")
}
```

Signature notes: `cutoff` is a `usize` const chosen by Landing 1's measurement
(section 4), stored as a module const (e.g. `const ENGINE_PERFECT_CUTOFF: usize`)
with a comment citing the bench UUID and the crossover reading. No env var, no
runtime knob (tech-spec: no benchmark scaffolding as the way forward).

Terminology: the spec title and prose call this predicate "exact" for continuity
with the roadmap E1b bullet, but the contract it actually implements is a
CONSERVATIVELY SOUND screen - it accepts a subset of the engine's return-false
verdict and deliberately admits false negatives (near-snap ambiguity, `n >=
cutoff` fallthrough). Read every "exact" in this document as "conservatively sound
perfect-CCW screen"; the soundness direction (accept ==> slow body returns false)
is the only exact property, and it is the one the gates pin.

### 3.2 The wiring in `simplify_contour_into`

```rust
#[inline]
pub fn simplify_contour_into(
    &mut self,
    contour: &[IntPoint],
    fill_rule: FillRule,
    out: &mut IntShapes,
) -> bool {
    if self.is_engine_perfect_ccw(contour, ENGINE_PERFECT_CUTOFF) {
        // Identical to the slow body's perfect + Correct-winding verdict:
        // out is left untouched, caller keeps its own allocation.
        return false;
    }
    self.simplify_contour_into_slow(contour, fill_rule, out)
}
```

`simplify_contour_into_slow` stays exactly as it is - it remains the independent
soundness reference the tests call directly, per the E1-close intent.

Engine-state note (do NOT skip the clear): the slow body OPENS with `self.clear()`
(`simplify.rs`), so on the reject path the engine's `segments`/split state is reset
before `find_intersections` appends. The early-return path above skips that
`clear()`. This is benign for output only because the NEXT `simplify_contour_into_slow`
call clears again before it reads any state, and the accept path itself reads none
of the stale state - but that is a downstream invariant, not the "identical method
behavior" the flip claims, and it weakens the warm-engine differential oracle
(which reuses one `Overlay` across cases). The classifier must therefore call
`self.clear()` on the accept path too (matching the slow body's opening), OR the
`clear()` is refactored into the shared `simplify_contour_into` entry ahead of the
branch. Either keeps warm-engine state bit-identical to the all-slow baseline; the
`cmp -s` gate cannot see this difference because it never surfaces in output, so it
is pinned here explicitly rather than left to the artifact gate.

### 3.3 The pricing instrument (Landing 1 only; removed or superseded at close/flip)

The instrument is the FIRST landing (tech-spec point 5: "the counter is the first
landing... states an explicit proceed/close threshold"). It runs the classifier
in SHADOW - computes the verdict and prices it, but does NOT change control flow,
so the archive is byte-identical while it measures. It lives behind the existing
sidecar counter channel (`src/debug.rs` emitters), NOT as new struct state on the
hot path beyond what timing needs.

Bucketing (finer than E1's four - REQUIRED so the cutoff is pinnable). E1's
coarse `n3 / n4_8 / n9_32 / n33p` buckets CANNOT locate a scalar `usize` cutoff:
a crossover inside `9..=32` or the open-ended `33..` is invisible to a bucket
mean. Landing 1 therefore counts at PER-n granularity (or narrow bins, e.g. width
1 up to some N then width 4) across the whole observed n range, with the top bin
OPEN and large enough that the O(n^2) cost is actually observed where it bites -
NOT capped at the provisional cutoff (see Landing 1). Keep the four coarse rollups
too for continuity with the E1 table, but the crossover is decided from the fine
counters.

Per single-contour call, in shadow, accumulated per n-bin:
- `e1b_accept_<bin>` - classifier returned true (would skip).
- `e1b_slow_false_<bin>` - slow body returned false, `out` empty (the ceiling per
  bin). `e1b_slow_reverse_<bin>` - perfect but reversed (return true; out of
  scope, counted for completeness). `e1b_slow_rebuilt_<bin>` - not perfect (solver
  actually did work).
- `e1b_avoided_solver_ns_<bin>` - for calls where classifier == accept AND slow
  body == false, add the measured `find_intersections` (+`contour_direction`,
  negligible) nanos for THAT call. This is the solver time the flip would skip.
- `e1b_classifier_scan_ns_<bin>` - the classifier's OWN scan nanos, on EVERY call
  it runs, split so the accounting matches 2.3's precedence: the always-paid O(n)
  screen (triple/winding + convex fast-accept) is timed on every call including
  `n >= cutoff` rings, and the O(n^2) extension is timed only where it runs
  (`n < cutoff`, no convex accept). The flip pays the O(n) part on all ~19.4M
  calls; that cost must NOT be omitted from `cost` just because the ring was large.

Timing methodology (a pinned hazard, not an afterthought). Per-call
`clock_gettime` across ~19.4M calls both distorts the numbers it prices - for the
dominant cheap n4_8 bin, `find_intersections` is on the order of the clock read
itself (~20ns) - and adds real contended overhead to the hotpath. E1 already hit
this and used 256 cache-line-aligned counter shards precisely because one
process-wide atomic per call becomes a hotpath cost. E1b's instrument MUST follow
suit: thread-local or sharded (cache-line-aligned) accumulation, no single
contended atomic on the per-call path; flushed/summed once at end of run. Prefer
AGGREGATE timing (one guard spanning many calls, or a per-thread running sum
divided by the per-thread call count) over an independent scoped clock on every
individual call, and account for clock-read overhead explicitly (subtract a
measured empty-guard baseline) so the sub-microsecond n4_8 numbers are not pure
clock noise. The guard around `find_intersections` lives inside the slow body
under a `cfg`/instrument gate for Landing 1; the classifier scan is real
production work and is timed the same sharded way. Read via
`brokkr sidecar <uuid> --counters` and cross-referenced with the `normalize_into`
hotpath thread-time.

Soundness cross-check counter (must be identically zero or E1b is unsound):
`e1b_unsound_accept` - classifier == accept but slow body != false. If this is
ever nonzero, STOP: the conservative test has a hole; fix or shrink acceptance
before any flip.

## 4. Landings (ordered; each kept or reverted on its gate)

Every command below that runs the full pipeline on real PBF is an explicit
measurement the USER triggers (AGENTS.md: never run the full pipeline on real PBF
unless the user asks). The spec pins the exact invocations; the operator runs
them at the measurement points.

### Landing 1 - instrument + shadow classifier (price the avoided solver cost)

Scope: promote `is_perfect_ccw_convex` to non-test, add `is_engine_perfect_ccw`,
add the shadow instrument (3.3). Control flow unchanged - the slow body still runs
on every call; `simplify_contour_into` does NOT yet early-return.

Provisional cutoff for the PRICING run: effectively UNBOUNDED (e.g. `usize::MAX`,
or the max observed n), NOT 64. A cutoff of 64 returns before the O(n^2) scan on
exactly the n33p rings (2.0M rejects, n up to thousands) that motivate having a
cutoff at all, so the O(n^2) cost is never observed where it bites and the
measurement would conclude by construction that the crossover sits below 64.
Landing 1 must scan and time every n so the per-n scan cost is real all the way
out; the shadow run pays that (bounded) cost once, to buy the crossover. (The O(n)
screens run on every call regardless, per 2.3.)

Correctness gate (BLOCKING): `brokkr check`. This is the differential oracle
(2,000-case, i_overlay dev-dependency) plus the retained soundness tests plus the
new soundness tests from section 5. It is the authoritative correctness gate for
this repo's engine work.

```
brokkr check
```

Byte-identity gate: because Landing 1 does not change output, the archive is
bit-identical to pre-change. Do NOT use `brokkr compare-tiles` (it samples 200
tiles per zoom and compares aggregate counts - not an identity gate) and do NOT
use `brokkr regress --dataset denmark` (its blessed baseline
`denmark-a702427.pmtiles` is STALE, predating the boundary and ocean-VW landings).
Instead bank a pre-change archive and `cmp -s` the complete deterministic archives
(two builds of a bit-identical change are byte-identical):

```
# before landing (bank the baseline), from the pre-change commit:
elivagar run <norway-locations.pbf> -o data/scratch/e1b-base.pmtiles --locations-on-ways
# after landing:
elivagar run <norway-locations.pbf> -o data/scratch/e1b-l1.pmtiles --locations-on-ways
cmp -s data/scratch/e1b-base.pmtiles data/scratch/e1b-l1.pmtiles
```

(The `run` invocation is the raw binary path so the output goes to a scratch file
we control for `cmp`. `elivagar` is NOT on PATH in this checkout: build it with
`brokkr` and invoke the produced release binary by its resolved path
(`target/release/elivagar` from the repo root after a release build), or add a
thin brokkr passthrough - do not assume a bare `elivagar`. Resolve the norway
locations PBF from `brokkr.toml` (`datasets.norway`, `variant = locations`) rather
than a literal `<norway-locations.pbf>` placeholder; the `PBF=` env the ocean
scripts read is the same source. Before these commands are runnable the spec's
operator must PIN, in this section and in section 8, the exact pre-change commit
hash and the bench host - both are `<...>` placeholders here and the tech-spec
requires them concrete before Landing 1 runs. The `cmp -s` must exit 0. As a cheap
structural cross-check alongside `cmp -s`, `brokkr verify pmtiles` on the L1/L2
archive is optional but recommended; the byte-identity argument (bit-identical
archive renders identically) remains the authoritative gate, so the earcut/human
ocean gates are not in frame.)

Pricing read (the deliverable): commit Landing 1, then

```
brokkr tilegen --hotpath --dataset norway --variant locations
brokkr results <uuid>
brokkr sidecar <uuid> --counters
```

Compute the crossover by CUMULATIVE totals over candidate cutoffs, not by
per-bucket means (per-bucket means cannot locate a scalar cutoff, and dividing
avoided by ACCEPTED calls while dividing scan by ALL calls overstates the benefit
whenever acceptance is below 100%). For each candidate cutoff `k` (sweep k over
the observed n range using the fine per-n bins of 3.3):
- `avoided(k) = sum over accepted-and-slow-false calls with n < k of
  e1b_avoided_solver_ns` - solver time the flip skips when the O(n^2) test is
  enabled up to k. (Convex fast-accepts contribute at every n, since the convex
  screen is not cutoff-gated.)
- `cost(k) = [O(n) screen ns on ALL calls at every n] + [O(n^2) extension ns on
  calls with n < k]` - the full production cost the flip pays at cutoff k,
  including the always-run O(n) screen on `n >= k` rings (per 2.3). Both terms are
  TOTALS across all calls, on the same denominator as `avoided(k)`, so `net` is an
  apples-to-apples time difference, not a mean ratio.
- `net(k) = avoided(k) - cost(k)`. The chosen `ENGINE_PERFECT_CUTOFF` is the k
  that MAXIMIZES `net(k)` (the marginal per-n crossover - where adding the next n's
  O(n^2) scan stops paying - is where this maximum sits).
- `e1b_unsound_accept` MUST be 0.

Proceed/close threshold (explicit, set here so the reading is judged, not
rationalized): E1b proceeds to Landing 2 only if `net` (at the measured cutoff)
is BOTH positive with >= 2x margin over run-to-run noise AND >= 5% of the
`normalize_into` hotpath thread-time on this bench. The 5% DENOMINATOR is the
PRE-INSTRUMENT baseline (the clean ~129 thread-s at the commit before Landing 1),
NOT the number the Landing 1 hotpath run reports: in L1 the shadow classifier runs
its full scan inside `normalize_into`, so the L1-measured `normalize_into` time is
inflated by exactly `cost` and would flatter the fraction. Null-hypothesis
arithmetic:
`normalize_into` is ~129 thread-s on norway locations (roadmap H6); 5% is ~6.5
thread-s. A correct-but-not-worth-it classifier looks like this - it soundly
accepts the 0.974 ceiling but the accepted calls are dominated by cheap n4_8
solves the solver already does fast, so `avoided` is small and `cost` (the scan
on all ~19.4M calls) eats it; `net` lands below 6.5 thread-s and E1b CLOSES.
E1 already showed the passes cluster cheap; E1b only wins if broadening into the
expensive n9_32/n33p perfect rings adds enough avoided solver time to clear the
scan cost at those n, which is precisely what the per-bucket crossover decides.

If the threshold is NOT met: CLOSE E1b (honest close path). Remove the instrument
and the shadow classifier wiring; return `is_perfect_ccw_convex`,
`is_engine_perfect_ccw`, and the soundness tests to `cfg(test)` as the retained
predicate/gate (mirroring the E1 close). Append a dated close to
`reference/performance.md` (the buckets, `avoided`/`cost`/`net`, the commit and
bench UUID) and update the E1b bullet in `notes/planet-30gb-roadmap.md` to CLOSED.
No Landing 2.

### Landing 2 - flip the shadow to a real skip (only if Landing 1 cleared)

Scope: set `ENGINE_PERFECT_CUTOFF` to the measured crossover; make
`simplify_contour_into` early-return `false` when `is_engine_perfect_ccw` accepts
(3.2); remove the Landing 1 shadow timers and pricing counters (they were the
instrument, not the product - tech-spec: no benchmark scaffolding left behind).
The classifier itself and its tests stay in production.

This is a bit-identical change by construction: an accepted contour gets the same
`false` + untouched-`out` verdict the slow body produced. Therefore the identity
gate is a soundness gate.

Correctness gate (BLOCKING):

```
brokkr check
```

Byte-identity gate (also the soundness proof at artifact scale):

```
elivagar run <norway-locations.pbf> -o data/scratch/e1b-l2.pmtiles --locations-on-ways
cmp -s data/scratch/e1b-base.pmtiles data/scratch/e1b-l2.pmtiles
```

`cmp -s` MUST exit 0 against the same banked `e1b-base.pmtiles`. Any byte
difference means the classifier accepted a contour the engine would have rewritten
- an unsound accept - and Landing 2 is reverted, not tuned. (Optionally repeat on
denmark locations for the broadest tile coverage - 1.3M tiles - banking a
denmark base the same way; denmark stresses the 11.4M-call shape.)

Performance verdict (the keep decision): a wall-clock `--bench 3` comparison
between two UNINSTRUMENTED commits, per `reference/performance.md`'s rule that
keep/revert decisions come from clean best-of-N bench runs (hotpath ranks
functions and is supporting evidence, never the sole keep decision). The two
commits are: the pinned PRE-L1 commit (no shadow instrument, no classifier - the
clean baseline; Landing 1's timers must NOT be the baseline, since L2 removes them
and their removal would masquerade as the classifier's win) and the L2 commit
(classifier live, instrument gone). Commit Landing 2 first, then:

```
brokkr tilegen --bench 3 --dataset norway --variant locations   # L2 commit
brokkr tilegen --bench 3 --commit <pre-L1-hash> --dataset norway --variant locations
brokkr results --compare <L2-uuid> <pre-L1-uuid>
brokkr tilegen --hotpath --dataset norway --variant locations   # supporting: normalize_into thread-time
```

Keep iff the wall-clock best-of-3 improves consistently with the `net` Landing 1
predicted and `normalize_into` hotpath thread-time drops correspondingly (within
noise). Record the pre-change baseline (pinned commit + host) and the post-change
number in `reference/performance.md`, anchored to the Landing 2 commit hash
(tech-spec point 10: commit, then benchmark, then write the hash-anchored
numbers). Variant pinned to locations - a raw-vs-locations read is not a verdict.

## 5. Named unit tests (behavior no oracle reaches)

In the retained `#[cfg(test)] mod tests` of `simplify.rs`. The existing
`assert_slow_verdict` helper (asserts the slow body returns `!rebuilt` with empty
`out`) is the soundness assertion; reuse it verbatim.

1. `engine_perfect_ccw_screen_cases` - mirror `perfect_ccw_convex_screen_cases`
   for `is_engine_perfect_ccw`: the existing convex accepts still accept; ADD
   non-convex simple CCW rings that must now accept (e.g. an L-shape, a reflex
   hexagon) and, for each, `assert_slow_verdict`. Keep every existing reject
   (CW, collinear, duplicate, bowtie, the self-lapping {5/2} pentagram, the
   figure-with-inner-loop, degenerate n<3). The pentagram and the concave-with-
   crossing cases are the load-bearing rejects proving invariants 2/4 hold beyond
   convexity.
2. `engine_perfect_ccw_is_sound_on_random_contours` - extend the existing 10,000
   random-contour + 2,000 affine-copy soundness loop to call
   `is_engine_perfect_ccw`; for every accept, `assert_slow_verdict`. Raise the
   random vertex count range so contours reach into the n9_32 bucket (the
   broadened acceptance region), and seed a generator that produces simple
   non-convex rings (e.g. star-shaped polygons from sorted angular sweeps) so the
   accept branch is exercised at n > convex-only shapes reach.
3. `engine_perfect_ccw_cutoff_falls_through` - a simple NON-CONVEX CCW ring with
   `n >= cutoff` must return false (fall through), and `assert_slow_verdict` still
   holds (the slow body would have accepted it - we simply chose not to; sound,
   just not maximal). It MUST be non-convex: a convex ring at any n is caught by
   the always-run O(n) convex fast-accept (2.3) and would NOT fall through, so a
   convex fixture here would wrongly assert the opposite of the pinned precedence.
4. `engine_perfect_ccw_rejects_snap_radius_ambiguity` - a ring with a
   near-touch within `snap_radius` that the split solver would rewrite must be
   rejected (conservative). If constructing an exact snap-radius case is fragile,
   at minimum assert the classifier never accepts where `assert_slow_verdict`
   would fail - which is the general soundness loop's job, so this test is the
   targeted witness.

Soundness is the one property tests must pin exhaustively: `is_engine_perfect_ccw`
true ==> slow body returns false with empty out. Completeness (accepting all
perfect rings) is NOT required - conservative rejection is always safe.

## 6. Verification matrix (every brick names its gate)

| brick | gate | exact command |
|---|---|---|
| L1 predicate + shadow instrument, correctness | `brokkr check` | `brokkr check` |
| L1 byte identity (shadow is output-neutral) | `cmp -s` banked archives | `cmp -s data/scratch/e1b-base.pmtiles data/scratch/e1b-l1.pmtiles` |
| L1 pricing (the deliverable) | hotpath + sidecar counters | `brokkr tilegen --hotpath --dataset norway --variant locations` then `brokkr sidecar <uuid> --counters` |
| L2 flip, correctness + soundness | `brokkr check` | `brokkr check` |
| L2 byte identity == soundness at scale | `cmp -s` vs same base | `cmp -s data/scratch/e1b-base.pmtiles data/scratch/e1b-l2.pmtiles` |
| L2 perf keep/revert | hotpath, `normalize_into` thread-time | `brokkr tilegen --hotpath --dataset norway --variant locations` then `brokkr results <uuid>` |

Why these and not others: `brokkr regress` is unavailable as an identity gate
(stale denmark blessed baseline); `brokkr compare-tiles` samples and aggregates,
so it cannot prove byte identity; `--alloc` is the wrong instrument (E1b skips CPU
work, not allocations - the skip may not even change allocation volume). The
earcut / boundary-line oracles are not in frame: E1b changes no emitted geometry
(if it did, `cmp -s` would fail first). No MapLibre human check is needed for the
same reason - a bit-identical archive renders identically.

## 7. Data flow (unchanged except one early return)

`normalize_into` (single-contour arm) -> `simplify_contour_into` ->
`is_engine_perfect_ccw`:
- accept: return `false` immediately, `out` untouched -> caller keeps its
  allocation, runs `clean_shape_in_place`, pushes the shape (existing code path).
- reject: `simplify_contour_into_slow` exactly as today.

No type changes, no ownership changes, no boundary changes. The only new module
state is one `usize` const. The Landing 1 instrument's counters live in the
existing sidecar channel and are deleted at Landing 2 / close.

## 8. Baselines (tech-spec point 10)

- Measurement record: `reference/performance.md` (E1 close section, `65ae629`,
  norway locations) + `.brokkr/results.db`. E1b's Landing 1 pricing and Landing 2
  perf numbers are written back here, hash-anchored, per the same discipline.
- Pre-change baseline for the perf verdict: the `normalize_into` norway-locations
  hotpath thread-time at the commit immediately before Landing 2 (record commit +
  host). E1 measured ~19.4M single-contour calls on this bench; that call shape is
  the denominator.
- Byte-identity baseline: `data/scratch/e1b-base.pmtiles`, built by `elivagar run`
  on the norway locations PBF at the pre-change commit, banked once and reused for
  both L1 and L2 `cmp -s`.

## 9. Stopping rule (blast radius)

- Files touched: `src/geometry/overlay/port/simplify.rs` (predicate promotion,
  new classifier, wiring, tests) and, for Landing 1 only, a scoped timer in the
  slow body plus counter emission in `src/debug.rs`. Nothing else.
- The split solver, segment builder, loop test, extraction, and every other
  engine module are READ (to reproduce their verdict) but NOT modified. E1b does
  not change noding, so it does not touch the baseline-rotating surface (E4c).
- If Landing 1 prices below threshold, the teardown is: revert to the E1-retained
  `cfg(test)` state plus a dated close. That is the terminal state; E1b does not
  spawn follow-ups. A future exact-classifier idea (e.g. a cheaper-than-O(n^2)
  simplicity test) would be a NEW item, not a reopening of E1b.
- Explicitly out of scope: the reversed-winding fast path, the multi-contour arm,
  `intersect_rect_into`, E2/E3/E4*.

## 10. Lateral-findings channel

While implementing, flag (do not silently fix) anything surprising: a cheaper
exact simplicity test than pairwise O(n^2); evidence the reversed-winding case is
larger than assumed; any place `find_intersections` does work the classifier could
also skip; or any `normalize_into` caller that already knows its contour is
perfect and could bypass `simplify_contour_into` entirely (a structurally better
win than screening every call). Label findings bug / gap / smell / nit.

## 11. Review reconciliation (R1 Opus + R2 Codex, folded 2026-07-13)

Both reviews were validated against source before folding. Every valid finding is
now IN the body above; this section is the audit trail, not a second home for the
fixes.

Folded (accepted):
- Cutoff not pinnable from four coarse buckets (R1 gap / R2 blocker 1). Section
  3.3 now counts at per-n granularity with an open top bin; section 4 Landing 1
  computes the crossover from CUMULATIVE `net(k)` over candidate cutoffs, on a
  single consistent denominator (fixing the avoided-per-accepted vs
  scan-per-all mismatch R2 flagged).
- Provisional cutoff 64 hides the motivating n33p cost (R1 gap). Landing 1 now
  prices with an effectively UNBOUNDED cutoff.
- Invariant-2 simple-polygon test was `todo!()` prose (R1 gap / R2 blocker 3).
  Section 2.2 pins it concretely: primitive (`CrossSolver::cross`), radius source
  (`snap_radius`, solver-dependent - so the function became an `Overlay` method,
  section 3.1 signature corrected), cyclic adjacency exclusion, reject
  classification over `CrossType::{Pure,Overlay,TargetEnd,OtherEnd}`, and the
  inclusive bbox-proximity margin.
- Adjacency trap (R1 smell). Folded into 2.2's adjacency rule (cyclic, closing
  pair included, chords are non-adjacent).
- Sub-microsecond per-call timing hazard + E1's 256-shard precedent (R1 gap / R2
  high 6). Section 3.3 now mandates sharded/thread-local accumulation, aggregate
  timing, and explicit clock-overhead subtraction.
- 5% denominator inflated during L1 (R1 nit). Section 4 pins it to the
  pre-instrument baseline.
- Perf verdict violated best-of-N and was confounded by instrument removal (R2
  blocker 2). Landing 2 now uses `--bench 3` between the pinned pre-L1 and the L2
  commits, hotpath as supporting evidence.
- Cutoff-precedence contradiction: convex-first vs "no attempt >= cutoff", and the
  scan counter omitting the always-paid O(n) cost (R2 high 4). Section 2.3
  resolves the ordering (O(n) screens always run; cutoff gates only the O(n^2)
  extension); 3.1, 3.3, and unit test 3 are aligned.
- Early return skips `self.clear()` (R2 high 5). Section 3.2 requires the accept
  path to clear (or refactors clear into the shared entry) so warm-engine state
  matches the all-slow baseline.
- Non-copy-pasteable gates / unpinned baseline (R2 medium 7). Landing 1 note now
  says how to resolve the binary (not on PATH) and PBF (from brokkr.toml), and
  requires the pre-change commit + host be pinned before running.
- Stale `notes/rendering-fix-log.md` citation (R2 medium 8). Corrected to
  `notes/rendering-postmortem.md` (verified: old file deleted).
- "Exact" overclaims a screen that admits false negatives (R2 closing nit). A
  terminology note in 3.1 pins the contract as "conservatively sound perfect-CCW
  screen"; the title keeps "exact" for roadmap continuity.

Partially accepted:
- Contract's fresh-archive verification gate (R2 medium 7, second half). The
  spec's `cmp -s` byte-identity argument is sound - a bit-identical archive
  renders identically, so the earcut/human ocean gates genuinely are not in
  frame. Rather than add a redundant blocking gate, `brokkr verify pmtiles` is
  folded as an OPTIONAL structural cross-check (Landing 1 note); `cmp -s` stays
  authoritative.

Rejected: none. No finding was found factually wrong or inapplicable; R1
independently verified the source decomposition and found no factual errors, and
R2's blockers all reproduced against the code (`self.clear()` at slow-body open,
`CrossSolver::cross(.., radius)` signature, missing PATH binary).
