# elivagar TODO

## Active priorities

1. [ ] Scale validation: run Europe full pipeline (locations-on-ways path).
2. [ ] Scale validation: run planet full pipeline when hardware is available.

## Refactoring opportunities

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

- [ ] Sort keys within layers: emit feature order hints (e.g. road importance) so renderers
  stack correctly without client-side sorting. Audit whether MapLibre depends on this (#323).
- [ ] Feature dropping: iteratively drop least-important features from oversized tiles until
  they fit a size budget. Needed for dense urban areas at planet scale. Tippecanoe (#378)
  hit an infinite loop bug here — need a guaranteed convergence invariant (also #340, #45).
  Low-zoom dropping must be profile-aware to avoid overaggressive removal (tippecanoe #201).
  Prerequisite: tile size diagnostics (see below) to identify which tiles need dropping.
- [ ] Building merge at z13: Planetiler optionally unions adjacent buildings to reduce tile
  size in dense urban areas. Their benchmarks show ~37% planet runtime increase (expensive).
  JTS polygon union also hits robustness bugs on real data (Planetiler #700). Cheaper
  alternatives: simplify building geometry, drop small buildings at low zoom. Only worth
  pursuing if output tile size becomes a blocking issue.
  Ref: Planetiler `--building-merge-z13`, `notes/upstream-issues/planetiler-relevant.md`.

## Geometry correctness

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
  Remaining work: topology-aware lockstep simplification across polygon groups:
  detect shared edge chains, simplify each shared chain once, and reuse that
  exact chain in all incident polygons before rebuilding rings.
  Suggested scope split:
  Phase A: boundary/admin polygons only (highest visibility, narrower schema).
  Phase B: land/water polygons and general multipolygons.
  Acceptance checks: no shared-edge divergence after simplification within a
  tolerance threshold, no ring-validity regressions, and no large planet-scale
  runtime/RSS regression.

## Schema extensions (beyond Shortbread 1.0)

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

## Future architecture

- [ ] Investigate OGC TileMatrixSet v2 / non-Mercator tiling schemes. Nidhogg's API surface
  is expanding, and consumers may need non-WebMercator projections (EPSG:4326, polar, etc.).
  Tippecanoe has an open request for this (tippecanoe #286).
- [ ] MLT (MapLibre Tile) output format follow-ups:
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
