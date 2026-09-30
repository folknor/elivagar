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

**These rows record the 2026-07-14 regression, RESOLVED 2026-07-15.** The
regressed quantity was live scratch retention, with `mi_commit` as its
loudest symptom: the i_overlay port moved the boolean engine's per-op
allocations into caller-owned scratch, and phase12's phase-lifetime
accumulators (the way-acc pool: 24 accs x ~770 MB at NA; the relation
tail's per-worker fold accumulators and their reduce-queue copies)
ratcheted that scratch capacity to ~20 GB of anon, evicting the page
cache and turning the tail's way_index mmap reads into the 1.93M major
faults. Bisect (NA phase12 peak anon): `e34cc7b` 5.4 GB good, `d20ddd5`
5.1 GB good, `659a187` OOM at 24.8 GB, `e2284ec` 21.0 GB - the port
introduced it, the de-churn fixed only germany's manifestation. The
three-way allocator A/B at HEAD, run before the bisect, refuted the
retention-in-mimalloc theory (sys-alloc: same 21.3 GB phase12 RSS, 3.8M
majflt, wall 413s vs 314s) and produced a germany data point for the
rip-out decision (sys-alloc and jemalloc both ~60s wall vs mimalloc 68s,
phase12 34-35s vs 42.7s).

The fix: way accs die with their block task, the relation tail drops its
geometry-scaled scratches per relation and finalizes fold accumulators
before they queue for reduce. Post-fix dirty-run readings (stored bench
rows follow the commit): NA wall 288s, phase12 170.3s / 5.64 GB peak
anon / 11.9K majflt, mi_commit_phase12_end 18.7 GB (from 44.6); germany
wall 63.5s, phase12 39.1s / 4.2 GB anon; denmark 10.0s. Output
byte-identical vs a clean-HEAD worktree build (regress
--file/--against: 1,296,999 tiles, raw-equal on every blob pair; the
blessed `ec5bd11` baseline predates the provenance block and cannot be
gated against - re-bless pending user decision). Germany
`mi_commit_phase12_end` stays at 14.2 GB against 4.2 GB anon: mimalloc
arena commitment under churn, owned by the rip-out decision in
`notes/planet-30gb-roadmap.md` (H6 allocator addendum).

The 12.4s (NA) / 5.15s (germany) serial gap between PHASE12_END and
OCEAN_START is explained: `b833fc8`'s provenance contract hashes the
input PBF single-threaded at run end (a full re-read). Real wall, ~5-8%
of a run - reporting, not pipeline. CLOSED 2026-07-15 (commit follows
this note): the hash runs on a background thread spawned before phase12
starts and joined at the checkpoint save, streamed through a fixed 8 MiB
buffer instead of a whole-file mmap so file-backed pages never inflate
the sampled RSS. The join carries `input_hash_join_wait_ns`; a non-zero
reading means the overlap assumption broke. Germany dirty-run readings
at the fix: gap 5.15s -> 0.30s, join wait 13us, phase12 31.2s against
the 32.2s stored baseline (the concurrent hash reader costs nothing
measurable), wall 47.9s dirty vs 59s stored - cache-warmth flattered,
the stored rows below are the honest number. Resumes still hash up
front by design: the checkpoint must be validated before any work runs
under it.

The post-fix allocator A/B at `98824b4` (stored bench-1 arms, plantasjen,
locations, artifact-active) ended mimalloc: the system allocator wins
wall outright on both datasets, not merely within noise, with a fraction
of the minor faults:

| dataset | arm | run | wall | phase12 | avg cores | phase12 minflt |
|---|---|---|---|---|---|---|
| north-america | mimalloc | `ee259d09` | 282s | 170.8s | 16.1 | 50.4M |
| north-america | sys-alloc | `001930af` | 269s | 155.0s | 17.9 | 17.6M |
| germany | mimalloc | `050f0de1` | 65s | 39.3s | 15.8 | 11.1M |
| germany | sys-alloc | `9399d48c` | 60s | 31.5s | 19.7 | 4.4M |

jemalloc measured at par with sys-alloc on germany (60s at pre-fix
76492b8) and does not pay for its dependency. mimalloc, libmimalloc-sys
and tikv-jemallocator are removed with their feature arms; the sidecar
emits `malloc_held_<boundary>` / `malloc_live_<boundary>` from glibc
`mallinfo2` where pre-2026-07-15 rows carry `mi_commit_*`.

Stored baselines at the rip-out commit `4cd8496` (plantasjen, locations,
artifact-active, system allocator; sidecar values transcribed):

| dataset | run | wall | phase12 | assemble | phase12 peak anon | phase12 majflt |
|---|---|---|---|---|---|---|
| denmark | `c7413332` | 8.9s best-of-3 | - | - | - | - |
| germany | `e5e0cfa0` | 59s | 32.2s | 19.6s | 3.5 GB | ~700 |
| north-america | `b9d63821` | 276s | 156.4s / 17.7 cores | 102.3s | 4.75 GB | 12.3K |

Against the 07-14 regressed HEAD rows above: NA wall -11% with phase12
memory at a quarter and the fault storm gone; denmark takes the record
from 11.4s to 8.9s. Both runs still carry the serial provenance-hash
tail (the PHASE12_END segment reads the whole PBF once more), which is
now the largest non-pipeline cost at NA (~16s) - removed by the hash
overlap landing above; the stored rows at that commit are the
post-overlap baselines.

Stored baselines at the hash-overlap commit `eb1e36e` (plantasjen,
locations, artifact-active; sidecar values transcribed; NA and germany
are bench-1 per the NA-and-larger measurement discipline, denmark is
bench-3):

| dataset | run | wall | phase12 | ocean | assemble | phase12 peak anon | phase12 majflt |
|---|---|---|---|---|---|---|---|
| denmark | `c92b7529` | 8.5s best-of-3 | - | - | - | - | - |
| germany | `fcd3bde4` | 51.4s | 35.0s | 2.0s | 14.1s | - | - |
| north-america | `52cc955a` | 254.2s | 154.7s / 18.5 cores | 0.2s | 98.9s / 14.3 cores | 5.14 GB | 11,283 |

The PHASE12_END-to-OCEAN_START gap is 0.3-0.5s on every dataset
(`input_hash_join_wait_ns` 12-13us: the hash always finishes first),
against 12.4s at NA / 5.15s at germany before the overlap. Denmark
takes the record again, 8.9s to 8.5s; NA -7.9% wall, germany -13%
(bench-1, treat the germany figure as indicative to ~5%). NA peak RSS
5.76 GB whole-run - the buffered hasher adds nothing the sampler can
see.

The `52cc955a` sidecar also exposed the next serial tail: assemble
workers finished at t=235.3 but the run ended at 254.2 - ~19s of
symmetric rd+wr (14 GB each way at ~800 MB/s, `pmtiles_write_wait_ns`
18.8s) copying the tile-data temp file into the archive. Landed
2026-07-15 (commit follows this note): the writer adopts planetiler's
section layout (tile data at fixed offset 16,384, metadata + leaf dirs
appended after it, header + root patched into the reserved init
section), so tile data streams once, directly into `<output>.partial`,
and finalize is rename-only. Riding along: the compressed root
directory now always fits the spec's first-16,384-bytes window (leaf
fanout doubles until it does) - NA's root was 19 KB before, past the
window. Gates: denmark regress vs the `eb1e36e` archive raw-equal on
all 1,296,999 tiles, elivagar verify PASS, JS pmtiles reader decode
confirmed; archive BYTES change by layout (offsets), so blessing-style
whole-archive hashes rotate, tile blobs do not.

