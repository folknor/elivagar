*Note: Script references below predate the dev tool. Use dev for current equivalents.*

# Guide

Elivagar is a fast Shortbread vector tile generator. It reads OpenStreetMap PBF
files and produces [PMTiles v3](https://github.com/protomaps/PMTiles) archives
with the [Shortbread](https://shortbread-tiles.org/) schema (26 layers).

This guide covers installation, basic usage, CLI options, ocean shapefiles, and
the checkpoint/resume workflow.

## Prerequisites

### Rust nightly toolchain

Elivagar uses Rust edition 2024, which requires a nightly compiler. Install it
with [rustup](https://rustup.rs/):

```sh
rustup install nightly
rustup default nightly
```

Verify your toolchain:

```sh
rustc --version
# Should show "nightly" in the version string
```

### pbfhogg (PBF reader)

Elivagar depends on [pbfhogg](https://github.com/folknor/pbfhogg), a fast OSM
PBF reader. It must be cloned as a **sibling directory** — that is, next to the
elivagar directory, not inside it:

```
parent/
  elivagar/      # this project
  pbfhogg/       # PBF reader dependency
```

Clone it:

```sh
cd /path/to/parent
git clone https://github.com/folknor/pbfhogg.git
```

### System libraries

The build uses `zlib-ng` (via the `flate2` crate with the `zlib-ng` feature) for
gzip compression. On most Linux distributions this builds from source via the
`cmake` crate, so you need a C compiler and CMake:

```sh
# Debian/Ubuntu
sudo apt install build-essential cmake

# Fedora
sudo dnf install gcc cmake
```

### OpenStreetMap PBF extract

Download a PBF extract from [Geofabrik](https://download.geofabrik.de/). The
Denmark extract (483 MB) is the standard test dataset:

```sh
mkdir -p data
wget -O data/denmark-latest.osm.pbf \
  https://download.geofabrik.de/europe/denmark-latest.osm.pbf
```

## Quick Start

Build the release binary:

```sh
cargo build --release
```

Generate tiles from a PBF file:

```sh
./target/release/elivagar data/denmark-latest.osm.pbf output.pmtiles
```

That produces a PMTiles archive at zoom levels 0 through 14, without ocean water
tiles. To include ocean coverage (recommended), see the
[Ocean Shapefiles](#ocean-shapefiles) section below.

A full run with ocean shapefiles looks like this:

```sh
./target/release/elivagar denmark-latest.osm.pbf denmark.pmtiles \
  --ocean water-polygons-split-3857/water_polygons.shp \
  --ocean-simplified simplified-water-polygons-split-3857/simplified_water_polygons.shp
```

### Using the run script

The project includes a convenience script that builds and runs in one step. It
auto-detects ocean shapefiles in `data/`:

```sh
scripts/run.sh data/denmark-latest.osm.pbf data/denmark.pmtiles
```

## CLI Reference

```
elivagar <input.osm.pbf> <output.pmtiles> [options]
```

### Positional arguments

| Argument | Description |
|----------|-------------|
| `input.osm.pbf` | Path to the input OpenStreetMap PBF file |
| `output.pmtiles` | Path for the output PMTiles v3 archive |

### Options

| Flag | Value | Description |
|------|-------|-------------|
| `--ocean` | `path.shp` | Path to the full-resolution ocean polygon shapefile (`water-polygons-split-3857`). Used for z8-14 when combined with `--ocean-simplified`, or all zoom levels if used alone. |
| `--ocean-simplified` | `path.shp` | Path to the simplified ocean polygon shapefile (`simplified-water-polygons-split-3857`). Used for z0-7 to reduce vertex count at low zoom levels. Requires `--ocean` to also be set. |
| `--tmp-dir` | `path` | Directory for temporary sort chunks and intermediate files. Created automatically if it does not exist. Default: `data/tilegen_tmp` |
| `--skip-to` | `ocean` or `sort` | Resume from a previous run's checkpoint, skipping earlier pipeline phases. See [Checkpointing with --skip-to](#checkpointing-with---skip-to). |
| `--in-memory` | *(flag)* | Keep the tile data blob in RAM instead of streaming to a temporary file on disk. Faster for small extracts, but uses significantly more memory at planet scale. |
| `--compression-level` | `0-10` | Gzip compression level. Lower values are faster but produce larger output. Default: 6. Level 3-4 is a good tradeoff for faster builds with ~5% larger output. |

### Output

Elivagar generates tiles at zoom levels 0 through 14. The zoom range is
currently fixed in the pipeline and not configurable via CLI flags.

The output is a PMTiles v3 archive with Hilbert-ordered tile IDs and gzip
compression (default level 6, configurable via `--compression-level`).
All 26 Shortbread layers are included.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Error (invalid arguments, I/O failure, corrupt input) |

## Pipeline Overview

Elivagar processes data in four sequential phases:

```
PBF read  ──>  Ocean  ──>  Sort  ──>  Assembly
 (phase12)                                │
                                          ▼
                                     output.pmtiles
```

1. **PBF read** (phase 1+2) -- Single-pass read of the PBF file. Builds
   memory-mapped node and way coordinate indices, matches OSM elements against
   the Shortbread tag profile, projects coordinates to Web Mercator, simplifies
   geometries per zoom level, and emits binary sort records to chunk files on
   disk.

2. **Ocean** -- Reads the ocean polygon shapefile (if `--ocean` is provided),
   clips polygons to tile boundaries at each zoom level, and emits additional
   sort records for water fill tiles. Uses scanline fill to determine which tiles
   need ocean polygons.

3. **Sort** -- External merge sort of all chunk files by Hilbert tile ID. Uses a
   k-way merge with a binary heap. This groups all features for each tile
   together, which is required for the assembly phase.

4. **Assembly** -- Reads sorted records, encodes MVT protobuf tiles, gzip
   compresses them, and writes the final PMTiles v3 archive with Hilbert tile
   IDs and content deduplication.

Temporary sort chunks are stored in the `--tmp-dir` directory (default:
`data/tilegen_tmp`). These files can be large -- roughly 2-4x the input PBF size.

## Ocean Shapefiles

Ocean shapefiles provide water polygon coverage for coastal and oceanic areas.
Without them, the output tiles will not contain `ocean` layer features, and
coastal areas will lack water fill.

### What they are

The [OpenStreetMap Data](https://osmdata.openstreetmap.de/data/water-polygons.html)
project publishes pre-processed ocean polygon shapefiles derived from OSM
coastline data. These are ESRI Shapefiles in Web Mercator projection (EPSG:3857),
split into manageable polygon pieces.

### The two variants

There are two variants of the ocean shapefile, and elivagar can use both
simultaneously for best results:

| Variant | Directory name | Flag | Zoom levels | Description |
|---------|---------------|------|-------------|-------------|
| Full resolution | `water-polygons-split-3857` | `--ocean` | z8-14 (or z0-14 if used alone) | High-detail polygons for medium and high zoom levels |
| Simplified | `simplified-water-polygons-split-3857` | `--ocean-simplified` | z0-7 | Fewer vertices, significantly faster at low zoom levels where full detail is invisible |

When both are provided, the simplified variant handles z0-7 and the full
resolution variant handles z8-14. When only `--ocean` is provided, the full
resolution shapefile is used for all zoom levels.

### Downloading

Download both shapefiles from the OpenStreetMap Data site:

```sh
mkdir -p data
cd data

# Full resolution (~ 800 MB compressed)
wget https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip
unzip water-polygons-split-3857.zip

# Simplified (~ 12 MB compressed)
wget https://osmdata.openstreetmap.de/download/simplified-water-polygons-split-3857.zip
unzip simplified-water-polygons-split-3857.zip
```

After extracting, the directory structure should look like:

```
data/
  water-polygons-split-3857/
    water_polygons.shp
    water_polygons.shx
    water_polygons.dbf
    water_polygons.prj
  simplified-water-polygons-split-3857/
    simplified_water_polygons.shp
    simplified_water_polygons.shx
    simplified_water_polygons.dbf
    simplified_water_polygons.prj
```

The `.shp` file is what you pass to elivagar. The `.shx` index file must be
present alongside it (elivagar uses it for fast record lookup via mmap).

### Usage with ocean shapefiles

```sh
./target/release/elivagar denmark-latest.osm.pbf denmark.pmtiles \
  --ocean data/water-polygons-split-3857/water_polygons.shp \
  --ocean-simplified data/simplified-water-polygons-split-3857/simplified_water_polygons.shp
```

### Output size impact

Ocean shapefiles add water coverage tiles to the output. For the Denmark extract:

| Configuration | Output size |
|---------------|-------------|
| Without ocean | 317 MB |
| With ocean | 380 MB |

The size increase depends on how much coastline the extract contains.

## Checkpointing with `--skip-to`

The `--skip-to` flag lets you resume from a previous run's intermediate data,
skipping expensive earlier pipeline phases. This is useful when iterating on
the tile assembly or ocean processing without re-reading the entire PBF file
each time.

### How it works

Each full run saves checkpoint data in the `--tmp-dir` directory:

- **Sort chunk files** from the PBF read phase
- **Sort chunk files** from the ocean phase
- A **checkpoint file** recording how many PBF chunks were produced
- A **land mask** used for ocean tile filtering

The `--skip-to` flag tells elivagar to skip phases and reuse this saved data.

### Skip levels

| Value | Skips | Reuses | Re-runs |
|-------|-------|--------|---------|
| `--skip-to ocean` | PBF read | PBF sort chunks | Ocean + Sort + Assembly |
| `--skip-to sort` | PBF read + Ocean | All sort chunks | Sort + Assembly |

### Workflow example

First, do a full run to generate all checkpoint data:

```sh
./target/release/elivagar denmark-latest.osm.pbf denmark.pmtiles \
  --ocean data/water-polygons-split-3857/water_polygons.shp \
  --ocean-simplified data/simplified-water-polygons-split-3857/simplified_water_polygons.shp
```

Then iterate on the sort and assembly phases without re-reading the PBF:

```sh
# Re-run ocean processing + sort + assembly (skip PBF read)
./target/release/elivagar denmark-latest.osm.pbf denmark.pmtiles \
  --ocean data/water-polygons-split-3857/water_polygons.shp \
  --skip-to ocean

# Re-run only sort + assembly (skip PBF read and ocean)
./target/release/elivagar denmark-latest.osm.pbf denmark.pmtiles \
  --skip-to sort
```

### Requirements

- The `--tmp-dir` must contain valid checkpoint data from a previous run with the
  **same PBF file**. Using checkpoint data from a different PBF file will produce
  incorrect output.
- The tmp directory must not have been cleaned or moved between runs.
- For `--skip-to ocean`, the PBF path is still required as a positional argument
  (it appears in log output) but the file itself is not read.

### When to use each level

| Scenario | Recommended skip |
|----------|-----------------|
| Iterating on ocean shapefile processing | `--skip-to ocean` |
| Iterating on tile assembly or encoding | `--skip-to sort` |
| Benchmarking sort + assembly only | `--skip-to sort` |
| Changed the PBF extract | No skip (full run) |
| Changed ocean shapefiles | `--skip-to ocean` |

## Temporary files

Elivagar writes intermediate sort chunk files to the `--tmp-dir` directory
(default: `data/tilegen_tmp`). These can be large:

| Dataset | PBF size | Approximate tmp size |
|---------|----------|---------------------|
| Denmark | 483 MB | [TODO: exact size] |
| Planet | ~70 GB | [TODO: exact size] |

The tmp directory is cleaned at the start of each full run (without `--skip-to`).
When using `--skip-to`, the existing contents are preserved and reused.

The directory is **not** automatically cleaned after a successful run, because
it may be needed for subsequent `--skip-to` runs. To reclaim disk space after
you are done iterating:

```sh
rm -rf data/tilegen_tmp
```

## Memory usage

Elivagar uses memory-mapped files for its node and way coordinate indices. The
node index is a sparse file that can be very large (up to ~110 GB for a planet
extract) but only pages in the portions that are actually accessed.

| Dataset | Node index size | RAM needed for good performance |
|---------|----------------|--------------------------------|
| Denmark (483 MB PBF) | ~16 GB sparse | 32 GB |
| Planet (~70 GB PBF) | ~110 GB sparse | 64+ GB |

When the node index exceeds available RAM, performance becomes I/O-dominated
as the kernel serves page faults from disk. An NVMe SSD is strongly recommended.

The `--in-memory` flag keeps the output tile blob in RAM instead of streaming it
to a temporary file. This is faster for small extracts but increases peak memory
usage. For planet-scale runs, leave it off.

## Performance

For reference, Denmark extract times on a Ryzen 9 7950X with 64 GB RAM and
NVMe storage:

| Phase | Time |
|-------|------|
| **Total** | **29s** |
| PBF read | 20s |
| Ocean | 2.8s |
| Sort | 0.7s |
| Assembly | 3.4s |

The PBF read phase dominates wall time. Sort and assembly are fast.

## License

Apache-2.0
