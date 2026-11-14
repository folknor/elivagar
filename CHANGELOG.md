# Changelog

## Unreleased

- Breaking: removed the unused crate-root exports `RingOrientation`,
  `ring_is_simple` and `ring_role`. These polygon helpers are now
  crate-internal. Tile output is unchanged.

- `verify` holds its tile-level error list to the documented 100: a tile
  carrying several structure findings could push the report past the cap.
  Pass and fail verdicts and tile counts are unchanged.

- `PmtilesWriter::write_to` consumes the writer, and both writer modes now
  publish by renaming a fresh, exclusively created sibling over the output
  path after closing it. The in-memory mode used to truncate an existing
  file in place, and the streaming partial had the fixed name
  `<output>.partial`, which a stale or linked file could alias; the
  partial is now `<output>.partial-<pid>-<n>` and is removed if the
  writer is dropped unpublished. No archive write can rewrite an existing
  inode, so a run mapping the ocean artifact survives its replacement.
  Output unchanged.

- Every Shortbread `kind` value is declared in a typed family (water,
  pier, dam, aerialway, ferry, public transport, place, site, street label
  point and bridge kinds join `LandKind`, `StreetClass` and `LinkClass`),
  and each declaration asserts at compile time that its names are distinct
  and present in the wire interning dictionary, so a new kind can no
  longer silently miss it. `StreetKind::label_name` is removed. Output
  unchanged.

- A polygon feature whose only ring has zero area is kept as a one-ring
  outer by `tile_detail` and by MLT conversion, as MapLibre's
  `classifyRings` keeps it, instead of decoding to no components and
  failing MLT encoding. Zero-area rings among two or more are still
  skipped. Output unchanged.

- One MVT protobuf message reader. `tile_detail`, `verify`, `svg`, `diag`
  and the ocean artifact splice walk tile, layer, feature and value
  messages through `mvt::message`, so a truncated tag or field now fails
  everywhere instead of silently ending a walk. `verify` checks payload
  structure strictly: unknown and duplicate singular fields, features
  without geometry, undecodable values (referenced or not) and tag indices
  outside the layer tables fail the tile, and it parses each tile once
  instead of four times. `tile_detail` strict mode also rejects duplicate
  layer name, extent and version fields; a packed tags or geometry field
  ending inside a varint is refused in both modes instead of being
  repaired by the next occurrence; and polygon ring area wraps to the
  first vertex, as MapLibre's does, which changes tolerant output only for
  unclosed rings. Ring grouping is one rule shared by `tile_detail` and
  MLT. The artifact splice admits only a payload holding exactly one
  ocean layer and nothing else, and takes it verbatim. Output unchanged.

- A resume now refuses a multi-partition sort chunk whose section table
  is corrupt (out-of-order partitions or offsets, a zero-record section,
  a table or section running past the file) instead of deriving silent
  zero-size sections from it. A missing chunk on `--skip-to ocean` is
  reported by id and directory rather than a guessed filename.

- The provenance `build` block is typed. `build.rs` writes it once as
  generated Rust instead of string env vars that provenance parsed back,
  and a failed `git status` at build time now records `dirty` as null
  instead of false. Wire shape and bytes are otherwise unchanged.

- Sort keys are a type. Breaking: `sort::SortKey` is a packed `u64`
  newtype rather than an alias, minted by `SortKey::new(TileId, Layer,
  rank)`; `make_sort_key`, `tile_id_from_key`, `layer_from_key`,
  `priority_from_key` and `partition_from_key` become the methods
  `tile`, `layer` (an `Option<Layer>`, with `layer_byte` for the raw
  byte), `rank` and `partition`, and `from_raw`/`raw` cross the chunk
  wire. `sort::PayloadRecord` is replaced by the named
  `sort::PayloadRef { key, offset, len }`. `partition_start_tile_id`
  returns a `TileId`, and `archive::TileRuns::covering` takes one. A
  sort record whose layer byte names no layer is now refused when
  assembly reads it back, instead of by the encoder. Chunk bytes and
  output are unchanged.

