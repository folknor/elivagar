# Elivagar

Shortbread vector tile generator. Reads OSM PBF files and produces PMTiles v3 archives with 26 layers.

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never chain commands with ;. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use `brokkr`.
- **Never run the full pipeline on real PBF data (brokkr bench self, brokkr run) unless the user explicitly asks.** Use synthetic benchmarks (brokkr bench node-store, brokkr bench pmtiles) for iteration. Full pipeline runs are expensive and should only happen when the user decides it's time.

## Brokkr tool

Standalone development tool at `~/Programs/brokkr`. Installed via `cargo install --path ~/Programs/brokkr`. Invoked as `brokkr` from the project root (reads `./brokkr.toml` for project detection).

- `brokkr check [-- args]` — run clippy + tests
- `brokkr env` — show environment info
- `brokkr run [args]` — build release and run with auto-injected flags: `--tmp-dir` (from scratch_dir config), `--ocean`/`--ocean-simplified` (auto-detected from data_dir), `HOTPATH_METRICS_SERVER_OFF=true` env var. Use `--no-ocean` to suppress ocean injection. Use `--mem 8G` to wrap with `systemd-run --scope -p MemoryMax=8G` for OOM protection on large datasets
- `brokkr bench self [--dataset name] [--pbf path] [--runs N] [--skip-to ocean|sort] [--no-ocean] [--compression-level N]` — full pipeline benchmark
- `brokkr bench planetiler [--dataset name] [--pbf path] [--runs N]` — Planetiler comparison benchmark
- `brokkr bench tilemaker [--dataset name] [--pbf path] [--runs N]` — Tilemaker comparison benchmark (stub)
- `brokkr bench node-store [--nodes N] [--runs N]` — SortedNodeStore benchmark (default: 50M nodes, 5 runs)
- `brokkr bench pmtiles [--tiles N] [--runs N]` — PMTiles writer benchmark (default: 500K tiles, 5 runs)
- `brokkr bench eliv-all [--dataset name] [--pbf path] [--runs N]` — full benchmark suite
- `brokkr hotpath [--dataset name] [--pbf path] [--alloc]` — hotpath profiling (timing or allocation)
- `brokkr profile [--dataset name] [--pbf path] [--tool perf|samply]` — sampling profiler (perf or samply)
- `brokkr compare-tiles <a> <b> [--sample N]` — compare feature counts between PMTiles archives
- `brokkr download ocean` — download ocean shapefiles
- `brokkr results [--commit X] [--compare A B]` — query benchmark results from SQLite
- `brokkr clean` — remove tilegen_tmp and scratch files

Benchmark results stored in `.brokkr/results.db` (SQLite, tracked in git for cross-host access).

**NEVER run two elivagar processes at the same time.** They share `data/tilegen_tmp/` (causes crashes) and hotpath uses conflicting cargo feature flags (causes build conflicts). Always run sequentially.

## Scripts

No shell scripts remain. All development tooling is in `brokkr`.

## Architecture

Single-crate library + binary. Public API is `elivagar::run(&TilegenConfig)`.

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
- `sort.rs` — external merge sort (chunk files, k-way merge via binary heap)
- `pmtiles_writer.rs` — PMTiles v3 writer with Hilbert tile IDs
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
- `libdeflater` (libdeflate) — gzip compression for MVT tiles
- `mimalloc` — global allocator (critical for rayon performance)
- `hotpath` — function profiling, feature-gated (`--features hotpath`), zero-cost when disabled

## Key conventions

- `#[global_allocator]` mimalloc in main.rs — do not remove
- `.unwrap()` forbidden by clippy — use `expect()` or propagate errors
- Cast lints are strict — annotate with `#[allow(clippy::cast_*)]` where needed
- Test fixtures live in `tests/fixtures/` (YAML files for Shortbread spec)
- `ELIVAGAR_NODE_STATS=1` — enables detailed SortedNodeStore diagnostic scan (chunk counts, compression ratio, blob bytes). Runs during PBF phase so it adds to `phase12_ms` — safe for hotpath runs but not for bench timing. Basic stats (`node_store_nodes`, `node_store_groups`) are always emitted after all timing kv pairs and never affect benchmarks.

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
- Denmark PBF baseline: ~12.3s total (8s pbf, 1.5s ocean, 0.6s sort, 2s assemble)

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

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.
