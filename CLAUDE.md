# Elivagar

Shortbread vector tile generator. Reads OSM PBF files and produces PMTiles v3 archives with 26 layers.

## Bash rules
- Never use sed, find, awk, or complex bash commands
- Never chain commands with &&
- Never chain commands with ;
- Never pipe commands with |
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use `brokkr`.
- **Never run the full pipeline on real PBF data (brokkr tilegen, brokkr tilegen --bench) unless the user explicitly asks.** Use synthetic benchmarks (brokkr node-store, brokkr pmtiles-writer) for iteration. Full pipeline runs are expensive and should only happen when the user decides it's time.

## Brokkr tool

Standalone development tool at `~/Programs/brokkr`. Installed via `cargo install --path ~/Programs/brokkr`. Invoked as `brokkr` from the project root (reads `./brokkr.toml` for project detection).

- `brokkr check [-- args]` — run clippy + tests. Supports `--features` and `--no-default-features`
- `brokkr env` — show environment info and dataset status with computed XXH128 hashes (copy into the `xxhash` field in `brokkr.toml`)
- `brokkr results [UUID]` — look up specific result by UUID prefix (shows full detail + hotpath report)
- `brokkr results [--commit X] [--compare A B] [--compare-last] [--command CMD] [--variant V] [--top N]` — query/compare benchmark results from SQLite. Use `--top 0` to show all hotpath functions. Use `--compare-last --command hotpath` to diff two most recent hotpath runs.
- `brokkr results <UUID> --timeline [--stat FIELD] [--fields F1,F2] [--every N] [--phase P] [--where EXPR]` — query sidecar /proc samples (JSONL, stats, downsampled, per-phase, filtered)
- `brokkr results <UUID> --markers --durations` — phase duration table from markers
- `brokkr results --compare-timeline <A> <B>` — phase-aligned sidecar comparison
- `brokkr results dirty --timeline --stat anon` — inspect last failed/dirty run
- `brokkr clean` — remove tilegen_tmp and scratch files
- `brokkr history [--command CMD] [--project P] [--failed] [--since DATE] [--slow MS] [-n N] [--all]` — query global command history (stored in `$XDG_DATA_HOME/brokkr/history.db`). Every brokkr invocation is recorded with timing, exit status, project, and git context. Works from any directory.

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

# Suite
brokkr suite elivagar [--bench [N]] [--dataset D] [--variant V]
```

Pipeline flags on `tilegen` (`--tile-format`, `--tile-compression`, `--compress-sort-chunks`, `--in-memory`, `--locations-on-ways`, etc.) are passed through to the elivagar binary and stored as `meta.*` kv pairs in the results DB.

### Sidecar profiler

Every `--bench`, `--hotpath`, and `--alloc` run automatically samples `/proc/{pid}/status` and `/proc/{pid}/io` at 100ms intervals. Data stored in `.brokkr/sidecar.db` (gitignored, local-only). Preserved even if the child is OOM-killed.

Phase markers and counters via FIFO: brokkr creates a FIFO, sets `BROKKR_MARKER_FIFO` in the child's environment, spawns a sidecar thread for `/proc` sampling, reads markers/counters from the FIFO, and bulk-inserts everything into results.db after exit.

Protocol (two line formats, same FIFO):
- Markers: `{timestamp_us} {PHASE_NAME}\n`
- Counters: `{timestamp_us} @{name}={value}\n` (i64 value)

Elivagar emits markers at phase boundaries (`PHASE12_START/END`, `OCEAN_START/END`, `SORT_START/END`, `ASSEMBLE_START/END`) and counters for key metrics (`phase12_ms`, `ocean_ms`, `ocean_features`, `assemble_ms`, `tiles`, `unique_tiles`, `features`). Implementation is in `pipeline/mod.rs` via `emit_marker()`/`emit_counter()` — OnceLock fd caching, O_NONBLOCK, no-op when brokkr isn't running.

Query with:
- `brokkr results <uuid> --markers --durations` — phase timing table
- `brokkr results <uuid> --markers --counters` — counter values
- `brokkr results <uuid> --markers --phases` — phases with peak RSS + counters inline

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

- `pbf.<variant>` — PBF files keyed by variant name. `--variant` selects (default: `raw`).
- `xxhash` — XXH128 file hash. Run `brokkr env` to see computed values.

Benchmark results stored in `.brokkr/results.db` (SQLite, tracked in git for cross-host access). Bench runs record `meta.*` kv pairs (e.g. `meta.compress_sort_chunks`, `meta.tile_format`, `meta.locations_on_ways`) so runs with different flags are distinguishable. Bench and hotpath commands require a clean git tree (ignoring `*.md` and `.brokkr/results.db`); use `--force` to run anyway (results will not be stored). Example: `brokkr tilegen --bench --force --dataset denmark`.

**NEVER run two elivagar processes at the same time.** They share `data/tilegen_tmp/` (causes crashes) and hotpath uses conflicting cargo feature flags (causes build conflicts). Always run sequentially.

## Scripts

No shell scripts remain. All development tooling is in `brokkr`.

## Architecture

Single-crate library + binary. Public API is `elivagar::run(&TilegenConfig)`. CLI uses clap derive with subcommands (`run`, `inspect`, `verify`, `svg`, `diag`).

### Modules

**Pipeline orchestrator:**
- `pipeline.rs` — PBF → ocean → sort → assemble → PMTiles

**Shortbread profile:**
- `shortbread/` — tag matching, layer definitions, 26 layers (mod.rs, boundaries.rs, land.rs, streets.rs, transport.rs, water.rs)
- `shortbread_tests.rs` — spec test cases (65+), loaded via `#[path]` from shortbread/mod.rs
- `pois.rs` — POI tag matching
- `wire_format.rs` — sort record binary serialization

