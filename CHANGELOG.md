# Changelog

## Unreleased

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