Stored baselines at the writer-layout commit `bc71cf1` (plantasjen,
locations, artifact-active; NA and germany bench-1, denmark bench-3):

| dataset | run | wall | phase12 | assemble | note |
|---|---|---|---|---|---|
| denmark | `186b7627` | 8.8s best-of-3 | - | - | tail was sub-second here; noise vs 8.5s |
| germany | `3f042746` | 53.8s | 36.8s | 14.6s | pmtiles_write_wait 628ms -> 42ms |
| north-america | `1e058ff0` | 251.0s | 164.3s | 86.1s | tail ~19s -> 2.3s; root 19 KB -> 10.3 KB |

The decisive read is NA assemble: 98.9s -> 86.1s (-12.9s), with the
post-worker tail at 2.3s (`pmtiles_write_wait_ns` 2.27s, mostly leaf
gzip + the 31 MB trailing write). NA phase12 ran 164.3s against its
154.7s baseline in the same session - bench-1 variance on untouched
code - so the wall delta (254.2 -> 251.0) understates the tail win;
read the phase numbers, not the wall. The NA root-directory demotion
(leaf fanout 4096 -> 8192, root 10,564 B) fired exactly as designed.

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
was unrunnable as written at the time - brokkr's verify resolved only
brokkr.toml-pinned pmtiles entries and this project pins none - and is
subsumed here by the full-decode zero-diff regress against a
verified-lineage reference. That resolution gap is closed as of 2026-07-25:
`verify pmtiles` now takes the standard dataset/variant/commit/file
resolver, so the gate is expressible for later landings.

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
`b2e8f2c`, 2026-07-12) precomputes
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

