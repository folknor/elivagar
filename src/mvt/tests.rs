#![allow(clippy::unwrap_used)]

use super::merge::{test_append_geometry, test_decode_line_segments};
use super::*;
use protohoggr::{Cursor, WIRE_LEN};

#[derive(Debug, PartialEq, Eq)]
struct ParsedFixtureFeature {
    feature_id: Option<u64>,
    feature_type: u64,
    feature_tags: Vec<u32>,
    feature_geometry: Vec<u32>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedFixtureLayer {
    layer_name: String,
    layer_version: u64,
    layer_extent: Option<u64>,
    keys: Vec<String>,
    string_values: Vec<String>,
    features: Vec<ParsedFixtureFeature>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedFixtureTile {
    layers: Vec<ParsedFixtureLayer>,
}

#[derive(Debug, PartialEq, Eq)]
struct ParsedFixtureSingleFeature {
    layer_name: String,
    layer_version: u64,
    layer_extent: Option<u64>,
    key: String,
    string_value: String,
    feature_id: Option<u64>,
    feature_type: u64,
    feature_tags: Vec<u32>,
    feature_geometry: Vec<u32>,
}

fn parse_fixture_tile(bytes: &[u8]) -> Result<ParsedFixtureTile, String> {
    let mut tile_cursor = Cursor::new(bytes);
    let mut layers = Vec::new();
    while let Some((field, wire)) = tile_cursor
        .read_tag()
        .map_err(|e| format!("read tile tag: {e}"))?
    {
        if field != 3 || wire != WIRE_LEN {
            tile_cursor
                .skip_field(wire)
                .map_err(|e| format!("skip tile field {field}: {e}"))?;
            continue;
        }
        let layer_buf = tile_cursor
            .read_len_delimited()
            .map_err(|e| format!("read tile.layers: {e}"))?;
        layers.push(parse_fixture_layer(layer_buf)?);
    }
    Ok(ParsedFixtureTile { layers })
}

fn parse_fixture_layer(layer_buf: &[u8]) -> Result<ParsedFixtureLayer, String> {
    let mut layer_name = String::new();
    let mut layer_version = 0u64;
    let mut layer_extent: Option<u64> = None;
    let mut keys = Vec::new();
    let mut string_values = Vec::new();
    let mut features = Vec::new();

    let mut layer_cursor = Cursor::new(layer_buf);
    while let Some((field, wire)) = layer_cursor
        .read_tag()
        .map_err(|e| format!("read layer tag: {e}"))?
    {
        match (field, wire) {
            (15, WIRE_VARINT) => {
                layer_version = layer_cursor
                    .read_varint()
                    .map_err(|e| format!("read layer.version: {e}"))?;
            }
            (1, WIRE_LEN) => {
                layer_name = String::from_utf8_lossy(
                    layer_cursor
                        .read_len_delimited()
                        .map_err(|e| format!("read layer.name: {e}"))?,
                )
                .to_string();
            }
            (5, WIRE_VARINT) => {
                layer_extent = Some(
                    layer_cursor
                        .read_varint()
                        .map_err(|e| format!("read layer.extent: {e}"))?,
                );
            }
            (3, WIRE_LEN) => {
                let key = String::from_utf8_lossy(
                    layer_cursor
                        .read_len_delimited()
                        .map_err(|e| format!("read layer.keys: {e}"))?,
                )
                .to_string();
                keys.push(key);
            }
            (4, WIRE_LEN) => {
                let value_msg = layer_cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read layer.values: {e}"))?;
                string_values.push(parse_fixture_value_string(value_msg)?);
            }
            (2, WIRE_LEN) => {
                let feature_msg = layer_cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read layer.features: {e}"))?;
                features.push(parse_fixture_feature(feature_msg)?);
            }
            _ => {
                layer_cursor
                    .skip_field(wire)
                    .map_err(|e| format!("skip layer field {field}: {e}"))?;
            }
        }
    }

    Ok(ParsedFixtureLayer {
        layer_name,
        layer_version,
        layer_extent,
        keys,
        string_values,
        features,
    })
}

fn parse_fixture_value_string(value_msg: &[u8]) -> Result<String, String> {
    let mut vcur = Cursor::new(value_msg);
    while let Some((field, wire)) = vcur
        .read_tag()
        .map_err(|e| format!("read value tag: {e}"))?
    {
        if field == 1 && wire == WIRE_LEN {
            return Ok(String::from_utf8_lossy(
                vcur.read_len_delimited()
                    .map_err(|e| format!("read value.string: {e}"))?,
            )
            .to_string());
        }
        vcur.skip_field(wire)
            .map_err(|e| format!("skip value field {field}: {e}"))?;
    }
    Ok(String::new())
}

fn parse_fixture_feature(feature_msg: &[u8]) -> Result<ParsedFixtureFeature, String> {
    let mut feature_id = None;
    let mut feature_type = 0u64;
    let mut feature_tags = Vec::new();
    let mut feature_geometry = Vec::new();

    let mut fcur = Cursor::new(feature_msg);
    while let Some((field, wire)) = fcur
        .read_tag()
        .map_err(|e| format!("read feature tag: {e}"))?
    {
        match (field, wire) {
            (1, WIRE_VARINT) => {
                feature_id = Some(
                    fcur.read_varint()
                        .map_err(|e| format!("read feature.id: {e}"))?,
                );
            }
            (2, WIRE_LEN) => {
                let packed = fcur
                    .read_len_delimited()
                    .map_err(|e| format!("read feature.tags: {e}"))?;
                feature_tags = parse_packed_u32(packed, "feature.tags")?;
            }
            (3, WIRE_VARINT) => {
                feature_type = fcur
                    .read_varint()
                    .map_err(|e| format!("read feature.type: {e}"))?;
            }
            (4, WIRE_LEN) => {
                let packed = fcur
                    .read_len_delimited()
                    .map_err(|e| format!("read feature.geometry: {e}"))?;
                feature_geometry = parse_packed_u32(packed, "feature.geometry")?;
            }
            _ => {
                fcur.skip_field(wire)
                    .map_err(|e| format!("skip feature field {field}: {e}"))?;
            }
        }
    }

    Ok(ParsedFixtureFeature {
        feature_id,
        feature_type,
        feature_tags,
        feature_geometry,
    })
}

fn parse_packed_u32(buf: &[u8], ctx: &str) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    let mut cur = Cursor::new(buf);
    while !cur.is_empty() {
        let val = cur
            .read_varint()
            .map_err(|e| format!("read {ctx} varint: {e}"))?;
        let n = u32::try_from(val).map_err(|_| format!("{ctx} value out of u32 range: {val}"))?;
        out.push(n);
    }
    Ok(out)
}

fn parse_single_layer_single_feature(bytes: &[u8]) -> Result<ParsedFixtureSingleFeature, String> {
    let parsed = parse_fixture_tile(bytes)?;
    if parsed.layers.len() != 1 {
        return Err(format!("expected 1 layer, got {}", parsed.layers.len()));
    }
    let layer = &parsed.layers[0];
    if layer.features.len() != 1 {
        return Err(format!("expected 1 feature, got {}", layer.features.len()));
    }
    let feature = &layer.features[0];
    if feature.feature_tags.len() % 2 != 0 {
        return Err("feature tags must be key/value pairs".to_string());
    }

    let (key, string_value) = if feature.feature_tags.len() >= 2 {
        let key_idx = usize::try_from(feature.feature_tags[0])
            .map_err(|_| "key index conversion failed".to_string())?;
        let val_idx = usize::try_from(feature.feature_tags[1])
            .map_err(|_| "value index conversion failed".to_string())?;
        let key = layer
            .keys
            .get(key_idx)
            .ok_or_else(|| format!("key index out of bounds: {key_idx}"))?
            .clone();
        let string_value = layer
            .string_values
            .get(val_idx)
            .ok_or_else(|| format!("value index out of bounds: {val_idx}"))?
            .clone();
        (key, string_value)
    } else {
        (String::new(), String::new())
    };

    Ok(ParsedFixtureSingleFeature {
        layer_name: layer.layer_name.clone(),
        layer_version: layer.layer_version,
        layer_extent: layer.layer_extent,
        key,
        string_value,
        feature_id: feature.feature_id,
        feature_type: feature.feature_type,
        feature_tags: feature.feature_tags.clone(),
        feature_geometry: feature.feature_geometry.clone(),
    })
}

#[test]
fn test_zigzag() {
    assert_eq!(zigzag(0), 0);
    assert_eq!(zigzag(-1), 1);
    assert_eq!(zigzag(1), 2);
    assert_eq!(zigzag(-2), 3);
    assert_eq!(zigzag(2), 4);
}

#[test]
fn test_varint_encoding() {
    use protohoggr::encode_varint;

    let mut buf = Vec::new();
    encode_varint(&mut buf, 0);
    assert_eq!(buf, [0]);

    buf.clear();
    encode_varint(&mut buf, 1);
    assert_eq!(buf, [1]);

    buf.clear();
    encode_varint(&mut buf, 127);
    assert_eq!(buf, [127]);

    buf.clear();
    encode_varint(&mut buf, 128);
    assert_eq!(buf, [0x80, 0x01]);

    buf.clear();
    encode_varint(&mut buf, 300);
    assert_eq!(buf, [0xAC, 0x02]);
}

#[test]
fn test_command_encoding() {
    assert_eq!(command(1, 1), 9); // MoveTo, count=1
    assert_eq!(command(2, 3), 26); // LineTo, count=3
    assert_eq!(command(7, 1), 15); // ClosePath, count=1
}

#[test]
fn test_encode_point() {
    let mut cmds = Vec::new();
    encode_point(&mut cmds, 25, 17);
    assert_eq!(
        cmds,
        vec![
            9,  // MoveTo, count=1
            50, // zigzag(25) = 50
            34, // zigzag(17) = 34
        ]
    );
}

#[test]
fn test_encode_linestring() {
    let coords = [(2, 1), (4, 3), (6, 5)];
    let mut cmds = Vec::new();
    encode_linestring(&mut cmds, &coords);
    assert_eq!(
        cmds,
        vec![
            9,  // MoveTo count=1
            4,  // zigzag(2)
            2,  // zigzag(1)
            18, // LineTo count=2
            4,  // zigzag(4-2=2)
            4,  // zigzag(3-1=2)
            4,  // zigzag(6-4=2)
            4,  // zigzag(5-3=2)
        ]
    );
}

#[test]
fn test_encode_polygon() {
    // Simple square: (0,0) -> (10,0) -> (10,10) -> (0,10) -> (0,0)
    let ring = [(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let mut cmds = Vec::new();
    encode_polygon(&mut cmds, &[&ring]);
    // MoveTo(0,0): cmd=9, zigzag(0)=0, zigzag(0)=0
    // LineTo 3 points: cmd=26
    //   delta(10,0): 20, 0
    //   delta(0,10): 0, 20
    //   delta(-10,0): 19, 0
    // ClosePath: cmd=15
    assert_eq!(cmds, vec![9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15]);
}

#[test]
fn test_layer_builder_key_interning() {
    let mut layer = LayerBuilder::new("test");
    let k1 = layer.intern_key("name");
    let k2 = layer.intern_key("highway");
    let k3 = layer.intern_key("name");
    assert_eq!(k1, 0);
    assert_eq!(k2, 1);
    assert_eq!(k3, 0); // dedup
}

#[test]
fn test_layer_builder_value_interning() {
    let mut layer = LayerBuilder::new("test");
    let v1 = layer.intern_value(Value::String("residential".into()));
    let v2 = layer.intern_value(Value::Bool(true));
    let v3 = layer.intern_value(Value::String("residential".into()));
    assert_eq!(v1, 0);
    assert_eq!(v2, 1);
    assert_eq!(v3, 0); // dedup
}

#[test]
fn test_encode_tile_produces_bytes() {
    let mut layer = LayerBuilder::new("streets");
    let ki = layer.intern_key("kind");
    let vi = layer.intern_value(Value::String("motorway".into()));
    let mut geom = Vec::new();
    encode_linestring(&mut geom, &[(100, 200), (300, 400)]);
    layer.add_feature(Feature {
        id: Some(42),
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(ki, vi)],
    });

    let buf = encode_tile(&[&layer]);
    assert!(!buf.is_empty());
    // Verify outer field tag: field 3, wire type 2
    assert_eq!(buf[0], (3 << 3) | 2);
}

#[test]
fn test_encode_empty_tile() {
    let layer = LayerBuilder::new("empty");
    let buf = encode_tile(&[&layer]);
    assert!(buf.is_empty());
}

fn assert_matches_mvt_fixture(fixture_id: &str, geom_type: GeomType, geometry: &[u32]) {
    let mut layer = LayerBuilder::new("hello");
    let key_idx = layer.intern_key("hello");
    let val_idx = layer.intern_value(Value::String("world".to_string()));
    layer.add_feature(Feature {
        id: Some(1),
        geom_type,
        geometry: geometry.to_vec(),
        tags: vec![(key_idx, val_idx)],
    });
    let encoded = encode_tile(&[&layer]);

    let expected: &[u8] = match fixture_id {
        "017" => include_bytes!("../../tests/fixtures/mvt_fixtures/017/tile.mvt"),
        "018" => include_bytes!("../../tests/fixtures/mvt_fixtures/018/tile.mvt"),
        "019" => include_bytes!("../../tests/fixtures/mvt_fixtures/019/tile.mvt"),
        "020" => include_bytes!("../../tests/fixtures/mvt_fixtures/020/tile.mvt"),
        "021" => include_bytes!("../../tests/fixtures/mvt_fixtures/021/tile.mvt"),
        "022" => include_bytes!("../../tests/fixtures/mvt_fixtures/022/tile.mvt"),
        _ => panic!("unknown fixture id: {fixture_id}"),
    };

    let expected_parsed = parse_single_layer_single_feature(expected)
        .unwrap_or_else(|e| panic!("fixture {fixture_id} parse failed: {e}"));
    let actual_parsed = parse_single_layer_single_feature(&encoded)
        .unwrap_or_else(|e| panic!("fixture {fixture_id} encoded parse failed: {e}"));

    // Upstream fixtures sometimes omit default-encoded fields like extent.
    assert_eq!(
        actual_parsed.layer_name, expected_parsed.layer_name,
        "fixture {fixture_id} layer_name"
    );
    assert_eq!(
        actual_parsed.layer_version, expected_parsed.layer_version,
        "fixture {fixture_id} version"
    );
    assert_eq!(
        actual_parsed.key, expected_parsed.key,
        "fixture {fixture_id} key"
    );
    assert_eq!(
        actual_parsed.string_value, expected_parsed.string_value,
        "fixture {fixture_id} value"
    );
    assert_eq!(
        actual_parsed.feature_id, expected_parsed.feature_id,
        "fixture {fixture_id} id"
    );
    assert_eq!(
        actual_parsed.feature_type, expected_parsed.feature_type,
        "fixture {fixture_id} type"
    );
    assert_eq!(
        actual_parsed.feature_tags, expected_parsed.feature_tags,
        "fixture {fixture_id} tags"
    );
    assert_eq!(
        actual_parsed.feature_geometry, expected_parsed.feature_geometry,
        "fixture {fixture_id} geometry"
    );

    // Our encoder always writes extent=4096 for spec compliance.
    assert_eq!(
        actual_parsed.layer_extent,
        Some(4096),
        "fixture {fixture_id} extent"
    );
}

#[test]
fn fixture_parser_supports_multi_layer_multi_feature_tiles() {
    let mut layer_a = LayerBuilder::new("layer_a");
    let ka = layer_a.intern_key("kind");
    let va = layer_a.intern_value(Value::String("a".to_string()));
    let mut ga1 = Vec::new();
    encode_point(&mut ga1, 1, 2);
    layer_a.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::Point,
        geometry: ga1,
        tags: vec![(ka, va)],
    });
    let mut ga2 = Vec::new();
    encode_point(&mut ga2, 3, 4);
    layer_a.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::Point,
        geometry: ga2,
        tags: vec![(ka, va)],
    });

    let mut layer_b = LayerBuilder::new("layer_b");
    let kb = layer_b.intern_key("kind");
    let vb = layer_b.intern_value(Value::String("b".to_string()));
    let mut gb = Vec::new();
    encode_linestring(&mut gb, &[(0, 0), (8, 8)]);
    layer_b.add_feature(Feature {
        id: Some(3),
        geom_type: GeomType::LineString,
        geometry: gb,
        tags: vec![(kb, vb)],
    });

    let encoded = encode_tile(&[&layer_a, &layer_b]);
    let parsed = parse_fixture_tile(&encoded).expect("multi-layer tile parse should succeed");
    assert_eq!(parsed.layers.len(), 2);
    assert_eq!(parsed.layers[0].layer_name, "layer_a");
    assert_eq!(parsed.layers[0].features.len(), 2);
    assert_eq!(parsed.layers[1].layer_name, "layer_b");
    assert_eq!(parsed.layers[1].features.len(), 1);
}

