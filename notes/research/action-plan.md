# Planet-Scale Action Plan

Date: 2026-02-28
Source: 41 distilled findings from 7 box investigations (see `distilled-findings.md`)

Seven tiers, ordered by "do this first because it unblocks or de-risks everything after it."

---

## Tier 0 — Trivials (< 1 hour, zero risk) ✅ DONE

All 11 changes committed as `f41e433`. Benchmarked on dm6: Denmark -2.4%, Japan -2.6%.

| # | Finding | Change | Planet savings |
|---|---------|--------|----------------|
| 1 | F41 | `CACHE_ENTRIES` 4→8 | ~10-15s wall |
| 2 | F21 | `cmd_count` u32→u16 | 4.8 GB sort I/O |
| 3 | F33 | `encode_tile_with` buffer 4KB→64KB | 200-400M reallocs eliminated |
| 4 | F23 | Output BufWriter 8KB→1MB | fewer syscalls on 3GB write |
| 5 | F24 | Dir temp BufWriter 8KB→64KB | fewer syscalls |
| 6 | F5 | Drop `entries` Vec after `build_directories` | 120 MB freed |
| 7 | F6 | Clear dedup HashMap at finalization start | 50 MB freed |
| 8 | F31 | `current_coords` `.clear()` not re-alloc | 25M alloc cycles eliminated |
| 9 | F29 | `ring_refs` → `SmallVec<[_; 4]>` | millions of small allocs |
| 10 | F39 | Reuse Compressor for leaf directories | minor |
| 11 | F8 | Fix `buffer_bytes` accounting (+32B per record) | accurate 1GB target |

## Tier 1 — Sort payload reduction (1-2 days, highest leverage) ✅ DONE

| # | Finding | Change | Planet savings | Status |
|---|---------|--------|----------------|--------|
| 1 | F18 | Key string → u8 key_id | ~44 GB | ✅ `a6b1977` + fix `6de3e1a` |
| 2 | F18b | Kind value → u8 value_id (129-entry table) | ~19 GB | ✅ `aa5cdff` |
| 3 | F20 | Remove osm_id (or make optional) | ~19 GB | ❌ Dismissed — osm_id flows to MVT Feature.id |

F18+F18b combined: ~63 GB sort I/O reduction at planet scale. Benchmarked on dm6:
Denmark -1.5%, Norway -4.3%, Japan -4.4% vs `61a85b0` baseline.

These also reduce peak memory during sort (smaller chunks = more headroom for node store).
Directly alleviates F1 by proxy.

## Tier 2 — Safety guardrails (half day) ✅ DONE

Committed as `24b9b20`.

| # | Finding | Change | Status |
|---|---------|--------|--------|
| 1 | F36 | PBF size guard on flat fallback — abort if >1 GB unsorted | ✅ |
| 2 | F38 | Diagnostic scan gated behind `ELIVAGAR_NODE_STATS=1` | ✅ |

Cheap stats (`node_store_nodes`, `node_store_groups`) always emitted after timing — no
benchmark contamination. Expensive scan only runs with env var, acceptable for hotpath runs.

## Tier 3 — Geometry allocation reduction (1-2 days) ✅ DONE

Cumulative vs `aa5cdff` baseline on Denmark (dm6):

| Function | Before | After | Change |
|---|---|---|---|
| `for_each_zoom_simplified` | 5.5 GB | 4.0 GB | -27.3% |
| `process_raw_way` | 7.8 GB | 6.3 GB | -19.2% |
| `emit_polygon_feature` | 3.8 GB | 2.9 GB | -23.7% |
| `emit_multipolygon_feature` | 276 MB | 208 MB | -24.6% |
| `main` total | 4.2 GB | 4.1 GB | -2.4% |
| Bench | 19944 ms | 20089 ms | +0.7% (noise) |

| # | Finding | Change | Denmark savings | Status |
|---|---------|--------|-----------------|--------|
| 1 | F26 | `SimplifySingleScratch` via thread_local | 1.3 GB (23.6%) | ✅ `c02f951` |
| 2 | F27 | `for_each_clipped_segment` thread_local + callback | 200 MB (15.4%) | ✅ `bd19649` |
| 3 | F28 | `all_rings` pool with `to_tile_coords_into` | 68 MB (24.6%) | ✅ `bd19649` |
| 4 | F35 | Way reversal in-place | minor, skipped | ❌ Not worth it (220 MB total in assemble) |

