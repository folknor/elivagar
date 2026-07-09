# Planet on 30 GB: hypotheses toward world-record tile generation

Status: 2026-07-08, hypotheses only - no code, no landings.

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
(`scripts/validate/earcut-oracle.mjs`), and `elivagar regress` against a
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
