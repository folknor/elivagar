use crate::mvt::{Feature, GeomType, LayerBuilder, Value};

use geo_types::{
    Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
use mlt_core::encoder::EncoderConfig;
use mlt_core::{PropKind, PropValue as MltPropValue, TileLayer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MltColumnType {
    String,
    Float,
    Double,
    Int,
    UInt,
    SInt,
    Bool,
    Mixed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MltColumnModel {
    pub key: String,
    pub column_type: MltColumnType,
    pub value_count: usize,
    /// Number of distinct value types observed for this key across layer features.
    pub observed_type_count: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MltGeometryMix {
    pub points: usize,
    pub lines: usize,
    pub polygons: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MltLayerModel {
    pub name: String,
    pub feature_count: usize,
    pub geometry_mix: MltGeometryMix,
    pub columns: Vec<MltColumnModel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MltTileModel {
    pub layer_count: usize,
    pub feature_count: usize,
    pub layers: Vec<MltLayerModel>,
}

#[derive(Debug)]
pub(crate) enum MltEncodeError {
    GeometryDecode {
        layer: String,
        feature_index: usize,
        message: String,
    },
    Encode(String),
}

impl std::fmt::Display for MltEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GeometryDecode {
                layer,
                feature_index,
                message,
            } => write!(
                f,
                "geometry decode failed for layer '{layer}' feature#{feature_index}: {message}"
            ),
            Self::Encode(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for MltEncodeError {}

pub(crate) fn encode_tile(layers: &[&LayerBuilder]) -> Result<Vec<u8>, MltEncodeError> {
    let mut out = Vec::new();
    for layer in layers {
        if layer.is_empty() {
            continue;
        }
        let encoded = encode_layer(layer)?;
        out.extend_from_slice(&encoded);
    }
    Ok(out)
}

fn encode_layer(layer: &LayerBuilder) -> Result<Vec<u8>, MltEncodeError> {
    build_mlt_tile_layer(layer)?
        .encode(EncoderConfig::default())
        .map_err(|e| MltEncodeError::Encode(e.to_string()))
}

fn build_mlt_tile_layer(layer: &LayerBuilder) -> Result<TileLayer, MltEncodeError> {
    let features = layer.features();
    let tile_model = build_tile_model(&[layer]);
    let columns = tile_model
        .layers
        .first()
        .map(|lm| lm.columns.as_slice())
        .unwrap_or(&[]);
    let mut builder = mlt_core::TileLayer::builder(layer.name(), 4096)
        .map_err(|e| MltEncodeError::Encode(e.to_string()))?;
    let mut property_keys = Vec::with_capacity(columns.len());

    for col in columns {
        let key = builder
            .add_property(col.key.clone(), mlt_prop_kind(col.column_type))
            .map_err(|e| MltEncodeError::Encode(e.to_string()))?;
        property_keys.push(key);
    }

    for (idx, feature) in features.iter().enumerate() {
        let geom =
            decode_feature_geometry(feature).map_err(|msg| MltEncodeError::GeometryDecode {
                layer: layer.name().to_string(),
                feature_index: idx,
                message: msg,
            })?;
        let mut feature_builder = builder.feature(geom);
        feature_builder.id(feature.id);
        for (col, key) in columns.iter().zip(&property_keys) {
            if let Some(value) = feature_value_for_key(layer, feature, &col.key) {
                feature_builder
                    .property(*key, value_to_mlt_prop(value, col.column_type))
                    .map_err(|e| MltEncodeError::Encode(e.to_string()))?;
            }
        }
        feature_builder
            .finish()
            .map_err(|e| MltEncodeError::Encode(e.to_string()))?;
    }

    Ok(builder.finish())
}

fn mlt_prop_kind(column_type: MltColumnType) -> PropKind {
    match column_type {
        MltColumnType::String | MltColumnType::Mixed => PropKind::Str,
        MltColumnType::Float => PropKind::F32,
        MltColumnType::Double => PropKind::F64,
        MltColumnType::Int | MltColumnType::SInt => PropKind::I64,
        MltColumnType::UInt => PropKind::U64,
        MltColumnType::Bool => PropKind::Bool,
    }
}

fn value_to_mlt_prop(value: &Value, column_type: MltColumnType) -> MltPropValue {
    match column_type {
        MltColumnType::String => MltPropValue::Str(match value {
            Value::String(x) => Some(x.clone()),
            _ => None,
        }),
        MltColumnType::Float => MltPropValue::F32(match value {
            Value::Float(x) => Some(*x),
            _ => None,
        }),
        MltColumnType::Double => MltPropValue::F64(match value {
            Value::Double(x) => Some(*x),
            _ => None,
        }),
        MltColumnType::Int => MltPropValue::I64(match value {
            Value::Int(x) => Some(*x),
            _ => None,
        }),
        MltColumnType::UInt => MltPropValue::U64(match value {
            Value::UInt(x) => Some(*x),
            _ => None,
        }),
        MltColumnType::SInt => MltPropValue::I64(match value {
            Value::SInt(x) => Some(*x),
            _ => None,
        }),
        MltColumnType::Bool => MltPropValue::Bool(match value {
            Value::Bool(x) => Some(*x),
            _ => None,
        }),
        MltColumnType::Mixed => MltPropValue::Str(Some(value_to_string(value))),
    }
}

fn feature_value_for_key<'a>(
    layer: &'a LayerBuilder,
    feature: &Feature,
    key: &str,
) -> Option<&'a Value> {
    for &(k_idx, v_idx) in &feature.tags {
        if layer.key(k_idx) == Some(key) {
            return layer.value(v_idx);
        }
    }
    None
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Float(v) => v.to_string(),
        Value::Double(v) => v.to_string(),
        Value::Int(v) => v.to_string(),
        Value::UInt(v) => v.to_string(),
        Value::SInt(v) => v.to_string(),
        Value::Bool(v) => v.to_string(),
    }
}

fn decode_feature_geometry(feature: &Feature) -> Result<Geometry<i32>, String> {
    match feature.geom_type {
        GeomType::Point => decode_point_geometry(&feature.geometry),
        GeomType::LineString => decode_line_geometry(&feature.geometry),
        GeomType::Polygon => decode_polygon_geometry(&feature.geometry),
    }
}

fn decode_point_geometry(commands: &[u32]) -> Result<Geometry<i32>, String> {
    let points = parse_points(commands)?;
    if points.is_empty() {
        return Err("point geometry is empty".to_string());
    }
    if points.len() == 1 {
        return Ok(Geometry::Point(Point(points[0])));
    }
    let multi = MultiPoint(points.into_iter().map(Point).collect());
    Ok(Geometry::MultiPoint(multi))
}

fn decode_line_geometry(commands: &[u32]) -> Result<Geometry<i32>, String> {
    let paths = parse_paths(commands)?;
    let mut lines = Vec::new();
    for path in paths {
        if path.len() < 2 {
            continue;
        }
        lines.push(LineString(path));
    }

    if lines.is_empty() {
        return Err("linestring geometry has no valid paths".to_string());
    }
    if lines.len() == 1 {
        return Ok(Geometry::LineString(lines.remove(0)));
    }
    Ok(Geometry::MultiLineString(MultiLineString(lines)))
}

fn decode_polygon_geometry(commands: &[u32]) -> Result<Geometry<i32>, String> {
    let ring_points = parse_paths(commands)?;
    let mut rings = Vec::new();
    for points in ring_points {
        rings.push(close_ring(points)?);
    }
    if rings.is_empty() {
        return Err("polygon geometry has no rings".to_string());
    }

    let mut polygons = group_rings_to_polygons(rings);
    if polygons.len() == 1
        && let Some(poly) = polygons.pop()
    {
        return Ok(Geometry::Polygon(poly));
    }
    Ok(Geometry::MultiPolygon(MultiPolygon(polygons)))
}

fn close_ring(mut points: Vec<Coord<i32>>) -> Result<LineString<i32>, String> {
    if points.len() < 3 {
        return Err("polygon ring has fewer than 3 vertices".to_string());
    }
    if let (Some(first), Some(last)) = (points.first().copied(), points.last().copied())
        && first != last
    {
        points.push(first);
    }
    Ok(LineString(points))
}

fn group_rings_to_polygons(rings: Vec<LineString<i32>>) -> Vec<Polygon<i32>> {
    let mut polygons = Vec::new();
    let mut exterior: Option<LineString<i32>> = None;
    let mut holes: Vec<LineString<i32>> = Vec::new();
    let mut exterior_sign: Option<i64> = None;

    for ring in rings {
        let sign = ring_signed_area(&ring);
        if exterior.is_none() {
            exterior_sign = Some(sign);
            exterior = Some(ring);
            continue;
        }

        let same_as_exterior = exterior_sign
            .map(|s| (sign >= 0) == (s >= 0))
            .unwrap_or(true);
        if same_as_exterior {
            if let Some(ext) = exterior.take() {
                polygons.push(Polygon::new(ext, holes));
                holes = Vec::new();
            }
            exterior_sign = Some(sign);
            exterior = Some(ring);
        } else {
            holes.push(ring);
        }
    }

    if let Some(ext) = exterior {
        polygons.push(Polygon::new(ext, holes));
    }

    polygons
}

fn ring_signed_area(ring: &LineString<i32>) -> i64 {
    let mut sum = 0_i64;
    for segment in ring.0.windows(2) {
        if let [a, b] = segment {
            sum += i64::from(a.x) * i64::from(b.y) - i64::from(b.x) * i64::from(a.y);
        }
    }
    sum
}

fn parse_points(commands: &[u32]) -> Result<Vec<Coord<i32>>, String> {
    let mut out = Vec::new();
    let mut cursor_x = 0_i32;
    let mut cursor_y = 0_i32;
    let mut i = 0_usize;

    while i < commands.len() {
        let cmd = commands[i];
        i += 1;
        let cmd_id = cmd & 0x7;
        let count = usize::try_from(cmd >> 3).map_err(|_| "command count overflow".to_string())?;
        match cmd_id {
            1 => {
                for _ in 0..count {
                    let (dx, dy, next_i) = read_delta_pair(commands, i)?;
                    i = next_i;
                    cursor_x += dx;
                    cursor_y += dy;
                    out.push(Coord {
                        x: cursor_x,
                        y: cursor_y,
                    });
                }
            }
            7 => {}
            2 => return Err("point geometry contains LineTo command".to_string()),
            _ => return Err(format!("unsupported command id {cmd_id} in point geometry")),
        }
    }

    Ok(out)
}

fn parse_paths(commands: &[u32]) -> Result<Vec<Vec<Coord<i32>>>, String> {
    let mut paths = Vec::new();
    let mut current = Vec::new();
    let mut cursor_x = 0_i32;
    let mut cursor_y = 0_i32;
    let mut i = 0_usize;

    while i < commands.len() {
        let cmd = commands[i];
        i += 1;
        let cmd_id = cmd & 0x7;
        let count = usize::try_from(cmd >> 3).map_err(|_| "command count overflow".to_string())?;

        match cmd_id {
            1 => {
                for _ in 0..count {
                    let (dx, dy, next_i) = read_delta_pair(commands, i)?;
                    i = next_i;
                    cursor_x += dx;
                    cursor_y += dy;
                    if !current.is_empty() {
                        paths.push(std::mem::take(&mut current));
                    }
                    current.push(Coord {
                        x: cursor_x,
                        y: cursor_y,
                    });
                }
            }
            2 => {
                if current.is_empty() {
                    return Err("LineTo encountered before MoveTo".to_string());
                }
                for _ in 0..count {
                    let (dx, dy, next_i) = read_delta_pair(commands, i)?;
                    i = next_i;
                    cursor_x += dx;
                    cursor_y += dy;
                    current.push(Coord {
                        x: cursor_x,
                        y: cursor_y,
                    });
                }
            }
            7 => {}
            _ => return Err(format!("unsupported command id {cmd_id}")),
        }
    }

    if !current.is_empty() {
        paths.push(current);
    }

    Ok(paths)
}

fn read_delta_pair(commands: &[u32], index: usize) -> Result<(i32, i32, usize), String> {
    if index + 1 >= commands.len() {
        return Err("truncated geometry command stream".to_string());
    }
    let dx = decode_zigzag(commands[index])?;
    let dy = decode_zigzag(commands[index + 1])?;
    Ok((dx, dy, index + 2))
}

fn decode_zigzag(value: u32) -> Result<i32, String> {
    let half = i32::try_from(value >> 1).map_err(|_| "zigzag value out of range".to_string())?;
    let sign = i32::try_from(value & 1).map_err(|_| "zigzag sign out of range".to_string())?;
    Ok(half ^ -sign)
}

pub(crate) fn build_tile_model(layers: &[&LayerBuilder]) -> MltTileModel {
    let mut out_layers = Vec::with_capacity(layers.len());
    let mut total_features = 0usize;

    for layer in layers {
        let feature_count = layer.features().len();
        total_features += feature_count;

        let mut columns: Vec<MltColumnModel> = Vec::new();
        let mut type_masks: Vec<u8> = Vec::new();
        let mut geometry_mix = MltGeometryMix::default();
        for feature in layer.features() {
            match feature.geom_type {
                GeomType::Point => geometry_mix.points += 1,
                GeomType::LineString => geometry_mix.lines += 1,
                GeomType::Polygon => geometry_mix.polygons += 1,
            }
            for &(k_idx, v_idx) in &feature.tags {
                let Some(key) = layer.key(k_idx) else {
                    continue;
                };
                let Some(value) = layer.value(v_idx) else {
                    continue;
                };
                let value_ty = value_type(value);
                let value_ty_bit = value_type_bit(value_ty);
                if let Some(idx) = columns.iter().position(|c| c.key == key) {
                    let existing = &mut columns[idx];
                    existing.value_count += 1;
                    type_masks[idx] |= value_ty_bit;
                    existing.observed_type_count = u8::try_from(type_masks[idx].count_ones())
                        .expect("type cardinality should fit in u8");
                    if existing.observed_type_count > 1 {
                        existing.column_type = MltColumnType::Mixed;
                    }
                } else {
                    columns.push(MltColumnModel {
                        key: key.to_string(),
                        column_type: value_ty,
                        value_count: 1,
                        observed_type_count: 1,
                    });
                    type_masks.push(value_ty_bit);
                }
            }
        }

        columns.sort_by(|a, b| a.key.cmp(&b.key));
        out_layers.push(MltLayerModel {
            name: layer.name().to_string(),
            feature_count,
            geometry_mix,
            columns,
        });
    }

    MltTileModel {
        layer_count: out_layers.len(),
        feature_count: total_features,
        layers: out_layers,
    }
}

fn value_type(value: &Value) -> MltColumnType {
    match value {
        Value::String(_) => MltColumnType::String,
        Value::Float(_) => MltColumnType::Float,
        Value::Double(_) => MltColumnType::Double,
        Value::Int(_) => MltColumnType::Int,
        Value::UInt(_) => MltColumnType::UInt,
        Value::SInt(_) => MltColumnType::SInt,
        Value::Bool(_) => MltColumnType::Bool,
    }
}

fn value_type_bit(value_type: MltColumnType) -> u8 {
    match value_type {
        MltColumnType::String => 1 << 0,
        MltColumnType::Float => 1 << 1,
        MltColumnType::Double => 1 << 2,
        MltColumnType::Int => 1 << 3,
        MltColumnType::UInt => 1 << 4,
        MltColumnType::SInt => 1 << 5,
        MltColumnType::Bool => 1 << 6,
        MltColumnType::Mixed => 0,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::mvt::{Feature, GeomType, LayerBuilder};
    use geo_types::Geometry;
    use mlt_core::LendingIterator as _;
    use serde::Deserialize;
    use serde_json::{Number, Value as JsonValue};
    use std::collections::BTreeMap;

    fn parse_decoded_layers<'a>(
        encoded: &'a [u8],
    ) -> mlt_core::MltResult<Vec<mlt_core::ParsedLayer<'a>>> {
        let mut parser = mlt_core::Parser::default();
        let layers = parser.parse_layers(encoded)?;
        let mut decoder = mlt_core::Decoder::default();
        decoder.decode_all(layers)
    }

    fn point_geom() -> Vec<u32> {
        vec![9, 50, 50]
    }

    #[test]
    fn build_tile_model_detects_mixed_property_types() {
        let mut layer = LayerBuilder::new("places");
        let kind_key = layer.intern_key("kind");
        let pop_key = layer.intern_key("population");
        let city_val = layer.intern_value(Value::String("city".to_string()));
        let town_val = layer.intern_value(Value::String("town".to_string()));
        let pop_int = layer.intern_value(Value::Int(1200));
        let pop_float = layer.intern_value(Value::Float(1200.5));

        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(kind_key, city_val), (pop_key, pop_int)],
        });
        layer.add_feature(Feature {
            id: Some(2),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(kind_key, town_val), (pop_key, pop_float)],
        });

        let model = build_tile_model(&[&layer]);
        assert_eq!(model.layer_count, 1);
        assert_eq!(model.feature_count, 2);
        assert_eq!(model.layers[0].name, "places");
        assert_eq!(model.layers[0].feature_count, 2);
        assert_eq!(model.layers[0].geometry_mix.points, 2);
        assert_eq!(model.layers[0].geometry_mix.lines, 0);
        assert_eq!(model.layers[0].geometry_mix.polygons, 0);
        assert_eq!(model.layers[0].columns.len(), 2);
        assert_eq!(model.layers[0].columns[0].key, "kind");
        assert_eq!(
            model.layers[0].columns[0].column_type,
            MltColumnType::String
        );
        assert_eq!(model.layers[0].columns[0].value_count, 2);
        assert_eq!(model.layers[0].columns[0].observed_type_count, 1);
        assert_eq!(model.layers[0].columns[1].key, "population");
        assert_eq!(model.layers[0].columns[1].column_type, MltColumnType::Mixed);
        assert_eq!(model.layers[0].columns[1].value_count, 2);
        assert_eq!(model.layers[0].columns[1].observed_type_count, 2);
    }

    #[test]
    fn encode_tile_uses_upstream_mlt_core() {
        let mut layer = LayerBuilder::new("test");
        let key = layer.intern_key("kind");
        let val = layer.intern_value(Value::String("poi".to_string()));
        layer.add_feature(Feature {
            id: Some(7),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(key, val)],
        });

        let encoded = encode_tile(&[&layer]).expect("mlt encode should succeed");
        assert!(!encoded.is_empty());

        let parsed = parse_decoded_layers(&encoded).expect("encoded mlt should parse");
        assert_eq!(parsed.len(), 1);
        let mlt_core::Layer::Tag01(l01) = &parsed[0] else {
            panic!("expected tag01 layer");
        };
        assert_eq!(l01.name(), "test");
        assert_eq!(l01.extent().get(), 4096);
    }

    #[test]
    fn build_tile_model_tracks_geometry_mix_and_sparse_columns() {
        let mut layer = LayerBuilder::new("mixed");
        let key_name = layer.intern_key("name");
        let key_level = layer.intern_key("level");
        let v_name = layer.intern_value(Value::String("main".to_string()));
        let v_level = layer.intern_value(Value::UInt(5));

        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(key_name, v_name)],
        });
        layer.add_feature(Feature {
            id: Some(2),
            geom_type: GeomType::LineString,
            geometry: vec![9, 0, 0, 10, 2, 0],
            tags: vec![(key_level, v_level)],
        });
        layer.add_feature(Feature {
            id: Some(3),
            geom_type: GeomType::Polygon,
            geometry: vec![9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15],
            tags: vec![],
        });

        let model = build_tile_model(&[&layer]);
        let lm = &model.layers[0];
        assert_eq!(lm.geometry_mix.points, 1);
        assert_eq!(lm.geometry_mix.lines, 1);
        assert_eq!(lm.geometry_mix.polygons, 1);
        assert_eq!(lm.columns[0].key, "level");
        assert_eq!(lm.columns[1].key, "name");
        assert_eq!(lm.columns[0].value_count, 1);
        assert_eq!(lm.columns[1].value_count, 1);
    }

    #[test]
    fn build_tile_model_maps_all_value_types() {
        let mut layer = LayerBuilder::new("types");
        let ks = layer.intern_key("s");
        let kf = layer.intern_key("f");
        let kd = layer.intern_key("d");
        let ki = layer.intern_key("i");
        let ku = layer.intern_key("u");
        let ksi = layer.intern_key("si");
        let kb = layer.intern_key("b");

        let vs = layer.intern_value(Value::String("a".to_string()));
        let vf = layer.intern_value(Value::Float(1.5));
        let vd = layer.intern_value(Value::Double(2.5));
        let vi = layer.intern_value(Value::Int(-3));
        let vu = layer.intern_value(Value::UInt(4));
        let vsi = layer.intern_value(Value::SInt(-5));
        let vb = layer.intern_value(Value::Bool(true));

        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![
                (ks, vs),
                (kf, vf),
                (kd, vd),
                (ki, vi),
                (ku, vu),
                (ksi, vsi),
                (kb, vb),
            ],
        });

        let model = build_tile_model(&[&layer]);
        let cols = &model.layers[0].columns;
        assert_eq!(
            cols.iter().find(|c| c.key == "s").map(|c| c.column_type),
            Some(MltColumnType::String)
        );
        assert_eq!(
            cols.iter().find(|c| c.key == "f").map(|c| c.column_type),
            Some(MltColumnType::Float)
        );
        assert_eq!(
            cols.iter().find(|c| c.key == "d").map(|c| c.column_type),
            Some(MltColumnType::Double)
        );
        assert_eq!(
            cols.iter().find(|c| c.key == "i").map(|c| c.column_type),
            Some(MltColumnType::Int)
        );
        assert_eq!(
            cols.iter().find(|c| c.key == "u").map(|c| c.column_type),
            Some(MltColumnType::UInt)
        );
        assert_eq!(
            cols.iter().find(|c| c.key == "si").map(|c| c.column_type),
            Some(MltColumnType::SInt)
        );
        assert_eq!(
            cols.iter().find(|c| c.key == "b").map(|c| c.column_type),
            Some(MltColumnType::Bool)
        );
    }

    #[test]
    fn build_tile_model_tracks_true_mixed_type_cardinality() {
        let mut layer = LayerBuilder::new("cardinality");
        let key = layer.intern_key("mixed_key");
        let v_str = layer.intern_value(Value::String("a".to_string()));
        let v_float = layer.intern_value(Value::Float(1.5));
        let v_bool = layer.intern_value(Value::Bool(true));
        let v_int = layer.intern_value(Value::Int(7));

        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(key, v_str)],
        });
        layer.add_feature(Feature {
            id: Some(2),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(key, v_float)],
        });
        layer.add_feature(Feature {
            id: Some(3),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(key, v_bool)],
        });
        layer.add_feature(Feature {
            id: Some(4),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(key, v_int)],
        });

        let model = build_tile_model(&[&layer]);
        let col = model.layers[0]
            .columns
            .iter()
            .find(|c| c.key == "mixed_key")
            .expect("mixed_key column should exist");
        assert_eq!(col.column_type, MltColumnType::Mixed);
        assert_eq!(col.observed_type_count, 4);
    }

    #[derive(Debug, Deserialize)]
    struct GeometryFixture {
        id: String,
        description: String,
        geom_type: String,
        geometry: Vec<u32>,
        expected_geojson_type: String,
    }

    #[derive(Debug, Deserialize)]
    struct PropertyFixtureCase {
        id: String,
        features: Vec<PropertyFeatureFixture>,
        expected: Vec<PropertyExpectation>,
    }

    #[derive(Debug, Deserialize)]
    struct PropertyFeatureFixture {
        id: u64,
        geom_type: String,
        geometry: Vec<u32>,
        tags: Vec<PropertyTagFixture>,
    }

    #[derive(Debug, Deserialize)]
    struct PropertyTagFixture {
        key: String,
        #[serde(rename = "type")]
        value_type: String,
        value: String,
    }

    #[derive(Debug, Deserialize)]
    struct PropertyExpectation {
        key: String,
        kind: String,
        non_null: usize,
    }

    fn fixture_geom_type(raw: &str) -> GeomType {
        match raw {
            "point" => GeomType::Point,
            "line" => GeomType::LineString,
            "polygon" => GeomType::Polygon,
            _ => panic!("unknown fixture geom_type"),
        }
    }

    fn geometry_name(geom: &Geometry<i32>) -> &'static str {
        match geom {
            Geometry::Point(_) => "Point",
            Geometry::LineString(_) => "LineString",
            Geometry::Polygon(_) => "Polygon",
            Geometry::MultiPoint(_) => "MultiPoint",
            Geometry::MultiLineString(_) => "MultiLineString",
            Geometry::MultiPolygon(_) => "MultiPolygon",
            Geometry::Line(_) => "Line",
            Geometry::Rect(_) => "Rect",
            Geometry::Triangle(_) => "Triangle",
            Geometry::GeometryCollection(_) => "GeometryCollection",
        }
    }

    fn mvt_value_to_json(value: &Value) -> JsonValue {
        match value {
            Value::String(v) => JsonValue::String(v.clone()),
            Value::Float(v) => {
                Number::from_f64(f64::from(*v)).map_or(JsonValue::Null, JsonValue::Number)
            }
            Value::Double(v) => Number::from_f64(*v).map_or(JsonValue::Null, JsonValue::Number),
            Value::Int(v) => JsonValue::Number((*v).into()),
            Value::UInt(v) => JsonValue::Number((*v).into()),
            Value::SInt(v) => JsonValue::Number((*v).into()),
            Value::Bool(v) => JsonValue::Bool(*v),
        }
    }

    fn expected_feature_properties(
        layer: &LayerBuilder,
        feature: &Feature,
    ) -> BTreeMap<String, JsonValue> {
        let mut props = BTreeMap::new();
        for &(k_idx, v_idx) in &feature.tags {
            let key = layer.key(k_idx).expect("key index should resolve");
            let value = layer.value(v_idx).expect("value index should resolve");
            props.insert(key.to_string(), mvt_value_to_json(value));
        }
        props.insert(
            "_layer".to_string(),
            JsonValue::String(layer.name().to_string()),
        );
        props.insert("_extent".to_string(), JsonValue::Number(4096.into()));
        props
    }

    #[test]
    fn mlt_semantic_roundtrip_preserves_geometry_and_properties() {
        let mut layer = LayerBuilder::new("semantic");
        let k_kind = layer.intern_key("kind");
        let k_name = layer.intern_key("name");
        let k_pop = layer.intern_key("population");
        let k_ratio = layer.intern_key("ratio");
        let k_rank = layer.intern_key("rank");
        let k_visible = layer.intern_key("visible");

        let v_kind_city = layer.intern_value(Value::String("city".to_string()));
        let v_kind_road = layer.intern_value(Value::String("road".to_string()));
        let v_kind_land = layer.intern_value(Value::String("landuse".to_string()));
        let v_name_oslo = layer.intern_value(Value::String("Oslo".to_string()));
        let v_pop = layer.intern_value(Value::UInt(700_000));
        let v_ratio = layer.intern_value(Value::Double(1.25));
        let v_rank = layer.intern_value(Value::SInt(-2));
        let v_visible = layer.intern_value(Value::Bool(true));

        layer.add_feature(Feature {
            id: Some(101),
            geom_type: GeomType::Point,
            geometry: vec![9, 50, 34],
            tags: vec![(k_kind, v_kind_city), (k_name, v_name_oslo), (k_pop, v_pop)],
        });
        layer.add_feature(Feature {
            id: Some(102),
            geom_type: GeomType::LineString,
            geometry: vec![9, 4, 4, 18, 0, 16, 16, 0],
            tags: vec![(k_kind, v_kind_road), (k_ratio, v_ratio), (k_rank, v_rank)],
        });
        layer.add_feature(Feature {
            id: Some(103),
            geom_type: GeomType::Polygon,
            geometry: vec![9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15],
            tags: vec![(k_kind, v_kind_land), (k_visible, v_visible)],
        });

        let expected_by_id: BTreeMap<u64, (Geometry<i32>, BTreeMap<String, JsonValue>)> = layer
            .features()
            .iter()
            .map(|feature| {
                let id = feature.id.expect("semantic fixture features must have ids");
                let geom = decode_feature_geometry(feature).expect("source geometry should decode");
                let props = expected_feature_properties(&layer, feature);
                (id, (geom, props))
            })
            .collect();

        let encoded = encode_tile(&[&layer]).expect("mlt encode should succeed");
        let parsed = parse_decoded_layers(&encoded).expect("mlt parse should succeed");
        assert_eq!(parsed.len(), 1);
        let fc = mlt_core::geojson::FeatureCollection::from_layers(parsed)
            .expect("feature collection conversion");
        assert_eq!(fc.features.len(), expected_by_id.len());

        for got in &fc.features {
            let id = got.id.expect("decoded feature should have id");
            let (want_geom, want_props) = expected_by_id
                .get(&id)
                .expect("decoded feature id should exist in source");
            assert_eq!(&got.geometry, want_geom, "geometry mismatch for id {id}");
            assert_eq!(
                &got.properties, want_props,
                "properties mismatch for id {id}"
            );
        }
    }

    #[test]
    fn mlt_geometry_fixtures_roundtrip() {
        let fixtures_json = include_str!("../tests/fixtures/mlt_fixtures/geometry_fixtures.json");
        let fixtures: Vec<GeometryFixture> = serde_json::from_str(fixtures_json).unwrap();

        for fixture in fixtures {
            let mut layer = LayerBuilder::new("fixture");
            let key = layer.intern_key("kind");
            let val = layer.intern_value(Value::String("fixture".to_string()));
            layer.add_feature(Feature {
                id: Some(1),
                geom_type: fixture_geom_type(&fixture.geom_type),
                geometry: fixture.geometry.clone(),
                tags: vec![(key, val)],
            });

            let encoded = encode_tile(&[&layer]).unwrap();
            let parsed = parse_decoded_layers(&encoded).unwrap();
            assert_eq!(parsed.len(), 1, "fixture {}", fixture.id);
            let fc = mlt_core::geojson::FeatureCollection::from_layers(parsed).unwrap();
            assert_eq!(fc.features.len(), 1, "fixture {}", fixture.id);
            let got = geometry_name(&fc.features[0].geometry);
            let source_geom = decode_feature_geometry(&Feature {
                id: Some(1),
                geom_type: fixture_geom_type(&fixture.geom_type),
                geometry: fixture.geometry.clone(),
                tags: Vec::new(),
            })
            .expect("fixture source geometry should decode");
            assert_eq!(
                got, fixture.expected_geojson_type,
                "fixture {} ({})",
                fixture.id, fixture.description
            );
            assert_eq!(
                fc.features[0].geometry, source_geom,
                "fixture {} ({}) geometry coordinates/rings mismatch",
                fixture.id, fixture.description
            );
        }
    }

    fn decode_fixture_value(tag: &PropertyTagFixture) -> Value {
        match tag.value_type.as_str() {
            "string" => Value::String(tag.value.clone()),
            "float" => Value::Float(
                tag.value
                    .parse::<f32>()
                    .expect("fixture float must parse as f32"),
            ),
            "double" => Value::Double(
                tag.value
                    .parse::<f64>()
                    .expect("fixture double must parse as f64"),
            ),
            "int" => Value::Int(
                tag.value
                    .parse::<i64>()
                    .expect("fixture int must parse as i64"),
            ),
            "uint" => Value::UInt(
                tag.value
                    .parse::<u64>()
                    .expect("fixture uint must parse as u64"),
            ),
            "sint" => Value::SInt(
                tag.value
                    .parse::<i64>()
                    .expect("fixture sint must parse as i64"),
            ),
            "bool" => Value::Bool(
                tag.value
                    .parse::<bool>()
                    .expect("fixture bool must parse as bool"),
            ),
            _ => panic!("unknown fixture value type"),
        }
    }

    fn decoded_property_kind(value: mlt_core::PropValueRef<'_>) -> &'static str {
        match value {
            mlt_core::PropValueRef::Bool(_) => "bool",
            mlt_core::PropValueRef::I8(_)
            | mlt_core::PropValueRef::I32(_)
            | mlt_core::PropValueRef::I64(_) => "i64",
            mlt_core::PropValueRef::U8(_)
            | mlt_core::PropValueRef::U32(_)
            | mlt_core::PropValueRef::U64(_) => "u64",
            mlt_core::PropValueRef::F32(_) => "f32",
            mlt_core::PropValueRef::F64(_) => "f64",
            mlt_core::PropValueRef::Str(_) => "str",
        }
    }

    #[test]
    fn mlt_property_fixtures_roundtrip() {
        let fixtures_json = include_str!("../tests/fixtures/mlt_fixtures/property_fixtures.json");
        let cases: Vec<PropertyFixtureCase> = serde_json::from_str(fixtures_json).unwrap();

        for case in cases {
            let mut layer = LayerBuilder::new("props_fixture");

            for f in &case.features {
                let mut tags = Vec::new();
                for tag in &f.tags {
                    let k = layer.intern_key(&tag.key);
                    let v = layer.intern_value(decode_fixture_value(tag));
                    tags.push((k, v));
                }
                layer.add_feature(Feature {
                    id: Some(f.id),
                    geom_type: fixture_geom_type(&f.geom_type),
                    geometry: f.geometry.clone(),
                    tags,
                });
            }

            let encoded = encode_tile(&[&layer]).expect("mlt encode should succeed");
            let parsed = parse_decoded_layers(&encoded).expect("mlt parse should succeed");
            assert_eq!(parsed.len(), 1, "case {}", case.id);
            let mlt_core::Layer::Tag01(l01) = &parsed[0] else {
                panic!("expected tag01 layer, case {}", case.id);
            };

            let expected: BTreeMap<&str, (&str, usize)> = case
                .expected
                .iter()
                .map(|e| (e.key.as_str(), (e.kind.as_str(), e.non_null)))
                .collect();

            let mut decoded: BTreeMap<String, (&'static str, usize)> = BTreeMap::new();
            let mut features = l01.iter_features();
            while let Some(feature) = features.next() {
                let feature = feature.expect("feature should decode");
                for prop in feature.iter_properties() {
                    let name = prop.name().to_string();
                    let kind = decoded_property_kind(prop.value());
                    let entry = decoded.entry(name).or_insert((kind, 0));
                    assert_eq!(entry.0, kind, "case {} property kind changed", case.id);
                    entry.1 += 1;
                }
            }

            let mut seen = 0usize;
            for (name, (got_kind, got_non_null)) in &decoded {
                if let Some((want_kind, want_non_null)) = expected.get(name.as_str()) {
                    assert_eq!(
                        *got_kind, *want_kind,
                        "case {} property '{}' kind mismatch",
                        case.id, name
                    );
                    assert_eq!(
                        *got_non_null, *want_non_null,
                        "case {} property '{}' non_null mismatch",
                        case.id, name
                    );
                    seen += 1;
                }
            }
            assert_eq!(
                seen,
                case.expected.len(),
                "case {} did not observe all expected properties",
                case.id
            );
        }
    }

    #[test]
    fn mlt_no_compression_size_guard_vs_mvt() {
        let mut layer = LayerBuilder::new("size_guard");
        let k_kind = layer.intern_key("kind");
        let k_name = layer.intern_key("name");
        let k_rank = layer.intern_key("rank");

        for i in 0..64u32 {
            let v_kind = layer.intern_value(Value::String("poi".to_string()));
            let v_name = layer.intern_value(Value::String(format!("name_{i}")));
            let v_rank = layer.intern_value(Value::UInt(u64::from(i % 10)));
            layer.add_feature(Feature {
                id: Some(u64::from(i) + 1),
                geom_type: GeomType::Point,
                geometry: vec![9, (i + 1) * 2, (i + 1) * 2],
                tags: vec![(k_kind, v_kind), (k_name, v_name), (k_rank, v_rank)],
            });
        }

        let mlt_bytes = encode_tile(&[&layer]).expect("mlt encode should succeed");
        let mvt_bytes = crate::mvt::encode_tile(&[&layer]);

        assert!(!mlt_bytes.is_empty());
        assert!(!mvt_bytes.is_empty());
        assert!(
            mlt_bytes.len() <= mvt_bytes.len() * 4,
            "mlt payload unexpectedly large: mlt={} mvt={}",
            mlt_bytes.len(),
            mvt_bytes.len()
        );
    }
}
