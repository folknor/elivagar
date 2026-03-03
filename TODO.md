# elivagar TODO

## Active priorities

1. [ ] Scale validation: run Europe full pipeline (locations-on-ways path).
   - Ref: Planet scale milestone below.
2. [ ] Scale validation: run planet full pipeline when hardware is available.
3. [ ] PMTiles dedup correctness hardening.
   - Current dedup uses hash+len without byte-compare.
   - Probability is ~2^-81 per pair (SipHash-64 + length match) — negligible, but failure
     mode is silent wrong tile content with no detection mechanism.
   - Ref: `pmtiles_writer.rs:196`.

## Bugs (2026-03-03 audit)

### B1. `natural=wetland` silently dropped from land layer
- **Severity:** bug (data loss — affects real OSM data)
- [x] Fix early-return in `land_match` that prevents fallthrough to `wetland` tag check.
  - `land_match` does `if let Some(v) = tags.get(“natural”) { return land_match_natural(v); }`.
  - `land_match_natural` has no arm for `”wetland”` → returns `None` → function returns `None`.
  - The `wetland` tag check on lines 37-39 is never reached when `natural=wetland` is present.
  - All 5 wetland subtypes (bog, marsh, swamp, string_bog, wet_meadow) are unreachable
    under standard OSM tagging (`natural=wetland` + `wetland=<subtype>`).
  - Fix: fall through on `None` instead of returning.
  - Ref: `land.rs:34-36`.

### B2. `maritime=no` treated as maritime boundary
- **Severity:** bug (wrong attribute on output features)
- [x] Fix `tags.has(“maritime”)` to `tags.has_value(“maritime”, “yes”)`.
  - `has()` returns true for any value including `”no”`.
  - Boundaries explicitly tagged `maritime=no` get `maritime: true` in output.
  - Ref: `boundaries.rs:27`.

### B3. Sort chunk count saved before `finish()` — breaks `--skip-to sort`
- **Severity:** bug (breaks the integrity check added in `c1f7da2`)
- [x] Move `save_sort_chunk_count` to after `sw.finish()`.
  - `finish()` flushes the final in-memory chunk, incrementing count by 1.
  - The checkpoint records N, but disk has N+1 chunks → spurious mismatch error.
  - Ref: `pipeline.rs:359-360`.

### B4. `--skip-to ocean` loses unflushed PBF records
- **Severity:** bug (silent data loss on checkpoint resume)
- [x] Flush `SortWriter` buffer before saving checkpoint.
  - `save_checkpoint` records `sw.chunk_count()` (flushed chunks only).
  - Records still in the in-memory buffer (up to ~1GB) are lost on resume.
  - Ref: `pipeline.rs:298`.

### B5. Multipolygon relations missing address matching
- **Severity:** potential-bug (missing features)
- [x] Add `land::match_addresses_centroid(tags, out)` to `match_multipolygon`.
  - `match_closed_way` calls it (line 258), `match_multipolygon` does not (lines 277-294).
  - Complex building multipolygons with `addr:housenumber` won't produce address features.
  - Ref: `shortbread/mod.rs:277-294`.

### B6. `is_poi_element` overly broad — drops addresses for unrecognized POI-key values
- **Severity:** potential-bug (missing features)
- [x] Tighten `is_poi_element` to check specific values, not just key presence.
  - Checks for key *presence* (`amenity`, `shop`, `office`, etc.) but `pois_match`
    only handles *specific* values. Elements with unrecognized values (e.g.,
    `office=company`, `amenity=parking_entrance`) are excluded from addresses
    AND don't appear as POIs — their address info is lost entirely.
  - Ref: `land.rs:172-178`.

### B7. Early-return pattern in `land_match` blocks cross-tag matching
- **Severity:** smell (root cause of B1, also affects other tag combos)
- [x] Restructured `land_match` and `water_polygon_match` to fall through on `None`.
  - An element with `landuse=military` + `leisure=park` only checks `landuse` (unrecognized),
    returns `None`, and misses the `leisure=park` match.
  - Same pattern exists in `water_polygon_match` (`water.rs:12-36`).
  - Ref: `land.rs:28-39`, `water.rs:12-36`.

## Potential bugs (2026-03-03 audit) — all resolved

### Wire format truncation risks

