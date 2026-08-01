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
write surface: the PMTiles reader, the `tile_detail` MVT decoder with
selectable unknown-field strictness, typed provenance, and the writer's
`add_run` / `set_metadata_verbatim`.

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