## Corpus digest gate calibration (2026-07-15, commit a8c4f84, denmark locations)

The oracle-discipline readings that promoted `elivagar corpus check` from
advisory toward gate duty (spec A of the corpus plan; the gate stays
formally advisory until spec C rotates the standing gate). Baseline archive:
`data/tilegen/denmark-a8c4f84.pmtiles`, clean build, artifact-active at
ocean policy 2. Committed baseline: `corpus/denmark/` - contract.json
1,597 B, digest 949 B, leaves 10,799,001 B (the ~11 MB the spec predicted;
proceed threshold was 32 MB).

Both directions, all six readings exactly as pinned:

| reading | expectation | result |
|---|---|---|
| self check | PASS exit 0 | PASS, 1,296,999 tiles / 166,365 unique, 2,156 ms |
| regzip control (byte-different, semantically equal; 378,353,776 vs 377,793,332 bytes) | PASS exit 0 | PASS, 2,175 ms |
| mutate drop-tile 5/16/9 | FAIL exit 1 naming the tile | `zoom 5 tiles 11->10`, `removed 5 16 9` |
| mutate nudge-geometry 5/16/9 | FAIL exit 1 naming the tile | `changed 5 16 9` with old->new hash |
| mutate layer-version 5/16/9 | FAIL exit 1 naming the tile | `changed 5 16 9` with old->new hash |
| stale-artifact archive `denmark-bc71cf1.pmtiles` | REFUSE exit 2 on contract | `contract mismatch: config.ocean.artifact_key.policy_version` |

Check cost on denmark: ~2.2 s for the full 1.3M-tile decode+hash - cheap
enough to run per landing.

The owed `write_to` neutrality read (landing 3 touched
`PmtilesWriter::write_to` via `set_metadata_verbatim`): pmtiles-writer bench
`1279120b` at ecef3b8 (pre) 6,411 ms vs `e0faaadb` at a8c4f84 (post)
6,560 ms - +2.3% on a ~6.5 s bench, inside the noise band; neutral. No
recent comparable baseline existed (the March rows predate the writer-layout
rewrite), so these two rows are also the fresh microbenchmark baseline pair.

## SVG corpus render core calibration (2026-07-15, commit 41a953a, denmark locations)

The spec-B gate readings, run by the orchestrator after the landing:

- **Differential ring-grouping oracle**: `elivagar corpus rings` vs the
  independent `scripts/validate/ring-grouping-oracle.mjs`, both over the
  full `denmark-a8c4f84.pmtiles` (1,296,999 tiles): dumps byte-equal at
  59,464,966 bytes each. The Rust classifyRings port and the Node
  implementation agree on every polygon in the archive.
- **Determinism gate**: `corpus render-manifest` run three times against
  the same archive; renders two and three byte-identical across all nine
  manifest SVGs (sha256-compared). The committed corpus only changes when
  content changes.
- **Tier-2 FIRES**: a `nudge-geometry` mutant at z7/68/39 (an unfiltered
  manifest entry, so the first-encoded-layer mutation is visible) renders
  a canonical SVG that differs from the committed corpus file; the same
  mutant fails tier 1 first through `corpus check` (`changed 7 68 39`),
  which short-circuits before tier 2 by design - tier 2 is the human
  layer, never an independent detector.
- **Tier-2 CLEARS**: the regzip control renders byte-identical to the
  committed corpus file, and the full `corpus check` on the regzip
  archive passes tiers 1 and 2 in 2,376 ms.
- **Wire-order regeneration**: the step-5 wire-order fix changed 5 of the
  9 committed SVGs (the multi-component tiles the pre-fix
  component-order renderer had grouped differently); the corrected
  baseline is committed with these readings.
- **Human calibration pair**: `corpus render` of z5/16/9 from the
  preserved stale-artifact archive (`denmark-bc71cf1.pmtiles`,
  contract-free path) vs the committed clean corpus tile - 33,375 vs
  39,583 bytes, differing exactly where the 2026-07-15 ocean spike lives.

## Standing gate rotation (2026-07-15, spec C teardown)

`brokkr bless`/`brokkr regress` and `datasets.denmark.blessed` were removed.
The corpus check became the standing output gate, following the digest
gate calibration at `a8c4f84` and render-core calibration at `41a953a`; its
denmark cost is about 2.2 s. The committed corpus baseline supersedes the
07-15 re-bless-pending-user-decision thread. (Spelled `elivagar corpus
check` until the 2026-07-24 corpus redesign; the standing spelling is now
`brokkr pmtiles-corpus check`, native brokkr code over the linked crate.)

