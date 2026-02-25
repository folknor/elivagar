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

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server
- [ ] Generally check for updates to all dependencies

## Performance

Investigated-and-rejected optimizations are documented in code comments at each site.
Hotpath profile results and analysis: `docs/hotpath-profile.md`

- [ ] **Visvalingam-Whyatt instead of Douglas-Peucker.** VW computes per-vertex importance
  once in O(n log n), then each zoom level filters by threshold — no re-scanning. Would
  replace DP entirely. Requires new algorithm, tolerance recalibration, and visual verification.

- [ ] **Compression level tradeoff [pre-release]** — Level 6 is used; level 3-4 would be
  noticeably faster with ~5% larger output. Make configurable. Final tuning item — do this
  right before 0.1 release after all other optimizations are locked in. (`pipeline.rs:1239`)

- [ ] **Rayon alternatives for slice-based parallelism** — Research notes in previous git
  history. Key options: paralight, orx-parallel, chili, forte. Not a current bottleneck.

## Performance: Linux kernel features for planet-scale I/O

All implemented. Research notes and I/O profile analysis: `docs/linux-io.md`.

- `MADV_RANDOM` on node index + way index (conditional on >50% RAM)
- `MADV_HUGEPAGE` on node index
- `MADV_POPULATE_READ` on node index (conditional on ≤50% RAM, Linux 5.14+)
- `MADV_SEQUENTIAL` on ocean shapefile mmap
- `FADV_SEQUENTIAL` on sort chunk reads, `FADV_DONTNEED` when drained
- `FADV_DONTNEED` after sort chunk writes
- `FADV_SEQUENTIAL` + `FADV_DONTNEED` on PMTiles temp blob read-back
- io_uring: not applicable (CPU-bound, not I/O-bound). See `docs/linux-io.md`.

## Test Coverage Gaps

- [ ] **No integration test for PMTiles output validity** — No test verifies generated
  PMTiles can be read back and tiles decoded correctly (beyond header check).
