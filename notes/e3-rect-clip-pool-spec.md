# E3: pool `clip_shape_rect_fast` through the reconnection logic

Implementation specification. Written against
`reference/technical-implementation-spec.md` (the contract). Spawned from the
E-surface backlog in `notes/planet-30gb-roadmap.md`, item **E3** in the H6
post-port engine section ("pool `clip_shape_rect_fast` through the reconnection
logic"); the measurement record it is read against is `reference/performance.md`
plus `.brokkr/results.db`.

## Problem

`clip_shape_rect_fast` (`src/geometry/pyramid.rs`) is the middle tier of
`intersect_shapes_with_rect`, the per-cell polygon clip that drives EVERY
polygon layer's pyramid descent (`descend`, `root_bisect`,
`split_for_parallel`). The identity tier above it is already pooled (it draws a
shell from `scratch.take_shape` and fills it with `copy_shape_into`); the exact
boolean tier below it (`intersect_rect_into`) was pooled in Landing 2. The
middle tier - reached for every shape that straddles a cut line, i.e. most
shapes at most zooms - was flagged by the Landing 2 review as the next-largest
sound churn target and correctly declined at the time. It allocates
unconditionally and per call:

- `let mut outers: Vec<Contour> = vec![shape[0].clone()];` - one list Vec plus a
  cloned outer contour.
- four half-plane passes, each `let mut next = Vec::with_capacity(outers.len())`
  - a fresh list Vec per pass.
- per hole: `let mut parts = vec![hole.clone()];` plus a fresh `next` per pass.
- `clip_ring_half_plane_multi`, called once per ring per pass, returns a fresh
  `Vec<Contour>` and internally allocates `chains: Vec<Contour>` (each chain a
  fresh contour), a per-chain `current: Option<Contour> = Some(vec![c])`,
  `endpoints: Vec`, `exit_to_entry: Vec`, `visited: Vec`, `rings_out:
  Vec<Contour>` (each output a fresh contour), plus the `Some(vec![ring.clone()])`
  / `Some(Vec::new())` early returns.
- component assembly: `component = Vec::with_capacity(..)` in the single-outer
  case, and `components: Vec<Shape> = outers.into_iter().map(|o| vec![o]).collect()`
  in the multi-component case.

Every one of those is a fresh heap allocation on a path taken millions of times
per denmark build. The theory (roadmap E3): thread the engine scratch in,
ping-pong two pooled contour lists across the four passes, draw every contour
and every component shell from a pool. The transform is geometry-neutral -
same crossings, same reconnection, same winding, same order - so the design
claim is **byte-identical output**. The blocking gate is `brokkr check` (the
differential oracle plus `fast_rect_clip_equivalent_to_boolean`); byte-identity
is confirmed by banking a pre-change denmark-locations archive and comparing it
byte-for-byte (`brokkr compare-tiles`) against the post-change build. `brokkr
regress --dataset denmark` is NOT the gate here - its blessed baseline is
currently stale (see Brick 2) and it proves only semantic, not byte, equality.

This is the opener the roadmap names: "a self-contained brick; spec it as the
opener of whichever campaign touches the descent next." It survives H5 deleting
the ocean recompute because the descent clip runs for all OSM polygon layers,
not just ocean.

## Survey of the ground

### The call graph and ownership today

`intersect_shapes_with_rect(scratch: &mut IntEmitScratch, shapes: &Shapes, rect,
_min_area, out: &mut Shapes)` (pyramid.rs) is the only caller of
`clip_shape_rect_fast`, which is the only caller of `clip_ring_half_plane_multi`.
Both helpers are module-private to pyramid.rs and have NO direct unit tests -
they are gated only through `intersect_shapes_with_rect`. The three production
call sites of `intersect_shapes_with_rect` (`root_bisect` line ~263, `descend`
line ~319, `split_for_parallel` line ~148) all already hold `&mut
PyramidScratch` and pass `&mut scratch.int`. The signature of
`intersect_shapes_with_rect` does NOT change in this spec; only its body and the
two private helpers do. So the three production callers and every test that
calls `intersect_shapes_with_rect` are untouched by signature.

Ownership contract that must be preserved: `out` receives pooled `Shape` shells
whose contours are pooled contours; downstream (`emit_cell`, the next descent
level, `scratch.return_shapes` -> `int.recycle_shapes`) recycles them back into
the pool. The identity tier already demonstrates the target pattern -
`scratch.take_shape(shape.len())` + `copy_shape_into` + `out.push(copy)`. The
fast path must produce shells of the same provenance so the existing recycle at
`descend`/`root_bisect`/`split_for_parallel` (`scratch.return_shapes`) feeds
them back correctly. It already does structurally (they are plain `Shape`s); the
change is only WHERE the shells and contours come from (pool vs fresh).

### The existing pool (what to reuse, not rebuild)

