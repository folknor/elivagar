# elivagar TODO

## Active priorities

1. [ ] Scale validation: run Europe full pipeline (locations-on-ways, defaults including boundaries:8).
   Capture phase splits + seam metrics + deferred-vertex counts.
2. [ ] Scale validation: run planet full pipeline when hardware is available.

## Baselines

Pre-2026-07 baselines removed: the integer-clipping rewrite (ledger
R21-R24, specs/) changed the perf profile wholesale. Current
hash-anchored numbers live in CLAUDE.md and .brokkr/results.db; the
ocean phase is being re-optimized in specs/ocean-perf-structural.md.
Scale-validation runs (Europe/planet, Active priorities above) must
re-establish per-dataset baselines when run.

## Sort chunk compression investigation

- [x] Investigate why lz4 compression adds ~25s to phase12 on Germany (4.7 GB PBF).
  **Resolved** (commits `274e21b`, `30a023c`). Two causes found:
  1. **~16s from millions of small writes** through lz4 `FrameEncoder` (3 write_all calls
     per record × millions of records). Fixed by pre-serializing all records into a
     contiguous `Vec<u8>` before compressing in bulk.
  2. **~10s from real compression throughput** - `lz4_flex` (pure Rust) runs at ~450-500 MB/s,
     not the 2-4 GB/s of C lz4. 10 GB / 500 MB/s ≈ 20s. This is inherent.
  Also tested Snappy (`snap` crate, pure Rust) as an alternative - Planetiler uses Snappy
  for the same use case. Result: Snappy phase12 is identical to lz4, but assemble is worse
  (Snappy decompression slower than lz4 during merge). lz4 remains the better option.
  Germany results (commit `30a023c`, plantasjen, locations-on-ways):
  - None: 114s total, 8.2 GB RSS
  - lz4: 134s total, 9.2 GB RSS
  - snappy: 143s total, 8.6 GB RSS
  Compression remains opt-in (`--compress-sort-chunks lz4|snappy`), off by default.
  Worth revisiting at planet scale where disk I/O may dominate.
  Reference: tippecanoe and tilemaker do not compress sort data. Planetiler supports
  Snappy (opt-in, off by default).

- [x] Benchmark results database should differentiate compression modes.
  Resolved: brokkr now records `meta.compress_sort_chunks` (lz4/snappy/none) in the
  results DB (commit `27f9371` in brokkr).

## Refactoring opportunities

