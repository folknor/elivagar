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

## Planet scale milestones

- [ ] Step 5: Europe full pipeline (~28 GB, needs >=64 GB RAM)
- [ ] Step 6: Planet full pipeline (~75 GB, needs >=64 GB RAM)

## Release prep

- [ ] Switch `pbfhogg` dependency from path to crates.io version
- [ ] Publish `elivagar` to crates.io

## Tile output optimizations

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
