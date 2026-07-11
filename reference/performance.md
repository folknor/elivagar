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

The injected-prepass wait-work (2026-07-11) landed two neutrality-claiming
commits while the pbfhogg producer is still being built: `430f28b` (way
membership resolved once at plan build) and `f683129` (dormant injected
way-members consumption plumbing; see notes/injected-prepass-spec.md for the
design it pre-lands). Gate readings, all plantasjen: denmark raw bench-3
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
5.8 GB (notes/regress.md), so the germany activation equality is now
proven semantically. norway is enriched and registered but unvalidated
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
ever run exposed the tool's cubic matched-feature path (hunt findings and
rewrite plan in notes/regress.md; rerun the diagnostic after the rewrite
lands). The blessed denmark archive predates the pins geometry change, so
the standing bare `brokkr regress` gate is stale until a user-gated
`brokkr bless` rotation.

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
- `ELIVAGAR_NODE_STATS=1` adds a diagnostic scan inside `phase12_ms`: fine for
  hotpath runs, never for bench timing.

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