#[test]
fn conformance_fixture_malformed_truncated_tile_rejected() {
    let fixture = include_bytes!("../../tests/fixtures/mvt_fixtures/017/tile.mvt");
    let truncated = &fixture[..fixture.len() - 1];
    assert!(parse_fixture_tile(truncated).is_err());
}

#[test]
fn conformance_fixture_malformed_invalid_wire_type_rejected() {
    // field=3, wire=7 (invalid protobuf wire type)
    let malformed = [0x1F];
    assert!(parse_fixture_tile(&malformed).is_err());
}

#[test]
fn conformance_fixture_017_valid_point_geometry() {
    // mapbox/mvt-fixtures#017
    assert_matches_mvt_fixture("017", GeomType::Point, &[9, 50, 34]);
}

#[test]
fn conformance_fixture_018_valid_linestring_geometry() {
    // mapbox/mvt-fixtures#018
    assert_matches_mvt_fixture("018", GeomType::LineString, &[9, 4, 4, 18, 0, 16, 16, 0]);
}

#[test]
fn conformance_fixture_019_valid_polygon_geometry() {
    // mapbox/mvt-fixtures#019
    assert_matches_mvt_fixture(
        "019",
        GeomType::Polygon,
        &[9, 6, 12, 18, 10, 12, 24, 44, 15],
    );
}

