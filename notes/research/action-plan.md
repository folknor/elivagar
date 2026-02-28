# Planet-Scale Action Plan

Date: 2026-02-28
Source: 41 distilled findings from 7 box investigations (see `distilled-findings.md`)

Seven tiers, ordered by "do this first because it unblocks or de-risks everything after it."

---

## Tier 0 — Trivials (< 1 hour, zero risk)

All one-line or few-line changes. No benchmarking needed.

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

## Tier 1 — Sort payload reduction (1-2 days, highest leverage)

Reduces planet sort I/O from ~240 GB to ~177 GB. Already designed in TODO.

| # | Finding | Change | Planet savings |
|---|---------|--------|----------------|
| 1 | F18 | Key string → u8 key_id | ~44 GB |
| 2 | F20 | Remove osm_id (or make optional) | ~19 GB |

These also reduce peak memory during sort (smaller chunks = more headroom for node store).
Directly alleviates F1 by proxy.

## Tier 2 — Safety guardrails (half day)

| # | Finding | Change |
|---|---------|--------|
| 1 | F36 | PBF size guard on flat fallback — abort with clear message |
| 2 | F38 | Make `into_reader()` diagnostic scan optional/skip in release |

No performance impact. Prevents catastrophic planet-scale failure modes.

## Tier 3 — Geometry allocation reduction (1-2 days)

Targets the 5.8 GB `for_each_zoom_simplified` alloc number.

| # | Finding | Change | Denmark savings |
|---|---------|--------|-----------------|
| 1 | F26 | `SimplifySingleScratch` — eliminate cascade copy | ~2 GB |
| 2 | F27 | `clip_linestring_into` buffer-reuse variant | ~1.5 GB |
| 3 | F28 | `to_tile_coords_into` in multipolygon emission | per-tile allocs eliminated |
| 4 | F35 | Way reversal in-place (`.reverse()`) | minor |

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

Tiers 0-2 are "do before anything else" — they're either free, or they're safety, or they
directly reduce the memory pressure that makes planet runs risky. Tier 1 in particular buys
~63 GB of sort I/O reduction which translates to real RAM headroom.

After Tiers 0-2, benchmark a North America run (~17 GB PBF) and see where you actually stand
before committing to Tier 7 architectural work.

## Milestone: North America gate

After completing Tiers 0-2, run `brokkr bench self --pbf north-america.osm.pbf` on a ≥32 GB
machine. This validates:
- Sort payload reduction actually delivers projected I/O savings
- Memory headroom is sufficient for a mid-scale dataset
- Flat fallback guard fires correctly on unsorted PBFs

If North America succeeds cleanly, proceed with Tiers 3-6 and then attempt Europe (~28 GB).
If it OOMs or regresses, escalate Tier 7 (node store architecture) immediately.
