# elivagar TODO

## Release prep

- [ ] Publish `pbfhogg` to crates.io first (currently a path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## GitHub

- [x] Write GitHub repo description and tags (vector-tiles, openstreetmap, pmtiles, shortbread, rust)
- [x] Add GitHub Actions CI — `.github/workflows/ci.yml` (manual-only until pbfhogg published)
- [x] Add GitHub Actions release pipeline — `.github/workflows/release.yml` (manual-only until pbfhogg published)
- [x] Add a CHANGELOG.md before first tagged release

## Website

- [ ] Write a small 1-page project website (what it does, benchmark, usage, link to repo)
- [ ] Host via GitHub Pages

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server
- [x] Check for updates to all dependencies before first release — done 2026-02-25

## Performance

Investigated-and-rejected optimizations are documented in code comments at each site.
Hotpath profile results and analysis: `notes/hotpath-profile.md`

- [ ] **Visvalingam-Whyatt instead of Douglas-Peucker.** VW computes per-vertex importance
  once in O(n log n), then each zoom level filters by threshold — no re-scanning. Would
  replace DP entirely. Requires new algorithm, tolerance recalibration, and visual verification.

- [x] **Compression level tradeoff** — Now configurable via `--compression-level 0-10`
  (default 6). `TilegenConfig.compression_level` field.

- [x] **`add_feature_to_layer` per-feature Vec pool** — Was 4.4 GB (317 B avg), now 4.2 GB
  (302 B avg, −5%). Per-rayon-worker Vec pools for geometry + tags, reclaimed after encode.
  Modest gain because ~70% of the 4.4 GB is intern operations (key_map, value_map,
  string_value_map, features Vec growth) which are fresh per tile and not pooled. Further
  optimization would require pooling entire `LayerBuilder`s across tiles — diminishing
  returns given assemble phase is ~2% of wall time.

- [x] **`merge_same_attr_geometries` buffer reuse + in-place merge** — Was 3.5 GB (11.8 KB
  avg), now 3.4 GB (11.7 KB avg, −3%). `MergeScratch` hoists HashMap + geom buffer. Tag sort
  eliminated (deterministic from shortbread). In-place merge via scratch geom + `mem::swap` +
  `mem::take` to reclaim dead feature Vecs into pool + `retain`. Tag clone kept (raw entry
  API = future work).

- [ ] **Rayon alternatives for slice-based parallelism** — Research notes in previous git
  history. Key options: paralight, orx-parallel, chili, forte. Not a current bottleneck.

## Performance: Linux kernel features for planet-scale I/O

All implemented. Research notes and I/O profile analysis: `notes/linux-io.md`.

- `MADV_RANDOM` on node index + way index (conditional on >50% RAM)
- `MADV_HUGEPAGE` on node index
- `MADV_POPULATE_READ` on node index (conditional on ≤50% RAM, Linux 5.14+)
- `MADV_SEQUENTIAL` on ocean shapefile mmap
- `FADV_SEQUENTIAL` on sort chunk reads, `FADV_DONTNEED` when drained
- `FADV_DONTNEED` after sort chunk writes
- `FADV_SEQUENTIAL` + `FADV_DONTNEED` on PMTiles temp blob read-back
- io_uring: not applicable (CPU-bound, not I/O-bound). See `notes/linux-io.md`.

## Test Coverage Gaps

- [x] **No integration test for PMTiles output validity** — `tests/pmtiles_roundtrip.rs`:
  4 always-run tests (header, data round-trip, MVT decode, deduplication) + 1 `#[ignore]`
  full pipeline test. Uses inline sync PMTiles reader + MVT decoder.
