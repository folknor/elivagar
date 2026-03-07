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

### Option D: Coordinated simplification via edge hashing (recommended)

Use a deterministic edge-identity scheme so that shared edges are simplified
identically by both polygons during PBF phase, without either polygon
needing to know the other exists.

**Algorithm:**
1. Define a canonical edge identity: for any edge between two OSM nodes,
   the identity is `(min(node_id_a, node_id_b), max(node_id_a, node_id_b))`.
   This is the same regardless of which polygon references the edge and in
   which direction.
2. During PBF phase, when simplifying a polygon ring at zoom z:
   a. Run DP as normal to get the kept-vertex set.
   b. For each *original* edge in the ring that was removed by DP, compute
      its canonical edge identity. Check: is this edge part of a shared
      boundary? (See below for how to know this.)
   c. If shared, the simplification decision must be deterministic based
      solely on the edge's geometry and the zoom tolerance — not on the
      polygon context. This means: for a sequence of vertices along a shared
      boundary, the DP result must be the same regardless of which polygon
      is being simplified.

**The key insight:** Two polygons sharing an edge have *the same vertex
sequence* along that edge (by definition — they reference the same OSM
ways). If we simplify that vertex sequence the same way from both sides,
the results are identical. DP is already deterministic given the same input
and tolerance. The problem is that each polygon has *additional* vertices
(the non-shared parts of its ring) that affect the DP recursion.

**Refined algorithm — segment isolation:**
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

This is equivalent to `simplify_into_with_required` where the required
indices are the shared-segment endpoints. We already have this function.

**Shared edge identification — two approaches:**

*Approach D1: OSM topology (PBF phase, zero extra data)*

OSM relations and ways carry explicit topology. A boundary relation
references ways; adjacent boundary relations share way references. During
PBF phase, when processing a way that belongs to multiple relations (or to
a relation and also appears as a standalone polygon), we know it's shared.

Implementation:
- In the relation accumulation pass, build a `way_id -> Vec<relation_id>`
  index (already partially done for multipolygon assembly).
- A way appearing in 2+ polygon relations (or in 1 relation + as a
  standalone polygon way) has shared edges.
- Mark the way's vertex indices as "shared segment endpoints" (the first
  and last vertex of each shared way reference).
- Pass these as `required_indices` to `simplify_into_with_required`.

This is cheap: one HashMap lookup per way during relation processing,
plus the existing `simplify_into_with_required` machinery. No sort record
changes, no assemble-phase changes.

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
- **Recommend D1 first**, with D2 as a follow-up if D1 leaves visible seams
  from non-relation shared edges.

## Recommended plan

### Phase 1: Relation-aware vertex pinning (D1)

**Goal:** Pin shared-edge endpoints during PBF-phase DP simplification so
that adjacent relation-derived polygons produce identical simplified edges.

**Data flow changes:**
1. During relation batch processing (`flush_rel_batch`), when resolving a
   multipolygon relation's member ways, note which ways appear in multiple
   relations. Build a per-batch `FxHashSet<i64>` of shared way IDs.
2. When emitting polygon sort records, for ways marked as shared, pass
   their vertex indices as `required_indices` to
   `simplify_into_with_required`. This is already done for seam-detected
   vertices via `preserve_vertex_mask` — extend the same mechanism.
3. No sort record format changes. No assemble-phase changes.

**What this fixes:**
- Admin boundary seams (boundaries layer) — the primary visual issue.
- Multipolygon relation seams (land, water_polygons where geometry comes
  from relations).

**What this doesn't fix:**
- Shared edges between independent closed-way polygons (rare for the
  affected layers).
- Shared edges between a relation polygon and an overlapping standalone
  polygon.

**Cost model:**
- PBF phase: one `FxHashSet<i64>` per relation batch (way IDs only, not
  vertices). Lookup per way during emission — negligible.
- The `simplify_into_with_required` path is already benchmarked and has
  no measurable overhead vs regular DP.
- Sort record sizes: slightly larger at low zoom (pinned vertices survive
  that would otherwise be removed), but this is bounded — only shared-edge
  endpoints are pinned, not entire vertex sequences. Expected impact: <1%
  sort byte increase.

### Phase 2: Remove defer-then-reconcile (cleanup)

Once Phase 1 is validated:
1. Remove `seam_reconcile_layers` config, CLI flag, and `DeferralStats`.
2. Remove `reconcile_boundary_seams` in assemble phase.
3. Remove `detect_shared_chains`, `canonicalize_shared_chains`,
   `build_pinned_mask`, `simplify_ring_tile_coords` from geometry.rs
   (or keep as utility if other uses exist).
4. The `--seam-reconcile-layers` flag becomes a no-op (warn and ignore)
   for one release, then remove.

### Phase 3: Node-pair hashing for completeness (D2, if needed)

Only if Phase 1 leaves visible seams from non-relation shared edges.
Not designed in detail here — gate on evidence.

## Synthetic benchmark plan

Before touching pipeline code, validate the core hypothesis:

1. Generate N polygon pairs with known shared edges (varying vertex counts:
   10, 100, 1000 vertices per shared segment).
2. Simplify each polygon independently with standard DP at z8 tolerance.
3. Measure: how many shared-edge vertices diverge? What's the gap size?
4. Simplify again with shared-segment endpoints pinned as required indices.
5. Measure: do shared edges now agree? What's the vertex count difference?

This can be a `#[test]` in `geometry_tests.rs` — no pipeline or data files
needed. The test validates that segment-isolated DP with pinned endpoints
produces identical output for both sides of a shared edge.

## Risk assessment

**Low risk:**
- Phase 1 uses existing `simplify_into_with_required` (well-tested).
- No sort record format changes.
- No assemble-phase algorithm changes.
- Boundary layer already uses required-vertex pinning for detected shared
  vertices — this extends the same mechanism to relation-level topology.

**Medium risk:**
- Relation batch processing currently doesn't track cross-relation way
  sharing. Need to verify that way IDs are available at the right point
  in the pipeline. The `flush_rel_batch` function processes batches of
  relations — ways shared across batches won't be detected within a single
  batch. This is acceptable: cross-batch sharing is rare for adjacent
  polygons (they tend to be in the same or nearby PBF blocks).

**Not a risk:**
- Performance regression. The only new work is a HashSet lookup per way
  during emission, which is O(1) and negligible vs geometry processing.

## Comparison with other tools

- **Planetiler:** Does not reconcile shared edges. Relies on buffer overlap
  to hide seams. Documented as a known issue.
- **Tilemaker:** No shared-edge handling. Same buffer-overlap strategy.
- **Tippecanoe:** Issue #105 documents the problem. No fix implemented.
  Recommends pre-processing (e.g. Mapshaper's `-snap` or `-clean`).

Elivagar's approach (relation-topology-aware vertex pinning) would be
unique among open-source tile generators. The key enabler is that we
process OSM PBF directly and have access to relation topology, unlike
tools that consume pre-processed GeoJSON/shapefiles.
