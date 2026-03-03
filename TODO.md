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
4. [x] Flat index guardrail: enforce unsorted-input size gate before creating `nodes.idx`.
   - User-facing behavior: fail fast when header is not `Sort.Type_then_ID` and input PBF is above threshold (target: `>1 GB`), instead of allowing pathological sparse-index runs.
   - Error UX: include exact input size and concrete fixes (`--force-sorted` when true, or sort first with `pbfhogg sort`; `osmium sort` as fallback).
   - Code context: pipeline preflight near current unsorted node-store branch in `src/pipeline.rs`.
5. [x] Flat index guardrail: hard-cap flat node-index file size.
   - User-facing behavior: abort with actionable error once projected/actual index growth crosses cap (target: `16 GB`) to prevent machine-wide thrash.
   - Error UX: explain cap rationale and recovery path (sorted PBF or locations-on-ways input).
   - Code context: `NodeIndex::put` growth path (`MAX_FLAT_INDEX_SIZE = 16 GB`) with unit test coverage.
6. [ ] Flat index guardrail: CLI/config surface for controlled override.
   - User-facing behavior: sensible default safety on, with explicit override for expert/CI scenarios.
   - UX requirement: warning must state that override may cause severe IO/memory degradation.
   - Deliverable: define flag/env naming and precedence, document in `--help`.
7. [ ] Flat index guardrail: integration tests for rejection and allow paths.
   - Tests: sorted large input allowed; unsorted small input allowed; unsorted large input rejected with stable error text; override path works.
   - Success criterion: deterministic failures before heavy work starts.
8. [ ] Flat index guardrail: docs update for operators.
   - Update README/notes with a short “why this fails early” section and copy-paste remediation commands.
   - Include explicit guidance for 32 GB vs 64 GB hosts.
9. [ ] PMTiles dedup correctness hardening.
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
