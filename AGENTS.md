# Elivagar

Shortbread vector tile generator. Reads OSM PBF files and produces PMTiles v3 archives with 26 layers.

## Rules

### General rules

- Don't use gremlins! Em-dash, en-dash, strange quotes, whatever - they're all verboten.
- Don't remind the user of the rules. They wrote them, so they know them.
- The user can exempt you from any rule at any time.
- ./docs/* and ./notes/* are transient. Do not reference them from code comments. Code comments should contain the full context - it will outlive the docs.
- ./reference/* is durable and lives on. Code comments may reference these.
- In general ./docs/ and ./notes/ documents, try to refrain from referencing direct line numbers in the rust source. You can use line numbers, but they drift fast.
- When asked to write a plan or a specification, read `reference/technical-implementation-spec.md` first; it defines what such a document must contain.
- **Never run the full pipeline on real PBF data (brokkr tilegen, brokkr tilegen --bench) unless the user explicitly asks.** Use synthetic benchmarks (brokkr node-store, brokkr pmtiles-writer) for iteration. Full pipeline runs are expensive and should only happen when the user decides it's time.

### Bash rules
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use `brokkr`.

### git commit rules

- Always run `brokkr fmt` before a commit.
- Never commit markdown changes alone. Bundle them with upcoming code commits.
- When committing other changes: always tag along markdown files if dirty.
- Write substantive engineering-focused commit messages.
- Hard-wrap the message body at ~72 columns, matching the existing history; the
  subject stays one concise line. The wall-of-text we keep producing comes from
  `git commit -m "<whole paragraph>"`: a single `-m` is recorded as ONE unwrapped
  line. Embed real line breaks so every body line wraps at ~72 (one `-m` per
  paragraph is fine only when each paragraph already carries its own newlines).
  Newlines are not metacharacters, so this composes with the no-metacharacters-in
  `-m` rule (CLAUDE.md Bash rules) - wrap with literal newlines while still
  avoiding braces, brackets, parens, angle brackets and the hash sign.
- Has `Cargo.lock` changed? Commit it.
- Never `git push` unless the user explicitly asks. Stop after the commit.

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
brokkr tilegen [--bench [N] | --hotpath [N] | --alloc [N]] [pipeline flags...]
brokkr pmtiles-writer [--bench [N] | --hotpath [N] | --alloc [N]] [--tiles N]
brokkr node-store [--bench [N] | --hotpath [N] | --alloc [N]] [--nodes N]
brokkr planetiler [--bench [N]] [--dataset D] [--variant V]
brokkr tilemaker [--bench [N]] [--dataset D] [--variant V]

# Verification
brokkr verify pmtiles [--dataset D] [--tiles VARIANT]

# Utilities
brokkr compare-tiles <file_a> <file_b> [--sample N]
brokkr download-ocean
brokkr download-natural-earth
brokkr download <region> [--osc-seq N]  # PBF + indexed + OSC diffs, auto-registers in brokkr.toml

# Suite
brokkr suite elivagar [--bench [N]] [--dataset D] [--variant V]
```

Pipeline flags on `tilegen` (`--tile-format`, `--tile-compression`, `--compress-sort-chunks`, `--in-memory`, `--locations-on-ways`, etc.) are passed through to the elivagar binary. They land in the results DB as the literal subprocess invocation in `cli_args` - query by flag with `brokkr results --grep tile-compression=brotli`. `meta.*` kv pairs are reserved for runtime observations only (detected locations-on-ways mode, resolved paths); anything derivable from the invocation is not duplicated there.

### Sidecar profiler

Every `--bench`, `--hotpath`, and `--alloc` run automatically samples `/proc/{pid}/status` and `/proc/{pid}/io` at 100ms intervals. Data stored in `.brokkr/sidecar.db` (gitignored, local-only). Preserved even if the child is OOM-killed.

Phase markers and counters via FIFO: brokkr creates a FIFO, sets `BROKKR_MARKER_FIFO` in the child's environment, spawns a sidecar thread for `/proc` sampling, reads markers/counters from the FIFO, and bulk-inserts everything into results.db after exit.

Protocol (two line formats, same FIFO):
- Markers: `{timestamp_us} {PHASE_NAME}\n`
- Counters: `{timestamp_us} @{name}={value}\n` (i64 value)

Elivagar emits markers at phase boundaries (`PHASE12_START/END`, `OCEAN_START/END`, `SORT_START/END`, `ASSEMBLE_START/END`) and counters for key metrics (`phase12_ms`, `ocean_ms`, `ocean_features`, `assemble_ms`, `tiles`, `unique_tiles`, `features`). Implementation is in `pipeline/mod.rs` via `emit_marker()`/`emit_counter()` - OnceLock fd caching, O_NONBLOCK, no-op when brokkr isn't running.

Query with:
- `brokkr results <uuid> --markers --durations` - phase timing table
- `brokkr results <uuid> --markers --counters` - counter values
- `brokkr results <uuid> --markers --phases` - phases with peak RSS + counters inline

### Common flags

All measurement commands share: `--force` (run with dirty git tree, results not stored), `--verbose` (full output), `--commit <hash>` (build and benchmark an old commit), `--features <F>` (cargo features), `--wait` (queue behind lock instead of failing).

### brokkr.toml

```toml
project = "elivagar"

[plantasjen]
data = "data"
scratch = "data/scratch"

[plantasjen.datasets.denmark]
origin = "Geofabrik"
download_date = "2026-02-20"
bbox = "8.0,54.5,13.0,58.0"

[plantasjen.datasets.denmark.pbf.raw]
file = "denmark-raw.osm.pbf"
xxhash = "aa5bb865..."
seq = 4704
```

- `pbf.<variant>` - PBF files keyed by variant name. `--variant` selects (default: `raw`).
- `brokkr tilegen --dataset denmark --variant locations` - elivagar auto-detects `LocationsOnWays` from the PBF header. The `--locations-on-ways` flag is only needed to force it when the PBF doesn't have the header flag.
- `xxhash` - XXH128 file hash. Run `brokkr env` to see computed values.

Benchmark results stored in `.brokkr/results.db` (SQLite, tracked in git for cross-host access). Runs with different flags are distinguishable via the `cli_args` and `brokkr_args` columns - the literal subprocess and brokkr invocations are stored verbatim, so `brokkr results --grep ...` finds any flag combination. Bench and hotpath commands require a clean git tree (ignoring `*.md` and `.brokkr/results.db`); use `--force` to run anyway (results will not be stored). Example: `brokkr tilegen --bench --force --dataset denmark`.

**NEVER run two elivagar processes at the same time.** They share `data/tilegen_tmp/` (causes crashes) and hotpath uses conflicting cargo feature flags (causes build conflicts). Always run sequentially.

## Scripts

No shell scripts. Build/bench/verify tooling is in `brokkr`.

**Node (`scripts/validate/`, pnpm; run from that directory):**
- `earcut-oracle.mjs <file.pmtiles> [layer] [threshold]` - **the MapLibre tessellation-fidelity gate.** Decodes every tile with @mapbox/vector-tile, groups rings with maplibre-gl's verbatim self-calibrating `classifyRings` (maxRings=500), tessellates each polygon with earcut, and reports per-zoom `earcut.deviation` plus misattached-hole counts (hole bbox outside its assigned outer). Pass = 0 over threshold, 0 misattached, on every polygon layer. This is the oracle that caught the R23 ClosePath cursor bug after every internal validator passed for three months - run it on any change that touches geometry or MVT encoding.
- `feature-probe.mjs <file.pmtiles> <z> <x> <y> <layer> <featIdx>` - dumps one feature exactly as MapLibre sees it: per-ring vertex count, signed area, bbox, then classifyRings grouping and per-polygon deviation. For drilling into an oracle offender.
- `winding-probe.mjs <file.pmtiles> <z> <x> <y> [layer]` - per-ring signed-area/winding summary for every polygon feature in one tile.
- `validate.mjs` / `roundtrip.mjs` - vtvalidate structural checks and decode/re-encode round-trip (NOTE: a round-trip through any single decoder cannot catch symmetric encoder/decoder convention bugs - that is what the earcut oracle is for).

## Architecture

Single-crate library + binary. Public API is `elivagar::run(&TilegenConfig)`. CLI uses clap derive with subcommands (`run`, `inspect`, `verify`, `svg`, `diag`).

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
- `geometry/int_ocean.rs` - integer polygon geometry engine (ocean AND OSM layers): early quantization to max_zoom pixel space (unclamped, antimeridian-safe), exact shift-round per-zoom rescale, rotation-invariant pin-aware integer DP, i_overlay Simplify/Intersect (NonZero) topology ops, recursive row-band bisection, shared per-zoom emission engine (emit_shape_for_zoom). ALL polygon emission goes through this - see specs/. The earcut oracle (scripts/validate/) is the standing gate: 0 deviant polygons, 0 misattached holes, every polygon layer, every build that touches geometry or MVT encoding.
- `mvt.rs` - MVT protobuf encoder. CRITICAL: ClosePath does NOT move the delta cursor (MVT spec 4.3.3.3) - a symmetric encoder/decoder violation of this was invisible to all internal round-trips for three months (ledger R23)
- `multipolygon.rs` - relation ring assembly
- `ocean.rs` - ocean shapefile processing (mmap reader + scanline fill + quantize-early integer boolean clipping; no S-H, no LandMask)

**Infrastructure:**
- `sort.rs` - external merge sort (gzip-compressed chunk files, k-way merge via binary heap)
- `pmtiles_writer.rs` - PMTiles v3 writer with Hilbert tile IDs
- `inspect.rs` - PMTiles v3 archive inspector (header + metadata reader)
- `svg.rs` - single-tile SVG renderer (decodes MVT geometry from PMTiles, outputs SVG)
- `node_index.rs` - node coordinate index (SortedNodeStore for sorted PBFs, flat mmap fallback)
- `way_index.rs` - flat mmap'd way geometry index

### Pipeline phases

Sequential, same PBF input:
- **phase12**: PBF read + OSM feature emission → sort chunks + checkpoint
- **ocean**: shapefile read + ocean feature emission → more sort chunks
- **sort**: external merge sort all chunks
- **assemble**: MVT encode + gzip + PMTiles write

`--skip-to ocean` reuses PBF chunks from a previous full run.
`--skip-to sort` reuses all chunks (PBF + ocean).

## CLI

### `elivagar run <INPUT> -o <OUTPUT> [flags]`

- `-o` / `--output` - output PMTiles path (required)
- `--tmp-dir path` - temporary directory for sort chunks (default: `data/tilegen_tmp`)
- `--ocean path.shp` - ocean polygon shapefile (water-polygons-split-3857). Auto-detected from `data/` when omitted.
- `--ocean-simplified path.shp` - simplified ocean shapefile for z0-7. Auto-detected from `data/` when omitted.
- `--no-ocean` - disable ocean shapefile processing (skip auto-detection)
- `--skip-to ocean|sort` - resume from checkpoint
- `--in-memory` - keep tile blob in RAM (faster for small extracts)
- `--compression-level 0-10` - compression level (default 6)
- `--tile-compression gzip|brotli` - tile compression algorithm (default gzip)
- `--force-sorted` - force compact node store even without PBF header flag
- `--locations-on-ways` - PBF has node coordinates embedded in ways
- `-j N` / `--threads N` - thread count (default: logical CPUs)
- `--sort-budget <size>` - sort chunk memory budget (default 1G). Accepts `256M`, `512M`, `1G`, or raw bytes. Minimum 64M. Lower values reduce peak RSS during PBF processing at the cost of more merge chunks.
- `--way-budget <size>` - in-flight way processing budget (default 128M standard, 256M in `--locations-on-ways` mode). Minimum 1M.
- `--rel-budget <size>` - relation batch accumulation budget (default 64M). Minimum 1M.
- `--assemble-budget <size>` - tile assembly batch budget (default 32M). Minimum 1M.
- `--fanout-cap-default N` - default fanout cap for all polygon layers (0 = uncapped). Per-layer overrides take precedence.
- `--fanout-cap layer=N,...` - per-layer fanout caps (e.g. `water_polygons=2048,boundaries=4096`). Features whose bbox tile count exceeds the cap are skipped at that zoom. Comma-separated, strict layer name validation.

### `elivagar inspect <FILE>`

Reads a PMTiles archive and prints header info, tile statistics, section layout, and metadata (layer list with zoom ranges).

### `elivagar verify <FILE>`

Validates a PMTiles archive end-to-end: container integrity, metadata schema, tile decompression, MVT payload structure, geometry command validation, and layer coverage. Also checks ocean polygon rings for self-intersections. `--geometry-stats` prints per-zoom ocean-layer statistics (ring counts, max/p99 ring vertices, consecutive duplicates, full-tile fills). Exits 0 on pass, 1 on failure. Stops after 100 tile-level errors.

### `elivagar svg <FILE> -z <Z> -x <X> -y <Y> [-W width] [-H height] [-l layers] [-o output.svg]`

Renders tiles from a PMTiles archive as SVG. Supports single tiles or NxM grids (`-W`/`-H`, default 1x1). `--layers` filters to specific layers (comma-separated, e.g. `ocean,boundaries`). Decodes MVT geometry and draws each layer with a distinct color. Points render as circles, lines as stroked paths, polygons as filled paths with `evenodd` fill-rule. Background is land-colored (`#f2efe9`). Grid lines drawn between tiles when width or height > 1. Output goes to stdout by default, or to a file with `-o`.

### `elivagar diag <FILE> -z <Z> -x <X> -y <Y>`

Diagnoses ocean polygon ring winding for a specific tile. Decodes MVT protobuf, finds polygon features across all layers, and prints per-ring vertex count, signed area, and winding direction (CW = outer, CCW = hole). Prints first/last 3 vertices for large rings, full vertices for small ones (≤6).

## Key conventions

- `#[global_allocator]` mimalloc in main.rs - do not remove
- `.unwrap()` forbidden by clippy - use `expect()` or propagate errors
- Cast lints are strict - annotate with `#[allow(clippy::cast_*)]` where needed
- Test fixtures live in `tests/fixtures/` (YAML files for Shortbread spec)
- **Test geometry must fit in one tile at the test zoom level.** World-spanning polygons (e.g. [0.1-0.9] Mercator) at z14 iterate 268M tiles and OOM the machine. If a test needs high zoom, use geometry confined to a single tile at that zoom.
- `ELIVAGAR_NODE_STATS=1` - enables detailed SortedNodeStore diagnostic scan (chunk counts, compression ratio, blob bytes). Runs during PBF phase so it adds to `phase12_ms` - safe for hotpath runs but not for bench timing. Basic stats (`node_store_nodes`, `node_store_groups`) are always emitted after all timing kv pairs and never affect benchmarks.
- Memory instrumentation (`3a729ab`) - always-on, not feature-gated. Emits per-phase peak RSS (`phase12_rss_kb`, `ocean_rss_kb`, `sort_rss_kb`, `assemble_rss_kb`), `sort_chunks`, and in-flight HWM counters (`max_way_inflight_bytes`, `max_rel_batch_bytes`, `max_assemble_batch_bytes`). Overhead is negligible: 4 `/proc` reads total, per-block byte estimation, per-feature counter increment. Nothing in hot inner loops.

## Benchmark discipline

All performance numbers (wall time, phase splits, allocation profiles) MUST include:
1. **Host name** (plantasjen, dm6, etc.)
2. **Git commit hash** of the code that was measured

Workflow: commit code first, THEN benchmark, THEN update docs with the commit hash.
Never write benchmark numbers for uncommitted code - the hash is the anchor.

## Benchmark machines

### plantasjen (current)
- CPU: AMD Ryzen 9 5900X (12 cores / 24 threads, 4.95 GHz boost)
- RAM: 30 GB DDR4
- Denmark PBF baseline (commit `60fd209`, post spec-3 ocean-perf): ~35s total (18s pbf, 12s ocean, 0.6s sort, 4.2s assemble), 2.8 GB RSS, 1323406 tiles, 351 MB output. The integer-clipping rewrite (ledger R21-R24) and the ocean-perf restructure (R25 / spec 3) reshaped this wholesale: ocean fell from 48.8s to 11.9s via parallel prologue + piece-by-zoom fan-out, at the cost of RSS rising to 2.8 GB from 24-way parallelism under mimalloc's non-purging arenas. Pre-rewrite Denmark was ~12.4s but with the earcut-broken geometry the rewrite fixed.
- North America baseline (commit `8704b11`, PRE-rewrite - stale, re-measure at scale): 605s total (413s pbf, 37s ocean, 0.7s sort, 155s assemble), 22.8 GB RSS, 12.5 GB output
- Node store baseline (50M nodes, commit `cb2cd29`): build 1.7s, way-like 77 ns/lookup, random 394 ns/lookup
- PMTiles writer baseline (500K tiles, commit `cb2cd29`): 164 ms

### dm6
- CPU: AMD Ryzen 5 5600G (6 cores / 12 threads, 4.46 GHz boost)
- RAM: 32 GB DDR4
- Denmark PBF baseline: ~45s total (26s pbf, 8s ocean, 0.7s sort, 5s assemble)

README.md performance numbers should always come from plantasjen (the reference host).

## Data

- `data/tilegen_tmp/` - temporary sort chunks (inside gitignored `data/`)
- Ocean shapefile not included - pass via `--ocean` flag
