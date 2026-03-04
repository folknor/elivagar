Fixture set for MLT adapter roundtrip coverage.

These fixtures are geometry-command inputs (MVT command streams) that our MLT
adapter consumes, then encodes via upstream `mlt-core`.

Scope:
- Single point, line, polygon
- MultiPoint, MultiLineString, MultiPolygon
- Property-column behavior: mixed-type fallback, sparse columns, typed numeric/bool columns

Runtime/CI note:
- Tests read these committed fixture files directly.
- No network download is required for `cargo test` or CI.