- One PMTiles archive module. Breaking: `pmtiles_reader` and
  `pmtiles_writer` are replaced by `archive`. `PmtilesReader` and
  `ArchiveView` merge into `archive::Archive`, which reads through
  positional reads (the memory map is now crate-internal, used only for
  the ocean artifact). The header is the typed, losslessly decoded
  `archive::Header` with `TileType`, `Compression` and `Clustering`
  enums and named E7 bounds and center, replacing the raw byte array and
  its per-field accessors; `TileDecoder::from_header_fields` becomes
  `for_contract` over those enums. `read_all_runs` returns `TileRuns` of
  `TileRun` (non-zero run length, `TileId` start, `BlobRef` payload);
  `RawDirEntry`, `TileEntry`, `find_entry`, `read_directory`,
  `read_tile_raw`, `decode_directory`, `decode_mvt_layers`, the gzip and
  byte helpers and `HEADER_SIZE` leave the public surface, and
  `read_tile` and `raw_blob` take a `BlobRef`. The reader now refuses
  leaf pointers outside the leaf section, payloads outside the tile data
  section, leaves whose first id differs from their pointer, and runs
  crossing their directory routing interval; nested leaves are followed
  to four directory levels instead of being dropped. `PmtilesConfig` is
  built by `PmtilesConfig::new` from `LonLatBbox` and `MapCenter`, which
  refuses non-finite, out-of-range or inverted geography before any tile
  work. `inspect` and the readability errors print unrecognized header
  bytes by value. `svg` and `diag` refuse archives that are not MVT +
  gzip instead of decoding their payloads as MVT. Output is unchanged.

- One MVT geometry command decoder. `tile_detail`, `verify`, `svg`,
  `diag`, the line merger and the MLT encoder all decode through
  `mvt::decode`, which owns command ids, truncation, zigzag, the
  ClosePath cursor rule and the strict spec 4.3.4 grammar. `tile_detail`
  returns identical detail for every input it accepted before, in both
  modes; some strict-mode error messages changed wording. `verify` reads
  geometry and tag varints with the protobuf reader, rejecting overlong
  varints its hand parser let through. `svg` fails a tile with a wire
  error instead of drawing what decoded before it. `diag` is now the
  public `diag` module and prints a per-feature decode error instead of
  partial rings. Output is unchanged.

- Resume records are split by producer (checkpoint v11). `checkpoint.txt`
  holds only what the PBF phase depends on and derives; the ocean chunks'
  identity moved to a new completion record, `chunks_complete.json`,
  which replaces `sort_chunks.count` and is written after the ocean
  phase. `--skip-to ocean` now admits a different ocean input, or none,
  since it rebuilds the ocean; `--skip-to sort` and `assemble` refuse an
  ocean identity that differs from the run's. The artifact's compression
  level no longer gates a resume, as chunks hold geometry. A full run
  fails if it cannot clear the tmp dir. Ocean, input and producer-config
  refusals now emit `resume_refusal_*` counters. Earlier checkpoints are
  refused. `sort::ChunkCompression` gains serde derives. Output is
  unchanged.

- `TilegenConfig` budgets are typed instead of zero-means-default.
  Breaking: `sort_chunk_size`, `way_inflight_budget` and
  `assemble_batch_budget` are `Option<resources::ByteBudget>`,
  `dedup_cap` is `Option<NonZeroUsize>`, `fanout_caps` holds
  `Option<NonZeroU32>` per layer with `None` uncapped, `threads` is a
  `NonZeroUsize` and `ocean_build` takes one. The new public `resources`
  module owns the defaults, the minimums (now enforced for library
  callers too) and `default_threads()`. Every budget, worker count and
  env knob is resolved once per run and emitted as `res_*` sidecar
  counters; a malformed env knob now fails the run even when its phase
  is skipped. `-j 0` is rejected. Output is unchanged.

