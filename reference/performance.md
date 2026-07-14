# Performance: baselines, gates, and how to read them

The durable measurement record for elivagar performance work. Specs written
against `reference/technical-implementation-spec.md` cite THIS document plus
`.brokkr/results.db` as the measurement record: the pre-change baseline (host +
commit hash) a keep/revert verdict is read against comes from here, and after a
landing the post-change numbers are recorded here the same way.

## Discipline

- Every published number carries **host name** and **git commit hash** of the
  measured code. A number without both is not a baseline.
- Workflow: commit first, THEN benchmark, THEN write the hash-anchored numbers.
  Never benchmark uncommitted code (brokkr enforces a clean tree, ignoring
  `*.md` and `.brokkr/results.db`; `--force` runs are not stored and prove
  nothing durable).
- README.md performance numbers always come from plantasjen (the reference
  host).
- Raw results live in `.brokkr/results.db` (SQLite, tracked in git). Every row
  stores the literal subprocess invocation; query with `brokkr results --grep`.

## Hosts

**plantasjen** (reference host)
- AMD Ryzen 9 5900X, 12 cores / 24 threads, 4.95 GHz boost
- 30 GB DDR4, data on SSD (Samsung 990 PRO), source on NVMe

**dm6**
- AMD Ryzen 5 5600G, 6 cores / 12 threads, 4.46 GHz boost
- 32 GB DDR4, everything on NVMe
- Denmark: ~45s total (26s pbf, 8s ocean, 0.7s sort, 5s assemble)

## Baselines (plantasjen, clean bench mode)

| dataset | commit | run | wall | phase12 | ocean | sort | assemble | reader | peak RSS | output |
|---|---|---|---|---|---|---|---|---|---|---|
| denmark | `a0fca65` | plain | 26.4s | 15.8s | 5.9s | 0.02s | 4.1s | - | 2.9 GB | 365 MB, 1.33M tiles / 170K unique |
| norway | `b26b335` | plain | 105.0s | 70.5s | 7.8s | - | 26.5s | - | 5.9 GB | 1.38 GB, 16.3M tiles / 804K unique |
| norway | `661cd1c` | `8d1d19ca` | 160.1s | 121.3s | 14.9s | 0.03s | 23.4s | 23.0s | 5.9 GB | 1.28 GB, 16.3M tiles / 820K unique |
| germany | `c3c1520` | `28e2b5b4` | 180.6s | 142.8s | 5.1s | 0.01s | 31.9s | 30.5s | 11.1 GB | 3.0 GB, 2.69M tiles / 347K unique |

Current HEAD readings, both artifact-active, locations variant, plantasjen
(2026-07-14, `b833fc8`). Per-phase RSS, core counts and `mi_commit` are
sidecar values transcribed here because the results row keeps only
`elapsed_ms`:

| dataset | run | wall | phase12 | ocean | assemble | peak RSS | mi_commit at phase12 end | phase12 majflt |
|---|---|---|---|---|---|---|---|---|
| north-america | `b9d6c12c` | 311.2s | 206.3s / 23.3 GB / 15.4 cores | 0.3s | 91.5s / 4.7 GB / 14.9 cores | 23.3 GB | **44.4 GB** | 1,933,730 |
| germany | `2d715357` | 61.3s | 36.7s / 6.58 GB / 16.6 cores | 2.0s | 16.7s / 3.2 GB / 12.3 cores | 6.9 GB | **16.2 GB** | 1,133 |

**These are live regressions, not baselines, and the regressed quantity is
`mi_commit` - not RSS.** Allocator commitment has roughly doubled on both
datasets: NA 20.3 GB recorded at `69c0f18` to 44.4 GB (2.19x), germany
7.37 GB recorded at `9e8dce2` to 16.2 GB (2.20x). The same factor, so this
is systemic and proportional.

Only the consequence differs. NA's 44.4 GB of commitment against 23.3 GB
resident on a 30 GB host produces 1.93M major faults and +58s of phase12
wall; germany's 16.2 GB fits, so germany's RSS reads 6.58 GB - BETTER than
its own 8.8 GB baseline - while carrying the identical defect. **Do not
read peak RSS on germany and conclude the pipeline is healthy.** At NA the
commitment is also frozen byte-identical from PHASE12_END to run end:
mimalloc takes 44.4 GB during phase12 and returns none of it.