## Low-zoom ocean union (2026-07-19, OCEAN_POLICY_VERSION v3, denmark locations)

The corpus z2-x2-y1 finding (human layer, day one): band-computed low-zoom
ocean rendered seam wedges and dropped fragments on the source split grid.
Root cause was per-piece pyramid descent of the pre-split osmdata cells -
independent per-cell VW simplification plus per-fragment min-area drops -
NOT a band-vs-artifact difference: 1,363 of the committed corpus tile's
1,421 non-grid coastline edges were bit-identical in the v2 artifact's
z2/2/1, so the artifact carried the same defect and "serve artifact content
at low zoom" was rejected as a fix. Landing: union the z0-z7 pass's pieces
before descent, always split the passes (full-only spelling now serves both
passes from the full shapefile), policy v2 -> v3. Design critiqued pre-code
by codex-xhigh (six findings, all folded: pass-orchestration bug, ring-cap
gate, defect-level tests, mosaic invariant, union resource counters,
`ocean_pieces` semantics).

Readings at the landing, all on plantasjen:

- **Union cost**: denmark band 394 pieces (153,103 v) -> 361 shapes,
  69 ms; world (`ocean-build`) 14,282 pieces (1,749,806 v) -> 2,106
  shapes, 1.03 s. Counters `ocean_union_*` now record this per run.
- **Defect metric** (z2/2/1, canonical render): 73 subpaths -> 10;
  128px-lattice-aligned edges 283 (16.6%) -> 11 (0.9%, the bbox clip
  line). Ocean features in the tile: 71 -> 2.
- **Earcut oracle**: green pre- and post-fix builds, all zooms, 0 over
  threshold, 0 misattached (1.27M tiles).
