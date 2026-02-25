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

- [x] **Wire up `NodeIndexReader::advise_random()`.** Conditionally sets `MADV_RANDOM`
  when index exceeds 50% of physical RAM. Called after `into_reader()` in the pipeline.

- [x] **`MADV_HUGEPAGE` for node index.** `advise_hugepage()` on `NodeIndexReader`,
  called after `into_reader()`. `#[cfg(target_os = "linux")]` gated.

- [x] **`MADV_SEQUENTIAL` for ocean shapefile mmap.** Applied after mmap creation.

- [x] **`fadvise(POSIX_FADV_SEQUENTIAL)` for sort chunk reads.** Applied when opening
  each `ChunkReader`. Linux-only via `libc::posix_fadvise`, no-op elsewhere.

- [x] **`fadvise(POSIX_FADV_DONTNEED)` after sort chunk reads.** Applied per-chunk
  when the merge reader drains it. Frees 100+ GB of cache at planet scale.

- [x] **Way index `MADV_RANDOM` threshold.** Now conditional on total way index size
  exceeding 50% of RAM, matching node index threshold logic.

### Tier 2: Planet-specific (NVMe, Linux 5.14+)

At planet scale the node index (96 GB) exceeds typical 64 GB RAM. Every cache miss
is a random 4 KB read — ~10µs on NVMe, ~5ms on HDD. These items assume NVMe and
recent kernels.

- [x] **`MADV_POPULATE_READ` for node index prefaulting (Linux 5.14+).** After node
  write phase, prefault pages before way processing begins. Currently pages fault on
  demand during parallel rayon way processing. Prefaulting does a sequential read
  pass the kernel can optimize with readahead. **Only beneficial when the index fits
  in RAM** (regional extracts up to ~Germany). At planet scale (96 GB > 64 GB RAM),
  skip — prefaulting would just evict pages that need faulting again.
  Uses the inverse of `advise_random()`'s RAM threshold.

- [x] **`FADV_DONTNEED` after sort chunk writes.** Sort chunks (100+ GB) are written
  during the PBF phase — exactly when the node index needs maximum page cache
  residency. Each chunk's pages are evicted from cache immediately after writing via
  `fadvise(POSIX_FADV_DONTNEED)`. Simpler than `O_DIRECT` (no aligned-buffer
  infrastructure) with the same net effect: chunk data doesn't accumulate in cache.
  Applied unconditionally in `write_sorted_chunk()`.

- [x] **`FADV_SEQUENTIAL` + `FADV_DONTNEED` for PMTiles temp blob.** The tile blob
  temp file (100-200 GB at planet) is written sequentially then read back once.
  `FADV_SEQUENTIAL` on the read-back file hints aggressive readahead;
  `FADV_DONTNEED` after the copy evicts all read-back pages from cache.
  (`pmtiles_writer.rs`)

- [ ] **io_uring: not applicable.** elivagar is CPU-bound (DP simplification, S-H
  clipping, MVT encoding, gzip level 6), not I/O-bound on write throughput. The write
  paths are sequential BufWriter appends that barely register in profiling. io_uring's
  batched async writes would save microseconds on a pipeline that takes minutes.
  See `docs/linux-io.md` for analysis.

## Test Coverage Gaps

- [ ] **No integration test for PMTiles output validity** — No test verifies generated PMTiles can be read back and tiles decoded correctly (beyond header check).