- [x] `geom_cmds.len() as u16` — clamped with `.min(u16::MAX)`.
- [x] `filtered_count as u8` — clamped with `.min(u8::MAX)`.
- [x] String length `as u16` — replaced `debug_assert` with `.min()` clamping + truncated write.

### MVT key/value index overflow

- [x] `LayerBuilder` key/value interning — saturated at `u16::MAX`.

### PMTiles writer

- [x] `tile_id_to_zxy` — moved `z >= 31` guard before overflow-prone computation.
- [x] `encode_tile_id_column` — changed to `saturating_sub`.
- [x] `data.len() as u32` — added early error return for tiles >4GB.

### Way index / node index

- [x] `read_varint` — added bounds check on `*pos` and shift cap at 28.
- [x] `decode_way` — added bounds checking, capacity cap, `saturating_add` for deltas.
- [x] `encode_way` — changed to `wrapping_sub` for delta computation.
- [x] `SortedNodeStore::get` / `SortedNodeStoreReader::get` — added negative node_id early return.

### Inspect

- [x] `print_metadata_json` — clamped chunk_end to char boundary for non-ASCII safety.

### Ocean

- [x] Ring orientation inconsistency — fixed line 178 to match line 169 convention (`>= 0.0`).
- [x] Missing `.max(0.0)` on `_max` tile coordinates — added to all four locations.

## Smells (2026-03-03 audit) — all resolved

- [x] `VmHWM` per-phase RSS — documented as intentional cumulative HWM in `peak_rss_kb()` comment.
- [x] `from_dir` / `resume` cleanup — now scans past 10 consecutive gaps to catch stale chunks.
- [x] `append_geometry` truncated input — patches command header with actual count after loop.
- [x] Orphan inner rings — promoted to outer rings (shells), matching Planetiler.
- [x] `ref_cols` bytes vs characters — changed to `s.chars().count()`.
- [x] `capital=2` — documented as consistent gap across all Shortbread implementations.
- [x] `has_name` empty string — changed to `tags.get(“name”).is_some_and(|v| !v.is_empty())`.

## Test coverage gaps (2026-03-03 audit)

### High priority

- [x] `mvt.rs`: `merge_same_attr_geometries` — 7 tests added (d11e9e9).
- [ ] `ocean.rs`: `emit_ocean_polygon` — scanline fill algorithm (~180 lines), zero tests.
  - Handles boundary tile rasterization, gap filling, land mask filtering, zoom iteration.
  - A bug produces missing or duplicate ocean tiles globally.
- [x] `wire_format.rs`: interned kind value (type=4) encode/decode roundtrip — 3 tests added (d11e9e9).

### Medium priority

- [ ] `pipeline.rs`: `emit_multipolygon_feature` (~200 lines) — zero tests.
  - Multipolygon emission with inner rings, interior tile detection, per-zoom clipping.
- [x] `pipeline.rs`: checkpoint save/load roundtrip — 5 tests added (d11e9e9).
- [x] `main.rs`: `parse_byte_size` edge cases — 7 tests added (d11e9e9).
- [x] `mvt.rs`: `append_geometry` — 4 tests added (d11e9e9).
- [ ] `geometry.rs`: `for_each_zoom_simplified_multi` — cascading simplification with inner rings, zero tests.

### Low priority

- [ ] `inspect.rs`: entire module untested (read-only diagnostic tool).
- [ ] `sort.rs`: `SortWriter::resume` and `adopt_chunk_files` (checkpoint/resume features).
- [ ] `pois.rs`: individual match functions (`pois_match_shop`, `pois_match_tourism`, etc.) —
  covered by YAML spec test but no targeted edge case tests.

## Previously completed

### Code review findings (2026-03-03)

- [x] Add chunk-count integrity check for `--skip-to sort` (has bug B3 — count saved too early).
- [x] Fix clippy errors and warnings.
- [x] Extract shared “emit feature → sort record” helper in `pipeline.rs`.
- [x] Unify point/centroid matcher bodies in `pois.rs` and `transport.rs`.
- [x] Measure relation block buffering RSS impact on Denmark + Germany (no impact found).
- [x] Flat index guardrails (enforce, cap, CLI override, integration tests, docs).

### Refactoring opportunities

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