This voids the planet RAM go/no-go. Full evidence, the allocator A/B that
tests it at HEAD, and the open leads are in `notes/planet-30gb-roadmap.md`
under the H3 void notice. Both rows also carry a 12.4s (NA) / 5.15s
(germany) serial gap between PHASE12_END and OCEAN_START at 0.3-0.5 cores
that did not exist at 899f436.

Superseded rows (kept for delta reading):

| dataset | commit | run | wall | phase12 | peak RSS | note |
|---|---|---|---|---|---|---|
| denmark | `9b51e46` | `e18231c5` | 31.8s | 15.2s | 2.6 GB | pre pyramid-descent (ocean 11.8s) |
| denmark | `60fd209` | `1a6ca281` | 35.0s | 18s | 2.8 GB | pre prepass-overlap |
| norway | `95d6d52` | `38dcd3e8` | 171.1s | 131.1s | 4.1 GB | pre prepass-overlap |
| germany | `95d6d52` | `6fc97675` | 255.9s | 213.1s | 10.3 GB | pre prepass-overlap |
| germany | `9b51e46` | `15add85d` | 231.5s | 187.6s | 15.0 GB | pre pyramid-descent (ocean 10.8s) |
| germany | `9994e5f` | `fa3a8236` | 230.7s | 188.3s | 14.8 GB | pre P2 phase12 ownership rewrite |

