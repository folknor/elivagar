# MLT Integration Plan (Alternative to MVT)

Date: 2026-03-04
Status: Proposed implementation plan

## TODO

Plan was written before we were aware of some features in tippecanoe:
- --pretessellate`: When using `--output-format=mlt`, pre-triangulate polygon geometries for faster rendering. Only applies to layers where all features are polygons.
- --no-mlt-feature-sort`: When using `--output-format=mlt`, disable within-tile spatial sorting of features by Hilbert curve index. Sorting is on by default and improves compression.

Let's investigate this and perhaps merge it into the existing plan below, or alternatively we can see about this after the main stack has been completed.

https://github.com/felt/tippecanoe/compare/main...dannote:tippecanoe:feature/mlt-encoder
https://github.com/felt/tippecanoe/issues/380

## Goal

Integrate MLT (MapLibre Tile) as a first-class output format in elivagar while keeping MVT as the default and fully supported path.

The PMTiles writer remains unchanged at the container layer; only tile payload encoding changes.

## Non-Goals

1. Replacing MVT entirely in this phase.
2. Refactoring unrelated pipeline phases unless needed for correctness/performance.
3. Shipping unverified speculative optimizations before correctness parity is established.

## Constraints and Assumptions

1. MLT must be selectable at runtime as an alternative output encoding.
2. Existing `run` and `inspect` workflows must remain stable for MVT archives.
3. No full real-data pipeline runs during iteration unless explicitly requested by the user.
4. MLT support in downstream stack (tile server/client) is available and can be tested in nidhogg integration after local elivagar validation.

## High-Level Architecture Change

Current assemble phase is effectively:

1. decode sorted feature stream by tile
2. build tile-local feature/layer structures
3. encode MVT bytes
4. gzip MVT bytes
5. write tile blob to PMTiles

Target assemble phase:

1. decode sorted feature stream by tile
2. build tile-local feature/layer structures (shared representation)
3. encode either MVT or MLT via selected encoder
4. apply tile compression policy appropriate to selected format
5. write tile blob to PMTiles

## Step-by-Step Plan

## Phase 1: Format Selection Plumbing

1. Add a tile format enum in config:
   - `TilePayloadFormat::Mvt`
   - `TilePayloadFormat::Mlt`
2. Add CLI flag to `elivagar run`:
   - `--tile-format mvt|mlt` (default `mvt`)
3. Thread the selected format through `TilegenConfig` to assemble phase.
4. Emit selected format in run logs/metrics for traceability.

Exit criteria:

1. Build passes.
2. Existing MVT behavior unchanged by default.

## Phase 2: Encoder Abstraction Boundary

1. Introduce an internal encoder trait/interface, e.g. `TileEncoder`:
   - input: tile-local intermediate representation
   - output: encoded tile bytes
2. Keep current `mvt.rs` encoder behind this abstraction with zero behavior change.
3. Ensure assemble phase no longer hardcodes `mvt.rs`.
4. Add unit tests that assert MVT output byte-for-byte unchanged for fixed fixtures.

Exit criteria:

1. MVT regression tests pass and encoded bytes are unchanged for golden fixtures.
2. No measurable runtime regression in synthetic benchmark noise band.

## Phase 3: MLT Encoding Implementation

1. Select integration mode for MLT encoder:
   - preferred: dependency crate when available and stable
   - fallback: vendor/module integration pinned to known commit
2. Implement `MltEncoder` that consumes the same tile-local intermediate representation.
3. Map geometry and attributes from Shortbread output to MLT schema semantics.
4. Define deterministic ordering guarantees (layers/features/columns) to keep output stable.
5. Wire `TilePayloadFormat::Mlt` to `MltEncoder`.

Exit criteria:

1. `--tile-format mlt` produces structurally valid tile payloads.
2. Encode path succeeds on synthetic pipeline fixtures.

## Phase 4: Compression and PMTiles Metadata Policy

