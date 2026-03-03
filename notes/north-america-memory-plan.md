# North America Memory Plan - Consolidated Status (2026-03-03)

This document is now a status reference (not a forward-only plan).  
Detailed, live task tracking remains in `TODO.md`.

## Objective

Keep North America locations-on-ways runs reliable on 32 GB class hosts while preserving throughput.

## What Has Shipped

1. Memory budgeting controls and instrumentation landed.
2. WayIndex out-of-core path landed (large memory reduction vs in-memory index path).
3. North America full pipeline succeeded with locations-on-ways input.
4. Hotpath allocation reduction work completed in way/relation geometry paths:
   - large cumulative allocation drops in top way-path allocators
   - meaningful Germany wall/RSS improvements from relation-path optimization passes
5. Regressing relation fast path (`pair_rings` single-outer special-case) was reverted.

## Key Results Snapshot

- North America locations full run has already been demonstrated in a stable memory envelope.
- Germany runs delivered the strongest reproducible micro-optimization signal:
  - relation-path pass 1: notable wall and RSS win
  - relation-path pass 2: small additional wall win, RSS neutral
- Further micro-tuning appears to be in diminishing-returns territory without larger algorithmic changes.

For benchmark UUIDs/commit-level deltas, use `TODO.md` and:

- `notes/north-america-hotpath-alloc-2026-03-03.md`
- `notes/way-budget-locations-on-ways.md`

## Open Memory Risks (Still Worth Measuring)

1. Relation block buffering RSS impact at larger scales (North America/Europe).
2. Europe and planet scale validation milestones (hardware-gated).
3. Flat node-index safety guardrails (operational protection, not hotpath speed).

## Relation Buffering Measurement (2026-03-03)

Method:

- Added `relation_blocks_drop_rss_kb` metric (VmRSS delta before/after releasing buffered relation blocks).
- Ran `brokkr bench self` with locations variant on Denmark and Germany.

Results:

- Denmark: `relation_blocks_buffered=6`, `relation_blocks_drop_rss_kb=0`, `peak_rss_kb=2691584`
- Germany: `relation_blocks_buffered=111`, `relation_blocks_drop_rss_kb=0`, `peak_rss_kb=8616648`

Interpretation:

- At Denmark/Germany scale, relation block buffering does not show a measurable VmRSS release event, and is unlikely to be a dominant peak RSS source.
- Keep this as an open item only for larger datasets where relation buffering volume may be materially higher.

## Decision Guidance

1. Keep North America reruns for milestone verification only (expensive/noisy for tight micro-bench decisions).
2. Use Germany as default optimization dataset for iterative work.
3. Prioritize peak RSS risk reduction and scale validation over additional allocator micro-optimizations.

## Source of Truth

- Primary tracker: `TODO.md`
- Alloc hotpath context: `notes/north-america-hotpath-alloc-2026-03-03.md`
- Way-budget calibration context: `notes/way-budget-locations-on-ways.md`
