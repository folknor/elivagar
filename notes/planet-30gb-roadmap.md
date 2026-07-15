# Planet on 30 GB: hypotheses toward world-record tile generation

Status: 2026-07-15. No longer hypotheses-only - H1, H5 and most of the
RAM campaign have landed; each hypothesis carries its own dated landing
and verdict notes below.

**Start here: the 2026-07-14 planet-RAM NO-GO is RESOLVED.** The phase12
memory regression was not the allocator: it was scratch-capacity
retention in phase-lifetime accumulators, introduced by the i_overlay
port making buffers scratch-owned (the fourth instance of THE PATTERN,
see H1). The fix (way accs die with their task; relation scratches drop
per relation; fold accumulators finalized before queuing for reduce)
restores NA phase12 to 5.6 GB anon / 12K majflt / 170s - the 69c0f18
numbers - with tile output byte-identical. The allocator A/B that the
07-14 notice called for was run first and REFUTED the retention theory:
sys-alloc holds the same ~21 GB live and pays 3.8M majflt for it.
Germany's `mi_commit` doubling is the one piece the fix does not touch
(14.2 GB before and after against 4.2 GB anon): that is mimalloc arena
commitment under churn, and it belongs to the mimalloc rip-out decision
(H6 allocator addendum), not to a pipeline defect.

Dated notes elsewhere in this document describe the state on their own
date, not today's.

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
(`scripts/validate/earcut-oracle.mjs`), and `brokkr regress` against the
blessed archive. Nothing in the current pipeline structure is protected.

Companion: `notes/virtual-planet-serving.md` - the hypothesis that
production may never store a full planet archive at all (on-demand
generation + cache over the record store, elivagar as a library). The
full-build track in THIS document is unchanged by it: cold start,
disaster recovery, and the blessed baseline the serve path verifies
against. The two tracks share instruments (H2d/H8's per-tile-range index,
H3's ledger) and should not diverge on them.

## Where we stand (plantasjen, `11dc159`, 2026-07-08 suite)

plantasjen is itself a ~30 GB host (27.4 GB visible to the benchmark
harness), so every number below is already a 30 GB-class measurement.
Locations variants are the production input shape.

| dataset | variant | run | wall | phase12 | ocean | assemble | peak RSS |
|---|---|---|---|---|---|---|---|
| denmark | locations | `a3782b12` | 14.4s | 6.1s | 5.5s | 2.7s | 3.0 GB |
| denmark | raw | `4e7e4669` | 19.0s | - | - | - | - |
| norway | locations | `759864a2` | 67.9s | 43.6s | 7.9s | 16.1s | 5.0 GB |
| norway | raw | `10ea82d5` | 90.1s | - | - | - | - |
| germany | locations | `92803833` | 77.8s | 55.3s | 4.8s | 17.4s | 8.8 GB |
| germany | raw | `5b462a69` | 123.7s | - | - | - | - |

Germany locations detail (`92803833` sidecar): 156.2M features, 12.3 GB
sort-record bytes in 46 chunks, 234 assemble partitions, 300K unique tiles,
2.81 GB output. Locations-on-ways beats raw by 24-37% everywhere; the raw
node-store path is legacy-adequate and not the record path.

P0-P3 of `notes/performance-backlog.md` have all landed (prepass overlap,
pyramid descent, phase12 ownership rewrite, partitioned sort + parallel
assemble). Germany went 255.9s -> 123.7s raw across the 2026-07-06..07-07
campaigns. This document is about what comes after the backlog.

### What the fresh profiles say

1. **Phase12 is consumer-bound, and it is THE phase.** Phase12 is 70% of
   germany wall, 64% of norway. The dominant stall everywhere is
   `pipeline_decoded_send`: 335.1s cumulative on germany locations (432% of
   wall - on average ~4.3 decode workers blocked handing decoded blocks to
   the serial ordered callback), 89.9s on norway (133%). Average cores
   during phase12: 7.5-9.1 of 24. The machine is more than half idle in the
   phase that dominates wall.

2. **Assemble still leaves cores on the table.** Post-P3, germany assemble
   averages 12.3 cores; `assemble_partition_batch` wait is 18.6% of wall,
   partition reader max 10.2s vs 39.3s total across 4 workers (straggler
   skew). Norway assemble averages 20.0 cores - better.

3. **Allocation churn is enormous.** Germany locations alloc run
   (`45065b2f`): 621.6 GB allocated / 605.2 GB freed over a 142s
   instrumented wall, ~4.4 GB/s of churn. Top exclusive sinks:
   `emit_polygon_feature` 68.5 GB, `simplify_shape_dp` 50.4 GB,
   `process_planned_way_into` 31.4 GB, `normalize_into` 19.0 GB. The P1
   descent flattened the ocean engine; the way-path emit chain still churns
   per call.

4. **The relation/multipolygon stack owns coastal geographies.** Norway
   locations hotpath (`46ec61ba`): `process_prepared_relation_into` 267.8
   thread-s, `emit_multipolygon_feature` 243.9, `normalize_into` 129.2,
   `simplify_shape_dp` 81.6 (hotpath ranks, not absolutes). Planet has
   every fjord, lake, and admin boundary on earth.

5. **Instrumented modes already OOM at germany scale on 30 GB.** Four of
   the 18 overnight runs were killed: norway raw alloc, germany raw
   hotpath + alloc, germany locations hotpath. Clean bench runs are
   comfortable (8.8 GB peak). Consequence: planet-scale profiling cannot
   use hotpath/alloc builds; the sidecar (OOM-surviving by design) plus
   sampling profilers are the planet instruments. (Re-run on `9e8dce2`,
   2026-07-08 afternoon: germany locations alloc ALSO died - five kills -
   so germany-scale instrumented builds are now fully off the table on
   this host.)

6. **The enriched PBF already deletes most node work.** Germany locations
   processes 21.1M nodes vs 429.2M raw - add-locations-to-ways drops
   untagged nodes, and the node store is gone entirely. pbfhogg's blob
   filter already skips 62,313 of ~187K blobs on the prepass reads
   (`BlobFilter::only_ways` / `only_relations` against injected indexdata).
   The metadata-injection channel exists and works; it is just barely used.

## The planet model: what the extrapolation says, and what breaks

Naive linear scaling from germany locations (5.54 GB in 77.8s, ~71 MB/s of
enriched PBF end-to-end; phase12 alone ~100 MB/s): a 90 GB enriched planet
would take **~21 minutes** if nothing broke.

Calibrate that against the field (planetiler's published planet-log table,
`research/planetiler/README.md`, snapshot including a 2026-03 run): their
best absolute number is **19 minutes on 192 cores / 720 GB RAM**
(v0.10.1, 92 GB planet, avg 117 cores busy), 29-42 minutes on
64 cores / 128 GB. So ~21 minutes on 12 cores / 30 GB would NOT beat the
absolute wall-clock record - that record is bought with 24x our RAM and
16x our cores. In our hardware class, though, their published numbers are
**2h38m on 16 cpu / 32 GB** (v0.5.0) and **3h35m on 8 cpu / 16 GB**
(v0.7.0): the extrapolated 21 minutes would be ~7x faster than anything
published at 30 GB-class memory. The record this document targets is
therefore the resource-classed one - fastest planet on <= 30 GB RAM -
with the absolute number reported alongside, not claimed. (Comparison
caveats: those runs use the OpenMapTiles profile, not Shortbread, and our
enriched-input framing must disclose preprocessing - see H10.)

So the plan has two halves: (a) remove the things
that break at planet scale, (b) then make the linear model itself faster.
The extrapolation is a hypothesis, not a plan of record - germany is
unusually way-dense and has almost no coastline; planet mixes both
stresses plus oceans of empty tiles.

Known and suspected planet-scale breakage, in rough order of certainty:

- **Sort scratch vs page cache.** Germany writes 12.3 GB of sort records
  and reads ~14.3 GB back (`sort_merge_bytes`). Linear planet estimate:
  ~200 GB written + read through a 30 GB host. The page cache cannot hold
  it; chunk writes will evict PBF readahead and partition prefetch, and
  every merge read becomes a real disk read. Chunks default to
  uncompressed (`ChunkCompression::None`; LZ4/Snappy plumbing exists).
- **Ocean stops being flat.** Extract oceans are clipped to a bbox
  (germany hits 448 of 28,883 shapes). Planet hits everything: all
  coastlines at z8-z14 plus full-tile fills for two thirds of the earth.
  Norway already shows ocean at 59% of sort records on a coastal extract
  (16.7M ocean features). Planet ocean volume plausibly rivals the OSM
  feature volume - for input data that changes only when the shapefile
  release changes.
- **RAM structures that scale with the planet.** Candidates to audit (H3):
  relation block buffering + prepared relations (germany buffers 111
  blocks; planet has ~10M+ relations), the pbfhogg reorder buffer under
  consumer-bound backpressure (`pipeline_reorder_high_water` hit 847
  blobs on germany vs 23 early - decoded blobs, so potentially GBs; is it
  byte-bounded?), way_index for relation members (mmap'd, fine, but
  competes for page cache), and mimalloc retention (`mi_commit` 7.4 GB at
  germany phase12 end while RSS had dropped to ~1.9 GB). The PMTiles
  writer, initially feared here, turns out already engineered for this:
  the default path streams directory entries to a temp file
  (DirStore::Streaming, O(1) memory) and caps the dedup map at 1M
  entries / ~60 MB - audit confirmed 2026-07-08, ledger counters added.
- **Straggler partitions.** Tokyo/NYC-density partitions become the
  assemble critical path (already visible as 10.2s max vs ~9.8s mean on
  germany's 234 partitions).

## Hypotheses

Each: claim, evidence, theory, and the first (cheap, usually
measurement-only) step. Ordered by expected leverage on the goal.

### H1: Phase12 does not need an ordered serial drain at all

**Claim.** The single biggest structural win left. Phase12 inherits
pbfhogg's ordered-callback semantics (`for_each_pipelined`: out-of-order
decode, ordered drain). But elivagar's phase12 output is sort records - the
external sort erases emission order by construction. The ordering
requirement is not elivagar's; it is an artifact of the borrowed reader
shape.

**Evidence.** `pipeline_decoded_send` 432% of germany wall / 133% norway;
7.5-9.1 avg cores in the dominant phase. The consumer is the bottleneck of
the bottleneck.

**Theory.** What actually needs order or serialization in phase12 today is
small and enumerable: the node -> way -> relation phase barriers, way_index
writes for relation members, relation block buffering, and the sort
writer's chunk bookkeeping. Feature emission itself (the bulk of the work)
is order-free. If workers classify, resolve, emit, and flush per-worker
`RecordSink` arenas directly to per-partition chunk files - with the drain
reduced to barrier bookkeeping and way_index appends, or deleted in favor
of a small amount of synchronization - phase12 approaches decode-limited
throughput. pbfhogg's planet-scale decode rates suggest the ceiling is far
above 100 MB/s. Expected effect: up to ~2x on phase12, which is ~70% of
wall; the largest single lever on the extrapolated planet number.

**First step.** Instrument, no behavior change: a drain-thread
busy-vs-blocked counter pair and a per-callback-stage time split
(way-task drain vs sort push vs way_index put vs relation buffering).
If the drain's busy fraction is low, the theory is wrong and dies cheaply;
if high, the counter names exactly which serial work to shard. (pbfhogg
lesson 20: shadow-measure before restructuring.)

LANDED 2026-07-08. The survey found three serial actors, not one, and
the counters split all of them: busy - `phase12_node_blocks_ns` (ordered
consumer, node blocks inline), `phase12_way_count_ns` (consumer, way
recount), `phase12_plan_build_ns` (worker thread, serial
build_way_plans), `phase12_drain_ns` (drain thread),
`phase12_relation_tail_ns` (serial relation tail); stalls -
`way_block_send_wait_ns` (consumer blocked on worker),
`way_budget_wait_ns` (worker blocked on inflight byte budget),
`way_result_send_wait_ns` (rayon tasks blocked on drain),
`prepass_join_wait_ns` (consumer blocked joining prepasses). Together
with pbfhogg's `pipeline_decoded_recv/send` waits these close the
phase12 accounting; the next measured run reads the verdict.

VERDICT READ 2026-07-08 (overnight suite on `9e8dce2`, germany locations
bench `c51205b3`, norway locations bench `63eade98`). Confirmed, with a
different decomposition than "delete the ordered drain":

- Germany locations (wall 79.3s, phase12 54.3s): the serial chain
  accounts for the phase almost exactly - consumer inline 13.2s
  (node blocks 8.4s + way recount 4.8s) + serial `build_way_plans`
  worker 26.9s + serial relation tail 9.6s = 49.7s of 54.3s. The
  dominant serial actor is PLAN BUILD at 26.9s (half the phase, one
  thread); the consumer corroborates from the other side with 31.3s
  blocked on `way_block_send` waiting for it. The drain thread is busy
  only 14.6s - drain removal alone would not have fixed germany.
- Norway locations (wall 67.2s, phase12 43.6s): completely different
  shape - the serial RELATION TAIL is 31.7s, 73% of phase12. Plan
  build 8.1s, drain 2.7s, consumer inline 2.5s. On relation-heavy
  coastal data the lever is H7 tier 1 (parallelize relation prep,
  backlog item 18), and it dwarfs anything the drain buys.
- Oddity to check during the campaign: germany rayon tasks accumulated
  112s (141% of wall) blocked on `way_result_send` while the drain was
  mostly idle - smells like bounded-channel burstiness, cheap capacity
  tuning, not drain CPU.
- `prepass_join_wait` ~0 on both datasets - the prepass overlap is
  genuinely free.

Consequence for sequencing: the H1+H6 campaign is three shards with
measured prices - (1) parallelize/pipeline plan build (26.9s germany),
(2) parallelize the relation tail (31.7s norway; merges with H7
item 18), (3) drain/channel plumbing (small). Planet has both stresses,
so the campaign needs shards 1 and 2 both; "up to ~2x on phase12"
still looks reachable via this decomposition.

CAMPAIGN LANDED 2026-07-08 evening (commits 1ad1d66..e6f5fad), all
gates green (brokkr check + `elivagar regress` vs the blessed
9e8dce2 archives: 0 diffs on norway 13.9M tiles and germany 827K):

- Shard 2 first: streamed relation tail (`process_relation_blocks`,
  par_bridge + single end barrier, replacing 728 per-batch rayon
  barriers each idling the pool on its straggler). Norway tail
  31.7s -> 15.1s. First landing regressed peak RSS 5.2 -> 12.1 GB
  (tail-scoped accs flushing at the 1G chunk budget); capped per-acc
  flush at 32M -> RSS 4.96 GB, BELOW baseline. --rel-budget deleted
  (in-flight is one prepared relation per worker by construction).
- Shard 1: build_way_plans moved off the worker stage into the rayon
  tasks (the consumer's way_block_send 31.3s was queueing behind that
  serial loop). Budget reservation split: block bytes pre-spawn, plan
  bytes added task-side without waiting (condvar wait inside a rayon
  task can deadlock the pool).
- The choke then moved to the feed: way_budget wait 31.9s at the 256M
  locations default (~25MB raw in flight for a 24-thread pool). Raised
  to 768M: germany 70.1 -> 64.6s, cores 10.7 -> 13.6.
- Shard 3 turned out structural, not plumbing: ALL record volume
  funneled through the drain's serial sort_writer push (12.3 GB on
  germany; way_result_send 141s cumulative). Way accs now pooled
  across block tasks, self-flushing partitioned chunks at 64M; the
  drain handles only way_index puts (878K member ways). way_result_send
  141s -> 0.4s.