1. Define compression behavior per format:
   - MVT path: keep current gzip behavior
   - MLT path: choose initial policy (recommended: no extra gzip first, then benchmark)
2. Ensure PMTiles header/metadata fields reflect payload + compression correctly.
3. Extend metadata emitted by elivagar with explicit payload format marker (`mvt`/`mlt`).
4. Verify readers can differentiate and decode tiles correctly through metadata contract.

Exit criteria:

1. Produced PMTiles archives are readable by inspect tooling.
2. Downstream consumer can identify payload format unambiguously.

## Phase 5: Inspect Tool Support

1. Extend `inspect.rs` metadata reporting to show:
   - tile payload format
   - tile compression mode
2. Keep backward compatibility for old archives with missing format metadata.
3. Add tests for:
   - MVT archive inspection
   - MLT archive inspection
   - legacy metadata fallback path

Exit criteria:

1. `elivagar inspect` correctly reports both formats.
2. No regressions for existing PMTiles artifacts.

## Phase 6: Correctness Validation Matrix

1. Build fixture-based parity tests (MVT vs MLT) for:
   - points, lines, polygons, multipolygons with holes
   - layer coverage for representative Shortbread classes
   - property typing edge cases (string, int, float, bool)
2. Build render-level sanity tests in nidhogg/MapLibre:
   - zoom/layer spot checks
   - labels and symbol placement spot checks
3. Add failure diagnostics:
   - encoder errors include tile ID/layer context
   - optional tile dump for failing fixtures

Exit criteria:

1. No correctness regressions in fixture suite.
2. Visual sanity checks pass for target sample regions.

## Phase 7: Performance and Size Evaluation

1. Use synthetic microbench paths first:
   - `brokkr bench pmtiles`
   - targeted encoder microbench for identical tile sets (MVT vs MLT)
2. Measure:
   - encode wall time
   - output tile bytes (per tile and aggregate)
   - archive bytes on disk
   - decode/render impact in downstream client where available
3. Record host + commit hash for every benchmark entry.
4. Only run heavier dataset benchmarks when explicitly requested.

Exit criteria:

1. Benchmark report added to notes with reproducible commands and commit hash.
2. Decision made on default compression policy for MLT.

## Phase 8: Rollout Strategy

1. Keep default as `--tile-format mvt`.
2. Mark MLT as experimental for initial release window.
3. Add guardrails:
   - explicit warning for unsupported combinations if any
   - clear error messages on decoder incompatibility assumptions
4. Add release notes section with migration instructions for nidhogg stack.

Exit criteria:

1. MLT path is production-usable behind explicit opt-in.
2. Operational runbook exists for switching workloads to MLT.

## Code Touchpoints (Expected)

1. `src/main.rs` (CLI flag parsing and config wiring)
2. `src/pipeline.rs` (assemble phase selection)
3. `src/mvt.rs` (adapter into encoder trait)
4. `src/inspect.rs` (metadata display and format awareness)
5. New files likely:
   - `src/tile_encoder.rs` (shared interface)
   - `src/mlt.rs` (MLT encoder integration)

## Risk Register

1. Spec mismatch risk:
   - Mitigation: pin to official spec tests/reference vectors where available.
2. Metadata ambiguity risk:
   - Mitigation: explicit payload format field + inspect fallback behavior.
3. Performance regression risk:
   - Mitigation: microbench gate before broader rollout.
4. Integration churn risk from evolving upstream MLT crates:
   - Mitigation: version pinning and compatibility matrix in notes.

## Acceptance Criteria (Project-Level)

1. `elivagar run` supports `--tile-format mvt|mlt`.
2. MVT default path remains byte-stable for existing fixtures.
3. MLT archives are generated, inspectable, and render correctly in target stack.
4. Benchmarks document tradeoffs (size/time) with host + commit anchors.
5. Documentation updated in `README.md`, `CLAUDE.md`, and `TODO.md` status sections after implementation.