- **Ring-cap census** (`ring-cap-census.mjs`, new gate: 0 polygons over
  MapLibre's 500-ring clamp): denmark union zooms max 305 rings (z7);
  world artifact union zooms max 352 (z7). All offenders are full-pass
  (z8+) features untouched by the union: the denmark pair (z9/285/148
  feat 10: 510 rings, z9/286/147 feat 4: 602) is bit-identical pre- and
  post-fix, and the world artifact adds z10/546/260 feat 1 (725 rings).
  Both world offender tiles were already over the cap in the DP-era
  2026-07-12 artifact (z10/546/260: 628 rings, z9/286/147: 544), so the
  class spans simplifier generations and is out of this landing's
  scope. MapLibre silently drops the hole rings beyond 500 in those
  three polygons today; the follow-up is tracked in the roadmap note.
- **Determinism**: two full denmark builds of the same dirty tree
  sha256-identical (687ff258...).
- **Contract guard**: `corpus check` of the v3 build against the v2
  baseline refused exit 2 naming `config.ocean.artifact_key.policy_version`.
- **Regress attribution** (v3 vs 8b7c4a0): 154 changed tiles of
  1,296,999; every diff ocean-layer z0-z7, zero z8-z14, zero other
  layers, zero attr changes. Displacements bounded (max 5,129 units at
  z7 = merged-feature reorganization, not drift).
- **`elivagar verify`**: PASS, both builds.

The corpus rotation (`bless --rotate` + `render-manifest`) runs against the
clean rebuild of the landing commit - bless refuses dirty builds.

## Corpus redesign recalibration (2026-07-24, gate native in brokkr)

The corpus redesign moved the gate machinery into brokkr (elivagar
`0129ef3`, corpus and regress subcommands removed; brokkr links the crate
and owns the hash, fold, gating policy, render core, mutate, and verdicts).
Full acceptance readings on plantasjen, all against the UNCHANGED committed
`corpus/denmark/` baseline - the a40c077 rotation was never re-blessed, so
the committed leaves and SVGs served as the parity oracle for the ported
code:

- **Digest parity, existing archive**: native check on
  `denmark-locations-3344eaa.pmtiles` (built by pre-redesign elivagar)
  exit 0, 1,296,998 tiles / 166,347 unique matching the committed digest
  line, zero corpus diff.
- **End-to-end at the redesign commit**: fresh
  `brokkr tilegen --dataset denmark --variant locations` at `0129ef3`
  (10.6 s), then native check: exit 0, same counts. New producer, new
  judge, old baseline - all three agree.
- **Calibrands** (mutants of the 0129ef3 archive, tile 14/8764/5132):
  drop-tile FIRED exit 1 naming `removed 14 8764 5132`; nudge-geometry
  FIRED exit 1 naming the changed tile with old and new hash;
  layer-version FIRED exit 1 likewise; regzip CLEARED exit 0 through both
  tiers (digest and SVG staleness).
- **Ring-grouping differential oracle**: `brokkr pmtiles-corpus rings` on
  the 0129ef3 archive byte-equal (`cmp`) to
  `scripts/validate/ring-grouping-oracle.mjs` over all 1.3M tiles.
- **Render-port fidelity save, recorded as the gate save it was**: the
  first native check run exited 3 with two manifest SVGs stale (z12, z14)
  under a passing digest. Cause: the ported render core emitted features
  in wire order - the missing `compare_detail_features` canonical sort
  (plus attr/component canonicalization the old decoder did internally).
  The committed SVGs were the correct side; the fix went into brokkr's
  render port and staleness cleared with zero corpus changes. The
  subordinate-staleness pipeline ordering held: the digest verdict was
  never masked.

Same-day follow-up readings after brokkr's regress/compare-tiles port
went native (brokkr `c1a49b1`-era):

- **Native regress, known-clean pair**: `brokkr regress` on the
  `0129ef3` archive against `3344eaa` - exit 0, 1,296,998 identical
  tiles, zero diffs in every class, all 166,348 blob pairs resolved in
  the raw pass (19 ms). Counters now report in-band; the FIFO emission
  was dropped deliberately (brokkr is the drain process).
- **dump_overlays off-by-one, verified pre-existing**: the port found
  the old overlay dumper iterating `start..=end` over spans that are
  half-open (`PairSpan::tiles = end - start`, coalescing on
  `last.end == start`), rendering one unchanged tile past every
  differing range and burning an `--overlay-max` slot on it. The bug
  was in the shed elivagar code, not introduced by the port; the port
  fixes it. Attribution-only surface, no verdict ever affected.
- **Native compare-tiles**: full per-zoom/per-layer census on the same
  pair, identical counts both sides; the `cmds` column became `verts`
  (it decodes through `tile_detail` now, tolerant mode).
- **Bless guard**: bless-without-`--rotate` on the digest-equal fresh
  archive refused exit 1 (`rotation requires --rotate`), zero corpus
  writes.
- **Example retired**: `examples/compare_tiles.rs` deleted from
  elivagar - its only caller was the shell-out wrapper this port
  replaced.

The streaming hash definition is frozen: it is the meaning of every
committed leaf, so any semantic change to it is a corpus-rotation plus
recalibration event, never a quiet edit. Recalibration triggers and the
two-leg drift mechanism live in `reference/corpus.md`.

## Ring-cap partition (2026-07-24, OCEAN_POLICY_VERSION v4, denmark locations)

MapLibre's classifyRings clamps `EARCUT_MAX_RINGS = 500` per CLASSIFIED
POLYGON - an outer plus its following opposite-wound holes - not per feature,
and silently drops all but the 500 largest rings past it. The three offenders
carried over from the v3 landing were single many-holed ocean polygons:
feature-probe on `denmark-locations-da6995f.pmtiles` showed z9/285/148 to be
ONE outer of 896 vertices spanning the buffered tile plus 509 holes, so no
regrouping or merge-side change could help. Emission now bisects an over-cap
normalized shape's buffered tile rect - longer axis, ties to x, halves closed
at the shared integer cut coordinate - and recurses until every piece fits
under the cap. Clipping runs at min-area 0 (island survival was decided by
`normalize_into` upstream); a piece still over the cap in an indivisible rect
is a hard error, not an emission. Spec: ring-cap-partition-spec, critiqued
pre-code by codex-xhigh (14 findings); the spec note was deleted after the
landing per convention and lives in git history.

Readings at the landing (`b6b6844`), all on plantasjen:

- **Bench, unchanged**: pre `brokkr tilegen --bench 3 --dataset denmark
  --variant locations` 10,100 ms best-of-3 (`8023d844`, at `b4f4390`); post
  10,100 ms (`06d5f2d4`). Same best-of-3 to the recorded resolution - the
  partition executes on two shapes in the whole world.
- **Ring-cap census, both directions**: FIRES on the pre-landing archive
  (`ring-cap-census.mjs ... all`, exit 1, naming exactly `ocean z9/285/148
  feat 10: 510 rings` and `ocean z9/286/147 feat 4: 602 rings`); CLEARS on
  the post-landing archive and on the rebuilt world artifact
  (`ocean-tiles.pmtiles ocean --unique`, 9,175,064 unique payloads, max 500
  rings at z9, 0 over cap). That is this landing's both-direction
  calibration.
- **Coverage preservation** (what the census alone cannot show - a fix that
  DELETED the excess holes would clear it too): the partition's pieces XOR
  to zero area against the original shape, refereed by the `i_overlay`
  dev-dependency rather than the in-tree engine doing the split
  (`ring_cap_partition_preserves_coverage`).
- **Earcut oracle**: 0 over threshold, 0 misattached on EVERY polygon layer,
  pre and post, denmark (1.3M tiles) and the world artifact (9.18M unique
  payloads). Layer coverage is what the new `all` argument buys; both
  instruments gained it first, calibrated by the ocean block of an `all`
  scan being line-identical to the single-layer run.
- **Corpus preflight**: bless-without-`--rotate` on the fresh archive
  refused exit 1 naming `config.ocean.artifact_key.policy_version` and
  exactly two changed leaves, z9/285/148 and z9/286/147. No third tile.
- **The renderer's clamp, measured not assumed** (`data/ring-cap-evidence/`,
  canonical renders - plain `elivagar svg` cannot show this defect, and
  OpenLayers keeps every ring). Rings in the ARCHIVE versus rings the
  pre-landing render draws: z9/285/148 526 vs 516, z9/286/147 621 vs 519.
  Each shortfall is exactly that tile's offending polygon's excess over the
  cap (510-500 = 10, 602-500 = 102), which is what establishes that the
  canonical renderer ports the clamp rather than merely differing. Post
  landing the two agree (521/521, 621/621) and z10/546/260 goes 501 -> 723
  drawn. Post ring totals sit slightly under the unclamped originals (526,
  621, 726) because a hole straddling the cut is absorbed into the pieces'
  outer boundaries as a notch on each side - ring count drops, covered area
  does not, which is what the XOR test pins.
