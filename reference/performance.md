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
| germany | `9994e5f` | `fa3a8236` | 230.7s | 188.3s | 5.2s | 0.01s | 36.2s | 34.4s | 14.8 GB | 3.0 GB, 2.69M tiles / 347K unique |

Superseded rows (kept for delta reading):

| dataset | commit | run | wall | phase12 | peak RSS | note |
|---|---|---|---|---|---|---|
| denmark | `9b51e46` | `e18231c5` | 31.8s | 15.2s | 2.6 GB | pre pyramid-descent (ocean 11.8s) |
| denmark | `60fd209` | `1a6ca281` | 35.0s | 18s | 2.8 GB | pre prepass-overlap |
| norway | `95d6d52` | `38dcd3e8` | 171.1s | 131.1s | 4.1 GB | pre prepass-overlap |
| germany | `95d6d52` | `6fc97675` | 255.9s | 213.1s | 10.3 GB | pre prepass-overlap |
| germany | `9b51e46` | `15add85d` | 231.5s | 187.6s | 15.0 GB | pre pyramid-descent (ocean 10.8s) |

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
compact-counter work (backlog item 15 half 2, P2) removes the RSS cost.

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
