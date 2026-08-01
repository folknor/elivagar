# Getting Started

elivagar reads an OpenStreetMap PBF and writes a PMTiles v3 archive carrying
the [Shortbread](https://shortbread-tiles.org/) schema - 26 layers, zoom 0 to
14.

## The shortest run

```sh
elivagar run denmark-latest.osm.pbf -o denmark.pmtiles
```

That produces a valid archive with no ocean. Coastlines come from shapefiles
you name explicitly, which is the subject of [Ocean Input](./ocean).

## The production shape

```sh
elivagar run denmark-latest.osm.pbf -o denmark.pmtiles \
  --ocean z0-z7:data/simplified-water-polygons-split-3857/simplified_water_polygons.shp \
  --ocean z8-z14:data/water-polygons-split-3857/water_polygons.shp \
  --ocean data/ocean-tiles.pmtiles
```

The output is a single file you can serve from any HTTP host that supports
range requests, or open in the
[PMTiles Viewer](https://protomaps.github.io/PMTiles/).

## The rule that shapes the CLI

**Either it is explicit, or it is not set.** Nothing is inferred from the
filesystem. A run's behaviour is a function of its arguments and its input,
and of nothing else.

That is not a style preference; it was bought with a specific failure. Ocean
inputs used to be auto-detected from a data directory, and two consequences
followed. The run's meaning lived in the filesystem rather than the
invocation, so two runs of the same binary on the same PBF could produce
different geometry with nothing in the recorded command line saying which. And
absence was silent: an archive was once built, verified and adopted as a
comparison baseline while the ocean artifact was simply missing, so it took a
different code path throughout while every gate stayed green.

Hence: no auto-detection, `--ocean` names every ocean input, omitting it means
no ocean, and a named input that cannot be honoured is an error rather than a
fallback.

The one piece of detection that survives is `--locations-on-ways`, because it
reads the PBF header rather than the filesystem - a property of an input that
is itself named explicitly and hashed into the archive's provenance.

## Checking the result

```sh
elivagar verify denmark.pmtiles
elivagar inspect denmark.pmtiles
```

`verify` validates container integrity, metadata schema, tile decompression,
MVT payload structure, geometry commands and layer coverage, and checks ocean
rings for self-intersections. `inspect` prints the header, tile statistics and
the provenance block recording what the archive was built from and under which
configuration - which is how you answer "are these two archives comparable"
before diffing them.

To look at a single tile, `elivagar svg` renders one to SVG and `elivagar
diag` reports per-ring winding. Both are in the
[CLI reference](../reference/cli).

## Where to go next

- [Installation](./install) - crates.io, from source, and the MLT feature flag
- [Ocean Input](./ocean) - shapefiles, the world artifact, and why it is a cache
- [The Pipeline](./pipeline) - the four phases and where the time goes
- [Correctness Gates](./correctness) - how output is verified, and what a gate has to satisfy
- [Performance](./performance) - measured numbers and how to read them