Scoreboard (locations, best sidecar run): norway 67.2s -> 48.4s
(-28%, at 64bdee1, before shards 1/3 - re-measure), germany
79.3s -> 57.4s (-28%), phase12 54.3 -> 35.3s, avg cores 9.2 -> 16.5.
Peak RSS: germany 8.8 -> 8.5 GB (pooled accs add a worker-scaled
~1.6 GB inside phase12 - bounded by workers x 64M, input-independent).
Sort chunks 46 -> 247, merge fan-in 28 -> 229 with assemble reader
time unchanged - fine at extract scale, but planet-scale flush sizing
belongs to H4 (200 GB of scratch at 64M flushes = thousands of chunks).
Remaining phase12 stalls: pipeline_decoded_send still 362% (the
ordered consumer itself is now the frontier again: node blocks 8s +
way recount 5.7s serial), way_budget 32.6% even at 768M.

CONTINUED same evening (commits 0513cfa..899f436):

- Way counting moved task-side (plans.len()); the consumer no longer
  re-parses way blocks (5.7s serial gone).
- Node blocks moved to a dedicated ordered node-worker thread that owns
  node_store + sort_writer for the node phase. Consumer now only
  classifies and forwards. Germany wall unchanged: node_block_send
  wait 8.0s shows the single node worker is the node-phase rate
  limiter now - the serial work moved threads but did not shrink.
  Next lever there: parallelize tagged-node processing (locations mode
  has no node store, embarrassingly parallel, same pooled-acc pattern).
- Relation-block buffer CAPPED at 1G decompressed (H3 ledger item
  closed): past the cap the tail re-reads relation blobs via
  BlobFilter::only_relations instead of holding them - the largest
  input-scaled RAM stock is now bounded. Spill path forced on denmark
  via ELIVAGAR_REL_BLOCKS_CAP=1 and regress-verified bit-identical.

Scoreboard at d9351df: denmark 13.7s, norway 44.2s (-35% vs overnight
baseline), germany 57.4s (-26%), peak RSS 5.0/8.5 GB. Gates: denmark
regress clean per landing; norway+germany regress clean at 64bdee1 and
e6f5fad respectively (heavy regress reserved for campaign milestones).
Denmark's largest phase is now OCEAN (5.6s of 13.7) - H8a's overlap is
the next denmark-visible lever; germany/norway frontier is way-phase
CPU (H6 churn) and assemble stragglers (H8b).

NA LOCATIONS RE-BASELINE (H9 step 1, run `6a13f306`, 899f436,
2026-07-08 late). Wall 364.8s vs 462.6s March baseline (-21%);
phase12 283s -> 148.4s - the campaign scales. Two planet blockers
found, one per project:

1. (pbfhogg) Phase12 peak RSS 21.5 GB, anon ramping 0 -> 20.5 GB in
   the first ~20s at NVMe read rate. Root cause in pbfhogg's pipelined
   reader: the stage-2 dispatcher spawns a decode task per blob with
   no in-flight bound (the raw channel drains instantly into the pool
   queue), and the reorder buffer admits far-ahead decoded blocks
   unboundedly while one straggler decode lags
   (pipeline_reorder_high_water 660 on NA vs <=51 germany). Problem
   statement + proposed fix (token-bounded in-flight decode, cap =
   decode_ahead) handed to the pbfhogg dev 2026-07-08. Until it lands,
   planet phase12 RSS is NOT bounded by elivagar's own budgets.

   RESOLVED ELIVAGAR-SIDE 2026-07-09 (`72b7c25`): pbfhogg's own
   research tree showed the precedent - cat_filtered documents the
   identical retention pathology (~25 GB planet, OOM 28.9 GB,
   2026-04-26) and pbfhogg migrated its planet commands to bounded
   pread workers (parallel_classify_phase) instead of patching the
   pipelined reader. Elivagar made the equivalent move on public API:
   UnorderedBlockSource (BlobReader + Blob::to_primitiveblock, one
   reader thread, N decode workers, two bounded channels, NO reorder
   buffer - locations mode is order-free end to end). Raw path stays
   on the ordered reader for the sorted node store. The pbfhogg
   token-bound proposal is now purely their call for their other
   consumers; nothing on the record path depends on it.
2. (elivagar) Assemble regressed 164s -> 198.6s: the pooled way-acc
   64M flushes fragment scratch into 2754 chunks / merge fan-in 1076 /
   15442 partitions, driving 95.8 GB of assemble reads against
   68.4 GB of merge bytes plus 210K majflt of page-cache thrash. Fix
   direction: coalesce acc flushes through a shared bulk-append spill
   buffer that writes ~1 GB sorted chunks (the old drain path's chunk
   shape) without the old drain serialization.

   FIXED (`26e4cd6`, sort::SpillCoalescer): NA chunks 2754 -> 73,
   fan-in 1076 -> 70, assemble majflt 210K -> 0, assemble 198.6 ->
   172.6s, wall 363 -> 351s.

THE PATTERN (write it down, it has now bitten three times in one day):
unbounded queue + ordered-or-slow consumer + straggler = input-scaled
RAM. Instances: pbfhogg's pipelined-read reorder window (20 GB),
elivagar's own drain result funnel (141s of blocked senders), and
assemble's pending-partition map (19.5 GB of encoded tiles parked
behind a dense straggler partition - fixed at `33ce85e` with a
partition claim window, workers may not start partition N until it is
within worker_count x 2 of the writer). Every queue between a parallel
producer and an ordered consumer needs an explicit window or byte
bound, decided at design time, with a wait counter on the bound.

THE SIBLING PATTERN (2026-07-15, now bitten five times): warm scratch
whose lifetime exceeds its work item = capacity ratcheted to the worst
item it ever served, times the pool width. Instances: the per-thread
AssemblyScratch pool (69c0f18), the ocean PyramidScratch pool (H4
rip-out), E3's rect-clip pool (reverted), and - the 07-14 planet-RAM
NO-GO - the phase12 way-acc pool (24 accs x ~770 MB at NA) plus the
relation tail's per-worker fold accumulators and their reduce-queue
copies. The i_overlay port created the exposure: it moved the engine's
per-op allocations into caller-owned scratch, so any accumulator
holding that scratch became a ratchet. Scratch lives exactly as long
as its work item (task, relation, tile); reuse beyond that must prove
its wall win against the retention it buys, and gated/thresholded
variants of "mostly reuse" were tried here and lost to the aggregate
(1 MiB input gate left 14 GB, adding a 2 MiB emission gate left
12.6 GB, unconditional drop cost zero measurable wall).

Remaining planet-RSS items after the claim window: ocean phase 9.4 GB
at NA (grew ~4 GB with the ocean spill coalescer - fine standalone,
but planet ocean is all coastlines; H5's precomputed ocean stream
remains the structural answer), and the H6 churn/retention work.

SESSION CLOSE 2026-07-09 (~05:30, commits 1ad1d66..69c0f18, 15
landings). The assemble plateau hunt concluded the day:

- Claim window (33ce85e): bounded parked batches, but the HWM counter
  showed only 1.29 GB parked - not the eater. Kept: it bounds a real
  worst case (max single partition encodes to 459 MB; planet
  straggler exposure is window x that).
- Allocator three-way A/B (f2184ce/a32c960): NA wall within 1.7%
  across mimalloc/system/jemalloc, assemble RSS 19.4/19.0/17.0 GB -
  ruled out retention, memory is live. mimalloc is now
  rip-out-eligible on simplicity (system allocator costs nothing
  measurable); decision parked, mi_commit stays useful meanwhile.
