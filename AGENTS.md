# Elivagar

Shortbread vector tile generator. Reads OSM PBF files and produces PMTiles v3 archives with 26 layers.

## Rules

### General rules

- Don't use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten.
- Don't remind the user of the rules. They wrote them, so they know them.
- The user can exempt you from any rule at any time.
- ./docs/* and ./notes/* are transient. Do not reference them from code comments. Code comments should contain the full context - it will outlive the docs.
- ./reference/* is durable and lives on. Code comments may reference these.
- ./research/* holds full vendored source for related projects, readable from any agent sandbox. Two are our own dependencies, not competitors: `research/pbfhogg/` (Rust, our PBF reader and the injection/preprocessing counterpart) and `research/iOverlay/` plus `research/i_float/`, `research/i_shape/`, `research/i_tree/`, `research/i_key_sort/` (the reference sources the in-tree polygon topology engine used for Simplify/Intersect was ported from; i_overlay itself is now only a dev-dependency, the differential oracle that gates that engine). Consult these directly when a task involves changing what pbfhogg injects into the PBF, or modifying the in-tree polygon topology engine. (The competitor sources - planetiler, tilemaker, tippecanoe, stedsplakat - live here too.)
- In general ./docs/ and ./notes/ documents, try to refrain from referencing direct line numbers in the rust source. You can use line numbers, but they drift fast.
- When asked to write a plan or a specification, read `reference/technical-implementation-spec.md` first; it defines what such a document must contain.

### Bash rules
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use `brokkr`.

## Brokkr tool

Invoked as `brokkr` from the project root (reads `./brokkr.toml` for project detection).

- `brokkr check [-- args]` - run clippy + tests. Supports `--features` and `--no-default-features`
- `brokkr env` - show environment info and dataset status with computed XXH128 hashes (copy into the `xxhash` field in `brokkr.toml`)
- `brokkr results [UUID]` - look up specific result by UUID prefix (shows full detail + hotpath report)
- `brokkr results [--commit X] [--compare A B] [--compare-last] [--command CMD] [--mode M] [--grep STR] [--top N]` - query/compare benchmark results from SQLite. `--mode` filters by measurement mode (`bench`/`hotpath`/`alloc`); `--grep` substring-matches against both the subprocess `cli_args` and the recorded `brokkr_args` (use it to find runs by flag/axis). Use `--top 0` to show all hotpath functions. Use `--compare-last --mode hotpath` to diff two most recent hotpath runs.
- `brokkr results <UUID> --timeline [--stat FIELD] [--fields F1,F2] [--every N] [--phase P] [--where EXPR]` - query sidecar /proc samples (JSONL, stats, downsampled, per-phase, filtered)
- `brokkr results <UUID> --markers --durations` - phase duration table from markers
- `brokkr results --compare-timeline <A> <B>` - phase-aligned sidecar comparison
- `brokkr results dirty --timeline --stat anon` - inspect last failed/dirty run
- `brokkr clean` - remove tilegen_tmp and scratch files
- `brokkr history [--command CMD] [--project P] [--failed] [--since DATE] [--slow MS] [-n N] [--all]` - query global command history (stored in `$XDG_DATA_HOME/brokkr/history.db`). Every brokkr invocation is recorded with timing, exit status, project, and git context. Works from any directory.

### Elivagar commands

Commands are top-level (no `bench`/`hotpath` namespace). Measurement modes are flags: `--bench [N]` (full benchmark, N runs), `--hotpath [N]` (function-level timing), `--alloc [N]` (allocation tracking). Without a measurement flag, the command does a plain build+run.

```
# Measured commands
brokkr tilegen [--bench [N] | --hotpath [N] | --alloc [N]] [--dataset D]
               [--variant V] [--skip-to ocean|sort|assemble] [--dry-run]
               [--stop MARKER]
brokkr pmtiles-writer [--bench [N] | --hotpath [N] | --alloc [N]] [--tiles N]
brokkr node-store [--bench [N] | --hotpath [N] | --alloc [N]] [--nodes N]
brokkr planetiler [--bench [N]] [--dataset D] [--variant V]
brokkr tilemaker [--bench [N]] [--dataset D] [--variant V]

# Verification
brokkr verify pmtiles [--dataset D] [--tiles VARIANT] [--geometry-stats]
brokkr regress [--dataset D]   # current output vs the blessed archive from
                               # brokkr.toml (datasets.<D>.blessed); no args
                               # = denmark. THE standing gate: denmark bench
                               # + brokkr regress after any pipeline change.
brokkr bless [--dataset D] [--commit H | --file P]  # promote an output to
                               # the blessed regress reference (copies into
                               # data/blessed/, updates brokkr.toml). Only on
                               # user say-so - blessing rotates the baseline.

# Archive inspection (wrap the elivagar subcommands; named pmtiles-inspect
# because brokkr inspect is pbfhogg's PBF inspector)
brokkr pmtiles-inspect [--dataset D] [--commit H | --file P]
brokkr diag [--dataset D] [--commit H | --file P] -z Z -x X -y Y
brokkr svg [--dataset D] [--commit H | --file P] -z Z -x X -y Y [-W] [-H] [-l] [-o]

# Utilities
brokkr compare-tiles <file_a> <file_b> [--sample N]
brokkr download-ocean
brokkr download-natural-earth
brokkr download <region> [--osc-seq N]  # PBF + indexed + OSC diffs, auto-registers in brokkr.toml

# Suite
brokkr suite elivagar [--bench [N]] [--dataset D] [--variant V]
```

**`tilegen` has no pipeline flags.** Ocean inputs, tile format, budgets and
geometry all live in `[<host>.tilegen.default]` in brokkr.toml: either it is
explicit in the block, or it is not set. What remains on the command line is
the input axis (`--dataset`, `--variant`), the measurement mode, and the
per-invocation resume point (`--skip-to`). To A/B a setting, edit the block -
the resolved value lands in the results DB as the literal subprocess
invocation in `cli_args`, so `brokkr results --grep` still finds any arm after
the fact.

That inversion is deliberate. A flag that can be omitted has a default
somewhere else, and this pipeline kept losing to exactly that: brokkr used to
stat `data/` for ocean shapefiles and pass whichever it found, so two runs of
the same binary on the same PBF could differ with nothing in the recorded
invocation saying how. `meta.*` kv pairs remain reserved for runtime
observations only; anything derivable from the invocation is not duplicated
there.

Two flags worth knowing: `--dry-run` validates argv, config and path
resolution without building or running, which is how you sanity-check a queued
script before leaving it overnight; `--stop MARKER` kills the child when that
sidecar marker fires, for benchmarking one phase.

### Sidecar profiler

Every `--bench`, `--hotpath`, and `--alloc` run automatically samples
`/proc/{pid}/{stat,io,status}` at 100ms intervals AND reads phase markers plus
application counters from a FIFO. All of it lands in `.brokkr/sidecar.db`
(gitignored, local-only), NOT results.db. Preserved even if the child is
OOM-killed. The git-tracked `.brokkr/results.db` keeps only the small per-run
row (`elapsed_ms` plus git/host/env metadata and the literal `cli_args`); the
whole metric firehose lives in the sidecar.

brokkr creates the FIFO, sets `BROKKR_MARKER_FIFO` in the child's environment,
and drains it on a background thread. Two line formats share the FIFO:
- Markers: `{timestamp_us} {NAME}\n`
- Counters: `{timestamp_us} @{name}={value}\n` (value must parse as i64 or the
  line is dropped - there is no string/categorical counter channel)

Emission lives in `src/debug.rs` (`emit_marker`, `emit_counter`,
`emit_counter_u64`/`_usize`, `marker_span`, `emit_alloc_boundary`) - OnceLock fd
caching, O_NONBLOCK, silent no-op when `BROKKR_MARKER_FIFO` is unset. That means
the metrics are visible ONLY through `brokkr sidecar <uuid>` after a measured
run: a bare `elivagar run`, or `brokkr tilegen` with no measurement flag, emits
none of them (run mode stores nothing and attaches no sidecar).

What elivagar emits:
- Phase boundary markers, and ONLY these: `PHASE12_START/END`, `OCEAN_START/END`,
  `SORT_START/END`, `ASSEMBLE_START/END`. Markers are reserved for phase
  boundaries - the marker-consuming views (default summary, `--durations`,
  `--phase`) each treat a marker as a segment boundary, so keeping the stream to
  ~4 boundaries is what keeps those views compact.
- `<category>_wait_ns` cumulative stall counters (`src/debug.rs`: the `WAIT`
  static plus the `wait_span` RAII guard). One per blocking category
  (`sort_chunk_write`, `assemble_partition_batch`, `assemble_encode_input`,
  `pmtiles_write`, ...); each `wait_span` adds its measured nanoseconds to the
  category atomic on drop, and `emit_wait_counters()` flushes the totals once at
  end of run. Blocking time is an accumulated quantity, not a boundary, so it
  belongs in the counter channel - a per-event marker would flood the phase
  views. brokkr's `--stalls` rolls up every `*_wait_ns` counter (max per name,
  since they are cumulative) as a fraction of wall.
- `phase12_*_ns` actor busy counters (`src/debug.rs`: the `BUSY` static,
  same `wait_span` guard, flushed by the same `emit_wait_counters()`). Busy
  time on phase12's actors - ordered-consumer node-block work and
  drain-thread result handling are serial-stage time; plan build and the
  relation tail run inside rayon, so read those two as summed thread-time.
  The `_ns`-without-`_wait` suffix keeps them out of `--stalls`; paired with
  the wait counters they split each actor into busy vs blocked.
- `malloc_held_<boundary>` / `malloc_live_<boundary>` at each phase boundary:
  glibc heap accounting via `mallinfo2` (held = arena + hblkhd, live =
  uordblks + hblkhd; held minus live is allocator retention). RSS/peak
  RSS/faults are already covered per phase by the /proc sampler, so the one
  number worth pulling from the allocator is what it holds versus what the
  program holds. Sidecar rows from before 2026-07-15 carry
  `mi_commit_<boundary>` / `mi_peak_commit_<boundary>` instead: the binary
  was mimalloc-allocated then (mallinfo2 was a dead signal under it), and
  those counters read mimalloc's committed bytes via `mi_process_info` -
  the signal that exposed the 07-14 scratch-retention regression before
  mimalloc lost the post-fix allocator A/B and was removed.
- Coarse metric counters: `total_ms`, `phase12_ms`, `ocean_ms`, `phase3_ms`,
  `assemble_ms`, `features`, `tiles`, `unique_tiles`, `output_bytes`,
  `peak_rss_kb` + per-phase rss, `tile_format`/`tile_compression` (enum ints:
  format 0=mvt/1=mlt, compression 0=gzip/1=brotli), ocean input
  (`ocean_shapes`/`_shapes_hit`/`_pieces`/`_shapefile_bytes`), sort merge
  (`sort_merge_bytes`, `sort_merge_max_fanin`), plus dedup, oversize, and
  missing-ref stats. This is the ~100-counter set that a bare measured run emits.
- The per-layer per-zoom firehose (`sort_layer_<layer>_z<z>_records`/`_bytes`
  /`_fanout_p50`/`p95`/`p99`/`max`/`above_N`, ~800 counters) is gated behind
  `ELIVAGAR_LAYER_STATS`. Left off, `--counters` stays readable; set it when
  doing layer or fanout-cap analysis. The per-layer totals
  (`sort_layer_<name>_records`/`_bytes`) are always emitted.

pbfhogg, used as the PBF reader, emits its OWN counters into the same FIFO
(`pipeline_decode_tasks`, `pipeline_reorder_high_water`, and its own
`pipeline_*_wait_ns` stall counters). They show up in `--counters` alongside
elivagar's, and because `--stalls` rolls up any `*_wait_ns` counter, pbfhogg's
decode-pipeline stalls appear there too - one stall view, both projects.

Query with `brokkr sidecar <uuid>` (JSONL by default, `--human` for tables):
- (no selector) - per-phase summary: duration, peak RSS/anon, disk IO, avg cores
- `--durations` - phase START/END pair timings (the four phases; no WAIT noise)
- `--stalls` - `*_wait_ns` counters as a fraction of wall, biggest first. The
  fraction may exceed 100%: a category's counter is summed across concurrent
  threads, so e.g. 430% reads as "on average ~4.3 threads blocked here."
- `--counters` - application counter values over time
- `--markers` / `--samples` - raw marker or /proc-sample JSONL
- `--stat <field>` - min/max/avg/p50/p95 for one sample field (`rss`, `anon`,
  `majflt`, `rd`, `wr`, ...)
- `--compare <a> <b>` - phase-aligned comparison of two runs

### Common flags

All measurement commands share: `--force` (run with dirty git tree, results not stored), `--verbose` (full output), `--commit <hash>` (build and benchmark an old commit), `--features <F>` (cargo features), `--wait` (queue behind lock instead of failing).

### brokkr.toml

```toml
project = "elivagar"

[plantasjen]
data = "data"
scratch = "data/scratch"

# The tilegen contract. Everything `brokkr tilegen` passes to elivagar comes
# from here; there are no override flags. Paths are relative to `data`.
[plantasjen.tilegen.default]
ocean = [
    "z0-z7:simplified-water-polygons-split-3857/simplified_water_polygons.shp",
    "z8-z14:water-polygons-split-3857/water_polygons.shp",
    "ocean-tiles.pmtiles",
]

[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "8.0,54.5,13.0,58.0"

[plantasjen.datasets.denmark.pbf.raw]
file = "denmark-raw.osm.pbf"
xxhash = "aa5bb865..."
seq = 4704
```

- `tilegen.default` - the pipeline contract, per host. `ocean` is the repeatable
  `--ocean` spelling from `reference/cli.md`: zoom-ranged shapefiles plus an
  optional `.pmtiles` artifact. Omit `ocean` entirely and the run has no ocean.
  Drop the `.pmtiles` line for an artifact-absent arm; `brokkr results --grep`
  separates the arms afterwards, because the resolved value is in `cli_args`.
- `pbf.<variant>` - PBF files keyed by variant name. `--variant` selects (default: `raw`).
- `brokkr tilegen --dataset denmark --variant locations` - elivagar auto-detects `LocationsOnWays` from the PBF header. That detection reads the header, not the filesystem, and survives on purpose: it is a property of an input that is itself named explicitly and hashed into provenance. `elivagar inspect` prints the header features it found.
- `xxhash` - XXH128 file hash. Run `brokkr env` to see computed values.

Benchmark results stored in `.brokkr/results.db` (SQLite, tracked in git). Each
`--bench` row is one number plus provenance: brokkr's OWN external wall-clock
`elapsed_ms` (best-of-N, measured by brokkr wrapping the subprocess start to
exit - the same pbfhogg-parity path), plus git/host/env metadata and the literal
`cli_args`/`brokkr_args`. tilegen no longer self-reports timing on stderr: brokkr
reads nothing from tilegen's stderr in `--bench`, and every pipeline metric goes
to sidecar.db (above), not the results row. `--hotpath`/`--alloc` rows
additionally carry the hotpath JSON timing/alloc report (and that capture path
still scrapes stderr, so those rows also show node-store stats and the
locations-on-ways flag - the `--bench` row does not). Runs with different flags
are distinguishable via `cli_args`/`brokkr_args` - `brokkr results --grep ...`
finds any flag combination. Bench and hotpath require a clean git tree (ignoring
`*.md` and `.brokkr/results.db`); use `--force` to run anyway (results will not
be stored). Example: `brokkr tilegen --bench --force --dataset denmark`.

**NEVER run two elivagar processes at the same time.** They share `data/tilegen_tmp/` (causes crashes) and hotpath uses conflicting cargo feature flags (causes build conflicts). Always run sequentially.

## Scripts

Build/bench/verify tooling is in `brokkr`. One helper shell script lives in
`scripts/` for a gate brokkr does not wrap.

**Shell (`scripts/`, run from repo root):**
- `ocean-coverage.sh` - **BROKEN as of 2026-07-14, pending a decision to fix or
  delete.** It drives the ocean coverage discriminator by invoking the
  `elivagar` binary directly, and its step 2 builds the verbatim baseline with
  `--ocean`/`--ocean-simplified`/`--no-ocean-simplify`. All three spellings
  were removed with the ocean CLI rework: the first two have a new syntax, but
  `--no-ocean-simplify` is gone outright, so the baseline it caches cannot be
  produced at all and the script cannot be repaired by rewriting flags. It
  drove a diagnostic that was already demoted from gate to triage (one
  confirmed false negative; see the `ocean-coverage` subcommand caveat), so the
  live question is whether that diagnostic is worth keeping a verbatim-emit
  path in the production binary for.

**Node (`scripts/validate/`, pnpm; run from that directory):**
- `earcut-oracle.mjs <file.pmtiles> [layer] [threshold]` - **the MapLibre tessellation-fidelity gate.** Decodes every tile with @mapbox/vector-tile, groups rings with maplibre-gl's verbatim self-calibrating `classifyRings` (maxRings=500), tessellates each polygon with earcut, and reports per-zoom `earcut.deviation` plus misattached-hole counts (hole bbox outside its assigned outer). Pass = 0 over threshold, 0 misattached, on every polygon layer. This is the oracle that caught the R23 ClosePath cursor bug after every internal validator passed for three months - run it on any change that touches geometry or MVT encoding.
- `feature-probe.mjs <file.pmtiles> <z> <x> <y> <layer> <featIdx>` - dumps one feature exactly as MapLibre sees it: per-ring vertex count, signed area, bbox, then classifyRings grouping and per-polygon deviation. For drilling into an oracle offender.
- `winding-probe.mjs <file.pmtiles> <z> <x> <y> [layer]` - per-ring signed-area/winding summary for every polygon feature in one tile.
- `ring-grouping-oracle.mjs <file.pmtiles> -o <out.txt>` - emits the canonical polygon ring grouping dump used to differentially inspect corpus rendering.
- `validate.mjs` / `roundtrip.mjs` - vtvalidate structural checks and decode/re-encode round-trip (NOTE: a round-trip through any single decoder cannot catch symmetric encoder/decoder convention bugs - that is what the earcut oracle is for).
- `boundary-line-oracle.mjs <file.pmtiles> [--only categories]` - the `boundaries`-layer line-fidelity gate. Decodes every line feature into its MoveTo-delimited sub-lines and flags, per zoom, palindromes (a sub-line equal to its own reverse), spurs (a retrace apex not at the sub-line ends, closure pair excluded for closed loops), intra-feature duplicates, and cross-feature duplicates (exact and reversed) - the last catches the maritime=true/false double-draw that `merge_same_attr_geometries` cannot merge across differing attributes. Categories are separately selectable via `--only`. Run on any change touching boundary-line emission, the line merger, or closed-line simplification.
- `line-probe.mjs <file.pmtiles> <z> <x> <y> <featIdx> <subIdx>` - dumps one `boundaries` sub-line exactly as MapLibre decodes it (per-vertex coordinates) plus any spur apexes, for drilling into a boundary-line-oracle offender.
- `svg-roi.mjs <minx> <miny> <maxx> <maxy> <file.svg...>` - prints every path edge touching a bbox ROI in SVG dumps made with `brokkr svg -o`, so one defect region can be compared across many archives without reading whole tiles (no deps, plain node). Built for the 2026-07-15 stale-ocean-artifact hunt; the archive-bracketing idiom is: dump the same tile from each archive, scan the same ROI, diff the edges.

## Oracle discipline

A validator gates work only after it is calibrated in BOTH directions; until
then it is advisory, never blocking. Before promoting any new oracle to a gate:

- (a) it FIRES on a known-bad artifact - the defect it targets,
- (b) it CLEARS on a known-good artifact, or absent one, a defect-free control
  region, and
- (c) it measures the defect itself, or its correlation to the defect is
  demonstrated - including the null-hypothesis arithmetic ("what does a correct
  build score?") before any threshold is chosen.

The gates that have ever been authoritative here (earcut, boundary-line) share
three properties, and consumer-path decode is only the first: consumer-path
decode (read geometry the way MapLibre does), a CATEGORICAL defect with
near-zero base rate on good output (pass = count 0, and 0 is achievable), and
both-direction calibration. A gate that thresholds a CONTINUOUS quantity that is
legitimately large on good output is a false negative waiting to happen: the
ocean coverage measure did exactly this, reporting a working low-zoom ocean fix
as FAILED because any correct simplifier loses large sub-pixel coastline detail
versus a verbatim baseline. That case is why this rule exists; it was caught
only by the human visual check, so the visual check and earcut - not aggregate
area - are the standing ocean gates. `ocean-coverage` is triage, never a gate.

## Architecture

Single-crate library + binary. Public API is `elivagar::run(&TilegenConfig)`. CLI uses clap derive with subcommands (`run`, `inspect`, `verify`, `svg`, `diag`, `regress`, `corpus`, `ocean-coverage`).

### Modules

**Pipeline orchestrator:**
- `pipeline.rs` - PBF → ocean → sort → assemble → PMTiles

**Shortbread profile:**
- `shortbread/` - tag matching, layer definitions, 26 layers (mod.rs, boundaries.rs, land.rs, streets.rs, transport.rs, water.rs)
- `shortbread_tests.rs` - spec test cases (65+), loaded via `#[path]` from shortbread/mod.rs
- `pois.rs` - POI tag matching
- `wire_format.rs` - sort record binary serialization

**Geometry + encoding:**
- `geometry.rs` - Mercator projection, clipping (Sutherland-Hodgman for OSM layers), Douglas-Peucker simplification
- `geometry/int_ocean.rs` - integer polygon geometry engine (ocean AND OSM layers): early quantization to max_zoom pixel space (unclamped, antimeridian-safe), exact shift-round per-zoom rescale, rotation-invariant pin-aware integer DP, Simplify/Intersect (NonZero) topology ops via the in-tree `geometry/overlay/` boolean engine (ported from i_overlay, no longer a dependency - i_overlay lives on only as a dev-dependency differential oracle), recursive row-band bisection, shared per-zoom emission engine (emit_shape_for_zoom). ALL polygon emission goes through this - see specs/. The earcut oracle (scripts/validate/) is the standing gate: 0 deviant polygons, 0 misattached holes, every polygon layer, every build that touches geometry or MVT encoding.
- `mvt.rs` - MVT protobuf encoder. CRITICAL: ClosePath does NOT move the delta cursor (MVT spec 4.3.3.3) - a symmetric encoder/decoder violation of this was invisible to all internal round-trips for three months (ledger R23)
- `multipolygon.rs` - relation ring assembly
- `ocean.rs` - ocean shapefile processing (mmap reader + quantize-early integer boolean clipping via the shared int_ocean pyramid; no scanline fill, no point-in-polygon, no S-H, no LandMask)

**Infrastructure:**
- `sort.rs` - external sort partitioned by Hilbert tile-id range at write time (z7-calibrated partitions, `PARTITION_SPLIT_Z`; chunk files uncompressed by default, LZ4/Snappy via `--compress-sort-chunks`; per-partition k-way merge via binary heap, consumed lazily by the assemble partition readers)
- `pmtiles_writer.rs` - PMTiles v3 writer with Hilbert tile IDs
- `inspect.rs` - PMTiles v3 archive inspector (header + metadata reader)
- `svg.rs` - single-tile SVG renderer (decodes MVT geometry from PMTiles, outputs SVG)
- `ocean_coverage.rs` - same-source one-sided ocean coverage diff (the `ocean-coverage` subcommand); decodes both archives' ocean polygons and integrates lost area per tile. Diagnostic, not a gate.
- `node_index.rs` - node coordinate index (SortedNodeStore for sorted PBFs, flat mmap fallback)
- `way_index.rs` - flat mmap'd way geometry index

### Pipeline phases

Sequential, same PBF input:
- **phase12**: PBF read + OSM feature emission → partitioned sort chunks + checkpoint
- **ocean**: shapefile read + ocean feature emission → more sort chunks.
  When `--ocean` names the durable world-ocean artifact (built once per
  shapefile release by `elivagar ocean-build`), this phase computes only
  the boundary band near the extract bbox edge (empty at world bounds)
  and assemble merges the artifact as run copies for the interior. The
  artifact is used because it was named, never because it was found: it
  serves only the MVT+gzip z0-14 contract at a matching compression
  level, and naming it for any other run is an error, not a silent
  fallback to computed ocean. Artifact-served tiles differ benignly from
  extract-computed ones (descent seams depend on the piece clip extent;
  adjudicated equivalent in the viewer 2026-07-12) - so the blessed
  regress baseline is artifact-active, the gate machine must pass the
  same artifact, and rotating the artifact forces a bless rotation.
- **sort**: partition bookkeeping only (near-zero; the merge is deferred)
- **assemble**: streamed per-partition merge → MVT encode + gzip + PMTiles write (parallel partition readers; merge/decompress cost lands in `assemble_reader_ns`)

`--skip-to ocean` reuses PBF chunks from a previous full run.
`--skip-to sort` reuses all chunks (PBF + ocean).

## CLI

**`reference/cli.md` is the complete surface.** It is the reference; this
section covers only what a session reaches for most, and does not repeat the
flag lists - the duplicate list that used to live here is how `--ocean-tiles`
stayed undocumented for the two days it mattered.

### The rule

**Either it is explicit, or it is not set.** Nothing is inferred from the
filesystem. Ocean auto-detection was removed on 2026-07-14 because it put the
run's meaning in the filesystem instead of the invocation: `cli_args` records
the literal subprocess call, so no bench row could be classified after the fact
as artifact-active or computed, and a denmark archive was blessed as the
regress baseline while the artifact was silently absent.

### `elivagar run <INPUT> -o <OUTPUT> [flags]`

Ocean input is `--ocean <SPEC>`, repeatable; **omit it entirely for no ocean.**
Specs are `z0-z7:<shp>`, `z8-z14:<shp>`, `z0-z14:<shp>`, or `<file.pmtiles>`.
Shapefiles must partition z0-z14 as either `z0-z14` alone or the `z0-z7` +
`z8-z14` pair - the engine splits at z7/z8 and nowhere else, and any other
partition is rejected rather than silently rounded. The `.pmtiles` artifact is
a cache over the shapefiles, not a substitute: an extract computes its boundary
band from them and the artifact key is validated by re-hashing them, so the
artifact is rejected on its own. The production shape:

```
--ocean z0-z7:data/simplified-water-polygons-split-3857/simplified_water_polygons.shp
--ocean z8-z14:data/water-polygons-split-3857/water_polygons.shp
--ocean data/ocean-tiles.pmtiles
```

Everything else - budgets (`--sort-budget`, `--way-budget`,
`--assemble-budget`), formats (`--tile-format`, `--tile-compression`,
`--compress-sort-chunks`), geometry (`--fanout-cap`, `--seam-reconcile-layers`,
`--polygon-simplify-factor`), `--skip-to`, `--locations-on-ways`, `--threads` -
is in `reference/cli.md`.

### `elivagar inspect <FILE>`

Reads a PMTiles archive and prints header info, tile statistics, section
layout, provenance, and metadata (layer list with zoom ranges).

The provenance section is how you answer "was this archive artifact-active or
computed" and "is it comparable to that one". `Input` plus the whole of
`Config` is the comparability contract and is printed in full, not sampled -
a partial contract would let two archives differing in a fanout cap or the
simplify factor display identically. `Build`, `Effective` and `Resumed` are
diagnostic and must never be equality-gated.

Reporting is not enforcement: `brokkr regress` still does not read the block,
so these lines let you refuse a comparison, they do not refuse it.

Every way of having no contract names itself - `absent`, `unavailable`,
`invalid`, an uninterpretable schema, an `INCOMPLETE` contract - because
silence would read as nothing to report. Full detail in `reference/cli.md`;
the raw block via `scripts/dump-pmtiles-metadata.py`.

### `elivagar verify <FILE>`

Validates a PMTiles archive end-to-end: container integrity, metadata schema, tile decompression, MVT payload structure, geometry command validation, and layer coverage. Also checks ocean polygon rings for self-intersections. `--geometry-stats` prints per-zoom ocean-layer statistics (ring counts, max/p99 ring vertices, consecutive duplicates, full-tile fills). Exits 0 on pass, 1 on failure. Stops after 100 tile-level errors.

### `elivagar svg <FILE> -z <Z> -x <X> -y <Y> [-W width] [-H height] [-l layers] [-o output.svg]`

Renders tiles from a PMTiles archive as SVG. Supports single tiles or NxM grids (`-W`/`-H`, default 1x1). `--layers` filters to specific layers (comma-separated, e.g. `ocean,boundaries`). Decodes MVT geometry and draws each layer with a distinct color. Points render as circles, lines as stroked paths, polygons as filled paths with `nonzero` fill-rule (matching MapLibre and OpenLayers, which both fill nonzero; `evenodd` agrees only for well-formed alternating outers/holes and papers over exactly the ring-role bugs worth catching). Background is land-colored (`#f2efe9`). Grid lines drawn between tiles when width or height > 1. Output goes to stdout by default, or to a file with `-o`.

### regress - invoke as `brokkr regress`, never the raw binary

A blessed archive is ALWAYS locations-generated - never bless a raw one.
`tilegen` and `regress` default to `raw`, so pass `--variant locations`
explicitly; raw vs locations tiles differ legitimately, not as a regression.

`brokkr regress [--dataset D]` resolves the current output and the blessed
archive (`datasets.<D>.blessed` in brokkr.toml) itself; no paths, defaults
to denmark. What it computes: a semantic diff of two PMTiles archives
(MVT + gzip only). Decodes every tile into a canonical form (layers sorted,
features sorted, merged-feature components sorted - erasing intra-layer
feature order, the one dimension the pipeline deliberately leaves
unconstrained across archives, and nothing else). Within-run record order is
total (chunk-independent sort + paint-order key, `src/sort.rs` +
`src/shortbread/paint_order.rs`), so two builds of the same commit are
byte-identical and never need this canonicalization; two builds of
*different* commits can still legitimately reorder features (e.g. a
paint-rank table change) without any semantic difference, which is what the
canonicalization tier continues to absorb. It then classifies:
tiles/layers added or removed, extent mismatches, missing/added features,
attr changes (bit-exact values), and matched-feature geometry moves split
into tolerance vs structural (component-count / ring-role /
hole-containment changes). Ocean features match geometrically (their ids
are synthetic). Exit 0 only if nothing structural; the report prints
per-zoom/layer counters and displacement percentiles. Design and gates
settled in the spec-5 output-regression landing (see git history).

### `elivagar diag <FILE> -z <Z> -x <X> -y <Y>`

Diagnoses ocean polygon ring winding for a specific tile. Decodes MVT protobuf, finds polygon features across all layers, and prints per-ring vertex count, signed area, and winding direction (CW = outer, CCW = hole). Prints first/last 3 vertices for large rings, full vertices for small ones (≤6).

### `elivagar ocean-coverage <FILE> --baseline <REF> [--zmin Z] [--zmax Z] [--threshold-2x N] [--layer L]`

Diagnostic, NOT a landing gate, and currently UNFEEDABLE: its baseline was
built with `--no-ocean-simplify`, removed 2026-07-14, and there is no other way
to produce a verbatim same-source comparand. Only an archive built before that
removal still works as `--baseline`. Measures per-tile one-sided ocean coverage
loss of FILE against that baseline REF.
For each z in `[zmin, zmax]` it clips both archives to the tile extent, computes
`area(ref) - area(ref INTERSECT file)` in 2x-pixel^2 units, prints every tile
over `--threshold-2x` plus a per-zoom max/p99/worst summary, and exits nonzero
if any tile exceeds the threshold. Defaults: zmin 1, zmax 6, threshold-2x 512,
layer `ocean`.

Known limitation: at low zoom ANY correct simplifier removes large sub-pixel
coastline detail versus a verbatim baseline, so this over-reports and cannot
separate legitimate generalization from a real coverage defect (a confirmed
false negative on the 2026-07-12 ocean VW landing). Use it as a
discriminator - compare two builds' losses to price a regression - never as
a pass/fail gate. The
authoritative ocean gates remain the earcut oracle and the human visual check.
Driven by `scripts/ocean-coverage.sh` (brokkr has no wrapper).

## Key conventions

- The global allocator is the system allocator, decided by measurement - do
  not reintroduce an allocator dependency without a fresh A/B (main.rs has
  the numbers; mimalloc and jemalloc lost at `98824b4`)
- `.unwrap()` forbidden by clippy - use `expect()` or propagate errors
- Cast lints are strict - annotate with `#[allow(clippy::cast_*)]` where needed
- Test fixtures live in `tests/fixtures/` (YAML files for Shortbread spec)
- **Test geometry must fit in one tile at the test zoom level.** World-spanning polygons (e.g. [0.1-0.9] Mercator) at z14 iterate 268M tiles and OOM the machine. If a test needs high zoom, use geometry confined to a single tile at that zoom.
- Env vars: no variable duplicates a flag or overrides one - a knob is a flag or an env var, never both. brokkr's `capture_env` in brokkr.toml matches `ELIVAGAR*`/`MALLOC*` and stores what it finds with the run, so an env-configured run is reconstructable from the results row. The set is `ELIVAGAR_ASSEMBLE_WORKERS`, `ELIVAGAR_ASSEMBLE_PARK_BUDGET`, `ELIVAGAR_REL_BLOCKS_CAP`, `ELIVAGAR_LAYER_STATS`; full detail in `reference/cli.md`. In-session, `VAR=x brokkr ...` is blocked by the harness permission matcher, so use `scripts/envrun.sh VAR=x brokkr ...`.
- `ELIVAGAR_LAYER_STATS=1` - emits the per-layer per-zoom sort-stats firehose (`sort_layer_<name>_z<z>_records`/`_bytes`/`_fanout_p50`/`p95`/`p99`/`max`/`above_N`, ~800 counters). Off by default so `brokkr sidecar --counters` stays readable; the per-layer totals (`sort_layer_<name>_records`/`_bytes`) are always emitted. Emitted at end of run, so it never affects timing. Set it (it is inherited by the child through `brokkr`) for layer or fanout-cap analysis.
- Memory instrumentation (`3a729ab`) - always-on, not feature-gated. Emits per-phase peak RSS (`phase12_rss_kb`, `ocean_rss_kb`, `sort_rss_kb`, `assemble_rss_kb`), `sort_chunks`, and in-flight HWM counters (`max_way_inflight_bytes`, `max_rel_inflight_bytes`, `max_assemble_batch_bytes`). Overhead is negligible: 4 `/proc` reads total, per-block byte estimation, per-feature counter increment. Nothing in hot inner loops.

## Benchmarks

Baselines, benchmark machines, gate commands, discipline, and the rules for
reading hotpath/alloc/bench numbers live in `reference/performance.md`. That
document plus `.brokkr/results.db` is the measurement record specs cite - though
after the sidecar migration the tracked results row holds only wall-clock
`elapsed_ms`; the per-phase timings and per-layer stats a spec wants to quote now
come from the local-only `.brokkr/sidecar.db` (`brokkr sidecar <uuid>`), so
capture those numbers into the spec or `reference/performance.md` when they
matter, since sidecar.db does not travel between machines.

Typical loop, from a clean tree:

```
brokkr tilegen --bench --dataset denmark      # -> UUID, wall-clock best-of-3
brokkr results <uuid>                          # row: elapsed_ms + provenance
brokkr sidecar <uuid> --human                  # per-phase RSS / IO / cores
brokkr sidecar <uuid> --stalls --human         # WAIT_ blocking-time breakdown
brokkr sidecar <uuid> --counters               # all ~900 metric counters
brokkr tilegen --hotpath --dataset denmark     # function-level timing report
brokkr tilegen --alloc  --dataset denmark      # per-function allocation report
```

Never run two elivagar processes at once (shared `tilegen_tmp/`, conflicting
hotpath feature flags) - the three modes go one at a time.

## Data

- `data/tilegen_tmp/` - temporary sort chunks (inside gitignored `data/`)
- Ocean shapefile not included - pass via `--ocean` flag