Descent-era figures are plain single runs (not bench-3), so treat them
as indicative to ~10%, not verdict-grade. Germany is now re-measured
post-descent at `9994e5f` (`fa3a8236`, bench-3, verdict-grade): the
descent cut ocean 10.8 to 5.2s, but germany is phase12-bound (188s of
231s wall), so wall held at 230.7s (231.5 pre-descent). Spec 4 (Landings 1+2 +
convexity fix) took denmark from `9b51e46` 31.8s / ocean 11.8s to
`a0fca65` 26.4s / ocean 5.9s, and norway from `661cd1c` 160.1s to
`b26b335` 105.0s (phase12-dominated there; ocean 14.9 to 7.8s). Output
grew (+4% denmark, +8% norway after Brick 8's seam thinning): buffer-
strip coverage the old unbuffered z8 pre-split dropped, now emitted -
a rendering-correctness gain (edge polygons no longer pop at tile
borders), matching the ocean/planetiler/tilemaker convention.

The prepass-overlap landing (`9b51e46`, P0 of the perf backlog) cut wall ~9%
on both gates with byte-identical output, at the cost of germany peak RSS
rising 10.3 to 15.0 GB: the prepass hash sets now coexist with node-store
construction instead of preceding it. The norway row predates the overlap
(expect ~10-20s less wall and somewhat higher RSS when re-measured). The
P2 phase12 ownership rewrite (`c3c1520`, items 16, 22 reduced, 15 half 2)
then landed and is kept: rayon tasks own their PBF block and borrow tags,
emission goes through a per-worker arena, way resolution is gated to relation
members and matches, and `prepass_shared_nodes` builds `shared` from an exact
external merge-sort instead of an in-memory `seen` set. Germany bench-3
(`28e2b5b4`) fell to 180.6s wall / 142.8s phase12 / 11.1 GB peak RSS from the
`9994e5f` baseline - phase12 down 24 percent, wall down 22 percent, RSS down
25 percent, output regress-identical (denmark tol 0, germany counts
bit-identical).

The i_overlay extraction took the polygon boolean engine in-tree at
`src/geometry/overlay/` in two landings, both denmark-regress bit-identical at
tol 0. Landing 1 (verbatim i32-monomorphized port, `659a187`) is dependency
removal at neutral wall; i_overlay stays only as the dev-dependency differential
oracle. Landing 2 (`e2284ec`, the de-churn: engine-owned scratch, CSR nodes,
two-level ring/shell pooling) cut the two hot boolean frames normalize_into +
intersect_rect_into from 8.1 to 6.3 GB combined exclusive alloc (-22%; alloc
`0433cbc1` vs baseline `546b9d58`), peak RSS 9.2 to 7.3 GB, denmark bench-3 wall
13.3 to 11.6s (`c1053012` vs `bcac01ad`). The spec's under-3 GB churn target
proved mis-calibrated - L1's monomorphization had already cut the pair from the
pre-port 15.1 GB to 8.1 GB, so ~6.3 GB is the floor from that kill list.
Landing 2's germany-locations effect, attributed post-hoc from sidecar
(`--compare a6d1cb9e f2b93719`, the only code commit in the interval): wall
84.5s at `8eaa8bf` to 51.8s at `4ceacd1`, all of it phase12 (67.1 to 34.2s,
-49%), with phase12 peak RSS 22.8 to 6.3 GB and avg cores 11.4 to 16.9 -
germany's polygon volume was allocator-bound and at 22.8 GB the 30 GB host
was under memory pressure, so the de-churn paid off far beyond its denmark
reading (13.3 to 11.6s).

That germany reading corroborates the 2026-07-14 allocator-commitment
regression (see the HEAD table above): it establishes on this host that
phase12 polygon churn alone can hold ~22.8 GB and put the machine into
memory pressure, and that the shape of the fix is de-churn. It does NOT
establish cause - `e2284ec` is inside the regression window, so the commit
that fixed germany then cannot be what broke both datasets since. The test
is the allocator A/B at HEAD, not a bisect; see the H3 void notice.

The injected-prepass wait-work (2026-07-11) landed two neutrality-claiming
commits while the pbfhogg producer is still being built: `430f28b` (way
membership resolved once at plan build) and `f683129` (dormant injected
way-members consumption plumbing; the spec it pre-landed is retired, see
git history). Gate readings, all plantasjen: denmark raw bench-3
17.0s (`1157a31a`) vs 16.7s at `430f28b` (`262d55f3`) vs 17.5s at `8eaa8bf`
(`928725a5`); denmark locations bench-3 11.7s (`9933e811`) vs 11.6s at
`e2284ec` (`c1053012`); germany locations bench 53.1s (`65e499f2`) vs 51.8s
at `4ceacd1` (`f2b93719`). Neutrality: raw regress `f683129` vs `430f28b`
zero-diff at tol 0 (1,326,395 tiles), locations regress `f683129` vs
`4ceacd1` zero-diff at tol 0 (1,296,996 tiles; that reference predates both
commits, so it spans the pair). The germany locations regress was attempted
and OOM-killed mid-decode (two 2.8 GB archives while pbfhogg builds competed
for RAM) and was judged redundant for a dormant-path landing: the injected
arm is unreachable on every existing input, so branch-level neutrality is
dataset-independent and denmark carries it. The spec's `verify pmtiles` gate
is unrunnable as written - brokkr's verify resolves only brokkr.toml-pinned
pmtiles entries and this project pins none - and is subsumed here by the
full-decode zero-diff regress against a verified-lineage reference.

The injected-prepass activation (2026-07-11, Brick 3 + Brick 4 in effect):
the pbfhogg producer (their `29e4eabd`) re-enriched denmark, germany, and
norway via `pbfhogg add-locations-to-ways --index-type external
--inject-prepass --compression zlib:6` over the same-hash indexed inputs;
the enriched files are registered as the `locations` variants (`20c8bd7`
denmark + germany, norway in the follow-up commit) and the dormant
consumption plumbing from `f683129` activates on them with no code change.
On-disk growth (honesty clause): denmark 525 to 532 MB (+1.3%), germany
5.5 to 5.6 GB (~+2%), norway 1.4 GB unchanged at GB rounding. Activation
readings, plantasjen at `20c8bd7`: denmark locations bench-3 11.7s
(`67474f0d`), wall level with pre-enrichment, `prepass_join` 490ns,
`relation_plan_*` 0, `way_members_marked` 84,249 vs the old exact plan's
78,275, way_index data 9.26 to 11.30 MB; semantic regress vs the
`f683129` archive zero-diff at tol 0 (1,296,996 tiles). germany locations
bench 50.2s (`da63d783`) vs 53.1s pre-enrichment - the win is the deleted
3.36s prepass-join stall; `way_members_marked` 1,064,174 vs 886,109
needed (1.20x), way_index data 112.3 to 146.9 MB = 1.31x against the
Brick 4 bound of 2x, peak RSS 6.55 vs 6.38 GB; features, tiles, and every
per-layer sort counter bit-identical to pre-enrichment; the germany
semantic regress was OOM-killed twice mid-decode under the old regress
engine (a tooling capacity limit, not a verdict) and was left resting on
counter identity - until the regress engine rewrite (`7178425`), after
which the pair completed: ZERO diffs across 827,010 tiles, ~36s, peak RSS
5.8 GB, so the germany activation equality is now proven semantically. norway is enriched and registered but unvalidated
here (no bench, no regress, by explicit decision); its pre-enrichment
baseline archive `norway-20c8bd7.pmtiles` (`525c553a`) is banked for a
later regress. The old locations files remain on disk unregistered.

The injected-pins landing (H2b, `acbe400`, 2026-07-11) consumed field-20
shared-node pins on the enriched path and deleted the global shared-node
prepass (net -626 lines). Readings, plantasjen, denmark locations bench-3
`acf5ea76`: wall 12.0s vs 11.7s pre-pins (noise), `way_pins_marked`
11,615,766, `phase12_plan_build_ns` halved (4.47 to 1.91 s thread-time,
block-local counting skipped), features +0.4%, output_bytes +4.4% (347.9 to
363.1 MB) concentrated in land +14.8% / streets +6.7% / water_polygons
+7.2% - cross-block junction retention paying its byte cost. Earcut oracle
clean (0 over threshold, 0 misattached) on ocean, water_polygons, and land.
The parent's <=2% wall / <=3% bytes keep bounds were defined on germany
locations, which is UNMEASURED (bench freeze by user decision) - those
verdicts are unread, denmark readings stand in. The displacement-percentile
diagnostic regress was killed after 30+ min: the first diff-heavy regress
ever run exposed the tool's cubic matched-feature path. The regress engine
rewrite (`7178425`, blob-pair spans + tiered passes) fixed that: the
identical denmark pair completes in 1.9s (was ~4-5 min), the diff-heavy
pins pair in 3.5s with the full report (9,983 differing tiles,
tolerance_moved 28,886, structural_moved 204,769; was unfinishable), and
the germany-scale pair in ~36s at 5.8 GB peak (was OOM). Post-rewrite
refinements (2026-07-12, four landings): streaming tier-2 fingerprints,
sparse min-cost residual matching, the run-preserving PmtilesReader with
per-run verify decompression, and legacy-oracle retirement - see git
history. The blessed denmark archive rotated to the paint-order build
(`blessed/denmark-506b9bc.pmtiles`, blessed 2026-07-12), so the standing
bare `brokkr regress` gate is current again.

The H5 ocean tile stream landing (`92ed329` + world-only activation
`b2e8f2c`, 2026-07-12; spec notes/ocean-tile-stream-spec.md) precomputes
the world's ocean into a durable PMTiles artifact and merges it into
assemble as run copies on world-covering runs. The world build itself
(plantasjen, one shot): data/ocean-tiles.pmtiles at 942.7 MB, 212.4M
addressed tiles, 9.2M unique blobs, 13.97M directory runs, 95.7%
deduplicated; unique-payloads verify PASS (9,208,945 group validations)
and unique-mode earcut clean over 17.7M polygons. Extract gates,
denmark locations: computed-path regress vs blessed zero-diff across
1,296,996 tiles at tol 0 (the canonical full-fill id change is
geometry-invisible), verify PASS, earcut clean, bench-3 12.5s
(`f005ae56` at `92ed329`) vs the 12.3s + 3% keep bound. The extract hybrid's
gate read 27 structural ocean diffs, one strictly interior (z8/121/81):
the pyramid's root/bisection structure depends on the piece clip
extent, so artifact-served interior tiles legitimately differ from
extract-computed ones. Adjudication: the human viewer gate judged the
artifact-active archive equivalent (displacements mostly 10-97 units,
the accepted seam-drift class), the hybrid stands, and the blessed
baseline rotated to the artifact-active build. Artifact-active denmark:
ocean phase 5.75s -> 1.87s (7,141 band features vs 1.39M computed),
wall ~9.8s plain-run vs 12.5s computed bench. Standing caveat: regress
gates assume the gate machine carries the same data/ocean-tiles.pmtiles
the blessed archive was built with.

The paint-order determinism landing (`a631b5f` + comparator optimization
`2c770c7`, 2026-07-11) made archives byte-reproducible and wired the
SortKey priority byte to paint-rank tables (land background-first,
streets kind-major). Gates, plantasjen, denmark locations: FOUR
independent full-pipeline builds across both commits hash to one archive
(`git hash-object eb6bd7c3...`) - reproducible builds achieved; semantic
regress vs the pins archive (`acbe400`) zero-diff across 1,296,996 tiles
(the reorder is invisible to content comparison); earcut oracle clean on
ocean and land; the new `verify` within-layer order check passes on all
tiles. Wall: the landed fused comparator read 12.9-13.0s vs the 12.0s
pins baseline (`acf5ea76`), breaking the +5% bound - entirely phase12
chunk-sort cost, as the spec review predicted. The adjudicated fix
(two-pass sort: bare key, then equal-key runs on a gathered 8-byte
big-endian osm_id prefix, full payload compare only on prefix collision)
brought it to 12.3s stored (`ba1db6a8`), +2.5%, within bound. A/B chain
for the record (forced, unstored): fused 12.9/13.0, two-pass unprefixed
12.6/12.7, by-key-only control 11.9, final 12.3/12.3. The residual
+0.3s is the intrinsic price of the determinism guarantee. Guarantee
scope: MVT + gzip (the MLT encoder re-sorts for size and is out of
scope by spec).

"reader" is the `assemble_reader_ns` counter: serial k-way merge reader time
inside the assemble phase. The Denmark reader value is from the instrumented
runs at the same code state (bench-mode counter not captured for `1a6ca281`).

History behind the Denmark row: the integer-clipping rewrite (ledger R21-R24)
and the ocean-perf restructure (R25 / spec 3) reshaped the profile wholesale -
ocean fell from 48.8s to 11.9s via parallel prologue + piece-by-zoom fan-out,
at the cost of RSS rising to 2.8 GB from 24-way parallelism under mimalloc's
non-purging arenas. Pre-rewrite Denmark was ~12.4s, but with the
earcut-broken geometry the rewrite fixed.

**North America is stale.** Both NA baselines predate the integer-clipping
rewrite (R21-R24), the ocean-perf restructure (R25 / spec 3), and the
shared-node prepass. For the record: raw 605s / 22.8 GB RSS at `8704b11`,
locations-on-ways 462.6s / 19.4 GB at `90ad2ef` (2026-03). At those RSS levels
NA does not reliably fit plantasjen's 30 GB alongside a desktop; NA runs are an
explicit user decision, never part of routine iteration.

Scale contrast worth knowing when picking a gate dataset:
- **denmark**: ocean/coastline-weighted at small scale; fastest iteration.
- **norway**: coastal-multipolygon stress (777K relations; single z14
  water_polygons features fan out to 22,460 tiles; ocean is 59% of sort
  records vs 10% denmark, 2% germany).
- **germany**: inland/way-volume stress (69.6M ways, 39.5M buildings);
  phase12 is 83% of wall.

## Microbenchmark baselines (plantasjen)

- node-store, 50M nodes (`cb2cd29`): build 1.7s, way-like 77 ns/lookup,
  random 394 ns/lookup
- pmtiles-writer, 500K tiles (`cb2cd29`): 164 ms

## Gate commands

Exact invocations for spec gates (per
`reference/technical-implementation-spec.md` clause 5):

```
brokkr tilegen --bench 3 --dataset denmark     # default perf gate
brokkr tilegen --bench 3 --dataset norway      # coastal/relation-heavy claims
brokkr tilegen --bench 3 --dataset germany     # phase12/way-volume claims
brokkr results --compare-last                  # diff the two most recent runs
brokkr results --compare-last --mode hotpath   # diff hotpath profiles
```

Pick the gate dataset whose stress matches the claim; a change sold on
coastal-multipolygon cost is not proven on denmark alone.

## Reading rules

- **Keep/revert verdicts are read from `--bench 3` (best-of), never a single
  `--bench 1` run.** Single-run deltas under ~10% are within observed
  bench-to-bench variance; multi-run best-of deltas under ~5% are still
  suspect. A spec claiming a win states the expected bound up front and the
  verdict is read against that bound.
- **Hotpath mode ranks, it does not measure absolutes.** Wall-clock inflation
  from instrumentation was 15% (denmark), 17% (norway), 53% (germany,
  call-count-heavy) at `95d6d52`. Entries with tens of millions of calls at
  sub-microsecond averages are inflated the most; entries with large per-call
  work (tens of microseconds up) are mostly real.
- **Never read RSS from `--hotpath` or `--alloc` runs.** Per-call records
  dominate: norway measured 20.1 GB under hotpath vs 4.1 GB clean, denmark
  9.9 GB vs 2.8 GB. RSS comes from bench mode or plain runs only.
- **Alloc numbers before `95d6d52` are invalid.** The hotpath 0.14 to 0.20
  bump silently tracked zero bytes; older alloc profiles (e.g. the 2026-02
  "48.6 GB Denmark" figure) undercount and are not comparable to current ones.
- Timing percentages over 100% in hotpath reports are cross-thread seconds
  relative to wall clock, not errors.
- `ELIVAGAR_NODE_STATS=1` added a diagnostic scan inside `phase12_ms` and was
  never safe for bench timing. Deleted 2026-07-15: it profiled the node store,
  which only the raw path builds, and printed to a stderr that `--bench`
  discards - so it charged the runs whose output it could not reach. Numbers
  from runs with it set are still suspect for that reason.

## Reference profiles (2026-07-06 campaign, commit 95d6d52)

The profiling campaign behind the current `notes/performance-backlog.md`
prioritization. All plantasjen:

| run | dataset | mode | headline |
|---|---|---|---|
| `13c024bb` | denmark | hotpath | intersect_rect_into 157 thread-s; ocean engine dominates |
| `081975b3` | denmark | alloc | intersect_rect_into 48.4 GB = 67% of tracked allocation |
| `4e62f519` | norway | hotpath | tier-2/relation stack: 528 + 595 + 614 thread-s; intersect_rect_into 721 |
| `cc14c34a` | germany | hotpath | process_raw_way 754 thread-s; prepass 79s serial; ring_is_simple_complete 110 |
| `38dcd3e8` | norway | bench | clean baseline row above |
| `6fc97675` | germany | bench | clean baseline row above |

Constants across all three datasets: `intersect_rect_into` is the top
geometry sink everywhere; the serial merge reader is 97-98% of the assemble
phase everywhere; the shared-node prepass is 7-79s of pure serial latency
scaling with way volume; the ocean phase itself is flat (11-16s) at every
scale measured.

## E1 normalize fast-path close (2026-07-13, commit 65ae629, norway locations)

E1 added a strictly-convex/CCW/non-self-intersecting screen
(`is_perfect_ccw_convex`) on the single-contour arm of `normalize_into`, plus a
fall-through instrument, to test whether a cheap O(n) screen could skip the
segment-build + split-solver body on the "already perfect" fraction of calls.
Measured on a norway `--variant locations` bench at `65ae629`, the screen is
below its spec 0.30 proceed threshold and is CLOSED without landing the fast
path.

Instrument counters:

| counter | value |
|---|---|
| `normalize_screen_pass` | 5,282,144 |
| `normalize_screen_reject` | 14,118,318 |
| total single-contour calls | 19,400,462 |
| `normalize_screen_perfect_return` | 13,610,939 |

- `f = pass / (pass + reject) = 0.272` - below the 0.30 proceed threshold.
- pass + perfect_return = 18,893,083, a **0.974 ceiling** the strict screen
  captures only 28% of: the engine itself found perfect (returned
  `false`/untouched) on 13.6M of the rejected calls, so a looser but still
  exact classifier could in principle reach ~97% of calls.

Size buckets (n = outer vertex count):

| bucket | pass | reject |
|---|---|---|
| n3 | 82,732 | - |
| n4_8 | 4,923,309 | 4,445,750 |
| n9_32 | 276,069 | 7,631,905 |
| n33p | 34 | 2,040,663 |

93% of passes are cheap n4_8 solves; the expensive rings (n9_32, n33p) are
mostly rejected and mostly perfect-return. Weighting the pass fraction by
solver cost therefore sinks the flat 0.272 further, not toward the threshold.

Verdict: the strict-convex screen is mispriced - it proves the wrong predicate
(convexity, not the engine's exact perfect-input verdict) and captures a small,
cheap slice while the real cost sits in the rejected large rings. The 0.974
ceiling is a genuine opportunity, deferred to E1b (an exact perfect-CCW
classifier priced by avoided solver cost - see
`notes/planet-30gb-roadmap.md`). The per-call instrument
(`ScreenCounters`/`SCREEN` in `src/debug.rs`, the screen invocation in
`src/geometry/overlay/port/simplify.rs`) is removed; `is_perfect_ccw_convex`,
its slow-body reference helper, and the soundness tests are retained under
`cfg(test)` as the predicate and gate for E1b.

## E1b exact perfect-CCW classifier close (2026-07-13, uncommitted Landing 1, norway locations)

E1b shadowed a conservatively-sound perfect-CCW classifier on every
single-contour normalize call. The slow solver still ran unconditionally, so
the measurement did not change output. The Norway locations hotpath run took
48.700s on the current host and retained its dirty-run sidecar. The shadow
classifier accepted 12,302,148 contours; its unsound-accept counter was zero.

| measure | thread time |
|---|---:|
| avoided slow-body time on accepted, slow-false calls | 13.651s |
| always-paid O(n) screen | 2.395s |
| O(n^2) distinctness and pair scan | 36.169s |
| total classifier cost | 38.564s |
| net avoided minus cost | -24.914s |

The cumulative cutoff cannot make this positive: the full scan is already
25.0s slower than the solver work it would replace, and the required proceed
bar is positive net plus at least 6.5s, five percent of the pre-instrument
Norway `normalize_into` baseline. E1b is CLOSED. The Landing 1 classifier,
timers, sharded counters, and shadow wiring were removed; the prior
test-only strict-convex predicate and slow-body soundness reference remain.
No Landing 2 production skip was attempted.

## E3 rect-clip pooling close (2026-07-13, REVERTED, denmark locations)

E3 pooled the descent rect clip (`clip_shape_rect_fast` /
`clip_ring_half_plane_multi`), threading `IntEmitScratch` in and recycling
contour lists and component shells across the four half-plane passes instead
of allocating fresh per call. It won its churn gate decisively but failed both
perf gates, so it is CLOSED as regressive and its code has been reverted. The
engine sits back at the pre-E3 state.

Alloc pricing (Brick 0, baseline commit `32bda50`): `clip_ring_half_plane_multi`
14.9 GB + `clip_shape_rect_fast` 4.1 GB = **19.0 GB combined exclusive
allocation**, far above the spec 1.0 GB proceed threshold - so the item
proceeded to the rewrite.

The pooled rewrite (Brick 1) landed at commit `d1f26b6`. Post-change alloc:
`clip_ring_half_plane_multi` 14.9 -> 2.5 GB and `clip_shape_rect_fast` below
1.0 GB, roughly 3 GB combined - an **82 to 87 percent churn reduction**,
clearing the 80 percent gate. Total run churn 571 -> 554 GB. Output was
byte-identical: `brokkr compare-tiles` reported plus 0 percent on every layer
and every zoom z0 to z14 (both archives 1,296,996 tiles), and the earcut oracle
was clean (0 over threshold, 0 misattached).

The perf gates failed. Bench baseline `34b3e7cc` at `32bda50` vs post-change
`beaa3cbc` at `d1f26b6` (best-of-3, same host):

| gate | before | after | delta | verdict |
|---|---|---|---|---|
| wall | 13,400 ms | 14,300 ms | +6.7% | FAIL (noise band ~5%) |
| peak RSS | 3.54 GB | 4.36 GB | +23% | FAIL (gate 2%) |
| retained memory (alloc, end of run) | 161 MB | 1.2 GB | +7.4x | corroborates |

Verdict (codex xhigh adjudication): REVERT. The RSS regression is per-worker
shared-pool retention growing to the fattest tile - the exact input-scaled
retention pattern the roadmap already deleted twice (the per-thread
AssemblyScratch pool and the ocean PyramidScratch pool, both removed in favor
of per-item scratch with large RSS wins and unchanged wall). E3 reintroduced an
abandoned architecture. The larger-dataset argument does not rescue it: bigger
datasets give MORE opportunity for an outlier tile to poison every worker's
pool, so an NA or planet bench would only be testing a known unbounded-retention
mechanism against the 30 GB objective - so `d1f26b6` was NOT benched on NA and
the campaign did NOT restart from Brick 2.

The lasting value: the 19 GB churn surface is real evidence that the descent
rect clip deserves a STRUCTURAL fix, not indefinite pooling. The roadmap E2
direction (flat point and range storage at the engine boundary) is the aligned
answer - it removes the copy, the linear pool scan, and the recycle protocol
rather than retaining their capacities. If rect-clip churn is revisited, it
should be as a new item under E2 or an E3b that specifies a bounded
retained-byte budget BEFORE implementation, started from the pre-E3 code state,
not from this pooled design.