`BooleanExtractionBuffer` (`src/geometry/overlay/port/extract.rs`) owns
`contour_pool: Vec<IntContour>` and `shape_pool: Vec<IntShape>`, drained by
`take_vec_with_capacity` (best-fit-by-capacity linear scan). It exposes
`take_shape(ring_count) -> IntShape`, `recycle_owned_shape`, `recycle_shapes`,
`recycle_shapes_from`, `recycle_shape`, `recycle_contours_from`, and PRIVATE
`take_contour_from_points` / `recycle_contour`. `BoolOverlay`
(`src/geometry/overlay/mod.rs`) forwards the public six; `IntEmitScratch`
(`src/geometry/int_ocean.rs`) forwards them again as `take_shape`,
`recycle_owned_shape`, `recycle_shapes`, `recycle_shape`,
`recycle_contours_from`, `recycle_shapes_from`.

There is today NO exposed take/recycle for a bare `Contour`, and no pooled
scratch for the three primitive Vecs inside `clip_ring_half_plane_multi`. This
spec adds exactly those, reusing the boolean engine's `contour_pool` (the same
warm buffers the identity tier and the boolean tier already share - the fast
path and the boolean tier are never live for the same shape simultaneously, so
sharing one pool is correct and keeps buffers hot).

### `IntEmitScratch` sub-scratch precedent

`IntEmitScratch` already composes per-algorithm sub-scratch structs cleared per
call: `dp: DpScratch`, `vw: VwScratch`, plus `rect_contour: Contour`. Adding a
`rect_clip: RectClipScratch` field is the same pattern. That is where the three
primitive Vecs and the two ping-pong list buffers live.

### Failure history check

`notes/rendering-postmortem.md` (the R-ledger, R01-R23 and the S-series) is the
ledger a geometry spec must clear. This change touches NO geometry decision - crossings, snap-rounding,
Jordan pairing, tie guards, winding enforcement, hole re-nesting are all copied
verbatim; only allocation provenance changes. R23 (the ClosePath cursor bug) and
the R23-class boolean fallback net are untouched: the fallback still fires on
odd crossing counts, colliding snap-rounded line positions, bridges that fail to
join an exit to an entry, and un-nestable holes, exactly as today. (Precision
note for the reviewer: a vertex lying exactly ON the cut line is NOT itself a
fallback trigger - the `inside` test uses `<=`/`>=`, so on-line vertices classify
as inside and the exact rational crossing reproduces the crossing on the vertex
verbatim. What triggers a fallback is a tangency touching the line from outside,
which yields two same-position crossings caught by the tie guard - `pyramid.rs`
line ~734, `endpoints.windows(2)` equal-position check. Do not paraphrase the
trigger as "on-line vertices.") No logged failure is re-proposed.

### Existing local gate

`fast_rect_clip_equivalent_to_boolean` (pyramid.rs tests, line ~1530) already
drives `intersect_shapes_with_rect` across contained / straddle / split /
on-line-vertex tiers and asserts XOR-empty against the exact boolean. Because
this spec preserves `intersect_shapes_with_rect`'s signature and semantics, this
test is the standing per-refactor correctness gate and must remain green
unchanged. `convexity_early_out_vs_normalize_equivalence`,
`cut_identity_dp_tol_0_geometry_equivalence_reference`, and the seam-window XOR
tests likewise ride the same entrypoint and must stay green.

## Target artifacts

### 1. Contour-level pool exposure

`BooleanExtractionBuffer` (`src/geometry/overlay/port/extract.rs`) - add two
pub(crate) methods delegating to the existing pool, alongside
`take_contour_from_points` / `recycle_contour`:

```rust
pub(crate) fn take_contour(&mut self, cap: usize) -> IntContour {
    let mut c = take_vec_with_capacity(&mut self.contour_pool, cap);
    c.clear();
    c
}

pub(crate) fn recycle_owned_contour(&mut self, mut c: IntContour) {
    c.clear();
    self.contour_pool.push(c);
}
```

(The private `recycle_contour` already does exactly the recycle body; expose it
under a pub(crate) name rather than duplicating, or keep both - the private one
is called from `take_contour_from_points`'s siblings. Naming: `recycle_contour`
is already taken as private; `recycle_owned_contour` is the pub(crate) twin, or
simply widen the existing `recycle_contour` visibility to pub(crate) and drop
the new name. Pick one and use it consistently.)

`BoolOverlay` (`src/geometry/overlay/mod.rs`) - forward both:

```rust
pub(crate) fn take_contour(&mut self, cap: usize) -> Contour { self.inner.take_contour(cap) }
pub(crate) fn recycle_owned_contour(&mut self, c: Contour) { self.inner.recycle_owned_contour(c); }
```

`IntEmitScratch` (`src/geometry/int_ocean.rs`) - forward again:

```rust
pub(crate) fn take_contour(&mut self, cap: usize) -> Contour { self.overlay.take_contour(cap) }
pub(crate) fn recycle_owned_contour(&mut self, c: Contour) { self.overlay.recycle_owned_contour(c); }
```