- The eater: per-thread AssemblyScratch pools growing to the fattest
  tile each rayon thread ever saw. Canary reset never fired (wrong
  proxy); the ELIVAGAR_SCRATCH_RESET=always diagnostic proved it
  (germany assemble-only 7.2 -> 1.8 GB, wall unchanged 21.6s), and
  69c0f18 deleted the pool entirely - scratch is per-tile now.

FINAL NA LOCATIONS NUMBERS (`b66fcc6e`, 69c0f18, single run):
wall 361.6s (March baseline 462.6s, -22%; morning-of re-baseline
364.8s), phase12 155.5s / 5.5 GB RSS (was 283s / 21.5 GB), ocean
19.5s / 9.9 GB, assemble 186.4s / 5.0 GB (was 19.4 GB). Run peak is
now the OCEAN phase at 9.9 GB - every phase under 10 GB at NA scale
on this 30 GB host, with all input-scaled stocks bounded except
ocean's (H5) and the per-partition claim-window exposure.

**NO LONGER TRUE (2026-07-14): phase12 measures 23.3 GB at HEAD, worse
than the 21.5 GB this campaign fixed.** The under-10-GB property is
broken and the planet RAM go/no-go with it - see the void notice in H3.
These 69c0f18 numbers stand as the record of what the campaign achieved
and as the target to get back to; they are not a description of HEAD.

RESTORED 2026-07-15: the scratch-retention fix put NA phase12 back at
5.6 GB anon / 170s / 12K majflt (see the H3 resolution block). The
69c0f18 numbers describe HEAD again, minus the ocean phase H5 deleted.

Next session's queue, in leverage order: (1) H3 extrapolation of the
two open terms - planet ocean RSS (H5 decides) and planet partition
counts / claim-window exposure - to produce the planet go/no-go
number; (2) ocean phase RSS grew ~4 GB with the spill coalescer,
worth one look (reduce-tree merge_from concatenation is the suspect);
(3) assemble wall is now the biggest phase at NA (186s at 17.3 cores,
80 GB of reads: H4's I/O pricing - LZ4 chunks - and H8b partition
sizing); (4) the mimalloc rip-out decision; (5) NA/germany/norway
outputs at 69c0f18 regress-verified against blessed before the next
blessing rotation.

THIS QUEUE IS SPENT (2026-07-14), and item 3 is a trap - it sent a
session at H8b believing NA assemble was the biggest phase. It was, at
69c0f18. It is not now: NA assemble is 91.5s against phase12's 206.3s,
and items 1 and 2 were both settled by H5's artifact, which deleted the
ocean phase's RSS line entirely (NA ocean now 0.3s / 1.1 GB). The live
item is the phase12 RSS regression in the H3 void notice. Read that
instead.

Ledger validation from the same run: relation buffer stayed under its
1 GB cap (236 MB, no spill), pmtiles dedup capped at 1M entries as
designed, dir entries 20.3M streamed fine. max_rel_inflight_bytes hit
90 MB - real monster-relation signal, survivable at NA, still wants a
per-relation gate before planet. way_index at NA: 6.45M member ways,
1.37 GB data + 103 MB index (mmap'd).

### H2: Use the pbfhogg preprocessing pass as elivagar's free prepass

**Claim.** The production input is written by pbfhogg, which already
streams the entire planet to add locations. PBF headers have free space
(pbfhogg already injects indexdata/tagdata). Anything elivagar derives
per run by reading the file before really reading it should instead be
computed once at enrichment time and injected.

**Evidence.** The channel works today: `BlobFilter::only_ways` /
`only_relations` prepass skipping rides on injected indexdata (62K blobs
skipped on germany). The relation-planning prepass costs a full extra
pre-read (6.3s serial on germany locations, 13.2s raw; minutes at planet
scale). The global shared-node prepass was disabled in favor of
block-local pins (log: "Global shared-node prepass disabled") - a quality
compromise made because computing exact shared nodes per run is expensive.

**Theory - concrete injection candidates, each independently priceable:**

- **(a) Relation plan.** Inject the member-way id set (or per-blob
  membership bitmaps) computed at altw time. Deletes the relation prepass
  read pass entirely and lets the way pass know membership without any
  runtime set construction. Saves serial minutes at planet scale.
- **(b) Exact shared-node pins.** altw sees every way's refs; it can mark
  shared (multi-way) nodes exactly and embed a per-way pin bitmap
  alongside the injected locations. Restores exact global DP pinning
  (currently approximated block-locally) at zero elivagar runtime cost -
  a correctness/quality win and a code deletion.
- **(c) Shortbread relevance masks.** Compile the layer tag matchers into
  a tag-key relevance check; altw marks blobs (or elements) that cannot
  match any layer. Post-altw node blobs are tagged-only so the win is
  bounded; price it with a shadow counter first (pbfhogg's measured
  caution: several passthrough ideas qualified zero blobs).
- **(d) Partition calibration.** Per-blob way-count/vertex-count/bbox
  stats let elivagar pick P3 partition boundaries and budgets from the
  header walk instead of hardcoded Hilbert prefixes - direct attack on
  straggler skew (H8).

**Honesty clause.** These shift work into a pass that is already paid in
production, but any "world record" claim against tools that read raw PBF
must publish both framings (see H10).

**First step.** Spec (a) first - it is the one with a measured serial cost
today. Requires a paired pbfhogg change; the injection format is pbfhogg's
domain (BlobHeader extension fields), the consumption is elivagar's.

LANDED 2026-07-11: (a) and (b) are both consumed on the injected path
(paired pbfhogg change, their `29e4eabd`, plus two elivagar landings; see
git history and `reference/performance.md` for the commits and gate
readings). The relation-plan prepass now runs only on non-enriched input;
the global shared-node prepass (`prepass_shared_nodes`,
`global_shared_node_pins`) is deleted outright - exact pins are read
straight from the injected per-way bitmap at zero elivagar runtime cost.
Both fallbacks (block-local pins, the runtime relation plan) remain
first-class for raw Geofabrik input. (c) and (d) remain open.

REMAINING from the retired injected-prepass spec - the NA planet-slope
reading (measurement only, no code; user-gated, real-PBF run):
re-enrich north-america locations with the new altw
(`pbfhogg add-locations-to-ways --index-type external --inject-prepass
--compression zlib:6`), register in brokkr.toml, then one
`brokkr tilegen --bench --dataset north-america --variant locations`
plus the sidecar readings (`--human`, `--stalls`, `--counters`).
Numbers that feed the H3 ledger: phase12 s/GB with the prepass deleted
(baseline 8.2 s/GB at NA `b66fcc6e` / `69c0f18`),
`way_index_data_bytes` under superset membership at NA scale,
`way_members_marked` vs the old `relation_plan_needed_ways`, and peak
RSS with the `needed_ways` stock gone. Write the row here (H3) and in
`reference/performance.md`.

### H3: A planet RAM ledger before any planet run

**Claim.** "Comfortably on 30 GB" is an engineering property, not a hope.
Every structure whose size scales with input must carry a counter and a
byte budget. Today's budgets (`--sort-budget`, `--way-budget`,
`--rel-budget`, `--assemble-budget`) cover the flows; the ledger must also
cover the stocks.

**Evidence.** The known-unbounded or unaudited list: reorder buffer
(847-blob high water under consumer-bound backpressure - the H1 fix also
relieves this, but it needs a byte bound regardless), relation
buffering/preparation, PMTiles directory + dedup fingerprints at planet
tile counts, mimalloc retention (7.4 GB committed vs 1.9 GB RSS at a phase
boundary), way_index page-cache footprint. Four OOM kills this morning
show what un-audited memory does on this host, albeit under
instrumentation.

**Theory.** Emit a per-structure peak-bytes counter set (sidecar), then
assign budgets: a planet run's peak RSS should be predictable from the
ledger before it is attempted. Structures that cannot be bounded in RAM
get external spill designs (the pbfhogg external-bucket playbook, already
proven here by the P2 shared-node external merge-sort). The PMTiles
directory question (pbfhogg techniques doc question 6) is already
answered in the code: streaming directory + capped dedup, see the
breakage list above.

**First step.** Counters only - LANDED 2026-07-08 alongside the H1
counters: `relation_blocks_bytes`, `relation_plan_needed_ways`,
`global_shared_nodes`, `way_index_ways`/`_data_bytes`/`_index_bytes`,
`pmtiles_dedup_entries`/`_bytes_est`, `pmtiles_dir_entries`,
`pmtiles_root_dir_bytes`/`pmtiles_leaf_dirs_bytes`. Next: the paper
exercise - planet ledger estimate from germany/norway per-unit numbers,
published in this note.

FIRST READING 2026-07-08 (germany locations `c51205b3`, norway
locations `63eade98`): the stocks are all small at extract scale -
germany `relation_blocks_bytes` 199 MB (111 blocks), way_index 126 MB,
PMTiles dedup 16.8 MB / 332K dir entries / 724 KB leaf dirs.
`pipeline_reorder_high_water` came in at 51 blobs (germany) / 264
(norway); the feared 847-blob high water did not reappear on this
commit. The standout ledger item is mimalloc retention: `mi_commit`
7.37 GB at germany phase12 end and 11.2 GB at run end vs 8.8 GB peak
RSS - allocator-committed ~2.4 GB above resident, the largest
unbudgeted stock and another point for H6's churn reduction. The
extrapolation exercise remains open.

LEDGER EXTRAPOLATION (paper, 2026-07-08 late, post-campaign; NA run
pending as the calibration check). Phase12 stocks at planet scale,
with today's bounds in place:

- sort chunk buffer: 1 GB (--sort-budget), input-independent.
- pooled way accs: workers x 64 MB = 1.5 GB on 24 threads,
  input-independent (landed today).
- relation blocks: capped 1 GB, spills to PBF re-read (landed today;
  was the largest input-scaled stock, est. 6-10 GB at planet).
- prepared relations in flight: one per worker; germany/norway HWM
  6-8 MB total. OPEN RISK: a planet monster multipolygon (Antarctic
  coastline class, ~50K member ways) could prepare to hundreds of MB;
  worst case is workers x largest-relation, transient. Watch
  max_rel_inflight_bytes on NA/planet; a per-relation size gate or
  fanout-cap-style skip is the backstop.
- pbfhogg reorder buffer: 19-51 blobs germany (~hundreds of MB
  worst case); consumer is much faster now, high water should stay low.
- way_index: mmap'd, disk-backed - planet ~30-40M member ways,
  ~4-6 GB on disk, page-cache pressure not RSS.
- PMTiles writer: dedup capped ~60 MB + streaming directory. Bounded.
- mimalloc retention: ~2.4 GB observed above RSS (H6 target).

Sum of bounded phase12 stocks: ~5-7 GB + churn headroom - phase12 fits
26 GB with room. The two phases WITHOUT planet-ready bounds are ocean
(H5: planet hits all 28,883 shapes; germany ocean phase RSS 3.1 GB is
bbox-clipped and not representative) and assemble (germany peak 8.5 GB;
scales with per-partition density and reader count, needs the NA
number). Planet go/no-go per H9 step 4 stays gated on the measured NA
slope for those two.

PLANET GO/NO-GO (paper, 2026-07-09, calibrated on NA `b66fcc6e` at
69c0f18). Assumptions: 90 GB enriched planet, ~5.3x NA ways (1.1B vs
209M), ocean features 2.5-3x NA (NA already hits 16,990 of 28,883
shapes and pays 106.5M ocean features), unique tiles 3-4x NA
(~60-70M; tiles scale with area, not input bytes - NA is 18.0M).

Per-unit constants from the NA run: 52.7 MB/s end-to-end (19.06 GB /
361.6s), phase12 8.2 s/GB, 32.7M features/GB, sort scratch 2.83 GB
per GB of input (54.0 GB records), assemble 0.30 us/feature. That
last one answers the open slope question: germany was 71 MB/s and
0.11 us/feature - the germany->NA bend is concentrated in assemble
(60x the unique tiles for 3.4x the input, 79.3 GB of merge reads)
plus the ocean bbox term. Phase12 itself bends only mildly
(6.4 -> 8.2 s/GB).

