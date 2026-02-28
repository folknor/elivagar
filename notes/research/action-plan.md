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

## Tier 3 — Geometry allocation reduction (1-2 days)

| # | Finding | Change | Denmark savings | Status |
|---|---------|--------|-----------------|--------|
| 1 | F26 | `SimplifySingleScratch` via thread_local | 1.3 GB (23.6%) | ✅ `c02f951` |
| 2 | F27 | `clip_linestring_into` buffer-reuse variant | ~1.5 GB | |
| 3 | F28 | `to_tile_coords_into` in multipolygon emission | per-tile allocs eliminated | |
| 4 | F35 | Way reversal in-place (`.reverse()`) | minor | |

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

## Tier 4 — Assembly worker persistence (1 day)

| # | Finding | Change | Planet savings |
|---|---------|--------|----------------|
| 1 | F3 | Persist compressor + pools across batches (thread-local or worker struct) | ~288 GB churn |
| 2 | F32 | Pool LayerBuilders per worker, `.clear()` instead of drop | ~500M alloc cycles |

These two are the same refactor — replace `map_init` with persistent per-worker state.

## Tier 5 — Geometry CPU (2-3 days)

Targets the 23.4s multipolygon clipping cost (105% of Denmark wall time).

| # | Finding | Change | Impact |
|---|---------|--------|--------|
| 1 | F11 | Inner ring bbox prefilter | O(1) reject, low effort |
| 2 | F10 | Interior tile detection (4-corner PIP) | skip S-H for ~298K tiles on Germany z14 |
| 3 | F14 | Row pre-clipping for PBF polygons (port from ocean) | ~rows_covered reduction |

Do F11 first — cheapest and helps most relations. F10 and F14 are independent of each other.

## Tier 6 — Compression strategy (half day)

| # | Finding | Change | Planet savings |
|---|---------|--------|----------------|
| 1 | F9 | Per-zoom compression levels (z13-z14 at level 3) | ~150-400s CPU |

Already designed in TODO. ~20 lines. Do after Tier 4 since assembly worker persistence
changes the same code.

## Tier 7 — Architectural / needs measurement

High-risk or needs benchmarking before deciding:

| # | Finding | Question |
|---|---------|----------|
| F22 | Geometry varint encoding | Saves ~115 GB sort I/O but loses memcpy decode. Need to benchmark decode cost. |
| F1 | SortedNodeStore spilling/streaming | The real planet blocker. After Tiers 0-6, re-evaluate: does sort payload reduction + trivial memory wins buy enough headroom, or do we still need architectural changes? |
| F16 | `find_chunk_in_blob` offset table | 254 chunks/group at planet. Linear scan may matter. Need planet-scale measurement. |
| F7 | Streaming ocean polygon parse | 1-5 GB at planet. Only matters if F1 headroom is still tight. |

---

## Sequencing rationale

Tiers 0-2 complete, Tier 3 F26 done. ~63 GB sort I/O reduction + ~1.3 GB geometry alloc
reduction banked (Denmark). Japan total thread alloc: 211 GB.

Next: remaining Tier 3 items (F27, F28, F35), then North America gate.

## Milestone: North America gate

After completing Tiers 0-2, run `brokkr bench self --pbf north-america.osm.pbf` on a ≥32 GB
machine. This validates:
- Sort payload reduction actually delivers projected I/O savings
- Memory headroom is sufficient for a mid-scale dataset
- Flat fallback guard fires correctly on unsorted PBFs

If North America succeeds cleanly, proceed with Tiers 3-6 and then attempt Europe (~28 GB).
If it OOMs or regresses, escalate Tier 7 (node store architecture) immediately.
