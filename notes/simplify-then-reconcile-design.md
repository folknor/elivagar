# Design: Simplify-Then-Reconcile Seam Fix

**Status:** Design doc (not yet implemented)
**Prerequisite reading:** `notes/tile-seam-fix-2026-03-06.md`, TODO.md Phase 3A findings

## Problem

Adjacent polygons sharing an edge (e.g. two admin boundaries, or land meeting
water) are simplified independently during PBF phase. Douglas-Peucker can
choose different kept vertices on each side, creating gaps/overlaps ("seams")
visible at low zoom along long boundaries.

The current approach ("defer-then-reconcile") skips PBF-phase simplification
for configured layers at z <= max_zoom, emitting full-resolution geometry into
sort records. During assemble, shared chains are detected, canonicalized, and
then simplified with pinned shared vertices.

**Why it doesn't scale:** Full-res deferral multiplies sort record size
dramatically for geometry-heavy layers. Norway benchmarks showed 3.3x phase12
regression for `water_polygons:8` and 2x for `water_polygons:5`. The cost is
in serializing thousands-of-vertex coastline ways into sort records at every
zoom level without simplification. The reconciliation itself is cheap (~18ms
for 455 chains); all cost is upstream.

## Goal

Eliminate full-res deferral entirely. Simplify during PBF phase as normal
(keeping current performance), then detect and fix divergent shared edges
during assemble. The assemble phase already has all polygon rings for a tile
in memory — the question is what "fix" to apply.

## Architecture options

### Option A: Vertex snapping (post-simplification)

After independent DP simplification, each polygon has its own simplified
vertex set along the shared boundary. Snap vertices from one ring to the
nearest vertex (or edge) of the other ring within a tolerance.

**Algorithm:**
1. Detect shared chains via `detect_shared_chains` (existing code, works on
   tile-coordinate rings).
2. For each 2-incident chain, one ring is "canonical" (first incident).
3. For each vertex in the canonical ring's shared segment, find the closest
   point on the target ring's corresponding segment (vertex or edge
   interpolation).
4. If distance < snap_tolerance, insert/move the target vertex to match.

**Pros:** Simple, local operation, no re-simplification needed.
**Cons:** Can't fix cases where DP removed a critical vertex entirely — the
shared edge may have different vertex counts after independent simplification.
Vertex insertion changes ring topology. Snap tolerance tuning is fragile.

**Verdict:** Fragile. The core problem is that independent DP produces
*different vertex counts* along shared edges, not just slightly displaced
vertices. Snapping can't reconcile 3 vertices on one side with 5 on the other.

### Option B: Re-simplify shared chains (post-simplification)

After independent DP, detect shared chains on the *original* (pre-simplified)
geometry, then re-simplify just the shared segments with a single canonical
DP pass, and splice results back into both rings.

**Algorithm:**
1. During PBF phase, simplify normally (current behavior). Additionally,
   store the *original* shared-edge vertex sequence alongside the simplified
   sort record. This is the expensive part — but only for the shared segment,
   not the entire polygon.
2. During assemble, detect which features share edges (by matching original
   vertex sequences).
3. For each shared chain, run DP once on the canonical original vertices.
4. Splice the canonical simplified segment into both rings.

**Pros:** Produces geometrically correct shared edges. Single DP pass
guarantees identical output.
**Cons:** Requires carrying original shared-edge vertices through sort,
adding per-record overhead. Shared-edge detection during PBF phase requires
knowing which edges are shared before seeing all polygons — but we only know
this within a tile during assemble. Detection during assemble on simplified
geometry may miss edges that were shared in the original but diverged.

**Verdict:** Chicken-and-egg problem. We can't identify shared edges during
PBF phase (each polygon is processed independently). By assemble time, the
original geometry is gone.

### Option C: Tile-coordinate detection + re-simplification from simplified geometry

Accept that PBF-phase simplification produces divergent shared edges. During
assemble, detect *approximately* shared edges on the already-simplified tile-
coordinate geometry, then force agreement.