#[test]
fn conformance_fixture_020_valid_multipoint_geometry() {
    // mapbox/mvt-fixtures#020
    assert_matches_mvt_fixture("020", GeomType::Point, &[17, 10, 14, 3, 9]);
}

#[test]
fn conformance_fixture_021_valid_multilinestring_geometry() {
    // mapbox/mvt-fixtures#021
    assert_matches_mvt_fixture(
        "021",
        GeomType::LineString,
        &[9, 4, 4, 18, 0, 16, 16, 0, 9, 17, 17, 10, 4, 8],
    );
}

#[test]
fn conformance_fixture_022_valid_multipolygon_geometry() {
    // mapbox/mvt-fixtures#022
    assert_matches_mvt_fixture(
        "022",
        GeomType::Polygon,
        &[
            9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15, 9, 22, 2, 26, 18, 0, 0, 18, 17, 0, 15, 9, 4, 13,
            26, 0, 8, 8, 0, 0, 7, 15,
        ],
    );
}

// -----------------------------------------------------------------------
// append_geometry tests
// -----------------------------------------------------------------------

#[test]
fn test_append_geometry_single_linestring() {
    // Encode a linestring: (10,20) -> (30,40)
    let mut src = Vec::new();
    encode_linestring(&mut src, &[(10, 20), (30, 40)]);
    let mut dest = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    test_append_geometry(&mut dest, &src, &mut cx, &mut cy);
    // First geometry appended with cursor at (0,0) should be identical to source
    assert_eq!(dest, src);
    assert_eq!(cx, 30);
    assert_eq!(cy, 40);
}

