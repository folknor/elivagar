# elivagar TODO

## Active priorities

1. [ ] Scale validation: run Europe full pipeline (locations-on-ways path).
2. [ ] Scale validation: run planet full pipeline when hardware is available.
3. [ ] PMTiles dedup correctness hardening.
   - Current dedup uses hash+len without byte-compare.
   - Probability is ~2^-81 per pair (SipHash-64 + length match) — negligible, but failure
     mode is silent wrong tile content with no detection mechanism.
   - Ref: `pmtiles_writer.rs:196`.

## Test coverage gaps

- [ ] `ocean.rs`: `emit_ocean_polygon` — scanline fill, needs integration test with shapefile.
- [ ] `pipeline.rs`: `emit_multipolygon_feature` — glue code, needs full pipeline context.
- [ ] `inspect.rs`: untested read-only diagnostic tool.
- [ ] `sort.rs`: `SortWriter::resume` / `adopt_chunk_files` — indirectly tested via checkpoint tests.

## Refactoring opportunities

- [ ] Relation-geometry scratch-based decode/project (if profiling warrants).
  - `way_index.get()` allocates fresh `Vec<(i32, i32)>` per call; caller allocates
    another `Vec<Point>` for projection. A decode-into-scratch API could cut alloc pressure.
  - Only worth doing if relation processing shows up as a hotpath bottleneck.
  - Ref: `pipeline.rs:1285`, `way_index.rs:470`.

## Planet scale milestones

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

## Recently completed

- 2026-03-03 code audit: 7 bugs fixed (wetland, maritime, checkpoint flush, multipolygon
  addresses, POI exclusion, tag fallthrough), defensive hardening across wire format, MVT,
  PMTiles, way/node index, inspect, ocean. 25+ new tests added. Spec deviations fixed
  (ref_cols, orphan rings). All potential bugs and smells resolved.
- Memory optimization phases P1-P5 complete.
- Locations-on-ways pipeline (eliminates node store).
- Allocation hotspot reduction passes.
- Way index finalize/load memory work.

## Notes index

- `notes/north-america-memory-plan.md`
- `notes/north-america-hotpath-alloc-2026-03-03.md`
- `notes/way-budget-locations-on-ways.md`
