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
6. [x] Flat index guardrail: CLI/config surface for controlled override.
   - User-facing behavior: sensible default safety on, with explicit override for expert/CI scenarios.
   - UX requirement: warning states that override may cause severe IO/memory degradation.
   - Delivered: `--allow-unsafe-flat-index` and `ELIVAGAR_ALLOW_UNSAFE_FLAT_INDEX=1`;
     CLI flag takes precedence in intent (effective behavior is logical OR).
7. [x] Flat index guardrail: integration tests for rejection and allow paths.
   - Tests: sorted large input allowed; unsorted small input allowed; unsorted large input rejected with stable error text; override path works.
   - Success criterion: deterministic failures before heavy work starts.
8. [x] Flat index guardrail: docs update for operators.
   - Update README/notes with a short “why this fails early” section and copy-paste remediation commands.
   - Include explicit guidance for 32 GB vs 64 GB hosts.
9. [ ] PMTiles dedup correctness hardening.
   - Current dedup uses hash+len without byte-compare.
   - Probability is ~2^-81 per pair (SipHash-64 + length match) — negligible, but failure
     mode is silent wrong tile content with no detection mechanism.
   - Ref: `pmtiles_writer.rs:196`.

## Code review findings (2026-03-03)

Items from external code review, triaged by severity.

### Medium: stale sort chunks in `--skip-to sort`

- [ ] Add chunk-count integrity check for `--skip-to sort`.
  - `--skip-to sort` reads chunks via `SortReader::from_dir()` with no validation.
    If old chunks survive a failed `remove_dir_all`, they get silently merged.
  - Fix: save total chunk count (PBF + ocean) to checkpoint before sort phase;
    validate on `--skip-to sort` that discovered count matches expected.
  - Ref: `pipeline.rs:275`, `sort.rs:318`.

### Low: robustness

- [x] Fix clippy errors and warnings (collapsible-if, cast_possible_wrap, unused import,
  unused assignment, dead code, doc continuation).
  - Fixed: 2026-03-03.

### Refactoring opportunities

- [ ] Extract shared "emit feature → sort record" helper in `pipeline.rs`.
  - 6+ near-identical 4-line blocks: encode_feature_data_with_attrs → make_sort_key → push → count.
  - Ref: `pipeline.rs:955`, `pipeline.rs:1567`, `pipeline.rs:1617`, `pipeline.rs:1691`,
    `pipeline.rs:1821`, `pipeline.rs:1942`.
- [ ] Unify point/centroid matcher bodies in `pois.rs` and `transport.rs`.
  - Each pair differs only in `GeomExpect::Point` vs `GeomExpect::PolygonPointOnSurface`.
  - Ref: `pois.rs:19`/`pois.rs:32`, `transport.rs:66`/`transport.rs:86`.
- [ ] Relation-geometry scratch-based decode/project (if profiling warrants).
  - `way_index.get()` allocates fresh `Vec<(i32, i32)>` per call; caller allocates
    another `Vec<Point>` for projection. A decode-into-scratch API could cut alloc pressure.
  - Only worth doing if relation processing shows up as a hotpath bottleneck.
  - Ref: `pipeline.rs:1285`, `way_index.rs:470`.

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
