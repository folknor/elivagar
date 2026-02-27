# elivagar TODO

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Performance

Known regressions from SortedNodeStore compression (`b866306`). These code paths are required for planet-scale ingestion and cannot be reverted.

- [ ] **PBF phase +1.3s** (8.0s → 9.3s on plantasjen Denmark) — SortedNodeStore compression is slower than old flat mmap. Two cost centers: `put()` (write path, runs during node parsing) and `get()` (read path, runs during way processing).

  Done:
  - [x] Add hotpath instrumentation to node_index.rs — 7 functions annotated (`205aa41`)
  - [x] Build synthetic benchmark (`bench_node_store.rs`) — 5M-50M nodes, way-like + random lookups (`205aa41`)
  - [x] Cache `node_mask` in `DecompressCache` — skip `find_chunk_in_blob` on cache hits (`d90d4a1`). Synthetic: -18% way-like lookups. Real Denmark: in the noise.
  - [x] Offset table for `find_chunk_in_blob` — tried, reverted. Cache hit rate ~95% means miss path is irrelevant.
  - [x] Remove `#[hotpath::measure]` from `get` and `get_from_group_cached` (`178ca73`). Instrumentation at 23M calls added >50% overhead, distorting the profile. Way-like: 2187ms → 938ms (-57%).
  - [x] Replace `RefCell<DecompressCache>` with `UnsafeCell<DecompressCache>` (`4ac8c11`). Way-like: 938ms → 909ms (-3.1%).
  - [x] Add `--build-only` mode to `bench_node_store.rs` (`da78f60`). Write path profile: put 53%, flush_chunk 24%, compress_coords_into 8%.
  - [x] Hoist scratch Vecs in `flush_chunk()` and `compress_coords_into()` (`9e014d1`). Build: 986ms → 934ms (-5.3%). Way-like: 909ms → 847ms (-6.8%).

  Next:
  - [ ] Run `scripts/run-hotpath.sh` on Denmark — see what fraction of PBF phase is node lookups vs PBF parsing vs tag matching vs geometry. This tells us whether optimizing node_index further has any ROI.

- [ ] **Assemble phase +0.5s** (2.2s → 2.7s on plantasjen Denmark) — NOT from node lookups (assemble phase does not use the node store). Separate root cause. Diagnose by comparing `run-hotpath.sh` output at `b866306` vs prior commit to see which assemble sub-function got slower.

- [x] **Re-measure on dm6** — done, see baselines below.

Plantasjen Denmark baseline (best of 3): 14.7s total (9.3s pbf, 1.5s ocean, 0.4s sort, 2.7s assemble). 16.0M features, 53.9K unique tiles, 273 MB output.

dm6 Denmark baseline (best of 3, `d90d4a1`, includes node_mask cache optimization):
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