- New public `tiling` module: the profile top zoom `MAX_ZOOM`, the
  archive ceiling z31, and a validated `TileId` owning the only Hilbert
  conversions and per-zoom id ranges. Breaking: the writer's
  `xy_to_tile_id`, `try_xy_to_tile_id` and `tile_id_to_zxy` and sort's
  `zoom_from_tile_id` are removed in favour of `TileId::at`,
  `TileId::from_coord`, `TileId::coord` and `TileId::zoom`;
  `PmtilesWriter::add_run`, `sort::make_sort_key` and
  `sort::tile_id_from_key` take or return a `TileId`. Output of a full
  z0-z14 run is unchanged.
- Out-of-range tile ids are refused instead of misread: the directory
  reader and `add_run` reject ids and runs past the last z31 tile, sort
  keys refuse tiles beyond z14 instead of clamping them into the last
  partition, and `verify` counts addressed tiles at every archive zoom.
- A narrowed `TilegenConfig` zoom window now truncates rather than
  re-plans: lines and polygons are built over each layer's own zoom
  range and only emission is cut, so a narrowed archive's OSM tiles
  equal a full build's at those zooms, and the metadata advertises only
  the layers the window can hold, clamped to it.
- pbfhogg 0.6 as the PBF reader, plus hotpath 0.31 and the pmtiles 0.28
  dev-dependency. Output is unchanged.
- Computed ocean follows the same truncation rule: one ocean plan owns
  the two passes (z0-z7 on a z7 grid, z8-z14 on a z14 grid), and a
  narrowed window only cuts their emission, so its ocean tiles equal a
  full build's. A single `z0-z14` shapefile against the world artifact
  now judges band ownership on both pass grids; before, its low pass was
  pruned on the z14 grid.
- The world ocean artifact is admitted once at run entry, so a key
  mismatch fails before phase12 instead of after it; its directory is
  still read only at assembly.
- An artifact-owned tile whose OSM features all encode to nothing keeps
  its artifact ocean; it used to be dropped. Denmark output is unchanged.
- A `TilegenConfig` naming a simplified ocean shapefile or an artifact
  without the full shapefile is an error instead of silently no ocean.
- Checkpoint format v10. It adds no field; v9 checkpoints are refused
  because chunks built on the old pass grids carry identical keys.
  Artifact bytes and `OCEAN_POLICY_VERSION` are unchanged.

Design-hunt fixes, tenth wave. This changes output.

- A multipolygon relation gets one label point per zoom instead of one
  per outer ring, matching the Shortbread spec and Planetiler, in
  `boundary_labels`, `water_polygons_labels`, `addresses`,
  `streets_polygons_labels`, `public_transport` and `pois`. Denmark's
  boundary labels drop from one per island to one per relation.
- The line merger and its invariant checker moved into geometry, and
  assemble's piece planner sits beside the claim window it feeds.
- Way ids and coordinates are typed through the way index.
- Rendered-pixel tolerances and area thresholds derive from one
  `RENDER_PX`, with every value and comparison unchanged.

Design-hunt fixes, ninth wave. The ocean artifact key moves to
`OCEAN_POLICY_VERSION` v8, so the world ocean artifact must be rebuilt
with `ocean-build`; ocean geometry is unchanged.

- Ocean full-tile fills carry one reserved feature id instead of 0,
  which the first fragment of an overlapping source piece also used;
  identical fills still deduplicate across pieces.
- `--dedup-cap 0` is refused instead of silently meaning the default;
  the default cap is a named public constant.
- The node store is one `NodeStore` type over its two backends, with
  typed node ids and coordinates; the old names remain as aliases.
- Dead code removed: the parked multi-zoom float simplifier, the float
  Sutherland-Hodgman polygon clip, unused geometry and debug helpers,
  and MVT value variants the pipeline never produces.
- New tests: a seeded polygon property suite, sort chunk record round
  trips through every codec, and ocean band ownership checked against
  assembly's actual copy ranges.