## Suggested Implementation Order

1. Phase 1 and Phase 2 (safe refactor, no behavior changes).
2. Phase 3 and Phase 4 (new encoding path and metadata contract).
3. Phase 5 and Phase 6 (tooling and correctness hardening).
4. Phase 7 and Phase 8 (performance characterization and rollout).

## Concrete Upstream API Surfaces (maplibre-tile-spec/rust)

The Rust workspace currently exposes what we need through `mlt-core` (crate version `0.1.2` in the repo workspace):

1. Top-level parse/encode surfaces:
   - `mlt_core::parse_layers(&[u8]) -> Result<Vec<Layer>, MltError>`
   - `OwnedLayer::write_to(&mut Write)` for serializing one encoded layer tuple `(size, tag, data)`
2. v01 layer model (MVT-compatible tag `0x01`):
   - `mlt_core::v01::OwnedLayer01`
   - `mlt_core::OwnedLayer::Tag01(OwnedLayer01 { ... })`
3. Geometry build/encode:
   - Build decoded form with `DecodedGeometry` + `push_geom(...)`
   - Encode with `OwnedGeometry::Decoded(decoded).encode_with(GeometryEncoder)`
   - Key encoder type: `GeometryEncoder::all(IntEncoder::varint())` (and other tuning options)
4. ID build/encode:
   - `OwnedId::Decoded(DecodedId(Some(Vec<Option<u64>>)))`
   - Encode with `id.encode_with(IdEncoder { logical, id_width })`
5. Property build/encode:
   - `DecodedProperty { name, values: PropValue::* }`
   - Batch encode with:
     - `MultiPropertyEncoder::new(Vec<PropertyEncoder>, shared_dict_encoders)`
     - `Vec<OwnedEncodedProperty>::from_decoded(&decoded_props, encoder)`
6. Conversion helpers present today:
   - `convert::mvt::mvt_to_feature_collection(...)` exists (MVT -> feature collection)
   - There is no ready-made direct helper for our exact in-memory `LayerBuilder` -> encoded MLT tile path, so we should build a direct adapter.

## Exact Elivagar Plug Points

This is where MLT should plug in the current codebase:

1. CLI/config selection:
   - `src/main.rs` (`RunArgs`, `run`) add `--tile-format mvt|mlt`
   - `src/pipeline.rs` `TilegenConfig` add payload format enum
2. Assemble encode switch:
   - `src/pipeline.rs` `phase_assemble(...)`
   - `src/pipeline.rs` `encode_tile_batch(...)` currently hardcodes:
     - MVT encode via `mvt::encode_tile_into(...)`
     - gzip via `flate2::write::GzEncoder`
3. PMTiles header + metadata contract:
   - `src/pmtiles_writer.rs` currently fixed:
     - `h[98] = 2` (tile compression gzip)
     - `h[99] = 1` (tile type MVT)
     - metadata `"format":"pbf"`
   - Must become format-aware for MLT archives.
4. Inspector reporting:
   - `src/inspect.rs` `tile_type_name(...)` only knows `MVT/PNG/JPEG/WebP/AVIF`
   - Add MLT reporting and metadata keys for explicit payload format.
5. Existing shared tile IR candidate:
   - `src/mvt.rs` `LayerBuilder`, `Feature`, `Value`, `GeomType`
   - `src/wire_format.rs` populates `LayerBuilder` via `add_feature_to_layer(...)`
   - This is the natural place to adapt into `mlt-core` layer inputs.

## Proposed Adapter Contract (Implementation-Level)

Add a dedicated adapter module (recommended: `src/mlt_encoder.rs`) that consumes the same per-tile `LayerBuilder` array already built in assemble:

1. Input:
   - `&[&LayerBuilder]` (non-empty layers for one tile)
2. Output:
   - `Vec<u8>` containing full MLT tile bytes (concatenated serialized `OwnedLayer::Tag01` records)
