Targeted fixtures imported from Mapbox `mvt-fixtures` for MVT conformance tests.

Source:
- Repository: `https://github.com/mapbox/mvt-fixtures`
- Commit: `7243184`
- Imported fixture IDs: `017`, `018`, `019`, `020`, `021`, `022`

These fixtures are the canonical valid geometry examples from the vector tile
spec (point, linestring, polygon, multipoint, multilinestring, multipolygon).

Runtime/CI note:
- Tests read these committed fixture files directly.
- No network download is required for `cargo test` or CI.
- Any local `.cache/` clone is maintenance-only scratch space and is not used
  by the test harness.
