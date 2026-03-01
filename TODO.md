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

## Planet scale

- [x] Step 1: `pbfhogg node-stats` tool
- [x] Step 2: Validate compression on Germany/Norway/Japan (worst case 72%, planet fits under 64 GB)
- [x] Step 3: SortedNodeStore compression — 75% ratio (planet: 51 GB, fits in 64 GB)
- [ ] Step 4: Full pipeline on North America (~17 GB) — needs ≥32 GB RAM
- [ ] Step 5: Full pipeline on Europe (~28 GB) — needs ≥64 GB RAM
- [ ] Step 6: Planet (~75 GB) — needs ≥64 GB RAM hardware

## Performance

### Baselines

Plantasjen Denmark (`605a1a5`): 14.2s (9.2s pbf, 1.4s ocean, 0.5s sort, 2.5s assemble).
dm6 Denmark (`2db9494`): 21.2s (13.7s pbf, 2.6s ocean, 0.5s sort, 2.9s assemble).

### Completed optimizations

- [x] Tier 0 — 11 trivial optimizations (`f41e433`). Denmark -2.4%, Japan -2.6%.
- [x] Tier 1, F18 — Key string → u8 key_id interning (`a6b1977`). ~44 GB planet savings.
- [x] Tier 1, F18b — Kind value → u8 value_id interning (`aa5cdff`). ~19 GB planet savings.
- [x] Tier 1, F20 — Dismissed: osm_id flows to MVT Feature.id.

### Next up: Tier 2 (safety guardrails), then Tier 3 (geometry alloc reduction)

### Investigation summaries (2026-02-28)

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

- [ ] **`find_chunk_in_blob` linear scan:** 254 chunks/group at planet vs 7 on Denmark. Consider
  offset table or binary search for planet-scale lookup.

- [ ] **Ocean `fill_data` cloning:** 10M-100M clones of identical ~50-byte data per planet run.
  Use Arc or sentinel to avoid per-tile copies.
