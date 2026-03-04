# elivagar TODO

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

- [ ] `ocean.rs`: `emit_ocean_polygon` — scanline fill, needs integration test with shapefile.
  Coastline correctness matters for regional extracts where the boundary cuts through
  ocean polygons (tilemaker #16).
- [ ] `pipeline.rs`: `emit_multipolygon_feature` — glue code, needs full pipeline context.
  Large lakes/water bodies at low zoom are particularly vulnerable to simplification + clipping
  producing visible topology artifacts (tilemaker #191). Very large polygons ("monster polygons")
  also stress the clipping path specifically (tilemaker #607).
- [x] `inspect.rs`: read-only diagnostic tool tested on root-only and leaf-directory layouts,
  and migrated to use `pmtiles_reader.rs` shared helpers to reduce parsing duplication.
- [x] `sort.rs`: `SortWriter::resume` / `adopt_chunk_files` now directly tested.
  Coverage includes: checkpoint resume success, missing required chunk failure,
  stale leftover deletion on resume, empty checkpoint resume, and adopted chunk merge correctness.
- [ ] Import targeted cases from Mapbox's `mvt_fixtures` corpus for `mvt.rs` conformance testing
  (tilemaker #103).
- [x] PMTiles metadata JSON validation: `elivagar verify` now validates metadata JSON
  structure and `vector_layers` schema. Remaining gap: fuzz testing for attribute/value edge
  cases that could produce malformed JSON (tippecanoe #181).
- [ ] Per-attribute minzoom filtering: `encode_attrs_bytes()` in wire_format.rs filters
  attributes by zoom. Verify with tests that per-attribute zoom gates actually take effect
  and don't leak attributes to wrong zoom levels (tilemaker #671).

## Refactoring opportunities

- [ ] Expand checkpoint/skip-to for faster profile iteration: currently supports `--skip-to
  ocean` and `--skip-to sort`. A `--skip-to assemble` mode that reuses sorted chunks but
  re-runs MVT encoding with different profile settings would speed up Shortbread tuning on
  large extracts. Planetiler #1497/#1468 adds exactly this (reuse feature DB for post-
  processing iteration).
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
- [ ] Tile size diagnostics: report per-tile size stats, flag oversized tiles, expose
  layer-level breakdowns. Needed for feature dropping tuning and profile regression triage
  (Planetiler #391).
- [ ] Compressed sort chunks: gzip-compress temp sort files to reduce I/O during sort phase.
  Tippecanoe (#56) saw wins from this. Low priority since sort is already fast.
- [ ] Building merge at z13: Planetiler optionally unions adjacent buildings to reduce tile
  size in dense urban areas. Their benchmarks show ~37% planet runtime increase (expensive).
  JTS polygon union also hits robustness bugs on real data (Planetiler #700). Cheaper
  alternatives: simplify building geometry, drop small buildings at low zoom. Only worth
  pursuing if output tile size becomes a blocking issue.
  Ref: Planetiler `--building-merge-z13`, `notes/upstream-issues/planetiler-relevant.md`.

## Geometry correctness

- [ ] Post-simplification ring validity: Douglas-Peucker can produce self-intersecting rings.
  Low risk but no safety net currently. Planetiler hit this with VW+smoothing (#1263, #1192).
  Tippecanoe (#164) is rewriting their polygon cleaner (replacing Wagyu) for robustness on
  messy/self-contradictory input geometry — same problem class. High pixel tolerance can also
  invalidate multipolygon topology specifically (Planetiler #496). Tilemaker #828 confirmed
  that DP-simplified polygons break MapLibre's earcut triangulation — concrete downstream
  failure mode for elivagar's current simplifier.
- [ ] Deterministic multipolygon assembly: `join_ways()` in multipolygon.rs uses
  `std::collections::HashMap` which has random iteration order. For well-formed multipolygons
  the two-pass algorithm converges, but broken/ambiguous ones can produce different ring
  assignments between runs. Replace with `FxHashMap` (already used in mvt.rs) or sort chains
  before joining (Planetiler #788).
- [ ] Pre-quantization polygon validity: validate/repair polygon rings before snapping to tile
  grid coordinates. Invalid polygons that survive clipping+simplification can cascade into
  tile artifacts after integer quantization (Planetiler #566). Tilemaker #602 hit this as
  spikes/self-intersections introduced during coordinate scaling. The interplay between
  simplification and rounding specifically produces visible artifacts (Planetiler #324).
  Planetiler #1493 is actively exploring optimistic naive polygon snapping approaches.
- [ ] Label points in polygon holes: `point_on_surface()` scans outer ring only and does not
  avoid placing points inside inner rings. Multipolygons with large holes could get label
  points in the wrong place (tippecanoe #62). Tilemaker #461 hit centroid exceptions on
  problematic geometry — need robust fallbacks for edge-case polygons. Also affects address
  labels: building polygons with courtyards can get housenumber placed inside the hole
  (Planetiler #237). Current algorithm is a simple 5-scan horizontal sweep; Planetiler #723
  implements pole of inaccessibility (max inscribed circle) which gives better results for
  irregular polygons but is more expensive.
- [ ] Shared-node simplification: Douglas-Peucker simplifies all points uniformly and can
  remove road intersections or boundary junctions if they fall below the tolerance threshold.
  Tippecanoe (#99) had bugs in their shared-node preservation mode. For polygons, independent
  simplification of adjacent polygons sharing an edge produces slivers/gaps along the shared
  boundary (tippecanoe #105).
- [ ] Antimeridian handling: no special-casing for features or datasets crossing 180°/-180°
  longitude. Matters for planet output — archive metadata bbox and geometry wrapping both
  need attention (tippecanoe #82, #205). Tippecanoe #254 also fixes bbox for geometries
  extending beyond strict mercator-plane limits in buffered areas. Wraparound detection
  heuristics should be disabled when geometry becomes contradictory (tippecanoe #176).

## Schema extensions (beyond Shortbread 1.0)

- [ ] Building height/levels attributes: emit `height`, `min_height`, `building:levels` from
  OSM tags on building polygons. Shortbread 1.0 only specifies `dummy=1` for buildings, but
  the data is in PBFs and useful for 2.5D/3D rendering in nidhogg. Well-tagged in European
  urban areas. MLT's 2.5D basemap support makes this more relevant if we adopt that format.
  Also consider `building:part=yes` sub-components (tilemaker #692) — needed for proper 3D
  rendering of complex buildings with towers, wings, etc.
- [ ] Elevation attribute on peaks/passes: elivagar's POI layer does not match `natural=peak`
  or `natural=volcano`, and does not emit the `ele` (elevation) tag. Investigate whether
  Shortbread 1.0 specifies these, and add with proper unit conversion — OSM `ele` is
  nominally meters but sometimes tagged in feet (Planetiler #224 had a unit-conversion bug
  here). Useful for topographic labeling in nidhogg.
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
- [ ] Cliff/landform line features: `natural=cliff` is a linear feature not in Shortbread 1.0.
  Elivagar's land layer only matches polygon natural features (bare_rock, beach, etc.). Cliffs
  are well-tagged in mountainous areas and useful for topographic rendering. Would need a new
  landform line layer or extension to an existing layer. Tilemaker #265 hit rendering issues
  with cliff classification.
- [ ] EV charging stations as POIs: `amenity=charging_station` is not in elivagar's POI list
  because Shortbread 1.0 doesn't include it, but EV charging infrastructure is increasingly
  important for map consumers. OSM has good coverage in Europe. Investigate adding as a
  schema extension alongside other beyond-Shortbread POI types (Planetiler #765 hit a bug
  where charging stations were silently dropped).

## Future architecture

- [ ] Investigate OGC TileMatrixSet v2 / non-Mercator tiling schemes. Nidhogg's API surface
  is expanding, and consumers may need non-WebMercator projections (EPSG:4326, polar, etc.).
  Tippecanoe has an open request for this (tippecanoe #286).
- [ ] MLT (MapLibre Tile) output format. Column-oriented successor to MVT, spec stable since
  October 2025, first release January 2026. Up to 6x better compression on large tiles and
  3x faster decoding vs MVT+gzip. Supported by MapLibre GL JS 5.12+, Native (Android 12.1,
  iOS 6.2), Planetiler, PMTiles, and Martin. Rust encoder+decoder exists in the
  maplibre-tile-spec repo (not yet on crates.io). Migration path: replace MVT encoding in
  assemble phase, PMTiles writer stays the same (opaque tile blobs). Elivagar's sorted-by-tile
  feature stream is a natural fit for columnar encoding. Since nidhogg controls the full stack
  (elivagar → tile serving → MapLibre client), we can adopt MLT without waiting for broad
  ecosystem maturity — we're our own first consumer. Caveat: Planetiler #1491 reports
  MLT-in-PMTiles archives failing in MapLibre 5.19, so compatibility is not yet solid.
  Planetiler #1463 adds `--mlt-shared-dict` for compression tuning (tippecanoe #380).

## Quality

- [ ] Visual verification (tracked in nidhogg TODO)
- [ ] Missing-ref diagnostics: when ways reference missing nodes or relations reference missing
  ways (common in regional extracts), elivagar silently skips them. Should emit a summary
  count so users know data is incomplete rather than wondering why features are missing
  (tilemaker #332). Also applies to nested relations: `prepare_relation()` skips non-way
  members (`MemberId::Relation`), so relation-members-of-relations are silently dropped
  (tilemaker #638). Correct for multipolygon relations (way-only per OSM spec) but worth
  counting.
- [ ] Source provenance in PMTiles metadata: include OSM replication timestamp and source
  PBF filename in the metadata JSON so consumers can trace data freshness. Currently only
  emits schema info (name, format, layers). Planetiler #120 added this for MBTiles; same
  applies to PMTiles.

## Website

- [ ] Write a one-page project website (what it does, benchmarks, usage, repo link)
- [ ] Host via GitHub Pages

## Notes index

- `notes/north-america-memory-plan.md`
- `notes/north-america-hotpath-alloc-2026-03-03.md`
- `notes/way-budget-locations-on-ways.md`
