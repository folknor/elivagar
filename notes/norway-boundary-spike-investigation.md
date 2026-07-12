# Norway Boundary Spike Investigation

Investigated 2026-07-12. Archive under test: `data/tilegen/norway-20c8bd7.pmtiles`
(MVT + gzip, z0-14, world bounds), built from commit `20c8bd7`. All coordinates
below are MVT tile-extent units (4096) unless noted.

This document has three layers:

1. **Narrative** - the diagnosis, corrected for the errors the two verifiers found.
2. **Cross-verification** - what two independent reviewers (codex gpt-5.5 xhigh,
   and a Fable agent) confirmed, corrected, or flagged.
3. **Atomic claim ledger** - every load-bearing factual statement decomposed into
   a single falsifiable atom with a code citation and verdict. The ledger is the
   authority; the narrative is prose over it. The ledger has a `competitor` column
   reserved for the next step (cross-referencing each atom against
   planetiler / tilemaker / tippecanoe / stedsplakat).

Scope note: this started as "the Norway spike" and is now a **fix-elivagar-properly**
program. It is not one bug. At least five distinct defects are recorded below
(merger palindrome, per-way over-emission, assembly-gate inconsistency,
parent-attribute mis-application, latent closed-ring DP), each carrying a cluster
of atoms.

**Resolution (2026-07-12):** every cluster below is fixed - `merge_line_segments`
gained an upfront parallel-edge dedup plus a `build_chain` guard (cluster C),
`simplify_into`/`simplify_into_with_required` became ring-aware with a
farthest-pair split and a 3-distinct-vertex floor (cluster DP), and boundary
lines are now emitted exactly once, in the way phase, from a concurrent
relation-metadata prescan that resolves `admin_level`/`disputed` independently
of `maritime` (clusters R and AT). The claims below are a frozen snapshot of the
pre-fix code, kept for the diagnostic trail; they no longer describe the current
implementation. See git history for the landing commits.

---

## 1. Narrative

### Symptom

On mainland Norway the `boundaries` layer renders thin spikes / hairs at low zoom
(z1 through z5, easiest at z4) - a sliver shooting out of the national border and
back. The initial guess was that the Norway PBF lacked data at low zoom. Wrong:
sparse data makes geometry disappear, never spike, and the low-zoom coastline does
not come from the PBF at all.

### How the layer was isolated

- **Ocean ruled out.** The earcut oracle (`scripts/validate/earcut-oracle.mjs`,
  MapLibre-faithful) over the whole archive, ocean layer, `--unique`: 0 polygons
  over deviation threshold, 0 misattached holes, every zoom 0-14. Global worst
  deviation `1.359e-4` at z11 (was mis-stated as `6.8e-5`, which is only z8), far
  below the `0.01` threshold.
- **Elimination.** At z1-3 the only line/polygon geometry is `ocean` (polygons,
  clean) and `boundaries` (lines, z0-14). `water_polygons` is z4+, `land` z7+, all
  labels are points. So the z1-3 spikes are in `boundaries`.
- **Direct confirmation.** Boundaries-only SVG for z4/8/3 and z4/8/4 shows the
  palindromes verbatim.

### The spike, in the data

Tile z4/8/4, path `p89`:

```
M 2813,76 -> 2809,55 -> 2799,13 -> 2813,-5 -> 2828,-20 -> 2813,-5 -> 2799,13 -> 2809,55 -> 2813,76
```

Out to apex `(2828,-20)`, then every vertex retraced in reverse back to start -
a zero-area out-and-back lollipop, start == end. The same feature appears in z4/8/3
as `M 2828,4076 -> ... -> 2813,4172 -> ... -> 2828,4076`, straddling the y=3/y=4
seam. Rendered with stroke width, the palindrome is the visible spike.

**The apex `E` is a real source vertex, not a clipping artifact.** The line buffer
is `8/256` of the tile = 128 extent units (`geometry/mod.rs`, `BUFFER_FRACTION`),
so the z4 buffered tile spans `[-128, 4224]`. The apex y-values `-20` and `4172`
are strictly inside that band, nowhere near the `-128`/`4224` clip edges. Clipping
truncates *at* the edge; it cannot create an interior dead end. `E` is a genuine
degree-2 dead-end vertex in the source boundary topology (or earlier way
segmentation), so the defect does not depend on tiling.

### Root cause (primary): the line merger fabricates the palindrome

