# elivagar TODO

## Memory optimization ✓ COMPLETE (P1-P5)

All five phases implemented. Planet-scale finalization peak: ~240 MB → ~30 MB.
Assemble batch peak: 1.4 GB → 35 MB. In-flight controls are byte-budgeted.
Sort chunk budget configurable via `--sort-budget`.

| Phase | Commit | Summary |
|-------|--------|---------|
| P1: Byte-budgeted in-flight | `c88cfc0`, `862d0b7` | Way/rel/assemble byte budgets, clone elimination |
| P2: Stream relation outputs | `a8627be` | Ocean-style parallel chunk flushing for relations |
| P3: Tighten assemble phase | `2442343` | MVT buffer reuse, should_emit flag |
| P4: Configurable sort chunk | `b82afae` | `--sort-budget` CLI flag, ocean byte accounting fix |
| P5: PMTiles directory streaming | `d11744e` | O(1) finalization, never materializes full Vec\<DirEntry\> |

Remaining (contingent — only if planet runs still OOM):
- Way index offset loading: finalize step reads full offsets file. Ref: `way_index.rs:197`
- Low-memory mode profiles: lower sort/relation/assemble budgets for tight 64 GB runs

### Instrumentation (always-on, `3a729ab`)
- Per-phase RSS: `phase12_rss_kb`, `ocean_rss_kb`, `sort_rss_kb`, `assemble_rss_kb`, `peak_rss_kb`
- In-flight HWM: `max_way_inflight_bytes`, `max_rel_batch_bytes`, `max_assemble_batch_bytes`
- Phase timings, feature/tile counts, node store stats, `sort_chunks`

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

## Next Steps (Current)

1. [x] **Locations-on-ways correctness gate (Denmark semantic parity).**
   Validate locations-on-ways output against standard path output on Denmark.
   2026-03-03 run status:
   - `--no-ocean` Denmark outputs are **not** byte-identical (`cmp`/SHA mismatch),
     which is acceptable.
   - `brokkr compare-tiles --sample 1000000` shows sampled/common tile content parity
     (matching per-layer feature and command totals across all sampled tiles).
   Gate decision: semantic tile parity is the correctness criterion.
   Ref: `notes/locations-on-ways.md` (migrated), `notes/north-america-memory-plan.md`.
2. [ ] **Allocation hotspot reduction (way/relation geometry paths).**
   Prioritize scratch-buffer reuse and `_into` API coverage in:
   `process_raw_way`, simplify, clip, and emit paths.
   2026-03-03 run status (commit `ff87135`, UUID `dcc4102f`, compared to `fb0c5e3`):
   - Allocation drops:
     - `process_raw_way`: `224.9 GB -> 102.8 GB` (`-54.3%`)
     - `for_each_zoom_simplified`: `148.4 GB -> 59.9 GB` (`-59.6%`)
     - `emit_polygon_feature`: `85.2 GB -> 30.8 GB` (`-63.8%`)
     - `emit_line_feature`: `63.2 GB -> 29.2 GB` (`-53.8%`)
   - Hotpath wall regressed (`900190 ms -> 942449 ms`, `+4.7%`) and peak RSS increased
     (`23346 MB -> 25389 MB`, `+8.7%`), so this needs confirmation with `bench self`
     timing (hotpath alloc mode is intrusive).
   Bench confirmation (`bench self` North America locations):
   - Baseline `604f7f0e` (`1e22411`) vs new `b47b95e3` (`ff87135`):
     - wall: `454436 ms -> 456151 ms` (`+0.38%`, effectively flat)
     - `phase12_ms`: `270966 -> 276162` (`+1.9%`)
     - `phase4_ms`: `167229 -> 163136` (`-2.4%`)
     - peak RSS: `21317.5 MB -> 20909.1 MB` (`-1.9%`)
   Relation-path pass 1 (multipolygon row preclip copy elimination via buffer swaps):
   - Germany hotpath alloc baseline `c4c87193` (`ae6bb39`) vs new `412b8a84` (`86b9fe8`):
     - wall: `259725 ms -> 249269 ms` (`-4.0%`)
     - peak RSS: `10727.3 MB -> 7237.5 MB` (`-32.5%`)
     - `phase12_ms`: `221028 -> 213801` (`-3.3%`)
   - Allocation totals in top relation functions were mostly flat or mixed in hotpath accounting
     (`process_prepared_relation_into` slightly up), so this likely reduces transient copy pressure
     more than cumulative allocation volume.
   Relation-path pass 2 (multipolygon reverse-copy allocation removal in join paths):
   - Germany hotpath alloc `412b8a84` (`86b9fe8`) vs `c1072381` (`1d270e5`):
     - wall: `249269 ms -> 248094 ms` (`-0.5%`)
     - peak RSS: `7237.5 MB -> 7240.7 MB` (`~flat`)
     - `phase12_ms`: `213801 -> 210703` (`-1.4%`)
   - Net: small throughput gain, negligible RSS change.
   Relation-path pass 3 (single-outer `pair_rings` fast path) — REJECTED.
   - Germany hotpath alloc `c1072381` (`1d270e5`) vs `aa134b22` (`9b7b046`):
     - wall regressed `248094 ms -> 257714 ms` (`+3.9%`) with no RSS benefit.
   - Reverted in commit `4382006`.
   Ref: `notes/north-america-hotpath-alloc-2026-03-03.md`.
