# elivagar TODO

## Release prep (0.1.0)

- [x] Switch `pbfhogg` dependency from path to crates.io version.
  Its provenance version is derived from Cargo.lock at build time
  (`build.rs` `locked_version`), so no hardcoded version to keep in sync.
- [x] Scale validation: planet full pipeline. Ran 2026-07-31 on bygg
  (16c/32t, 30.5 GiB RAM): 571.7s wall, 12.7 GB peak RSS, 58.7 GiB output,
  269.8M tiles addressed / 52.2M unique. Details in
  `notes/planet-30gb-roadmap.md`. Europe was dropped as a separate step -
  planet subsumes it and nothing needs the intermediate rung.
- [x] Gate the MLT encoder behind the non-default `mlt` cargo feature.
  Never validated against a client, no per-tile compression, no standing
  gate covers it; `mlt-core` is pre-1.0. Default builds are MVT-only and
  `--tile-format mlt` refuses with the feature named.
- [x] Exclude development state from the published package (`corpus/`,
  `notes/`, `docs/`, `scripts/`, `.brokkr/` and friends). Note this makes
  the README's relative links to `notes/` and `scripts/` resolve only on
  GitHub, not on crates.io.
- [x] Set `rust-version = "1.97"`. The tree uses no nightly features; the
  README's old nightly requirement was stale.
- [ ] Publish `elivagar` to crates.io.

## Known limitations to document at release

- **Oversized tiles.** The planet run produced 1 severe and 44 warn
  oversize tiles, max 1.08 MB at z14/13722/7013. No feature dropping
  exists yet (see below).
- **Cross-piece ocean seam, z8-z14.** The full-resolution ocean pass
  descends per source piece with `pins: None`, so a boundary shared
  between two pieces can be simplified differently on each side. Bounded
  sub-pixel by the fixed per-zoom tolerances and unobserved in practice;
  no standing gate would catch it. Roadmap H5 has the detector candidate.
- **1M dedup cap.** At planet the cap skipped 51.2M of 52.2M unique
  payload inserts. Costs output bytes, not RAM, and still saved 13.18 GB.
  Unpriced - roadmap H3 wants that number before any record claim.

## Scale validation

Planet is done (above). Re-establish per-dataset baselines after any
structural landing; current numbers live in `reference/performance.md`
and `.brokkr/results.db`. Note that every stored baseline below the bygg
line is plantasjen's and the two hosts are not comparable (bygg is ~1.9x).

## Tile output optimizations

- [ ] Feature dropping: iteratively drop least-important features from oversized tiles until
  they fit a size budget. Needed for dense urban areas at planet scale. Tippecanoe (#378)
  hit an infinite loop bug here - need a guaranteed convergence invariant (also #340, #45).
  Low-zoom dropping must be profile-aware to avoid overaggressive removal (tippecanoe #201).
  Prerequisite: tile size diagnostics to identify which tiles need dropping.
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
  preservation, which changes seam incidence wholesale. The
  seam-reconcile machinery (`--seam-reconcile-layers`, assemble
  reconcile_boundary_seams) is still wired and functional.
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

## MLT (behind the `mlt` feature)

The encoder is real - `src/mlt.rs` calls upstream `mlt-core`, and committed
geometry fixtures round-trip through `mlt_core::parse_layers` covering
point/line/polygon and multi-geometries. What is missing is everything that
would make it default-on:

- [ ] End-to-end client compatibility validation in nidhogg/MapLibre, and
  rollout guidance. This is the gate on flipping the feature to default.
- [ ] Per-tile compression for the MLT path. It currently writes
  `TileDataCompression::None` and has never been through the gzip/brotli
  contract the MVT path uses.
- [ ] Bring MLT output under a standing gate. The corpus baseline, earcut
  oracle and `verify` are all MVT-only today.
- [ ] Price MLT against MVT on assemble CPU and output size (roadmap's open
  question on the record's CPU budget).
- [ ] MLT feature-order controls: `--no-mlt-feature-sort` equivalent
  semantics, and evaluate default behavior for compression/size.
- [ ] MLT polygon tessellation mode: `--pretessellate` equivalent for
  polygon-only layers, benchmark render/size tradeoffs.
- [ ] MLT compression/encoding tuning flags (e.g. shared dictionary mode),
  with benchmark-guided defaults.

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