#[test]
fn test_append_geometry_two_linestrings() {
    // First linestring: (10,20) -> (30,40)
    let mut src1 = Vec::new();
    encode_linestring(&mut src1, &[(10, 20), (30, 40)]);
    // Second linestring: (5,5) -> (15,15)
    let mut src2 = Vec::new();
    encode_linestring(&mut src2, &[(5, 5), (15, 15)]);

    let mut dest = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    test_append_geometry(&mut dest, &src1, &mut cx, &mut cy);
    assert_eq!(cx, 30);
    assert_eq!(cy, 40);
    test_append_geometry(&mut dest, &src2, &mut cx, &mut cy);
    assert_eq!(cx, 15);
    assert_eq!(cy, 15);

    // Decode the concatenated geometry back to absolute coordinates
    let coords = decode_commands_to_abs(&dest);
    // Should have: MoveTo(10,20), LineTo(30,40), MoveTo(5,5), LineTo(15,15)
    assert_eq!(coords, vec![(10, 20), (30, 40), (5, 5), (15, 15)]);
}

#[test]
fn test_append_geometry_polygon_closepath() {
    // Triangle polygon: (0,0) -> (100,0) -> (50,100) -> close
    let ring = [(0, 0), (100, 0), (50, 100), (0, 0)];
    let mut src = Vec::new();
    encode_polygon(&mut src, &[&ring]);

    let mut dest = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    test_append_geometry(&mut dest, &src, &mut cx, &mut cy);
    // ClosePath does not move the cursor (MVT spec 4.3.3.3): it stays at
    // the last LineTo vertex (50,100).
    assert_eq!(cx, 50);
    assert_eq!(cy, 100);
}

