# Elivagar

Shortbread vector tile generator. Reads OSM PBF files and produces
[PMTiles v3](https://github.com/protomaps/PMTiles) archives with the
[Shortbread](https://shortbread-tiles.org/) schema (26 layers).

Named after the rivers of Niflheim.

## Usage

```
elivagar <input.osm.pbf> <output.pmtiles> [options]
```

### Options

| Flag | Description |
|------|-------------|
| `--ocean path.shp` | Ocean shapefile for water polygons |
| `--tmp-dir path` | Directory for temporary sort files (default: `.tilegen_tmp`) |
| `--skip-to ocean\|sort` | Resume from a previous run's checkpoint |
| `--in-memory` | Keep tile blob in RAM instead of streaming to disk |

### Example

```
elivagar denmark-latest.osm.pbf denmark.pmtiles --ocean water-polygons-split-4326/water_polygons.shp
```

## Pipeline

1. **PBF read** -- single-pass read building node/way indices and emitting sort records
2. **Ocean** -- ocean shapefile processing (optional, requires `--ocean`)
3. **Sort** -- external merge sort by Hilbert tile ID
4. **Assembly** -- MVT encode + gzip + PMTiles write

## Performance

Denmark extract (483 MB PBF) → Shortbread PMTiles, best of 3 runs:

<!-- BENCH:START -->
| Tool | Total | PBF+Features | Ocean | Sort | Assembly |
|------|-------|-------------|-------|------|----------|
| **elivagar** | **29s** | 19s | 4s | 0.3s | 5s |
| Planetiler 0.10 | 44s | — | — | — | — |
<!-- BENCH:END -->

System: Linux 6.18, Ryzen 9 7950X.

Measured with `scripts/bench.sh`. Results are logged to `benchmarks.tsv` for tracking over time.

## Building

Requires Rust nightly (edition 2024) and [pbfhogg](https://github.com/folknor/pbfhogg) as a sibling directory.

```
cargo build --release
```

## License

Apache-2.0
