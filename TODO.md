# elivagar TODO

## Commit review backlog (after `f393448`, 35 commits)

### Tests only (12 commits) — reviewed
- [x] `3e99f9b` PMTiles layout and decode regression tests
- [x] `6a56464` Minzoom attribute filtering tests
- [x] `6063edd` Sort chunk resume/adopt regression tests
- [x] `b237a3c` Mapbox MVT fixture conformance tests (6 fixtures)
- [x] `143d583` Metadata verification edge-case tests
- [x] `5f65fd1` Ocean shapefile integration tests
- [x] `0d9366e` Multipolygon emission coverage tests
- [x] `5400bcc` Harden minzoom attribute filtering tests
- [x] `0550018` MLT tile model validation tests
- [x] `a1ff829` MLT geometry fixtures + roundtrip test
- [x] `3972789` MLT property fixtures + tests
- [x] `1691056` Verifier MVT geometry anomaly checks + tests

### Schema extensions (4 commits) — reviewed
- [x] `eab1cb4` EV charging station POIs
- [x] `a8d8f6b` `natural=cliff` line support
- [x] `9de9bf0` Peak/pass POIs with elevation normalization
- [x] `8653f93` Building height/levels attributes

### Geometry correctness (7 commits)
- [ ] `3aca1d6` Point-on-surface hole-aware for multipolygons
- [ ] `9c56786` Pre-quantization polygon ring validation
- [ ] `74b74d8` Post-simplification invalid ring guard
- [ ] `639def2` Deterministic multipolygon assembly
- [ ] `b08c360` Shared-node line simplification hardening
- [ ] `0de6fb1` Shared vertices in polygon simplification
- [ ] `031151b` Shared vertices in relation multipolygons

### Pipeline features (3 commits)
- [ ] `43e614f` Missing-ref diagnostics
- [ ] `19cd941` PMTiles metadata source provenance
- [ ] `4a7a92e` Tile size diagnostics

### Infrastructure (3 commits)
- [ ] `52d51e4` `--skip-to assemble` resume mode
- [ ] `244f4a9` Antimeridian-aware geometry wrapping
- [ ] `9831ca0` Refactor inspect to use shared PMTiles reader helpers

### MLT integration (6 commits)
- [ ] `f9269e1` Tile-format CLI plumbing
- [ ] `3ed8a65` Assembly encoding tile-format dispatch
- [ ] `312d3b8` MLT encoder scaffold + tile-model extraction
- [ ] `d848c71` PMTiles tile contract format-aware
- [ ] `cea838d` Inspect payload contract reporting + legacy fallback
- [ ] `1d3fd6d` Upstream mlt-core encoder integration

## Active priorities

1. [ ] Scale validation: run Europe full pipeline (locations-on-ways path).
2. [ ] Scale validation: run planet full pipeline when hardware is available.
3. [x] PMTiles dedup correctness hardening.
   - Dual-fingerprint dedup: two salted SipHash passes + length match (was single hash + length).
   - Bucketed dedup map: `HashMap<u64, Vec<...>>` prevents silent entry eviction on hash collisions.
   - Full observability via `DedupStats`: candidates, tiles_reused, bytes_saved,
     reject_len_mismatch, reject_fp_mismatch, insert_skipped_cap, hash_bucket_collisions.
   - Stats emitted as key=value metrics in pipeline output.
   - Tests: metrics accounting, cap overflow, fingerprint rejection via injected entries,
     bucket collision handling, length mismatch counting.
   - Follow-up regression hardening completed:
     - Tippecanoe #98-style PMTiles directory index OOB/off-by-one edge cases now covered
       in `pmtiles_reader` decode tests (truncated streams, invalid first-entry offset sentinel,
       contiguous offset overflow).
     - Tilemaker #794-style small-archive/root-directory-only layout now covered via
       root-only offset/layout tests plus root/leaf threshold boundary tests.
     - `inspect.rs` now has coverage for both root-only and leaf-directory archive layouts.

## Test coverage gaps