- **Human visual gate, CLOSED** (2026-07-24, the standing ocean gate per
  AGENTS.md "Oracle discipline"): two islands visible in the post renders
  and absent from the pre renders, adjudicated by the human on the canonical
  SVGs, and the post archive confirmed clean in MapLibre itself. This is the
  reading no aggregate measure supplies - and MapLibre is the consumer whose
  semantics define the defect, so it is the one that closes it.
- **The cut line is not a seam, adjudicated on the same renders.** The
  partition's cut shows in every post render as a faint vertical hairline at
  local x = 2048 (a square buffered tile rect ties on the longer axis, so
  the first cut is vertical through the middle). It is a rasterization
  artifact, not a gap: `scripts/validate/cut-seam-probe.mjs` sweeps the cut
  line and finds it covered by exactly TWO paths along its whole
  partition-produced extent - 3894 units at z9/285/148, 2387 at z9/286/147,
  4268 at z10/546/260 - meaning both pieces put their boundary on the same
  integer coordinate and meet edge-to-edge. The only singly-covered residue
  (5 and 35 units) is coastline that lies on the midline in the PRE renders
  too. The hairline is the SVG renderer antialiasing two abutting fills
  independently, and is SVG-only: MapLibre shows no seam at either z9 tile
  (human inspection of `denmark-locations-8a3b86c.pmtiles`, 2026-07-24),
  which is what its fill bucket predicts - it triangulates and draws
  interior edges without per-polygon AA. Live
  consequence to remember: the cut is a genuine polygon boundary, so any
  style stroking the ocean layer (`fill-outline-color`, or a line layer over
  it) would draw it. Shortbread's ocean is a plain fill.
- **`elivagar verify`**: PASS on both the archive (1,296,998 tiles, 26
  layers) and the artifact (9,177,102 tiles, `--unique-payloads`).
- **Counters**: `ring_cap_partitions` / `ring_cap_pieces` read ZERO in the
  gate build, and that is correct rather than a wiring fault - an
  artifact-active extract takes its interior tiles from the artifact, so the
  two denmark offenders were partitioned during `ocean-build`, which is not
  a measured command and attaches no sidecar. (The 20 `*_wait_ns` counters
  in the same run come through the same `counter_group!` macro and the same
  end-of-run flush, which is what rules out the wiring fault.) The spec's
  expectation of "partitions 2" holds only for an artifact-absent run.
- **Comparand lost during the landing**: brokkr's archive retention pruned
  `data/tilegen/denmark-locations-da6995f.pmtiles` - the archive the spec
  pinned as the regress comparand - when the fifth build landed. No verdict
  depended on it (the preflight reads the corpus baseline, and `b4f4390` is
  the output-identical pre-landing build that the ring-count measurements
  above were taken from), but a spec that pins an archive across a landing
  has to account for that retention window.

## i_overlay 9.0.0 fixes ported (2026-09-30, OCEAN_POLICY_VERSION v5, denmark locations)

