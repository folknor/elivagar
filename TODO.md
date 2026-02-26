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

## Performance

Investigated-and-rejected optimizations are documented in code comments at each site.
Hotpath profile results and analysis: `notes/hotpath-profile.md`

- [x] ~~**Visvalingam-Whyatt instead of Douglas-Peucker.**~~ Tried and reverted — VW's
  allocation overhead exceeds DP savings for small geometries (avg ~10 vertices). Non-cascading
  VW also produces more output at low zooms. See `notes/vw-simplification-experiment.md`.

- [x] ~~**SortedNodeStore**~~ — replaced 96 GB sparse mmap with compact in-RAM hierarchical
  store (bitmask+popcount, ~420 MB for Denmark). PBF phase: 16s → 11s. Total: 24s → 17s.

- [x] ~~**libdeflate**~~ — replaced flate2 with libdeflater for gzip compression. Assemble
  phase: 2.5s → 2.3s.

- [ ] **Bitpacked coordinate compression for SortedNodeStore** — needed for planet scale
  (8.5B nodes × 8 bytes = 68 GB uncompressed, target 64 GB RAM). Denmark doesn't need it.
  Node IDs are chronological, NOT geographic — consecutive IDs can be anywhere on the globe,
  so naive delta encoding between consecutive nodes is unreliable. Best approach: FOR
  (Frame of Reference) encoding per 128-value block using `bitpacking` crate (v0.9, stable
  Rust, SSE3 SIMD). Per block: compute min_lat/min_lon, store offsets as u32, bitpack at
  the block's max bit-width. Estimated compression: ~24 bits avg (vs 32 raw) → 68 GB → ~51 GB.
  Lookup overhead: ~150ns to decompress a 128-value block (+13% on `process_raw_way`).
  **Validate first**: run a standalone tool on a planet PBF to measure actual bit-width
  distribution before implementing in the pipeline.

- [x] ~~**Double-buffer + block-level way dispatch**~~ — block-level dispatch via
  `into_blocks_pipelined`. Worker thread receives owned PrimitiveBlocks, extracts + rayon
  processes. Main thread drains results between blocks. PBF phase: 13.3s → 9.3s. Total: 17s → 15s.

- [ ] **Reduce serial drain cost** — `drain_processed_ways` takes 4.55s (39% of PBF phase).
  Investigated breakdown: `sort_writer.push()` is NOT the bottleneck — just Vec::push (no
  I/O for Denmark). `land_mask.mark_bbox()` is already thread-safe (AtomicU8, &self) and
  already called from rayon during relations — can trivially move back to rayon. The real
  cost is `way_index.put()`: random mmap writes to a sparse offset file (way IDs up to ~1B
  → 12 GB file with 79 MB populated). Three approaches ranked:
  1. **Land_mask → rayon + deeper channel** (-0.5 to -1s, trivial, risk-free)
  2. **Dedicated drain thread** (-2 to -3s, medium complexity, ownership dance at
     way→relation transition)
  3. **Concurrent way_index offset writes** (-1 to -2s, medium-high, requires unsafe mmap
     writes to non-overlapping 12-byte slots)

- [ ] **Rayon alternatives for slice-based parallelism** — Research notes in previous git
  history. Key options: paralight, orx-parallel, chili, forte. Not a current bottleneck.

## Performance: Linux kernel features — all reverted

All madvise/fadvise hints were tried and removed. Every hint caused regressions because the
node index was a sparse file. Now that SortedNodeStore is in-RAM, the madvise concern is moot
for the node index. See CLAUDE.md for full history.