Design-hunt fixes, eighth wave. This changes output.

- Feature ids in layers fed by more than one OSM element type
  (addresses, POIs, public transport, land and others) are encoded as
  `osm_id * 10 + type` (node 1, way 2, relation 3), the Planetiler
  convention, so a node, a way and a relation with the same number no
  longer share an id. Ids stay within JavaScript's exact integer range;
  single-source layers keep raw ids.
- Centroid label points (`water_polygons_labels`, convex addresses) use
  the area centroid with holes subtracted instead of a vertex average.
- Archive metadata is built as one typed document; duplicate top-level
  keys are an error.
- Checkpoint v9: ocean mode and effective-path facts are typed enums
  shared by checkpoint and provenance; provenance bytes are unchanged.

Design-hunt fixes, seventh wave. This changes output.

- In locations-on-ways mode the data extent now covers the inline
  coordinates of every way, not just the nodes the PBF still carries
  (pbfhogg drops untagged nodes by default). Denmark's extent grows,
  so its ocean band and tile coverage grow with it; checkpoint v8.
- PMTiles headers carry the extract's real bounds and center instead
  of the whole world; the ocean artifact keeps world bounds.
- Address points use the polygon centroid only for convex polygons
  without holes and point-on-surface otherwise, as the Shortbread spec
  declares.
- `--locations-on-ways` asserts the PBF header feature instead of
  overriding it, so a PBF without inline locations is refused rather
  than producing an archive with every way dropped.
- A run that cannot identify its input PBF refuses instead of writing
  an archive with no provenance block.
- An unknown layer id in a sort record is a typed error instead of a
  silent drop; the sort record's command count is checked.
- Pinned vertex masks are checked against their rings, so a desync
  fails loudly instead of being padded.

- MLT archives record tile compression `none` in their provenance
  contract and sidecar counters, matching their header, instead of the
  gzip default they never used; `--tile-format mlt` with
  `--tile-compression brotli` is refused by the library, not only by the
  CLI help. MVT contracts are byte-identical.
- The per-zoom compression policy and the version label provenance
  records for it are defined side by side.
- Library callers passing a compression level the assembler cannot use
  are refused before any IO, in both `run` and `ocean_build`.

Design-hunt fixes, fourth wave. Output-neutral: a fresh denmark build is
byte-identical to the third wave's.

- Env knobs (`ELIVAGAR_ASSEMBLE_WORKERS`, `ELIVAGAR_ASSEMBLE_PARK_BUDGET`,
  `ELIVAGAR_ASSEMBLE_SPLIT_TARGET`, `ELIVAGAR_REL_BLOCKS_CAP`) fail the
  run on a malformed value instead of silently using the default, so a
  results row can no longer claim an arm the run never took.
- A computed-ocean run checks the shapefiles it reads against the key it
  recorded, so a shapefile replaced mid-run is an error rather than a
  silent mismatch between chunks and provenance.
- Retained ocean artifacts carry a key-hash prefix in their name, and
  retention no longer deletes an older copy before its replacement link
  exists. The artifact zoom contract no longer admits a z15 tile.
- Artifact run copies walk per-zoom id ranges rather than every tile id.
- Boundary admin levels have one typed parser; comma-formatted way
  levels such as `0,2` are rejected.
- `inspect` reports whether an archive can be validated.
- A golden test pins a small ocean's decoded tile digest together with
  `OCEAN_POLICY_VERSION`, so an ocean change without a policy bump
  fails the build.

Design-hunt fixes, third wave. This changes output: the ocean artifact
key moves to `OCEAN_POLICY_VERSION` v7, so the world ocean artifact must
be rebuilt with `ocean-build`.

- The polygon rect clipper rounds a clip crossing from a canonical
  endpoint, so two rings sharing an edge in opposite directions cut it
  at the same point; opposite traversals used to land one unit apart and
  leave a sliver of missing fill.