The dev-dependency oracle moved from i_overlay 8.1.0 to 9.0.0 in a
dependency refresh, and the differential oracle tests failed: upstream had
fixed four defects the in-tree engine still carried. Ported: the fragment
splitter's border lookup (upstream issue 87 - segments ending on a column
border live in the LEFT group, the engine looked in the right one), the
removal of the fragment-rect filter on crossings and collinear overlaps
(it dropped any whose rounded point or overlap start fell in a neighbouring
column), the snap-radius exponent cap (56 for i32, saturating), and
collinear output cleanup deferred until after hole binding (upstream issue
91). Both engine entry points now replace their output buffer instead of
appending, since the binder scans all of it, and the binder's unresolved
parent is a sentinel in release builds too.

Readings, all on bygg:

- **Issue 91 tests, both directions**: with pre-bind simplification
  restored for one throwaway build, the ported repros FIRE - index out of
  bounds indexing `parent_for_child` with `ContourIndex::EMPTY` (the panic
  the issue reported on planet OSM buildings, and a release-build panic too,
  since EMPTY is out of range whatever the fill value), and the hole
  `(-4,3),(-4,4),(-3,2)` bound to the triangle `(-1,0),(-4,1),(-3,0)`
  instead of its 13-vertex container. Both original differential oracle
  tests fail the same way. With the fix they CLEAR.
- **Corpus**: `check` refuses exit 2 on
  `config.ocean.artifact_key.policy_version`, as it must.
