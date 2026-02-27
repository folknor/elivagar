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

- [x] **PBF phase regression** (8.0s → 9.3s on plantasjen Denmark) — **accepted cost**. The regression is the price of SortedNodeStore compression, which is required for planet-scale (51 GB vs 200+ GB uncompressed). Root cause: `decompress_chunk` DRAM latency on the 270 MB compressed blob. Partially mitigated on dm6 (14.6s→13.7s PBF phase) via:
  - 4-entry LRU decompression cache (`18b13e4`) — decompress_chunk total -52%
  - UnsafeCell replacing RefCell (`4ac8c11`) — -3.1%
  - Scratch vec hoisting (`9e014d1`) — -5-7%
  - Tried and rejected: larger chunks (512, +800ms), accumulator unpacking (-10%), offset table (irrelevant at 76% cache hit rate)
  - Remaining bottleneck is fundamentally DRAM-latency-bound. The 270 MB blob doesn't fit in L3 cache (16 MB on dm6, 64 MB on plantasjen), so ~24% of lookups miss all caches and wait ~820ns for DRAM (vs 25ns when L1-hot in synthetic benchmarks). Compute optimizations don't help — the CPU is waiting on memory, not arithmetic.
  - **Smaller blob**: Better compression ratio → more of the blob fits in L3 → fewer cache misses. Current ratio is 75% on Denmark. Unclear how much further it can shrink without losing decode speed.
  - **Better access locality**: Node lookups follow way-reference order, which jumps randomly across the blob. If nodes frequently accessed together were stored nearby in memory, cache lines (64 bytes per fetch) would serve multiple lookups before eviction. Would require reordering the store to match access patterns — significant undertaking.
  - Both approaches are speculative with unclear payoff. The blob grows with dataset size (planet >> 270 MB), so even server-class L3 caches (32 MB/CCD on EPYC Genoa, 96 MB/CCD on Genoa-X) won't cover it. The access pattern is dictated by way references and is essentially random. Reordering would require a pre-pass over all ways, duplicating the expensive work. The 1.3s regression is a reasonable trade for planet-scale support. See inline comments in `node_index.rs` for details.

- [x] **Assemble phase +0.5s** — **not real**, confirmed noise. Plantasjen at `605a1a5`: 2.5s assemble (was 2.7s in old baseline, 2.2s pre-compression). The +0.5s was a measurement artifact.

### Plantasjen TODO — completed 2026-02-27

- [x] **Re-baseline on plantasjen** — `bench-self.sh` best of 3 at `605a1a5`: 14.2s total (9.2s pbf, 1.4s ocean, 0.5s sort, 2.5s assemble). LRU cache optimizations show modest -0.1s PBF improvement (plantasjen's 64 MB L3 already covered most of the blob, unlike dm6's 16 MB).
- [x] **Confirm assemble phase regression** — **not real**. 2.5s at `605a1a5`, consistent with pre-compression baseline (2.2s). The old 2.7s measurement was noise.
- [x] **Update README performance numbers** — updated to `605a1a5` baseline.
- [x] **Run hotpath on plantasjen** — `decompress_chunk`: avg 560ns (vs 820ns on dm6), 13.4s total (60% wall). The 64 MB L3 gives a 32% latency reduction. P95 is 980ns on both machines — the true DRAM penalty when L3 misses. See hotpath-profile.md for full results.

### Baselines

Plantasjen Denmark baseline (best of 3, `605a1a5`, all optimizations):
14.2s total (9.2s pbf, 1.4s ocean, 0.5s sort, 2.5s assemble). 16.0M features, 56.4K unique tiles, 286 MB output.

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