**Algorithm:**
1. PBF phase: simplify normally, no changes. No extra data in sort records.
2. Assemble phase, per tile, per reconcile-configured layer:
   a. Decode all polygon rings to tile coordinates (existing code path).
   b. Build edge index: for each ring, enumerate edges as `(v0, v1)` pairs
      (canonicalized so `v0 < v1`). Hash edges to find exact matches across
      rings.
   c. For exact-match edges: no action needed (already agree).
   d. For *near-match* edges (same edge in the original geometry, but DP
      kept different vertices): this is where divergence happens. But after
      simplification we can't distinguish "near-match shared edge" from
      "two unrelated edges that happen to be close."
   e. Fall back to the existing `detect_shared_chains` which finds
      vertex-identical shared segments. Any divergent segments won't be
      detected.

**Pros:** Zero PBF-phase cost. No sort record changes.
**Cons:** Can only reconcile edges that are *already identical* after
independent simplification. The interesting cases (where DP diverged) are
invisible to this approach. This is essentially what we already have.

**Verdict:** Doesn't solve the problem. The existing assemble-phase
reconciliation already does this for boundaries (where it works because
boundary geometry is vertex-light and DP rarely diverges).

### Option D: Coordinated simplification via vertex pinning (recommended)

Use OSM relation topology to identify shared edges, then pin shared-segment
boundaries during DP so that both polygons produce identical simplified
edges for the shared portion.

**The key insight:** Two polygons sharing an edge have *the same vertex
sequence* along that edge (by definition — they reference the same OSM
ways). If we simplify that vertex sequence the same way from both sides,
the results are identical. DP is already deterministic given the same input
and tolerance. The problem is that each polygon has *additional* vertices
(the non-shared parts of its ring) that affect the DP recursion.

**Solution — segment-isolated DP:**
1. During PBF phase, for each polygon ring, identify which sub-sequences
   are shared with other polygons. (This requires knowing shared edges —
   see "Shared edge identification" below.)
2. Split the ring at shared/non-shared boundaries.
3. Simplify each shared segment independently with DP (endpoints pinned).
   Because the segment is isolated from the rest of the ring, and both
   polygons have the same segment, they get the same DP result.
4. Simplify each non-shared segment independently with DP (endpoints
   pinned at the junction with shared segments).
5. Concatenate simplified segments back into the ring.

**Critical implementation detail:** This requires true segment-isolated DP,
not merely marking shared-segment endpoints as required indices in a
full-ring DP pass. Full-ring DP with required indices prevents deletion of
pinned vertices but does NOT isolate the recursion context — the DP
recursion partitions are determined by the full ring's vertex positions, so
interior vertices of the shared segment can still be kept/removed differently
by each polygon. `simplify_into_with_required` achieves segment isolation
only if it splits the ring at required indices first and runs DP on each
sub-segment independently. This must be verified or enforced.

**Shared edge identification — two approaches:**

*Approach D1: OSM relation topology (PBF phase, zero extra data)*

OSM relations and ways carry explicit topology. A boundary relation
references ways; adjacent boundary relations share way references. During
PBF phase, when processing a way that belongs to multiple relations (or to
a relation and also appears as a standalone polygon), we know it's shared.

Implementation:
- In the relation accumulation pass, build a `way_id -> count` index
  tracking how many polygon relations reference each way.
- A way appearing in 2+ polygon relations (or in 1 relation + as a
  standalone polygon way) has shared edges.
- At shared/non-shared way boundaries, pin the junction vertices and
  run segment-isolated DP on the shared sub-sequence.

**Limitation:** Only works for shared edges that come from explicit OSM
topology (relations referencing the same ways). Two independent closed-way
polygons that happen to share an edge (same node IDs) won't be detected
unless we also build a `node_id -> way_id` adjacency index.

*Approach D2: Node-pair hashing (PBF phase, lightweight index)*

Build a global `(node_a, node_b) -> count` map during PBF phase. First
pass (or piggyback on existing PBF scan): for each polygon way, enumerate
node-ID pairs and increment counts. Second pass (or during emission):
edges with count >= 2 are shared. Pin their vertices.

This catches all shared edges regardless of OSM topology, but requires
either a two-pass PBF scan or a deferred-emission architecture. The edge
count map for planet scale could be large (billions of edges), though only
edges with count >= 2 need to be retained.

**Verdict on D1 vs D2:**
- D1 (OSM topology) is simpler, cheaper, and handles the dominant case
  (admin boundaries, coastline relations, multipolygon relations). It
  misses the rare case of two independent closed-way polygons sharing an
  edge by coincidence.
- D2 (node-pair hashing) is complete but requires significant memory and
  either two passes or deferred emission.