### 2. `RectClipScratch`

New sub-scratch in `src/geometry/int_ocean.rs`, field on `IntEmitScratch`:

```rust
#[derive(Default)]
pub(crate) struct RectClipScratch {
    // ping-pong contour lists for the four half-plane passes. Used first for
    // the OUTER passes, then re-used for each hole's passes (safe because by
    // the time a hole is clipped the surviving outers already live in
    // `outers_final`, so list_a/list_b are free to ping-pong again).
    list_a: Vec<Contour>,
    list_b: Vec<Contour>,
    // Stable parking for the surviving outer contours across the whole hole
    // phase. MUST be distinct from list_a/list_b/chains - see the buffer-role
    // table below. This is what makes the safe (defer-all-`out`-appends)
    // ownership design possible: outers are held here, never appended to `out`,
    // until success is certain.
    outers_final: Vec<Contour>,
    // Accumulator for surviving hole contours across all holes, held until the
    // final assembly. Also distinct from every other list.
    holes_final: Vec<Contour>,
    // clip_ring_half_plane_multi per-call temporaries
    chains: Vec<Contour>,
    endpoints: Vec<(i32, usize, bool)>,
    exit_to_entry: Vec<usize>,
    visited: Vec<bool>,
}
```

Buffer-role table (the fix for the under-provisioning gap - four `Vec<Contour>`
lists are live simultaneously during the hole passes, plus `chains` one level
down, so each needs its own field; no field may be claimed by two live roles):

| field          | outer passes        | hole passes (per hole)      | assembly        |
| ---            | ---                 | ---                         | ---             |
| `list_a`       | ping-pong `outers`  | ping-pong `parts`           | free            |
| `list_b`       | ping-pong `next`    | ping-pong `next`            | free            |
| `outers_final` | (empty)             | HOLDS surviving outers      | source of shells|
| `holes_final`  | (empty)             | accumulates surviving holes | source of holes |
| `chains` (+ endpoints/exit_to_entry/visited) | detached inside `clip_ring_half_plane_multi` per ring per pass | same | free |

Note `chains` cannot double as the outers-parking buffer (an earlier draft
suggested this): `clip_ring_half_plane_multi` detaches `chains` on EVERY
ring-pass call, so parking finalized outers there would be clobbered on the
first hole-pass ring. That is why `outers_final` is a separate field.

Added to `IntEmitScratch`:

```rust
pub(crate) struct IntEmitScratch {
    // ...existing fields...
    rect_clip: RectClipScratch,
}
```

initialized `rect_clip: RectClipScratch::default()` in `new()`.

The `Vec<Contour>` list buffers hold pooled contours while in use and are
`clear()`ed (not dropped) between uses; the contours they hold are drained back
to the engine `contour_pool` via `recycle_owned_contour` before the list is
cleared. `RectClipScratch` is NOT `Clone`; it never crosses the rayon
`map_init` boundary (each worker has its own `IntEmitScratch` already).

Borrow note and PINNED API (the loose "pick one of several strategies" wording
is replaced - the implementer follows exactly this). Two facts force the shape:
(1) `rect_clip` and `overlay` are disjoint private fields of `IntEmitScratch`,
and the clip helpers need both the `rect_clip` buffers and the contour pool
(via `take_contour` -> `overlay`) live at once; (2) the clip helpers are FREE
FUNCTIONS in `pyramid.rs` and cannot name `IntEmitScratch`'s private fields
(they live in `int_ocean.rs`), so `scratch.rect_clip.chains` does not compile
from `pyramid.rs`, and merely making `RectClipScratch` `pub(crate)` exposes
neither its fields nor the `rect_clip` field.

Pin this: `IntEmitScratch` gets two `pub(crate)` accessor methods, defined in
`int_ocean.rs` where the fields are in scope:

```rust
// Detach the whole rect-clip sub-scratch so the caller owns its buffers as a
// local while `self.take_contour` / `self.recycle_owned_contour` stay callable
// (the sub-scratch is disjoint from `overlay`, so no aliasing).
pub(crate) fn take_rect_clip(&mut self) -> RectClipScratch {
    std::mem::take(&mut self.rect_clip)
}
pub(crate) fn put_rect_clip(&mut self, rc: RectClipScratch) {
    self.rect_clip = rc;
}
```

`clip_shape_rect_fast` does `let mut rc = scratch.take_rect_clip();` at entry
and `scratch.put_rect_clip(rc);` at EVERY exit (including fallbacks), passing
`&mut rc` down to `clip_ring_half_plane_multi` alongside `&mut *scratch`. The
individual buffers (`rc.list_a`, `rc.chains`, ...) are now plain locals reachable
from `pyramid.rs`, and `scratch.take_contour(..)` remains callable throughout
because `rc` no longer borrows `scratch`. This is the take-a-buffer-then-return
idiom `PyramidScratch::take_shapes`/`return_shapes` already uses, applied to the
whole sub-scratch at once. Do NOT expose `RectClipScratch`'s fields directly and
do NOT try to `std::mem::take` individual fields through a borrow of `scratch` -
the whole-struct detach is the one pinned route.

