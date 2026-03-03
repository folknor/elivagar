# elivagar TODO

## Active priorities

1. [x] Measure relation block buffering RSS impact on Denmark + Germany.
   - 2026-03-03 (`brokkr bench self`, locations variant):
     - Denmark: `relation_blocks_buffered=6`, `relation_blocks_drop_rss_kb=0`, `peak_rss_kb=2691584`
     - Germany: `relation_blocks_buffered=111`, `relation_blocks_drop_rss_kb=0`, `peak_rss_kb=8616648`
   - Result: no observable VmRSS drop when buffered relation blocks are released; no evidence this is a material RSS driver at these scales.
   - Ref: `notes/north-america-memory-plan.md`.
2. [ ] Scale validation: run Europe full pipeline (locations-on-ways path).
   - Ref: Planet scale milestone below.
3. [ ] Scale validation: run planet full pipeline when hardware is available.
4. [ ] Flat node-index safety guardrails.
   - Add PBF-size guard for unsorted input and hard cap on flat index size.
5. [ ] PMTiles dedup correctness hardening.
   - Current dedup uses hash+len without byte-compare.
   - Ref: `pmtiles_writer.rs:196`.

## Planet scale milestones

- [x] Step 1: `pbfhogg node-stats` tool
- [x] Step 2: Compression validation on Germany/Norway/Japan
- [x] Step 3: SortedNodeStore compression
- [x] Step 4: North America full pipeline (locations-on-ways)
- [x] Step 4b: North America hotpath alloc profiling
- [x] Way-budget calibration for locations-on-ways
- [ ] Step 5: Europe full pipeline (~28 GB, needs >=64 GB RAM)
- [ ] Step 6: Planet full pipeline (~75 GB, needs >=64 GB RAM)

## Release prep

- [ ] Publish `pbfhogg` to crates.io (currently path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Quality

- [ ] Visual verification (tracked in nidhogg TODO)

## Website

- [ ] Write a one-page project website (what it does, benchmarks, usage, repo link)
- [ ] Host via GitHub Pages

## Recently completed (kept brief)

- Memory optimization phases P1-P5 complete (byte budgets, streaming relation output,
  assemble tightening, configurable sort budget, PMTiles directory streaming).
- Locations-on-ways Denmark correctness gate is semantic parity (not byte-identical output).
- Allocation hotspot reduction passes landed; regressing single-outer `pair_rings` fast path reverted.
- Way index finalize/load memory work landed.

## Notes index

- `notes/north-america-memory-plan.md`
- `notes/north-america-hotpath-alloc-2026-03-03.md`
- `notes/way-budget-locations-on-ways.md`