3. Internal steps per layer:
   - Extract feature rows from `LayerBuilder`
   - Convert row geometry commands -> `geo_types::Geometry<i32>`
   - Build `DecodedGeometry` via `push_geom`
   - Build `DecodedId`
   - Build `Vec<DecodedProperty>` (columnar vectors aligned to feature count)
   - Encode geometry/id/properties with selected encoders
   - Build `OwnedLayer01`, wrap in `OwnedLayer::Tag01`, `write_to(&mut tile_buf)`

## Geometry Conversion Details (Critical Surface)

Current elivagar geometry in `LayerBuilder::Feature.geometry` is MVT command stream (`Vec<u32>`).  
MLT encoder expects decoded geometry objects.

Required adapter work:

1. Implement command decoder from MVT commands to absolute coordinates.
2. Map `GeomType`:
   - `Point` -> `geo_types::Point` or `MultiPoint`
   - `LineString` -> `LineString` / `MultiLineString`
   - `Polygon` -> `Polygon` / `MultiPolygon`
3. Preserve ring semantics for polygons:
   - maintain exterior/interior ring grouping from MVT commands
   - validate winding/closure assumptions before pushing to `DecodedGeometry`
4. Add focused tests for mixed/multi geometries and hole handling.

## Property Conversion Details (Critical Surface)

`LayerBuilder` stores keys/values interned and feature tags as `(key_idx, value_idx)`.
MLT property encoding is typed by column.

Required adapter work:

1. Introduce `LayerBuilder` read-only accessors (`pub(crate)`):
   - features
   - key table
   - value table
2. Build one column per property key:
   - allocate `Vec<Option<T>>` length = feature count
   - fill from per-feature tag refs
3. Map MVT value types to `PropValue`:
   - `String` -> `PropValue::Str`
   - `Bool` -> `PropValue::Bool`
   - `Int/UInt/SInt` -> signed/unsigned integer columns
   - `Float/Double` -> float columns
4. Mixed-type key policy (must be explicit):
   - recommended first pass: fallback mixed columns to stringified `PropValue::Str`
   - add diagnostics counter for type-mixed keys per tile/layer

## Encoder Defaults to Start With

Start with simple/stable settings, then tune later:

1. Geometry: `GeometryEncoder::all(IntEncoder::varint())`
2. IDs: `IdEncoder::new(LogicalEncoder::Delta, IdWidth::OptId64)` (or `Id64` if all present)
3. Properties:
   - integers: `ScalarEncoder::int(PresenceStream::Present, IntEncoder::varint())`
   - floats: `ScalarEncoder::float(PresenceStream::Present)`
   - strings: `ScalarEncoder::str_fsst(PresenceStream::Present, IntEncoder::varint(), IntEncoder::varint())`

These are safe defaults for correctness-first rollout; optimize after benchmarking.

## PMTiles Integration Specifics for MLT

1. Extend `PmtilesConfig` with payload metadata:
   - tile payload format (`mvt`/`mlt`)
   - tile compression mode (`none`/`gzip`)
2. Header fields in `build_header(...)` must be derived, not fixed:
   - `tile_compression` byte
   - `tile_type` byte
3. Metadata JSON in `build_metadata(...)` must include explicit format token for readers.
4. `inspect.rs` should surface both header bytes and metadata format token for auditability.

## Minimal Change Sequence (Code-First)

1. Add payload format enum + CLI/config wiring.
2. Refactor `encode_tile_batch(...)` into:
   - feature collection stage (existing)
   - format-specific encode stage (`encode_mvt_tile` / `encode_mlt_tile`)
   - compression stage (format-dependent policy)
3. Add `mlt_encoder.rs` adapter with correctness tests.
4. Make `pmtiles_writer` header/metadata dynamic by payload format.
5. Extend `inspect` and tests.
6. Benchmark and tune encoder choices.
