# Boundary-line fix: implementation specification

Written against `reference/technical-implementation-spec.md` (the contract).
Source / problem writeup: `notes/norway-boundary-spike-investigation.md` (the
atomic claim ledger this spec implements; cluster ids RN/ATN/CN/DPN/EN below
refer to that document). Failure-history reconciliation:
`notes/rendering-postmortem.md` records only polygon/ocean tile-ring work (its one
`simplify` hit is `simplify_tile_ring`, unrelated); no boundary-line merge,
closed-ring simplification, or Visvalingam approach is a logged R/S failure, so
nothing here re-proposes rejected work.

Measurement note: boundary lines are a tiny fraction of features and none of this
touches node-store or sort. But two of the shared primitives DO run at
assemble time on every line feature, so "off the hot path" is only partly true:
- L2 (`merge_connected_lines` / `merge_line_segments`) runs per line feature in
  assembly, adding a per-segment dedup hash/canonicalize.
- L3 (`simplify_into*`) is the production line simplifier invoked from
  `emit_line_feature` for every line layer, and grows the closed-input path.
- L4 adds a relation prescan pass.
No `reference/performance.md` update is owed if all three stay within noise, but
each owes a neutrality reading, not just L4. Gate:
`brokkr tilegen --bench --dataset norway --variant locations` within noise of the
prior landing's commit-anchored baseline (record `elapsed_ms` at the preceding
commit hash before each measured landing; sidecar `assemble_reader_ns` /
`assemble_ms` is the finer signal if wall moves). (R1-7: the earlier "off the
measured hot path / only L4" claim was overstated - L2/L3 touch assemble-time
line code and are measured here too.)

---

## Problem, in one paragraph

The `boundaries` layer renders out-and-back palindrome spikes at low zoom on
Norway (and draws every shared border two-to-three times). Two root defects
(ledger clusters R and AT): admin boundary lines are emitted once per relation
member way, stamped with parent-relation attributes, with no dedup and no
stitching, so shared borders arrive at assembly as byte-identical parallel edges.
The assembly-time line merger (cluster C) then walks a duplicate pair out to a
degree-2 source vertex and appends the duplicate reversed, fabricating the spike.
A third, latent defect (cluster DP): Douglas-Peucker on a closed input has a
zero-length baseline and can collapse a thin ring into the same hair shape.

The competitor survey (investigation doc section 6) settled the target: both real
Shortbread implementers (tilemaker shipping; planetiler core) emit boundary lines
**once per way**, resolving relation attributes down onto the way. That is the
architecture this spec builds.

---

## Survey of current ground

### Producers of boundary line features (two today, must become one)

1. **Way-level** - `pipeline/phase12.rs::process_planned_way_into`. A way whose
   OWN tags match `match_boundaries_line` (`boundary=administrative` +
   `admin_level` in {2,4}) is emitted once via `emit_line_feature`, with a real
   `preserve_vertex_mask` (shared-node pins), `osm_id = way_id`. This producer is
   correct in shape but **incomplete**: under modern OSM tagging the admin tags
   live on the relation, not the member ways, so most member ways carry no
   `admin_level` and are invisible to it.

2. **Relation-member-level** - `pipeline/relations.rs::process_prepared_relation_into`,
   `GeomExpect::Line` branch. Iterates `rel.member_ways`, emits each with
   `osm_id = rel.osm_id`, parent-relation attributes, an EMPTY preserve mask, and
   no dedup (ledger R1-R5). Gated behind `multi.polygons.is_empty()` early-return
   and `rel.is_boundary` (ledger R6/R7), and emits every raw member once one
   polygon assembles, including non-ring members (ledger R8). This is the producer
   that creates duplicates (shared way is a member of both countries) and
   mis-attributes (parent tags on every member, ledger AT1-AT3, AT14-AT16).

### Structures (what changes)

- `multipolygon.rs`: `pub struct MemberWay { pub role: WayRole, pub coords:
  Vec<Point> }` - 32 bytes, `const _` size-asserted. It discards member way id and
  member tags (ledger AT2). `WayRole { Outer, Inner, Other }`.
- `relations.rs`: `PreparedRelation { osm_id, matches, member_ways: Vec<MemberWay>,
  is_boundary }`. `prepare_relation` already walks `rel.members()` and sees
  `MemberId::Way(way_id)` per member (the way ids ARE available here). NOTE the
  memory accounting: `estimate_prepared_rel_bytes` hardcodes `32 + coords*16` per
  MemberWay (its size). Any struct-size change to `MemberWay` must update this
  constant too or the relation in-flight budget undercounts (R1-3 / R2-nit3).
- `phase12.rs`: `process_planned_way_into(way, plan: &WayPlan, ...)`. `plan.way_id`
  and `plan.is_member` exist. `PinSource::{Injected, BlockLocal}`. Pins come from
  `way.shared_node_pins()` (injected) or `plan.preserve_node_refs` (block-local).
- `boundaries.rs`: `boundary_match` returns `(admin_level: i64, min_zoom: u8)` for
  {2 -> (2,0), 4 -> (4,7)}; `match_boundaries_line` sets `maritime = maritime=yes
  OR natural=coastline`, `disputed = disputed=yes`, from whatever tags it is given.
