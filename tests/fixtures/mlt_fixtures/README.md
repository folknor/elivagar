Fixture set for MLT adapter roundtrip coverage.

These fixtures are geometry-command inputs (MVT command streams) that our MLT
adapter consumes, then encodes via upstream `mlt-core`.

Scope:
- Single point, line, polygon
- MultiPoint, MultiLineString, MultiPolygon
- Property-column behavior: sparse columns, typed numeric/bool columns. Keys are profile attribute keys: a column's type comes from the key schema, so a fixture key must be one the profile declares, with a value of its declared type

Runtime/CI note:
- Tests read these committed fixture files directly.
- No network download is required for `cargo test` or CI.
