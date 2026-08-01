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
| `--way-budget size` | In-flight way processing budget (default: 128M standard / 8G with `--locations-on-ways`, min: 1M). Charged as decompressed block size x10, and capped independently at `--threads` blocks in flight, so real memory is bounded by that count regardless of this value |
| `--assemble-budget size` | Tile assembly batch budget (default: 32M, min: 1M) |
| `--tile-format mvt\|mlt` | Tile payload format (default: `mvt`). `mlt` requires a build with the `mlt` cargo feature; see [MLT output](#mlt-output) |
| `--tile-compression gzip\|brotli` | Tile compression algorithm (default: `gzip`, MVT only) |
| `--compress-sort-chunks lz4\|snappy` | Compress sort chunk files (off by default). Reduces disk I/O at the cost of CPU |
| `--fanout-cap-default N` | Default fanout cap for all polygon layers (0 = uncapped) |
| `--fanout-cap layer=N,...` | Per-layer fanout caps (e.g. `water_polygons=4096`). Features whose bbox tile count exceeds the cap are skipped at that zoom. Comma-separated |
| `--seam-reconcile-layers layer:maxzoom,...` | Layers for shared-edge seam reconciliation (default: `boundaries:8`). Format: `layer` or `layer:maxzoom` |
| `--polygon-simplify-factor F` | Simplification multiplier for polygon layers (default: 1.0, same as lines). Higher simplifies harder, cutting sort volume; fills tolerate this better than stroked lines do |
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

No variable duplicates a flag or overrides one: a knob is a flag or an env
var, never both.

| Variable | Description |
|----------|-------------|
| `ELIVAGAR_ASSEMBLE_WORKERS` | Assemble worker cap (default 8). Encode CPU saturates near 8; 12 measured worse. |
| `ELIVAGAR_ASSEMBLE_PARK_BUDGET` | Bytes of encoded-but-unwritten tiles workers may run ahead of the writer (default 2 GiB). The RAM ceiling on assemble. |
| `ELIVAGAR_ASSEMBLE_SPLIT_TARGET` | Record-byte target for hot-partition splitting (default 64 MiB). Partitions over 2x this split into contiguous tile-range pieces so one dense block cannot stall the writer. |
| `ELIVAGAR_REL_BLOCKS_CAP` | Relation-block buffer cap in bytes. Past it the tail re-reads relation blobs instead of holding them. |
| `ELIVAGAR_LAYER_STATS=1` | Per-layer per-zoom sort-stats firehose (~800 counters), emitted at end of run. Per-layer totals are always emitted. |

Full detail, including the harness variables, in [reference/cli.md](reference/cli.md).

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
`--allow-unsafe-flat-index`.

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

### Planet

A full planet build, 2026-07-31, on a 16-core desktop with 30.5 GiB of RAM:

| | |
|---|---|
| Input | 90.5 GB enriched PBF (locations-on-ways), planet seq 4912 |
| Wall | **571.7s** (9m32s) |
| Peak RSS | 12.7 GB |
| Output | 58.7 GiB PMTiles, z0-z14, 26 layers |
| Tiles | 269.8M addressed, 52.2M unique (80.7% deduplicated) |
| Features | 2.52 billion |

Host: bygg (Ryzen 9 9950X3D2, 16c/32t, 30.5 GiB RAM, single NVMe).

For context, planetiler's published planet table lists 2h38m on a 16 cpu /
32 GB machine, and 19 minutes on 192 cores / 720 GB. Two things must be said
alongside any such comparison, and neither is optional: the number above is
for **enriched input**, so pbfhogg's preprocessing (roughly 10 minutes at
planet) is not in it - end to end the honest figure is ~20 minutes - and the
profiles differ (Shortbread here, OpenMapTiles there), as do the planet
snapshots. Treat this as a measurement, not a record claim.

### Extracts

| dataset | bygg | plantasjen |
|---------|------|-----------|
| denmark | 6.9s | 8.8s |
| germany | 26.7s | 53.4s |
| north-america | 130.1s | 251.0s |

Locations-on-ways variants. The two hosts are not comparable run-to-run
(bygg is ~1.9x); older numbers elsewhere in the tree are plantasjen's.

Denmark against other tools, plantasjen, best of 3:

| Tool | Total |
|------|-------|
| **elivagar** | **12s** |
| Tilemaker | 30s |
| Planetiler 0.10 | 41s |

Those competitor runs are stale and predate several campaigns on our side;
re-run them before quoting a ratio. Measured with `brokkr tilegen --bench`;
results in `.brokkr/results.db`, per-phase detail in `.brokkr/sidecar.db`.

### Sort chunk compression

Optional `--compress-sort-chunks` reduces disk I/O for sort data at the cost
of CPU. Intended for planet-scale runs where sort data exceeds available RAM.

North America (19.06 GB PBF, `--locations-on-ways`), plantasjen: lz4 costs
+2.0% wall and cuts phase12 physical writes from 60.1 to 26.3 GB and assemble
reads from 79.3 to 30.8 GB, for ~2.6x less scratch on disk.

On regional extracts where sort data fits in page cache, compression adds
overhead without benefit. At planet scale the merge reads overflow the page
cache and the trade is expected to invert; the 571.7s run above was
uncompressed, so it is the control rather than the demonstration.

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

Run with `brokkr pmtiles-writer --bench [--tiles N]`.

## MLT output

`--tile-format mlt` writes [MapLibre Tile](https://github.com/maplibre/maplibre-tile-spec)
payloads instead of MVT, via upstream `mlt-core`. It is **off by default** and
requires an opt-in build:

```
cargo build --release --features mlt
```

Without the feature, `--tile-format mlt` refuses with the feature named rather
than silently falling back to MVT.

It is gated because it is unvalidated, not because it is unfinished: the
encoder is real and round-trips through `mlt_core` in tests, but no client has
ever consumed its output, the path writes uncompressed payloads, and none of
the standing correctness gates below cover it. Treat it as experimental and do
not serve it to users without validating against your renderer first.

## Correctness gates

The archive is the only sacred artifact here, and three gates define correct:

- `elivagar verify <file>` - container integrity, metadata schema, tile
  decompression, MVT structure, geometry command validation, layer coverage,
  and ocean ring self-intersection checks.
- `scripts/validate/earcut-oracle.mjs <file> all` - the tessellation-fidelity
  gate. Decodes every tile the way MapLibre does, groups rings with
  maplibre-gl's own `classifyRings`, tessellates with earcut, and requires zero
  deviant polygons and zero misattached holes on every polygon layer. This is
  the oracle that caught a ClosePath cursor bug after three months of clean
  internal round-trips; run it on anything touching geometry or MVT encoding.
- A committed per-tile digest baseline (`corpus/denmark/`) that a fresh build
  is checked against, so an unintended output change fails loudly and an
  intended one is reviewed as a diff.

A validator gates work only once it has been calibrated in both directions -
shown to fire on a known-bad artifact and clear on a known-good one. Aggregate
measures that are legitimately large on correct output make poor gates; the
ones above are categorical, where passing means a count of zero.

## Building

Requires Rust 1.97 or newer (edition 2024). Stable toolchain; no nightly
features are used.

```
cargo build --release
```

Add `--features mlt` for MLT output (see above).

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