- RAM: CONDITIONAL GO. Phase12 ~6-7 GB (all stocks bounded; relation
  blocks will hit the 1 GB cap and take the verified spill path - NA
  used 236 MB at ~1/5 of planet relations). Assemble ~6-9 GB
  (claim-window worst case = window 8 x ~0.9 GB planet max
  partition; observed NA parked HWM only 1.28 GB against a 459 MB
  max partition). Ocean is the peak and the one open term: 9.9 GB at
  NA; if it scales with shapes-hit (x1.7) that is ~12-17 GB - fits
  standalone (phase12 releases to 1.35 GB before ocean starts) but
  eats all headroom. The coalescer look (next-session item 2) or H5
  is wanted before the run, not necessarily blocking it.
- WALL: ~25-35 min. Phase12 730-820s + ocean 50-60s + assemble
  700-900s, with the assemble band wide because ~370 GB of merge
  reads through a 30 GB page cache is the untested term (H4). Even
  the pessimistic end is ~5x under the 2h38m published 16cpu/32GB
  record; the resource-classed claim survives the margin.
- DISK: NO-GO TODAY - this is the hard blocker, not RAM. Input
  90 GB + uncompressed scratch ~255 GB = ~345 GB on the NVMe that
  has 292 GB free (output goes to the hdd target). Consequence: H4
  knob 2 (LZ4 chunks, plumbing exists) is promoted from optimization
  to planet ENABLER - at ~2x ratio scratch drops to ~130 GB and the
  run fits with ~70 GB headroom. Alternative/complement: unlink
  chunks as assemble consumes them, and/or free space on the drive.
  UPDATE same day: user freed space - Banan now has ~607 GB free, so
  disk is GO even uncompressed; lz4 remains the planet configuration
  for the page-cache effect, not survival. Also corrected
  brokkr.toml drive classes: data/scratch/target are ALL on NVMe
  (nvme1n1p1); the ssd/hdd labels in earlier results rows are stale
  hardware provenance, and the "output goes to hdd" caveat above is
  obsolete.
- Ledger addendum: pmtiles dedup cap skipped 17.0M inserts at NA
  (101.8M tiles still reused, 6.2 GB saved); at planet the 1M cap
  costs output bytes (missed dedup), not RAM - price before H10
  record runs. Dir entries ~80-100M streamed, leaf dirs ~150 MB,
  fine. way_index ~7 GB on disk, page cache pressure only.
  mi_commit ended 20.3 GB vs 10.0 GB RSS at NA - the rip-out
  decision (queue item 4) stands on its own.

Sequencing consequence: H4's LZ4 A/B (NA-scale, --compress-sort-chunks
on vs off) is now first in line - it is simultaneously the disk
enabler, the assemble I/O price probe, and cheap. Ocean RSS (item 2)
second. Planet dry run gates on both landing green.

**THE RAM VERDICT ABOVE IS VOID (2026-07-14). Allocator commitment has
roughly DOUBLED on every dataset measured, and at NA that puts phase12 at
44.4 GB committed against 23.3 GB resident on a 30 GB host.** Peak RSS at
NA is 23.3 GB against the 5.5 GB this go/no-go is calibrated on and the
6-7 GB it extrapolates to planet - worse than the 21.5 GB that was logged
as planet blocker 1 on 2026-07-08. The headline result of the 07-08..09
campaign, "every phase under 10 GB at NA scale", does not hold. Planet RAM
is NO-GO; nothing below the go/no-go should be read as current.

The regression is `mi_commit`, and it is systemic and proportional, not an
NA phenomenon:

| dataset | mi_commit recorded | mi_commit now | factor |
|---|---|---|---|
| north-america | 20.3 GB (run end, 69c0f18) | 44.4 GB (`b9d6c12c`) | 2.19x |
| germany | 7.37 GB (phase12 end, 9e8dce2) | 16.2 GB (`2d715357`) | 2.20x |

The same factor on both. What differs is only whether the host can absorb
it: NA's 44.4 GB of commitment on a 30 GB box forces the resident set into
conflict and produces 1.93M major faults, while germany's 16.2 GB fits and
its RSS reads 6.58 GB - BETTER than its own 8.8 GB baseline. **Germany is
not a negative control.** Reading RSS on germany hides this defect
completely; read `mi_commit_phase12_end`.

At NA the commitment is also frozen: `mi_commit` is byte-identical at
44,412,502,016 from PHASE12_END through ocean, sort, assemble and run end -
mimalloc commits it all during phase12 and never returns a byte for the
remaining 104s.

Per-phase re-measurement (`b9d6c12c`, b833fc8, NA locations,
artifact-active) against the same PBF, host and byte-identical invocation
as `6a13f306` at 899f436:

| phase | 899f436 | 69c0f18 recorded | b833fc8 measured |
|---|---|---|---|
| phase12 | 148.4s / 21.5 GB / 18.9 cores | 155.5s / 5.5 GB | 206.3s / 23.3 GB / 15.4 cores |
| phase12 majflt | 12,053 | - | 1,933,730 |
| gap to ocean | 0.2s | - | 12.4s / 0.5 cores / 18.7 GB read |
| ocean | 15.5s | 19.5s / 9.9 GB | 0.3s / 1.1 GB |
| assemble | 198.6s / 16.2 GB / 16.0 cores | 186.4s / 5.0 GB | 91.5s / 4.7 GB / 14.9 cores |
| wall | 364.8s | 361.6s | 311.2s |

Wall improved, which is how this hid: H5's artifact took ocean from 19.5s
to 0.3s and assemble from 186.4s to 91.5s, more than paying for phase12's
+58s. Nothing gates RSS or commitment - regress compares tiles, earcut
compares geometry, verify checks structure - so this is invisible to every
standing gate by construction.

Reading of the numbers, and what is NOT yet established:
- The +58s of phase12 wall is probably a symptom, not a second bug:
  1.93M major faults at roughly 30us each is ~58s, which is the whole
  delta. Arithmetic only - not confirmed.
- CHEAPEST TEST FIRST, and it needs no old commits: the allocator A/B at
  HEAD. `f2184ce` and `a32c960` already built the three-way switch
  (`--features sys-alloc`, `--features jemalloc-alloc`). If phase12's
  numbers collapse under either arm, this is mimalloc retention and the
  answer is queue item 4 - the parked rip-out decision, whose own note
  says the system allocator "costs nothing measurable" and that mimalloc
  "predates all measurement here". Prior A/B evidence is narrow, not a
  refutation: `18e1656` ruled out retention for ASSEMBLE's plateau at NA
  (19.4 / 19.0 / 17.0 GB across the three arms - memory was live there).
  Phase12 has never been A/B'd.
- If a bisect is still wanted after that, run it on GERMANY against
  `mi_commit_phase12_end` at ~1 min a step, not NA at ~7. The 2.20x factor
  is fully visible there.
- Window `e34cc7b..b833fc8`, ~60 commits. Do NOT bisect 899f436..e34cc7b:
  the campaign's own commit messages price that window and `72b7c25` is
  the commit that took phase12 from 21.5 to 5.5 GB.
- Suspects have NOT been narrowed. Commit subjects suggest `4f2cd90` and
  `d20ddd5` on cross-run scratch retention - the failure mode `69c0f18`
  and `c6d4e16` deleted twice and E3 was reverted for - but subject-line
  pattern matching produced two wrong hypotheses already that day, so
  treat it as unexamined.
- Corroborating, not causal: reference/performance.md records germany
  phase12 peak RSS at 22.8 GB at `8eaa8bf`, falling to 6.3 GB via the
  i_overlay de-churn at `e2284ec`, with the note that germany's polygon
  volume was allocator-bound and the host under memory pressure at that
  figure. `e2284ec` is INSIDE this window, so it is not the cause. It
  establishes that phase12 churn on this host can hold this much, and that
  de-churn is the shape of the fix.
- **BLOCKER: brokkr.** It passes `--ocean path` and `--ocean-simplified
  path`, both removed at `38250b9`, so every measured command fails until
  it emits the new spelling. That gates the A/B, any bisect, and any
  re-measurement. Note also that `brokkr tilegen --commit <hash>` cannot
  straddle `38250b9`: it builds old code but passes one flag dialect, so
  bisecting a window that predates the CLI change needs that handled
  first. The A/B does not - it runs at HEAD.
- New and unexplained: a serial gap between PHASE12_END and OCEAN_START
  that scales with input (germany 5.15s / 5.3 GB read; NA 12.4s / 18.7 GB
  read) at 0.3-0.5 cores, against 0.2s at 899f436. It pairs with a
  `prepass_join` stall of 13.5s where this document records
  prepass_join_wait at ~0 on germany and norway. ~8% of germany's wall.
- Confound to respect: b833fc8 is artifact-active and the comparands are
  not, which moves work between the ocean and assemble phases. Phase12 is
  unaffected - it does not touch ocean - so the RSS finding stands, but
  the ocean and assemble rows above are not like-for-like.

All of this is sidecar data (`brokkr sidecar b9d6c12c --human`), which is
local to plantasjen and gitignored; the numbers are transcribed here
because the results row keeps only elapsed_ms.

RESOLVED 2026-07-15 (same-day session; the fix commit follows this
note). Findings, in the order they overturned the notice's guesses:

- The allocator A/B at HEAD (run first, as instructed) REFUTED the
  mimalloc-retention theory. NA sys-alloc: phase12 peak RSS 21.3 GB -
  the same as mimalloc's 21.1 - with 3.8M majflt and wall 413s vs
  314s. The memory was reachable under every allocator; `mi_commit`
  was the symptom (mimalloc never decommits what the live peaks force
  it to take), not the cause. Germany arms: sys-alloc and jemalloc
  both ~60s wall vs mimalloc 68s, phase12 ~34-35s vs 42.7s at
  identical output - a standing data point for the rip-out decision.
- The NA live regression bisected (NA phase12 peak anon, my config,
  endpoints verified) to the i_overlay port: e34cc7b 5.4 GB good,
  d20ddd5 5.1 GB good (both named scratch suspects exonerated),
  659a187 OOM-KILLED at 24.8 GB, e2284ec 21.0 GB, 2c770c7 21.4 GB,
  HEAD 21.4 GB. e2284ec's de-churn was the germany-scale fix (22.8 to
  6.3 GB) but only took NA from OOM to 21 GB.
- Mechanism (sidecar timeline attribution): the port moved the boolean
  engine's per-op allocations into caller-owned scratch, and every
  phase-lifetime accumulator holding that scratch became a capacity
  ratchet - hump 1 was the way-acc pool (24 accs x ~770 MB, freed
  exactly when the read ends), hump 2 the relation tail's per-worker
  fold accumulators plus finished accumulators queued for reduce. The
  ~20 GB of anon evicted the page cache, and the tail's way_index
  mmap reads became the 1.9-2.0M major faults; the +58s of phase12
  wall was that thrash, as the notice's arithmetic suspected.
- Fix (verified attribution-first with a fresh-scratch diagnostic run:
  5.95 GB, wall unchanged): way accs die with their block task
  (flush residuals to the coalescer, ship way_puts + tally through
  the drain); the relation tail drops its geometry-scaled scratches
  after every relation and finalizes each fold accumulator on its
  worker before it queues for reduce. Gated "mostly warm" variants
  measured worse (see THE SIBLING PATTERN in H1).
