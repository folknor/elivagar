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
- [ ] Check for updates to all dependencies before first release

## Performance

Investigated-and-rejected optimizations are documented in code comments at each site.
Hotpath profile results and analysis: `docs/hotpath-profile.md`

- [ ] **Visvalingam-Whyatt instead of Douglas-Peucker.** VW computes per-vertex importance
  once in O(n log n), then each zoom level filters by threshold — no re-scanning. Would
  replace DP entirely. Requires new algorithm, tolerance recalibration, and visual verification.

- [ ] **Compression level tradeoff [pre-release]** — Level 6 is used; level 3-4 would be
  noticeably faster with ~5% larger output. Make configurable. Final tuning item — do this
  right before 0.1 release after all other optimizations are locked in. (`pipeline.rs:1239`)

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

All implemented. Research notes and I/O profile analysis: `docs/linux-io.md`.

- `MADV_RANDOM` on node index + way index (conditional on >50% RAM)
- `MADV_HUGEPAGE` on node index
- `MADV_POPULATE_READ` on node index (conditional on ≤50% RAM, Linux 5.14+)
- `MADV_SEQUENTIAL` on ocean shapefile mmap
- `FADV_SEQUENTIAL` on sort chunk reads, `FADV_DONTNEED` when drained
- `FADV_DONTNEED` after sort chunk writes
- `FADV_SEQUENTIAL` + `FADV_DONTNEED` on PMTiles temp blob read-back
- io_uring: not applicable (CPU-bound, not I/O-bound). See `docs/linux-io.md`.

## Code Quality Review Findings

From Opus code review (2026-02-25). Grouped by severity.

### Bugs

- [x] **`max_zoom >= 15` panics stats arrays** — Per-zoom statistics arrays are `[u64; 15]`
  but indexed by `config.max_zoom` with no upper bound check. User passing `--max-zoom 15`
  causes index-out-of-bounds panic. (`pipeline.rs:1183-1226`)
  - **Severity**: Low in practice — CLI hardcodes `max_zoom: 14`, no `--max-zoom` flag.
    Only triggerable via public API `elivagar::run()`.
  - Writer thread accumulation (line 1193) is guarded with `< 15`, but the printing loop
    at lines 1223-1226 panics unconditionally.
  - All Shortbread layers have `max_zoom: 14`, so higher is meaningless (empty tiles).
  - **Fixed**: Validation at top of `run()` rejects `max_zoom > 14` and `min_zoom > max_zoom`.

### Correctness

- [x] **NaN floats violate `Eq` contract in value interning** — `PartialEq` derives use
  float comparison (`NaN != NaN`) but `Hash` uses `to_bits()`. If NaN were interned, HashMap
  lookups would never find existing entries, leaking memory. (`mvt.rs:26-57`)
  - **Unreachable today**: Only float source is `way_area` from `area_sq_meters()` on valid
    geographic coordinates — cannot produce NaN. Correctness-by-coincidence.
  - Secondary bug: derived `PartialEq` says `+0.0 == -0.0` but `Hash` gives different hashes
    via `to_bits()`. Could cause duplicate interning.
  - No other float-containing types used as HashMap keys in the codebase.
  - **Fixed**: Manual `PartialEq` impl using `to_bits()` for `Float`/`Double` variants.

- [x] **Ocean shapefile parsing panics on corrupt input** — Multiple unsafe casts and missing
  bounds checks in `.shp` record parsing. (`ocean.rs:109-168`)
  - Lines 109-112: negative `num_parts`/`num_points` i32 cast `as usize` → huge values → OOM/panic.
  - Line 115: `points_start` not validated against `shp.len()`.
  - Lines 117-124: part index loop has no bounds check on `shp[b..b+4]`.
  - Line 121: negative part index value cast `as usize` → garbage ring slicing on line 140.
  - Lines 127-134: points loop has no bounds check on `shp[b..b+16]`.
  - **Fixed**: Validate num_parts/num_points >= 0, record_end <= shp.len(), part indices
    <= num_points. Corrupt records skipped with `eprintln!` warning.