### 3. `clip_shape_rect_fast` rewritten signature

```rust
fn clip_shape_rect_fast(
    scratch: &mut IntEmitScratch,
    shape: &Shape,
    rect: IntRect,
    out: &mut Shapes,
) -> bool
```

The function detaches the sub-scratch once (`let mut rc =
scratch.take_rect_clip();`) and restores it (`scratch.put_rect_clip(rc);`) at
every exit. All buffer names below (`rc.list_a`, `rc.outers_final`, ...) are
locals through `rc`.

Body transform (semantics identical, allocation pooled). CRITICAL invariant,
carried from today's control flow: `clip_shape_rect_fast` must NOT push anything
to `out` until success is certain - a `false` return must leave `out` exactly as
the caller left it, because the boolean fallback in
`intersect_shapes_with_rect` then appends into that clean `out`. Today this holds
because outers/holes live in locals and `out` is touched only by the terminal
`out.push`/`out.extend`; the pooled design MUST preserve it (this is why
`outers_final`/`holes_final` exist rather than early-appending shells to `out`).

- `outers`: `let mut outers = std::mem::take(&mut rc.list_a); outers.clear();`
  then seed with a pooled copy of `shape[0]`: `let mut o0 =
  scratch.take_contour(shape[0].len()); o0.extend_from_slice(&shape[0]);
  outers.push(o0);`
- `next`: `let mut next = std::mem::take(&mut rc.list_b);` reused across passes
  via ping-pong. After each pass, recycle the now-consumed contours of the old
  `outers` and swap `outers`/`next`.
- Per pass, per ring, call the rewritten `clip_ring_half_plane_multi(scratch,
  &mut rc, ring, axis, bound, keep_le, &mut next)` returning `bool`; on `false`,
  before returning `false` from `clip_shape_rect_fast`, drain `outers`, `next`,
  and any partial state back to the pool (see cleanup rule) so a fallback does
  not leak the pooled buffers, then `scratch.put_rect_clip(rc)`.
- The `outers.retain(...)` filter recycles the dropped contours:
  `outers.retain(|r| keep)` cannot recycle in place; replace with a manual
  partition that pushes rejects to `scratch.recycle_owned_contour`.
- Finalize outers into `rc.outers_final` (move the surviving outer contours
  there), leaving `list_a`/`list_b` free to be re-used as the hole ping-pong.
  `outers_final` is NOT `out` - nothing is handed to the caller yet.
- Holes (SAFE deferred-append design - the earlier "move outers into `out`
  first" recommendation is STRUCK because the hole passes can still return
  `false`, and an already-appended shell would then be double-counted by the
  boolean fallback, corrupting `out`): for each hole, `let mut parts =
  std::mem::take(&mut rc.list_a); parts.clear();` seed with a pooled copy of the
  hole, ping-pong `parts`/(`rc.list_b`) through the four passes exactly as the
  outers did, and on any `clip_ring_half_plane_multi` `false` return `false` from
  `clip_shape_rect_fast` (after full cleanup + `put_rect_clip`). Surviving hole
  contours (>= 3 verts, area >= 1) are moved into `rc.holes_final`, not into
  `out`. During this whole phase `rc.outers_final` holds the finalized outers
  and `rc.holes_final` accumulates - both live simultaneously with the
  `parts`/`next` ping-pong, which is exactly why they are separate fields.
- Winding enforcement (`signed_area_2x` reverse) operates in place on the
  pooled contours in `outers_final`/`holes_final` - unchanged.
- Assembly is the LAST action, after the final "hole not inside any outer ->
  return false" nesting check has passed (no fallback can fire past this point):
  - Single-outer case: push one pooled shell via `scratch.take_shape(0)` and
    `push` the already-pooled outer + hole contours into it (requesting
    `take_shape(0)` avoids the placeholder churn of `take_shape(n)`'s pre-filled
    empty contours). Then `out.push(shell)`.
  - Multi-outer case: build the components by `point_in_contour` nesting over
    `outers_final`, each component a pooled shell from `scratch.take_shape(0)`
    with its outer and matched holes pushed in, appended to `out` - eliminating
    the fresh `components: Vec<Shape>` allocation. This nesting is the last
    fallback point: if a hole's probe vertex is inside no outer, clean up and
    return `false` BEFORE any `out.push`.
  - Only after assembly succeeds: `scratch.put_rect_clip(rc)` and return `true`.

