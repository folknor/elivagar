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

- Shared-edge simplification (adjacent polygons): independent simplification of polygons
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
  - [ ] Phase 1 — shared chain detection: given a set of polygon rings in a tile,
    find contiguous shared vertex sequences (not just shared points). Output:
    `SharedChain { vertices, incidents: Vec<ChainRef> }` where each `ChainRef`
    identifies (ring_index, start, end, reversed). Uses `Vec<ChainRef>` instead
    of a fixed pair to handle >2 coincident rings (rare but possible with
    duplicate/overlapping geometry). Testable in isolation with synthetic geometry,
    no simplification changes yet.
    Ordering constraint: must run BEFORE `merge_same_attr_geometries` in the
    assemble phase, because that merge concatenates unrelated rings into one
    `Vec<u32>`, destroying per-ring identity needed for chain provenance.
    Function signature: `detect_shared_chains(rings: &[Vec<(i32, i32)>]) -> Vec<SharedChain>`
    in geometry.rs. Pure function, no side effects.
    Edge cases: ring wrap-around (chain crossing start/end), self-touching rings,
    multiple disconnected chains per ring pair, three-way junctions (vertex where
    3+ polygons meet — each adjacent pair gets its own chain terminating there).
  - [ ] Phase 2 — boundary/admin polygons: wire chain detection into simplification
    for `boundaries` and `boundary_labels` layers only. Only act on chains with
    exactly 2 incidents (clean adjacency); skip >2 with a counter/metric.
    Simplify each shared chain once, stitch canonical chains back into rings,
    simplify remaining non-shared segments independently. Narrow scope allows
    visual validation on admin borders (the most visible seam source) without
    risking regressions across all layers.
  - [ ] Phase 3 — all polygon layers: extend to landuse, land, water, sites, etc.
    Mostly enabling the same code path for more layers, but adds cross-layer shared
    edges (e.g. landuse polygon sharing an edge with a water polygon). Phase 2 only
    handles intra-layer sharing.
  - Acceptance checks (all phases): no shared-edge divergence after simplification
    within a tolerance threshold, no ring-validity regressions, and no large
    planet-scale runtime/RSS regression.

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
  - Not a current priority — worth doing before planet-scale release for label coverage.
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
  - Neither Planetiler's nor Tilemaker's Shortbread profiles use NE — differentiation.
  - CLI: `--natural-earth dir/` with auto-detection, `--no-natural-earth` to disable.
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