- Sites emits `kind=sports_center` as the Shortbread spec expects, while
  still matching the real OSM tag `leisure=sports_centre`. Street polygon
  labels classify by `aeroway`, and `rail` and `narrow_gauge` take their
  z8 or z10 zoom from `service` as specified.
- Computed-ocean archives record the shapefile source key in their
  contract, so archives built from different water-polygon releases are
  no longer contract-identical. Artifact keys travel as typed values and
  can no longer be dropped from provenance by a JSON round-trip.
- Resume refuses obsolete sort-chunk filenames and a different partition
  split zoom instead of falling back to a legacy merge, and the legacy
  single-reader assemble path is gone.
- Library callers running more than one tilegen in a process get
  markers in the right FIFO and counters reset per run.
- OSM length and number parsing has one implementation; building
  heights accept feet-and-inches values such as `5'6"`.

Design-hunt fixes, second wave. Changes OSM output; ocean is unchanged.

- Shortbread matchers no longer give up when one key carries an unlisted
  value: `amenity=shelter` with `highway=bus_stop`, a railway station
  with any amenity, and similar multi-key objects now match, in
  `public_transport`, `streets`, `street_labels` and the street polygon
  layers.
- Relation rings no longer lose their last vertex during antimeridian
  unwrapping, members are unwrapped before assembly, and triangular
  relation outers are kept.
- `--fanout-cap` counts tiles with the tile buffer, so it no longer
  undercounts, and is accepted only for the OSM polygon layers it can
  affect.
- Unsorted PBF input requires `--allow-unsafe-flat-index` (or
  `--force-sorted`) instead of panicking mid-run on modern node ids.
- Resume refuses a changed `--compress-sort-chunks` (checkpoint v7) and
  a missing or invalid chunk-count record, which previously let a
  crashed ocean phase resume as complete. Compressed chunk writers no
  longer stage a whole chunk in memory.
- MLT archives get the same tile post-processing as MVT, including line
  merging. MVT attribute interning is `u32`, so tiles with more than
  65,535 distinct keys or values no longer alias attributes.
- `verify` caches each payload's whole outcome and checks paint order
  through the profile's own rank recovery. Statistics reset per run
  for library callers that run more than once in a process.
- The PMTiles directory has one codec shared by reader and writer, with
  a round-trip property test.
- New `reference/coordinates.md` describes the coordinate spaces and
  conversions.

Correctness fixes from the 2026-10-28 design hunt, first wave. This
changes output: the ocean artifact key moves to `OCEAN_POLICY_VERSION`
v6, so the world ocean artifact must be rebuilt with `ocean-build`.

- The polygon engine no longer promotes a hole into the outer slot when
  the outer collapses under rescale, simplification or cleanup; the
  shape is dropped instead of emitting a filled hole.
- Multipolygon assembly pairs each inner ring with the smallest outer
  that contains it, tests containment on a vertex off the outer's
  boundary, and classifies role-less rings by containment depth rather
  than winding. Inner rings touching their outer at a shared node no
  longer fill the hole.
- Street labels take kind and zoom from one key order, so a way tagged
  with both aeroway and railway gets the aeroway zoom.
- Relation member ways crossing the antimeridian decode correctly; the
  way index delta decoder saturated instead of wrapping.
- Sort records cap attributes at the count they declare and truncate
  long strings on a UTF-8 boundary instead of emptying them.
- The PMTiles writer refuses out-of-order or duplicate tile ids instead
  of silently corrupting the directory.
- The PMTiles reader decompresses tiles by the header's compression,
  reads metadata as strict UTF-8, and bounds every read by the file.
- Malformed ocean shapefile records are an error rather than skipped;
  `ocean-build` keys the artifact on the bytes it actually read.
- `verify` enforces the MVT command grammar and checks ocean rings with
  an overflow-safe decoder. The corpus gate's Strict decoder rejects
  malformed command streams, duplicate fields and missing geometry.