#[test]
fn test_append_geometry_two_polygons_cursor_continuity() {
    // Polygon 1: square at (0,0)
    let ring1 = [(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
    let mut src1 = Vec::new();
    encode_polygon(&mut src1, &[&ring1]);

    // Polygon 2: square at (100,100)
    let ring2 = [(100, 100), (110, 100), (110, 110), (100, 110), (100, 100)];
    let mut src2 = Vec::new();
    encode_polygon(&mut src2, &[&ring2]);

    let mut dest = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    test_append_geometry(&mut dest, &src1, &mut cx, &mut cy);
    // Cursor stays at ring1's last LineTo vertex = (0,10)
    assert_eq!(cx, 0);
    assert_eq!(cy, 10);
    test_append_geometry(&mut dest, &src2, &mut cx, &mut cy);
    // Cursor stays at ring2's last LineTo vertex = (100,110)
    assert_eq!(cx, 100);
    assert_eq!(cy, 110);

    // Decode and verify all absolute positions are correct
    let coords = decode_commands_to_abs(&dest);
    // ring1: MoveTo(0,0), LineTo(10,0), LineTo(10,10), LineTo(0,10)
    // ring2: MoveTo(100,100), LineTo(110,100), LineTo(110,110), LineTo(100,110)
    assert_eq!(coords[0], (0, 0));
    assert_eq!(coords[1], (10, 0));
    assert_eq!(coords[4], (100, 100));
    assert_eq!(coords[5], (110, 100));
}

/// Decode MVT commands into absolute (x,y) coordinates (ignoring ClosePath).
fn decode_commands_to_abs(cmds: &[u32]) -> Vec<(i32, i32)> {
    let mut result = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    let mut i = 0;
    while i < cmds.len() {
        let cmd = cmds[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;
        match cmd_id {
            1 | 2 => {
                for _ in 0..cmd_count {
                    let dx = decode_zigzag(cmds[i]);
                    let dy = decode_zigzag(cmds[i + 1]);
                    cx += dx;
                    cy += dy;
                    result.push((cx, cy));
                    i += 2;
                }
            }
            7 => {
                // ClosePath: cursor unchanged (MVT spec 4.3.3.3) - it stays
                // at the last LineTo vertex, matching MapLibre's decoder.
            }
            _ => {}
        }
    }
    result
}

/// Regression test: appending a multi-ring polygon (exterior + hole) as a
/// single source geometry. Both encoder and decoder must use MVT-spec cursor
/// semantics (ClosePath does not move the cursor); the hole's MoveTo delta is
/// relative to the exterior's LAST LineTo vertex. The historical bug encoded
/// the delta relative to the exterior's MoveTo, displacing every ring after
/// the first in spec-compliant decoders (MapLibre).
#[test]
fn test_append_geometry_multi_ring_polygon_spec_cursor_semantics() {
    // Exterior ring: (100,100) → (200,100) → (200,200) → (100,200) → close
    // Hole ring:     (120,120) → (180,120) → (180,180) → (120,180) → close
    let ext = [(100, 100), (200, 100), (200, 200), (100, 200), (100, 100)];
    let hole = [(120, 120), (180, 120), (180, 180), (120, 180), (120, 120)];
    let mut src = Vec::new();
    encode_polygon(&mut src, &[&ext, &hole]);

    let mut dest = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    test_append_geometry(&mut dest, &src, &mut cx, &mut cy);

    // Decode and verify all absolute coords
    let coords = decode_commands_to_abs(&dest);
    // Exterior: (100,100), (200,100), (200,200), (100,200)
    assert_eq!(coords[0], (100, 100), "ext vertex 0");
    assert_eq!(coords[1], (200, 100), "ext vertex 1");
    assert_eq!(coords[2], (200, 200), "ext vertex 2");
    assert_eq!(coords[3], (100, 200), "ext vertex 3");
    // Hole: (120,120), (180,120), (180,180), (120,180)
    assert_eq!(coords[4], (120, 120), "hole vertex 0");
    assert_eq!(coords[5], (180, 120), "hole vertex 1");
    assert_eq!(coords[6], (180, 180), "hole vertex 2");
    assert_eq!(coords[7], (120, 180), "hole vertex 3");
}

/// Regression test: merge two polygon features that each have multi-ring
/// geometry. Verifies the full merge pipeline produces correct coordinates.
#[test]
fn test_merge_two_multi_ring_polygons_coords_in_bounds() {
    let mut layer = LayerBuilder::new("test");
    let ki = layer.intern_key("kind");
    let vi = layer.intern_value(Value::String("ocean".into()));

    // Feature 1: exterior + hole
    let ext1 = [(0, 0), (1000, 0), (1000, 1000), (0, 1000), (0, 0)];
    let hole1 = [(100, 100), (900, 100), (900, 900), (100, 900), (100, 100)];
    let mut geom1 = Vec::new();
    encode_polygon(&mut geom1, &[&ext1, &hole1]);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::Polygon,
        geometry: geom1,
        tags: vec![(ki, vi)],
    });

    // Feature 2: exterior + hole at different location
    let ext2 = [
        (2000, 2000),
        (3000, 2000),
        (3000, 3000),
        (2000, 3000),
        (2000, 2000),
    ];
    let hole2 = [
        (2100, 2100),
        (2900, 2100),
        (2900, 2900),
        (2100, 2900),
        (2100, 2100),
    ];
    let mut geom2 = Vec::new();
    encode_polygon(&mut geom2, &[&ext2, &hole2]);
    layer.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::Polygon,
        geometry: geom2,
        tags: vec![(ki, vi)],
    });

    let mut scratch = MergeScratch::new();
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

    assert_eq!(layer.test_feature_count(), 1, "should merge into 1 feature");
    let coords = decode_commands_to_abs(&layer.test_feature(0).geometry);

    // All coordinates must be within the expected range [0, 3000]
    for (i, &(x, y)) in coords.iter().enumerate() {
        assert!((0..=3000).contains(&x), "coord {i}: x={x} out of range");
        assert!((0..=3000).contains(&y), "coord {i}: y={y} out of range");
    }

    // Verify specific coordinates from both features
    // Feature 1 exterior
    assert_eq!(coords[0], (0, 0));
    assert_eq!(coords[1], (1000, 0));
    assert_eq!(coords[2], (1000, 1000));
    assert_eq!(coords[3], (0, 1000));
    // Feature 1 hole
    assert_eq!(coords[4], (100, 100));
    assert_eq!(coords[5], (900, 100));
    // Feature 2 exterior
    assert_eq!(coords[8], (2000, 2000));
    assert_eq!(coords[9], (3000, 2000));
    // Feature 2 hole
    assert_eq!(coords[12], (2100, 2100));
    assert_eq!(coords[13], (2900, 2100));
}

// -----------------------------------------------------------------------
// merge_same_attr_geometries tests
// -----------------------------------------------------------------------

#[test]
fn test_merge_no_features() {
    let mut layer = LayerBuilder::new("test");
    let mut scratch = MergeScratch::new();
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);
    assert_eq!(layer.test_feature_count(), 0);
}