### F26 notes

First attempt used rayon `map_init` to thread scratch buffers through `process_raw_way`.
Reduced per-function alloc by 14.5% but **regressed wall-clock by 5.4%** on Japan (74.3→78.3s).
Root cause: `map_init` increased total thread allocations by 1.2 GB despite reducing per-function
alloc — rayon scheduling overhead outweighed the savings. mimalloc already handles small
alloc/free cycles efficiently, so avoiding them didn't help.

Second attempt used `thread_local!` inside `for_each_zoom_simplified` directly. No signature
changes, no `map_init`, plain `map`. Results on dm6:

| Dataset | Bench | `for_each_zoom_simplified` alloc | Total thread alloc |
|---------|-------|----------------------------------|-------------------|
| Denmark | 19748 ms (-1.0% vs baseline) | 4.2 GB (-23.6%) | 47.9 GB (-2.8%) |
| Japan | 75718 ms (+1.9% vs baseline) | 23.9 GB | 211.3 GB |

Lesson: prefer `thread_local!` over `map_init` for per-worker scratch in rayon. The TLS
lookup cost is negligible; `map_init` changes rayon's work distribution and adds overhead.

## Tier 4 — Assembly worker persistence (1 day) ✅ DONE

Committed as `e0cabcb`. Replaced `map_init` with `thread_local!` in `encode_tile_batch`.
`LayerBuilder::prepare_for_reuse()` clears interning state while keeping HashMap capacity.

| # | Finding | Change | Status |
|---|---------|--------|--------|
| 1 | F3 | Persist compressor + pools across batches via thread_local | ✅ |
| 2 | F32 | Pool LayerBuilders per worker, `prepare_for_reuse()` instead of drop | ✅ |

Results on dm6 vs `bd19649` baseline:

| Metric | Before | After | Change |
|---|---|---|---|
| Denmark bench | 20089 ms | 19791 ms | -1.5% |
| Japan bench | 74705 ms | 74107 ms | -0.8% |
| `add_feature_to_layer` alloc (DK) | 4.0 GB | 153 MB | -96.3% |
| `merge_same_attr_geometries` alloc (DK) | 2.7 GB | 1.3 GB | -51.9% |
| Thread total alloc (JP) | 211.3 GB | 168.6 GB | -20.2% |

The HashMap bucket arrays survive across tiles — interning no longer needs to reallocate
on each tile. `add_feature_to_layer` dropped out of the top-15 allocators on Japan.

## Tier 5 — Geometry CPU (2-3 days) ✅ DONE

Targets the 23.4s multipolygon clipping cost (105% of Denmark wall time).

| # | Finding | Change | Impact | Status |
|---|---------|--------|--------|--------|
| 1 | F11 | Inner ring bbox prefilter | O(1) reject, low effort | ✅ `9867c38` |
| 2 | F10 | Interior tile detection (4-corner PIP) | skip S-H for ~298K tiles on Germany z14 | ✅ `83902cc` |
| 3 | F14 | Row pre-clipping for PBF polygons (port from ocean) | ~rows_covered reduction | ✅ `8ca350c` |

Results on dm6 vs `9867c38` baseline:

| Metric | Before | After | Change |
|---|---|---|---|
| Denmark bench | 20041 ms | 20044 ms | +0.0% |
| Japan bench | 72483 ms | 71392 ms | -1.5% |
| Germany bench | -- | 142076 ms | (first run, no baseline) |
| Output size (all) | unchanged | unchanged | +0.0% |

F10 uses single-pass O(n) combining 4-corner PIP with edge-bbox overlap check.
Interior tiles emit a pre-computed full-tile rectangle. F14 pre-clips polygons to each
row's Y-band before per-tile S-H clipping — reduces per-tile vertex count for large
polygons. Both optimizations scale with polygon size; Denmark/Japan too small to show
significant improvement. Germany (first run) at 142s establishes baseline for future
comparison.

## Tier 6 — Compression strategy (half day) ✅ DONE

Committed as `af902d9`. Per-zoom levels: z0-8 → level 9, z9-12 → configured (default 6),
z13-14 → capped at 3. `--compression-level` flag anchors the middle tier.

| # | Finding | Change | Status |
|---|---------|--------|--------|
| 1 | F9 | Per-zoom compression levels (z13-z14 at level 3) | ✅ |