- [ ] Sort fanout and record weight reduction for planet scale.
  **Problem**: sort record volume grows superlinearly with input size. NA (19 GB PBF)
  produces 51.2 GB of sort data (2.68x amplification) vs Germany's 2.07x. The
  superlinear growth comes from polygon-heavy layers (water_polygons, land) that
  fan out across many tiles at low zoom with heavy per-record geometry.
  **Baseline** (commit `f92c435`, plantasjen):
  | Metric | Germany 5.5 GB | NA 19 GB |
  |--------|---------------|----------|
  | sort_bytes/input_MB | 2.07x | 2.68x |
  | records_per_way | 2.1 | 2.3 |
  | chunks_per_input_GB | 99 | 284 |
  Top layers by sort bytes (NA): streets 12.1 GB, water_polygons 10.9 GB,
  land 9.3 GB, buildings 7.3 GB, water_lines 4.4 GB.
  Per-record weight: land 209 B, water_polygons 156 B, streets 76 B, buildings 82 B.
  **Phase 1 - diagnostics** (done, commits `81c4d6b`, `pending`):
  - [x] Per-layer record count, bytes, and per-record weight.
  - [x] Per-layer-per-zoom record and byte distribution.
  - [x] Aggregate sort_records, sort_record_bytes, records_per_way.
  - [x] Per-feature tiles_touched tail stats (p50/p95/p99/max per layer per zoom).
  Full analysis in `notes/sort-fanout-analysis-2026-03-06.md`.
  Key finding: superlinear growth concentrated in polygon layers (water_polygons,
  land) due to geometric tile subdivision - one polygon × O(4^z) tiles × heavy
  per-record geometry (156-209 B/rec). Streets/buildings scale linearly.
  Tail stats (Denmark): p95 tiles_touched is low (1-4 for polygon layers),
  amplification comes from the tail (max 205-2864). Cap would primarily affect
  tail features, not median behavior.
  **Phase 2 - per-layer fanout caps** (done, commit `710e356`):
  - [x] Per-layer fanout caps via `--fanout-cap-default N` (global fallback) and
    `--fanout-cap layer=N,layer=N` (per-layer overrides). Effective cap: override
    if nonzero, else default, else uncapped. Applied to polygon-geometry emit only.
  - [x] Cap impact metrics: `fanout_capped_features_{layer}`, `capped_tiles`,
    `capped_bytes_estimated` (avg record size × capped bbox tiles). Strict layer
    name validation (fail on unknown, not warn).
  - [x] Top-10 capped feature ID reporting for visual QA targeting (commit `2cd1907`).
  NA cap benchmarks (plantasjen, commit `c8392e1`):
  | Config | Wall | phase12 | sort_bytes | features capped | output |
  |--------|------|---------|------------|-----------------|--------|
  | Uncapped (`bed3629`) | 699s | 507s | 51.32 GB | 0 | 13.44 GB |
  | water_polygons=4096 | 678s (-21s) | 492s | 50.81 GB | 197 | 13.36 GB |
  | water_polygons=2048 | 670s (-29s) | 485s | 50.77 GB | 425 | 13.34 GB |
  Diminishing returns from 4096→2048: +8s savings but 2.2x more capped features.
  **Policy**: water_polygons=4096 is default candidate (most benefit, smallest blast
  radius). water_polygons=2048 for aggressive profiles. Other layers uncapped.
  Gate default flip on targeted visual QA of top capped IDs (Great Lakes/coasts).
  **Instrumentation note**: FanoutStats collection (histograms, per-layer-per-zoom
  stats) has negligible overhead (array writes, no hot-path allocations). Not worth
  stripping. The per-layer-per-zoom fanout table output is noisy for production runs;
  consider making it opt-in via a `--fanout-stats` flag when not debugging sort behavior.
  - [ ] Visual QA: inspect top capped feature IDs on coastline/lake tiles.
    Gate for enabling water_polygons=4096 as default.
    Workflow: run Norway + Japan PBFs with `--fanout-cap water_polygons=4096`,
    use nidhogg `/map` viewer with OSM ID lookup (nidhogg `GET /api/element/{type}/{id}`
    + viewer highlight) to inspect each top-10 capped ID.
    Acceptance: no visible missing water features at z6-z12.
    Top-10 capped IDs were not captured because the NA runs (`c8392e1`) predated the
    top-10 reporting commit (`2cd1907`). The KV format is already brokkr-compatible
    (`fanout_capped_top_N=layer/zZ/osm_id=ID/bbox_tiles=N`). Re-run to capture.
    NA re-run command:
    `brokkr bench self --dataset north-america-latest --fanout-cap water_polygons=4096`
  - [ ] Zoom-dependent subpixel area threshold for polygon layers (flag-gated).
    Quality tradeoff: eliminates small-but-visible features at mid-zoom.
  **Phase 3 - polygon record weight reduction** (second, but soon):
  - [x] ~~More aggressive DP tolerance policy for polygon layers at z8-z12~~
    Superseded 2026-07-06: polygon simplification is now per-zoom integer DP
    from base geometry (OSM_DP_TOL_PX in geometry/int_ocean.rs, no cascade);
    tolerance tuning happens there if record weight still matters.
  - [ ] Compact polygon wire format (delta-encoded coords, smaller varint overhead).
  - [ ] Deferred geometry materialization (compact refs in phase12, late clip in
    assemble). Highest savings, largest effort, risk of assemble bottleneck.
  **Operational controls** (orthogonal, implement when needed):
  - Dynamic sort chunk flush threshold tied to RSS.
  - Back-pressure rayon via sort-buffer occupancy.
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