- [x] `ocean.rs`: `emit_ocean_polygon` — scanline fill, needs integration test with shapefile.
  Coastline correctness matters for regional extracts where the boundary cuts through
  ocean polygons (tilemaker #16).
- [ ] `ocean.rs` shapefile integration tests currently cover only a single simple polygon.
  Add multipart + inner-hole fixture cases to exercise ring assembly and clipping behavior.
- [ ] `ocean.rs` lacks malformed `.shp/.shx` negative-path tests in integration coverage.
  Add targeted broken-header/index/record cases to verify robust error handling.
- [x] `pipeline.rs`: `emit_multipolygon_feature` — glue code, needs full pipeline context.
  Large lakes/water bodies at low zoom are particularly vulnerable to simplification + clipping
  producing visible topology artifacts (tilemaker #191). Very large polygons ("monster polygons")
  also stress the clipping path specifically (tilemaker #607).
- [x] `pipeline_tests.rs` multipolygon coverage lacks invalid/degenerate inner-ring cases
  (too-short/self-intersecting holes). Add targeted negative-path tests for hole handling.
- [x] `pipeline_tests.rs` multipolygon coverage is mostly single-zoom assertions.
  Add multi-zoom simplification/retention checks to guard zoom-dependent behavior.
- [x] `inspect.rs`: read-only diagnostic tool tested on root-only and leaf-directory layouts,
  and migrated to use `pmtiles_reader.rs` shared helpers to reduce parsing duplication.
- [x] `inspect.rs`: strengthen tests beyond "no error" smoke coverage. Add output assertions
  for header/section reporting so formatting/field regressions are caught.
- [x] `inspect.rs`: add malformed metadata payload tests (e.g., non-gzip bytes with
  non-zero metadata length) to lock down graceful error-tolerant inspect behavior.
- [x] `inspect.rs`: add payload-contract fallback tests for cases where metadata is absent
  or unreadable, ensuring header-vs-metadata source reporting stays correct.
- [x] `sort.rs`: `SortWriter::resume` / `adopt_chunk_files` now directly tested.
  Coverage includes: checkpoint resume success, missing required chunk failure,
  stale leftover deletion on resume, empty checkpoint resume, and adopted chunk merge correctness.
- [x] Import targeted cases from Mapbox's `mvt_fixtures` corpus for `mvt.rs` conformance testing.
  Added fixture-backed conformance tests for canonical valid geometries:
  point/line/polygon + multipoint/multilinestring/multipolygon (fixture IDs 017-022).
- [x] `mvt.rs` fixture conformance currently covers canonical valid geometries only.
  Add targeted invalid/malformed fixture cases to lock down error handling behavior.
- [x] `mvt.rs` fixture parser helper assumes single-layer/single-feature fixtures.
  Generalize helper/assertions for future multi-layer and multi-feature fixture imports.
- [ ] Add parity tests for shared tile-model preparation used by MLT scaffolding to ensure
  layer/feature assembly stays consistent with MVT path as code evolves.
- [ ] `mlt.rs`: clarify `observed_type_count` semantics (currently saturates effectively at 2
  for mixed columns). Either track true distinct type cardinality or rename/reshape field
  to a boolean mixed-flag model with explicit tests/docs.
- [ ] Add MLT semantic roundtrip tests that compare decoded geometry/properties against
  source features, not only parse/decode success.
- [ ] MLT fixture roundtrip currently asserts geometry type only; add coordinate/ring-content
  equality checks to catch subtle geometry corruption that preserves type.
- [ ] Add explicit MLT size/perf guard checks (or benchmarks) for no-compression tile payloads
  to catch unintended regressions versus equivalent MVT tiles.
- [x] PMTiles metadata JSON validation: `elivagar verify` now validates metadata JSON
  structure and `vector_layers` schema. Added regression tests for malformed metadata payloads:
  invalid JSON, missing `vector_layers`, and invalid `vector_layers` entry schema.
  Remaining gap: fuzz testing for attribute/value edge cases that could produce malformed JSON
  (tippecanoe #181).
- [x] `verify.rs` geometry-anomaly hardening needs broader rule coverage tests:
  unknown command IDs, zero repeat counts, polygon MoveTo/ClosePath invariants, and
  absolute-coordinate limit checks.
- [x] `verify.rs` geometry anomaly checks: add seam-tile vs non-seam threshold tests
  to lock `MVT_DELTA_LIMIT_SEAM` and `MVT_DELTA_LIMIT` behavior.
- [x] `tests/pmtiles_roundtrip.rs` metadata corruption helper only supports in-place
  replacements that fit the existing metadata section. Add explicit oversize-replacement
  negative-path coverage for future corruption test scenarios.
- [x] `pmtiles_reader.rs`: add decode-directory malformed `tile_id` delta overflow test.
  Current hardening covers truncated columns and offset sentinel/overflow, but not cumulative
  tile-id delta overflow behavior.
- [x] Per-attribute minzoom filtering: `encode_attrs_bytes()` in wire_format.rs filters
  attributes by zoom. Verify with tests that per-attribute zoom gates actually take effect
  and don't leak attributes to wrong zoom levels (tilemaker #671).
- [x] `wire_format.rs` attr-count cap test checks `tags.len()==255` but not full stream
  consistency under >255 attrs. Add decode-boundary assertions for capped payload parsing.
- [x] `wire_format.rs`: add combined cap+minzoom interaction test (large attr list with
  mixed gated/ungated attrs) to lock down filtering behavior under truncation.
- [x] `wire_format.rs` minzoom tests currently assert some attributes by positional tag index.
  Make these assertions order-insensitive (key/value membership) so benign tag-order changes
  don't cause false regressions in filtering tests.
- [x] `sort.rs`: add resume coverage for `start_chunk=0` with pre-existing stale chunks
  present on disk. Current empty-checkpoint test uses an empty directory only.
- [x] `sort.rs`: add adopt-chunk negative-path tests (missing/corrupt adopted chunk files)
  to lock down merge-phase error surfacing behavior.

## Refactoring opportunities

- [x] Expand checkpoint/skip-to for faster profile iteration: now supports `--skip-to
  ocean`, `--skip-to sort`, and `--skip-to assemble`. Assemble mode reuses sorted chunks and
  re-runs MVT encoding with different profile settings to speed up Shortbread tuning on
  large extracts. Planetiler #1497/#1468 adds exactly this (reuse feature DB for post-
  processing iteration).
- [x] Add explicit `--skip-to assemble` control-flow coverage (CLI parse + pipeline behavior),
  including expected metric emission differences when phase3 is skipped.
- [x] Add `--skip-to assemble` negative-path tests for missing/stale chunk state to pin
  mode-specific failure/reporting behavior.
- [ ] Early simplification as memory pressure valve: simplify geometry during PBF processing
  when batch memory exceeds budget, rather than deferring all simplification to the assemble
  phase. Reduces peak RSS and sort chunk size. Different from feature dropping — this preserves
  all features but at lower fidelity. Tippecanoe (#38) moved simplification earlier for this
  reason. Would need per-zoom simplification tolerance available during PBF phase.
- [ ] Relation-geometry scratch-based decode/project (if profiling warrants).
  - `way_index.get()` allocates fresh `Vec<(i32, i32)>` per call; caller allocates
    another `Vec<Point>` for projection. A decode-into-scratch API could cut alloc pressure.
  - Only worth doing if relation processing shows up as a hotpath bottleneck.
  - R-tree spatial indexing for large polygons is another option if polygon memory becomes
    a hotspot (tilemaker #323).
  - Ref: `pipeline.rs:1285`, `way_index.rs:470`.

## Planet scale milestones

- [ ] Step 5: Europe full pipeline (~28 GB, needs >=64 GB RAM)
- [ ] Step 6: Planet full pipeline (~75 GB, needs >=64 GB RAM)

## Release prep

- [ ] Publish `pbfhogg` to crates.io (currently path dependency)
- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Tile output optimizations

- [ ] Line merging: merge connected LineStrings within same tile/layer into fewer features.
  Reduces tile size and renderer work. Biggest win for street-heavy urban tiles at planet
  scale. Planetiler's impl had bugs from merge → min-length filter interaction (#1330, #1174).
  Cap merged line vertex count to prevent pathological cases — tilemaker #248 uses 6000.
- [ ] Min polygon area filtering: drop sub-pixel polygons at low zoom levels.
  Planetiler applies min-pixel-size filtering to polygons and derived point features (#720).
  Tippecanoe (#160) buffers polygons outward before simplification to prevent small polygons
  from collapsing to zero area — an alternative to dropping them. Tippecanoe (#4) uses
  density instead of pure area for tiny features to avoid dropping visually important narrow
  segments (e.g. rivers, paths). Planetiler #1078 adds feature-size-in-meters utilities for
  scale-aware filtering. Also applies to inner rings: tilemaker #416 filters tiny inner rings
  by area threshold to reduce complexity without visual impact.
- [ ] Sort keys within layers: emit feature order hints (e.g. road importance) so renderers
  stack correctly without client-side sorting. Audit whether MapLibre depends on this (#323).
- [ ] Sort MVT attribute keys/values for better gzip compression. Reordering string/value
  tables so similar values are adjacent gives gzip better runs. Cheap optimization,
  potentially a few % output size reduction (tippecanoe #1).
- [ ] Feature dropping: iteratively drop least-important features from oversized tiles until
  they fit a size budget. Needed for dense urban areas at planet scale. Tippecanoe (#378)
  hit an infinite loop bug here — need a guaranteed convergence invariant (also #340, #45).
  Low-zoom dropping must be profile-aware to avoid overaggressive removal (tippecanoe #201).
  Prerequisite: tile size diagnostics (see below) to identify which tiles need dropping.
- [x] Tile size diagnostics: report per-tile size stats, flag oversized tiles, expose
  layer-level breakdowns. Needed for feature dropping tuning and profile regression triage
  (Planetiler #391).
- [x] Tile size diagnostics currently have helper-level ordering tests only.
  Add end-to-end metric emission assertions for `tile_bytes_*`, `tile_max_*`, and
  `oversize_top_*` summary lines on a controlled pipeline run.
- [x] Tile size diagnostics: add boundary-value tests at exact warn/severe thresholds
  (500KB and 1MB) to lock down classification semantics.
- [ ] Compressed sort chunks: gzip-compress temp sort files to reduce I/O during sort phase.
  Tippecanoe (#56) saw wins from this. Low priority since sort is already fast.
- [ ] Building merge at z13: Planetiler optionally unions adjacent buildings to reduce tile
  size in dense urban areas. Their benchmarks show ~37% planet runtime increase (expensive).
  JTS polygon union also hits robustness bugs on real data (Planetiler #700). Cheaper
  alternatives: simplify building geometry, drop small buildings at low zoom. Only worth
  pursuing if output tile size becomes a blocking issue.
  Ref: Planetiler `--building-merge-z13`, `notes/upstream-issues/planetiler-relevant.md`.

## Geometry correctness

- [x] Post-simplification ring validity: Douglas-Peucker can produce self-intersecting rings.
  Low risk but no safety net currently. Planetiler hit this with VW+smoothing (#1263, #1192).
  Tippecanoe (#164) is rewriting their polygon cleaner (replacing Wagyu) for robustness on
  messy/self-contradictory input geometry — same problem class. High pixel tolerance can also
  invalidate multipolygon topology specifically (Planetiler #496). Tilemaker #828 confirmed
  that DP-simplified polygons break MapLibre's earcut triangulation — concrete downstream
  failure mode for elivagar's current simplifier.
- [ ] Add explicit multipolygon-path tests for post-simplification invalid ring rejection
  (current regression coverage is stronger for single polygon emission than multipolygon).
- [x] Add boundary-policy tests for invalid ring guard at `z=14` cutoff
  (`z<14` reject behavior vs `z=14` behavior) to lock intended semantics.
- [x] Deterministic multipolygon assembly: `join_ways()` in multipolygon.rs uses
  `std::collections::HashMap` which has random iteration order. For well-formed multipolygons
  the two-pass algorithm converges, but broken/ambiguous ones can produce different ring
  assignments between runs. Replace with `FxHashMap` (already used in mvt.rs) or sort chains
  before joining (Planetiler #788).
- [x] `multipolygon.rs` determinism hardening follow-up: current `ring_sort_key`
  (bbox + ring length) is not unique, so tied rings can still preserve input-order
  differences. Use a canonical full-ring comparator/fingerprint for stable ordering.
- [x] Strengthen multipolygon determinism tests: compare full assembled ring coordinate
  sequences (or canonical fingerprints), not only coarse `ring_sort_key` equality.
- [x] Pre-quantization polygon validity: validate/repair polygon rings before snapping to tile
  grid coordinates. Invalid polygons that survive clipping+simplification can cascade into
  tile artifacts after integer quantization (Planetiler #566). Tilemaker #602 hit this as
  spikes/self-intersections introduced during coordinate scaling. The interplay between
  simplification and rounding specifically produces visible artifacts (Planetiler #324).
  Planetiler #1493 is actively exploring optimistic naive polygon snapping approaches.
- [ ] Pre-quantization ring-validity checks are O(n^2) intersection scans.
  Add perf/scale coverage for large rings to confirm acceptable overhead in worst-case geometry.
- [ ] Add borderline-valid ring coverage for pre-quantization validity checks
  (collinear segments, repeated vertices, near-touching edges) to avoid false positives.
- [x] Label points in polygon holes: `point_on_surface()` scans outer ring only and does not
  avoid placing points inside inner rings. Multipolygons with large holes could get label
  points in the wrong place (tippecanoe #62). Tilemaker #461 hit centroid exceptions on
  problematic geometry — need robust fallbacks for edge-case polygons. Also affects address
  labels: building polygons with courtyards can get housenumber placed inside the hole
  (Planetiler #237). Current algorithm is a simple 5-scan horizontal sweep; Planetiler #723
  implements pole of inaccessibility (max inscribed circle) which gives better results for
  irregular polygons but is more expensive.
- [ ] Add `point_on_surface_with_holes` coverage for multiple-hole and adjacent-hole layouts
  to validate interval-subtraction behavior in more complex multipolygons.
- [ ] Add explicit fallback-path tests for `point_on_surface_with_holes` when scan candidates
  fail and outer-only fallback lands inside a hole (current behavior returns `None`).
- [x] Shared-node simplification (line features): preserve block-local shared interior way
  nodes during DP simplification so common road/boundary junction vertices survive
  generalization. Implemented via block-local shared-node detection + required-vertex DP.
  Tippecanoe (#99) had bugs in shared-node preservation mode; this adds explicit coverage.
- [ ] Add end-to-end line-emission tests that assert pinned shared nodes survive
  zoom-level simplification in emitted geometry.
- [ ] Shared-node detection is block-local by design; add explicit coverage/docs for
  cross-block junction behavior limits so regression expectations are clear.
- [ ] Shared-edge simplification (adjacent polygons): independent simplification of polygons
  that share an edge can still produce slivers/gaps along shared boundaries
  (tippecanoe #105).
  Context: this is still a geometry-correctness issue (not just visual polish).
  Current behavior simplifies each polygon/ring independently; when neighboring
  features share an edge, DP can choose different kept vertices on each side.
  That creates tiny gaps/overlaps ("seams"), especially at low zoom and along
  long administrative/landuse boundaries.
  Implemented mitigation so far: preserve detected shared vertices during
  simplification for (1) line features, (2) closed-way polygons, and
  (3) relation-derived multipolygons. This reduces catastrophic drift but does
  not guarantee edge-identical output between neighboring polygons.
- [ ] Add explicit relation-derived multipolygon shared-vertex preservation tests
  (closed-way polygon coverage exists; relation path needs direct regression checks).
- [x] Strengthen shared-vertex simplification assertions to validate specific pinned
  vertex identity retention, not only increased encoded command counts.
- [x] Relation multipolygon shared-vertex preservation tests still infer success mostly via
  command-count deltas; add direct emitted-geometry vertex identity assertions.
- [x] Add precision-boundary tests for `relation_shared_vertex_keys` quantization to verify
  near-equal coordinate handling and avoid accidental shared-key collisions.
  Remaining work: topology-aware lockstep simplification across polygon groups:
  detect shared edge chains, simplify each shared chain once, and reuse that
  exact chain in all incident polygons before rebuilding rings.
  Suggested scope split:
  Phase A: boundary/admin polygons only (highest visibility, narrower schema).
  Phase B: land/water polygons and general multipolygons.
  Acceptance checks: no shared-edge divergence after simplification within a
  tolerance threshold, no ring-validity regressions, and no large planet-scale
  runtime/RSS regression.
- [x] Antimeridian handling: added dateline-aware unwrapping/wrapping for geometry crossing
  180°/-180° so line/polygon/multipolygon emission does not take the "long way" around.
  Implemented wrapped-copy emission on seam-crossing bboxes and wrapped land-mask marking.
  Also hardened data-bounds checkpoint handling for antimeridian-spanning extracts to avoid
  invalid narrow/wide bbox metadata behavior.
- [x] Antimeridian crossing detection currently uses a wide-span heuristic (`lon_span > 180°`)
  that can misclassify wide but non-crossing extracts as crossing. Refine detection to avoid
  forced world-wide x-bounds on non-crossing bboxes.
- [x] Add end-to-end antimeridian emission tests verifying wrapped-copy behavior does not
  introduce duplicate or missing features on seam-crossing geometries.
- [x] Add explicit tests for wide-but-non-crossing longitude spans to lock expected
  antimeridian detection behavior.

## Schema extensions (beyond Shortbread 1.0)

- [x] Building height/levels attributes: emit `height`, `min_height`, `building:levels` from
  OSM tags on building polygons. Shortbread 1.0 only specifies `dummy=1` for buildings, but
  the data is in PBFs and useful for 2.5D/3D rendering in nidhogg. Well-tagged in European
  urban areas. MLT's 2.5D basemap support makes this more relevant if we adopt that format.
  Also consider `building:part=yes` sub-components (tilemaker #692) — needed for proper 3D
  rendering of complex buildings with towers, wings, etc.
- [ ] Add building measurement parser coverage for more real-world formats
  (`24m`, `24 meters`, locale comma decimals, semicolon variants) to pin normalization behavior.
- [ ] Add explicit `building:levels` policy tests (zero, negative, fractional edge cases)
  to lock intended acceptance/rejection semantics.
- [x] Elevation attribute on peaks/passes: elivagar's POI layer does not match `natural=peak`
  or `natural=volcano`, and does not emit the `ele` (elevation) tag. Investigate whether
  Shortbread 1.0 specifies these, and add with proper unit conversion — OSM `ele` is
  nominally meters but sometimes tagged in feet (Planetiler #224 had a unit-conversion bug
  here). Useful for topographic labeling in nidhogg.
- [ ] `pois.rs` elevation parser needs broader format coverage tests
  (`1000m`, `1,234 m`, `1000;1200`, negatives, malformed strings) to pin normalization behavior.
- [ ] Add explicit POI tests for `natural=volcano` and `mountain_pass=yes` branches
  in peak/pass matching logic.
- [ ] Lake/river centerlines: generate centerline geometries for elongated water features to
  improve label placement. Current `point_on_surface()` works for compact polygons but
  produces poor label positions for long/thin features like fjords or narrow lakes. Planetiler
  #1137 hit bugs in their centerline stage. Investigate algorithmic approach (medial axis,
  Voronoi, or skeleton-based) and whether this should be a post-processing step or integrated
  into the assemble phase.
- [ ] Wikidata multilingual name enrichment: OSM features often carry `wikidata=Q*` tags
  linking to Wikidata entities with names in dozens of languages. Currently elivagar only
  emits `name:*` tags present directly in the PBF, which limits language coverage — many
  features only have the local-language name tagged. Investigate fetching the Wikidata JSON
  dump (or a pre-filtered names extract) and joining on `wikidata` tag during PBF processing
  to expand label language coverage. Planetiler does this via a background worker that
  pre-fetches `wikidata_names.json` (#1290 hit reliability issues). Design questions: offline
  pre-join in pbfhogg vs runtime join in elivagar, storage format, which languages to include.
- [ ] Natural Earth low-zoom layers: use Natural Earth vector data (1:10m/1:50m/1:110m) for
  z0-5 features like country boundaries, lakes, and land polygons instead of simplifying
  full-resolution OSM geometry. Currently elivagar uses the water-polygons-split-3857
  shapefile for ocean (similar pattern). Natural Earth data is cleaner at low zoom and avoids
  expensive simplification of detailed OSM geometry. Planetiler uses this extensively for its
  low-zoom layers (#431). Implementation: add a Natural Earth ingest phase (similar to ocean
  phase) that reads shapefiles and emits features to the sort stream.
- [x] Cliff/landform line features: `natural=cliff` is a linear feature not in Shortbread 1.0.
  Elivagar's land layer only matches polygon natural features (bare_rock, beach, etc.). Cliffs
  are well-tagged in mountainous areas and useful for topographic rendering. Would need a new
  landform line layer or extension to an existing layer. Tilemaker #265 hit rendering issues
  with cliff classification.
- [ ] Add explicit closed-way coverage for `natural=cliff` line matching (current regression
  test is open-way focused despite both open/closed wiring).
- [ ] Add tag-conflict/priority tests for `natural=cliff` alongside other matching line tags
  to lock expected classification behavior.
- [x] EV charging stations as POIs: `amenity=charging_station` is not in elivagar's POI list
  because Shortbread 1.0 doesn't include it, but EV charging infrastructure is increasingly
  important for map consumers. OSM has good coverage in Europe. Investigate adding as a
  schema extension alongside other beyond-Shortbread POI types (Planetiler #765 hit a bug
  where charging stations were silently dropped).
- [ ] Add POI coverage for non-node charging stations (way/area geometries) to lock
  geometry-path behavior for `amenity=charging_station`.
- [ ] Add richer tag-matrix tests for charging stations (additional tags present) to ensure
  classification remains stable and address suppression behavior stays correct.

## Future architecture

- [ ] Investigate OGC TileMatrixSet v2 / non-Mercator tiling schemes. Nidhogg's API surface
  is expanding, and consumers may need non-WebMercator projections (EPSG:4326, polar, etc.).
  Tippecanoe has an open request for this (tippecanoe #286).
- [x] MLT (MapLibre Tile) output format baseline integration.
  Implemented:
  - `elivagar run --tile-format mvt|mlt` CLI selection and pipeline wiring.
  - Real upstream `mlt-core` encoder integration in `src/mlt.rs` (not a local stub).
  - PMTiles tile contract made format-aware (`tile_payload_format`, `tile_compression`) with
    metadata + inspect fallback reporting for legacy archives.
  - Committed MLT geometry fixtures + roundtrip decode tests (`mlt_core::parse_layers` +
    decode path) covering point/line/polygon and multi-geometries.
  Remaining follow-ups:
  - [ ] Add MLT feature-order controls and evaluate default behavior:
    `--no-mlt-feature-sort` equivalent semantics and compression/size impact.
  - [ ] Add MLT polygon tessellation mode:
    `--pretessellate` equivalent for polygon-only layers and benchmark render/size tradeoffs.
  - [ ] Add MLT compression/encoding tuning flags (e.g. shared dictionary mode),
    with benchmark-guided defaults.
  - [ ] End-to-end client compatibility validation in nidhogg/MapLibre and rollout guidance.

## Quality

- [ ] Visual verification (tracked in nidhogg TODO)
- [x] Missing-ref diagnostics: when ways reference missing nodes or relations reference missing
  ways (common in regional extracts), elivagar silently skips them. Should emit a summary
  count so users know data is incomplete rather than wondering why features are missing
  (tilemaker #332). Also applies to nested relations: `prepare_relation()` skips non-way
  members (`MemberId::Relation`), so relation-members-of-relations are silently dropped
  (tilemaker #638). Correct for multipolygon relations (way-only per OSM spec) but worth
  counting.
- [ ] Missing-ref diagnostics currently have unit coverage for counter accumulation only.
  Add end-to-end pipeline-path tests that assert emitted summary metrics from real way/relation
  processing with missing references.
- [ ] Missing-ref diagnostics: add explicit skip/resume behavior coverage (e.g. `--skip-to sort`)
  to document/lock whether metrics are omitted or printed as zero when phase12 is skipped.
- [x] Source provenance in PMTiles metadata: include OSM replication timestamp and source
  PBF filename in the metadata JSON so consumers can trace data freshness. Currently only
  emits schema info (name, format, layers). Planetiler #120 added this for MBTiles; same
  applies to PMTiles.
- [x] `pmtiles_writer.rs`: add metadata JSON escape-path tests for `source_pbf` values with
  quotes, backslashes, and control characters.
- [x] Add end-to-end coverage for provenance propagation through `phase_assemble` (source
  filename + replication timestamp) rather than only `build_metadata` unit tests.
- [ ] Add end-to-end pipeline assertions that selected `tile_format` produces consistent
  PMTiles header + metadata tile contract fields in generated archives.

## Website

- [ ] Write a one-page project website (what it does, benchmarks, usage, repo link)
- [ ] Host via GitHub Pages

## Notes index

- `notes/north-america-memory-plan.md`
- `notes/north-america-hotpath-alloc-2026-03-03.md`
- `notes/way-budget-locations-on-ways.md`