- Post-fix NA (dirty-run values; stored bench follows the commit):
  wall 288s, phase12 170.3s / 5.64 GB peak anon / 11.9K majflt,
  mi_commit_phase12_end 18.7 GB (from 44.6). Germany: wall 63.5s,
  phase12 39.1s / 4.2 GB anon. Denmark 10.0s. Output byte-identical
  vs clean HEAD (regress --file/--against, 1,296,999 tiles, raw-equal
  on every blob pair).
- Germany mi_commit_phase12_end is UNCHANGED at 14.2 GB against
  4.2 GB anon: arena commitment under churn, mimalloc-specific,
  invisible in RSS, owned by the H6 allocator addendum (rip-out).
- The serial PHASE12_END-to-OCEAN gap is explained and benign in
  origin: b833fc8's provenance contract hashes the input PBF
  single-threaded at run end (18.5 GB re-read / ~15s at NA, 5.4 GB /
  ~5.3s germany). It is real wall (~5-8%) and a candidate for
  overlapping with the read or reusing brokkr.toml's recorded hash,
  but it is reporting, not pipeline. CLOSED 2026-07-15: the hash now
  runs on a background thread overlapped with phase12 (streamed, not
  mmap'd, so it cannot inflate sampled RSS; join instrumented as
  input_hash_join_wait_ns). Germany gap 5.15s -> 0.30s, join wait
  ~zero, phase12 unchanged; output regress raw-equal on all 1,296,999
  denmark tiles. Resumes still hash up front to validate the
  checkpoint before work runs under it.
- The blessed denmark baseline (`ec5bd11`) predates the provenance
  block and current `brokkr regress` refuses to gate against it, so
  NOTHING has been regress-gateable against blessed since b833fc8.
  This landing gated against a clean-HEAD worktree build instead.
  A re-bless from a post-provenance build needs a user decision.

Planet RAM go/no-go status: the phase12 stocks are bounded again and
the 07-09 ledger extrapolation is arguably current again, but re-read
it against a fresh NA stored run before any planet attempt.

### H4: Sort scratch needs page-cache hygiene, maybe compression

**Claim.** ~200 GB of scratch write+read through a 30 GB host will
dominate planet wall if left to default buffered I/O.

**Evidence.** Germany: 12.3 GB records, `sort_chunk_write` wait already 6%
of wall on a machine whose page cache fits the whole working set. pbfhogg's
sort/ALTW work hit this exact wall and solved it with DONTNEED-after-use,
O_DIRECT where measured, and deliberate scratch formats.

**Theory.** Three independent knobs, all priceable on NA before planet:
(1) `fadvise(DONTNEED)` after chunk write and after merge-read - pure
hygiene, no format change; (2) LZ4 chunks (plumbing exists; the earlier
25s regression was a streaming-encoder mistake, since fixed by
presorted-buffer compression - re-price the default); (3) partition-local
chunk sizing so merge fan-in stays low (`sort_merge_max_fanin` is 28 on
germany - already fine, verify at planet partition counts).

**First step.** NA-scale A/B: `--compress-sort-chunks` on vs off (bench 3),
plus majflt/PSI from the sidecar samples. No code needed to start pricing
knob (2); knob (1) is a small change priced by the same run.

PRICED AND UNBLOCKED 2026-07-09 (germany locations, 9ecbdd4). The
go/no-go block above promoted LZ4 chunks to planet enabler; the pricing
run then exposed a format blocker: compressed chunks bypassed the
multi-partition section format and wrote one FILE per partition range
per flush (1334 files at germany's 234 partitions vs 18 coalesced;
at NA's 15442 partitions that is the exact tiny-file fragmentation the
SpillCoalescer was built to kill). Fix landed: per-section compression
inside the multi-section chunk file - each partition section is an
independent LZ4/Snappy frame, the section table stores frame-start
offsets (written as a placeholder, patched by seek-back once compressed
sizes are known), and readers seek+stream-decode exactly count records.
Per-compression magics (ELVGSRT1/ELVGSRL1/ELVGSRS1) make a --skip-to
resume with a flipped flag fail loud. All compressed writes now route
through the partitioned coalesced path; the file-per-range branches are
deleted.

Germany numbers (locations, bench): baseline `ab34dc54` 61.6s, lz4 old
format 66.8s, lz4 sectioned 67.5s dirty-tree run - chunks 18, fan-in 17,
compression CPU cost ~8-9% of wall where the page cache already holds
everything. Physical writes 13.78 -> 6.17 GB in phase12 (ratio ~2.6x on
chunk bytes); assemble disk reads 5.49 GB -> ~0 (compressed scratch fits
in cache). At planet: scratch ~255 -> ~100 GB (disk NO-GO becomes GO),
and the CPU-vs-I/O trade should invert once merge reads stop fitting in
RAM - the NA A/B prices that. Correctness: germany regress vs 18e1656
archive, 827010 tiles identical, 0 diffs.

Gate policy from here (user call, 2026-07-09): regress runs on DENMARK
ONLY - a denmark bench + regress is minutes cheaper than a germany one
and catches the same format/geometry regressions. Germany/NA regress
only at blessing rotations.

NA A/B READ 2026-07-09 (`dffccc5f` vs `b66fcc6e`): lz4 costs +2.0%
wall at NA (361.6 -> 369.0s; phase12 +6.5s of compression CPU,
assemble flat) and cuts phase12 physical writes 60.1 -> 26.3 GB,
assemble reads 79.3 -> 30.8 GB, scratch on disk ~2.6x. Assemble was
flat because NVMe was never its NA bottleneck - encode CPU is; at
planet, where merge reads overflow the page cache, the trade should
invert. Verdict: lz4 is the planet-run configuration; extract-scale
default stays uncompressed until a record run cares.

RIP-AND-TEAR CAMPAIGN, SAME DAY (perf hunt on germany/NA, user
directive to explore the major theories aggressively):

- Assemble worker cap + BYTE-BUDGETED CLAIM WINDOW. The 4-worker cap
  and the partition-count window were both binding: 8 workers cut
  assemble 186.4 -> 173.0s but claim-window wait hit 83.6% of wall
  (workers parked behind stragglers while holding under 1.3 GB of a
  multi-GB budget); 12 workers made it WORSE (176.1s, RSS 8.7 GB) -
  count is saturated. The structural fix: workers may claim any
  distance ahead while the writer's parked bytes are under a byte
  budget (2 GiB default, ELIVAGAR_ASSEMBLE_PARK_BUDGET), collapsing
  to the tight window only over budget. Claim wait 83.6% -> 1.0%,
  assemble 186.4 -> 160.7s (-14%), cores 17.3 -> 19.2, parked HWM
  2.51 GB (budget + in-flight overshoot, as designed). Worker default
  still 4 (ELIVAGAR_ASSEMBLE_WORKERS=8 used in runs); promote 8 +
  window to defaults when committed numbers settle.
- OCEAN SCRATCH RIP-OUT. OCEAN_EMIT_SCRATCH was a per-thread
  PyramidScratch pool - the same input-scaled retention the assemble
  scratch pool had (deleted 69c0f18). Now built per work item; NA
  ocean RSS 9.9 -> 7.7 GB, ocean wall unchanged. Run peak moved to
  assemble (8.6 GB, mostly the parked window).
- PARALLEL NODE WORKERS (locations mode only; raw path stays 1 for
  store ordering). Pool of threads/4 clamped 2-6 sharing the node
  channel. NA: node_block_send wait 16.5 -> 2.7s but phase12 wall
  UNCHANGED - the freed consumer time moved into way_block_send/
  way_budget; node time was hidden behind the way path. Denmark wins
  outright: 13.7 -> 11.8s wall. Keep.
- Full-NA scoreboard with all three + lz4 (dirty-tree run, f3e71b0
  code + knobs): wall 343.9s (-4.9% vs 361.6 baseline), phase12
  162.3s (+6.8 lz4 CPU), ocean 20.1s / 7.7 GB, assemble 161.0s /
  8.6 GB. Denmark gate + regress clean (1,296,996 tiles identical).
- Way-budget A/B: 1.5G vs 768M was a NO-OP (wait 93.4 -> 91.3s,
  max_way_inflight peaked at 1.25 GB estimated, under the raised cap).
  The binding constraint is the task-count ceiling / way-stage CPU,
  not bytes - 768M default stands, and the way-path frontier is
  H6 churn reduction, not knobs. An ELIVAGAR_WAY_BUDGET env override
  was added here for future A/Bs, because brokkr's tilegen wrapper had
  no --way-budget passthrough; it outranked the flag, so a run could
  record 256M and use 768M. Deleted 2026-07-15 - the wrapper is
  configured from brokkr.toml now, and --way-budget is the only way to
  set this.
- Defaults promoted after gates: assemble workers 4 -> 8, byte-budget
  claim window 2 GiB (both env-overridable). Denmark gate at final
  defaults: 11.7s wall, regress vs blessed clean.

Assemble ceiling note for the next hunt: at 8 workers + byte window,
readers cost ~150 thread-s (k-way merge + lz4 decode) against ~19.2
avg cores; writer idle (partition_batch wait) is pipeline shape, not
a defect. Remaining assemble ideas, unpriced: reader/encode overlap
inside a worker (reader blocks during its rayon encode today),
pmtiles write path (7.3% of wall).

SPLIT Z7 LANDED same day: PARTITION_SPLIT_Z 6 -> 7 (z14 partitions go
from one z6 prefix / 65,536 tile ids to one z7 prefix / 16,384).
Germany was the proof case: it spans so few z6 prefixes at high zoom
that one partition encoded 921 MB - a third of total output behind a
single claim-window slot. At z7: max partition 294 MB, partitions
234 -> 634, germany assemble 17.0 -> 11.6s (-31%), wall 55.2 -> 49.9s
(-9.6%) against the same-code z6 baseline (`dbe5f576`), chunks and
fan-in unchanged, claim wait ~0.

NA CONFIRMATION (`88dc2385`, stored, lz4+8 workers): wall 348.5s,
assemble 166.4s - a ~5s assemble REGRESSION vs the z6 byte-window run
(341.1s / 161.0s). Partitions 15,442 -> 52,220, max partition
459 -> 199 MB, reader thread-time actually fell 150 -> 132s; the cost
is per-partition fixed overhead (52K partition opens x ~20 section
opens+seeks each). VERDICT: KEEP z7 anyway - planet contains Europe,
and Europe at z6 is germany's 921 MB-partition pathology multiplied;
the quartered straggler tail is planet insurance worth NA's +1.6%.
Claw-back candidate if it matters later: a per-chunk file-handle
cache (pread per section instead of open+seek per section).

Day scoreboard (stored runs, locations): germany 77.8s (07-08 suite)
-> 52.2s (`3c365742`, -33%); NA 462.6s (March) -> 361.6s (morning
baseline) -> 348.5s (`88dc2385`, -3.6% on the day with lz4's +2%
absorbed); denmark 13.7 -> 11.4s. Peak RSS at NA: 8.6 GB assemble,
ocean 7.8 GB, phase12 6.1 GB - all comfortably inside the 30 GB
ledger.

BLESSING ROTATION 2026-07-09: the toml blessed (a0fca65) predated the
intentional output change in aec278c (cross-block node pins made
optional), so brokkr regress against it ground for 15+ minutes in the
geometric ocean matcher diffing accepted deltas - the 9e8dce2 archives
were the de-facto baseline all along, and archive rotation had deleted
them. Re-blessed from denmark-f3e71b0.pmtiles (verified bit-identical
to 9e8dce2 twice today) as denmark-c9362c4.pmtiles. Lesson: bless
IMMEDIATELY after accepting an output-changing landing; the blessed
entry in brokkr.toml is the only baseline that survives archive
rotation. z7 denmark gate vs the new blessed: 1,296,996 tiles
identical, tol 0.