3. [ ] **Measure relation block buffering RSS impact.**
   Record Denmark + North America deltas and decide whether further work is needed.
   Ref: existing Code TODO item, `notes/north-america-memory-plan.md`.
4. [ ] **Scale validation milestones.**
   Run Europe then planet once steps 1-3 are in a good state.
   Ref: Planet scale Step 5/6 below.

## Planet scale

- [x] Step 1: `pbfhogg node-stats` tool
- [x] Step 2: Validate compression on Germany/Norway/Japan (worst case 72%, planet fits under 64 GB)
- [x] Step 3: SortedNodeStore compression — 75% ratio (planet: 51 GB, fits in 64 GB)
- [x] Step 4: Full pipeline on North America (18.2 GB locations PBF, plantasjen, `90ad2ef`)
  462.6s total (283s pbf, 15s ocean, 0.5s sort, 164s assemble), 19.4 GB RSS, 12.4 GB output
  510M features, 12.5M unique tiles. LocationsOnWays mode (no node store).
- [x] Step 4b: Hotpath alloc profile on North America locations-on-ways — identify top allocators at scale
  Completed 2026-03-03 on `fb0c5e3` (UUID `2acb944c`).
  See `notes/north-america-hotpath-alloc-2026-03-03.md` for top allocators and next optimization targets.
- [x] Way-budget calibration investigation for locations-on-ways completed (2026-03-03).
  See `notes/way-budget-locations-on-ways.md` for Denmark/Germany/North America sweep results and recommendations.
- [ ] Locations-on-ways validation: Denmark output must match standard path byte-for-byte.
- [ ] Step 5: Full pipeline on Europe (~28 GB) — needs ≥64 GB RAM
- [ ] Step 6: Planet (~75 GB) — needs ≥64 GB RAM hardware

## Performance

### Baselines

Plantasjen Denmark (`605a1a5`): 14.2s (9.2s pbf, 1.4s ocean, 0.5s sort, 2.5s assemble).
dm6 Denmark (`2db9494`): 21.2s (13.7s pbf, 2.6s ocean, 0.5s sort, 2.9s assemble).

### Tier 2 (safety guardrails)

**Sort payload width** (Boxes 3, 6) — ✓ DONE (`a6b1977`, `aa5cdff`).
Key+kind interning saves ~63 GB planet sort I/O.

**Assemble pipeline backpressure** (Boxes 1, 7) — DISMISSED.
Batching provides double-buffering. Encoder is the bottleneck; deeper queues can't help.

**Flat node-index safety** (Box 2) — TODO.
Unsorted PBFs silently create 96 GB sparse files. Needs: (1) PBF size guard — abort if >1 GB
without sorted flag, (2) hard cap on flat index size (16 GB).

