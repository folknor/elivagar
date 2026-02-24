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

## Performance Regression — Investigated

- [x] **ca6f17e caused +64% PBF regression** — Bisected (best-of-3 on Denmark). Two causes:
  Tags binary search (+55%) and `advise_random()` (+65%, not additive). Both reverted.
  Full investigation in `docs/madvise-investigation.md`.

## Bugs

- [ ] **[P2]** `area_sq_meters` cos²(lat) approximation — **investigated, moderate risk.**
  Uses single centroid latitude for entire polygon (`geometry.rs:485-500`). Affects
  `enrich_polygon_matches` thresholds (2M/700K/100K km²) in `pipeline.rs:726-750`.

  **Error by latitude span:**
  | Feature | True area | Lat span | Centroid | Est. error | Threshold | Risk |
  |---------|----------|----------|----------|-----------|-----------|------|
  | Russia | 17.1M km² | 41°–82°N | 60°N | 20-30% under | 2M km² | Safe (>>2M) |
  | Canada | 10M km² | 42°–83°N | 62°N | 20-30% under | 2M km² | Safe (>>2M) |
  | Norway | 385K km² | 58°–71°N | 64°N | 15-25% under | 100K km² | Safe (>>100K) |

  **Real risk:** moderately-sized high-latitude regions near thresholds. A 700K km² region
  at 70°N (cos²≈0.12) could be underestimated to ~490K km², misclassifying from z3→z4.
  Unlikely for well-known countries but possible for sub-national boundaries.

  **Fix options:** per-edge latitude weighting (~2× cost), or latitude-range-aware cos²
  averaging (moderate cost, good tradeoff).

## Quality

- [ ] Visual verification — tracked in nidhogg TODO
- [ ] Planet-scale test — run on full planet PBF (~73 GB), needs NVMe server

---

- [ ] Generally check for updates to all dependencies.

## Planet-Scale Blockers

- [ ] **Node index virtual memory at planet scale** — `advise_random()` method exists in
  node_index.rs but is **not called** — it regresses Denmark by +65% because way→node lookups
  have locality and readahead helps. The node index is 102 GB even for Denmark (OSM node IDs
  are global). Two-level index rejected (12B/4096 blocks ≈ 93 GB, no savings). Needs
  planet-scale testing to determine if MADV_RANDOM helps when node density is higher.
  See `docs/madvise-investigation.md` and `node_index.rs` module comment.

## Profiling

Hotpath profile results and analysis: `docs/hotpath-profile.md`

## Performance: Allocation Pressure (High Impact)

Investigated thoroughly. Each SortRecord must own its `data: Vec<u8>` because records
serialize to chunk files on disk and are deserialized during k-way merge — there is no
lifetime to reference into. The emit functions already reuse `geom_buf`, `attrs_buf`, and
`tc_buf` across iterations; only the final `encode_feature_data_with_attrs()` allocates per
record, which is unavoidable since the sort buffer takes ownership. With mimalloc, Denmark-scale
runs (~2M records × 200-300 bytes) spend <1ms total in malloc. Planet-scale is more pressure but
the bottleneck is CPU (geometry) and I/O (chunk files), not allocation.

- [ ] **SortRecord `data: Vec<u8>` — billions of small heap allocs** — Investigated: arena
  rejected. Each record must own its bytes for the chunk file → k-way merge pipeline to work.
  An arena would require redesigning the chunk file format (currently per-record `key|len|data`),
  the ChunkReader, and the HeapEntry ownership model. Minimal runtime benefit vs major complexity.
  The real bottleneck is CPU and I/O, not malloc. (`sort.rs:47`)

- [x] **`node_records` allocated per tagged node in PBF callback** — Hoisted before the
  `for_each_pipelined` closure, reused via `clear()` + `drain(..)`. Saves ~200M allocations
  at planet scale (usually 1-3 SortRecords per tagged node). (`pipeline.rs:313,328`)

- [ ] **`tags_vec` allocated per element in PBF callback** — Investigated: **cannot hoist**.
  `tags_vec: Vec<(&str, &str)>` holds `&str` references borrowed from PBF elements that don't
  outlive the closure body. Hoisting outside the closure fails with E0521 (borrowed data escapes
  closure) because `Vec<&'a str>` through a mutable reference is invariant over `'a` — the
  compiler can't see that `clear()` drops old references before `extend()` adds new ones.
  Allocated for every tagged node + every way with tags + every relation (~700M at planet).
  Would need owned `Vec<(String, String)>` which is worse, or unsafe lifetime transmute.
  (`pipeline.rs:326, 372, 416`)

