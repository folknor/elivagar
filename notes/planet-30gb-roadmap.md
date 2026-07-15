# Planet on 30 GB: roadmap to world-record tile generation

Status: 2026-07-15. Trimmed this date: the blow-by-blow campaign logs
(the H1 phase12 restructure, the 07-14 RAM regression and its
resolution, the allocator A/Bs, the E1-E3 engine experiments, the H4/H5
landing narratives) are removed. The full narrative lives in this
file's git history and in the commit messages; durable numbers are in
`reference/performance.md`. What remains here is current state, open
work, and the verdicts that still bind decisions.

## The goal

Process a planet-scale PBF (90 GB+, locations-on-ways, pbfhogg-enriched)
into a correct PMTiles archive in world-record time on a host with 30 GB or
less RAM. Comfortably: no swap thrash, no OOM roulette, headroom for the OS
and page cache to function.

The production pipeline is pbfhogg preprocessing (cat -> merge ->
add-locations-to-ways) feeding two consumers: elivagar (PMTiles) and nidhogg
ingest (query API). pbfhogg has already proven the constraint is beatable:
planet-scale PBF processing on 30 GB RAM + 8 GB swap, 2x-600x faster than
comparable tools. elivagar is the remaining half of the story.

The only sacred thing is a correct PMTiles artifact. The standing gates
define correct: `elivagar verify`, the earcut oracle
(`scripts/validate/earcut-oracle.mjs`), and `elivagar corpus check` against
the committed `corpus/denmark/` baseline. Nothing in the current pipeline
structure is protected.

