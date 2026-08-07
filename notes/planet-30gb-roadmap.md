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
(`scripts/validate/earcut-oracle.mjs`), and `brokkr pmtiles-corpus check`
against the committed `corpus/denmark/` baseline (native brokkr code over
the linked elivagar crate since the 2026-07-24 corpus redesign). Nothing in
the current pipeline structure is protected.

Companion: `notes/virtual-planet-serving.md` - the hypothesis that
production may never store a full planet archive at all (on-demand
generation + cache over the record store, elivagar as a library). The
full-build track in THIS document is unchanged by it: cold start,
disaster recovery, and the committed corpus baseline the serve path verifies
against. The two tracks share instruments (H2d/H8's per-tile-range index,
H3's ledger) and should not diverge on them.

## Where we stand (bygg, 2026-07-31)

**The host moved.** Every baseline below plantasjen's line is a
different machine and the two are NOT comparable run-to-run; bygg is
~1.9x on the same commit. bygg: Ryzen 9 9950X3D2, 16c/32t, 30.5 GiB
RAM, single WD_BLACK SN850X 4TB NVMe holding root, data, scratch and
target.

bygg, commit `4a1958d`, locations variants, `--bench 1` unless noted:

| dataset | bygg | plantasjen | run |
|---------|------|-----------|-----|
| denmark  | 6.9s (plain run) | 8.8s best-of-3 | - |
| germany  | 26.7s | 53.4s | `c76eb594` |
| north-america | 130.1s | 251.0s | `d96003d3` |
| **planet** | **571.7s** | never ran | `a5b3df34` |

NA on bygg by phase: phase12 76.9s at 23.9 avg cores, ocean 8.5s,
assemble 44.2s at 20.5 cores; peak anon 5.2 GB phase12 / 6.1 GB
assemble.

Two corrections to older text that this leg forced. **Ocean is 8.5s at
NA, not the 0.2s the go/no-go quotes** - that figure predates the v3
union and the v4 ring-cap artifact, and the boundary band now costs
single-threaded seconds at extract bbox. At world bounds it really is
0 (planet measured `ocean_ms=0`), so the planet projection is unharmed,
but "the artifact deletes the phase" is now true only at world bounds.
And **`prepass_join` costs 6.55s at NA on bygg**, the H2 re-enrichment
item's price on this host.

Current frontier, post-planet: phase12 is still the biggest phase
everywhere (61% of planet wall even after the way-budget landing), and
after that fix its cost is genuinely way-path/relation CPU - the paused
H6 engine surface and H2c/d are the known levers. Assemble is 38% of
planet wall at 18.3 avg cores. H8b's hot-partition splitter fires at
planet (`assemble_split_partitions=385`, `assemble_split_pieces=2043`)
and both it and the reader/encode overlap remain unpriced as isolated
changes, though both were active in every number above.

## PLANET RAN, 2026-07-31, bygg: 1062.5s, then 571.7s

Two full planet builds the same afternoon. The second differs from the
first by ONE config value - `way_budget = "6G"` in
`[bygg.tilegen.default]`, emitting `--way-budget 6G` - and it is the
headline of the day:

| run | way budget | wall | phase12 | phase12 cores | peak RSS |
|-----|-----------|------|---------|---------------|----------|
| `e27ca35a` | 768M (old default) | 1062.5s | 836.3s | 7.2  | 12.70 GB |
| `a5b3df34` | 6G (config)        | **571.7s** | **351.3s** | **23.1** | 12.69 GB |
| (forced)   | 8G (NEW default)   | 577.7s | - | - | - |

The third run carries no config at all - it is the raised
`DEFAULT_WAY_BUDGET_LOCATIONS` landing below, confirming the win is now
the out-of-the-box behaviour. It is within 1% of the 6G config run,
which is also the only variance sample we have at planet scale.

**-46.2% wall, 1.86x, for a config value and ~1.1 GB of peak anon**
(6.18 -> 7.33 GB in phase12; peak RSS is flat because the mmap'd way
index dominates). Diagnosis, mechanism and the reason the default was
wrong are in H3's fat-blob item below. `way_budget` at 6G is NOT the
optimum - `way_budget` is still the top elivagar stall at 190.2s /
33.3% of the new wall, so more is available; the sweep belongs on a
fat-blob EXTRACT, not on 10-minute planet runs.

The detail below describes the FIRST run (`e27ca35a`) and its counters,
which is the fully-instrumented, verified one. Where the second run
differs it is noted.

First full planet build. Run `e27ca35a`, commit `4a1958d`, `--bench 1`,
input `planet-20260223-locations-prepass.osm.pbf` (90.5 GB enriched,
xxh3 `2cc18188...`), host bygg (Ryzen 9 9950X3D2, 16c/32t, **30.5 GiB
RAM**), output 58.7 GiB / 63.06 GB PMTiles.

**Wall 1,062,500 ms = 17m42s**, against this document's predicted band
of 1050-1400s. The optimistic end of the band was right.

| phase | wall | share | avg cores | peak anon |
|-------|------|-------|-----------|-----------|
| phase12  | 836.3s | 78.7% | 7.2  | 6.18 GB |
| ocean    | 0.0s   | -     | -    | -       |
| sort     | 1.4s   | 0.1%  | 1.0  | 5.48 GB |
| assemble | 224.0s | 21.1% | 17.9 | 7.75 GB |

**Peak RSS 12.70 GB on a 30.5 GiB host.** The RAM go/no-go is now
measured, not extrapolated: predicted "under ~10 GB", observed 7.75 GB
peak anon with the rest being the mmap'd way index. Headroom is large.

Output: 269,815,541 tiles addressed, 52,182,158 unique (80.7%
deduplicated - the roadmap's 60-70M unique estimate was high),
56,922,829 directory entries, 2,518,077,036 features, 26 layers,
z0-z14.

Ledger items that fired exactly as designed: `ocean_ms=0` and
`ocean_features=0` (at world bounds the boundary band is empty by
construction, so the H5 artifact serves the whole planet);
`relation_blocks_spilled=1` at `relation_blocks_bytes=1.91 GB` (the 1 GB
cap and its BlobFilter re-read path, first exercise at planet);
`max_rel_inflight_bytes=200 MB` (the monster-relation watch item, still
bounded, was 90 MB at NA); `missing_way_node_refs=0`,
`missing_relation_way_refs=0`; `ring_cap_partitions=30` /
`ring_cap_pieces=530` (OCEAN_POLICY_VERSION v4 partitioning at planet);
`way_pins_marked=2,163,374,442` and `way_members_marked=37,214,953` from
the injected prepass, with `relation_plan_needed_ways=0` confirming the
runtime prepass never ran.

**H3's open dedup-cap pricing item now has its number**: the 1M cap
skipped **51,182,158** inserts - i.e. all but 1M of the 52.2M unique
payloads - while still reusing 217,633,383 tiles and saving 13.18 GB.
Price the cap against that before an H10 record claim.

Sort: 290 chunks, `sort_merge_max_fanin=290`, 281.5 GB merged,
251.3 GB of sort records. Scratch was uncompressed (H4's lz4 planet
configuration was NOT used on this run - so H4's open "does the lz4
trade invert at planet" question is still open, and this run is its
uncompressed control).

Oversize: 1 severe, 44 warn, max tile 1.08 MB at z14/13722/7013.

### Where 571.7s sits against the target (NOT a claim, see H10)

planetiler's published table, resource-classed: **2h38m = 9480s on
16 cpu / 32 GB**. This build is 16 cores / 30.5 GiB at **571.7s**, i.e.
**16.6x** in the same resource class. planetiler's best ABSOLUTE number
is 19 min = 1140s on 192 cores / 720 GB, which 571.7s also beats by 2x
on ~1/12th the cores and ~1/24th the RAM.

Do not publish that as-is. It is the ENRICHED framing (H10), and the
preprocessing is not in the number: the altw enrichment alone was
~603s at planet in pbfhogg's own measurements, so the raw framing is
roughly 571.7 + 603 + the cat/index pass, call it ~20 min end to end -
still ~7.9x the resource-classed reference, and that is the honest
apples-to-apples figure. Two further disclosures H10 requires and this
section cannot skip: the profile differs (Shortbread vs OpenMapTiles,
26 layers), and the planet snapshots differ (ours seq 4912, 2026-02-23).

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

1. RESOLVED 2026-07-31: the enriched planet PBF exists on bygg.
   `planet-20260223-locations-prepass.osm.pbf`, 90.5 GB, seq 4912,
   registered as `[bygg.datasets.planet.pbf.locations]`. Built by
   `pbfhogg add-locations-to-ways --index-type external --inject-prepass
   --compression zstd:1` from pbfhogg's seq-4912 indexed planet; header
   carries `pbfhogg.WayMembers-v1` + `pbfhogg.SharedNodePins-v1`, element
   counts identical to the plain-altw file, `0 missing locations`. The
   90 GB input assumption above was a good guess. NOTE the older
   `planet-20260223-altw.osm.pbf` in pbfhogg/data carries NEITHER injected
   feature - it is the plain arm, and pointing a run at it silently buys
   back the runtime prepass. H2's NA re-enrichment reading is still
   pending and no longer rides this decision; NA locations is confirmed
   prepass-free by its header.
2. RESOLVED 2026-07-31 for bygg: the H9 step-3 disk audit. 2.3T free on
   the single root NVMe against input 90 GB (landed) + scratch ~100-130 GB
   with lz4 + output ~60-70 GB. The old 607 GB figure was plantasjen's
   and no longer binds. altw's ~224 GB external temp was transient and
   has already been paid.
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

**DONE 2026-07-24: the brokkr bless-machinery teardown landed and is
verified.** brokkr commits 43e8e51 (bless/blessed-field/provenance-gate
teardown + explicit two-archive regress), 74cd96f (`pmtiles-corpus`
namespace), ce63405 (`ocean-build` from the tilegen ocean block); the
wrapper design of record lives in reference/corpus.md. All acceptance
gates ran green on plantasjen the same day: unknown-command `bless`,
comparand refusal at exit 2 (never 1, the regress verdict code),
wrapped-vs-raw corpus check parity on a fresh denmark locations build,
drop-tile firing with the tile named, regzip clearing, raw-variant
contract refusal at exit 2 with fields named, bless-without-rotate
refusing with nothing written, and the ocean-build dry-run deriving
the production ocean block. Both standing hazards are gone: `brokkr
bless` is an unknown command and `datasets.<D>.blessed` fails config
parse by name. Every elivagar invocation in the dev flow now has a
brokkr spelling.

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

PRICED 2026-08-07: the 1M dedup cap costs 777 MB of archive at planet
(1.2%) and saves ~4.9 GB of RAM. A/B on bygg via the new `--dedup-cap`
flag (brokkr passthrough same day): uncapped-at-60M arm `4994452d`
against capped control `ac2cf674` - output 62.28 vs 63.06 GB, unique
stored payloads 42.1M vs 52.2M (10M more tiles deduplicate, 227.7M
reused), directory entries 50.6M vs 56.9M, wall 596.2 vs 587.2s
(noise), peak RSS 17.25 vs 12.33 GB. The RSS delta is ~2x the map's
own 2.36 GB byte estimate - hash growth and per-entry overhead beyond
the 56 B/entry figure - so budget the real number, not the estimate.
Verdict: the default stays 1M; an H10 record run may choose
`dedup_cap = 60000000` for the smaller archive since 17.3 GB still
fits the 30 GB ledger comfortably, and must disclose the setting
either way (it is in cli_args).

**OPEN, UNPRICED 2026-07-31: the planet input's way blobs are ~7.5x
fatter than any extract's, and the way stage holds blobs in flight.**
Every extract in brokkr.toml packs 8,000 ways per blob (NA locations:
208.9M ways / 26,113 way blobs, 640 KB compressed each). The enriched
planet packs ~66,500 ways per blob (1.166B ways / 17,529 way blobs,
~4.8 MB compressed each) - and so did its input, so this is upstream
packing from planet.openstreetmap.org, not something altw chose. It
matters because P2's way-phase ownership rewrite sends the whole
`PrimitiveBlock` into the rayon task, with the in-flight ceiling scaling
on `config.threads`: in-flight way bytes are (blob size x concurrency),
and blob size is exactly the input-dependent term the RAM bullet above
calls "input-independent". Decoded, not compressed, so the real
multiplier is larger than 7.5x. Nothing here is known to break - 30
workers x a few tens of MB is survivable inside the ~10 GB prediction -
but the prediction was calibrated on 640 KB blobs and has never seen
this shape.

**ANSWERED 2026-07-31 by the full planet run, and it is not a RAM
problem - it is a THROUGHPUT problem, and the biggest one on the
board.** `max_way_inflight_bytes` came in at 183 MB, so the fat blobs
never threatened the ledger. But `way_budget` blocked for **674.4s of
the 1062.5s wall (63.5%)**, with `way_block_send` at 674.5s - the two
within 0.01%, so the way-budget gate IS what holds the reader.

The mechanism is a mis-calibrated admission estimate, not a real memory
bound (`src/pipeline/phase12.rs`, the in-flight condvar): admission
charges `decompressed_size * WAY_OUTPUT_MULTIPLIER` (10x) against
`DEFAULT_WAY_BUDGET_LOCATIONS` (768 MB), while the HWM counter records
the raw figure (`guard.1 / WAY_OUTPUT_MULTIPLIER`) - which is why 183 MB
raw and a saturated budget are consistent rather than contradictory. A
640 KB extract blob costs ~35 MB of budget, so ~20 admit concurrently
and NA runs at 23.9 avg cores. A 4.8 MB planet blob costs several
hundred MB, so 2-3 admit and planet runs at **7.2 avg cores of 32**.
The 10x multiplier and the 768 MB default were both calibrated on
8,000-element blobs (the code comment cites a germany-at-256M reading);
planet packs ~66,500.

This also retires the "phase12 stocks are input-independent" phrasing
in the RAM bullet: the stock is bounded, but the ADMISSION RATE is a
function of upstream blob packing, which is an input property.

The fix is a config value, not code: `way_budget` in
`[<host>.tilegen.default]` emits `--way-budget`. **Measured the same
day: `way_budget = "6G"` took planet from 1062.5s to 571.7s (-46.2%),
phase12 836.3s -> 351.3s, avg cores 7.2 -> 23.1, for ~1.1 GB of extra
peak anon and no change in peak RSS.** Run `a5b3df34`.

**LANDED: `DEFAULT_WAY_BUDGET_LOCATIONS` 768M -> 8G.** Swept on a
fat-blob germany proxy rather than on planet, because a planet arm is a
10-minute run and this needed four of them. Build the proxy with
`pbfhogg repack --elements-per-blob 66000` (registered as germany's
`locations-fat` variant): 4.36 MB blobs against planet's 4.8 MB, and it
reproduces the regime honestly - `way_budget` blocks 66.9% of wall on
the proxy vs 63.5% on planet at the old default. Note repack drops the
injected prepass metadata and warns; the proxy is for sweeping a knob
against itself, never for cross-input baselines.

| way_budget | fat germany | normal germany |
|-----------|-------------|----------------|
| 768M | 39.30s | 26.7s |
| 2G   | 29.50s | -      |
| 6G   | **25.70s** | -  |
| 16G  | 25.70s | 27.5s  |

Flat past ~6G, and neutral on ordinary blobs. `max_inflight`
(= `config.threads`) explains the plateau exactly: at 6G the byte budget
admits ~24 blocks, at 16G the COUNT ceiling caps at 32, and 24 of 32
already saturates.

That reframes what the byte budget is for. Since admission stops at
`max_inflight` blocks regardless, real in-flight memory is bounded by
(count x block size) whatever the budget says - so 768M was guarding a
case the count ceiling already covered, while throttling fat input to
2-3 blocks. 8G sits just past the knee and leaves the count ceiling as
the operative bound. Gates: `brokkr check` 669 passed, denmark corpus
check pass, planet output bit-identical to the pre-change archive.

LANDED 2026-08-07: packing-invariant admission. The `x10` multiplier
is gone - admission charges RAW bytes (decompressed block size, plus
each task's measured plan bytes once built) and the count ceiling
(threads) is the primary control, with the byte budget surviving only
as a safety net against individually huge blocks.
`DEFAULT_WAY_BUDGET_LOCATIONS` is now 4G raw (the 8G/x10 scheme's
~820MB raw equivalent was itself a workaround; 4G never binds on any
seen input - fat-germany `max_way_inflight_bytes` reads 649MB - while
still bounding a hypothetical 500k-element blob at ~15 in flight).
Priced on bygg same-day: fat proxy 27.6/28.1s new vs 27.7s old-rule at
HEAD, normal germany 27.9s vs 26.7-27.5s stored - parity both shapes,
denmark corpus check pass, `brokkr check` green. The
`max_way_inflight_bytes` HWM now records the charged raw figure
directly (no /10), so readings before this date are same-scale but
estimated differently. The repack backstop
(`pbfhogg repack --elements-per-blob 8000`) stays noted but should
never be needed now.

History note: the 07-14 planet-RAM NO-GO (NA phase12 at 23.3 GB / 1.9M
majflt) was scratch-capacity retention from the i_overlay port - THE
SIBLING PATTERN above - not the allocator; the A/B refuted the
retention theory first and the fix restored 5.6 GB anon with output
byte-identical. Full forensics in this file's git history and the fix
commit message (`98824b4`).

### H4: Sort scratch I/O - PRICED, planet answer in: uncompressed wins

- **ANSWERED 2026-08-07 on bygg: the lz4 trade does NOT invert at
  planet - uncompressed is the planet configuration.** A/B at the
  admission-rework commit: uncompressed control 587.2s (`ac2cf674`) vs
  lz4 662.0s (`bf2fdfe2`), +12.7% wall. Assemble carried the loss,
  224.5s -> 284.1s (+26%), phase12 +4% (compression on writes). The
  I/O prediction itself was right - physical assemble reads fell
  421 GB -> 170 GB - but on a single fast NVMe the decompress CPU in
  the merge readers costs more than the reads save. And a red flag
  beyond wall: assemble peak RSS read 24.5 GB under lz4 against
  6.7 GB uncompressed - the compressed-section read path holds far
  more resident than the streamed uncompressed path, which on its own
  disqualifies lz4 for the 30 GB ledger until understood. The read
  path itself streams (256 KB BufReader + FrameDecoder per
  ChunkReader; verified in src/sort.rs at this reading), so the
  suspect is per-open decoder/BufReader buffers times the merge
  fan-in (290 chunks at planet) times 8 workers, amplified by glibc
  retention across 117K partition opens - the sibling pattern's
  shape, unexamined further because lz4 is off.
  plantasjen NA pricing (+2.0% wall, 2.6x less scratch I/O) stands as
  history; spinning-disk or page-cache-starved hosts would need a
  fresh A/B.
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
re-scoped to seam gaps between adjacent features; (b) the corpus
gate (reference/corpus.md) would have flagged both tiles
mechanically - this incident was the concrete argument that got the
corpus built and the standing gate rotated to it the same day. Triage tooling from the hunt: `scripts/validate/
svg-roi.mjs` extracts the edges inside a bbox ROI from `brokkr svg
-o` dumps, comparing one defect region across archives without
reading whole tiles.

**LANDED 2026-07-19: low-zoom piece union, OCEAN_POLICY_VERSION v3
(`2ab6f83`, corpus rotation `a40c077`).** The corpus z2-x2-y1 tile -
the human layer on its first day - showed the cross-piece seam class
in the wild at low zoom, plus a second mechanism nobody had named: the
per-zoom min-area drop applied per cell FRAGMENT, deleting the smaller
half of any landform straddling a source-cell edge. Root cause was the
per-piece pyramid descent of the pre-split osmdata cells; the v2
artifact carried the identical defect (96% of the corpus tile's
coastline edges bit-identical in the artifact tile), killing the
"serve artifact interior at low zoom" fix. Resolution: the z0-z7 pass
unions its pieces before descent, the z0-z7/z8-z14 pass split is now
unconditional (full-only spelling serves both passes from the full
shapefile), and the artifact + chunk-resume keys carry v3. Full
calibration record: reference/performance.md, low-zoom ocean union
section. Adjudication tooling from the landing:
`scripts/validate/zoom-overlay.mjs` (tile fill under the same
archive's higher-zoom outline) and `ring-cap-census.mjs`.

**CLOSED 2026-07-26: ocean-build now retains the outgoing artifact one
generation deep.** The gap: `ocean-build` wrote `data/ocean-tiles.pmtiles`
in place, so every policy bump destroyed the only comparand for the
version it replaced - after the v4 ring-cap rebuild (2026-07-24) no v3
artifact existed anywhere on disk, and the one kept copy,
`ocean-tiles-dp-20260712.pmtiles`, is two simplifier generations back.
That comparand is the instrument the 07-15 stale-artifact hunt ran on
(dump the same tile from each archive, scan the same ROI with
`svg-roi.mjs`, diff the edges). Resolution: before a rebuild starts,
the existing artifact is hard-linked to
`<stem>-v<policy_version>-<build_date>.pmtiles` (one generation deep;
only names matching that scheme are cleaned up, so manual keeps
survive). Hard link, not rename: the active path is never empty and a
failed rebuild changes nothing. Full contract in `reference/cli.md`
(ocean-build section). NOTE the fix takes effect on the NEXT rebuild -
the v4 artifact currently on disk still has no v3 comparand; that
generation is simply lost. The sibling extract-side retention class
(brokkr's archive pruning ate the ring-cap spec's pinned regress
comparand `denmark-locations-da6995f.pmtiles` mid-landing) is brokkr's
to fix and remains open there; a spec that pins an archive across a
landing still has to account for that window.

**OPEN, UNGATED: the cross-piece ocean seam, now z8-z14 only.** The
07-19 union removed the class from z0-z7 outright (merged pieces have
no shared boundaries to disagree on). The full-resolution pass still
descends per piece with `pins: None`, so a boundary genuinely shared
between two source pieces, away from tile edges, is simplified
independently on each side and can open a seam - bounded sub-pixel by
the fixed per-zoom tolerances, which is why it stays unobserved. The
machinery exists unused (`quantize_polygon_pinned_into`,
`PyramidParams.pins`). No standing gate would catch it (earcut cannot:
a seam is a vertex in the wrong place, not a self-intersection). The
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

**CLOSED 2026-07-24: three full-pass ocean polygons exceeded
MapLibre's 500-ring clamp.** classifyRings silently drops all but the
500 largest rings of a polygon, so the excess hole rings in
z9/285/148 feat 10 (510 rings), z9/286/147 feat 4 (602) and
z10/546/260 feat 1 (725, artifact) were invisible to every consumer
and to the earcut oracle, which validates only retained rings.
Pre-existing across simplifier generations (both world offenders were
over the cap in the DP-era 2026-07-12 artifact); not caused by the
07-19 union, whose zooms max at 352 rings. Resolution
(`OCEAN_POLICY_VERSION` v4): the z9/285/148 offender turned out to be
ONE outer spanning the buffered tile plus 509 holes, so no
re-grouping helps - emission bisects an over-cap shape's clip rect
until every piece fits, halves closed at the shared integer cut
coordinate. Census now reads 0 over cap on the world artifact and on
denmark, every polygon layer. Calibration record:
reference/performance.md, ring-cap partition section; the spec note was
deleted after the landing and lives in git history.

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
  run per variant). Threshold pricing ATTEMPTED 2026-07-26,
  measurement voided by host contention (cross-project load halved
  available memory mid-session; an unrelated re-baseline came out
  uniformly ~1.9x slower per function). The one salvageable
  observation: list-heavy (16k/16k/16k) and tree-heavy (1k/16k/2k)
  arms landed within 1.6% of each other on norway hotpath wall,
  consistent with the call distribution (normalize_into P50 890ns -
  nearly every call is far below every threshold; only the P95+ tail
  routes differently). Verdict needs one clean back-to-back
  baseline-vs-variant pair on a quiet host; until then upstream values
  stand. Baseline for that pair: `cb9f55a4` (norway locations hotpath,
  69e829b, 32.7s - itself possibly cold-cache-high). Vectorization
  half untouched.

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
  campaign remains there). The GERMANY half LANDED 2026-07-26:
  hot-partition splitting - any partition over 2x
  `ELIVAGAR_ASSEMBLE_SPLIT_TARGET` (default 64 MiB of record bytes,
  known free from section-offset diffs) splits into contiguous
  tile-range pieces, boundaries at byte quantiles from a header-only
  pre-scan, each piece its own worker job and ordered-writer slot. The
  boundaries H2d would inject are thus picked code-side from measured
  bytes; H2d remains only a cheaper-source idea. Evidence and gates in
  reference/performance.md (H8b section): germany's Berlin z14-block
  partition was 297 MB encoded behind one slot with the writer starved
  12s while the claim window never bound; the denmark bit-identity gate
  (splits forced vs suppressed, identical archive hash) passed at the
  landing. PRICED 2026-08-07 on bygg (quiet): germany locations
  bench-1 A/B, splitter active `6518faea` vs suppressed via
  ELIVAGAR_ASSEMBLE_SPLIT_TARGET `43aa1f09`, back to back - 26.8s
  wall BOTH arms, assemble 6.8s vs 6.6s, parity. The 12s
  writer-starve the splitter was built against is no longer visible
  at germany scale, plausibly because the reader/encode overlap
  below also attacks that tail; the splitter stays as planet
  straggler insurance (385 partitions split there) at measured zero
  extract cost.
  Reader/encode overlap inside a worker LANDED 2026-07-30 (bygg): each
  partition worker stays the serial merge reader while a scoped encoder
  thread runs the rayon encode + artifact splice + writer send, fed by a
  sync_channel(1) of raw batches - the reader pulls batch N+1 while
  batch N encodes instead of idling through it. RAM bound: one extra
  raw batch (assemble_budget, 32 MB default) in flight per worker. The
  artifact cursor moved to the encoder thread where batches stay
  sequential; batch order is channel FIFO + the writer's batch_index
  parking, so drain order is unchanged and the denmark corpus check
  passed unchanged at the landing. New stall counter:
  assemble_encode_backpressure_wait_ns (reader blocked on a busy
  encoder). Wall win UNPRICED - and all stored baselines are
  plantasjen's, so pricing on bygg needs its own baseline leg first.
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

The go/no-go list above is now down to one gate: a green `corpus check`
at the attempt commit. The enriched planet PBF and the disk audit both
closed on bygg 2026-07-31. **bygg is a different machine and every plantasjen number needs
restating.** First bygg baseline, NA locations `--bench 1`, run
`d96003d3` at `4a1958d`: **130.1s** against plantasjen's stored 251.0s,
so bygg is ~1.9x. Per phase: phase12 76.9s at 23.9 avg cores, ocean
8.5s, assemble 44.2s at 20.5 cores. Peak anon 5.2 GB phase12 /
6.1 GB assemble. Denmark locations plain run 6.9s (was 8.8s best-of-3).
NOTE ocean is 8.5s here, not the 0.2s the go/no-go quotes - that figure
predates the v3 union and the v4 ring-cap artifact, and the boundary
band now costs single-threaded seconds at NA bbox. At world bounds the
band is empty by construction, so this does not touch the planet
projection, but the "artifact deletes the phase" line is now only true
at world bounds.

**Planet phase12 costs 2.3x more per GB than NA on the same host, at a
third of the cores.** Measured 2026-07-31, run `15fb2a6f`, phase12 only:
832s / 90.5 GB = **9.19 s/GB at 7.2 avg cores**, against NA's 76.9s /
19.06 GB = **4.03 s/GB at 23.9 avg cores**. Same binary, same commit,
same host, same locations shape - so this is neither host nor scale, and
the fat-blob decode hypothesis (H3) is the standing explanation. Peak
anon was 6.4 GB vs NA's 5.2 GB, near-flat, which confirms H3's bounded
stocks at planet scale for the first time.

METHOD WARNING, learned the hard way on that run: `--stop` kills the
child before elivagar's end-of-run counter flush, and BOTH the HWM
counters and the entire `*_wait_ns` stall set are flushed there. A
`--stop` run yields valid `/proc` series (wall, RSS, anon, cores, IO)
and pbfhogg's own periodically-flushed `pipeline_*` counters, and
NOTHING of elivagar's. An empty elivagar stall profile from a `--stop`
run is an artifact, not a finding - it was briefly misread as one here.

Optimization work that remains, none of it blocking: pricing the
landed reader/encode overlap (no suppression knob exists, so it needs
a revert-build A/B if ever wanted; the H8b pricing pair of 2026-08-07
could not isolate it), H2c/d injection candidates,
the paused E-surface (E6 thresholds await one clean A/B pair), and the
ocean needle detector if a geometry-level ocean gate is ever wanted.

## Open questions the first planet measurements must answer

- Actual enriched-planet PBF size and its blob-type byte split (pbfhogg
  can answer from an existing planet artifact without elivagar running).
- ANSWERED 2026-08-07: the 1M dedup cap costs 777 MB of archive (1.2%)
  and saves ~4.9 GB RAM at planet; verdict and the record-run guidance
  in H3.
- ANSWERED 2026-08-07: the lz4 trade does not invert at planet on
  bygg's NVMe - uncompressed wins by 12.7% wall and 3.7x assemble RSS
  (H4).
- Whether tile serving requirements (gzip vs brotli, MVT vs MLT) change
  the assemble CPU budget the record is computed against.