#[test]
fn test_merge_single_feature() {
    let mut layer = LayerBuilder::new("test");
    let ki = layer.intern_key("kind");
    let vi = layer.intern_value(Value::String("motorway".into()));
    let mut geom = Vec::new();
    encode_linestring(&mut geom, &[(0, 0), (10, 10)]);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(ki, vi)],
    });
    let mut scratch = MergeScratch::new();
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);
    assert_eq!(layer.test_feature_count(), 1);
}

#[test]
fn test_merge_two_lines_same_attrs() {
    let mut layer = LayerBuilder::new("test");
    let ki = layer.intern_key("kind");
    let vi = layer.intern_value(Value::String("residential".into()));

    let mut geom1 = Vec::new();
    encode_linestring(&mut geom1, &[(0, 0), (10, 10)]);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::LineString,
        geometry: geom1,
        tags: vec![(ki, vi)],
    });

    let mut geom2 = Vec::new();
    encode_linestring(&mut geom2, &[(20, 20), (30, 30)]);
    layer.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::LineString,
        geometry: geom2,
        tags: vec![(ki, vi)],
    });

    let mut scratch = MergeScratch::new();
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

    // Should merge into 1 feature
    assert_eq!(layer.test_feature_count(), 1);
    // Merged feature should have no ID
    assert_eq!(layer.test_feature(0).id, None);
    // Geometry should contain both linestrings
    let coords = decode_commands_to_abs(&layer.test_feature(0).geometry);
    assert_eq!(coords.len(), 4); // 2 points from each linestring
    assert_eq!(coords[0], (0, 0));
    assert_eq!(coords[1], (10, 10));
    assert_eq!(coords[2], (20, 20));
    assert_eq!(coords[3], (30, 30));
}

#[test]
fn test_merge_different_attrs_not_merged() {
    let mut layer = LayerBuilder::new("test");
    let ki = layer.intern_key("kind");
    let v1 = layer.intern_value(Value::String("residential".into()));
    let v2 = layer.intern_value(Value::String("motorway".into()));

    let mut geom1 = Vec::new();
    encode_linestring(&mut geom1, &[(0, 0), (10, 10)]);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::LineString,
        geometry: geom1,
        tags: vec![(ki, v1)],
    });

    let mut geom2 = Vec::new();
    encode_linestring(&mut geom2, &[(20, 20), (30, 30)]);
    layer.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::LineString,
        geometry: geom2,
        tags: vec![(ki, v2)],
    });

    let mut scratch = MergeScratch::new();
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

    // Different attrs → no merge
    assert_eq!(layer.test_feature_count(), 2);
}

#[test]
fn test_merge_points_not_merged() {
    let mut layer = LayerBuilder::new("test");
    let ki = layer.intern_key("kind");
    let vi = layer.intern_value(Value::String("city".into()));

    let mut geom1 = Vec::new();
    encode_point(&mut geom1, 10, 20);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::Point,
        geometry: geom1,
        tags: vec![(ki, vi)],
    });

    let mut geom2 = Vec::new();
    encode_point(&mut geom2, 30, 40);
    layer.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::Point,
        geometry: geom2,
        tags: vec![(ki, vi)],
    });

    let mut scratch = MergeScratch::new();
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

    // Points are explicitly skipped
    assert_eq!(layer.test_feature_count(), 2);
}

#[test]
fn test_merge_mixed_geom_types_separate() {
    let mut layer = LayerBuilder::new("test");
    let ki = layer.intern_key("kind");
    let vi = layer.intern_value(Value::String("residential".into()));

    // A linestring
    let mut geom1 = Vec::new();
    encode_linestring(&mut geom1, &[(0, 0), (10, 10)]);
    layer.add_feature(Feature {
        id: Some(1),
        geom_type: GeomType::LineString,
        geometry: geom1,
        tags: vec![(ki, vi)],
    });

    // A polygon with same attrs
    let ring = [(0, 0), (10, 0), (10, 10), (0, 0)];
    let mut geom2 = Vec::new();
    encode_polygon(&mut geom2, &[&ring]);
    layer.add_feature(Feature {
        id: Some(2),
        geom_type: GeomType::Polygon,
        geometry: geom2,
        tags: vec![(ki, vi)],
    });

    let mut scratch = MergeScratch::new();
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

    // Different geom types → no merge (even with same tags)
    assert_eq!(layer.test_feature_count(), 2);
}

#[test]
fn test_merge_three_lines_same_attrs() {
    let mut layer = LayerBuilder::new("test");
    let ki = layer.intern_key("kind");
    let vi = layer.intern_value(Value::String("path".into()));

    for i in 0..3 {
        let mut geom = Vec::new();
        let base = i * 100;
        encode_linestring(&mut geom, &[(base, base), (base + 10, base + 10)]);
        layer.add_feature(Feature {
            #[allow(clippy::cast_sign_loss)]
            id: Some(i as u64),
            geom_type: GeomType::LineString,
            geometry: geom,
            tags: vec![(ki, vi)],
        });
    }

    let mut scratch = MergeScratch::new();
    let mut gp = Vec::new();
    let mut tp = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

    // 3 features → 1 merged feature
    assert_eq!(layer.test_feature_count(), 1);
    let coords = decode_commands_to_abs(&layer.test_feature(0).geometry);
    assert_eq!(coords.len(), 6); // 2 points × 3 linestrings
}