### H5: Ocean becomes a durable precomputed tile stream

**Claim.** At planet scale, backlog item 23 stops being an optimization
and becomes structural. Ocean input is constant across runs (keyed by
shapefile release + simplification policy + zoom policy); regenerating
the entire world's coastline geometry every run is planet-scale work for
zero information gain.

**Evidence.** Norway: ocean is 59% of sort records (16.7M features) on a
coastal extract. Planet is the all-coastline case plus full-tile fills for
~70% of the surface. Dedup already shows the output is massively
redundant (15.5M of norway's 16.3M ocean tiles are reused payloads).

**Theory.** Precompute once: run the existing int_ocean engine over the
shapefile globally, emit an ordered-by-tile-id stream of canonical ocean
layer payloads (full-fill tiles collapse to references to one canonical
record - the uniform-subtree shortcut already produces these). At
generation time, the assemble phase merges it as one more ordered
partition input; the ocean phase and its sort volume disappear from every
run. Cache invalidation is a hash check; a cold cache falls back to
today's path. Composes with H8's partition streaming and deletes
planet-ocean RAM pressure (H3).

**First step.** Price the artifact: per-zoom ocean tile counts and bytes
for the planet shapefile (the counters exist per-zoom on any run;
extrapolation from the shapefile's global shape count is a paper
exercise). Then a spec.

SPEC WRITTEN 2026-07-12 (v5; three codex critique rounds plus a
competitor-comparison review folded; the spec is implemented and the
note since deleted - recover it from git history if the design
rationale is ever wanted). Key survey deltas vs the theory above:
dedup is post-gzip storage-only, so
the reused 85% of NA's addressed tiles still pay merge+encode+gzip -
the artifact deletes that, not just the phase; the artifact is itself a
PMTiles archive consumed run-aware at assemble.

IMPLEMENTED AND ADJUDICATED same day (`92ed329` + follow-ups): the
world artifact exists - 942.7 MB, 212.4M addressed tiles, 9.2M unique
blobs, 13.97M directory runs, 95.7% deduplicated, verify + earcut clean
in the new run-aware gate modes. The brick 5 gate found that interior
tiles depend on the piece clip extent through the pyramid's
root/bisection structure (27 structural ocean diffs on denmark, one
strictly interior) - artifact output is DIFFERENT from extract-computed
output, not wrong: the human viewer gate judged the artifact-active
archive equivalent, the hybrid stands, and the blessed baseline rotated
to the artifact-active build. Ledger consequences: H5's win applies to
extracts AND planet - denmark ocean phase 5.75s -> 1.87s band-only,
wall ~12.5 -> ~10s, and at planet the ocean phase RSS line (12-17 GB),
the ~290M-record sort share, and the ~236M assemble encode chains are
deleted. Standing caveat: extract output now depends on artifact
presence; regress gates assume the gate machine carries the same
data/ocean-tiles.pmtiles the blessed archive was built with, and a
shapefile-release rotation of the artifact is an output-changing event
that forces a bless rotation.

OPEN, UNGATED: the latent cross-piece ocean seam. `ocean.rs` sets
`pins: None`, so the only simplification pins come from
`build_edge_flags`, which pins the current cell's tile-edge window plus
`params.pins` junctions and nothing else. A boundary genuinely SHARED
between two ocean source pieces, away from tile edges, is therefore
simplified independently on each side and can open a seam. The ocean
shapefile path is the one polygon producer with shared edges and no pin
source - contrast the OSM path's shared-node pins. The 2026-07-12 VW
landing inherited `pins: None` from DP unchanged, so the fix is
orthogonal to that work; the machinery exists unused
(`quantize_polygon_pinned_into`, `PyramidParams.pins`).

Never observed in the wild - the Norway coastline spikes were within a
single feature, which is why this stayed latent - and no standing gate
would catch it if it appeared. earcut cannot: a seam is a vertex in the
wrong place, not a self-intersection. The connected-component render
gate that would have covered it was refuted on calibration
(2026-07-14): sweeping resolution and connectivity, the
spike-vs-legitimate-simplification separation topped out at 3.29x
against the 4x the threshold math needs, and the largest disagreement
component missed the pinned apex ROI in every cell - rendered-area and
rendered-component measures do not separate an ocean defect from
legitimate generalization, the same failure the coverage oracle had
with aggregate area. If a geometry-level ocean gate is wanted, the
recorded candidate is a baseline-free needle detector on the emitted
ring, analogous to the boundary oracle's spur detector: flag point
pairs a sub-pixel straight-line distance apart but a long path-length
apart, excursion on the land side. Categorical, no REF build, and it
satisfies the oracle discipline in AGENTS.md that the coverage oracle
failed.

### H6: Kill the way-path allocation churn with per-worker scratch

**Claim.** 622 GB of alloc traffic per germany run is a phase12 throughput
tax (it is the consumer side of H1) and an RSS-stability tax (mimalloc
retention under 24-thread churn).

**Evidence.** Alloc profile `45065b2f`: emit_polygon_feature 68.5 GB
across 46.6M calls (1.5 KB avg - per-feature temporaries),
simplify_shape_dp 50.4 GB, process_planned_way_into 31.4 GB. The P1
descent already proved the fix pattern (flat buffers + reuse) on the ocean
engine; the way path never got it.

**Theory.** Per-worker scratch (coordinate buffers, ring buffers, attr
encode buffers) reused across features within a task - pbfhogg technique
6, with its measured caution (no shared cross-thread pools). Expected:
higher phase12 throughput per core, flatter RSS, smaller mi_commit
retention.

**First step.** This one is cheap enough to spec directly once H1's
counters exist (the two land in the same region of code; sequence them as
one campaign to avoid double churn).

FIRST H6 BATCH LANDED 2026-07-09. Fresh denmark alloc profile
(`98b9f1f0`, 593.8 GB total churn) re-ranked the sinks - the stale
germany numbers above predate the H1 campaign. Top exclusive:
simplify_shape_dp 10.7 GB / 9.8M calls (~11 allocs per contour),
emit_ocean_piece 8.1 GB (partly the deliberate per-item scratch
trade), intersect_rect_into 8.1 GB + normalize_into 7.0 GB (both
i_overlay-internal, library-owned, not tractable from here),
emit_polygon_feature 5.2 GB (fresh Shape per quantize),
add_feature_to_layer 5.0 GB (assemble side). Landed: (1)
simplify_shape_dp rewritten in place - all temporaries live in a
DpScratch inside IntEmitScratch, cleared per call, output written
back into the input contour's own allocation; (2)
quantize_polygon_into / quantize_polygon_pinned_into recycle the
scratch Shape's ring Vecs across features in both
emit_polygon_feature and emit_multipolygon_feature; the unpinned
originals stay for ocean + tests, the pinned originals are deleted.
Both changes bit-identical at the denmark gate (tol 0), which also
subsumes the earcut oracle for pure refactors. Remaining tractable
sinks: add_feature_to_layer (assemble), merge_same_attr_geometries
3.8 GB, process_planned_way_into 2.6 GB; the i_overlay returns want
an upstream API that fills a caller buffer.

MEASURED (`e1afc38d` vs `98b9f1f0`, same denmark input):
simplify_shape_dp exclusive churn 10.7 GB -> 609 MB (-94%), its
thread-time 13.4 -> 10.4s; total run churn 593.8 -> 583.3 GB.
Germany wall 52.2 -> 50.2s (`39d085b8`). emit_polygon_feature only
5.2 -> 4.8 GB - the residue is pin-key hashing and pyramid-internal
allocs attributed to it, a later pass.

CAMPAIGN LANDED (user open door 2026-07-09, both landings closed
2026-07-10): i_overlay is no longer a production dependency. The
two ops elivagar used (simplify_contour / overlay Subject+NonZero,
and the rect intersect) were extracted and inlined into
`src/geometry/overlay/`. Landing 1 (verbatim in-tree engine, i32
monomorphized, dead-surface pruned, 12-file layout) landed across
commits `d570daa`, `fbca741`, `b97cddc`, `659a187`. Landing 2 (the
de-churn: engine-owned scratch, CSR nodes, pooled extraction, caller
recycling, plus a review pass that closed four churn/pool leaks in the
gated frames - a per-call Vec in ring_is_valid, a dropped shape shell,
an intersect-identity clone, and dead-ring drops in simplify_shape_dp)
landed at `e2284ec` with denmark regress bit-identical at tol 0
(1,296,996 tiles), earcut oracle clean, verify clean, wall best-of-3
13.3s -> 11.6s (-13%, bench `c1053012` vs baseline `bcac01ad`).
Post-fix churn normalize_into + intersect_rect_into 8.1 -> 6.3 GB
combined (-22%; normalize 5.8 -> 4.5, intersect_rect 2.3 -> 1.8, alloc
`0433cbc1` vs baseline `546b9d58`), peak RSS 9.2 -> 7.3 GB. The spec's
under-3 GB churn keep gate proved mis-calibrated against the
L1-reduced 8.1 GB baseline - L1's i32 monomorphization had already
cut the pair from the pre-port 15.1 GB to 8.1 GB, so ~6.3 GB is the
achievable floor from this kill list; the win was landed by user
decision. i_overlay
survives only as a dev-dependency: the 2,000-case differential
oracle, kept independent (crates.io build, boundary point
conversion, no [patch]) so it can keep gating the in-tree engine.
Full port rationale, kill list, and brick-by-brick history live in
git log (`d570daa`..`659a187` and the Landing 2 commits that follow).

THE POST-PORT SURFACE (forward-looking, 2026-07-10). Once owned and
i32-flat, the engine is optimizable beyond the port campaign's
stopping rule - options a crates.io dependency structurally could not
offer. None of this is scheduled; it is the theorized surface, ordered
by expected leverage on planet-on-30GB, each entry honest about what
is measured versus speculated. Two framing facts first.

Why engine CPU is planet-relevant at all: the overlay ops sit on the
exact path planet stresses hardest. `normalize_into` is rank 4 in the
norway hotpath (129.2 thread-s) and executes inside ranks 1-2
(`process_prepared_relation_into` 267.8, `emit_multipolygon_feature`
243.9) - the coastal/relation stack H7 names as planet's tomorrow.
On denmark the call shape is 11.41M `normalize_into` calls against
6,550 `intersect_rect_into` calls (alloc `546b9d58`): the engine's
planet bill is millions of tiny per-feature normalizes, not the rare
big boolean. And churn is the 30 GB budget's enemy independently of
wall (the H3 first reading pinned allocator commit ~2.4 GB above RSS).

The gate insight that reshapes what "forbidden" meant: the port
campaign's stopping rule banned everything below because the
instruments were still being built. Now that the tol-0 denmark regress
plus the in-tree 2,000-case differential oracle (including the
warm-engine recycle test) exist, most of this surface is
representation- or schedule-only - values and order unchanged - and
lands under the SAME bit-identical gate as Landing 2 did, no
re-blessing. Only changes that alter noding (E4c below) need a future
campaign willing to rotate the baseline. Port history and rationale:
git log `d570daa`..`659a187` plus the Landing 2 commits after
`8eaa8bf`.

- **E1: input-shaped fast paths on the 11.4M-call normalize -
  CLOSED 2026-07-13, below threshold.** The single-contour arm already
  has a "perfect input" verdict (`simplify_contour_into` returns false
  and the caller keeps its own allocation), but reaching that verdict
  still pays the full segment build + split solver per call - on
  contours that are post-DP tile-space rings, typically tiny and
  usually simple. `emit_cell` proved the pattern pays: its
  convex-single-ring screen skips normalize entirely. E1 implemented a
  strictly-convex, CCW, non-self-intersecting screen
  (`is_perfect_ccw_convex`) plus a fall-through instrument (per-call
  pass/reject + size buckets + perfect-return counters), landed at
  `65ae629` and measured on a norway locations bench. The screen was
  mispriced: it passes f=0.272 of single-contour calls
  (pass 5,282,144, reject 14,118,318, total 19,400,462), below the
  spec 0.30 proceed threshold. The richer counters make it worse:
  `normalize_screen_perfect_return` 13,610,939, so pass+perfect_return
  is 18,893,083 - a 0.974 ceiling the strict screen captures only 28%
  of. Size buckets: passes are n3 82,732 / n4_8 4,923,309 /
  n9_32 276,069 / n33p 34 (93% cheap n4to8 solves); rejects are
  n4_8 4,445,750 / n9_32 7,631,905 / n33p 2,040,663 - the expensive
  rings are mostly rejected and mostly perfect-return, so
  solver-cost weighting sinks the flat 0.272 further. Verdict:
  strict convexity is the wrong screen; the 97% ceiling is a real
  opportunity but belongs to a broader exact classifier (E1b).
  Instrument removed (the per-call scan + shard counters in
  `src/debug.rs`); `is_perfect_ccw_convex`, its slow-body reference
  helper, and the soundness tests are retained under `cfg(test)` in
  `src/geometry/overlay/port/simplify.rs` as the predicate and gate
  for E1b. See reference/performance.md for the dated evidence.
- **E1b: an EXACT perfect-CCW classifier priced by avoided solver
  cost - CLOSED 2026-07-13, below threshold.** The E1 ceiling (`perfect_return` 0.974 vs strict-screen pass
  0.272) says the opportunity is real but the screen must be exact, not
  merely convex. Design: keep strict convexity as an immediate accept,
  then for non-convex rejects test the actual engine invariants - no
  dropped collinear/duplicate vertex, no crossing/overlap, no
  repeated-vertex loop, and correct CCW winding - accepting only rings
  the engine would return `false`/untouched on (the same soundness
  invariant the retained tests pin). Price it per size bucket by
  AVOIDED SOLVER COST, not raw call fraction: add perfect_return size
  buckets to the instrument so the pass fraction is weighted by the
  solver work each bucket actually skips (the E1 buckets show the
  passes cluster in cheap n4_8 while the costly rings are the rejects).
  The classifier needs a proved large-ring cutoff or a solver fallback
  - a segment-pair test can duplicate the split solver's own work at
  large n, so beyond some n it stops being cheaper than the body it
  replaces. Set a NEW proceed threshold on avoided solver cost rather
  than the flat call ratio that mispriced E1. Landing 1 shadowed the
  conservatively-sound classifier on norway locations: 13.651s avoided solver
  time minus 38.564s classifier cost gave -24.914s net, with zero unsound
  accepts. This fails both positive-net and the 6.5s (five percent of the
  pre-instrument normalize baseline) proceed thresholds. The instrument and
  classifier were removed and Landing 2 was not attempted. See
  `reference/performance.md` for the measured close.
- **E2: flat point+range output at the module boundary.** Evidence:
  the engine already builds every output contour flat -
  `BooleanExtractionBuffer.points` is one `Vec<IntPoint>` - and then
  copies it into a pooled per-ring Vec (`take_contour_from_points`),
  where `take_vec_with_capacity` linear-scans the pool per take. The
  entire two-level recycle protocol Landing 2 added (shape shells +
  ring bodies, the six-method take/recycle pool API on `BoolOverlay`)
  exists only to keep the nested `Vec<Vec<Vec<IntPoint>>>` boundary
  alive. Downstream consumers (`emit_cell` -> `encode_tile_shape`,
  `clean_shapes_in_place`, the pyramid descent) iterate rings
  sequentially and never need owned per-ring Vecs. Theory: replace
  `Shapes` at the boundary with one points buffer plus a
  (shape, ring) range index; the copy, the pool scan, and the recycle
  API all delete. Same values, same order - gate: bit-identical.
  BOUNDARY-ONLY PROTOTYPE DESIGN-REFUTED 2026-07-13 (spec written,
  two reviews, codex xhigh adjudication; no code written). Static
  caller ordering already proves the prototype is NET-NEGATIVE, so no
  Brick-0 measurement was spent: every production caller recycles `out`
  before calling the producer, so the `materialize_nested` seam
  converter gets an empty `out` and must redo the existing per-ring
  pool scan and nested copy AFTER the new flat-buffer copy - an added
  copy, not a deleted one. And `FlatShapes::clear` retains all
  capacities, growing to each worker's fattest overlay result ON TOP of
  the still-needed nested pools - the exact additive per-worker
  retention that reverted E3. The alloc gate cannot even price this
  (the extraction copy is a warm-pool memcpy, zero allocations; the
  real cost is CPU and the linear `take_vec_with_capacity` scan). The
  engine de-churn surface (E1-E6) is PAUSED here per the sequencing
  note below: it does not outrank open phase-level work and is not a
  dedicated campaign. Flat storage returns ONLY as E5 - delete the seam
  converter so the flat buffer REPLACES the nested pools rather than
  adding to them - and only as a phase-level campaign instrumented on
  copied-point bytes, pool-probe CPU, and retained bytes (never alloc
  attribution), following evidence about the remaining bytes rather
  than preceding it. Note for any future bit-identity gate: `cmp -s`
  over complete deterministic archives, NOT `brokkr compare-tiles`
  (which samples 200 tiles/zoom and compares aggregate counts, never
  proving byte identity).
- **E3: pool `clip_shape_rect_fast` through the reconnection logic -
  IMPLEMENTED, MEASURED, CLOSED AS REGRESSIVE 2026-07-13 (reverted).**
  The target was real: alloc pricing at `32bda50` put
  `clip_ring_half_plane_multi` at 14.9 GB + `clip_shape_rect_fast` at
  4.1 GB = 19.0 GB combined exclusive churn on denmark locations, the
  next-largest sound churn surface and the one Landing 2's review had
  flagged and declined. The pooled rewrite (threaded `IntEmitScratch`
  in, ping-ponged two pooled contour lists across the four half-plane
  passes, pooled shells for components) landed at `d1f26b6` and WON the
  churn gate: the pair dropped to ~3 GB combined (82 to 87 percent
  reduction, clearing the 80 percent gate), total run churn 571 ->
  554 GB, output byte-identical (compare-tiles +0% on every layer/zoom,
  earcut clean). But both perf gates FAILED - denmark wall
  13,400 -> 14,300 ms (+6.7 percent, outside the ~5 percent noise band)
  and peak RSS 3.54 -> 4.36 GB (+23 percent, far over the 2 percent
  gate); the alloc run corroborated with end-of-run retained memory
  161 MB -> 1.2 GB. The regression is per-worker shared-pool retention
  growing to the fattest tile - the SAME input-scaled retention pattern
  this roadmap deleted twice (the per-thread AssemblyScratch pool,
  69c0f18; the ocean PyramidScratch pool, H4 rip-out), so E3
  reintroduced an abandoned architecture, and a bigger dataset only
  gives an outlier tile MORE chances to poison every worker's pool -
  `d1f26b6` was therefore not benched on NA and the campaign did not
  restart from a later brick. The 19 GB churn surface remains real
  evidence that this clip deserves a STRUCTURAL fix, not indefinite
  pooling: the aligned answer is E2's flat point+range storage at the
  engine boundary, which removes the copy, the linear pool scan, and
  the recycle protocol rather than retaining their capacities. If
  rect-clip churn is revisited it must be a new item under E2, or an
  E3b that specifies a bounded retained-byte budget BEFORE
  implementation and starts from the pre-E3 code state - never from
  this pooled design.
- **E4: fusions, in three gate classes.** (a) Iterator-fed segments:
  `add_contour` consumes any point iterator (`append_path_iter`), so
  producers that today materialize a `Shape` purely to hand it to
  `normalize_into`'s multi-contour arm can feed segments directly -
  `root_fragments` copying the base shape into a pooled `Shape` just
  to transfer ownership is the concrete instance, and the same move
  is what a fused quantize-to-segments path would look like for the
  `emit_polygon_feature` residue (4.8 GB post-H6). Gate:
  bit-identical (same segments). (b) Rect-specialized boolean: the
  clip in `intersect_rect_into` is always a 4-segment axis-aligned
  rect, yet it runs the general cross solver and sweep; axis-aligned
  crossings are single exact divisions and fill-vs-rect is an
  interval test. Plausibly bit-identical if the rounding is
  reproduced exactly - the oracle decides, cheaply. Only 6,550
  calls/2.3 GB on denmark, so this is a churn-and-code-size play,
  not a wall play. (c) Fuse the guarded integer S-H fast path with
  the boolean (extend fast-path coverage, skip the overlay for more
  of the common cases): the two paths nod differently by up to one
  unit along the cut line (documented in the pyramid XOR tests), so
  this CHANGES BITS - re-bless territory, for a campaign that wants
  it badly enough to rotate the baseline.
- **E5: caller-provided buffers end to end.** E2's ranges extended
  through the whole emission chain: quantize -> DP -> normalize ->
  `encode_tile_shape` -> wire record as one per-worker arena, no
  intermediate ownership transfers. This is the H6 endgame
  (remaining sinks: `add_feature_to_layer` 5.0 GB assemble-side,
  `merge_same_attr_geometries` 3.8 GB), and it is speculative in
  shape - do it after E1/E2 have shown where the remaining bytes
  actually are, not before.
- **E6: knobs the port made ours (lateral finds).** The solver
  thresholds (list/tree split at 4,000 segments, fragmentation at
  16,000, list fill at 8,000) are upstream's generic tuning; our
  distribution is bimodal - millions of tiny ops plus rare huge
  coastline shapes - and the thresholds are now a measurable knob.
  The oracle already pins each forced strategy point-for-point, and
  whether strategies agree on our data is exactly what the tol-0
  regress answers for free. Likewise the arithmetic is ours: the
  cross solver and fill sweep are i32-monomorphized with exact i64
  cross products, a fixed-lane-width shape that admits vectorization
  without changing results. Both speculative on payoff; both cheap
  to price (one norway hotpath run per variant).

Sequencing honesty: none of this outranks the open phase-level items
(H5 ocean stream, H8b assemble stragglers, H4 I/O pricing) on planet
leverage today. E1-E3 are the ones with measured evidence behind
them; they belong in the next campaign that is already in this code
region, not in a dedicated engine campaign.

**Allocator addendum (2026-07-08).** mimalloc predates all measurement
here (early experiment, never defended at current scale), and pbfhogg
reached record numbers on the plain system allocator - after its
arena/scratch work removed the churn. Same sequence applies: once H6
lands, run a three-way A/B (mimalloc default-feature vs system via
no-default-features vs jemalloc 5.3.1 via tikv-jemallocator 0.7, which
now tracks the release with the dealloc-only-thread tcache work aimed
at exactly our decode-worker -> consumer free pattern) on germany +
norway locations. Decision rule: system allocator within wall noise ->
delete mimalloc (simpler main.rs, two deps gone, glibc mallinfo2
becomes a live signal again); jemalloc stays on the table only for an
RSS/retention win the 30 GB budget cares about. Today's measured
retention (mi_commit 2.4 GB above peak RSS, see H3 first reading) is
the number to beat. Not a pre-H6 priority: at 622 GB/run churn the
allocator comparison would measure churn H6 is about to delete.

**PROMOTED TO FIRST IN LINE (2026-07-14).** Both preconditions are met and
the framing above is now too modest. H6 has landed, so the A/B no longer
measures churn about to be deleted. And retention is no longer 2.4 GB
above RSS: mi_commit is 44.4 GB against 23.3 GB resident at NA and 16.2 GB
against 6.58 GB at germany, roughly 2.2x its recorded value on both. That
is the largest single line on the 30 GB ledger by a wide margin and it is
what makes planet RAM NO-GO - see the H3 void notice.

Run it at HEAD on NA and germany locations, not norway: the arms are
`--features sys-alloc` and `--features jemalloc-alloc` against the
mimalloc default (`f2184ce`, `a32c960`), and brokkr passes `--features`
through. Read `mi_commit_phase12_end` and phase12 majflt, NOT peak RSS -
germany's RSS hides this entirely. Prior evidence does not pre-empt the
result: `18e1656`'s three-way A/B ruled out retention for ASSEMBLE's
plateau (19.4 / 19.0 / 17.0 GB across arms - live memory), but phase12 has
never been A/B'd, and phase12 is where the commitment is taken and never
returned.

Decision rule is unchanged and now has teeth: if an arm collapses phase12
commitment, the rip-out stops being a simplicity decision and becomes the
fix.

A/B RUN 2026-07-15 (at pre-fix HEAD 76492b8, bench-1 arms, sidecar
readings). No arm collapsed phase12 memory - the regression was live
scratch retention, not the allocator (see the H3 resolution) - so the
rip-out is NOT the fix. But the arms answered the standing simplicity
question in the system allocator's favor on germany: sys-alloc 60s wall
/ phase12 34.3s at 18.2 cores / 5.56 GB, jemalloc 60s / 35.0s / 5.55 GB,
mimalloc 68s / 42.7s at 14.4 cores / 6.11 GB - both alternatives beat
mimalloc by ~12% wall at germany, with half the minor faults. At NA
(pre-fix, under fault-storm confound) sys-alloc was worse (413s vs
314s); that arm needs a POST-FIX re-run before the rip-out call. What
mimalloc still costs: mi_commit frozen at 2-3x live (14.2 GB on germany
against 4.2 GB anon, never returned), which on a 30 GB host is address
space the ledger cannot trust. Next step: re-run sys-alloc on NA and
germany at the fixed commit; if wall holds, delete mimalloc (pbfhogg
precedent - it dropped mimalloc long ago; user leans the same way).

DECIDED 2026-07-15, mimalloc REMOVED. Post-fix arms at `98824b4`
(stored bench-1 runs): NA sys-alloc `001930af` 269s wall / phase12
155.0s at 17.9 cores / 17.6M minflt vs mimalloc `ee259d09` 282s /
170.8s at 16.1 cores / 50.4M minflt; germany sys-alloc `9399d48c` 60s /
phase12 31.5s at 19.7 cores vs mimalloc `050f0de1` 65s / 39.3s at 15.8
cores. The system allocator does not merely hold within noise - it wins
wall on both datasets - so the decision rule fires on its stronger
branch. jemalloc measured at par with sys-alloc on germany (60s) and
does not pay for its dependency. Removed: the mimalloc/libmimalloc-sys/
tikv-jemallocator deps and the three feature arms; `mallinfo2` is a
live signal again and the sidecar now emits `malloc_held_<boundary>` /
`malloc_live_<boundary>` in place of `mi_commit_*` (old rows keep the
old names). Germany's frozen 14.2 GB mi_commit line leaves the ledger
with the allocator that produced it.

PROMOTED 2026-07-09: the NA claim-window run showed the assemble
phase's 19.4 GB RSS is mostly allocator retention (mi_commit 14.9 GB
at phase12 end vs ~2 GB live; 24.5 GB committed by run end) - the
single largest planet-ledger line after the read-loop fix. The
three-way feature split landed at `f2184ce` (default mimalloc-alloc /
--no-default-features system / --features jemalloc-alloc); NA A/B
running. H6 churn reduction remains worthwhile independently, but the
allocator decision no longer waits for it.