Cleanup rule (critical): every early return - `false` fallback, empty-outer
`true`, empty-after-filter `true`, failed hole nesting `false` - must (1) drain
all live pooled contours (in `outers`/`next`/`outers_final`/`holes_final` and any
`parts`) back via `recycle_owned_contour`, (2) return the detached list/primitive
locals into `rc` (`clear()`ed, so they stay warm) - `rc.list_a = outers; ...` -
and (3) `scratch.put_rect_clip(rc)` before returning, so the whole sub-scratch is
restored and the next call finds warm buffers. A `finish`/`return_buffers(scratch,
&mut rc, outers, next, ...)` helper at each exit keeps this from being
copy-pasted across the five exit sites. Note the pool is a plain `Vec<Vec<_>>`
free-list; a leaked (never-recycled) contour is a correctness-safe churn
regression, not a crash - but the whole point is zero churn, so the cleanup must
be complete. The pool-neutrality unit test (below) pins this via take/recycle
balance counters across all five exit paths.

### 4. `clip_ring_half_plane_multi` rewritten signature

```rust
fn clip_ring_half_plane_multi(
    scratch: &mut IntEmitScratch,   // for take_contour / recycle_owned_contour
    rc: &mut RectClipScratch,       // the detached sub-scratch (chains, endpoints, ...)
    ring: &Contour,
    axis: Axis,
    bound: i32,
    keep_le: bool,
    out: &mut Vec<Contour>,   // pooled parts appended here
) -> bool                     // false == fall back to boolean
```

- `rc` is the sub-scratch the caller detached via `take_rect_clip`; it is a
  distinct value from `scratch` (which retains only `overlay` and the other
  fields), so `scratch.take_contour(..)` and `rc.chains` are usable together
  with no aliasing.
- `ring` is borrowed from a list buffer owned by the caller (`outers`/`parts`) -
  a distinct allocation from the pool and primitives, no borrow conflict.
