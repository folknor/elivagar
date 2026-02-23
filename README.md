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

## Building

Requires Rust nightly (edition 2024) and [pbfhogg](https://github.com/folknor/pbfhogg) as a sibling directory.

```
cargo build --release
```

## License

Apache-2.0