- **Regress, pre versus post** (artifact-active, each build against its own
  policy's world artifact): 14 of 1,296,998 tiles differ, all `ocean`, all
  structural, no attribute or feature-count change. Every changed vertex
  sits at x 4146..4224 of a 4096 extent - inside the right-hand tile
  buffer, which fill renderers clip away.
- **Attribution, by computed-ocean A/B**: artifact-served tiles cannot
  answer a code toggle, so every arm ran with the artifact line dropped from
  the tilegen block. All four fixes versus all but the border fix: the same
  14 tiles, identical per-zoom structural counts and displacement
  percentiles. All but the border fix versus the pre-change code: 0 diffs,
  every one of 1,296,995 tiles raw byte-equal. So the border fix is the
  whole output change on denmark and the other three are exactly neutral
  there.
- **Mechanism, and how far it is established**: the border lookup is proven
  by how segments are stored - one ending on a column border lives in the
  left group, and the old code searched the right one. Why it lands on the
  right clip edge is conditional: the grid takes `min_x` from the first
  sorted segment and `max_x` from all of them, not from the clip rect, so it
  holds only when a failing call's envelope is the buffered rect,
  [-128, 4224] in tile coordinates. For that envelope it is exact: the span
  4352 is 17 x 256, and every column power a fragment-sized input can pick
  (1 to 5) divides it, so the right edge is always a border entry, while the
  left edge is group 0, which never is. Fragmentation needs over 16,000
  segments, which is why only the largest ocean pieces reach it.
- **The two outliers**: z9/282/150 dropping from exactly 500 rings to 498 is
  two holes touching the right buffer edge that the corrected split absorbs
  into the outer as notches; it never crosses MapLibre's clamp, which acts
  only above 500. The z10 displacement max of 367 is regress's
  nearest-vertex metric, from (4224,3653) to the nearest opposite-side
  vertex (4207,3287) - vertex sampling, not a 367-unit coastline move.
- **Earcut oracle**: 0 over threshold, 0 misattached, every polygon layer,
  post. Pre ocean also 0/0 - earcut does not see this defect.
- **Ring-cap census**: 0 over cap on every polygon layer.
- **`elivagar verify`**: PASS on the pre archive; the defect produces no
  self-intersection that verify reports.
- **World artifact, v5**: `verify --unique-payloads` PASS (9,177,102
  tiles), earcut 0/0 and ring-cap 0 over cap on 9,175,064 unique payloads.
  Regress against the preserved v4 artifact: 26 of 212,393,397 addressed
  tiles differ, all `ocean`, all structural, z1 through z14. Denmark only
  reads a subset of artifact payloads, so this is the world-scale check.
- **No changed edge reaches a visible tile**: `scripts/validate/overlay-diff.mjs`
  compares the edge multisets of each overlay pair and tests every differing
  edge against the visible square [0, 4096]^2 (Liang-Barsky). Calibrated on
  a synthetic pair - an edge moved inside the square counts 4, a buffer-only
  move counts 0. Across all 26 world tiles and all 14 denmark tiles: 0
  changed edges touch the visible square; every one lies at x 4146..4225.
  That is stronger than vertex position, since an outside vertex can still
  move an edge that crosses into the tile. It covers fill renderers that
  clip at the tile: a wide stroke, a translated layer or a consumer that
  draws the buffer could still show it, and it says nothing about
  rasterization being bit-identical.
- **Artifact provenance**: the v5 world artifact was built from the working
  tree before the landing commit, with production code identical to what
  landed (only tests and documents changed afterwards). Its key - shapefile
  hashes, zoom range, compression level, policy version - validates the
  same either way, and an artifact carries no build provenance block of its
  own, so this line is the record connecting it to the landing.
- **Boundary-line oracle FAILS on both sides, identically**: palindromes
  and spurs at z7..z13 plus one spur per zoom at z0..z6, the same counts
  and the same named offenders before and after. Boundaries are lines and
  never reach the polygon engine; this is a standing failure at `88219f6`,
  not a finding of this landing, and it is open.
- **Human visual gate: OPEN.** The buffer-only location argues the change
  is invisible in a rendered tile, but that is an argument, not the
  standing ocean gate.

## H8b hot-partition splitting (2026-07-26, landed on 69e829b)

Assemble splits any sort partition whose source record bytes exceed twice
`ELIVAGAR_ASSEMBLE_SPLIT_TARGET` (default 64 MiB) into contiguous tile-range
pieces, boundaries at byte quantiles from a header-only pre-scan of the hot
partition's sources, each piece an independent worker job and ordered-writer
slot. Counters: `assemble_split_partitions`, `assemble_split_pieces`.

Motivating evidence (germany locations bench `3f042746`, commit `bc71cf1`,
07-15): `assemble_partition_encoded_max` 296.6 MB - ONE z14-block partition
(the Berlin z7 prefix; the oversize-tile table clusters at z10 x530-551
y340-354) carried 10% of the whole 2.99 GB archive behind a single ordered
slot and one serial merge reader (`assemble_reader_ns` max 3.63s). The
writer waited 12.0s on `assemble_partition_batch` (82% of the 14.6s
assemble phase, 14.5 avg cores) while `assemble_claim_window` wait was
7.7ms and parked HWM 1.25 GB of the 2 GiB budget: workers were never
blocked, they ran out of other partitions while the straggler ground
through one reader. Splitting attacks exactly that residue.

Calibration at the landing (denmark locations, plantasjen, dirty-tree
gate runs on `69e829b`):

- **Bit-identity gate PASSED**: `ELIVAGAR_ASSEMBLE_SPLIT_TARGET=10^15`
  (splits impossible, counters read 0/0) vs `=10^6` (37 partitions split
  into 624 pieces) produced byte-identical archives - `git hash-object`
  `b56b45bb535f5c21e8806e6d61679856e7429b8e` on both. Pieces are extra
  order slots draining in the same global Hilbert order, so dedup and
  directory bytes cannot move by construction; the hash pair is the
  demonstration.
- Unit gates: piece streams concatenate to the whole-partition stream
  (keys AND payloads) for uncompressed and lz4 chunks, across
  multi-section and single-partition chunk files, with a hot tile band
  forcing uneven quantile cuts; single-tile-range partitions and zero
  targets refuse to split.
- The default-target denmark run splits nothing (0/0 counters at 64 MiB;
  denmark's fattest partition is well under 128 MiB), so the standing
  corpus gate exercises the unsplit path and the forced-target pair
  above is the split path's gate.

Germany structural reading at the landing (`2ff3389b`, commit `38ab368`,
bench 1, CONTENDED host - read wall from this row as weather, not
verdict): splits fired on 27 partitions into 197 pieces, and every
straggler signal moved as designed - `assemble_partition_encoded_max`
296.6 MB -> 23.6 MB (12.6x), max reader time per order slot 3.63s ->
0.32s, `assemble_parked_bytes_hwm` 1.25 GB -> 123 MB (a real RAM-ledger
improvement: workers no longer park a gigabyte behind a straggler).
Total reader thread-time 34.6s -> 37.4s is the +8% piece overhead
(pre-scan, re-open, per-piece heap) the quiet-host run must net against
the tail win. The assemble phase read 22.0 GB from disk vs 10.3 GB
pre-landing: the contended host (half the RAM taken by cross-project
load) could not hold the 14.7 GB of sort chunks in page cache, so the
phase-duration comparison (14.6s -> 16.0s) is void alongside wall.

The wall verdict (the measured win this landing exists for) is PENDING a
quiet host. Compare against `3f042746` (54.1s wall, assemble 14.6s,
writer batch-wait 12.0s, `assemble_partition_encoded_max` 296.6 MB) with
the same spelling: `brokkr tilegen --bench 1 --dataset germany --variant
locations`, then sidecar `--stalls` and the split counters. Check the
results row's memory field first - the 07-26 rows carry ~10 GB against
the baseline's 21.4 GB, which is the tell.