- `n < 3` -> `true` (nothing appended), matching today's `Some(Vec::new())`.
- `!any_out` -> append one pooled copy of `ring` to `out`, return `true`
  (today's `Some(vec![ring.clone()])`).
- `!any_in` -> `true`, nothing appended (today's `Some(Vec::new())`).
- `chains`: `std::mem::take(&mut rc.chains)`, `clear()`; each chain is a pooled
  contour from `scratch.take_contour`. `current: Option<usize>` indexes into
  `chains` instead of owning a `Contour`, OR keep `current: Option<Contour>` as
  a single pooled contour taken from the pool. The point-dedup pushes
  (`chain.last() != Some(p)`) are unchanged. Restore `rc.chains` at every exit.
- `endpoints`, `exit_to_entry`, `visited`: `std::mem::take` from `rc`,
  `clear()`, size as today. All the guard logic (odd count via
  `endpoints.windows(2)` tie check, `exit_to_entry` MAX check, cycle
  `visited` check) is copied verbatim.
- `rings_out`: written directly into `out` as pooled contours
  (`scratch.take_contour`, `extend`, push to `out`). The `>= 3` and
  first==last pop filters are unchanged.
- Every `false` return path must recycle the pooled `chains` contours built so
  far (they are not moved into `out`) and restore the primitive buffers. Every
  `true` path recycles the `chains` contours (their points were copied into
  `out`'s rings) and restores buffers. Chains contours are ALWAYS recycled -
  they are scratch, never handed out; only `out`'s contours escape.

### 5. Call-site edit in `intersect_shapes_with_rect`

```rust
if clip_shape_rect_fast(scratch, shape, rect, out) {
    continue;
}
intersect_rect_into(scratch, shape, rect, 0, &mut clipped);
out.append(&mut clipped);
```

Note the fallback must be reached with `out` unmodified by the failed fast path.
This is the whole reason the safe (deferred-append) ownership design in
artifacts 2-3 is mandatory: the fast path must NOT append to `out` until it has
committed to returning `true`. An earlier draft that appended component shells to
`out` incrementally during the hole phase is STRUCK - a later hole-pass fallback
would then leave those shells in `out`, the boolean fallback would append more,
and `out` would carry doubled geometry. Restructure so shells are appended only after the
outer set is finalized AND no fallback can still fire from the hole passes -
i.e. run BOTH the outer passes and all hole passes into pooled lists first, and
only assemble+append shells to `out` at the very end, once success is certain.
This preserves today's invariant that a `false` return leaves `out` exactly as
the caller left it (today the function pushes to `out` only in the terminal
`out.push`/`out.extend` after every fallback point). Concretely: keep surviving
outers and surviving holes in detached pooled lists; the LAST action, after the
final "hole not inside any outer -> return false" check, is the shell assembly
and `out` append. That matches today's control flow (the multi-component
`out.extend(components)` is the last statement).

## Bricks (ordered landings)

Each brick keeps `brokkr check` and `elivagar verify` green at its boundary.

### Brick 0 - price it, set the proceed threshold

The roadmap lists E3 among "E1-E3 ... the ones with measured evidence," but the
specific exclusive-alloc mass of `clip_shape_rect_fast` +
`clip_ring_half_plane_multi` at HEAD has not been re-read since Landing 2. Before
any rewrite, capture the current allocation attribution.

Instrumentation prerequisite (do this FIRST - the pricing gate is unreadable
without it): none of `intersect_shapes_with_rect`, `clip_shape_rect_fast`, or
`clip_ring_half_plane_multi` currently carries `#[hotpath::measure]` (verified at
HEAD - `pyramid.rs` has zero hotpath attributes). Under `--alloc`, an
uninstrumented frame's allocations are attributed to its nearest instrumented
ANCESTOR, so the three per-function rows the pricing gate reads simply do not
exist until the attribute is added. Add `#[hotpath::measure]` to all three
functions, matching the crate's existing usage (e.g. `src/geometry/simplify.rs`,
`src/multipolygon.rs`). Retain these attributes through Brick 3 (the post-change
alloc re-read needs the same frames to compare) - they compile out when the
`hotpath` feature is off, so there is no production cost; decide at Brick 3
whether to keep them permanently or strip them in the final commit. This
instrumentation is a real source change, so it must be committed before the
`--alloc` measurement (a `--alloc` run requires a clean tree; `--force` leaves
the result unstored).

Gate command (exact):

```
brokkr tilegen --alloc --dataset denmark --variant locations
brokkr results <uuid>
```

Read the per-function exclusive-alloc report for `clip_shape_rect_fast` and
`clip_ring_half_plane_multi` (and their parent `intersect_shapes_with_rect`, now
also instrumented). Record the combined GB into this spec and
`reference/performance.md` against the commit hash of the instrumentation commit.

**Proceed threshold:** if the two frames' combined exclusive alloc is below
**1.0 GB** on denmark-locations, close E3 as mispriced - the surgery's churn win
cannot justify the reconnection-lifetime rewrite, and the item is retired here
(the estimate motivated the spec; only this measurement justifies the landing).
At or above 1.0 GB, proceed to Brick 1. (Rationale for the bar: Landing 2's
whole normalize+intersect de-churn moved 8.1 -> 6.3 GB / ~1.8 GB for a large
wall+RSS payoff on germany; a sub-1 GB frame is below the noise where a
lifetime-heavy rewrite earns its maintenance cost.)

This brick lands only the three `#[hotpath::measure]` attributes (no behavior
change) plus a recorded number and a go/no-go. Commit the instrumentation, then
measure it on a clean tree (the attributes are the only `src/` delta; the
measured commit hash is the instrumentation commit, recorded above).

### Brick 1 - pool infra + clip-helper rewrite (ONE commit)

The pool exposure (artifact 1) and the `RectClipScratch` field + accessors
(artifact 2) have no independent consumer - landed alone they are dead code with
a `#[allow(dead_code)]` window the contract's "one pinned coherent sequence"
disallows. So they are UNCONDITIONALLY combined with the clip-helper rewrite
(artifacts 3, 4, 5) into a single commit: the two helper signatures, the pool
API they consume, and the `intersect_shapes_with_rect` call site cannot compile
apart, and together they leave no dead code. Do not split this.

Land in one commit:
- artifact 1 (contour-level pool exposure: `take_contour` / `recycle_owned_contour`
  through `BooleanExtractionBuffer` -> `BoolOverlay` -> `IntEmitScratch`),
- artifact 2 (`RectClipScratch` with the six buffer fields, the `rect_clip`
  field, `take_rect_clip`/`put_rect_clip`, `new()` init),
- artifacts 3, 4, 5 (the two rewritten clip helpers and the call-site edit),
- the new pool-neutrality unit test (below).

Gates (all exact):

```
brokkr check
```

`brokkr check` runs `fast_rect_clip_equivalent_to_boolean`,
`convexity_early_out_vs_normalize_equivalence`,
`cut_identity_dp_tol_0_geometry_equivalence_reference`, the seam-window XOR
tests, and the new pool-neutrality unit test (below) - the geometry-equivalence
gate no oracle-external tool reaches.

Add one unit test in the pyramid tests module,
`fast_rect_clip_pool_is_reused_and_leak_free`. Design note (the naive
"pool length is stable and non-empty" check is INSUFFICIENT and must not be the
only assertion): a partial leak can settle at a stable, smaller non-empty
equilibrium - an implementation that leaks every consumed INPUT contour while
recycling one OUTPUT contour per iteration keeps the pool length stable and
nonzero, so length-stability alone passes while churn persists. It also says
nothing about lost `endpoints`/`visited`/list buffers. This unit test is a
COARSE guard; the authoritative leak verdict is Brick 3's `--alloc` re-read
(exclusive alloc must drop toward zero). Pin the test as:

- Expose `#[cfg(test)]` take/recycle BALANCE counters on the pool chain: count
  every `take_contour` / `take_shape` and every `recycle_owned_contour` /
  `recycle_owned_shape` / `recycle_shapes`, and assert that after a full clip +
  recycling the `out` back, takes == recycles (net zero contours and shapes held
  outside the pool). A `#[cfg(test)]` `contour_pool_len()` accessor on the
  buffer/overlay/scratch chain gives the aggregate; the balance counter gives
  the direction the length check misses.
- Assert the `RectClipScratch` buffers are RESTORED (non-detached) between calls:
  after a clip returns, `rc.chains`/`endpoints`/`exit_to_entry`/`visited`/`list_a`
  /`list_b`/`outers_final`/`holes_final` are all present (a `#[cfg(test)]`
  accessor confirming they are back in `scratch.rect_clip`, e.g. their capacities
  are observable and non-lost), catching a `std::mem::take` without a matching
  restore on some exit path.
- Exercise ALL FIVE outcome paths, not just the happy straddle: (1) successful
  multi-crossing clip, (2) empty output (outer vanishes / filtered to nothing),
  (3) outer-pass fallback (`clip_ring_half_plane_multi` returns false on an
  outer), (4) hole-pass fallback (false on a hole), (5) failed hole nesting
  (a surviving hole inside no outer). Each must leave the pool balanced and the
  buffers restored - these are exactly the five early-return sites the cleanup
  rule governs, so the test must reach each one (construct or reuse fixtures that
  force each; the `fast_rect_clip_equivalent_to_boolean` fixtures already cover
  the straddle and split tiers). Run N=1000 iterations into a reused `out`
  (recycled via `scratch.recycle_shapes` between iterations) and assert the pool
  length is stable across the last two iterations (no unbounded growth).

### Brick 2 - output-neutrality gate

Commit Brick 1 first, then measure (never benchmark uncommitted code).

The design claim is byte-identical output (see the identity subsection); the
BLOCKING correctness gate is `brokkr check`, and byte-identity is confirmed by a
banked-archive compare. `brokkr regress --dataset denmark` is currently NOT a
reliable gate and must not block this landing - see below.

**Blocking correctness gate (this is the one that must pass to land):**

```
brokkr check
```

`brokkr check` runs the differential oracle (the in-tree boolean engine gated
against i_overlay) plus `fast_rect_clip_equivalent_to_boolean`,
`convexity_early_out_vs_normalize_equivalence`,
`cut_identity_dp_tol_0_geometry_equivalence_reference`, the seam-window XOR
tests, and the new pool-neutrality unit test. Because this spec preserves
`intersect_shapes_with_rect`'s signature and semantics and changes only
allocation provenance, these tests ARE the authoritative per-refactor geometry
gate - the transform is geometry-neutral, so an equivalence break shows up here.

**Byte-identity confirmation (the design claim, gated concretely):**

The claim is byte-identical output, but `brokkr regress`'s tol-0 zero-diff only
establishes SEMANTIC equality - regress canonicalizes intra-layer feature order
(and merged-component order) before diffing, so a zero-diff regress does not by
itself prove byte-identity. To gate byte-identity directly, bank a pre-change
archive and byte-compare:

1. BEFORE landing Brick 1 (on the pre-change commit), produce a locations
   archive with a plain (non-bench) run and copy it aside into the project
   (e.g. `data/e3-pre.pmtiles`):

   ```
   brokkr tilegen --dataset denmark --variant locations
   ```

   Note the resolved OUTPUT path this run reports and copy that file to
   `data/e3-pre.pmtiles`. (Do NOT use a `--bench` run for this: `--bench` writes
   a throwaway self-output under `data/tilegen_tmp/` and stores no stable artifact.)
2. AFTER committing Brick 1, produce the post-change locations archive the same
   way and byte-compare the two:

   ```
   brokkr tilegen --dataset denmark --variant locations
   brokkr compare-tiles data/e3-pre.pmtiles <post-change-output-path>
   ```

   Pass = zero differing tiles. This is the real byte-identity verdict the
   design claims. (Same-commit builds are byte-identical by construction per the
   total within-run record order, so pre vs post isolates exactly this change.)

**Confirmatory-only, currently STALE - do NOT block on it:**

```
brokkr regress --dataset denmark   # confirmatory, currently unreliable
```

The blessed denmark baseline (`blessed/denmark-a702427.pmtiles`, brokkr.toml) is
STALE: it predates two output-changing landings that were never re-blessed -
`88f40af` (boundaries single-producer emission) and `31b8298` (ocean
Visvalingam simplifier). Against that baseline, `brokkr regress --dataset
denmark` reports large boundary and ocean structural diffs even on a
bit-identical change, so it CANNOT serve as this landing's identity gate.
Re-blessing is user-gated and has not happened. Run this only after the baseline
is re-blessed (user say-so), as a confirmatory cross-check; until then the
blocking gates are `brokkr check` plus the banked-archive `compare-tiles` above.

Geometry/MVT correctness oracle (this change touches the clip that feeds every
polygon layer's emission) - run on the FRESH post-change locations archive
produced above, at its resolved output path (`<post-change-output-path>` from the
plain non-bench run; the earcut oracle needs a real emitted archive, not the
`--bench` throwaway):

```
cd scripts/validate
pnpm install
node earcut-oracle.mjs <post-change-output-path>
```

Pass = 0 deviant polygons, 0 misattached holes, every polygon layer, every zoom.

Container integrity - `brokkr verify pmtiles --dataset denmark` is UNRUNNABLE as
a dataset gate (brokkr's verify resolves only brokkr.toml-pinned pmtiles entries
and this project pins none, only a blessed archive - documented in
`reference/performance.md`). Run the elivagar verifier directly on the resolved
archive instead:

```
elivagar verify <post-change-output-path>
```

Zero errors.

### Brick 3 - performance verdict

Commit first, then bench (best-of-3, clean tree).

```
brokkr tilegen --bench --dataset denmark --variant locations
brokkr results <uuid>
brokkr sidecar <uuid> --human
```

and the alloc re-read to confirm the churn actually left:

```
brokkr tilegen --alloc --dataset denmark --variant locations
brokkr results <uuid>
```

Expected: `clip_shape_rect_fast` + `clip_ring_half_plane_multi` exclusive alloc
drops toward zero (their only remaining allocations should be pool growth on
cold buffers, amortized to ~0 across the run).

Numeric keep/revert rule (the vague "measured mass minus cold pool warm-up"
prose is replaced with thresholds; pooling's characteristic risk is that it
trades churn for RETAINED per-worker memory, so RSS gets an explicit bound too):

- **Alloc win (required):** the two frames' combined exclusive alloc drops by at
  least **80%** of the Brick 0 measured mass. (Rationale: the pooled design leaves
  only cold-buffer growth, which is a small fixed count per worker amortized to
  near-zero over a denmark run; anything less than an 80% drop means a cleanup
  path is still allocating and the leak must be found before landing.)
- **RSS bound (required):** peak RSS from `brokkr sidecar <uuid> --human` on the
  post-change bench must not regress by more than **2%** versus the pre-change
  peak RSS on the same host. Pooling retains warm buffers per worker; a
  retained-memory blowup beyond this bound is a revert even if the alloc win
  lands. (2% is the documented RSS reading noise on denmark-locations per
  `reference/performance.md`; the pre-change peak RSS baseline comes from the
  performance.md denmark-locations scoreboard, or from a pre-change `brokkr
  tilegen --bench --dataset denmark --variant locations` sidecar read if the
  scoreboard number is not current for this host - the alloc run cannot supply
  it, as mimalloc is not the allocator under `--alloc`.)
- **Wall (required):** within `reference/performance.md` noise bounds (no
  regression beyond the documented bench noise band). This is a churn/RSS play,
  not primarily a wall play - wall is expected neutral-or-better.

Revert if any of: alloc drop below 80%, peak-RSS regression beyond 2%, or wall
regression beyond noise. An alloc win with an RSS or wall regression outside
bounds is still a revert - the point is de-churn without retained-memory or
throughput cost.

Record post-change numbers (commit hash, host, denmark-locations bench-3 wall,
peak RSS from sidecar, the two frames' alloc GB before/after) into
`reference/performance.md`, hash-anchored, matching the Landing 2 entry's style.

## Stopping rule / out of scope

- Only `clip_shape_rect_fast`, `clip_ring_half_plane_multi`, their call site in
  `intersect_shapes_with_rect`, and the minimal pool exposure they need are
  touched. `intersect_shapes_with_rect`'s signature, the identity tier, and the
  boolean tier (`intersect_rect_into`) are unchanged.
- E2 (flat point+range output at the module boundary) is a SEPARATE item and is
  NOT done here - the clip helpers keep producing nested `Vec<Contour>` shells;
  the range-based boundary rewrite is E2/E5's job. This spec pools the nested
  representation; it does not flatten it. Named and excluded per the contract
  (not deferral - a distinct TODO).
- E4 (rect-specialized boolean, iterator-fed segments) and E6 (solver
  thresholds) are untouched.
- The differential fallback to the exact boolean is preserved exactly; this spec
  does not widen or narrow the fast path's coverage. Any coverage change would be
  E4(c) re-bless territory, explicitly out of scope.
- No env-var scaffolding, no benchmark knob, no routing switch. The rewrite is
  the way forward; the old fresh-allocation body is deleted, not gated behind a
  flag.

## Standing references

- Contract: `reference/technical-implementation-spec.md`.
- Source item: `notes/planet-30gb-roadmap.md`, E3 (H6 post-port engine section).
- Failure ledger cleared: `notes/rendering-postmortem.md` (the R-ledger; no
  logged approach re-proposed; geometry unchanged).
- Measurement record: `reference/performance.md` + `.brokkr/results.db`. Baseline
  is HEAD at Brick 0 (record the commit hash and the two-frame alloc mass there);
  post-change numbers recorded at Brick 3 the same way. Bench dataset/variant and
  noise-reading rules follow `reference/performance.md`; the blessed regress
  reference and scoreboard rows are denmark-locations runs.