- **Recommend D1 first**, with D2 as a follow-up if D1 coverage metric
  (see below) shows significant uncovered seam chains.

## Recommended plan

### Phase 1: Relation-aware segment-isolated DP (D1)

**Scope:** Partial mitigation for relation-derived shared edges. Not a
complete replacement for assemble-phase reconciliation — treat as an
additive improvement that reduces the number of seams reaching assemble.

**Goal:** Pin shared-segment boundaries during PBF-phase DP simplification
so that adjacent relation-derived polygons produce identical simplified
edges for the shared portion.

**Mechanism — segment-isolated DP (not merely endpoint pinning):**
1. Identify shared ways via relation topology (see below).
2. For each polygon ring containing shared ways, split the ring into
   shared and non-shared sub-sequences at way junction vertices.
3. Run DP independently on each sub-sequence (endpoints pinned). This
   guarantees identical output for both polygons on the shared segment.
4. Concatenate simplified sub-sequences back into the ring.

This is stronger than marking endpoints as required indices in full-ring
DP. Full-ring DP with required indices prevents endpoint deletion but
allows interior-vertex divergence because the recursion context differs
between polygons. Segment isolation eliminates the context dependency.

**Shared-way detection — cross-batch challenge:**

The naive approach (per-batch `FxHashSet<i64>` of shared way IDs) has a
significant gap: adjacent admin boundary relations (e.g. France and Germany)
often have very different relation IDs and will land in different batches
under `--rel-budget 64M`. This means the primary visual seam case — country
and state borders — is the one most likely to be missed by batch-local
detection.

Two mitigation strategies:

*Strategy 1: Global way-reference counting.*
Maintain a global `FxHashMap<i64, u8>` across all batches, counting how
many polygon relations reference each way ID. Build incrementally during
relation accumulation. Problem: the first relation to use a shared way
doesn't know it's shared yet. Requires either a two-pass approach over the
relation section or deferred emission, both of which break the streaming
architecture.

*Strategy 2: Boundary relation heuristic.*
Treat ALL ways in `type=boundary` relations as shared unconditionally. Pin
their junction vertices without requiring cross-relation detection. This
is conservative (pins some non-shared vertices, producing slightly larger
sort records) but catches the dominant case without any cross-batch
coordination. Admin boundary ways are shared by definition — they are the
line between two territories.

**Recommend Strategy 2** for Phase 1: simple, no architectural changes,
catches the most visible seam source. Strategy 1 can be added later for
non-boundary relation types if needed. Track `extra_pinned_vertices_boundary`
metric to bound blast radius — measures how many additional vertices survive
DP due to boundary-heuristic pinning vs unpinned baseline.

**Interaction with `--polygon-simplify-factor`:**
More aggressive polygon simplification (factor > 1.0) amplifies divergence
on shared edges — more vertices removed, bigger gaps between independently
simplified polygons. D1's segment-isolated DP becomes more important with
aggressive factors. Note that the pinned junction vertices may be far apart
after aggressive simplification, leaving long straight segments that could
still produce visible seams between junctions. This interaction should be
validated during benchmarking.

**What this fixes:**
- Admin boundary seams (boundaries layer) — the primary visual issue.
- Multipolygon relation seams (land, water_polygons where geometry comes
  from relations).

**What this doesn't fix:**
- Shared edges between independent closed-way polygons.
- Shared edges between a relation polygon and an overlapping standalone
  polygon.
- Cross-layer shared edges (e.g. land meeting water_polygons).

**Cost model:**
- PBF phase: one `FxHashSet<i64>` of boundary-relation way IDs (built
  during relation accumulation). Lookup per way during emission — negligible.
- Segment-isolated DP is equivalent cost to current DP (same total vertex
  count, just partitioned differently).
- Sort record sizes: slightly larger at low zoom (pinned vertices survive
  that would otherwise be removed), but this is bounded — only way-junction
  vertices are pinned. **Hypothesis:** <1% sort byte increase — must be
  validated by benchmarking with `extra_pinned_vertices_boundary` metric
  and sort byte delta before/after on Denmark + Norway.