At assembly time, for every tile at z < 14, every layer runs through
`merge_connected_lines` (`src/mvt/merge.rs`) before `mvt::encode_tile_into`. This
is the default MVT path (MVT + gzip are CLI defaults; the merge runs before
compression, so it is compression-independent). Fed two byte-identical duplicate
segments `J -> E` where `E` is degree-2:

- Chain starts at junction `J`, walks copy 1 out: `J -> E`.
- At `E` (degree 2) `build_chain` selects "the other segment end" = copy 2's back,
  and appends it reversed (`seg.iter().rev().skip(1)`): `E -> J`.
- Chain is now `J -> E -> J`, apex `E`; the walk stops because `J` is degree > 2.

Neighbouring paths confirm the topology: `p87 == p88` are the byte-identical
duplicate pair meeting at `J = (2813,76)`; `p89` is the `J -> (2828,-20) -> J`
hair the merger built. The palindrome also forms via the merger's pass-2 pure-cycle
path when both endpoints of a duplicate pair are degree-2 (an isolated stub).

### Root cause (fuel): boundary over-emission and duplicate edges

`process_prepared_relation_into` (`src/pipeline/relations.rs`), boundary
`GeomExpect::Line` branch, iterates `rel.member_ways` independently, emits each as
its own line with the parent relation id and attributes, ignores the assembled
rings, and does not dedup. A shared border way is therefore emitted once per
adjacent boundary relation, plus again at way level (`process_planned_way_into`,
`src/pipeline/phase12.rs`) if the way's own tags match the boundaries layer. `sort.rs`
orders but never dedups, and relation copies differ by `osm_id` in the payload, so
they reach assembly as parallel edges - the merger's fuel.

### Root cause (assembly-gate inconsistency)

The relation returns early if `multipolygon::assemble` yields no polygons
(`multi.polygons.is_empty()`), so boundary **line** emission silently depends on
**ring** assembly succeeding. Yet once one polygon assembles, the line branch emits
*every* raw member - including members that never participated in an assembled
ring. So an incomplete relation loses all its boundary lines, while a
partially-valid one emits dangling members.

### Root cause (attribution): parent attrs on every member

`prepare_relation` builds `matches` from relation tags only; `MemberWay` keeps just
role + coords, discarding member tags. So the line branch stamps parent-relation
attributes on every member. Because `match_boundaries_line` computes
`maritime = (maritime=yes) OR (natural=coastline)` from whichever element's tags it
sees, and `merge_same_attr_geometries` groups by exact tag equality, a border
emitted `maritime=true` from one producer and `maritime=false` from another is kept
as two separate features that never merge - the coarse/fine double-border. (Which
producer supplied the maritime=true copy is not provable from the tile: feature ids
are erased after merge.)

### Latent bug (not the demonstrated cause): DP on a closed ring

`simplify_into` (`src/geometry/simplify.rs`) forces `keep[0]` and `keep[len-1]` and
splits on the baseline `points[0] -> points[len-1]`. For a closed line those are
equal, the baseline has zero length, `perp_dist_sq` takes its `len_sq < 1e-30`
branch, and the first split is the vertex farthest from the shared start/end. A
thin closed loop can then simplify to an out-and-back hair.

Two honest caveats: (1) the output is not *always* a hair - depending on tolerance
and pins a closed line may stay a valid loop, become an out-and-back, or collapse
below two vertices and disappear at encode. (2) We did **not** independently prove
the Norway inputs were open duplicate segments rather than an already-closed source
line: the code proves the merger *can* produce this exact output, not that no
earlier stage supplied a closed line. Distinguishing them needs a pre-merge capture
or a fixture. The merger remains the compelling explanation; the closed-ring path
is a real latent trap that is not excluded by evidence. Note the same degenerate
seeding also exists in `simplify_into_with_required`.

---

## 2. Cross-verification

Two independent reviewers checked the first draft against the source. Both
confirmed all load-bearing claims; they diverged on one:

- **codex (gpt-5.5, xhigh)** - the more adversarial. Overturned the clipping-origin
  claim with buffer arithmetic (verified correct here), corrected the `p23/p24`
  duplicate claim, the earcut worst-deviation figure, and the archive provenance,
  and surfaced the assembly-gate inconsistency and the epistemic gap on
  "inputs are open."