- `inspect` shows the real section layout, parses metadata as JSON, and
  reports the contract exactly as the corpus gate reads it; a provenance
  block without a build member is a complete contract.
- `svg` and `diag` refuse out-of-range tile coordinates; `svg` colours
  and orders layers by identity and rejects unknown `-l` names.
  Oversized byte-size arguments are refused instead of wrapping.
- The line merger no longer joins lines through a point on the buffered
  tile boundary. Two lines leaving a tile through the same quantized
  point were spliced into an out-and-back spike (three on norway's admin
  boundaries, the largest nearly a quarter of a tile). Affected lines are
  now split there instead; no line coverage changes. A witness-based
  invariant checker verifies every merge in debug builds.
- The boundary-line oracle gates on duplicate sub-lines only; palindrome
  and spur counts are advisory, with a retrace-extent census. They cannot
  tell degenerate real geometry from a fabricated spike.
- Bumped protohoggr 0.4 to 0.5, hotpath 0.27 to 0.29, mlt-core 0.16 to
  0.22 and pmtiles 0.24 to 0.25 (dev). The MLT encoder now asks for
  mlt-core's default consumer profile explicitly, since 0.21 replaced
  the default encoder config with per-consumer profiles.

Polygon engine brought up to i_overlay 9.0.0. This changes output: the
ocean artifact key moves to `OCEAN_POLICY_VERSION` v5, so the world
ocean artifact must be rebuilt with `ocean-build`.

- Fixed the fragment splitter's column-border lookup (upstream issue
  87). Vertical edges on a column border - in practice the right edge
  of every tile clip on the largest ocean pieces - were not split where
  other edges ended on them. Denmark: 14 ocean tiles change, all inside
  the right-hand tile buffer.
- The fragment splitter no longer drops crossings and collinear overlaps
  whose rounded point or overlap start falls in a neighbouring column.
- Collinear output cleanup now runs after hole binding (upstream issue
  91). Cleaning first could leave two touching result shapes in a tie
  the binder cannot order, which panicked or attached a hole to the
  wrong shape.
- Snap-radius growth is capped at the i32 engine's limit and saturates.
- Bumped i_overlay (dev, the differential oracle) 8 to 9, hotpath 0.25
  to 0.27, mlt-core to 0.16. Dropped the unused `toml` dependency and
  moved `geo-types` behind the `mlt` feature, its only user.

Dependency refresh, verified output-neutral: fresh denmark locations
build passes the committed corpus digest unchanged.

- Bumped brotli 8 to 9, hotpath 0.23 to 0.25, mlt-core 0.12.7,
  pmtiles 0.24 (dev), and rust-version to 1.98.
- Fixed the one lint the new toolchain surfaced (`needless_range_loop`
  in the MVT merge pool-reclaim loop); no behavior change.

## 0.1.0

Initial release. Reads an OSM PBF and writes a PMTiles v3 archive carrying
the full Shortbread schema (26 layers, z0-z14).

### Output

- Full Shortbread 1.0 schema: all 26 layers, z0-z14, with paint-order
  ranking and per-layer zoom ranges.
- PMTiles v3 writer with Hilbert tile IDs, content deduplication, a
  streaming directory, and an in-place layout whose finalize step is a
  rename. The data section is 4K-aligned, so tiles can be served with
  `O_DIRECT` / `io_uring` without page-cache pollution; this is
  backwards-compatible with all PMTiles readers.
- Provenance block in archive metadata recording input identity, the full
  build config, ocean source key and toolchain versions. `Input` plus
  `Config` is the comparability contract between two archives.
- MVT output with gzip or brotli compression. MLT output exists behind the
  non-default `mlt` cargo feature and is not validated against clients.

### Geometry

- Integer polygon geometry engine used by ocean and OSM layers alike:
  early quantization to max-zoom pixel space (unclamped, antimeridian-safe),
  exact shift-round per-zoom rescale, rotation-invariant pin-aware integer
  simplification, and Simplify/Intersect topology ops over an in-tree
  boolean engine ported from i_overlay (which remains only as a
  dev-dependency differential oracle).
