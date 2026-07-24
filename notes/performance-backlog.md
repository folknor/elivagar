# Perf-hunt backlog (prioritized)

Consolidated backlog distilled from independent analyses, ordered by
evidence from the 2026-07-06 profiling campaign at commit `95d6d52`
(plantasjen): denmark hotpath `13c024bb` + alloc `081975b3`, norway hotpath
`4e62f519` + bench `38dcd3e8`, germany hotpath `cc14c34a` + bench `6fc97675`.

Status 2026-07-24: reconciled against the July campaigns. P0-P3 are all
landed and kept, and the H5 ocean artifact plus the H1 phase12 rebuild
have moved every headline number below since. The profile section is the
2026-07-06 campaign's record - the don't-redo trail - not a current
reading. Current baselines and gate-reading rules live in
`reference/performance.md`; the live optimization queue is
`notes/planet-30gb-roadmap.md` (Sequencing). This file remains the item
ledger: stable IDs, verdicts, and what stays parked.

Item numbers are stable IDs from the original capture order, NOT ranks -
cross-references between items use them. Priority is the tier structure.

## What the profiles said (2026-07-06, superseded)

All readings at `95d6d52`, before P1-P3 and H5 landed. Known-superseded
highlights: the ocean phase is ~0.2s under the H5 artifact (was 11-16s),
the serial merge reader no longer exists (item 14), and phase12 was
rebuilt again by the H1 ordered-drain removal.

- `intersect_rect_into` is the top geometry sink on every dataset: 157 / 721 /
  243 thread-s (DK/NO/DE), and 67% of denmark's tracked allocation (48.4 GB,
  avg 28.8 KB per call). It is the shared primitive under ocean AND the OSM
  polygon paths.
- Coastal OSM multipolygons explode the tier-2 per-tile path. Norway:
  `emit_normalized_per_tile` 528 thread-s, `emit_multipolygon_feature` 595,
  `process_prepared_relation_into` 614 - one nested stack (relations ->
  multipolygon -> per-tile booleans -> intersect_rect). 777K relations;
  single z14 water_polygons features fan out to 22,460 tiles. On denmark the
  same path was 11.8 thread-s - denmark alone badly underprices it.
- Inland scale is phase12-bound: germany phase12 is 83% of clean wall (213s of
  256s); `process_raw_way` 754 thread-s across 69.6M ways;
  `prepass_shared_nodes` 79s pure serial; `ring_is_simple_complete` 110
  thread-s across 61.4M calls.
- The serial k-way merge reader IS the assemble phase at every scale:
  reader/phase = 4.1/4.3s DK, 23.2/23.7s NO, 30.1/30.9s DE (clean bench).
  The rayon encode stage is already fully hidden behind it.
- The ocean PHASE is flat (11-16s on all three datasets) - the spec-3
  restructure scales. But ocean's sort-record share varies wildly: 10% DK,
  59% NO, 2% DE.
- Clean RSS is comfortable (2.8 / 4.1 / 10.3 GB). The 20 GB figures seen
  under hotpath are instrumentation, not pipeline (see
  reference/performance.md reading rules).

Priority logic: P1 attacks the biggest cross-dataset sink (the emission
engine) with the design that subsumes the most other items. P2 attacks the
biggest single phase (phase12 ownership). P3 removes the one structurally
serial loop that caps assemble at planet scale. P0 is the one cheap, safe,
immediately-landable win. Ocean-specific redesigns demote to P4 because the
ocean phase is no longer a bottleneck at any measured scale.

---

## P0 - DONE (landed `9b51e46`, kept)

Item 15 half 1 (prepass overlap): landed and kept - denmark 35.0 to 31.8s,
germany 255.9 to 231.5s, output byte-identical. Cost recorded: germany peak
RSS 10.3 to 15.0 GB (prepass sets now coexist with node-store build);
removed by item 15 half 2, landed as part of the P2 phase12-ownership
rewrite below.

---

## P1 - DONE (spec 4, landed `c8f8184`..`a0fca65`, kept)

The spec 4 campaign (see git history) shipped all three landings plus
Landing 3's splitter (pulled forward) as one campaign. Result:
denmark ocean 11.8 to 5.9s / wall 31.8 to 26.4s; norway 160.1 to 105.0s.
Subsumed items 2, 3, 4, 5, 9, 10, 12 and Landing-3's item 1. All polygon
layers earcut-clean (a late convexity-soundness fix, `a0fca65`, caught 10
non-simple land polygons the pyramid's convex early-out let through -
recorded as the standing-gate save it was). Deferred within spec 4: the
germany verdict (phase12-bound, archives were wiped) and a norway bench-3.

