# elivagar TODO

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## GitHub

- [ ] Write GitHub repo description and tags (vector-tiles, openstreetmap, pmtiles, shortbread, rust)
- [ ] Add GitHub Actions CI — clippy, tests, `cargo build --release` on Linux
- [ ] Add GitHub Actions release pipeline — build binaries on tag push, attach to GitHub release
- [ ] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Bugs

- [ ] **[P2]** `area_sq_meters` cos²(lat) approximation — moderate risk for high-latitude
  regions near area thresholds. See `geometry.rs` docstring for details and fix options.

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

---

- [ ] Generally check for updates to all dependencies.

## Profiling

Hotpath profile results and analysis: `docs/hotpath-profile.md`

## Performance

Investigated-and-rejected optimizations are documented in code comments at each site.

- [ ] **Visvalingam-Whyatt instead of Douglas-Peucker.** VW computes per-vertex importance
  once in O(n log n), then each zoom level filters by threshold — no re-scanning. Would
  replace DP entirely. Requires new algorithm, tolerance recalibration, and visual verification.

- [ ] **Compression level tradeoff [pre-release]** — Level 6 is used; level 3-4 would be
  noticeably faster with ~5% larger output. Make configurable. Final tuning item — do this
  right before 0.1 release after all other optimizations are locked in. (`pipeline.rs:1239`)

- [ ] **Rayon alternatives for slice-based parallelism** — Research notes in previous git
  history. Key options: paralight, orx-parallel, chili, forte. Not a current bottleneck.

## Performance: Linux kernel features for planet-scale I/O

Research notes and I/O profile analysis: `docs/linux-io.md`.

elivagar generates 300-500 GB of file I/O at planet scale across mmap'd indices
(random reads), sort chunks (sequential write/read), and PMTiles output (sequential
write + read-back). The critical bottleneck is random read latency on the 96 GB node
index — hotpath profiling shows 56% kernel time from mmap page faults. NVMe storage
is effectively required at planet scale.

### Tier 1: mmap hints and page cache hygiene (any Linux 5.x+)

Prevent 200+ GB of sort/output data from evicting node index pages. Low complexity,
high impact at planet scale.

- [ ] **Wire up `NodeIndexReader::advise_random()`.** The method exists but is never
  called. Conditionally sets `MADV_RANDOM` when index exceeds 50% of physical RAM —
  prevents kernel from wasting I/O on readahead pages that get evicted before use.
  +65% PBF regression on Denmark when set unconditionally (readahead helps small
  datasets). Call after `into_reader()` in the pipeline. (`node_index.rs:88-109`,
  `pipeline.rs:364`)

- [ ] **`MADV_HUGEPAGE` for node index.** 96 GB index = 24M TLB entries at 4 KB pages.
  `madvise(MADV_HUGEPAGE)` hints the kernel to use 2 MB transparent huge pages,
  reducing to ~48K entries. Call after `into_reader()` transitions the index to
  read-only. Works on any kernel with THP enabled. On 6.14+, file-backed mmap gets
  2 MB folios automatically — the hint ensures it also works on older kernels.

- [ ] **`MADV_SEQUENTIAL` for ocean shapefile mmap.** The shapefile (~500 MB) is
  parsed sequentially record-by-record with no madvise hint. `MADV_SEQUENTIAL`
  enables aggressive kernel readahead. (`ocean.rs:68`)

- [ ] **`fadvise(POSIX_FADV_SEQUENTIAL)` for sort chunk reads.** During k-way merge,
  each chunk is read sequentially via BufReader (256 KB buffer). `FADV_SEQUENTIAL`
  doubles the kernel readahead window. Apply when opening each ChunkReader.
  (`sort.rs:198-206`)

- [ ] **`fadvise(POSIX_FADV_DONTNEED)` after sort chunk reads.** Sort chunk data
  (100+ GB at planet) is never re-read after merge. `FADV_DONTNEED` evicts those
  pages, freeing cache for the assemble phase's PMTiles read-back. Apply per-chunk
  as the merge reader drains it. (`sort.rs`)

- [ ] **Way index `MADV_RANDOM` threshold.** Currently unconditional — always sets
  `MADV_RANDOM` on both mmaps. Should mirror the node index's smart threshold (only
  when index > 50% of RAM) to preserve readahead on small datasets.
  (`way_index.rs:121-127`)

### Tier 2: Planet-specific (NVMe, Linux 6.14+)

At planet scale the node index (96 GB) exceeds typical 64 GB RAM. Every cache miss
is a random 4 KB read — ~10µs on NVMe, ~5ms on HDD. These items assume NVMe and
recent kernels.

- [ ] **`MADV_POPULATE_READ` for node index prefaulting (Linux 5.14+).** After node
  write phase, prefault pages before way processing begins. Currently pages fault on
  demand during parallel rayon way processing. Prefaulting does a sequential read
  pass the kernel can optimize with readahead. **Only beneficial when the index fits
  in RAM** (regional extracts up to ~Germany). At planet scale (96 GB > 64 GB RAM),
  skip — prefaulting would just evict pages that need faulting again.
  Use the same RAM threshold as `advise_random()`.

- [ ] **O_DIRECT for sort chunk writes.** Sort chunks (100+ GB) are written during
  the PBF phase — exactly when the node index needs maximum page cache residency.
  Without `O_DIRECT`, chunk writes pollute the cache, evicting hot node index pages.
  `O_DIRECT` bypasses the cache entirely. Requires page-aligned buffers (replace
  BufWriter with aligned write path, similar to pbfhogg's `DirectWriter`).
  Feature-gate under `linux-direct-io`.

- [ ] **O_DIRECT or `FADV_DONTNEED` for PMTiles temp blob.** The tile blob temp file
  (100-200 GB at planet) is written sequentially then read back once. Without hints
  it consumes the entire page cache. `O_DIRECT` on the write path is cleanest;
  alternatively interleave `FADV_DONTNEED` calls during read-back.
  (`pmtiles_writer.rs`)

- [ ] **io_uring: not applicable.** elivagar is CPU-bound (DP simplification, S-H
  clipping, MVT encoding, gzip level 6), not I/O-bound on write throughput. The write
  paths are sequential BufWriter appends that barely register in profiling. io_uring's
  batched async writes would save microseconds on a pipeline that takes minutes.
  See `docs/linux-io.md` for analysis.

## Test Coverage Gaps

- [ ] **No integration test for PMTiles output validity** — No test verifies generated PMTiles can be read back and tiles decoded correctly (beyond header check).