#[test]
fn test_merge_reclaims_to_pools() {
    let mut layer = LayerBuilder::new("test");
    let ki = layer.intern_key("kind");
    let vi = layer.intern_value(Value::String("residential".into()));

    for i in 0..3 {
        let mut geom = Vec::new();
        encode_linestring(&mut geom, &[(i * 10, 0), (i * 10 + 5, 5)]);
        layer.add_feature(Feature {
            #[allow(clippy::cast_sign_loss)]
            id: Some(i as u64),
            geom_type: GeomType::LineString,
            geometry: geom,
            tags: vec![(ki, vi)],
        });
    }

    let mut scratch = MergeScratch::new();
    let mut gp: Vec<Vec<u32>> = Vec::new();
    let mut tp: Vec<Vec<(u16, u16)>> = Vec::new();
    layer.merge_same_attr_geometries(&mut scratch, &mut gp, &mut tp);

    // Secondary features' vecs are reclaimed into the pools, and the merged
    // primary's ORIGINAL geometry Vec joins the geom pool too (the merged
    // stream is copied into a pooled Vec rather than swapped out of scratch,
    // so the primary's old buffer is freed up for reuse).
    assert_eq!(gp.len(), 3); // 2 secondaries + the primary's old geometry
    assert_eq!(tp.len(), 2);
}

#[test]
fn encode_sorts_keys_alphabetically() {
    let mut layer = LayerBuilder::new("test");
    // Insert keys in reverse alphabetical order.
    let k_z = layer.intern_key("zoo");
    let k_a = layer.intern_key("alpha");
    let k_m = layer.intern_key("mid");
    let v = layer.intern_value(Value::String("x".to_string()));
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::Point,
        geometry: vec![command(1, 1), zigzag(10), zigzag(20)],
        tags: vec![(k_z, v), (k_a, v), (k_m, v)],
    });
    let tile = encode_tile(&[&layer]);
    let parsed = parse_fixture_tile(&tile).unwrap();
    assert_eq!(parsed.layers[0].keys, vec!["alpha", "mid", "zoo"]);
}

#[test]
fn encode_sorts_values_by_type_then_content() {
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    // Insert values in mixed order: int, string, string, int.
    let v_i42 = layer.intern_value(Value::Int(42));
    let v_sb = layer.intern_value(Value::String("banana".to_string()));
    let v_sa = layer.intern_value(Value::String("apple".to_string()));
    let v_i1 = layer.intern_value(Value::Int(1));

    for v in [v_i42, v_sb, v_sa, v_i1] {
        layer.add_feature(Feature {
            id: None,
            geom_type: GeomType::Point,
            geometry: vec![command(1, 1), zigzag(10), zigzag(20)],
            tags: vec![(k, v)],
        });
    }
    let tile = encode_tile(&[&layer]);
    let parsed = parse_fixture_tile(&tile).unwrap();
    // Strings should come first (type 0), sorted alphabetically.
    // The parser puts all values into string_values (non-strings as "").
    assert_eq!(parsed.layers[0].string_values[0], "apple");
    assert_eq!(parsed.layers[0].string_values[1], "banana");
}

#[test]
fn encode_remaps_tag_indices_after_sort() {
    let mut layer = LayerBuilder::new("test");
    // Keys inserted as "z", "a". After sort: "a"=0, "z"=1.
    let k_z = layer.intern_key("z");
    let k_a = layer.intern_key("a");
    let v_x = layer.intern_value(Value::String("x".to_string()));
    let v_y = layer.intern_value(Value::String("y".to_string()));
    // Feature tags: z=y, a=x (using original indices).
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::Point,
        geometry: vec![command(1, 1), zigzag(10), zigzag(20)],
        tags: vec![(k_z, v_y), (k_a, v_x)],
    });
    let tile = encode_tile(&[&layer]);
    let parsed = parse_fixture_tile(&tile).unwrap();
    assert_eq!(parsed.layers[0].keys, vec!["a", "z"]);
    assert_eq!(parsed.layers[0].string_values, vec!["x", "y"]);
    // Tags should be remapped: a(0)=x(0), z(1)=y(1).
    assert_eq!(parsed.layers[0].features[0].feature_tags, vec![1, 1, 0, 0]);
}

// -----------------------------------------------------------------------
// Line merging tests
// -----------------------------------------------------------------------

/// Helper: decode a merged feature's geometry back to segments.
fn decode_segments(geom: &[u32]) -> Vec<Vec<(i32, i32)>> {
    let mut segs = Vec::new();
    test_decode_line_segments(geom, &mut segs);
    segs
}

/// Helper: build a multi-linestring geometry from segments.
fn build_multi_line(segments: &[&[(i32, i32)]]) -> Vec<u32> {
    let mut buf = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    for seg in segments {
        if seg.len() < 2 {
            continue;
        }
        buf.push(command(1, 1));
        buf.push(zigzag(seg[0].0 - cx));
        buf.push(zigzag(seg[0].1 - cy));
        cx = seg[0].0;
        cy = seg[0].1;
        let lineto_pos = buf.len();
        buf.push(0);
        let mut count = 0u32;
        for &(x, y) in &seg[1..] {
            buf.push(zigzag(x - cx));
            buf.push(zigzag(y - cy));
            cx = x;
            cy = y;
            count += 1;
        }
        #[allow(clippy::cast_possible_truncation)]
        {
            buf[lineto_pos] = command(2, count);
        }
    }
    buf
}

#[test]
fn line_merge_two_segments_degree2() {
    // A(0,0)→B(10,10) + B(10,10)→C(20,0): B is degree-2, should merge.
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    let v = layer.intern_value(Value::String("v".into()));
    let geom = build_multi_line(&[&[(0, 0), (10, 10)], &[(10, 10), (20, 0)]]);
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(k, v)],
    });
    let mut scratch = LineMergeScratch::new();
    layer.merge_connected_lines(&mut scratch);
    let segs = decode_segments(&layer.features[0].geometry);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0], vec![(0, 0), (10, 10), (20, 0)]);
}