Remaining descent-adjacent backlog items now unblocked and cheap (each a
few percent, do opportunistically): item 11 (memoize full-tile record),
item 13 (hoist zoom-independent OSM attrs - the pyramid's per-zoom attrs
cache already does most of this).

One coherent spec. Attacks the top of every profile simultaneously:
`intersect_rect_into` (157/721/243 thread-s + 67% of allocation), the
per-zoom chain (`emit_shape_for_zoom` 85/155/81, `emit_normalized_shape_for_zoom`
72/135/69, `cut_row_bands_with_scratch` 51/94/48), and - via folding tier-2 in -
norway's 528+595+614 thread-s relation/multipolygon stack.

### Item 10: recursive tile-pyramid descent (the core rewrite)

Presented by the per-zoom emission report as its high-conviction big rewrite,
done as a full coherent rewrite of the emission engine, not an incremental
patch. Replace prologue split + per-zoom row bands + gap runs + per-tile
boolean clips + OSM tier2/tier3 with a single recursive descent over the tile
pyramid in base (z14-grid) coordinates, per piece:

- At cell (z, tx, ty) hold the piece's fragment clipped to the cell's
  buffered rect, in base coords.
- Emit the tile for zoom z from the fragment: shift-round rescale (exact, as
  today), dedup, pin-aware DP, normalize (the fragment is tiny, so the
  boolean engine is cheap here), encode.
- Recurse: cut the base-coord fragment into 4 children (two axis bisections),
  reusing the ancestor-containment identity already proven and tested for
  `cut_row_bands` ((shape INTERSECT parent) INTERSECT child == shape
  INTERSECT child; child buffered rects are contained in the parent's because
  the child buffer is exactly half the parent's in base units).
- Uniform-subtree shortcut: when a fragment equals the full buffered rect,
  the entire subtree is full-tile fills - emit canonical full-tile records
  for the whole run with zero further geometry work. Replaces the rasterize +
  gap-run machinery with a trivially correct check and extends it across
  zooms, not just across one row.

Every boolean now operates on geometry that shrinks geometrically with depth:
the whole piece is noded once at the top instead of ~15 x log(rows) times,
and z14 boundary tiles clip tile-local fragments instead of row-global ones.
Deletes outright: `rasterize_shape_edges` and its three dilated-crossing
helpers, `cut_row_bands`, `emit_boundary_and_gap_tiles`, `split_piece`, and
the `emit_normalized_per_tile` tier - one code path used by ocean AND OSM
polygons.

The one real design problem is seams. Today's simplify-then-cut guarantees
adjacent tiles clip from the same globally simplified shape; cut-then-simplify
breaks that - neighbors simplify their fragments independently and the
coastline's tile-edge crossing can drift by up to `dp_tol`. Fix with existing
machinery: pin fragment vertices lying within a strip around the fragment's
clip border (the pin-aware DP was built for exactly this). Cut-introduced
vertices lie exactly on shared cut lines; both neighbors hold the identical
unsimplified chain in the shared buffer zone; pinning it makes the
edge-crossing geometry verbatim-identical on both sides. Interior
simplification stays free. Cost: unsimplified vertices in a ~buffer-wide
strip per tile at low zoom - measurable via the existing per-zoom tile-size
stats; earcut oracle + `verify --geometry-stats` + `compare-tiles` gate
correctness.

Parallel grain improves - today's (piece x zoom) fan-out has a fat tail (the
biggest z14 piece defines the critical path); the descent parallelizes within
a piece via rayon join on subtrees, so work granularity tracks geometry,
killing the tail. Fragments are transient and per-worker instead of a global
pre-split Vec<Shape>.

Risks: seam drift if the pinned-strip argument has a hole (gated by oracle,
verify, visual QA on coastline tiles); output not byte-identical (DP anchor
points change - accept, gate on the oracle not on diffing); low-zoom
tile-size inflation from pinned strips (measure; if bad, fall back to a tiny
strip tolerance rather than hard pins). Do it as a full rewrite behind
nothing (no env vars, no routing switch), benchmark, keep or revert. Gate on
all three datasets - denmark alone underprices the tier-2/relation payoff by
~50x (11.8 vs 528+ thread-s).

Build-on decisions folded in from day one:

- **Item 5 (flat representation)**: build the descent on flat buffers (points
  + ring ranges + shape ranges), not nested `Vec<Vec<IntPoint>>`. The alloc
  profile is the argument: `intersect_rect_into` 48.4 GB (28.8 KB/call),
  `simplify_shape_dp` 6.9 GB across 14.7M calls, `normalize_into` 2.8 GB -
  per-invocation Vec churn, exactly what flat buffers delete. The encoder
  already uses the right shape (`tile_points` + `tile_ranges`).
  `flags_from_pin_set` and `rescale_shape` per-zoom scratch churn fall out
  for free.
- **Item 9 (tier-2 unification)**: fold tier-2 in immediately after ocean and
  tier-3, not "later". Norway is the proof: `emit_normalized_per_tile` clips
  the whole ring per bbox tile (an 8x8-tile lake is 64 full booleans of the
  whole ring; a 22,460-tile fjord polygon is catastrophic). 528 thread-s NO,
  138 DE. The `OSM_TIER2_MAX_TILES = 64` seam then stops mattering and gets
  deleted - one path for all multi-tile polygons.
- **Item 12 (`ring_is_simple_complete`)**: deleted by the descent (normalize
  the small fragment instead). 110 thread-s on germany, 61.4M calls. If P1
  slips a quarter, this graduates to a standalone fix on that number alone.
- **Item 4 (cross-zoom sharing)**: the descent achieves it recursively
  (derive child fragments from parent, not from the z14 base 15 times);
  no separate LOD-cascade item survives.

### Item 1: dedicated O(n) integer rect clipper (second landing)

`intersect_rect` is the single primitive for row cutting, tile clipping,
tier-2 clipping, pre-split, and data-bounds clipping; a general boolean is
the right tool for exactly one of those (topology repair in `normalize`) -
everywhere else the clip geometry is a rectangle and the subject is already
simple. In the descent world the primitive is the 4-child cut: an exact
integer split-by-axis-line (walk the ring, exact rational crossings since the
cut line is on the grid, snap-round, sort crossings, reconnect;
O(V + K log K), plausibly 10-50x cheaper per cut than the general boolean).

Sequencing per the per-zoom report, unchanged by the new data: land the
descent (item 10) on the trusted in-tree boolean engine FIRST, then swap the
splitter in as a separate measured landing. Correctness burden: snap-round
crossings can create slivers/self-touches (the R23 class). Mitigation: keep
normalize (the boolean engine) as a debug-assert oracle on the splitter's
output during bring-up; earcut oracle as the gate. Known failure mode of
half-plane
clipping (concave subject crossing a clip edge twice yields one bridged ring)
is what i_overlay's `ContourDecomposition::decompose_contours` solves cheaply;
fall back to the full `Overlay` boolean if classification fails a cheap
validity check.

### Subsumed by P1 (do not schedule separately)

- **Item 2 (coverage-span emitter)**: the descent's uniform-subtree shortcut
  is the stronger form of the full/empty/boundary model; the x-bisection
  stopgap (R1c/A4) is pointless as a separate landing now.
- **Item 3 (edge/ring arena engine)**: the flat-buffer descent IS the arena
  idiom applied to emission; a second engine rewrite on top has no remaining
  target.
- **Item 4 (LOD cascade / `emit_shape_zoom_range` planner)**: see above.

---

## P2 - DONE (phase12 ownership rewrite, kept)

The largest phase at every scale (51% DK, 77% NO, 83% DE of clean wall), and
the production path (locations-on-ways, planet) lived or died here.

Item 16 (way-phase ownership rewrite): landed. The way phase now sends the
whole `PrimitiveBlock` into the rayon task instead of pre-extracting owned
`(String, String)` tags and node refs/coords into a `RawWay` on a serial
dispatch thread first; each task classifies and resolves against borrowed
`&str` tags directly. Every emitted feature writes into a per-worker
`RecordSink` (key, offset, len over a payload arena) instead of a per-record
`Box<[u8]>`, flushed straight to chunk files - the `OceanAcc`/`RelAcc`
arena-and-flush idiom, with `RelAcc` itself converged onto `RecordSink` as
part of the same landing. The in-flight concurrency ceiling now scales with
`config.threads` instead of a fixed 8, closing the parallelism-cap regression
risk raised in review. `RawWay` survives only as a `#[cfg(test)]` fixture.