**Geometry + encoding:**
- `geometry.rs` — Mercator projection, clipping (Sutherland-Hodgman), Douglas-Peucker simplification
- `mvt.rs` — MVT protobuf encoder
- `multipolygon.rs` — relation ring assembly
- `ocean.rs` — ocean shapefile processing (mmap reader + scanline fill)

**Infrastructure:**
- `sort.rs` — external merge sort (gzip-compressed chunk files, k-way merge via binary heap)
- `pmtiles_writer.rs` — PMTiles v3 writer with Hilbert tile IDs
- `inspect.rs` — PMTiles v3 archive inspector (header + metadata reader)
- `svg.rs` — single-tile SVG renderer (decodes MVT geometry from PMTiles, outputs SVG)
- `node_index.rs` — node coordinate index (SortedNodeStore for sorted PBFs, flat mmap fallback)
- `way_index.rs` — flat mmap'd way geometry index

### Pipeline phases

Sequential, same PBF input:
- **phase12**: PBF read + OSM feature emission → sort chunks + checkpoint
- **ocean**: shapefile read + ocean feature emission → more sort chunks
- **sort**: external merge sort all chunks
- **assemble**: MVT encode + gzip + PMTiles write

`--skip-to ocean` reuses PBF chunks from a previous full run.
`--skip-to sort` reuses all chunks (PBF + ocean).

## Dependencies

- `pbfhogg` — PBF reader, sibling dir `../pbfhogg`
- `rayon` — parallel processing
- `memmap2` — memory-mapped I/O for way index (and flat node index fallback)
- `flate2` (zlib-rs backend) — gzip compression for MVT tiles and sort chunks. Same zlib backend as pbfhogg.
- `brotli` — brotli compression for MVT tiles (optional via `--tile-compression brotli`)
- `mimalloc` — global allocator (critical for rayon performance)
- `clap` (derive) — CLI argument parsing with subcommands
- `hotpath` — function profiling, feature-gated (`--features hotpath`), zero-cost when disabled

## CLI

Uses clap derive with subcommands:

### `elivagar run <INPUT> -o <OUTPUT> [flags]`

- `-o` / `--output` — output PMTiles path (required)
- `--tmp-dir path` — temporary directory for sort chunks (default: `data/tilegen_tmp`)
- `--ocean path.shp` — ocean polygon shapefile (water-polygons-split-3857). Auto-detected from `data/` when omitted.
- `--ocean-simplified path.shp` — simplified ocean shapefile for z0-7. Auto-detected from `data/` when omitted.
- `--no-ocean` — disable ocean shapefile processing (skip auto-detection)
- `--skip-to ocean|sort` — resume from checkpoint
- `--in-memory` — keep tile blob in RAM (faster for small extracts)
- `--compression-level 0-10` — compression level (default 6)
- `--tile-compression gzip|brotli` — tile compression algorithm (default gzip)
- `--force-sorted` — force compact node store even without PBF header flag
- `--locations-on-ways` — PBF has node coordinates embedded in ways
- `-j N` / `--threads N` — thread count (default: logical CPUs)
- `--sort-budget <size>` — sort chunk memory budget (default 1G). Accepts `256M`, `512M`, `1G`, or raw bytes. Minimum 64M. Lower values reduce peak RSS during PBF processing at the cost of more merge chunks.
- `--way-budget <size>` — in-flight way processing budget (default 128M standard, 256M in `--locations-on-ways` mode). Minimum 1M.
- `--rel-budget <size>` — relation batch accumulation budget (default 64M). Minimum 1M.
- `--assemble-budget <size>` — tile assembly batch budget (default 32M). Minimum 1M.
- `--fanout-cap-default N` — default fanout cap for all polygon layers (0 = uncapped). Per-layer overrides take precedence.
- `--fanout-cap layer=N,...` — per-layer fanout caps (e.g. `water_polygons=2048,boundaries=4096`). Features whose bbox tile count exceeds the cap are skipped at that zoom. Comma-separated, strict layer name validation.