- [ ] Sort keys within layers: emit feature order hints (e.g. road importance) so renderers
  stack correctly without client-side sorting. Audit whether MapLibre depends on this (#323).
- [ ] Feature dropping: iteratively drop least-important features from oversized tiles until
  they fit a size budget. Needed for dense urban areas at planet scale. Tippecanoe (#378)
  hit an infinite loop bug here - need a guaranteed convergence invariant (also #340, #45).
  Low-zoom dropping must be profile-aware to avoid overaggressive removal (tippecanoe #201).
  Prerequisite: tile size diagnostics (see below) to identify which tiles need dropping.
- [ ] Building merge at z13: Planetiler optionally unions adjacent buildings to reduce tile
  size in dense urban areas. Their benchmarks show ~37% planet runtime increase (expensive).
  JTS polygon union also hits robustness bugs on real data (Planetiler #700). Cheaper
  alternatives: simplify building geometry, drop small buildings at low zoom. Only worth
  pursuing if output tile size becomes a blocking issue.
  Ref: Planetiler `--building-merge-z13`, `notes/upstream-issues/planetiler-relevant.md`.

## Geometry correctness

- Shared-edge simplification (adjacent polygons, tippecanoe #105):
  PREMISES SUPERSEDED 2026-07-06. The March analysis (full-res deferral
  3.3x regression, simplify-then-reconcile design research) targeted the
  old Mercator-space DP pipeline, which no longer exists. The integer
  engine simplifies every feature per zoom with rotation-invariant DP
  from a SHARED base quantization grid with pin-aware shared-vertex
  preservation (specs/emit-polygon-integer-port.md), which changes seam
  incidence wholesale. The seam-reconcile machinery
  (`--seam-reconcile-layers`, assemble reconcile_boundary_seams) is
  still wired and functional.
  - [ ] Re-evaluate seam incidence visually on the post-rewrite output
    (admin borders at z4-z8, landuse boundaries) BEFORE any further
    reconciliation work. If seams are gone or negligible, delete the
    deferral machinery; if not, design against the integer engine.
    (The March design docs and Phase 1-3A findings are in git history
    and notes/simplify-then-reconcile-design.md if needed.)

## Schema extensions (beyond Shortbread 1.0)

- [ ] Lake/river centerlines: generate centerline geometries for elongated water features to
  improve label placement. Current `point_on_surface()` works for compact polygons but
  produces poor label positions for long/thin features like fjords or narrow lakes. Planetiler
  #1137 hit bugs in their centerline stage. Investigate algorithmic approach (medial axis,
  Voronoi, or skeleton-based) and whether this should be a post-processing step or integrated
  into the assemble phase.
- [ ] Wikidata multilingual name enrichment: OSM features often carry `wikidata=Q*` tags
  linking to Wikidata entities with names in dozens of languages. Currently elivagar only
  emits `name:*` tags present directly in the PBF, which limits language coverage - many
  features only have the local-language name tagged. Planetiler does this via a background
  worker that pre-fetches `wikidata_names.json` (#1290 hit reliability issues).
  **Investigated 2026-03-06** (see `notes/wikidata-name-enrichment.md`):
  - Planetiler uses two-phase SPARQL approach (~291 MB cache, 9 min planet fetch). SPARQL
    endpoint is unreliable (#1290). Also open: rdfs:label vs P2561 name statements (#679).
  - Recommended: dump-based preprocessor (avoids SPARQL) + runtime join in elivagar.
    Lookup file (~200-300 MB) loaded as `HashMap<u64, Vec<(LangId, String)>>`, treated
    as optional input like `--ocean`. OSM `name:*` tags always take precedence.
  - Tilemaker and Tippecanoe have no Wikidata integration.
  - Requires wire format changes: extend beyond current 3 name keys (`name`, `name_en`,
    `name_de`) to support configurable language set.
  - Not a current priority - worth doing before planet-scale release for label coverage.
- [ ] Natural Earth low-zoom layers: use Natural Earth vector data (1:10m/1:50m/1:110m) for
  z0-5 features like country boundaries, lakes, and land polygons instead of simplifying
  full-resolution OSM geometry. Currently elivagar uses the water-polygons-split-3857
  shapefile for ocean (similar pattern).
  **Investigated 2026-03-06** (see `notes/natural-earth-low-zoom.md`):
  - Best candidates: `water_polygons` (lakes/glaciers) and `boundaries` at z0-5.
    Ocean layer stays as-is (osmdata.openstreetmap.de is more current than NE).
  - NE scales: 110m for z0-1, 50m for z2-3, 10m for z4-5. OSM takes over at z6.
  - Individual shapefiles ~50 MB total (no need for 400 MB full SQLite).
  - NE data is EPSG:4326 (not 3857 like ocean): reader calls `project()` instead
    of `from_epsg3857()`.
  - Implementation follows `ocean.rs` pattern: new `natural_earth.rs` module, emit
    `SortRecord`s to sort stream. Negligible performance cost.
  - Scale transition: NE features `max_zoom=5`, OSM features `min_zoom=6`. Split
    Shortbread profile zoom ranges by source.
  - Neither Planetiler's nor Tilemaker's Shortbread profiles use NE - differentiation.
  - CLI: `--natural-earth dir/` with auto-detection, `--no-natural-earth` to disable.

## Future architecture

- [ ] OGC TileMatrixSet v2 / non-Mercator tiling schemes. Nidhogg's API surface
  is expanding, and consumers may need non-WebMercator projections (EPSG:4326, polar, etc.).
  **Investigated 2026-03-06** (see `notes/non-mercator-tiling.md`):
  - PMTiles v3 is structurally WebMercator-locked (no CRS field, Hilbert assumes square
    grids). MapLibre doesn't render non-Mercator vector tiles. External blockers.
  - 12 coupling points identified in elivagar (geometry.rs, ocean.rs, pmtiles_writer.rs).
    Deepest: Mercator projection math and Hilbert tile IDs (very hard to abstract).
  - Watch-list item. Trigger: PMTiles v4 with CRS support, or MapLibre non-Mercator
    rendering, or a concrete nidhogg consumer need.
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

## Website

- [ ] Write a one-page project website (what it does, benchmarks, usage, repo link)
- [ ] Host via GitHub Pages

## Notes index

- `notes/north-america-memory-plan.md`
- `notes/north-america-hotpath-alloc-2026-03-03.md`
- `notes/way-budget-locations-on-ways.md`
- `notes/non-mercator-tiling.md`
- `notes/wikidata-name-enrichment.md`
- `notes/natural-earth-low-zoom.md`
- `notes/sort-fanout-analysis-2026-03-06.md`
