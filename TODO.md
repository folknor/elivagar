# elivagar TODO

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Performance

SortedNodeStore compression (`b866306`) introduced a PBF phase regression. Required for planet-scale, cannot be reverted.

- [ ] **PBF phase regression** (8.0s → 9.3s on plantasjen Denmark). Root cause: `decompress_chunk` DRAM latency on the 270 MB compressed blob. Partially mitigated on dm6 (14.6s→13.7s PBF phase) via:
  - 4-entry LRU decompression cache (`18b13e4`) — decompress_chunk total -52%
  - UnsafeCell replacing RefCell (`4ac8c11`) — -3.1%
  - Scratch vec hoisting (`9e014d1`) — -5-7%
  - Tried and rejected: larger chunks (512, +800ms), accumulator unpacking (-10%), offset table (irrelevant at 76% cache hit rate)
  - Remaining bottleneck is fundamentally DRAM-latency-bound (820ns avg per miss). Further gains need smaller blob or better access locality. See inline comments in `node_index.rs` for details.

- [x] **Assemble phase +0.5s** — not reproducible on dm6 (+82ms, noise). Needs plantasjen confirmation.

### Plantasjen TODO (when benchmark access available)

All dm6 optimizations are committed and ready. These items need plantasjen to measure:

- [ ] **Re-baseline on plantasjen** — run `bench-self.sh` at HEAD (`189abfe` or later) with 3 runs. The old baseline was 14.7s total (9.3s pbf, 2.7s assemble) at a pre-optimization commit. The 4-entry LRU cache, UnsafeCell, and scratch vec hoisting should reduce the PBF phase. Get new numbers for all phases.
- [ ] **Confirm assemble phase regression** — the +0.5s (2.2s→2.7s) was only measured on plantasjen and is not reproducible on dm6 (+82ms, noise). Re-measure at HEAD vs `2378159` to determine if it's real or was a measurement artifact. If real, run hotpath to identify which assemble sub-function is slower.
- [ ] **Update README performance numbers** — README numbers should always come from plantasjen (the reference host). Update with the new baseline once measured.
- [ ] **Run hotpath on plantasjen** — `run-hotpath.sh` to get plantasjen-specific decompress_chunk numbers. The 270 MB blob may behave differently with the Ryzen 9's larger L3 cache (64 MB vs dm6's 16 MB). This determines whether the DRAM-latency bottleneck is as severe on plantasjen.

### Baselines

Plantasjen Denmark baseline (best of 3, pre-optimization): 14.7s total (9.3s pbf, 1.5s ocean, 0.4s sort, 2.7s assemble). 16.0M features, 53.9K unique tiles, 273 MB output.

dm6 Denmark baseline (best of 3, `2db9494`, all optimizations applied):
21.2s total (13.7s pbf, 2.6s ocean, 0.5s sort, 2.9s assemble). 16.0M features, 56.4K unique tiles, 286 MB output.

dm6 Denmark baseline (best of 3, `d90d4a1`, pre-LRU-cache):
22.9s total (14.6s pbf, 2.9s ocean, 0.5s sort, 2.6s assemble). 16.0M features, 56.4K unique tiles, 286 MB output.

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
