# Elivagar

Shortbread vector tile generator. Reads OSM PBF files and produces PMTiles v3 archives with 26 layers.

## Bash rules
- Never use sed, find, awk, or complex bash commands. Write a script instead.
- Never chain commands with &&. Write a script instead.
- Never pipe commands with |. Write a script instead.
- Never read or write from /tmp. All data lives in the project.
- Never run raw cargo, curl, pkill. Use the scripts below.

## Scripts

Write new scripts in `scripts/` as needed. Follow these conventions:
- `scripts/build.sh` — build release
- `scripts/test.sh` — run tests
- `scripts/bench-self.sh [pbf] [runs] [--skip-to ocean|sort]` — quick self-benchmark (no comparisons)
- `scripts/bench.sh [pbf] [--skip-to ocean|sort]` — benchmark elivagar vs Planetiler vs Tilemaker
- `scripts/bench-tilemaker.sh [pbf] [runs]` — benchmark Tilemaker Shortbread (auto-builds from source, downloads shapefiles)
- `scripts/run.sh [pbf] [out.pmtiles]` — build + run
- `scripts/bench-pmtiles.sh [tiles] [runs]` — benchmark PMTiles writer vs pmtiles-rs (default: 500K tiles, 5 runs)

If you need something these scripts don't cover, write a new script.

## Architecture

Single-crate library + binary. Public API is `elivagar::run(&TilegenConfig)`.

### Modules

**Pipeline orchestrator:**
- `pipeline.rs` — PBF → ocean → sort → assemble → PMTiles

**Shortbread profile:**
- `shortbread.rs` — tag matching, layer definitions, 26 layers
- `shortbread_tests.rs` — spec test cases (65+), loaded via `#[path]` from shortbread.rs
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
- `node_index.rs` — flat mmap'd node coordinate index
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
- `memmap2` — memory-mapped I/O for node/way indices
- `flate2` (zlib-ng) — gzip compression for MVT tiles
- `mimalloc` — global allocator (critical for rayon performance)

## Key conventions

- `#[global_allocator]` mimalloc in main.rs — do not remove
- `.unwrap()` forbidden by clippy — use `expect()` or propagate errors
- Cast lints are strict — annotate with `#[allow(clippy::cast_*)]` where needed
- Test fixtures live in `tests/fixtures/` (YAML files for Shortbread spec)

## Data

- `.tilegen_tmp/` — temporary sort chunks (gitignored)
- Ocean shapefile not included — pass via `--ocean` flag

## Subagents
Subagents must NOT run any shell commands. They write code only. Integration, building, and testing is done in the main conversation.