- [ ] **`coords_e7` allocated per way, cannot hoist** — Investigated: `coords_e7: Vec<(i32, i32)>`
  is allocated for every way (~1B at planet, avg ~8 coords = 64 bytes). Ownership transfers into
  `MatchedWay` which gets batched and sent to rayon for parallel geometry processing. Hoisting
  would require `.to_vec()` or `.clone()` to preserve the reusable buffer while the batch takes
  ownership, defeating the purpose. The only real fix is changing the batch architecture — e.g.
  a flat arena of coords that `MatchedWay` references by offset+length, but that requires
  redesigning `MatchedWay`, `flush_way_batch`, and all emit functions that take `&[(i32, i32)]`.
  Not worth the complexity unless profiling shows way coord allocation as a bottleneck distinct
  from the geometry CPU work that dominates way processing. (`pipeline.rs:358-363`)

- [ ] **`encode_feature_data_with_attrs` allocates per call** — Investigated: unavoidable. The
  returned Vec becomes `SortRecord.data` which must be owned. Passing `&mut Vec<u8>` and reusing
  would still require `.to_vec()` into the SortRecord, saving only the capacity calculation.
  (`wire_format.rs:61-78`)

- [ ] **k-way merge allocates `Vec<u8>` per record read** — Investigated: marginal. The heap
  holds k entries (typically 1-4 chunks for Denmark, ~20 for planet). Each `read_record()` allocates
  a new Vec, but only k are live at once. A buffer pool would save k reallocs per record but the
  records vary in size, so the pool would often reallocate anyway. (`sort.rs:202-221`)

- [ ] **`add_feature_to_layer` allocates geom_cmds Vec per feature** — During tile assembly,
  every decoded feature creates a `Vec<u32>`. Pass a `&mut Vec<u32>` in, or reference geometry
  as a byte slice into the sort record data. (`wire_format.rs:120-131`)

- [ ] **Sort chunk buffer doesn't free after flush** — `self.buffer.clear()` keeps allocated
  capacity. Use `std::mem::take` or `shrink_to_fit()` after flush to release the pointer array.
  (`sort.rs:154-178`)

## Performance: Algorithms & Data Structures (Medium-High Impact)

- [ ] **Tags linear scan called billions of times** — Binary search tried (ca6f17e) and
  **reverted**: +55% PBF regression (20s → 31s on Denmark). `sort_unstable_by_key` per element
  plus `binary_search_by_key` per lookup is slower than linear `.any()` for 3-15 element slices.
  See `shortbread.rs` Tags comment and `docs/madvise-investigation.md`. Possible alternative:
  perfect hash (`phf`) over the ~50 known tag keys, mapping to enum — eliminates string
  comparison entirely but requires maintaining the key set.

- [ ] **Simplify allocates two Vecs per call** — `simplify()` allocates a `vec![bool]` keep array and a result Vec per invocation. Accept an output buffer, use a bitset for keep array. (`geometry.rs:167-180`)

- [ ] **POI `contains()` linear scan on 50-entry arrays** — `AMENITY_VALUES` (51 entries), `SHOP_VALUES` (37 entries) searched linearly. These are already sorted; use `binary_search()` or `phf` perfect hash set. (`pois.rs:93-157, 219-233`)

- [ ] **Projection transcendentals called billions of times** — `project_e7` calls tan, cos, ln per node. Use a lookup table for latitude projection (180K entries for 0.001-degree steps) with linear interpolation, or polynomial approximation. (`geometry.rs:103-118`)

- [ ] **`merge_same_attr_geometries` per-tile HashMap** — Clones and sorts tags for every feature per tile, allocates Vec for hash key. Hash tags in-place or pre-sort during insertion. (`mvt.rs:454-517`)

## Performance: Parallelism & I/O (Medium Impact)

- [ ] **PBF callback is single-threaded** — Tag matching and tag collection happen in the serial `for_each_pipelined` callback. Move tag matching and collection into rayon batch processing. (`pipeline.rs:305`)