Results on dm6 vs `e0cabcb` baseline:

| Metric | Before | After | Change |
|---|---|---|---|
| Denmark bench | 19791 ms | 19695 ms | -0.5% |
| Japan bench | 71992 ms | 72439 ms | +0.6% (noise) |
| Denmark output | 272.8 MB | 273.4 MB | +0.2% |
| Japan output | 1166.3 MB | 1168.2 MB | +0.2% |

Negligible output size increase (+0.2%). Full savings at planet scale where assemble phase
is a larger fraction of total wall time.

## Tier 7 — Way index compression ✅ DONE

Committed as `1f4955c`. Replaced sparse mmap'd way index with delta-varint compressed
in-memory structure. Coordinates encoded as first-coord-raw + zigzag-varint deltas.
Offset entries loaded into sorted Vec for binary search lookup.

| # | Finding | Change | Status |
|---|---------|--------|--------|
| 1 | Way index compression | Delta-varint encoding, sorted in-memory index | ✅ |

Results on dm6 vs `8ca350c` baseline:

| Metric | Before | After | Change |
|---|---|---|---|
| Denmark bench | 20044 ms | 15610 ms | -22.1% |
| Japan bench | 71392 ms | 65648 ms | -8.0% |
| Germany bench | 142076 ms | 122417 ms | -13.8% |
| Denmark output | 273.3 MB | 273.3 MB | +0.0% |
| Japan output | 1168.4 MB | 1168.1 MB | -0.0% |
| Germany output | 2494.5 MB | 2494.3 MB | -0.0% |

Surprising speed improvement even on small datasets — delta-varint decode from RAM is
faster than mmap page table overhead. Eliminates the NA blocker: projected ~15 GB way
index for NA (was 38 GB mmap'd), ~41 GB for planet (fits in 64 GB after node store drop).

### Remaining architectural items

| # | Finding | Question |
|---|---------|----------|
| F22 | Geometry varint encoding | Saves ~115 GB sort I/O but loses memcpy decode. Need to benchmark decode cost. |
| F1 | SortedNodeStore spilling/streaming | Resolved — compression keeps node store under 8.5 GB for NA. No longer the planet blocker. |
| F16 | `find_chunk_in_blob` offset table | 254 chunks/group at planet. Linear scan may matter. Need planet-scale measurement. |
| F7 | Streaming ocean polygon parse | 1-5 GB at planet. Only matters if memory headroom is tight. |

---

## Sequencing rationale

All tiers 0-7 complete. Total banked savings vs original baseline:
- ~63 GB sort I/O reduction (F18 + F18b)
- ~1.5 GB geometry alloc reduction (F26 + F27 + F28)
- Way index: mmap thrash eliminated, 22% wall-clock improvement on Denmark
- Per-zoom compression levels tuned

Next: NA gate re-test with compressed way index.

## Milestone: North America gate — FAILED (pre-compression)

Ran `brokkr bench self --dataset north-america --runs 1` on dm6 (32 GB RAM), commit `b5bc00e`.
Killed after ~2 hours with no completion in sight.

### What passed
- **SortedNodeStore: PASS** — 8.5 GB RSS, rock solid throughout. FOR compression works
  exactly as designed. Node store is no longer the planet blocker.
- **PBF phase completed** — 55 sort chunks written (55 × 870 MB = ~48 GB sort data),
  all within the first ~7 minutes of wall time.
- **No OOM** — 24 GB available throughout, no memory pressure from node store or sort chunks.

### What failed
- **Way index: 38 GB mmap'd** (20 GB way_data.bin + 17 GB way_offsets.bin) on a 32 GB
  machine. The way index stores resolved geometry for all ways so relations can look up
  member coordinates.
- **Relation processing thrashed** — after PBF phase, relation processing reads from the
  way index to resolve member geometries. With 38 GB mmap'd and 32 GB RAM, every way
  index lookup is a page fault. Process dropped to 10-14% CPU (I/O bound), stuck for
  2 hours with no new sort chunks emitted.
- **vmstat confirmed**: swap-in 57-96/s, 85-97% idle CPU. Classic mmap thrash pattern.

### Resolution
Way index compressed in `1f4955c`. Delta-varint encoding reduces 38 GB → ~15 GB (projected).
Loaded into RAM after node store drop. NA gate should be re-tested.