**Node-store cache sizing** (Box 2) — Low priority.
Bump CACHE_ENTRIES 4→8 is cheap (~10-15s planet wall savings). Software prefetching is the real
win but complex. Measure on North America first.

**Tile fanout and clipping** (Box 4) — Multiple paths identified:
1. `to_tile_coords_into` in multipolygon path (low-hanging fruit, 4 allocs/tile eliminated)
2. Interior tile detection for large polygons (skip S-H for tiles fully inside outer ring)
3. Inner ring bbox prefilter (O(1) bbox test before per-ring clip)
4. Row pre-clip for non-ocean multipolygons (already done for ocean path)
5. `clip_linestring_into` buffer reuse
6. `ring_refs` SmallVec

**Compression** (Box 7) — Per-zoom compression levels (~20 lines, -20-30% assemble time).
`--compression-level 3` already works for iteration builds. zstd blocked by client-side support.
Suggested thresholds: z0-8 → level 9, z9-12 → configured, z13-14 → capped at 3 (86.5% of tiles).

**Ocean split policy** (Box 5) — DISMISSED. z8 split with 500-vertex threshold is well-tuned.

**PMTiles dedup cap** (Box 8) — LOW IMPACT.
1M cap fills at z11; z12-z14 tiles are almost always unique. Raising to 5-10M is cheap
insurance (~160-320 MB). Add telemetry when cap is hit.

## Bugs

- [ ] **Way pipeline condvar deadlock on oversized blocks.** When a single PBF block's
  estimated cost (`estimate_raw_ways_bytes * WAY_OUTPUT_MULTIPLIER`) exceeds `way_budget`
  (default 128 MB), the condvar predicate `bytes + block_cost > way_budget` is permanently
  true even when `count == 0`. With no in-flight tasks, no `notify_one()` fires and the
  worker thread sleeps forever. Discovered on North America locations-on-ways (18 GB, 209M
  ways) — long highways/rivers/coastlines produce blocks where ways average 200+ coords,
  pushing `block_cost` over 128 MB. Germany (5 GB) never hits this because ways are shorter.
  **Fixed:** added `count > 0` guard so at least one task always proceeds. The underlying
  issue is that `WAY_OUTPUT_MULTIPLIER = 10` makes the effective per-block budget only 12.8 MB
  of raw way data — tight for locations-on-ways blocks. Consider whether the multiplier is
  still appropriate now that locations-on-ways skips the node store (the multiplier was
  calibrated for the node-lookup path where output expands significantly).

## Code TODOs

- [ ] **Dedup correctness is probabilistic:** Hash match + length match without byte validation.
  Ref: `pmtiles_writer.rs:196`. Collision risk ~2^-81 per pair — negligible but non-zero.

- [x] **Way index finalize reads full offsets file:** Fixed — streaming BufReader, no intermediate
  `Vec<u8>`. Transient peak halved (28.8 GB → 14.4 GB at planet scale). Further optimization:
  `WayEntry` has identical layout to the file (i64 + u64 LE, 16 bytes, no padding — static
  assert verified), so the raw bytes could be reinterpreted directly via bytemuck/transmute,
  eliminating the parse loop and the 14.4 GB `Vec<WayEntry>` entirely (just sort in-place on
  the raw allocation). Only matters if planet finalize is still a bottleneck.

- [ ] **Flat node-index safety guardrails:** PBF size guard + hard cap on flat index.
  See investigation summary above.

- [ ] **Measure relation block buffering impact:** Relation blocks are now buffered in memory
  during PBF reading (fix for non-strict block ordering in locations-on-ways PBFs). Measure
  peak RSS delta on Denmark and North America — relation blocks should be a tiny fraction of
  total data but worth verifying at scale.

- [x] **Ocean `fill_data` cloning:** Partially addressed — `SortRecord.data` changed from `Vec<u8>`
  to `Box<[u8]>` (`e779759`). Clones now allocate exactly `len` bytes (no excess capacity),
  struct shrunk 32→24 bytes. RSS -7.4% on Denmark. Full elimination (Arc/sentinel) would
  require changing `SortRecord.data` to an enum or shared type — not worth the complexity
  unless planet alloc profiles show fill clones as a top contributor.