- [ ] **Ocean processing: parallel collect then serial push** — `par_iter` collects into `Vec<Vec<SortRecord>>`, then pushes serially. Each rayon worker could flush to a thread-local sort chunk file directly. (`ocean.rs:180-203`)

- [ ] **Compression level tradeoff** — Level 6 is used; level 3-4 would be noticeably faster with ~5% larger output. Make configurable. (`pipeline.rs:1239`)

- [ ] **Relation tag String cloning** — Every relation's tags are cloned from `&str` to `String` because PBF borrows don't survive the batch boundary. Use string interning or buffer raw PBF bytes. ~14M relations * ~10 tags * ~30 bytes = ~4 GB. (`pipeline.rs:666-668`)

- [ ] **`boundary_way_coords.push(merc.clone())` duplicates geometry** — Clones full projected geometry for boundary relations. Store indices into `member_ways` instead. (`pipeline.rs:654`)

- [ ] **Sort chunk write buffer too small** — Default BufWriter (8 KB) for chunk writes. Use `BufWriter::with_capacity(1 << 20, file)` (1 MB). (`sort.rs:158-159`)

- [ ] **MVT value interning uses SipHash** — `DefaultHasher` is slower than needed for non-adversarial input. Use `FxHasher` or `ahash`. (`mvt.rs:366-379`)

- [ ] **Rayon alternatives for slice-based parallelism** — Wild linker discussion
  ([davidlattimore/wild#1072](https://github.com/davidlattimore/wild/discussions/1072)) surveys
  the landscape. Key options:
  - **paralight** (v0.0.8) — lightweight, targets slice/mut-slice parallelism. Can run on top of
    rayon's thread pool via `RayonThreadPool::new_global` (no extra threads). Has proper
    `try_for_each_init` that inits once per thread (rayon inits once per work item). Only needs
    `&` not `&mut` for the rayon backend. Limitation: no scopes, no graph algorithms, no recursive
    parallelism. Max `u32::MAX` elements.
  - **orx-parallel** — has `using()` API for guaranteed per-thread init. No thread pool yet
    (spawns threads per pipeline), on roadmap. No scopes/graph support.
  - **chili** — low-level, only provides `join`. A rayon fork (`par-iter`) builds par_iter on top
    of it. Uses lazy scheduling (less overhead for fine-grained work).
  - **forte** — experimental, rayon-like API with lazy scheduling. Supports spawn, join, scopes,
    scoped spawns. No par_iter or par_bridge yet.
  - **spindle** — built on rayon, optimised for small tasks. Very early.

  Wild's `thread_local` crate trick is also relevant: wrap per-thread state in
  `thread_local::ThreadLocal` and `.get_or()` inside rayon closures to guarantee one init per
  thread. Simple and works today without switching libraries.

  Not a current bottleneck — hotpath shows workers are starved by serial I/O, not slow at
  processing. But worth evaluating if we move more work into parallel batches (e.g. moving tag
  matching off the serial PBF callback).

- [ ] **NodeIndex madvise for planet scale** — MADV_SEQUENTIAL tried (6724e0a) and reverted
  (4e427b4, 2.3× regression). MADV_RANDOM tried (ca6f17e) and reverted (+65% regression on
  Denmark). Key finding: the node index is 102 GB even for Denmark (node IDs are global),
  but way→node lookups have locality so readahead helps. `advise_random()` method and
  `ram_bytes()` exist in node_index.rs but are not called. Needs planet-scale testing.
  Full investigation: `docs/madvise-investigation.md`. WayIndex MADV_RANDOM in
  `finish_writing()` is fine (relation lookups are truly non-sequential).

## Test Coverage Gaps

- [x] **ocean.rs** — rasterize_segment, point_in_polygon tested (10 tests)
- [x] **node_index.rs** — put/get, grow, sentinel, overwrite tested (6 tests)
- [x] **way_index.rs** — lifecycle, sentinel, overwrite tested (7 tests)
- [x] **pipeline.rs emit functions** — sort key/wire format decode roundtrips, cascading simplification, zoom-dependent attrs (7 tests)
- [x] **pmtiles_writer.rs** — end-to-end write_to header validation (1 test)
- [ ] **No integration test for PMTiles output validity** — No test verifies generated PMTiles can be read back and tiles decoded correctly (beyond header check).