### `elivagar inspect <FILE>`

Reads a PMTiles archive and prints header info, tile statistics, section layout, and metadata (layer list with zoom ranges).

### `elivagar verify <FILE>`

Validates a PMTiles archive end-to-end: container integrity, metadata schema, tile decompression, MVT payload structure, geometry command validation, and layer coverage. Also checks ocean polygon rings for self-intersections. Exits 0 on pass, 1 on failure. Stops after 100 tile-level errors.

### `elivagar svg <FILE> -z <Z> -x <X> -y <Y> [-W width] [-H height] [-l layers] [-o output.svg]`

Renders tiles from a PMTiles archive as SVG. Supports single tiles or NxM grids (`-W`/`-H`, default 1x1). `--layers` filters to specific layers (comma-separated, e.g. `ocean,boundaries`). Decodes MVT geometry and draws each layer with a distinct color. Points render as circles, lines as stroked paths, polygons as filled paths with `evenodd` fill-rule. Background is land-colored (`#f2efe9`). Grid lines drawn between tiles when width or height > 1. Output goes to stdout by default, or to a file with `-o`.

### `elivagar diag <FILE> -z <Z> -x <X> -y <Y>`

Diagnoses ocean polygon ring winding for a specific tile. Decodes MVT protobuf, finds polygon features across all layers, and prints per-ring vertex count, signed area, and winding direction (CW = outer, CCW = hole). Prints first/last 3 vertices for large rings, full vertices for small ones (≤6).

## Key conventions

- `#[global_allocator]` mimalloc in main.rs — do not remove
- `.unwrap()` forbidden by clippy — use `expect()` or propagate errors
- Cast lints are strict — annotate with `#[allow(clippy::cast_*)]` where needed
- Test fixtures live in `tests/fixtures/` (YAML files for Shortbread spec)
- **Test geometry must fit in one tile at the test zoom level.** World-spanning polygons (e.g. [0.1–0.9] Mercator) at z14 iterate 268M tiles and OOM the machine. If a test needs high zoom, use geometry confined to a single tile at that zoom.
- `ELIVAGAR_NODE_STATS=1` — enables detailed SortedNodeStore diagnostic scan (chunk counts, compression ratio, blob bytes). Runs during PBF phase so it adds to `phase12_ms` — safe for hotpath runs but not for bench timing. Basic stats (`node_store_nodes`, `node_store_groups`) are always emitted after all timing kv pairs and never affect benchmarks.
- Memory instrumentation (`3a729ab`) — always-on, not feature-gated. Emits per-phase peak RSS (`phase12_rss_kb`, `ocean_rss_kb`, `sort_rss_kb`, `assemble_rss_kb`), `sort_chunks`, and in-flight HWM counters (`max_way_inflight_bytes`, `max_rel_batch_bytes`, `max_assemble_batch_bytes`). Overhead is negligible: 4 `/proc` reads total, per-block byte estimation, per-feature counter increment. Nothing in hot inner loops.

## Benchmark discipline

All performance numbers (wall time, phase splits, allocation profiles) MUST include:
1. **Host name** (plantasjen, dm6, etc.)
2. **Git commit hash** of the code that was measured

Workflow: commit code first, THEN benchmark, THEN update docs with the commit hash.
Never write benchmark numbers for uncommitted code — the hash is the anchor.

## Benchmark machines

### plantasjen (current)
- CPU: AMD Ryzen 9 5900X (12 cores / 24 threads, 4.95 GHz boost)
- RAM: 30 GB DDR4
- Denmark PBF baseline: ~12.4s total (8s pbf, 1.5s ocean, 0.5s sort, 2.3s assemble), 1.8 GB RSS
- North America baseline (commit `8704b11`): 605s total (413s pbf, 37s ocean, 0.7s sort, 155s assemble), 22.8 GB RSS, 12.5 GB output
- Node store baseline (50M nodes, commit `cb2cd29`): build 1.7s, way-like 77 ns/lookup, random 394 ns/lookup
- PMTiles writer baseline (500K tiles, commit `cb2cd29`): 164 ms

### dm6
- CPU: AMD Ryzen 5 5600G (6 cores / 12 threads, 4.46 GHz boost)
- RAM: 32 GB DDR4
- Denmark PBF baseline: ~45s total (26s pbf, 8s ocean, 0.7s sort, 5s assemble)

README.md performance numbers should always come from plantasjen (the reference host).

## madvise / fadvise — do not add