### H7: The relation stack is norway's tax today and the planet's tomorrow

**Claim.** Multipolygon assembly + emission is the second-largest CPU
block after phase12's way path, and planet scales it by every coastal
geography on earth simultaneously.

**Evidence.** Norway hotpath: 267.8 + 243.9 thread-s in the
relation/multipolygon pair. `prepare_relation` is still serial end-of-read
work (backlog item 18, open). Single features fanning to 22,460 tiles.

**Theory.** Three tiers, cheapest first: (1) item 18 - parallelize
relation preparation (rayon over buffered blocks); (2) H2a's injected
relation plan removes the buffering-and-matching machinery; (3) if
profiles still bleed, precompute ring assembly adjacency at altw time
(H2 family - pbfhogg already walks member ways). Fanout caps
(`--fanout-cap`) remain the policy backstop for pathological features.

**First step.** NA locations re-baseline (below) to see what the stack
costs at 4x norway scale post-P1/P2/P3, before choosing a tier.

MEASURED 2026-07-08 (H1 counters, norway locations `63eade98`): the
serial relation tail is 31.7s of norway's 43.6s phase12 - 73% of the
dominant phase on coastal data. Tier 1 (parallelize relation prep,
backlog item 18) is promoted into the H1+H6 campaign as shard 2; see
the H1 verdict block.