Item 22 (selective way resolution + relation planning): landed in reduced
form. An upfront relation-planning prepass computes the member-way set
before the way pass runs; the way task now classifies tags under both
`ClosedWay`/`OpenWay` before resolving coordinates and returns immediately
for non-member ways that cannot match either geom type, skipping node-store
lookups entirely for them. Only relation members are written to
`way_index`. Landed narrower than the original pick: the prepass supplies
the member-way set only, not full relation skeletons - relation blocks are
still buffered and matched at end-of-read via `prepare_relation` exactly as
before, so item 18 (parallelize relation prepare, Parked below) was NOT
subsumed by this landing and remains open.

Item 15 half 2 (compact shared-node counters): landed. `prepass_shared_nodes`
replaced its in-memory `seen`/`shared` `FxHashSet<i64>` pair with an exact
external merge-sort of node refs (chunk files bounded by the sort budget,
k-way merge on read-back), removing the planet-scale `seen` set (est.
30-60 GB) by construction on both the node-store and locations-on-ways
paths. `shared` itself stays exact, so DP-pinning output is unchanged.

Output regress-identical throughout (`--tol 0`). This unblocks item 14 (P3):
its partitioned-sort producers can now write into the same arena idiom
directly. See git history for the campaign.

---

## P3 - DONE (item 14, partitioned sort + parallel assemble, kept)

Landed as designed: sort partitions by Hilbert tile-id range at write time
(z7-calibrated boundaries, `PARTITION_SPLIT_Z`), and assemble runs parallel
per-partition readers feeding rayon encode under a byte-budgeted claim
window (defaults promoted: 8 workers, 2 GiB park budget). The serial k-way
merge reader this item targeted no longer exists; per-partition merge cost
lands in `assemble_reader_ns`. The split-depth verdict (z7 kept: germany
assemble -31%, NA +1.6% accepted as straggler insurance) and the lz4 chunk
pricing live in the roadmap's H4. The skew risk called out here
materialized as predicted; its remaining half is H8b's recursive splitting
of hot partitions (germany-relevant, still open), with H2d's injected
stats as the natural boundary picker. Item 17 was subsumed, as predicted.

---

## P4 - ocean-specific redesigns (demoted, not dead)

The ocean phase is flat at 11-16s across all three datasets - no longer a
bottleneck at any measured scale. These stay on the books because of the
sort-volume angle and planet-scale coastline growth, but none outranks
P0-P3 on current evidence.

### Item 19: ocean as a tile compositor, not per-piece features

Ocean has one layer, no attributes, and visually wants tile coverage, yet it
is emitted as millions of piece-derived feature records, sorted, then read
back in assemble. On norway, ocean is 59% of all sort records (16.9M of
28.7M) - piece-derived records flow through sort AND the serial assemble
reader. Redesign: an `OceanTileLayerStore` - per zoom, collect coverage per
tile, union compatible clipped rings per tile, emit one canonical ocean layer
payload per tile; full interior tiles share a canonical geometry (the dedup
counter already shows 15.5M of norway's 16.3M tiles are reused payloads).
Risks: adjacent pieces, holes/islands, winding, canonical output validation;
ocean feature IDs disappear (acceptable). Note: item 14 removes the serial
reader structurally and item 10 cuts the clip cost, which together absorb
most of this item's payoff at extract scale - re-price after P1/P3 land.
2026-07-19 update: the low-zoom half of this landed for correctness, not
perf - the z0-z7 pass unions its source pieces before descent
(OCEAN_POLICY_VERSION v3, `2ab6f83`), so low zooms now emit few merged
features; union cost measured at 69 ms denmark band / 1.03 s world. The
z8-z14 pass still emits per-piece features, so the sort-volume angle at
high zoom - the bulk of the 59% norway share - is unchanged and this
item's remaining scope is z8-z14 only. 2026-07-24: the re-price gate
(after P1/P3) is satisfied, and H5 demotes this further - an
artifact-active run emits ocean sort records only for the bbox boundary
band, so the 59% share applies to artifact-absent runs and to
`ocean-build` itself. Not scheduled.

### Item 23: durable/cached ocean tile source - LANDED as H5 (2026-07-12)

Landed as the world-ocean artifact (`elivagar ocean-build`, one shot per
shapefile release): the ocean phase is deleted from every artifact-active
run (NA 19.5s -> 0.2s). The risks named here became the design: cache
invalidation is the artifact key (shapefile hashes + OCEAN_POLICY_VERSION,
re-validated every run - the 07-15 stale-artifact incident is why the
version half exists), and geometry parity was human-adjudicated benign
with the corpus contract recording the artifact key. Full caveats and
incident history: roadmap H5.

### Item 11: memoize the canonical full-tile record

`emit_full_tile` re-encodes constant bytes per call: measured 0.13s DK /
1.7s NO thread-time. A drive-by inside a P1 or P4 landing, never its own
change.

---

## Parked (no evidence pressure, or blocked on the above)

- **Item 6 (per-row bitset for boundary tiles)**: CLOSED, moot - the P1
  descent deleted the boundary-tile set concept entirely.
- **Item 7 (build_graph_view multi-rule extraction)**: only pays off if a
  future design extracts multiple overlay rules per tile; no such design
  exists.
- **Item 8 (read_ring_points buffer / mmap reads)**: parse-path allocation in
  the ocean prologue; ocean phase is flat 11-16s and the prologue is not the
  long pole. The mmap version ("small but free") is a drive-by candidate when
  ocean.rs is next open. The `#[cfg(unix)]` `read_exact_at` portability note
  stands (Linux-only project, acceptable).
- **Item 13 (hoist zoom-independent OSM attr encoding)**: measured 1.0-2.1s
  thread-time - noise. Drive-by only.
- **Item 18 (parallelize relation prepare)**: DONE - landed after this
  note was written, as the H1 campaign's streamed relation tail (norway
  tail 31.7s -> 15.1s; roadmap H7 tier 1).
