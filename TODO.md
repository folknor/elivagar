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

## Potential bugs (2026-03-03 audit)

### Wire format truncation risks

- [ ] `geom_cmds.len() as u16` — silent truncation for geometries with >65K commands.
  - A polygon ring with ~32K vertices would overflow. Unlikely but no guard.
  - Add `debug_assert!(geom_cmds.len() <= u16::MAX as usize)`.
  - Ref: `wire_format.rs:297`.
- [ ] `filtered_count as u8` — silent truncation above 255 attributes.
  - Currently unreachable (schema has ~48 keys), but fragile.
  - Ref: `wire_format.rs:236`.
- [ ] String length `as u16` — guarded only by `debug_assert` (stripped in release).
  - Malformed PBF with >64KB tag values would silently corrupt the sort record stream.
  - Ref: `wire_format.rs:253-254, 263-264`.

### MVT key/value index overflow

- [ ] `LayerBuilder` key/value interning uses `u16` indices via `as u16` truncation.
  - >65535 unique keys or values per tile wraps to 0 → wrong references.
  - Unreachable with current schema but unguarded.
  - Ref: `mvt.rs:155, 167, 187`.

### PMTiles writer

- [ ] `tile_id_to_zxy` overflows at z>=31 — debug panic, release wrong result.
  - `n * n * 4` overflows u64 at z=31. Guard evaluates after the computation.
  - Unreachable (max_zoom=14) but function is `pub`.
  - Ref: `pmtiles_writer.rs:736-743`.
- [ ] `encode_tile_id_column` u64 subtraction underflow on out-of-order entries.
  - Only `debug_assert` guards ordering; release mode silently corrupts directory.
  - Ref: `pmtiles_writer.rs:604`.
- [ ] `data.len() as u32` truncation for tiles >4GB.
  - Unreachable in practice but unguarded.
  - Ref: `pmtiles_writer.rs:209`.

### Way index / node index

- [ ] `read_varint` unbounded shift — panics in debug on corrupt data (shift >31).
  - Add shift guard (cap at 28, bail on overflow).
  - Ref: `way_index.rs:43-55`.
- [ ] `read_varint` / `decode_way` — no bounds checking on `*pos` vs data length.
  - Truncated/corrupt `way_data.bin` causes uncontrolled panic.
  - Ref: `way_index.rs:47, 73-96`.
- [ ] `encode_way` i32 delta subtraction can overflow in debug mode.
  - Unreachable for valid E7 coordinates but no guard.
  - Ref: `way_index.rs:65-66`.
- [ ] `SortedNodeStore::get` / `SortedNodeStoreReader::get` — no negative node_id guard.
  - Cast to u64 produces huge group_id → `None` (benign), but inconsistent with flat index.
  - Ref: `node_index.rs:842, 895`.

### Inspect

- [ ] `print_metadata_json` byte-index string slicing panics on non-ASCII metadata.
  - Only affects third-party PMTiles files with non-ASCII layer names.
  - Ref: `inspect.rs:157-183`.

### Ocean

- [ ] Inconsistent ring orientation logic after Y-flip coordinate transform.
  - Line 169: `signed_area >= 0.0` = outer. Line 178: `signed_area < 0.0` = outer.
  - Internally contradictory but benign: water-polygons-split-3857 has only single-part
    polygons so `w == 0` always catches outers.
  - Ref: `ocean.rs:169, 178`.
- [ ] Missing `.max(0.0)` guard on `_max` tile coordinates before f64-to-u32 cast.
  - Safe on modern Rust (saturating cast to 0) but inconsistent with `_min` guards.
  - Ref: `ocean.rs:220, 222, 440, 442`.

## Smells (2026-03-03 audit)

- [ ] `VmHWM` per-phase RSS values are monotonically non-decreasing (meaningless per-phase).
  - `peak_rss_kb()` reads `VmHWM` which is process-lifetime HWM, never decreases.
  - `ocean_rss_kb` is always >= `phase12_rss_kb`, etc.
  - Consider switching to `VmRSS` snapshots or documenting the limitation.
  - Ref: `pipeline.rs:297, 349, 372, 379`.
- [ ] `from_dir` / `resume` cleanup stops at first gap in chunk file numbering.
  - If a chunk file is missing, all subsequent chunks are silently ignored/not cleaned.
  - Ref: `sort.rs:324-333, 108-117`.
- [ ] `append_geometry` defensive bounds check produces silently malformed MVT.
  - On truncated input, pushes command header with wrong param count then breaks.
  - Ref: `mvt.rs:462-464`.
- [x] Orphan inner rings silently assigned to first polygon in `pair_rings`.
  - When inner ring's first vertex isn't inside any outer ring, falls back to `polygons[0]`.
  - Fix: promote orphan inners to outer rings (shells), matching Planetiler.
  - Ref: `multipolygon.rs:169`.
- [x] `ref_cols` counts bytes, not characters — wrong for non-Latin road refs.
  - Fix: changed `str::len` to `s.chars().count()`.
  - Ref: `streets.rs:201`.
- [x] `capital=2` not handled for national capitals (only `capital=yes` and `capital=4`).
  - All three Shortbread implementations (Planetiler, Tilemaker, elivagar) miss it — consistent gap.
  - Added code comment documenting the gap. Not fixing since it's rarely used in practice.
  - Ref: `boundaries.rs:95-96`.
- [ ] `has_name` doesn't filter `name=””` — features with empty names match label layers.
  - `name_attrs` correctly omits them, so only wasted sort record space.
  - Ref: `shortbread/mod.rs:364-366`.

## Test coverage gaps (2026-03-03 audit)

### High priority

- [ ] `mvt.rs`: `merge_same_attr_geometries` — complex sort+scan+in-place merge+tombstoning, zero tests.
  - A bug here produces visually wrong tiles (merged features with wrong geometry or lost features).
- [ ] `ocean.rs`: `emit_ocean_polygon` — scanline fill algorithm (~180 lines), zero tests.
  - Handles boundary tile rasterization, gap filling, land mask filtering, zoom iteration.
  - A bug produces missing or duplicate ocean tiles globally.
- [ ] `wire_format.rs`: interned kind value (type=4) encode/decode roundtrip, zero tests.
  - The `KIND_VALUES` short-circuit path is used for every “kind” attribute.

### Medium priority

- [ ] `pipeline.rs`: `emit_multipolygon_feature` (~200 lines) — zero tests.
  - Multipolygon emission with inner rings, interior tile detection, per-zoom clipping.
- [ ] `pipeline.rs`: checkpoint save/load roundtrip — zero tests.
  - Incorrect checkpoint data corrupts `--skip-to` resume (see B3/B4).
- [ ] `main.rs`: `parse_byte_size` edge cases — zero tests.
  - Budget parsing from CLI (e.g., “256M”, “1G”). Wrong parsing → OOM or degraded perf.
- [ ] `mvt.rs`: `append_geometry` — cursor re-encoding for concatenated multi-geometries, zero tests.
  - ClosePath cursor reset handling is error-prone.
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