- **Fable agent** - confirmed the full chain including an exact `build_chain` trace,
  and independently found the pass-2 palindrome route and that
  `simplify_into_with_required` shares the degeneracy. **Missed** the clipping
  arithmetic (called clipping "a plausible source" of `E`, which codex disproved).

Where they conflicted (is `E` a clip artifact?), the tie was broken by direct
verification of `BUFFER_FRACTION`: `E` is interior to the buffer, so **not** a clip
artifact. codex correct.

---

## 3. Atomic claim ledger

One row per falsifiable atom. Verdict legend: **OK** = code-verified true;
**FIX** = overturns/corrects a statement in an earlier draft; **GAP** = true but
originally omitted / an undocumented coupling; **UNPROVEN** = asserted but not
demonstrated by available evidence; **NIT** = imprecise wording. Per-cluster
`competitor` verdicts are consolidated in section 6; representative rows point there.

### Cluster W - merger wiring (default path)

| id | atom | evidence | verdict | competitor |
|----|------|----------|---------|-----------|
| W1 | The per-tile MVT encoder is `encode_tile_batch_mvt`, dispatched on `TilePayloadFormat::Mvt`. | `assemble.rs::encode_tile_batch_mvt` | OK | |
| W2 | `merge_same_attr_geometries` is called per layer before `mvt::encode_tile_into`. | `assemble.rs` (merge loop then encode) | OK | |
| W3 | `merge_same_attr_geometries` skips `Layer::Ocean`. | `assemble.rs` (`if li == Layer::Ocean continue`) | FIX (doc said "every layer") | |
| W4 | `merge_same_attr_geometries` runs at all zooms, including z14. | `assemble.rs` (unguarded loop) | OK | |
| W5 | `merge_connected_lines` is called per layer, gated by `z < 14`. | `assemble.rs` (`if z < 14`) | OK | |
| W6 | `merge_connected_lines` visits ocean too but no-ops on non-LineString features. | `mvt/merge.rs` (`geom_type != LineString`) | OK | |
| W7 | MVT is the CLI default tile format. | `main.rs` (`TileFormatArg::Mvt`) | OK | |
| W8 | gzip is the CLI default compression. | `main.rs` (`TileCompressionArg::Gzip`) | OK | |
| W9 | Compression is applied only after MVT encode; the merge is compression-independent (also runs for brotli). | `assemble.rs` (encode then compress) | FIX (doc said "MVT+gzip path") | |
| W10 | `merge_connected_lines` / `merge_same_attr_geometries` are `LayerBuilder` methods in `src/mvt/merge.rs`. | `mvt/merge.rs` | OK | |

### Cluster C - build_chain palindrome mechanism

| id | atom | evidence | verdict | competitor |
|----|------|----------|---------|-----------|
| C1 | `merge_line_segments` registers front and back of every segment in an endpoint map. | `mvt/merge.rs::merge_line_segments` | OK | AVOIDABLE; planetiler LoopLineMerger dedups edges at insert; JTS shares our bug, see §6 |
| C2 | Pass 1 seeds chain starts only from endpoints of degree != 2. | `mvt/merge.rs` (`ends.len() != 2`) | OK | |
| C3 | For duplicates `J->E` (E deg 2, J deg >2), build_chain starts at J and appends copy 1 forward. | `mvt/merge.rs::build_chain` | OK | |
| C4 | At E, continuation selection excludes the arrived end and picks the duplicate's back. | `mvt/merge.rs` (the `find` on ends) | OK | |
| C5 | Entering the duplicate at its back with a non-empty chain appends `seg.iter().rev().skip(1)`. | `mvt/merge.rs::build_chain` | OK | |
| C6 | `skip(1)` omits the already-present E and appends the remaining vertices through J. | `mvt/merge.rs::build_chain` | OK | |
| C7 | The walk stops at J because J is degree != 2. | `mvt/merge.rs` (degree check) | OK | |
| C8 | Result is exactly `J -> ... -> E -> ... -> J`. | derived from C3-C7 | OK | |
| C9 | Other stop conditions: visited segment, degree != 2, vertex cap, closed self-loop. | `mvt/merge.rs::build_chain` | OK | |
| C10 | The palindrome also forms via pass-2 pure-cycle collection when both endpoints are degree-2. | `mvt/merge.rs` (pass 2 loop) | GAP (Fable) | |
| C11 | Re-encode drops only consecutive-identical points, so the palindrome survives. | `mvt/merge.rs::encode_line_segments` | OK | |
| C12 | Endpoint E is NOT a clip artifact: buffer = 4096*8/256 = 128; z4 band [-128,4224]; E y = -20 / 4172 are interior. | `geometry/mod.rs::BUFFER_FRACTION`; `clip.rs` | FIX (doc said "clipping artifact") | |
| C13 | Duplicate edges inflate node degree at J, which can also block otherwise-legitimate stitching through J. | reasoned from C1-C2 | OK (2nd-order) | |