- **Item 20 (tile-owned polygon output)** and **item 21 (OSM polygon feature
  planner)**: the radical siblings of items 14 and 10 respectively. Both
  delete the feature-owned sort seam / per-match emission more aggressively.
  The re-price gate (after P1+P3) is now satisfied, and the answer is to
  stay parked: the reader and the bulk clip costs are gone, and neither
  item shows in the roadmap's current frontier (phase12 way-path/relation
  CPU, H8b). Revisit only if planet profiles reopen the question.
- **Item 24 (delay multi-zoom fanout to post-partition)**: item 14 landed,
  item 20 stays parked; the distinct payoff (topology-aware line
  simplification with tile context) is a quality feature as much as perf.
  Still parked; revisit against planet profiles.
- **Item 25 (pinned line DP cascade)**: line-layer analogue of the polygon
  cascade; no line layer shows up in the top profiles (emit_line_feature
  3.9-40s thread-time, wide spread but dominated by phase12 items). Parked.
- **Item 26 (PMTiles writer sharding + faster dedup fingerprint)**: half
  landed - the in-place writer (`bc71cf1`) made finalize rename-only and
  deleted the serial tail; central offset assignment with worker pwrite
  remains only if the writer-drain tail returns (roadmap H8c). The dedup
  half is now the H3 pricing item (the 1M cap's output-byte cost at
  planet), not a perf item.

---

## Non-targets (explicit, per report noted)

- Sort phase itself (0.02-0.6s at every scale measured) - the cost was never
  the sort, it was the reader (item 14, landed in P3) and the producers
  (item 16, landed in P2); both are gone.
- Micro-optimizing `simplify_shape_dp` / `rescale_shape` internals - the win
  is calling them on fragments (P1), not making them faster.
- Tuning `SPLIT_Z` / `SPLIT_MIN_VERTICES` / chunk sizes - knob-turning on a
  structure the P1 rewrite deletes.
- `decompress_chunk` / `find_chunk_in_blob` (187 thread-s on germany!) - the
  standard-path node store cost; the production pipeline is locations-on-ways,
  which deletes the node store entirely. Treat the standard path as
  legacy-adequate. (P2's selective-resolution landing shrinks it anyway as a
  side effect.)
- Do not micro-tune `match_element` (4.3-55 thread-s), `find_chunk_in_blob`,
  bitpacking, or the DP inner loops - prior attempts died on DRAM latency;
  none are structural.
- Do not touch seam-reconciliation or dedup machinery for perf; both are
  cheap in every profile (dedup reused 15.5M tiles on norway for free).
- RSS knobs: clean-bench RSS is 2.8/4.1/10.3 GB (DK/NO/DE) - comfortable,
  and the planet ledger (roadmap H3) now owns the budget question. mimalloc
  is gone (lost the 2026-07-15 A/B outright), and its purge knob with it.
  Never read RSS from hotpath/alloc runs (instrumentation-dominated; see
  reference/performance.md).
