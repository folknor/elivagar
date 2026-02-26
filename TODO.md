# elivagar TODO

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

## Planet scale

See [notes/planet-scale.md](notes/planet-scale.md) for the full roadmap.

- [x] Step 1: `pbfhogg node-stats` tool
- [x] Step 2: Validate compression on Germany/Norway/Japan (worst case 72%, planet fits under 64 GB)
- [x] Step 3: SortedNodeStore compression — **75% ratio achieved** (planet: 51 GB, fits in 64 GB)
  - [x] Arena allocation for SortedNodeStore chunks (per-group Vec<u8>)
  - [x] Selective compression — skip FOR when compressed ≥ raw
  - [x] BitPacker1x (32-value blocks) — tried, rejected: metadata overhead > compression gain
  - [x] Shrink ChunkMeta from 64B → 40B — 278 MB savings on Germany
  - [x] Exact-size bitpacking — replaced BitPacker4x (128-value padded blocks) with scalar N-value packing. Compressed chunks: 26% → 80%
  - [x] Flat byte blob per group — eliminated ChunkMeta struct, all metadata inline in `Box<[u8]>`. Removed `bitpacking` crate dependency
- [ ] Step 4: Full pipeline on North America (~17 GB) — needs ≥32 GB RAM
- [ ] Step 5: Full pipeline on Europe (~28 GB) — needs ≥64 GB RAM
- [ ] Step 6: Planet (~75 GB) — needs ≥64 GB RAM hardware