Companion: `notes/virtual-planet-serving.md` - the hypothesis that
production may never store a full planet archive at all (on-demand
generation + cache over the record store, elivagar as a library). The
full-build track in THIS document is unchanged by it: cold start,
disaster recovery, and the committed corpus baseline the serve path verifies
against. The two tracks share instruments (H2d/H8's per-tile-range index,
H3's ledger) and should not diverge on them.

## Where we stand (plantasjen, 2026-07-15, writer-layout commit)

Stored baselines, locations variants (the production input shape; full
sidecar detail in `reference/performance.md`): denmark 8.8s best-of-3,
germany 53.8s, north-america 251.0s with assemble 86.1s and the
post-worker finalize tail at 2.3s. The NA phase12 baseline is 154.7s at
18.5 cores / 5.14 GB peak anon / 11.3K majflt (hash-overlap commit; the
writer-layout run's 164.3s on untouched phase12 code is bench-1
variance). NA peak whole-run RSS 5.76 GB.

The march: NA 462.6s (March) -> 361.6s (07-09) -> 251.0s (07-15).
Germany 77.8s (07-08 suite) -> 53.8s. Denmark 14.4s -> 8.8s.

Current frontier: phase12 is the biggest phase at NA (154.7s vs
assemble's 86.1s), and its cost is way-path/relation CPU plus the
ordered-consumer residue - the paused H6 engine surface and H2c/d are
the known levers. Assemble's remaining ideas are H8b's recursive
splitting of hot partitions (germany-relevant) and reader/encode
overlap inside a worker (a reader blocks during its rayon encode).

## The record target

planetiler's published planet-log table
(`research/planetiler/README.md`, snapshot including a 2026-03 run):
best absolute 19 minutes on 192 cores / 720 GB (v0.10.1, 92 GB planet,
avg 117 cores busy); 29-42 minutes on 64 cores / 128 GB; **2h38m on
16 cpu / 32 GB** (v0.5.0); 3h35m on 8 cpu / 16 GB (v0.7.0). The record
this document targets is the resource-classed one - fastest planet on
<= 30 GB RAM - with the absolute number reported alongside, not
claimed. Comparison caveats: those runs use the OpenMapTiles profile,
not Shortbread, and our enriched-input framing must disclose
preprocessing (H10).

## Planet go/no-go (re-read 2026-07-15)

Calibrated on NA `52cc955a` at the hash-overlap commit - the first
stored baseline with the scratch fix, system allocator, H5 artifact and
hash overlap all in - plus the writer-layout landing after it. Fresh NA
constants: 75.0 MB/s end-to-end (19.06 GB input), phase12 8.1 s/GB,
ocean 0.2s (the artifact deletes the phase; at world bounds the
boundary band is empty by construction), assemble ~4.8 us/unique-tile
after the writer layout (18.0M unique tiles at NA).

Planet assumptions (unchanged from 07-09): 90 GB enriched input, ~5.3x
NA ways (1.1B vs 209M), unique tiles 3-4x NA at 60-70M.

- **RAM: GO, not conditional.** The old conditional hinged on ocean
  (extrapolating 12-17 GB) - that line no longer exists. Phase12
  stocks are bounded and input-independent (~6-7 GB with headroom;
  relation blocks spill at their 1 GB cap as designed, spill path
  verified). Assemble is claim-window bounded: 2 GiB park budget plus
  in-flight, observed 5.1 GB peak at NA with z7 max partitions of
  ~200 MB. Predicted planet peak stays under ~10 GB on a 27 GB host.
  Open risks, none load-bearing: the monster-relation transient
  (max_rel_inflight 90 MB at NA; watch it, a per-relation gate is the
  backstop), way_index page-cache competition, glibc retention
  (malloc_held 2.84 GB vs 26 MB live at germany run end - modest, now
  measurable).
- **WALL: ~19-24 min.** Phase12 8.1 s/GB x 90 GB ~ 730s; ocean ~0;
  assemble ~4.8 us x 60-70M unique ~ 290-340s plus the untested
  page-cache-overflow term on ~370 GB of merge reads (H4: lz4 is the
  planet configuration, priced at +2% NA wall for 2.6x less scratch
  I/O). The projected ~90s serial finalize tail is deleted by the
  in-place writer. Wide band honest: call it 1050-1400s. Even the
  pessimistic end is ~6x under planetiler's published 2h38m at
  16 cpu / 32 GB.
- **DISK: GO.** 607 GB free on Banan; lz4 halves scratch besides; the
  in-place writer means the output no longer exists twice (temp blob +
  archive) at finalize. data/scratch/target are ALL on NVMe
  (nvme1n1p1); ssd/hdd labels in old results rows are stale hardware
  provenance.

**What actually gates a planet attempt now, in order:**

1. The enriched planet PBF does not exist yet - a pbfhogg altw run,
   user-gated. H2's pending NA re-enrichment reading should ride the
   same decision.
2. The H9 step-3 disk audit against the real artifact size.
3. RESOLVED by the corpus gate (spec C): the committed `corpus/denmark/`
   baseline is in git and gateable at any commit. A planet run wants a green
   `corpus check` at the attempt commit before it starts. Planet-scale corpus
   blessing uses bucket mode; tier-3 overlays provide attribution.

## Standing policies and lessons

**THE PATTERN** (bitten three times in one day, 2026-07-09): unbounded
queue + ordered-or-slow consumer + straggler = input-scaled RAM.
Instances: pbfhogg's pipelined-read reorder window (20 GB), elivagar's
drain result funnel (141s of blocked senders), assemble's
pending-partition map (19.5 GB parked behind a dense straggler). Every
queue between a parallel producer and an ordered consumer needs an
explicit window or byte bound, decided at design time, with a wait
counter on the bound.

**THE SIBLING PATTERN** (bitten five times as of 2026-07-15): warm
scratch whose lifetime exceeds its work item = capacity ratcheted to
the worst item it ever served, times the pool width. Instances: the
per-thread AssemblyScratch pool, the ocean PyramidScratch pool, E3's
rect-clip pool (reverted), and the 07-14 planet-RAM NO-GO - the phase12
way-acc pool (24 accs x ~770 MB at NA) plus the relation tail's fold
accumulators and their reduce-queue copies. The i_overlay port created
the exposure: it moved the engine's per-op allocations into
caller-owned scratch, so any accumulator holding that scratch became a
ratchet. Scratch lives exactly as long as its work item (task,
relation, tile); reuse beyond that must prove its wall win against the
retention it buys. Gated "mostly reuse" variants were tried and lost to
the aggregate; the unconditional drop cost zero measurable wall.

**Gate policy**: the corpus check runs on DENMARK ONLY for routine landings;
germany/NA archives get checked only at corpus rotations (their corpora are
optional and currently absent).

**Rotate the corpus in the landing commit** after an accepted output-changing
landing: `corpus bless --rotate` rewrites digest, contract, and manifest, and
the commit diff is the review. The baseline lives in git and survives archive
rotation by construction.

**Bit-identity gates** use `cmp -s` over complete deterministic
archives, never `brokkr compare-tiles` (samples 200 tiles/zoom,
compares aggregate counts, proves nothing about bytes).

**Measurement discipline for NA-and-larger inputs:** `--bench 1`, not
best-of-3 (an NA best-of-3 is ~18 minutes of machine time for variance
data the ladder does not need). Best-of-3 stays standard for
extract-scale runs and for record-claim runs (H10). Instrumented
(hotpath/alloc) builds OOM at germany scale and above on this host:
the planet instruments are the sidecar (OOM-surviving by design) and
sampling profilers (`perf record`). Note for bisects: `brokkr tilegen
--commit <hash>` cannot straddle the ocean-CLI rework commit
(`38250b9`) - it builds old code but passes one flag dialect.

## Hypotheses

### H1: Phase12 ordered-drain removal - LANDED 2026-07-08..09

The claim held: phase12's output is sort records and the external sort
erases emission order, so the inherited ordered-callback shape was
pure tax. The campaign (three shards: plan build moved task-side,
streamed relation tail, pooled way accs self-flushing through the
SpillCoalescer with the drain reduced to way_index puts) plus the
follow-ups (task-side way counting, dedicated node workers, the
relation-block buffer capped at 1 GB with a verified BlobFilter
re-read spill) took NA phase12 from 283s to ~155s and germany/norway
wall down 26-35%. Locations mode now reads via UnorderedBlockSource
(one reader thread, N decode workers, bounded channels, NO reorder
buffer); the raw path stays on the ordered reader for the sorted node
store. Full shard-by-shard history: git log 1ad1d66..69c0f18 and the
H1 sections in this file's history.

### H2: pbfhogg enrichment as elivagar's free prepass

(a) injected relation plan and (b) exact shared-node pins LANDED
2026-07-11 (paired pbfhogg change, their `29e4eabd`): the relation-plan
prepass runs only on non-enriched input, the global shared-node prepass
is deleted, exact pins read from the injected per-way bitmap. Fallbacks
(block-local pins, runtime relation plan) remain first-class for raw
input.

Open: **(c) Shortbread relevance masks** - altw marks blobs/elements
that cannot match any layer; bounded win since post-altw node blobs are
tagged-only, price with a shadow counter first. **(d) partition
calibration** - per-blob way/vertex/bbox stats injected at altw time so
partition boundaries and budgets come from the header walk instead of
hardcoded Hilbert prefixes; direct attack on straggler skew (H8b).

**PENDING: the NA re-enrichment reading** (measurement only, no code;
user-gated, rides the planet-PBF enrichment decision): re-enrich
north-america locations with the new altw (`pbfhogg
add-locations-to-ways --index-type external --inject-prepass
--compression zlib:6`), register in brokkr.toml, then one `brokkr
tilegen --bench --dataset north-america --variant locations` plus
sidecar readings (`--human`, `--stalls`, `--counters`). Numbers that
feed the H3 ledger: phase12 s/GB with the prepass deleted,
`way_index_data_bytes` under superset membership at NA scale,
`way_members_marked` vs the old `relation_plan_needed_ways`, peak RSS
with the `needed_ways` stock gone. Priced 2026-07-15 (NA `52cc955a`):
the runtime prepass costs 14.7s of `prepass_join` wait at NA, 5.8% of
wall - the NA input predates prepass injection (provenance shows
`way members relation_scan, pins block_local`), so the re-enrichment
buys a measured ~15s at NA and proportionally more at planet.

### H3: The planet RAM ledger

Counters landed 2026-07-08; the ledger is current and the go/no-go
above reads from it. Bounded stocks: sort chunk buffer 1 GB
(--sort-budget), way accs die with their block task, relation blocks
capped 1 GB with verified spill, prepared relations one per worker,
PMTiles writer streaming directory + dedup map capped at 1M entries
(~60 MB), way_index mmap'd/disk-backed (~7 GB at planet, page-cache
pressure not RSS).

NA validation facts: relation buffer 236 MB (no spill at ~1/5 of
planet relations), dedup cap skipped 17.0M inserts while still reusing
101.8M tiles (6.2 GB saved), 20.3M dir entries streamed fine,
way_index 6.45M member ways / 1.37 GB data + 103 MB index,
max_rel_inflight 90 MB (the monster-relation signal to watch).

Open pricing item: at planet the 1M dedup cap costs output bytes
(missed dedup), not RAM - price before H10 record runs.

History note: the 07-14 planet-RAM NO-GO (NA phase12 at 23.3 GB / 1.9M
majflt) was scratch-capacity retention from the i_overlay port - THE
SIBLING PATTERN above - not the allocator; the A/B refuted the
retention theory first and the fix restored 5.6 GB anon with output
byte-identical. Full forensics in this file's git history and the fix
commit message (`98824b4`).

### H4: Sort scratch I/O - PRICED, verdicts standing

- **LZ4 chunks are the planet-run configuration**; extract-scale
  default stays uncompressed until a record run cares. Priced at NA:
  +2.0% wall (compression CPU), phase12 physical writes 60.1 ->
  26.3 GB, assemble reads 79.3 -> 30.8 GB, scratch on disk ~2.6x
  smaller. At planet, where ~370 GB of merge reads overflow the page
  cache, the trade should invert in lz4's favor - untested, the one
  open H4 term in the go/no-go wall band.
- The compressed chunk format is per-section frames inside the
  multi-section chunk file (seek+stream-decode per partition section);
  per-compression magics make a `--skip-to` resume with a flipped flag
  fail loud.
- **PARTITION_SPLIT_Z is 7, kept deliberately**: germany assemble
  -31% (one z6 partition had encoded 921 MB behind a single
  claim-window slot); NA paid +1.6% in per-partition fixed overhead
  (52K partition opens). Planet contains Europe, so the quartered
  straggler tail is insurance worth NA's cost. Claw-back candidate if
  it matters: a per-chunk file-handle cache (pread per section instead
  of open+seek per section).
- Defaults promoted: 8 assemble workers, 2 GiB byte-budgeted claim
  window (ELIVAGAR_ASSEMBLE_WORKERS / _PARK_BUDGET override). Way
  budget 768M stands; the binding constraint there is way-stage CPU,
  not bytes. fadvise(DONTNEED) hygiene remains unpriced and cheap if
  planet merge reads misbehave.

### H5: Ocean as a durable precomputed tile stream - LANDED 2026-07-12

The world artifact exists (942.7 MB, 212.4M addressed tiles, 9.2M
unique blobs, 95.7% deduplicated, verify + earcut clean in the
run-aware gate modes) and deleted the ocean phase from every run: NA
ocean 19.5s -> 0.2s, and the planet ocean RSS line (est. 12-17 GB) is
gone. Extracts compute only the boundary band near the bbox edge;
assemble merges the artifact for the interior. Spec rationale and
landing history: git history (the spec note was deleted after
implementation).

Standing caveats: the artifact is used because it is NAMED, never
found (see AGENTS.md); artifact output differs benignly from
extract-computed output (descent seams depend on the piece clip
extent; human-adjudicated equivalent 2026-07-12), so the committed corpus
baseline is artifact-active, its contract records the artifact key, and a
shapefile-release rotation forces a corpus rotation.

**INCIDENT 2026-07-15: the artifact served pre-VW geometry for three
days.** The VW landing (`31b8298`, 07-12 21:58) explicitly deferred
"an ocean-build artifact rebuild and a bless rotation"; the rebuild
never happened, and OCEAN_POLICY_VERSION stayed at 1, so the DP-era
artifact (built after `92ed329`, 07-12 12:39, predating even the
provenance block) kept key-validating. Every artifact-active archive
since served the pre-VW coastline spikes worldwide - at z5/16/9 the
spiked ring is vertex-identical to the 07-09 pre-VW computed output -
while every standing gate stayed green: regress was un-gateable
against blessed, the 07-15 landings gated against each other and all
shared the stale artifact, earcut cannot see a spike, and the one
clean archive (`ec5bd11`) was clean only because the 07-14
mis-blessing built it artifact-absent. Found by human visual
inspection (a spike at z5 16 9, another at z5 17 9). Resolution:
OCEAN_POLICY_VERSION bumped to 2 so a stale artifact now fails loud
at the key check; artifact rebuilt with VW active; corpus rotation to
follow visual verification. Gate lessons: (a) the needle-detector
candidate recorded below would NOT have fired on this - the defect is
a cross-feature coverage gap (the north cell's simplified chord
against the south cell's edge fill), so the candidate must be
re-scoped to seam gaps between adjacent features; (b) the svg-corpus
text-diff (notes/svg-corpus-plan.md) would have flagged both tiles
mechanically - this incident is that plan's strongest concrete
argument yet. Triage tooling from the hunt: `scripts/validate/
svg-roi.mjs` extracts the edges inside a bbox ROI from `brokkr svg
-o` dumps, comparing one defect region across archives without
reading whole tiles.

**OPEN, UNGATED: the latent cross-piece ocean seam.** `ocean.rs` sets
`pins: None`, so the only simplification pins come from
`build_edge_flags` (current cell's tile-edge window plus
`params.pins`). A boundary genuinely SHARED between two ocean source
pieces, away from tile edges, is simplified independently on each side
and can open a seam. The ocean shapefile path is the one polygon
producer with shared edges and no pin source - contrast the OSM path's
shared-node pins. The machinery exists unused
(`quantize_polygon_pinned_into`, `PyramidParams.pins`). Never observed
in the wild, and no standing gate would catch it (earcut cannot: a
seam is a vertex in the wrong place, not a self-intersection). The
connected-component render gate was REFUTED on calibration 2026-07-14
(separation topped out at 3.29x against the 4x the threshold math
needs) - rendered-area measures cannot separate an ocean defect from
legitimate generalization, the same failure as the coverage oracle. If
a geometry-level ocean gate is wanted, the recorded candidate is a
baseline-free needle detector on the emitted ring, analogous to the
boundary oracle's spur detector: flag point pairs a sub-pixel
straight-line distance apart but a long path-length apart, excursion
on the land side. Categorical, no REF build, satisfies the oracle
discipline in AGENTS.md.

### H6: Way-path churn and the engine surface

The de-churn campaign landed across 07-09..10: simplify_shape_dp
rewritten onto DpScratch (-94% exclusive churn), quantize scratch
recycling, and the i_overlay port - the boolean engine now lives
in-tree (`src/geometry/overlay/`), i32-monomorphized and
scratch-owned, with i_overlay surviving only as the dev-dependency
differential oracle (2,000 cases). Port history: git log
d570daa..659a187 plus the Landing 2 commits.

The allocator question is CLOSED: mimalloc removed 2026-07-15 after
losing the post-fix A/B on wall outright (NA 269s vs 282s, germany 60s
vs 65s, a third of the minor faults; jemalloc at par with sys-alloc
and not worth its dependency). The system allocator is a decided
convention (see AGENTS.md); `mallinfo2` is a live signal again
(malloc_held/malloc_live per phase boundary; pre-07-15 sidecar rows
carry mi_commit_* instead).

**The engine de-churn surface (E-items) is PAUSED**: none of it
outranks open phase-level work on planet leverage. Closed experiments,
kept as don't-redo records (measured detail in
`reference/performance.md`):

- **E1/E1b CLOSED 2026-07-13, below threshold.** The strict-convex
  normalize screen passed only 0.272 of single-contour calls (0.30
  threshold) against a 0.974 perfect-return ceiling; the exact
  classifier shadowed at -24.9s net on norway (38.6s classifier cost
  vs 13.7s avoided solver time, zero unsound accepts). Instruments
  removed; `is_perfect_ccw_convex` + soundness tests retained under
  `cfg(test)` in `src/geometry/overlay/port/simplify.rs`.
- **E2 (flat point+range at the module boundary) DESIGN-REFUTED
  2026-07-13** as a boundary-only prototype: every production caller
  recycles `out` pre-call, so the seam converter ADDS a copy, and
  `FlatShapes::clear` retains capacities on top of the still-needed
  nested pools. Flat storage returns only as E5 - delete the seam so
  the flat buffer REPLACES the nested pools - instrumented on
  copied-point bytes, pool-probe CPU and retained bytes, never alloc
  attribution.
- **E3 (pooled rect clip) IMPLEMENTED, REGRESSIVE, REVERTED
  2026-07-13.** Won the churn gate (-82-87%) but lost wall (+6.7%) and
  RSS (+23%) to per-worker pool retention - the sibling pattern again.
  The 19 GB churn surface is real; any revisit is an E3b with a
  bounded retained-byte budget specified BEFORE implementation,
  starting from the pre-E3 code state.
- **E4 open ideas:** (a) iterator-fed segments (producers feed
  `add_contour` directly instead of materializing a Shape;
  bit-identical gate), (b) rect-specialized boolean (churn/code-size
  play, only 6,550 calls on denmark), (c) fuse the guarded integer S-H
  fast path with the boolean - CHANGES BITS (nods differ by up to one
  unit along the cut line), corpus-rotation territory.
- **E5: caller-provided buffers end to end** - the H6 endgame
  (remaining sinks: add_feature_to_layer ~5 GB assemble-side,
  merge_same_attr_geometries ~3.8 GB); do it after evidence shows
  where the remaining bytes are.
- **E6: the port made the knobs ours** - solver thresholds (4,000 /
  16,000 / 8,000) are upstream's generic tuning against our bimodal
  distribution, and the i32/i64 arithmetic admits vectorization
  without changing results. Both cheap to price (one norway hotpath
  run per variant).

### H7: The relation stack

Tier 1 (parallelize relation prep) landed in the H1 campaign as the
streamed relation tail (norway tail 31.7s -> 15.1s); tier 2 is H2a,
landed. Tier 3 - precompute ring-assembly adjacency at altw time -
remains unpriced; revisit only if planet profiles show the
relation/multipolygon stack bleeding after the enriched-input run.
Fanout caps (`--fanout-cap`) remain the policy backstop for
pathological features.

### H8: Phase overlap and assemble stragglers

- (a) Run ocean concurrently with phase12: DEAD - the H5 artifact took
  ocean to ~0.2s; there is nothing left to hide.
- (b) Straggler mitigation: the NA half is DONE (z7 partitions + byte
  window + the artifact; NA assemble 91.5s -> 86.1s, no straggler
  campaign remains there). The GERMANY half is live and unchanged:
  assemble averages 12.3 cores vs norway's 20.0; **recursive splitting
  of hot partitions is still untried** and H2d's injected stats would
  pick the boundaries. Do not read the 12.3 as an NA number.
- (c) Workers pwrite tile payloads at final offsets: the in-place
  writer layout landed the single-write property (finalize is
  rename-only); central offset assignment with worker pwrite remains
  the further idea if the writer-drain tail ever shows up again.

### H9: Planet measurement is its own workstream

The ladder: (1) NA locations re-baseline - DONE, multiple times; the
current constants are the go/no-go's. (2) `perf record` sampling for
anything bigger than norway - hotpath/alloc builds OOM there. (3)
Planet enriched artifact + disk audit (scratch ~100-130 GB with lz4 +
output ~60-70 GB + input ~90 GB against 607 GB free) - pending the
artifact existing. (4) First planet run only after the ledger predicts
peak RSS with measured constants - satisfied at ~<10 GB predicted; a
fresh NA stored run should confirm after any structural landing.

### H10: Define the record so the claim survives scrutiny

Publish, per dataset (germany, NA, planet): host spec, input hash
(brokkr.toml carries XXH128), exact invocations, best-of-3 wall, peak
RSS, and output-correctness evidence (verify + earcut oracle +
regress). Compare on the same host via the existing `brokkr planetiler`
and `brokkr tilemaker` benches, in two framings:

- **Enriched framing:** elivagar on the locations-on-ways input (the
  production shape). Fastest number; must disclose preprocessing.
- **Raw framing:** pbfhogg (cat + altw) + elivagar total wall vs
  planetiler/tilemaker on the same raw PBF. The apples-to-apples
  number - planetiler resolves nodes internally, so our node-handling
  cost must appear somewhere in the comparison.

Same-host competitor reference points are stale; re-run the competitor
benches when a claim is made, not before. For planet-scale claims the
external reference is planetiler's published table (see The record
target above); the claimable record is the resource-classed one
(<= 30 GB RAM), wall against the big-iron numbers reported for
context, profile differences (OpenMapTiles vs Shortbread) disclosed.

## Sequencing

What gates a planet attempt is the go/no-go list above: the enriched
planet PBF (user-gated pbfhogg altw run, with H2's NA re-enrichment
reading riding the same decision), the disk audit against the real
artifact size, and a green `corpus check` at the attempt commit.

Optimization work that remains, none of it blocking: H8b recursive
partition splitting (germany), reader/encode overlap in assemble
workers, H2c/d injection candidates, the paused E-surface, and the
ocean needle detector if a geometry-level ocean gate is ever wanted.

## Open questions the first planet measurements must answer

- Actual enriched-planet PBF size and its blob-type byte split (pbfhogg
  can answer from an existing planet artifact without elivagar running).
- Planet unique-tile count and PMTiles directory entry volume - the
  streaming directory is bounded, but the 1M dedup cap's cost in
  output bytes needs pricing before a record run (H3).
- Whether the lz4 CPU-vs-I/O trade inverts as predicted when merge
  reads overflow the page cache (H4).
- Whether tile serving requirements (gzip vs brotli, MVT vs MLT) change
  the assemble CPU budget the record is computed against.
