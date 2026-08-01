---
layout: home

hero:
  name: "elivagar"
  text: "Shortbread vector tiles at planet scale"
  tagline: "Reads OSM PBF, writes PMTiles v3. The full planet in 9m32s on a 16-core desktop, peaking at 12.7 GB of RAM."
  image:
    src: /elivagar-logo.svg
    alt: Elivagar logo
  actions:
    - theme: brand
      text: Get Started
      link: /guide/
    - theme: alt
      text: CLI Reference
      link: /reference/cli
    - theme: alt
      text: GitHub
      link: https://github.com/folknor/elivagar

features:
  - icon:
      src: /icons/gauge.svg
    title: Planet on a Desktop
    details: "A 90.5 GB planet PBF to a 58.7 GiB archive in 571.7s on 16 cores, peaking at 12.7 GB of RAM. 269.8M tiles addressed, 52.2M unique, 2.52 billion features."
  - icon:
      src: /icons/globe.svg
    title: Full Shortbread Schema
    details: "All 26 layers, z0 through z14, with ocean coastlines served from a durable precomputed world artifact that most runs never recompute."
  - icon:
      src: /icons/shield.svg
    title: Gated on What Renderers See
    details: "The standing correctness gate decodes every tile the way MapLibre does, groups rings with its own classifyRings, and tessellates with earcut. Zero deviant polygons is the bar."
---

## What it does

elivagar turns an OpenStreetMap PBF into a
[PMTiles v3](https://github.com/protomaps/PMTiles) archive carrying the
[Shortbread](https://shortbread-tiles.org/) schema: 26 layers, zoom 0 to 14,
one file you can serve directly.

```
elivagar run denmark-latest.osm.pbf -o denmark.pmtiles \
  --ocean z0-z7:data/simplified-water-polygons-split-3857/simplified_water_polygons.shp \
  --ocean z8-z14:data/water-polygons-split-3857/water_polygons.shp \
  --ocean data/ocean-tiles.pmtiles
```

## Why the memory number matters

Tile generation at planet scale is usually a question of how much RAM you can
rent. elivagar is built the other way round: every stock in the pipeline is
bounded by construction, so the peak does not track the input size. The planet
build above used 12.7 GB on a host that had 30.5 GiB available.

That constraint is why the pipeline looks the way it does - an external merge
sort partitioned by Hilbert tile-id range, a byte-budgeted claim window in
assemble, a relation-block buffer that spills at a cap, and a memory-mapped
way index rather than an in-RAM one.

## Honest comparisons

planetiler's published planet table lists 2h38m on a 16 cpu / 32 GB machine,
and 19 minutes on 192 cores / 720 GB. Three things belong next to any such
number, and this project would rather state them than be corrected later:

- Our figure is for **enriched input**. The
  [pbfhogg](https://github.com/folknor/pbfhogg) preprocessing pass is not in
  it; end to end the honest number is roughly 20 minutes.
- The **profiles differ**. Shortbread here, OpenMapTiles there. Not the same
  work.
- The **planet snapshots differ**. Ours is seq 4912, 2026-02-23.

Treat 571.7s as a measurement of this pipeline, not as a record claim.
