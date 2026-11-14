# Installation


## From crates.io

```sh
cargo install elivagar
```

That builds the binary from source with release optimizations. Requires Rust
1.97 or newer; edition 2024, stable toolchain, no nightly features.

## As a library

```toml
[dependencies]
elivagar = "0.1"
```

The public API is `elivagar::run` over a `TilegenConfig`, plus the decode and
write surface in `elivagar::archive` (the PMTiles reader `Archive`, the typed
header, and the writer's `add_run` / `set_metadata_verbatim`), the
`tile_detail` MVT decoder with selectable unknown-field strictness, and typed
provenance.

Archive read operations (including header decode and tile-contract checks)
and `tile_detail` decoders return `DecodeError`, with `kind()` and `locus()`.
`DecodeErrorKind` distinguishes malformed bytes, unsupported formats, OS I/O
and opaque failures; `source()` retains the original decoder or I/O cause.
`inspect`, SVG rendering and diag return `PipelineError` and preserve this
classification. Conversion into `std::io::Error` retains the typed cause for
adapters. Directory encoding and writer configuration keep their I/O surface.


`TilegenConfig::ocean` is `Option<OceanInputs>`: the full-resolution shapefile
is required when ocean is enabled, with optional simplified and artifact paths.
`OceanInputs::parse_specs` accepts the CLI's repeatable ocean syntax.
Use `SimplifyFactor::new` for a finite multiplier in 0.1..=10 (default 1),
and `FanoutCaps::parse_specs` or `FanoutCaps::set` for checked layer caps.
`run` rejects MLT without the `mlt` build feature before phase work.
`run` and `ocean_build` return a `PipelineError` whose `kind()` classifies the
failure (invalid request, refused input, resume or ocean artifact, OS I/O,
internal, or unclassified); the binary exits with the matching code from the
[CLI reference](../reference/cli).

Library callers can inspect `PipelineError::artifact_refusal()` for a typed
`ArtifactRefusal`. `KeyMismatch::fields` names every differing identity field
(policy version, shapefile hashes, zooms, or compression level). Artifact
admission and directory refusals exit 4; OS read failures exit 74. Shapefile
hashing failures retain their own failure class.

Sort chunk readers and writer `resume`/`finish` return `sort::ChunkStoreError`, with
`kind()` distinguishing compression, stale names, missing chunks, counts,
corruption and I/O. Pipeline resume refusals expose these through
`ResumeRefusal::ChunkStore { kind }`. Invalid retained chunks exit 3; fresh
run read-back failures stay unclassified (2), and OS failures retain 74.

Verification reports expose `tile_errors: Vec<verify::TileFinding>` with
coordinates and a shared typed `verify::Finding`. `tile_error_count` includes
findings beyond the 100 retained diagnostics. Traversal continues past that
cap. `verify::PaintOrderError` distinguishes attribute and rank failures.
`VerifyError` retains typed decode causes for container and metadata failures
and converts to `PipelineError`. Operational tile reads abort (74), opaque
failures abort (2), and rejected tile bytes fail the completed report (1).

## From source

```sh
git clone https://github.com/folknor/elivagar
cd elivagar
cargo build --release
```

The binary lands at `target/release/elivagar`.

## Build features

| Feature | Default | Description |
|---|---|---|
| `mlt` | no | MapLibre Tile output via `mlt-core` |
| `hotpath` | no | Function-level timing instrumentation |
| `hotpath-alloc` | no | Per-function allocation tracking |

### MLT output

```sh
cargo build --release --features mlt
```

MLT is off by default deliberately. The encoder is real - it calls upstream
`mlt-core` and round-trips through it in tests covering point, line, polygon
and multi-geometries - but no client has ever consumed its output, the path
writes uncompressed payloads, and none of the
[correctness gates](./correctness) cover it. `mlt-core` is also pre-1.0 and
moving. Without the feature, `--tile-format mlt` refuses with the missing
feature named rather than silently falling back to MVT.

Treat it as experimental and validate against your renderer before serving it.

## Platform

Developed on Linux, untested elsewhere. The pipeline uses `madvise` and
`fadvise` I/O hints, positional reads, and a 4K-aligned output layout intended
for `O_DIRECT` serving.

## Requirements

Runtime has no external dependencies - no `protoc`, no C protobuf library. All
protobuf encoding and decoding is hand-rolled wire format.

Plan for disk: temporary sort chunks land beside the output and are roughly
proportional to feature volume. `--compress-sort-chunks lz4` cuts that by
about 2.6x at the cost of a few percent of wall time, which is the
planet-scale configuration.

## Verifying the installation

```sh
elivagar --version
elivagar --help
```