#[test]
fn line_merge_reverse_direction() {
    // A→B + C→B: second segment needs reversal to connect.
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    let v = layer.intern_value(Value::String("v".into()));
    let geom = build_multi_line(&[&[(0, 0), (10, 10)], &[(20, 0), (10, 10)]]);
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(k, v)],
    });
    let mut scratch = LineMergeScratch::new();
    layer.merge_connected_lines(&mut scratch);
    let segs = decode_segments(&layer.features[0].geometry);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0], vec![(0, 0), (10, 10), (20, 0)]);
}

#[test]
fn line_merge_junction_blocks() {
    // A→B, B→C, B→D: B is degree-3 (junction), should NOT merge through B.
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    let v = layer.intern_value(Value::String("v".into()));
    let geom = build_multi_line(&[
        &[(0, 0), (10, 10)],
        &[(10, 10), (20, 0)],
        &[(10, 10), (20, 20)],
    ]);
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(k, v)],
    });
    let mut scratch = LineMergeScratch::new();
    layer.merge_connected_lines(&mut scratch);
    let segs = decode_segments(&layer.features[0].geometry);
    assert_eq!(segs.len(), 3, "junction should prevent any merging");
}

#[test]
fn line_merge_single_segment_noop() {
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    let v = layer.intern_value(Value::String("v".into()));
    let mut geom = Vec::new();
    encode_linestring(&mut geom, &[(0, 0), (10, 10), (20, 0)]);
    let original = geom.clone();
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(k, v)],
    });
    let mut scratch = LineMergeScratch::new();
    layer.merge_connected_lines(&mut scratch);
    assert_eq!(layer.features[0].geometry, original);
}

#[test]
fn line_merge_closed_ring() {
    // A→B→C→A forms a closed loop (pure cycle).
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    let v = layer.intern_value(Value::String("v".into()));
    let geom = build_multi_line(&[
        &[(0, 0), (10, 0)],
        &[(10, 0), (10, 10)],
        &[(10, 10), (0, 0)],
    ]);
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(k, v)],
    });
    let mut scratch = LineMergeScratch::new();
    layer.merge_connected_lines(&mut scratch);
    let segs = decode_segments(&layer.features[0].geometry);
    assert_eq!(segs.len(), 1, "cycle should merge into one linestring");
    // Closed: first == last
    assert_eq!(segs[0].first(), segs[0].last());
    assert_eq!(segs[0].len(), 4);
}

#[test]
fn line_merge_skips_polygon_features() {
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    let v = layer.intern_value(Value::String("v".into()));
    let mut geom = Vec::new();
    encode_polygon(&mut geom, &[&[(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]]);
    let original = geom.clone();
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::Polygon,
        geometry: geom,
        tags: vec![(k, v)],
    });
    let mut scratch = LineMergeScratch::new();
    layer.merge_connected_lines(&mut scratch);
    assert_eq!(
        layer.features[0].geometry, original,
        "polygon should be untouched"
    );
}

#[test]
fn line_merge_chain_of_three() {
    // A→B→C→D: all interior nodes degree-2, should merge into one.
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    let v = layer.intern_value(Value::String("v".into()));
    let geom = build_multi_line(&[
        &[(0, 0), (10, 0)],
        &[(10, 0), (20, 10)],
        &[(20, 10), (30, 0)],
    ]);
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(k, v)],
    });
    let mut scratch = LineMergeScratch::new();
    layer.merge_connected_lines(&mut scratch);
    let segs = decode_segments(&layer.features[0].geometry);
    assert_eq!(segs.len(), 1);
    assert_eq!(segs[0], vec![(0, 0), (10, 0), (20, 10), (30, 0)]);
}

#[test]
fn line_merge_deterministic_output() {
    // Same logical segments in different input orders should produce
    // identical encoded geometry.
    let order_a = build_multi_line(&[&[(0, 0), (10, 0)], &[(10, 0), (20, 0)], &[(20, 0), (30, 0)]]);
    let order_b = build_multi_line(&[&[(20, 0), (30, 0)], &[(0, 0), (10, 0)], &[(10, 0), (20, 0)]]);
    let order_c = build_multi_line(&[&[(10, 0), (20, 0)], &[(20, 0), (30, 0)], &[(0, 0), (10, 0)]]);

    let mut results = Vec::new();
    for geom in [order_a, order_b, order_c] {
        let mut layer = LayerBuilder::new("test");
        let k = layer.intern_key("k");
        let v = layer.intern_value(Value::String("v".into()));
        layer.add_feature(Feature {
            id: None,
            geom_type: GeomType::LineString,
            geometry: geom,
            tags: vec![(k, v)],
        });
        let mut scratch = LineMergeScratch::new();
        layer.merge_connected_lines(&mut scratch);
        results.push(layer.features[0].geometry.clone());
    }
    assert_eq!(results[0], results[1], "order A vs B should match");
    assert_eq!(results[1], results[2], "order B vs C should match");
}

#[test]
fn line_merge_self_loop_not_merged_through() {
    // Segment A→A (self-loop) at point (10,10), plus B→(10,10):
    // The self-loop contributes degree 2 at (10,10) but both ends are
    // the same segment - should not merge B through it.
    let geom = build_multi_line(&[
        &[(10, 10), (20, 20), (10, 10)], // self-loop
        &[(0, 0), (10, 10)],
    ]);
    let mut layer = LayerBuilder::new("test");
    let k = layer.intern_key("k");
    let v = layer.intern_value(Value::String("v".into()));
    layer.add_feature(Feature {
        id: None,
        geom_type: GeomType::LineString,
        geometry: geom,
        tags: vec![(k, v)],
    });
    let mut scratch = LineMergeScratch::new();
    layer.merge_connected_lines(&mut scratch);
    let segs = decode_segments(&layer.features[0].geometry);
    // Self-loop and the other segment should remain separate.
    assert_eq!(segs.len(), 2, "self-loop should prevent merging");
}
