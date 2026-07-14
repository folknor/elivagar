# Elivagar

Shortbread vector tile generator. Reads OSM PBF files and produces
[PMTiles v3](https://github.com/protomaps/PMTiles) archives with the
[Shortbread](https://shortbread-tiles.org/) schema (26 layers).

Built with LLMs. See [LLM.md](LLM.md).


## Usage

### Generate tiles

```
elivagar run <input.osm.pbf> -o <output.pmtiles> [options]
```

| Flag | Description |
|------|-------------|
| `-o path` / `--output path` | Output PMTiles path (required) |
| `--ocean SPEC` | Ocean input, repeatable. Omit entirely for no ocean. See below |
| `--tmp-dir path` | Directory for temporary sort files (default: `data/tilegen_tmp`) |
| `--skip-to ocean\|sort\|assemble` | Resume from a previous run's checkpoint |
| `--in-memory` | Keep tile blob in RAM instead of streaming to disk |
| `--compression-level 0-10` | Gzip compression level (default: 6). Lower = faster, larger output |
| `--force-sorted` | Use compact in-RAM node store even if PBF header lacks `Sort.Type_then_ID` |
| `--allow-unsafe-flat-index` | Bypass flat-index safety guardrails (unsafe; may cause severe IO/RSS degradation) |
| `--locations-on-ways` | PBF has node coordinates embedded in ways |
| `--sort-budget size` | Sort chunk memory budget (default: 1G, min: 64M). Accepts `256M`, `1G`, or raw bytes |
| `--way-budget size` | In-flight way processing budget (default: 128M standard / 256M with `--locations-on-ways`, min: 1M) |
| `--assemble-budget size` | Tile assembly batch budget (default: 32M, min: 1M) |
| `--tile-format mvt\|mlt` | Tile payload format (default: `mvt`). `mlt` is wired but not yet implemented |
| `--tile-compression gzip\|brotli` | Tile compression algorithm (default: `gzip`, MVT only) |
| `--compress-sort-chunks lz4\|snappy` | Compress sort chunk files (off by default). Reduces disk I/O at the cost of CPU |
| `--fanout-cap-default N` | Default fanout cap for all polygon layers (0 = uncapped) |
| `--fanout-cap layer=N,...` | Per-layer fanout caps (e.g. `water_polygons=4096`). Features whose bbox tile count exceeds the cap are skipped at that zoom. Comma-separated |
| `--seam-reconcile-layers layer:maxzoom,...` | Layers for shared-edge seam reconciliation (default: `boundaries:8`). Format: `layer` or `layer:maxzoom` |
| `-j N` / `--threads N` | Thread count (default: logical CPUs) |

### Inspect a PMTiles archive

```
elivagar inspect <file.pmtiles>
```

Prints header info, tile statistics, section layout, provenance, and metadata
(layer list with zoom ranges).

Provenance records what the archive was built from and under which contract:

```
  Provenance:  schema 1
    Input:      north-america-seq4710-locations.osm.pbf
                xxh3 8122bcc83873ef95349e6a3522827fd9  17.8 GB
                features: sorted, locations-on-ways
    Config:     shortbread, z0-z14, polygon simplify x1
                tile: mvt gzip, base level 6, policy zoom-v1
                seam: boundaries=8
                fanout: none
    Ocean:      artifact, low zoom simplified, simplifier on
                key: shp c10be1c7843c simplified b3417e31c287 level 6 policy 1
    Build:      elivagar b833fc8730cd, pbfhogg 0f1eb01a1c1e
    Effective:  coords inline, way members relation_scan, pins block_local
```

`Input` plus the whole of `Config` is the comparability contract - two archives
whose contract differs describe different work, so a geometry diff between them
says nothing about the code. `Build` and `Effective` explain a diff once the
contract matches. Archives built before the metadata block existed print
`Provenance: absent`.

### Render a tile as SVG

```
elivagar svg <file.pmtiles> -z <zoom> -x <x> -y <y> [-o output.svg]
```

Renders a single tile as SVG for visual inspection. Each MVT layer gets a distinct
color. Points render as circles, lines as stroked paths, polygons as filled paths.
Output goes to stdout by default.

### Environment variables

| Variable | Description |
|----------|-------------|
| `ELIVAGAR_NODE_STATS=1` | Print detailed SortedNodeStore diagnostics (chunk count, compression ratio, blob bytes). Requires a full scan of the node store during the PBF phase - fast on regional extracts, slow at planet scale. Basic stats (`node_store_nodes`, `node_store_groups`) are always emitted after timing, without this variable. |
| `ELIVAGAR_ALLOW_UNSAFE_FLAT_INDEX=1` | Same as `--allow-unsafe-flat-index`. Bypasses unsorted-size and flat-index-size guardrails. |

### Ocean input

`--ocean` is repeatable and is the only ocean input. **Omit it entirely and the
archive has no ocean.** Each value is one of:

| Spec | Meaning |
|------|---------|
| `z0-z14:<file.shp>` | One shapefile serves every zoom |
| `z0-z7:<file.shp>` | Low-zoom shapefile (the pre-generalized one) |
| `z8-z14:<file.shp>` | Full-resolution shapefile |
| `<file.pmtiles>` | Precomputed world-ocean artifact, built by `elivagar ocean-build` |

Shapefiles must partition z0-z14, as either a single `z0-z14` or the `z0-z7` +
`z8-z14` pair - the engine splits at z7/z8 and nowhere else, so any other
partition is rejected rather than silently rounded to the split it can do. The
two shapefiles are the OSM standard datasets from
[osmdata.openstreetmap.de](https://osmdata.openstreetmap.de/): same coastlines,
differing vertex density. At z0-z7 the whole world is at most 256x128 pixels,
so full-resolution coastline detail is sub-pixel and the split just avoids
paying for it.

The `.pmtiles` artifact is a cache over the shapefiles, not a substitute, and
is rejected on its own: an extract still computes its boundary band from the
shapefiles, and the artifact's key is validated by re-hashing them.

Nothing is auto-detected. A run's ocean is a function of its arguments, so two
runs of the same binary on the same input cannot differ by which files happen
to be on disk.

### Example

```
elivagar run denmark-latest.osm.pbf -o denmark.pmtiles \
  --ocean z0-z7:data/simplified-water-polygons-split-3857/simplified_water_polygons.shp \
  --ocean z8-z14:data/water-polygons-split-3857/water_polygons.shp \
  --ocean data/ocean-tiles.pmtiles
```

Drop the last line to compute the ocean from the shapefiles; drop all three for
an archive with no ocean.

## Pipeline

1. **PBF read** -- single-pass read building node/way indices and emitting sort records.
   If the PBF declares `Sort.Type_then_ID` (all major producers do), nodes are stored in a
   compact in-RAM index with FOR compression (~420 MB for Denmark, 75% of raw for large
   extracts). Unsorted PBFs fall back to a flat mmap file.
2. **Ocean** -- ocean processing from the `--ocean` inputs. With an artifact, this
   computes only the boundary band near the extract bbox edge and assemble merges
   the artifact for the interior
3. **Sort** -- external merge sort by Hilbert tile ID
4. **Assembly** -- MVT encode + gzip + PMTiles write

`--skip-to ocean` reuses PBF chunks from a previous full run.
`--skip-to sort` reuses all chunks (PBF + ocean).
`--skip-to assemble` reuses all chunks and jumps straight to tile assembly.

### Flat index safety guardrails

When input does not declare `Sort.Type_then_ID`, elivagar falls back to the flat mmap node index.
That path can be dangerous at large scale, so guardrails are enabled by default:

1. Unsorted inputs larger than `1 GB` fail fast before heavy work starts.
2. Flat index growth is hard-capped at `16 GB`.

Recommended remediation:

```
pbfhogg sort input.pbf -o sorted.pbf
```

Alternative sorter:

```
osmium sort input.pbf -o sorted.pbf
```

If you intentionally want to bypass guardrails for expert debugging/CI, use
`--allow-unsafe-flat-index` (or `ELIVAGAR_ALLOW_UNSAFE_FLAT_INDEX=1`).

Host guidance:

- 32 GB hosts: avoid unsafe flat index mode. Use sorted PBFs or locations-on-ways input.
- 64 GB hosts: still prefer sorted PBFs. Unsafe flat index mode is for controlled/debug use only.

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
| Tilemaker | 30s | - | - | - | - |
| Planetiler 0.10 | 41s | - | - | - | - |
<!-- BENCH:END -->

System: plantasjen (Ryzen 9 5900X, Linux 6.18). Commit: `cb2cd29`.

Measured with `brokkr bench self`. Results stored in `.brokkr/results.db`.

### Sort chunk compression

Optional `--compress-sort-chunks` reduces disk I/O for sort data at the cost
of CPU. Intended for planet-scale runs where sort data exceeds available RAM.

Germany extract (5.3 GB PBF, `--locations-on-ways`), plantasjen, commit `30a023c`:

| Compression | Total | Peak RSS |
|-------------|-------|----------|
| None (default) | **114s** | **8.2 GB** |
| lz4 | 134s | 9.2 GB |
| snappy | 143s | 8.6 GB |

On regional extracts where sort data fits in page cache, compression adds
overhead without benefit. At planet scale (~100+ GB sort data, disk-bound
merge), compressed chunks may break even or win on total wall time.

### O_DIRECT-friendly layout

The data section of the output PMTiles archive is 4K-aligned, allowing tile
serving via `O_DIRECT` / `io_uring` without page cache pollution. This is
fully backwards-compatible with all PMTiles readers - the spec does not
constrain the data section offset.

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

## Test fixture refresh

MVT conformance fixtures live in `tests/fixtures/mvt_fixtures/` and are checked
into git. CI and local tests do not download them.

To refresh from upstream Mapbox `mvt-fixtures`:

1. Clone upstream to local scratch (for example `.cache/mvt-fixtures-upstream`).
2. Copy the selected fixture directories into `tests/fixtures/mvt_fixtures/`.
3. Update `tests/fixtures/mvt_fixtures/README.md` with the upstream commit hash
   and imported fixture IDs.

The scratch clone directory (for example `.cache/`) is optional maintenance
workspace and should remain untracked.

## License

Apache-2.0