- Recursive tile-pyramid descent for all polygon emission, replacing
  per-zoom row bands, gap runs and per-tile boolean clips. Geometry shrinks
  geometrically with depth, and uniform subtrees short-circuit to canonical
  full-tile records.
- Polygons carrying more than 500 contours are partitioned by recursive
  bisection, because MapLibre's `classifyRings` clamps each classified
  polygon at 500 rings and silently drops the rest. Halves share their
  integer cut coordinate, so pieces abut exactly under nonzero fill.
- Shared boundaries between polygon features cannot open seams under
  simplification: vertices shared between ways are pinned through
  simplification at every zoom (via pbfhogg's injected shared-node pins
  or their runtime fallbacks). An earlier assemble-side seam-reconcile
  pass and its `--seam-reconcile-layers` flag were removed as redundant
  with this mechanism.
- Per-layer fanout caps (`--fanout-cap`) as the policy backstop for
  pathological features.

### Ocean

- Ocean input is `--ocean`, repeatable, and nothing is auto-detected: omit
  it and the archive has no ocean. Accepts zoom-ranged shapefiles
  (`z0-z14`, or the `z0-z7` + `z8-z14` pair) plus an optional precomputed
  artifact.
- `elivagar ocean-build` produces a durable world-ocean PMTiles artifact,
  built once per shapefile release. An extract then computes only the
  boundary band near its bbox edge and merges the artifact for the
  interior. The artifact key (shapefile hashes + policy version) is
  re-validated every run, and a stale artifact fails loud.
- The z0-z7 pass unions its source pieces before descent, eliminating the
  cross-cell seam wedges and min-area fragment drops that per-cell descent
  produced at low zoom.

### Pipeline

- Single-pass PBF read with parallel feature processing. Sorted PBFs use a
  compact in-RAM node store with FOR compression; unsorted input falls back
  to a flat mmap index behind size guardrails.
- `--locations-on-ways` support, auto-detected from the PBF header, plus
  consumption of pbfhogg's injected relation plan and exact shared-node
  pins when present. Both have first-class fallbacks for raw input.
- External merge sort partitioned by Hilbert tile-id range at write time,
  with parallel per-partition readers feeding a byte-budgeted claim window
  in assemble. Hot partitions split into tile-range pieces so one dense
  z14 block cannot stall the writer.
- Optional sort-chunk compression (`--compress-sort-chunks lz4|snappy`),
  intended for planet-scale runs where scratch exceeds RAM.
- `--skip-to ocean|sort|assemble` checkpoint resume, validating input
  identity, producer config and ocean source against the current run so a
  resume cannot blend two contracts into one archive.
- Bounded memory throughout: sort chunk budget, capped relation-block
  buffer with a verified spill path, mmap'd way index, and a dedup map
  capped at 1M entries.
- Packing-invariant way-block admission: concurrency is bounded by the
  thread count, with a raw-byte budget (`--way-budget`) as a safety net
  against individually huge blocks - upstream blob packing (8,000-element
  extracts vs planet's ~66,500) cannot throttle phase12.

### Tooling

- `elivagar verify` - container integrity, metadata schema, tile
  decompression, MVT structure, geometry command validation, layer
  coverage, ocean ring self-intersection checks.
- `elivagar inspect` - header, tile statistics, section layout, provenance
  and metadata.
- `elivagar svg` and `elivagar diag` - single-tile SVG rendering and ring
  winding diagnosis for visual inspection.

### Validated at

Planet, 2026-07-31: a 90.5 GB locations-on-ways PBF to a 58.7 GiB archive
in 571.7s on 16 cores / 30.5 GiB RAM, peak RSS 12.7 GB, 269.8M tiles
addressed and 52.2M unique. See `notes/planet-30gb-roadmap.md` for the
measurement record and the disclosures that belong with any comparison.