### Cluster R - relation line emission / over-emission

| id | atom | evidence | verdict | competitor |
|----|------|----------|---------|-----------|
| R1 | The boundary `GeomExpect::Line` branch iterates `rel.member_ways` individually. | `relations.rs::process_prepared_relation_into` | OK | DEFECT (ours); tilemaker+planetiler emit per-way, see §6 |
| R2 | It does not consult assembled `multi.polygons` rings for line output. | `relations.rs` (Line branch) | OK | |
| R3 | Each emitted member line carries `rel.osm_id`. | `relations.rs` (emit calls) | OK | |
| R4 | Each emitted member line carries the parent relation's match attributes. | `relations.rs` (`rel.matches`) | OK | |
| R5 | No dedup of member ways within or across relations in this branch. | `relations.rs` (Line branch) | OK | |
| R6 | The whole relation returns early if `multi.polygons.is_empty()`, so line emission is gated on ring assembly. | `relations.rs` (`if multi.polygons.is_empty return`) | GAP | |
| R7 | The line branch additionally requires `rel.is_boundary`. | `relations.rs` (`if !rel.is_boundary continue`) | OK | |
| R8 | Once >=1 polygon assembles, ALL raw members are emitted, including members absent from any assembled ring. | `relations.rs` (Line branch after gate) | GAP (new bug, codex) | |
| R9 | A shared way is emitted once per adjacent relation that matches shortbread, is administrative, has >=1 assembled polygon, and contains it. | `relations.rs` (composition of R1-R7) | OK (refined) | |
| R10 | `process_planned_way_into` emits at way level only if the way's OWN tags match the boundaries layer. | `phase12.rs::process_planned_way_into` | OK | |
| R11 | Member ways are not suppressed at way level due to relation membership. | `phase12.rs` | OK | |
| R12 | `relation_shared_vertex_keys` is computed and passed to the polygon branch but NOT the line branch. | `relations.rs` | OK (evidence for no-pins) | |
| R13 | `sort.rs` orders records (key then payload) but never deduplicates. | `sort.rs` (cmp + merge; `duplicate_keys` test) | OK | |
| R14 | Relation copies differ in payload because `osm_id` is in the wire payload. | `wire_format.rs` | OK | |

### Cluster AT - attribution and simplification asymmetry