- `mvt/merge.rs`: `merge_line_segments` builds an endpoint graph; `build_chain`
  walks degree-2 nodes and appends a continuation reversed
  (`seg.iter().rev().skip(1)`) - the palindrome step (ledger C1-C9). No parallel-edge
  dedup anywhere.
- `geometry/simplify.rs`: `simplify_into` / `simplify_into_with_required` seed
  `keep[0]`, `keep[len-1]`, split on baseline `p[0]->p[len-1]`; `perp_dist_sq` has
  the `len_sq < 1e-30` degenerate branch (ledger DP1-DP7). No ring awareness.

### Phase ordering (the obstacle)

`phase12` processes ways; relations are a streamed **tail** afterward
(`process_relation_blocks`, which the code notes can consume "a filtered PBF
re-read" of relation blocks). So relation-derived admin metadata is not available
during way emission today. Getting it there is the one obstacle this spec resolves
(below).

**Survey correction (R1-4 / R2 load-bearing): a relation prepass already exists.**
`phase12` already spawns `prepass_relation_plan` on a background thread at phase
start and joins it at the first way block - BUT only when the PBF lacks injected
WayMembers (`if injected.members { None } else { Some(spawn(...)) }`). It does a
filtered relation-only PBF read and builds `RelationPlan` (a `needed_ways`
FxHashSet of all mp/boundary relation members). Consequences for this spec's
prescan:
- On **raw/indexed** variants that background prepass runs; the boundary metadata
  should be accumulated INSIDE it (one extra FxHashMap folded during the scan it
  already does), not as a second full relation read.
- On the **locations** variants - which every gate here uses, and which the
  blessed denmark baseline uses - `injected.members` is true, so NO prepass runs
  today; the boundary prescan is a genuinely new relation read. Its scheduling is
  load-bearing: `reference/performance.md` records germany's prepass-join stall at
  ~3.36s, and 2% of norway's ~160s wall is ~3.2s, so a *serial* prescan could by
  itself consume L4's entire perf-neutrality budget. It must run concurrently
  (same spawn-at-start / join-before-first-way pattern the existing prepass uses),
  not as a blocking "before phase12" step.

### Gate/bless implications (from `notes/ocean-tile-stream-spec.md` + AGENTS.md)

The blessed `brokkr regress` baseline is denmark (locations variant). Denmark has
admin boundaries, so L4 WILL diff the denmark `boundaries` layer (fewer features,
geometry moves) - this is an expected, non-neutral landing; the verdict is read as
"diffs confined to the boundaries layer, direction = fewer features, no new missing
non-boundary features." Re-blessing is a user decision after the human MapLibre
gate, not part of this spec. Norway is the correct dataset for the primary gates
(it has the relation/boundary load denmark lacks, per the spec contract's dataset
rule).

---

## Obstacle resolved inline: how relation metadata reaches way emission

**Decision: an in-elivagar boundary-relation prescan**, run concurrently with the
way phase (spawn at phase12 start, join before the first way block - the exact
pattern `prepass_relation_plan` already uses; see the survey correction above),
producing a `way_id -> BoundaryMeta` map that `process_planned_way_into` consults.
On raw/indexed inputs fold this accumulation into the existing prepass rather than
adding a second scan; on locations inputs it is a new concurrent scan.

Rejected alternative: injecting the metadata in pbfhogg (as shared-node pins are
injected). It is idiomatic but spans two repos and enlarges the blast radius past
this item's stopping rule; the in-tree prescan reuses pbfhogg's existing filtered
relation re-read and keeps the change in one crate. Named and excluded, not
deferred.

The prescan reads relation blocks (same filtered re-read the relation tail already
uses). It must model **admin** and **disputed** parentage independently, because
canonical Shortbread does (`research/shortbread-tilemaker/process.lua`
`process_boundary_lines`): a way's `disputed` flag comes from a SEPARATE
`boundary=disputed` parent relation, which does NOT carry `admin_level`. A single
`min_admin_level` field cannot represent a disputed-only parent, and an
administrative-only prescan filter would make the `disputed` branch unreachable
(R1-2 / R2 prescan-a). So the filter admits `boundary=administrative` (with
`admin_level` in {2,4}) OR `boundary=disputed`, and the metadata splits:

```rust
// new: pipeline/boundary_prescan.rs
pub struct BoundaryMeta {
    pub min_admin_level: Option<u8>, // 2 or 4; min across admin parents, None if
                                     // the way is only a disputed-relation member
    pub disputed: bool,              // OR of parent boundary=disputed relations
}
pub type BoundaryWayMeta = rustc_hash::FxHashMap<i64, BoundaryMeta>;

pub fn prescan_boundary_relations(
    relation_blocks: impl Iterator<Item = pbfhogg::PrimitiveBlock>,
) -> BoundaryWayMeta;
```

Aggregation (matches tilemaker): for a `boundary=administrative` parent,
`min_admin_level = Some(min(existing, this))`; for a `boundary=disputed` parent,
`disputed = true`. `boundary_match` returns `admin_level` as `i64`; convert to
`u8` at the single fold site (values are 2/4, so the cast is total; R2 prescan-c).

**Maritime is NOT a prescan field.** Shortbread derives `maritime` purely from the
WAY's own tags (`maritime=yes` OR `natural=coastline`), never from parent
relations (`process.lua` uses `Find(...)`, the way-local lookup). So `maritime` is
resolved at emission from the way's own tags only, matching
`match_boundaries_line` today; the earlier BoundaryMeta.maritime (OR of parent
maritime) was an undeclared divergence from canonical Shortbread and is dropped
(R1-2 maritime / R2 prescan). `disputed` is OR-merged at emission with the way's
own `disputed=yes` tag.

**Filter delta to flag (R2 prescan-b):** `prepare_relation` today pre-filters on
`type` in {multipolygon, boundary} before matching; this prescan filters on
`boundary=administrative|disputed` WITHOUT that `type` gate, so an admin/disputed
relation of some other `type` newly contributes member metadata. That is the
desired behavior (tilemaker keys on the `boundary` tag, not `type`), but it is a
deliberate behavioral delta from the relation-tail path - recorded here so it is
not mistaken for an accident.

---

## Target architecture (concrete)

After all landings, boundary lines have exactly one producer - the way phase -
and the merger/simplifier are hardened against the fabricated-spike class:

1. `MemberWay` is UNCHANGED. (The earlier plan added `way_id: i64` to it; that
   field had no consumer - the prescan reads `MemberId::Way(way_id)` straight from
   the relation blocks, and multipolygon assembly explicitly ignores it - so it was
   pure dead weight that also silently desynced `estimate_prepared_rel_bytes`.
   Removed; R1-3.)
2. `process_prepared_relation_into` loses its `GeomExpect::Line` branch entirely.
   Relations no longer emit boundary lines. (Polygon/label branches unchanged, so
   `boundary_labels` - a `PolygonPointOnSurface` feature off the assembled rings -
   is untouched.)
3. A `BoundaryWayMeta` prescan runs CONCURRENTLY with phase12 (spawn-at-start /
   join-before-first-way, folded into the existing prepass on raw/indexed);
   `process_planned_way_into` resolves boundary matches from the way's own tags OR,
   if absent, from `BoundaryWayMeta[way_id]`, emitting once with resolved
   `(admin_level, maritime, disputed)` and the way's real preserve mask. `maritime`
   is always the way's own tags; `admin_level` and `disputed` come from the meta
   (OR-merged with the way's own `disputed=yes`).
4. `merge_line_segments` dedups coordinate-equal (forward and reversed) segments
   per feature before graph construction; `build_chain` refuses a continuation
   identical (fwd/rev) to the current segment. (planetiler `LoopLineMerger`
   pattern.)
5. `simplify_into` / `simplify_into_with_required` are ring-aware: closed input is
   split on a real farthest-pair chord, not on the degenerate `p[0]->p[0]`
   baseline, with a 3-distinct-vertex floor (else drop).

Maritime policy (resolved inline): **keep** maritime as an attribute, resolved once
per way from the WAY's own tags (`maritime=yes` OR `natural=coastline`), never from
parents. Do NOT filter maritime boundaries out - that is OpenMapTiles-specific;
canonical Shortbread (our `tests/fixtures/shortbread.spec.yml`, and
shortbread-tilemaker) keeps it.

---

## Landings (ordered; `brokkr check` + `elivagar verify` green at every boundary)

Instruments first, then hardening, then the rearchitecture. L1 builds the gate
L2/L4 are read against (spec contract rule 5).

On `elivagar verify` (R2-nit4): the header names it as a per-landing gate but no
landing's command list runs it. `brokkr regress` full-decodes every tile
(container, decompression, MVT structure, geometry commands) and thus subsumes
what `verify` checks; the landings rely on regress for that coverage. Where a
landing wants the explicit standalone check it is spelled out in its gate block;
otherwise regress is the verify-equivalent. (Kept the header wording but this is
what "verify green" is delivered by.)

### L1 - Boundary-line oracle (instrument, zero behavior change)

New `scripts/validate/boundary-line-oracle.mjs`, modeled on `earcut-oracle.mjs`.
For the `boundaries` layer of an archive, decode every line feature into its
MoveTo-delimited sub-lines and flag, per zoom, with offender tiles:

- **palindrome**: a sub-line whose vertex sequence equals its own reverse
  (`v[i] == v[n-1-i]` for all i) - the exact p89 shape.
- **spur**: a sub-line containing an index j where `v[j-k] == v[j+k]` for k>=1
  (a retrace apex not at the sub-line ends).
- **intra-feature duplicate** (exact and reversed): two sub-lines WITHIN one
  feature with identical (or reversed-identical) coordinate lists.
- **cross-feature duplicate** (exact and reversed): two sub-lines in DIFFERENT
  features of the boundaries layer, same tile, with identical/reversed coords.
  This category is load-bearing (R1-5 / R2): assembly's `merge_same_attr_geometries`
  merges only byte-identical-attribute features, so the maritime=true/false and
  differing-admin_level double-draw (ledger AT16) lives as two SEPARATE features
  and is invisible to any within-feature check. It is the double-draw that half of
  L4 exists to remove, and nothing but this check (and the human MapLibre gate)
  verifies it.

The categories must be **separately selectable** (a `--only palindrome,spur,...`
flag or per-category exit codes), because L2 legitimately clears the spike/spur/
intra-feature classes while leaving cross-feature duplicates for L4 - a single
"any offender -> nonzero" exit cannot express that staged acceptance.

Output: per-zoom, per-category counts + up to 15 offender `z/x/y feat` lines.
Exit non-zero if any SELECTED category has an offender (default selection: all).

Gate (this landing just has to run and report on the current archive, showing the
known offenders): from repo root,
```
node scripts/validate/boundary-line-oracle.mjs data/tilegen/norway-20c8bd7.pmtiles
```
Acceptance: it flags the z4/8/3 and z4/8/4 palindromes and the p87==p88 duplicates
(proves the instrument detects the known defect). `brokkr check` IS run this
landing (the DP9 `dump-boundary-segments` subcommand below is Rust); the oracle
itself is JS and needs no Rust build.

DP9 note - the pre-merge closed-segment probe (pinned to ONE design; R1-8 / R2
DP9-bug). The earlier "read the sort-chunk stream in JS, OR a Rust dump" was an
unresolved either/or: the two are materially different implementations, the JS
option means reimplementing the sort wire format, the Rust option contradicts
this landing's "no Rust changed", and neither `src/debug.rs` nor anything else
has "existing debug plumbing" that dumps pre-merge segments. Resolved: it is a
**new Rust debug subcommand**, `elivagar dump-boundary-segments <FILE> -z -x -y`,
that runs the assemble decode for one tile up to (but not through)
`merge_connected_lines`, and prints per boundary-layer sub-line: vertex count and
whether `first == last` (closed). It is specified to brick standard like any
other landing:
- signature: `dump-boundary-segments <pmtiles-or-tmp> -z Z -x X -y Y`, output one
  `feat sub-line closed=bool nverts=N` line per sub-line to stdout;
- it reuses the existing `svg`/`diag` single-tile decode path (those already
  decode MVT geometry per tile), so no wire-format reimplementation;
- one unit test on a synthetic tile with a known-closed and known-open sub-line.

Because this adds Rust, L1 is NO LONGER "no Rust changed": it runs `brokkr check`
for the new subcommand and its test. If the probe reports zero closed pre-merge
boundary segments, cluster C (merger) is the exclusive spike cause and DP9 flips
to proven, so L3 is latent-hardening (keep verdict rests on unit tests + oracle
neutrality); if any are closed, L5's precondition fires and L3/L5 must also clear
the norway boundary oracle.

### L2 - Merger parallel-edge guard (cluster C)

In `mvt/merge.rs`:
- Add `dedup_parallel_segments(segments: &mut Vec<Vec<(i32,i32)>>)` called at the
  top of `merge_line_segments` (after the `< 2` early return): drop any segment
  whose coordinate list equals an earlier segment's forward or reversed. Canonical
  key: the lexicographically-smaller of `coords` and `coords.reversed()`.
- In `build_chain`, before accepting the degree-2 continuation `next`, if
  `segments[next.seg_idx]` equals the current segment forward or reversed, `break`
  instead. This is belt-and-suspenders and becomes dead once the upfront
  `dedup_parallel_segments` lands (the duplicate is gone before graph build) -
  harmless, kept as a guard against future pass-2 regressions (R2-nit5); say so in
  the commit.

Scope note (R1-6 / R2): `merge_connected_lines` (the `merge_line_segments`
caller) runs on EVERY line feature in EVERY layer, not just boundaries. The dedup
therefore also removes real OSM duplicate ways (identical coords + identical
attrs, a common data defect) in streets, water_lines, piers, etc. Those are
legitimate, correct removals - so L2's output-regression cannot demand
"boundaries only" (see the gate below).

Concrete unit tests (behavior no oracle reaches), added to `src/mvt/tests.rs`
(NOT "mvt/merge.rs tests" - merge.rs only exposes `pub(super)` helpers consumed by
tests.rs; R2-nit2):
- two identical `A->...->B` segments, A a junction (degree>2 via a third real
  segment at A), B degree-2: assert output contains no sub-line equal to its own
  reverse.
- the same with both endpoints degree-2 (pass-2 route).
- reversed-duplicate pair: assert single output, not a palindrome.
- a legitimate coincident-but-distinct case (two different lines sharing only
  endpoints) still merges normally.

Gates:
```
brokkr check
```
(clippy + full suite incl. the new merge tests; boundary lines touch encoding
semantics.) Then, on a fresh norway build:
```
brokkr tilegen --dataset norway --variant locations
node scripts/validate/boundary-line-oracle.mjs data/tilegen/norway-<hash>.pmtiles
```
(substitute the archive `brokkr tilegen` just wrote - it prints the path; the
`<hash>` is the short commit hash of the build.)
Acceptance: oracle reports 0 palindromes, 0 spurs, 0 intra-feature duplicates
(exact AND reversed) in `boundaries`. CROSS-feature duplicates may remain until L4
(they are the different-attribute double-draw L2 does not touch); select the
categories accordingly (`--only palindrome,spur,intra-duplicate`). State in the
commit that L2 kills the *spike* and intra-feature duplicates, not the
cross-feature double-draw. (R1-5: the earlier "exact duplicates may remain"
wording was contradictory - an all-category exit would have failed this gate.)
Output-regression - build denmark FIRST, then regress (R2 gate-seq bug: regress
compares the CURRENT output against the blessed archive, so without a fresh
locations-variant denmark build it compares a stale/absent archive):
```
brokkr tilegen --dataset denmark --variant locations
brokkr regress --dataset denmark
```
Acceptance: matched-feature geometry changes / fewer merged sub-lines in
`boundaries`, PLUS possible duplicate-removal-shaped line diffs in any line layer
(streets, water_lines, ...) where real OSM duplicate ways existed - those are
correct (R1-6 / R2). What must NOT appear: any polygon-layer structural diff, or
any line diff that is not duplicate-removal-shaped. Keep/revert: kept if the
oracle's selected categories are clean and regress shows only
duplicate-removal-shaped line diffs; reverted on any other structural diff.
Performance neutrality (L2 hashes/canonicalizes every line segment at assemble
time; R1-7): record the pre-L2 norway `elapsed_ms` at the L1 commit hash, then
```
brokkr tilegen --bench --dataset norway --variant locations
```
Acceptance: within noise (accepted bound: under 2% wall); investigate over that.

### L3 - Ring-aware closed-line simplification (cluster DP)

In `geometry/simplify.rs`, make both `simplify_into` and
`simplify_into_with_required` ring-aware. Algorithm for closed input
(`points[0] == points[len-1]`, `len > 3`):
- find `a` = index of the vertex farthest from `points[0]`;
- find `b` = index of the vertex farthest from `points[a]`. Call these the
  **farthest-pair** anchors, NOT "the diameter" (R1-1): the two-sweep
  farthest-point method does not guarantee the true diameter for an arbitrary
  point set - it only guarantees a long real chord, which is all we need to avoid
  the degenerate `p[0]->p[0]` baseline.
- Concrete circular traversal (the earlier "run the existing recursion on `a..b`
  and `b..a` wrapping through 0" is not implementable as written - `dp_recurse`
  takes LINEAR `start..end` ranges only; R1-1): rotate the ring so `a` is index 0
  into a scratch buffer `rot` of length `len-1` (drop the duplicated closing
  vertex, then re-close), locate `b`'s rotated index `b'`, and run the existing
  linear recursion on `rot[0..b']` and `rot[b'..len-1]`. Map surviving rotated
  indices back to force the corresponding `keep[]` bits. No new recursion variant.
- Required-index (pin) machinery is preserved: `a`/`b` are added to the keep set
  exactly as required indices already are, on top of caller pins.
- **Closed-ring floor of 3 distinct vertices, else drop** (R1-1 / R2): forcing
  `keep[a]`/`keep[b]` plus `keep[0]` means a fully-collapsed closed input still
  yields `{p0, pa, pb}`. If those are not 3 distinct points (e.g. both arcs
  collapse and `pa==pb`), the result is a 2-point-or-fewer hair `[p0, pa, p0]` -
  DROP it. The old "fewer than 2 distinct vertices -> drop" rule is UNREACHABLE
  under this algorithm (you always retain >=3 forced indices); the real degenerate
  to guard is exactly this 3-forced-but-<3-distinct case. Assert the output is
  never its own reverse.

Output-volume side effect to expect (R2): forcing 3 distinct vertices means thin
closed lines that TODAY collapse below 2 points and vanish at MVT encode will now
persist as thin triangles at every zoom. `skip_bbox_check` is true for streets and
boundaries (`emit.rs`), so there is no sub-pixel cull to hide them. This is a real
output delta - visible in the L3 regress as added/retained closed-line features -
and is accepted as the correct behavior (a valid ring should not silently vanish),
but it must be called out in the commit and not mistaken for a bug.

Considered alternative: Visvalingam-Whyatt (tippecanoe + tilemaker use it, immune
by construction). Rejected for this landing because it is a second simplifier to
maintain and would need its own required-index (pin) preservation path; the
farthest-pair-anchored DP above reuses the existing pin machinery and is the
smaller correct change. Recorded as the fallback (L5) if it proves insufficient.

Unit tests: a thin closed ring (a long narrow loop) simplifies to a valid loop or
drops - assert the result is never a palindrome and never a 3-point
`[start,apex,start]` with `start==apex`-collinear collapse. A fat closed ring keeps
its shape (retains > 3 vertices). An open line is unchanged by the new path.

Gates:
```
brokkr check
```
Then, because this touches geometry/simplification, the tessellation gate on a
fresh build (denmark suffices - the change is not boundary-specific). Run each
command on its own line from the repo root (the earlier one-liner chained with
`;` and included a pointless `pnpm --version` pipe - both forbidden by the repo
bash rules, so it was not copy-pasteable; R2-nit1):
```
brokkr tilegen --dataset denmark --variant locations
```
then from `scripts/validate` (that is the pnpm workspace):
```
node earcut-oracle.mjs ../../data/tilegen/denmark-<hash>.pmtiles ocean 0.01 --unique
```
Acceptance: earcut oracle still 0 over-threshold / 0 misattached (no polygon
regression). Then output-regression (denmark was just built above, so it is
current):
```
brokkr regress --dataset denmark
```
Acceptance (R2 - closed lines are NOT boundaries-only): diffs confined to
closed-line GEOMETRY in any line layer - closed ways are emitted as lines by
streets (roundabouts), water_lines, piers, AND boundaries, so legitimate
streets/water_lines closed-line diffs are expected and correct; plus the retained
thin-triangle additions noted above. What must NOT appear: any polygon-layer
structural diff, or any OPEN-line geometry change (the new path only touches closed
input). Keep/revert on those.
Performance neutrality (L3 grows the production line simplifier's closed path;
R1-7): record pre-L3 norway `elapsed_ms` at the L2 commit hash, then
`brokkr tilegen --bench --dataset norway --variant locations`; within noise
(under 2% wall) or investigate.

(If L1's `dump-boundary-segments` probe showed no closed source boundary lines, L3
is latent-hardening and its keep verdict rests on the unit tests + oracle
neutrality alone; if it showed some, L3's norway boundary-oracle run must also stay
clean.)

### L4 - Single-producer per-way boundary emission (clusters R + AT)

The structural landing. Coordinated edits, one commit:

1. `multipolygon.rs`: `MemberWay` is UNCHANGED (no `way_id` field - see target
   architecture item 1; it had no consumer and would have desynced
   `estimate_prepared_rel_bytes`). Nothing in this file changes.
2. `relations.rs`: `process_prepared_relation_into`: DELETE the entire
   `GeomExpect::Line` arm (the block that iterates `rel.member_ways` and calls
   `emit_line_feature`). Relations stop producing boundary lines. Remove the
   now-dead `is_boundary` field: survey CONFIRMS it is read only by that arm
   (`relations.rs` sets it in `prepare_relation` and reads it once, in the deleted
   arm), so drop it from `PreparedRelation` and stop computing it in
   `prepare_relation`. (`prepare_relation` itself is otherwise unchanged - it still
   fills `member_ways` for polygon/label assembly.)
3. New `pipeline/boundary_prescan.rs` (types above). Wire `prescan_boundary_relations`
   in CONCURRENTLY - spawn at phase12 start, join before the first way block, the
   same pattern as `prepass_relation_plan`; on raw/indexed inputs fold the boundary
   accumulation into that existing prepass instead of adding a second scan (see the
   Obstacle section). Thread `&BoundaryWayMeta` into `process_planned_way_into`.
4. `phase12.rs::process_planned_way_into`: when the way's own tags do NOT match the
   boundaries layer but `BoundaryWayMeta[plan.way_id]` has `min_admin_level =
   Some(level)`, synthesize the boundaries `LayerMatch` from the meta (`admin_level
   = level`, `min_zoom` per the 2->0 / 4->7 rule, `maritime` from the WAY's own
   tags, `disputed` = meta.disputed OR the way's own `disputed=yes`). When the
   way's own tags DO match, OR-merge the meta's `disputed` and take
   `min(admin_level, meta.level)`. A disputed-only meta entry (`min_admin_level =
   None`) contributes `disputed` to a way that gets its `admin_level` elsewhere but
   never synthesizes a match on its own. Emit once, with the way's real
   `preserve_vertex_mask`. This is the only boundary-line emission site left.
   The `min_zoom` mapping (2->0, 4->7) now lives in two places (`boundaries.rs`
   `boundary_match` and this synthesis site) - route BOTH through one shared helper
   so they cannot drift (R2 smell).

Cleanup in the same commit (R2 smell): `match_multipolygon` (`shortbread/mod.rs`)
still calls `boundaries::match_boundaries_line`; after the relation Line arm is
deleted, a `GeomExpect::Line` match from a multipolygon relation falls through to
`_ => {}` and is dead weight - remove that call from `match_multipolygon` (leave
`match_closed_way`'s call, which is a real way-level producer). Note that
`needed_ways`/`is_member` coverage of admin-relation members is NOT lost by this:
`match_boundary_labels` still fires for every admin relation, keeping those
relations in the plan.

This simultaneously removes duplication (R), the assembly-gate coupling (R6/R8, the
relation arm is gone), and the parent-attr mis-application (AT1-AT3, AT14-AT16:
attrs are now resolved per way).

Concrete tests. Test harness note (R2): nothing in the repo constructs synthetic
`pbfhogg::PrimitiveBlock`s, and `prescan_boundary_relations` as typed takes a block
iterator - so factor the aggregation (block-member -> BoundaryMeta fold) and the
match-synthesis (BoundaryMeta + way-own-tags -> LayerMatch) into PURE functions
that the tests call directly, without needing pbfhogg block fixtures. Then:
- a synthetic two-country relation pair sharing one member way: assert the shared
  way's synthesized match is exactly ONE boundaries `LayerMatch`, with
  `admin_level` = the min of the two relations.
- an untagged member way of an admin_level=2 relation: assert it synthesizes a
  match (via the prescan) - guards against the "delete relation emission -> lines
  vanish" failure the reviews flagged.
- a way tagged `natural=coastline` that is a member of an admin relation: assert a
  single match with `maritime=true` (maritime from way-own tags).
- a way that is a member of BOTH an admin (level 2) and a separate
  `boundary=disputed` relation: assert one match, `admin_level=2`, `disputed=true`
  (proves the independent admin/disputed modeling; R1-2).

Gates:
```
brokkr check
```
Fresh norway build + oracle, ALL categories including cross-feature (the
cross-feature double-draw is the class L4 exists to kill; L2 already cleared the
intra-feature classes):
```
brokkr tilegen --dataset norway --variant locations
node scripts/validate/boundary-line-oracle.mjs data/tilegen/norway-<hash>.pmtiles
```
Acceptance: 0 in EVERY category - palindromes, spurs, intra-feature duplicates AND
cross-feature duplicates (exact + reversed) - in `boundaries`, at every zoom. The
cross-feature check is what verifies the double-draw fix; without it only the human
gate covers it (R1-5 / R2).
Output-regression - build denmark FIRST (locations variant), then regress, so the
comparison is against a current denmark output not a stale one (R2 gate-seq bug;
L2 has the same fix):
```
brokkr tilegen --dataset denmark --variant locations
brokkr regress --dataset denmark
```
Acceptance: structural diffs confined to the `boundaries` layer, direction = fewer
features (duplicates gone) and possibly added features (untagged members now
drawn); ZERO diffs in every non-boundary layer. A diff outside `boundaries` fails
the landing.
Performance neutrality (L4 adds a relation prescan pass - CONCURRENT, so the risk
is the join stall not serial time; see the Obstacle section's ~3.2s budget note):
```
brokkr tilegen --bench --dataset norway --variant locations
```
Acceptance: within noise of the pre-L4 baseline (record the pre-change norway
`elapsed_ms` at the L3 commit hash first, per benchmark discipline); accepted cost
bound = under 2% wall. Over that, treat as a regression to investigate before
keeping. Read `brokkr sidecar <uuid> --stalls` for a prepass-join stall if wall
moves.
Human MapLibre gate (the one gate needing eyes): load the norway archive in a
MapLibre viewer and inspect z1 through z5 over mainland Norway, specifically tiles
**z4/8/3, z4/8/4, z3/4/2, z2/2/1**. Correct = one continuous national border with
no hairs/spikes and no doubled/parallel lines; the Norway-Sweden border draws as a
single line. (Exact per-tile SVG spot-check available via
`brokkr svg --dataset norway -z 4 -x 8 -y 3 -l boundaries` etc. for a
headless proxy before the viewer.)

---

## Landing 5 (conditional) - Visvalingam fallback

Only if L1's `dump-boundary-segments` probe proved closed source boundary lines
exist AND L3's farthest-pair-anchored DP does not fully clear the norway boundary
oracle. Because that precondition CAN fire, this landing is specified concretely,
not left as an outline (R1-8):

- New function alongside the DP simplifier in `geometry/simplify.rs`:
  `simplify_vw_into(points: &[Point], tolerance: f64, required: &[usize],
  keep_buf, output) -> f64`, signature-compatible with
  `simplify_into_with_required` so `emit_line_feature` swaps callee by geometry
  kind (closed -> VW, open -> existing DP).
- Metric: effective area of the triangle `(prev, v, next)`; iteratively remove the
  minimum-area interior vertex, recomputing only the two neighbours' areas
  (binary-heap of `(area, idx)`, lazy-deletion on stale heap entries). Stop when
  the smallest remaining area exceeds `tolerance^2` (area threshold in the same
  Mercator units the DP tolerance uses; document the units conversion at the site).
- Closed-ring floor: retain at least 3 distinct vertices (`isClosed` retain-3),
  else drop - identical drop rule to L3, same "never its own reverse" assertion.
- Required-index preservation: a vertex whose index is in `required` (caller pins
  plus the two farthest-pair anchors) is never eligible for removal - seed the heap
  without those indices.
- Tie-breaking: equal areas break on ascending vertex index (deterministic output,
  matching the DP path's determinism contract).
- Tests: mirror L3's closed-ring tests (thin ring drops or stays valid, fat ring
  keeps shape, pins survive) plus one asserting a known VW removal order on a small
  hand-checked ring.
- Gates: L3's gates (brokkr check, denmark earcut, denmark regress with the same
  closed-line acceptance, norway bench) PLUS the norway boundary oracle clean in
  all categories. Keep/revert on those.

Out of scope unless its precondition fires; fully specified here so firing it is a
build step, not a design step.

---

## Stopping rule / out of scope

- Only boundary **line** emission is the TARGET of the rearchitecture. But the two
  shared geometry primitives (merger, simplifier) are genuinely shared: L2's
  segment dedup runs on every line feature in every layer, and L3's closed-line
  simplification runs on every closed line way (streets roundabouts, water_lines,
  piers, ...). So "diffs confined to boundaries" is NOT the L2/L3 acceptance
  (R1-6 / R2) - those landings legitimately change duplicate-removal-shaped line
  geometry and thin-closed-line geometry in any line layer, and their gates say so.
  The confinement-to-boundaries acceptance applies only to L4 (the emission
  rearchitecture) and to the polygon layers throughout (earcut + regress prove no
  polygon regression at every landing).
- `boundary_labels`, `place_labels`, and all polygon layers are out of scope as
  producers; the merger/simplifier changes reach line layers only, and only to
  remove a real defect class (a spike or duplicate is wrong everywhere).
- The ocean artifact, sort, node-store, and pmtiles container are untouched.
- pbfhogg is not modified (injection alternative excluded above).
- Re-blessing the denmark/ norway baselines after L4 is a user decision, not part
  of this spec.

## Standing references

- Contract: `reference/technical-implementation-spec.md`.
- Problem writeup / claim ledger: `notes/norway-boundary-spike-investigation.md`.
- Gate/bless semantics: `notes/ocean-tile-stream-spec.md`, `AGENTS.md`.
- Failure history reconciled: `notes/rendering-postmortem.md` (no conflicting
  logged approach).
- Measurement record: `reference/performance.md` + `.brokkr/results.db`. L2, L3
  and L4 each owe a norway bench reading against the preceding landing's
  commit-anchored baseline (the earlier "only L4" was corrected - L2/L3 touch
  assemble-time line code; see the Measurement note).

## Review reconciliation (R1 codex, R2 fable)

Both reviews were validated against the source; nearly every finding held and is
folded above (tagged inline as R1-N / R2-...). Consolidated map:

- L3 hair survival + non-implementable wrapping arc + "diameter" misnomer +
  retained-thin-triangle output delta + unreachable <2-distinct drop rule
  (R1-1, R2 L3): folded into L3's algorithm (3-distinct floor, concrete rotate
  traversal, farthest-pair naming, output-volume note).
- disputed unreachable under administrative-only filter; admin/disputed must be
  independent; maritime is way-own not parent (R1-2, R2 prescan): folded into the
  Obstacle prescan types (Option<u8> admin + independent disputed, maritime
  dropped from meta) and the L4 tests (admin+disputed way test).
- MemberWay.way_id is dead weight + desyncs estimate_prepared_rel_bytes (R1-3,
  R2-nit3): field removed from the design (target arch item 1, L4 item 1).
- existing `prepass_relation_plan` missed; concurrent scheduling load-bearing on
  locations (R1-4, R2 load-bearing): folded into the Obstacle survey correction
  and L4 item 3.
- oracle within-feature only, misses cross-feature double-draw; L2 exact-duplicate
  wording contradictory (R1-5, R2): folded into L1 (cross-feature category,
  separately selectable) and the L2/L4 acceptance wording.
- L2/L3 affect all line layers; stopping rule + regress verdicts wrong (R1-6, R2):
  folded into L2/L3 acceptance and the stopping rule.
- performance premise overstated; L2/L3 owe benches (R1-7): folded into the
  Measurement note and per-landing bench gates.
- L1 DP9 instrument and L5 unresolved either/or (R1-8, R2 DP9): DP9 pinned to a
  concrete `dump-boundary-segments` subcommand; L5 fully specified.
- gates not copy-pasteable / verify never run / human step vague (R1-9, R2-nit1,
  R2-nit4): archive placeholders concretized, bash-rule-violating chain removed,
  verify-subsumed-by-regress note added, human gate given an SVG proxy.
- L2 gate ran regress denmark with no denmark build (R2 gate-seq): denmark build
  added before regress in L2 and L4.
- prescan drops the type= pre-filter (R2 prescan-b); i64->u8 conversion site
  (R2 prescan-c); L4 test harness needs pure functions (R2); dead
  match_boundaries_line in match_multipolygon + duplicated z-rule helper (R2
  smell); tests belong in src/mvt/tests.rs (R2-nit2); belt-and-suspenders guard
  goes dead after dedup (R2-nit5): all folded at their sites.

Rejected / partially rejected (nothing was fully invalid; these are scoped down):
- R1-9 "regress verdicts lack numerical tolerance / displacement thresholds":
  PARTIALLY rejected. `brokkr regress` already classifies diffs into tolerance vs
  structural and exits nonzero only on structural; the landings key on
  zero-structural, so explicit numeric displacement thresholds are not owed on top
  of regress's own design. The copy-pasteable-command and verify parts of R1-9 WERE
  folded.
- R1-7 "the document contradicts itself (no update owed, then L4 owes one)":
  PARTIALLY rejected as framing. A neutrality reading that stays within noise
  legitimately owes no `performance.md` UPDATE - "owes a bench reading" and "owes a
  doc update" are different obligations, so this is not a contradiction. The valid
  core (L2/L3 also touch assemble-time code and owe readings) WAS folded.