- [x] **Ocean `.shx` parsing panics on short files** — `(shx_data.len() - 100) / 8` underflows
  `usize` if `.shx` file is shorter than 100 bytes, wrapping to huge value → OOM on
  `Vec::with_capacity` or out-of-bounds access. (`ocean.rs:53`)
  - `.shx` is read via `std::fs::read()` (not mmap'd). No other underflow risks in file.
  - Also: negative `offset_words` at lines 57-61 cast `as usize` wraps to huge value.
  - **Fixed**: Panic with descriptive message if `shx_data.len() < 100`. Negative
    `offset_words` entries skipped.

- [x] **Missing endianness assert in `way_index.rs`** — `from_raw_parts` pointer cast at
  line 177 reinterprets LE bytes as native `(i32, i32)`. Would silently produce wrong
  coordinates on big-endian. (`way_index.rs:173-177`)
  - `wire_format.rs:23` has the guard: `const _: () = assert!(cfg!(target_endian = "little"), ...)`.
  - `node_index.rs` is safe — uses `from_le_bytes`/`to_le_bytes` API, no pointer casts.
  - Only two LE-dependent pointer casts in codebase: `wire_format.rs` (guarded) and
    `way_index.rs` (missing).
  - **Fixed**: Added compile-time endianness assert after existing size/align asserts.

- [x] **`merc_bbox()` returns inverted bbox on empty input** — Initializes min/max with
  `f64::MAX`/`f64::MIN` (not `NEG_INFINITY`/`INFINITY`). (`geometry.rs:1017-1029`)
  - **Purely theoretical**: All 8 callers guarantee non-empty input via upstream length guards
    (len >= 2 or len >= 4). Traced every call site — none can pass empty.
  - Even with empty input, downstream `for_each_tile_in_bbox` iterates zero tiles with
    inverted bbox (min > max → empty range), so behavior is safe.
  - `ring_bbox()` at line 835 uses `f64::INFINITY`/`f64::NEG_INFINITY` — inconsistent style.
  - **Fixed**: Swapped to `f64::INFINITY`/`f64::NEG_INFINITY` for consistency with `ring_bbox()`.

### Minor

- [ ] **`append_geometry` emits command word before validating parameters** — Command word
  with original `cmd_count` is pushed to dest before the parameter loop. If source geometry is
  truncated, output has mismatched command count. Internal data only. (`mvt.rs:470-490`)

- [ ] **Merge tombstone detection relies on `geometry.is_empty()`** — A legitimate zero-geometry
  feature (if one existed) would be incorrectly dropped by `retain`. In practice, encoders
  always produce at least one command. (`mvt.rs:579`)

- [ ] **DDA rasterization misses corner-crossing tiles** — When `t_max_x == t_max_y` (segment
  crosses exact grid corner), only Y step is taken, skipping the X-direction tile. Rare in
  practice. (`ocean.rs:476-532`)

- [ ] **`pair_rings` tests only first vertex, silent fallback** — Inner ring point-in-polygon
  test uses only `inner[0]`. Orphan inner rings silently assigned to polygon[0] via
  `unwrap_or(0)`. (`multipolygon.rs:150-163`)

- [ ] **Greedy chain-joining can fail depending on way order** — `join_ways` is a single-pass
  greedy algorithm. Endpoint map entries can be overwritten when chains extend, leaving other
  chains orphaned. Well-known limitation of greedy chain-joining. (`multipolygon.rs:231+`)

- [ ] **`tile_id_to_zxy` overflows at z=31** — `n * n * 4` overflows u64 before the `z >= 31`
  guard is checked. Test-only function, max zoom is 14. (`pmtiles_writer.rs:637-640`)

- [ ] **Dedup hash collision guard checks length only, not content** — Two tiles with same
  64-bit hash AND same compressed length would be incorrectly deduplicated. Probability ~2^-96.
  (`pmtiles_writer.rs:154-159`)

- [ ] **Wire format string value length truncated to u16** — String values exceeding 65535
  bytes silently truncate, desynchronizing the decoder for all subsequent attributes in that
  feature. OSM values rarely exceed this. (`wire_format.rs:44`)

- [ ] **`sort.rs` record count truncation to u32** — `records.len() as u32` truncates silently.
  Safe with 1 GB chunk budget (~89M max records). (`sort.rs:221`)

- [ ] **`make_sort_key` overflows at z24+** — `tile_id << 16` overflows u64 above z23. Safe
  with z0-z14. (`sort.rs:46-47`)

- [ ] **Division by zero in edge intersection** — `edge_intersect` divides by `dx`/`dy`
  without checking for zero. Produces NaN/Inf rather than panic. Should not be reachable due to
  inside/outside classification, but floating-point rounding in prior clipping passes could
  produce near-zero denominators. (`geometry.rs:689-702`)

## Test Coverage Gaps

- [ ] **No integration test for PMTiles output validity** — No test verifies generated
  PMTiles can be read back and tiles decoded correctly (beyond header check).