| id | atom | evidence | verdict | competitor |
|----|------|----------|---------|-----------|
| AT1 | `prepare_relation` builds `matches` from relation tags only, via `match_element(.., MultiPolygon)`. | `relations.rs::prepare_relation` | OK | |
| AT2 | `MemberWay` stores role + coords only; member tags are discarded. | `multipolygon.rs::MemberWay` | OK | |
| AT3 | Therefore parent attrs are applied to every member; member tags unavailable to the line branch. | AT1+AT2 | OK | DEFECT (ours); fix = per-way attr resolution, see §6 |
| AT4 | The relation line branch passes an empty preserve mask `&[]`. | `relations.rs` (emit calls) | OK | |
| AT5 | The way line branch passes `&preserve_vertex_mask`. | `phase12.rs` | OK | |
| AT6 | That mask may be empty; way lines do not necessarily have pins. | `phase12.rs` (mask population) | FIX (doc implied always pinned) | |
| AT7 | `emit_line_feature` uses `simplify_into_with_required` when pins exist, else the plain cascade. | `emit.rs::emit_line_feature` | OK | |
| AT8 | `match_boundaries_line` requires `boundary=administrative`. | `boundaries.rs::boundary_match` | OK | |
| AT9 | It requires an `admin_level` tag present. | `boundaries.rs` (`tags.get("admin_level")?`) | OK | |
| AT10 | It requires `admin_level` to parse as an integer. | `boundaries.rs` (`parse_i64(..)?`) | OK | |
| AT11 | Only `admin_level` 2 or 4 match; other integers return None. | `boundaries.rs` (`match level`) | OK | |
| AT12 | `natural=coastline` sets `maritime=true`. | `boundaries.rs::match_boundaries_line` | OK | |
| AT13 | `maritime=yes` also sets `maritime=true`. | `boundaries.rs` (the `||`) | OK | |
| AT14 | The maritime attr is computed only inside the `boundary_match` Some-branch, so `natural=coastline` ALONE never adds a way to boundaries. | `boundaries.rs` (attr inside `if let Some`) | FIX (doc's coastline example overstated) | |
| AT15 | `merge_same_attr_geometries` groups by exact geom-type + tag equality. | `mvt/merge.rs` | OK | |
| AT16 | A `maritime=true` and a `maritime=false` copy of one border never merge (double-draw). | AT15 | OK | |
| AT17 | Which producer supplied the maritime=true feature is not provable from the tile (ids erased after merge). | `mvt/merge.rs` (id cleared) | UNPROVEN (by design) | |

### Cluster DP - closed-ring simplification (latent)

| id | atom | evidence | verdict | competitor |
|----|------|----------|---------|-----------|
| DP1 | `simplify_into` forces `keep[0]` and `keep[len-1]`. | `geometry/simplify.rs::simplify_into` | OK | UNIVERSAL bug (planetiler+tippecanoe DP share it); cure = Visvalingam, see §6 |
| DP2 | It recurses on the baseline `points[0] -> points[len-1]`. | `geometry/simplify.rs` | OK | |
| DP3 | For closed input that baseline has `len_sq ~ 0`. | derived | OK | |
| DP4 | `perp_dist_sq` takes the `len_sq < 1e-30` branch and returns distance-to-start. | `geometry/simplify.rs::perp_dist_sq` | OK | |
| DP5 | The first split is the vertex farthest from the shared start/end. | `geometry/simplify.rs` | OK | |
| DP6 | The output is NOT always a hair - may stay a valid loop, become out-and-back, or drop below 2 vertices. | reasoned; tolerance/pin-dependent | FIX (doc overstated) | |
| DP7 | `simplify_into_with_required` shares the identical degenerate seeding for closed input. | `geometry/simplify.rs::simplify_into_with_required` | GAP (doc named only `simplify_into`) | |
| DP8 | Closed member ways reach this path (no closedness filter; only `coords.len() >= 2`). | `relations.rs` Line branch | OK | |
| DP9 | It was NOT demonstrated that the Norway spikes come from the merger rather than a pre-closed source line. | no pre-merge capture / fixture exists | UNPROVEN | |

### Cluster E - evidence and provenance

| id | atom | evidence | verdict | competitor |
|----|------|----------|---------|-----------|
| E1 | `norway-20c8bd7.pmtiles` was built from commit `20c8bd7`, which predates the durable ocean artifact; its low-zoom ocean came from computed shapefile processing. | archive filename + commit history | FIX (draft conflated with repo HEAD) | |
| E2 | The elimination argument survives: low-zoom ocean did not come from the OSM PBF either way. | ocean pipeline | OK | |
| E3 | earcut oracle on ocean: 0 over-threshold, 0 misattached, all zooms. | oracle re-run | OK | |
| E4 | Global worst earcut deviation is `1.359e-4` (z11), not `6.8e-5` (z8 only); both far below `0.01`. | oracle output | FIX | |
| E5 | Zero earcut deviation is strong rendering-fidelity evidence but not a formal proof of no ring self-intersection. | oracle scope | NIT | |
| E6 | z4/8/4 boundaries has exactly two merged features: maritime=true (3 sub-lines), maritime=false (86 sub-lines); both admin_level=2, disputed=false, no id. | tile re-decode | OK | |
| E7 | `p5==p6`, `p9==p10`, `p87==p88` are exact duplicates; `p23` and `p24` are NOT (shared start, diverging paths). | SVG dump | FIX (draft listed p23/p24 as a pair) | |
| E8 | The palindromes are present verbatim in the archived z4/8/3 and z4/8/4 tiles. | tile re-decode + SVG | OK | |
| E9 | "A spike is always a geometry defect" is too absolute (stroke joins can spike valid geometry); here the geometry is genuinely defective. | general | NIT | |
| E10 | boundaries starting at z0 does not alone prove this feature survives every low zoom; duplicate survival + simplification + clipping + the z<14 merger are also required. | reasoning chain | NIT | |

---

## 4. Fix directions (with reviewer critiques)

**Spike, smallest first:**

- **Parallel-edge guard in `build_chain`** - when the degree-2 continuation is
  geometrically identical (forward or reversed) to the current segment, do not
  traverse it. Placed inside `build_chain` it covers both pass-1 and pass-2 routes.
  Both duplicates still emit (the orphan is picked up as its own start), just
  un-palindromed.
- **Segment dedup before graph construction** - remove exact forward/reversed
  duplicates within one same-attribute feature. Also kills the intra-feature
  double-draw. Does NOT remove the maritime=true/false cross-feature double-draw
  (separate attrs). Needs tests for legitimately coincident lines and closed
  segments.
- **Emergency lever** - skip `merge_connected_lines` for boundaries. Blunt; leaves
  all duplicate drawing.
- **Regression fixture** - two identical `A..B` segments. Must cover: A a junction
  (pass 1) AND both endpoints degree-2 (pass 2); both segment orientations; and
  shuffled input order.

**Structural duplication (complete fix):**

- Build boundary metadata keyed by member way id in the relation prepass (requires
  extending `PreparedRelation` / `MemberWay`, which currently discard way id and
  member tags).
- Emit each physical boundary way once, in the way phase.
- Resolve multiple parents explicitly for admin_level, disputed, maritime, and
  roles - inheriting only missing `boundary`/`admin_level` would drop a parent's
  `disputed=yes` on an untagged member. Include the min-zoom split (admin 2 -> z0,
  admin 4 -> z7).
- Also fix the assembly-gate coupling (R6/R8): line emission should not depend on
  ring assembly succeeding, and should not emit members absent from any ring.
- Do NOT simply delete relation line emission: untagged member ways are reachable
  only via the relation path and would vanish.

**Latent DP:** make simplification ring-aware (split a closed ring at its two
mutually-farthest points before recursing) in BOTH `simplify_into` and
`simplify_into_with_required`; preserve the duplicated closure, translate required
pin indices, and define behavior when fewer than two distinct vertices remain.
Competitor-preferred route (see §6): switch closed-ring / boundary simplification
to a baseline-free metric (Visvalingam-Whyatt), which tippecanoe and tilemaker's
OpenMapTiles profile both use and which is immune to the degenerate baseline by
construction, rather than patching DP. This is a universal bug: planetiler's and
tippecanoe's own Douglas-Peucker paths still carry it.

**To actually prove merger-vs-DP:** capture the pre-merge segments for z4/8/4 (or a
fixture) and check whether any single input line is already closed. Until then DP9
stays UNPROVEN.

---

## 5. Tooling used

- `scripts/validate/earcut-oracle.mjs <archive> ocean 0.01 --unique` - ocean
  tessellation / self-intersection gate.
- `brokkr diag --file <archive> -z Z -x X -y Y` - per-ring winding/area.
- `brokkr svg --file <archive> -z Z -x X -y Y -l boundaries -o data/<name>.svg` -
  boundaries-only SVG; read the path `d` attributes for the palindrome.
- `brokkr pmtiles-inspect --file <archive>` - layer/zoom ranges, format.

---

## 6. Competitor cross-reference

Four competitors surveyed (one Sonnet agent each), each checked against the actual
source, not against this document's reasoning. Verdict matrix:

| cluster | planetiler | tilemaker | tippecanoe | stedsplakat |
|---------|-----------|-----------|------------|-------------|
| R (per-relation over-emission) | AVOIDS - core hardcodes `canBeLine=false` for relations; only ways emit lines | AVOIDS - per-way via `NextRelation`/`FindInRelation`; documented, deliberate | N/A - no relation model | N/A - draws no admin boundaries |
| AT (parent-attr stamping / maritime split) | AVOIDS mis-attribution but UNDER-attributes (relation->way is a `TODO`) | AVOIDS - maritime resolved once from way tags; `disputed` aggregated across parents | N/A | N/A |
| C (build_chain palindrome) | AVOIDS decisively - `LoopLineMerger` dedups edges at insert + post-simplify + assert invariants | AVOIDS by construction - forward-only join, never reverses a segment | AVOIDS by omission - never fuses lines through shared vertices | MATCHES (latent) - JTS `LineMerger` has the identical degree-2 blind spot |
| DP (closed-ring degeneracy) | MATCHES - same DP; lines get no `minPoints` floor, 3-pt `[start,apex,start]` spike survives | AVOIDS in OMT (Visvalingam `isClosed` floor); Shortbread falls back to unaudited Boost DP | MATCHES in DP; ships Visvalingam (immune) as opt-in | N/A - no simplification exists |

### What this settles

- **R and AT are our defects, not Shortbread behavior.** Both real Shortbread
  implementers emit boundary lines **once per way** and resolve relation attributes
  down onto the way. Tilemaker's OpenMapTiles profile carries a comment naming our
  exact failure: *"we process administrative boundaries as properties on ways,
  rather than as single relation geometries, because otherwise we get multiple
  renderings where boundaries are coterminous."* Its `docs/RELATIONS.md` states the
  rule outright: properties-on-ways for admin boundaries, complete-geometries for
  filled areas. Our per-member-way relation emission is the anti-pattern they
  designed around. The ledger's "complete fix" IS their shipping architecture.

- **C is avoidable, and the cheapest fix kills the fuel at the merger.**
  Planetiler's `util/LoopLineMerger` (built specifically to replace JTS's naive
  `LineMerger`; ref oliverwipfli.ch, "Improving Linestring Merging in Planetiler",
  2024-10-30) rejects an exact-coordinate duplicate edge at graph-insertion time
  (`Node.addEdge`), runs a second dedup after simplification, and asserts the very
  invariants we violated (no duplicate edge, no edge-and-its-reverse at a node, no
  non-loop degree-2 node after merge). Tilemaker's forward-only join is an
  alternative structural guard. Tippecanoe simply never fuses lines. The one system
  that fuses the way we do - JTS `LineMerger`, used by stedsplakat - has our exact
  bug. So "delegate to a real geometry library" would NOT have saved us.