### H8: Overlap the phases; stream the archive

**Claim.** The four-phase sequential structure leaves whole phases of
parallelism unused. Ocean input is independent of the PBF; partitions are
independent of each other.

**Evidence.** Phase12 averages 9 cores of 24 (15 idle); denmark's wall is
literally phase12 + an equal ocean phase back-to-back (6.1s + 5.5s of a
14.4s run). Assemble partition batches already stream (P3), but only
after sort completes.

**Theory.** (a) Run ocean concurrently with phase12 - with H5 this only
matters for cache-cold runs, without H5 it hides the entire ocean phase
behind phase12's idle cores; the shared sort-writer is the only
coupling point. (b) Straggler mitigation: more, smaller partitions +
recursive splitting of hot ones (H2d's injected stats pick the
boundaries), lifting assemble from 12.3 avg cores toward norway's 20.
(c) Longer-term: as partitions complete, their tile payloads can be
written at final offsets (PMTiles payload section order is partition
order) - the packer assigns offsets centrally, workers pwrite
(pbfhogg technique 15) - shrinking the tail where the writer drains
alone.

**First step.** (b) is a knob + counter exercise on existing machinery.
(a) needs a small spec (sort-writer sharing). (c) waits for H3's
directory audit.

RE-MEASURED 2026-07-14. The knob in (b) is already turned and the NA half
of the claim is dead; the germany half is alive and unchanged.

- H8b's "more, smaller partitions" landed at `e34cc7b` (equal-width -> z6
  -> z7), the day after the NA evidence above was taken. NA assemble is
  now 91.5s at 14.9 cores (`b9d6c12c`), against the 186.4s that made
  next-session item 3 call it "the biggest phase at NA". It is now less
  than half of phase12's 206.3s. `e34cc7b`, `33ce85e`, `c6d4e16` and H5's
  artifact did this between them; no straggler campaign remains at NA.
- The germany figure at the top of this section is EXACT and current:
  germany assemble still averages 12.3 cores at HEAD (`2d715357`,
  16.7s), unchanged from `92803833`. Recursive splitting of hot
  partitions is still untried and still the live part of (b). Norway's
  20.0 remains the comparand.
- Do not read the 12.3 as an NA number. It is germany's, and conflating
  the two is what made this item look bigger than it is.

### H9: Planet measurement is its own workstream

**Claim.** We cannot profile the planet the way we profile denmark, and
we should not discover that on the first planet attempt.

**Evidence.** Germany hotpath/alloc OOM on this host (today). NA baselines
are 4 months stale, pre-P1/P2/P3. sidecar.db survives OOM by design;
hotpath builds do not.

**Theory - the measurement ladder to planet:**
1. NA locations-on-ways re-baseline (18.7 GB raw; needs an altw-enriched
   NA variant, a pbfhogg run). Predicts the linear model's slope at 3.4x
   germany. Also the right scale for H4's I/O A/B and the RAM ledger's
   first extrapolation check.
2. `perf record` sampling instead of hotpath instrumentation for anything
   bigger than norway - no RSS cost, ranks functions the same way.
3. Planet enriched artifact + disk audit (scratch needs ~200 GB + output
   ~60-70 GB + input ~90 GB on the same SSD budget).
4. First planet run only after the H3 ledger predicts peak RSS under
   ~20 GB with the measured per-unit constants.

**First step.** Decide the NA enrichment run (user decision - real-PBF
runs are explicit per project rules), and add the H1/H3 counters before
it so one expensive run answers many questions.

Measurement discipline for NA-and-larger inputs: use `--bench 1`, not
the best-of-3 default - a NA best-of-3 costs ~18 minutes of machine
time for variance data the ladder does not need yet. Best-of-3 stays
the standard for extract-scale runs and for record-claim runs (H10),
where the variance actually matters.

### H10: Define the record so the claim survives scrutiny

**Claim.** "World-record" needs a protocol, or it is marketing.

**Theory.** Publish, per dataset (germany, NA, planet): host spec, input
hash (brokkr.toml already carries XXH128), exact invocations, best-of-3
wall, peak RSS, and output-correctness evidence (verify + earcut oracle +
regress). Compare on the same host via the existing `brokkr planetiler`
and `brokkr tilemaker` benches, in two framings:

- **Enriched framing:** elivagar on the locations-on-ways input (the
  production shape). Fastest number; must disclose preprocessing.
- **Raw framing:** pbfhogg (cat + altw) + elivagar total wall vs
  planetiler/tilemaker on the same raw PBF. The apples-to-apples number -
  planetiler resolves nodes internally, so our node-handling cost must
  appear somewhere in the comparison.

Existing same-host reference points are stale (tilemaker denmark 29.6s vs
elivagar 14.4s today; planetiler numbers in `.brokkr/results.db` predate
the current pipeline) - re-run the competitor benches when a claim is
made, not before. For planet-scale claims the external reference is
planetiler's published planet-log table (`research/planetiler/README.md`):
19m on 192 cpu / 720 GB (v0.10.1, 2026), 29-42m on 64 cpu / 128 GB,
2h38m on 16 cpu / 32 GB, 3h35m on 8 cpu / 16 GB. The claimable record is
the resource-classed one (<= 30 GB RAM); wall time against the big-iron
numbers is reported for context, and profile differences (OpenMapTiles vs
Shortbread) are disclosed.

## Proposed sequencing

Instrument-first, then the two structural attacks, then planet.

1. **Counters, no behavior change:** H1 drain split, H3 RAM ledger, H4
   PSI/majflt reading habits. One small landing.
2. **NA locations re-baseline** (H9 step 1) with those counters - one
   expensive run pricing H1, H3, H4, H7 simultaneously. Requires the
   enriched-NA artifact (pbfhogg altw run) and a user go-ahead.
3. **H1 + H6 spec** (phase12 drain removal + way-path scratch) - the
   biggest lever, one campaign.
4. **H5 spec** (durable ocean stream) and **H2a spec** (injected relation
   plan, paired pbfhogg change) - independent, parallelizable as specs.
5. **H3 ledger extrapolation -> planet dry-run decision** (H9 steps 3-4).
6. **H10 record runs** once the planet number exists.

## Open questions the first measurements must answer

- Actual enriched-planet PBF size and its blob-type byte split (pbfhogg
  can answer from an existing planet artifact without elivagar running).
- Planet unique-tile count and PMTiles directory entry volume - decides
  whether the directory build needs the H3 external design.
- NA-scale slope: does the 71 MB/s germany rate hold at 3.4x, or do
  relation/ocean terms bend it before planet?
- Reorder-buffer byte bound under consumer-bound backpressure (H1
  counters answer this for free).
- Whether tile serving requirements (gzip vs brotli, MVT vs MLT) change
  the assemble CPU budget the record is computed against.
