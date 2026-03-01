# Elivagar

Shortbread vector tile generator. Reads OSM PBF files and produces
[PMTiles v3](https://github.com/protomaps/PMTiles) archives with the
[Shortbread](https://shortbread-tiles.org/) schema (26 layers).

## Usage

```
elivagar <input.osm.pbf> <output.pmtiles> [options]
```

### Options

| Flag | Description |
|------|-------------|
| `--ocean path.shp` | Ocean shapefile (`water-polygons-split-3857`) |
| `--ocean-simplified path.shp` | Simplified ocean shapefile for z0-7 (fewer vertices) |
| `--tmp-dir path` | Directory for temporary sort files (default: `data/tilegen_tmp`) |
| `--skip-to ocean\|sort` | Resume from a previous run's checkpoint |
| `--in-memory` | Keep tile blob in RAM instead of streaming to disk |
| `--compression-level 0-10` | Gzip compression level (default: 6). Lower = faster, larger output |
| `--force-sorted` | Use compact in-RAM node store even if PBF header lacks `Sort.Type_then_ID` |
| `--sort-budget size` | Sort chunk memory budget (default: 1G, min: 64M). Accepts `256M`, `1G`, or raw bytes |
| `--way-budget size` | In-flight way processing budget (default: 128M, min: 1M) |
| `--rel-budget size` | Relation batch accumulation budget (default: 64M, min: 1M) |
| `--assemble-budget size` | Tile assembly batch budget (default: 32M, min: 1M) |
| `-j N` / `--threads N` | Thread count (default: logical CPUs) |

### Environment variables

| Variable | Description |
|----------|-------------|
| `ELIVAGAR_NODE_STATS=1` | Print detailed SortedNodeStore diagnostics (chunk count, compression ratio, blob bytes). Requires a full scan of the node store during the PBF phase — fast on regional extracts, slow at planet scale. Basic stats (`node_store_nodes`, `node_store_groups`) are always emitted after timing, without this variable. |

### Example

```
elivagar denmark-latest.osm.pbf denmark.pmtiles \
  --ocean water-polygons-split-3857/water_polygons.shp \
  --ocean-simplified simplified-water-polygons-split-3857/simplified_water_polygons.shp
```

## Pipeline

1. **PBF read** -- single-pass read building node/way indices and emitting sort records.
   If the PBF declares `Sort.Type_then_ID` (all major producers do), nodes are stored in a
   compact in-RAM index with FOR compression (~420 MB for Denmark, 75% of raw for large
   extracts). Unsorted PBFs fall back to a flat mmap file.
2. **Ocean** -- ocean shapefile processing (optional, requires `--ocean`)
3. **Sort** -- external merge sort by Hilbert tile ID
4. **Assembly** -- MVT encode + gzip + PMTiles write

`--skip-to ocean` reuses PBF chunks from a previous full run.
`--skip-to sort` reuses all chunks (PBF + ocean).

## Output size

Denmark extract (483 MB PBF), gzip level 6, z0-14 (plantasjen, commit `175435c`):

| | elivagar | Planetiler | Tilemaker |
|---|---|---|---|
| With ocean | **288 MB** | 406 MB | 308 MB |
| Without ocean | **317 MB** | 406 MB | 308 MB |

Full analysis: [`notes/tile-comparison-2026-02-24.md`](notes/tile-comparison-2026-02-24.md)

## Performance

Denmark extract (483 MB PBF) → Shortbread PMTiles, best of 3 runs:

<!-- BENCH:START -->
| Tool | Total | PBF+Features | Ocean | Sort | Assembly |
|------|-------|-------------|-------|------|----------|
| **elivagar** | **12s** | 8s | 1.5s | 0.6s | 2s |
| Tilemaker | 30s | — | — | — | — |
| Planetiler 0.10 | 41s | — | — | — | — |
<!-- BENCH:END -->

System: plantasjen (Ryzen 9 5900X, Linux 6.18). Commit: `cb2cd29`.

Measured with `brokkr bench self`. Results stored in `.brokkr/results.db`.

### PMTiles writer

Elivagar's hand-rolled PMTiles v3 writer vs [pmtiles-rs](https://github.com/stadiamaps/pmtiles-rs),
synthetic tiles (unique gzipped payloads, Hilbert-ordered), best of 5 runs
(plantasjen, commit `cb2cd29`):

| Tiles | elivagar | pmtiles-rs | Speedup |
|------:|---------:|-----------:|--------:|
| 100K | 34 ms | 74 ms | 2.2x |
| 500K | 164 ms | 356 ms | 2.2x |
| 1M | 303 ms | 686 ms | 2.3x |

Run with `brokkr bench pmtiles [--tiles N] [--runs N]`.

## Building

Requires Rust nightly (edition 2024) and [pbfhogg](https://github.com/folknor/pbfhogg) as a sibling directory.

```
cargo build --release
```

## License

Apache-2.0