All madvise hints (MADV_SEQUENTIAL, MADV_RANDOM, MADV_HUGEPAGE, MADV_POPULATE_READ) and
posix_fadvise hints (FADV_SEQUENTIAL, FADV_DONTNEED) were tried and removed. Every hint
caused regressions because the node index file is sparse — node IDs go up to ~12B regardless
of dataset size, so a Denmark extract produces a ~96 GB file with only ~400 MB populated.

The regression was severe: 45s → 160s on dm6, a 3.5x slowdown. It was bisected to commit
cdc382a ("Implement Tier 1 Linux I/O hints"). The MADV_HUGEPAGE hint was the primary culprit —
the kernel tries to assemble 2 MB huge pages from a file that is 99%+ holes, causing massive
page fault overhead. A "50% of RAM" threshold guard was in place but used file_len (~96 GB)
instead of the actual working set (~400 MB), so the guard never triggered correctly.

After removing ALL hints, performance returned to baseline. Kernel defaults (MADV_NORMAL,
no fadvise) work best for this access pattern. Do not re-add madvise or fadvise calls
without benchmarking on a sparse-file workload first.

## Data

- `data/tilegen_tmp/` — temporary sort chunks (inside gitignored `data/`)
- Ocean shapefile not included — pass via `--ocean` flag

## Data preparation (pbfhogg commands)

Elivagar reads PBF files produced by pbfhogg. The production pipeline has three stages, each run from the **pbfhogg** project root via `brokkr`:

### 1. Generate indexed PBF (cat)

`cat` embeds blob-level indexdata automatically when writing. Steps 2 and 3 are much faster with indexed PBFs. The passthrough path (no `--type`) adds indexdata without re-compressing blobs — use this for planet-scale files.

```
brokkr cat raw.osm.pbf -o indexed.osm.pbf
```

With `--type` for filtered output (full decode + re-encode, higher memory):

```
brokkr cat raw.osm.pbf --type node,way,relation -o indexed.osm.pbf
```

### 2. Apply OSC diffs (apply-changes)

Merge an OSC changeset into the indexed PBF. Uses indexdata for fast blob-level passthrough (~92% of blobs pass through as raw bytes at Denmark scale).

```
brokkr apply-changes indexed.osm.pbf changes.osc.gz -o merged.osm.pbf
```

With `--locations-on-ways`, apply-changes preserves and updates inline way-node coordinates through diffs. This eliminates the need to re-run step 3 after each merge — only needed once for bootstrapping.

```
brokkr apply-changes indexed.osm.pbf changes.osc.gz -o merged.osm.pbf --locations-on-ways
```

### 3. Generate locations PBF (add-locations-to-ways)

Embed resolved node coordinates into ways. This is the PBF variant elivagar's tile pipeline reads — ways arrive with geometry already resolved via `Way::node_locations()`, avoiding a separate node lookup pass.

```
brokkr add-locations-to-ways merged.osm.pbf -o locations.osm.pbf
```

Options:
- `--keep-untagged-nodes` — retain untagged nodes in output
- `--index-type dense` (default) — file-backed mmap, fastest when working set fits in RAM
- `--index-type external` — bounded-memory double radix join, all sequential I/O. Best for memory-constrained hosts. Planet (87 GB): 24 min, 17 GB peak RAM on a 30 GB host. Requires sorted PBF input and ~300 GB temp disk at planet scale.

### Notes

Steps 2 and 3 (and `sort`) expect indexed PBFs by default and will error if indexdata is missing. Use `--force` to override the check and run with raw PBFs (slower).

For steady-state operation, use `apply-changes --locations-on-ways` (step 2) instead of running steps 2 and 3 separately. Step 3 is only needed once to bootstrap the initial enriched PBF.

## Review tool

`review` fans out code review queries to persistent AI sessions (Claude Code + Codex), each primed as a competitor project developer. Configured in `.review.toml`.

Three competitor archetypes, grouped as `competitors`:
- `planetiler` — Java reference implementation. Source at `research/planetiler/`.
- `tilemaker` — C++ Shortbread generator. Source at `research/tilemaker/`.
- `tippecanoe` — Felt's tile tool. Source at `research/tippecanoe/`.

Each archetype has a Claude session and a Codex session (6 total). The sessions are primed with the role "you are a [project] developer we've hired to help" and have access to both the competitor source and elivagar source.

Usage:
- `echo "question" | review competitors` — ask all 6 sessions
- `echo "question" | review planetiler` — ask planetiler sessions about staged changes
- `echo "question" | review competitors --dry-run` — preview prompts without sending

When using `--anchor`, the global prefix does not reinforce each session's identity. Include a short reminder in the question text itself, e.g. "As a Planetiler/Tilemaker/Tippecanoe developer, how would you..." — or skip `--anchor` and rely on the sessions' initial priming.

**Use this tool before implementing geometry/rendering changes.** Write up the problem, send to competitors, wait for answers. The write-up + review cycle is faster than implement + build + discover it's wrong.

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.