**Coverage metric (required before Phase 2 decision):**
Run both D1 pinning and current assemble-phase reconciliation on
Norway + Denmark. Measure:
1. Total seam chains detected by assemble reconcile (baseline).
2. Of those, how many have zero divergence after D1 pinning (D1 hits).
3. Remaining seam chains that D1 doesn't cover (D1 misses).
4. D1 coverage = hits / baseline. Gate Phase 2 on coverage >= 90%.

### Phase 2: Evaluate defer-then-reconcile removal (candidate cleanup)

**Gated on Phase 1 coverage metric and visual QA across multiple datasets.**
Do not remove until:
1. D1 coverage metric >= 90% on NA + Norway + Denmark (all three).
2. Visual diffs show no new seam regressions vs current assemble reconcile.
3. `--polygon-simplify-factor` interaction validated at factor 2.0.

Candidate cleanup items (proceed only if all gates pass):
1. Remove `seam_reconcile_layers` config, CLI flag, and `DeferralStats`.
2. Remove `reconcile_boundary_seams` in assemble phase.
3. Remove `detect_shared_chains`, `canonicalize_shared_chains`,
   `build_pinned_mask`, `simplify_ring_tile_coords` from geometry.rs
   (or keep as utility if other uses exist).
4. The `--seam-reconcile-layers` flag becomes a no-op (warn and ignore)
   for one release, then remove.

If gating criteria NOT met, keep both paths (D1 pinning + assemble
reconciliation) as complementary — D1 reduces the seam count that reaches
assemble, and assemble reconcile handles the remainder.

### Phase 3: Node-pair hashing for completeness (D2, if needed)

Only if Phase 1 coverage metric shows significant uncovered seam chains
from non-relation shared edges. Not designed in detail here — gate on
evidence from the coverage metric.

## Synthetic benchmark plan

Before touching pipeline code, validate the core hypothesis:

1. Generate N polygon pairs with known shared edges (varying vertex counts:
   10, 100, 1000 vertices per shared segment).
2. Simplify each polygon independently with standard DP at z8 tolerance.
3. Measure: how many shared-edge vertices diverge? What's the gap size?
4. Simplify again with segment-isolated DP (shared segment simplified
   independently with pinned endpoints).
5. Measure: do shared edges now agree? What's the vertex count difference?

**Critical test:** Verify that `simplify_into_with_required` actually
performs segment-isolated DP (splits at required indices, runs DP on each
sub-segment independently) rather than full-ring DP with unkillable
vertices. If the current implementation does not isolate segments, it must
be modified or a separate `simplify_segments` function added.

This can be a `#[test]` in `geometry_tests.rs` — no pipeline or data files
needed. The test validates that segment-isolated DP with pinned endpoints
produces identical output for both sides of a shared edge.

## Risk assessment

**Low risk:**
- Segment-isolated DP is a well-understood algorithm variant.
- No sort record format changes.
- Boundary relation heuristic (Strategy 2) requires no architectural changes.

**Medium risk:**
- Must verify or enforce that DP implementation actually isolates segments
  at required indices. If `simplify_into_with_required` runs DP on the full
  ring with required vertices merely marked as unkillable, interior vertices
  of the shared segment can still diverge. This is the difference between
  "works in theory" and "works in practice."
- Coverage gap for non-relation shared edges is real and may require keeping
  assemble reconciliation as a complementary path indefinitely.

**Not a risk:**
- Performance regression. The only new work is a HashSet lookup per way
  during emission, which is O(1) and negligible vs geometry processing.

## Comparison with other tools

- **Planetiler:** Does not reconcile shared edges. Relies on buffer overlap
  to hide seams. Documented as a known issue.
- **Tilemaker:** No shared-edge handling. Same buffer-overlap strategy.
- **Tippecanoe:** Implements shared-border detection via `--detect-shared-borders`
  (canonical Tippecanoe issues #301/#302). The felt/tippecanoe fork extends
  this with `--no-simplification-of-shared-nodes`, which pins shared vertices
  during simplification — similar in spirit to D1. Tippecanoe's approach
  operates on pre-processed GeoJSON features, not OSM topology directly.

Elivagar's D1 approach (relation-topology-aware segment-isolated DP) has
the advantage of leveraging OSM relation structure to identify shared edges
without needing a separate shared-edge detection pass. The tradeoff is that
it only covers relation-derived shared edges, not arbitrary feature
adjacency. Tippecanoe's `--detect-shared-borders` finds shared edges by
geometric comparison across all input features, which is more complete but
requires an explicit detection pass.