- **DP-on-closed-ring is a universal latent bug**, present in planetiler,
  tippecanoe's DP path, and (very likely) Boost - not elivagar-specific. Two
  independents (tippecanoe, tilemaker-OMT) use Visvalingam-Whyatt, which is immune
  by construction (local triangle-area metric, no start/end baseline). Nobody
  implements the "split the ring at its two farthest points" DP patch; Visvalingam
  sidesteps the whole class.

### Corrections to premises used in this investigation

- **Planetiler does NOT have a demonstrable shared-edge / min-admin-level dedup
  pass in the vendored tree.** `planetiler-openmaptiles/` is empty here and the
  Shortbread sample carries an explicit `# TODO get min admin level from
  relations`. So planetiler's vendored Shortbread config would UNDER-draw admin
  boundaries (opposite of our over-draw) - same root gap (no relation->way
  resolution), different symptom. Do not claim planetiler solved shared-edge dedup;
  it is an acknowledged open problem in its own sample.
- **JTS `LineMerger` is not a robust primitive to adopt.** Hand-traced by the
  stedsplakat agent: its planar-graph `getDegree()` is a raw incident-edge count
  with no parallel-edge dedup, so it reproduces our `J->E->J` palindrome verbatim.

### Ranked guards to steal

1. **Per-way boundary emission + multi-parent attribute resolution** (tilemaker
   shipping; planetiler core's `OsmRelationInfo` / `RelationMember` channel). Fixes
   R and AT at the source. Requires extending our `PreparedRelation` / `MemberWay`
   to keep the member way-id and expose parent-derived `min(admin_level)`,
   `maritime`, `disputed` to a single per-way emit. THE fix - it is what both
   reference implementations do.
2. **Edge dedup at merger insertion** (planetiler `LoopLineMerger.addEdge`: reject
   an edge equal - forward or reversed - to one already at the node). Small, local,
   kills the C-cluster fuel independent of #1. Adopt regardless as the backstop.
3. **Baseline-free closed-ring simplification** (Visvalingam-Whyatt, per tippecanoe
   + tilemaker), or ring-aware DP split, in BOTH `simplify_into` and
   `simplify_into_with_required`. Fixes the universal DP class.
4. **Assertion-encoded graph invariants** (planetiler `LoopLineMerger.valid()`:
   no duplicate edge, no edge-and-its-reverse at a node, no non-loop degree-2 node
   after merge). Turns "this shape is structurally impossible" into executable
   checks - exactly the regression fixtures section 4 calls for.

### Priority read

#1 is the real fix and matches the reference implementations. #2 is a cheap,
independent second line of defense worth adopting on its own. #3 addresses a bug
that even the best competitor (planetiler) still ships. #4 supplies the tests. The
work is now a fix-elivagar-properly program with a validated target architecture,
not a one-line spike patch.
