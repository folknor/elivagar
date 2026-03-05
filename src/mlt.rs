use crate::mvt::{Feature, GeomType, LayerBuilder, Value};

use geo_types::{Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use mlt_core::Encodable;
use mlt_core::v01::{
    DecodedGeometry, DecodedId, DecodedProperty, GeometryEncoder, IdEncoder, IdWidth, IntEncoder,
    LogicalEncoder, OwnedGeometry, OwnedId, OwnedLayer01, OwnedProperty, PhysicalEncoder,
    PresenceStream, PropValue, ScalarEncoder,
};

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
        let owned_layer = encode_layer(layer)?;
        mlt_core::OwnedLayer::Tag01(owned_layer)
            .write_to(&mut out)
            .map_err(|e| MltEncodeError::Encode(e.to_string()))?;
    }
    Ok(out)
}

fn encode_layer(layer: &LayerBuilder) -> Result<OwnedLayer01, MltEncodeError> {
    let features = layer.features();
    let id = encode_layer_ids(features)?;
    let geometry = encode_layer_geometry(layer.name(), features)?;
    let properties = encode_layer_properties(layer, features)?;

    Ok(OwnedLayer01 {
        name: layer.name().to_string(),
        extent: 4096,
        id,
        geometry,
        properties,
    })
}

fn encode_layer_ids(features: &[Feature]) -> Result<OwnedId, MltEncodeError> {
    let ids: Vec<Option<u64>> = features.iter().map(|f| f.id).collect();
    if ids.iter().all(Option::is_none) {
        return Ok(OwnedId::None);
    }

    let mut id = OwnedId::Decoded(DecodedId(Some(ids)));
    id.encode_with(IdEncoder::new(LogicalEncoder::Delta, IdWidth::OptId64))
        .map_err(|e| MltEncodeError::Encode(e.to_string()))?;
    Ok(id)
}

fn encode_layer_geometry(layer_name: &str, features: &[Feature]) -> Result<OwnedGeometry, MltEncodeError> {
    let mut decoded = DecodedGeometry::default();
    for (idx, feature) in features.iter().enumerate() {
        let geom = decode_feature_geometry(feature).map_err(|msg| MltEncodeError::GeometryDecode {
            layer: layer_name.to_string(),
            feature_index: idx,
            message: msg,
        })?;
        decoded.push_geom(&geom);
    }

    let mut geometry = OwnedGeometry::Decoded(decoded);
    geometry
        .encode_with(GeometryEncoder::all(IntEncoder::varint()))
        .map_err(|e| MltEncodeError::Encode(e.to_string()))?;
    Ok(geometry)
}

fn encode_layer_properties(
    layer: &LayerBuilder,
    features: &[Feature],
) -> Result<Vec<OwnedProperty>, MltEncodeError> {
    let tile_model = build_tile_model(&[layer]);
    let columns = tile_model
        .layers
        .first()
        .map(|lm| lm.columns.as_slice())
        .unwrap_or(&[]);

    let mut properties = Vec::with_capacity(columns.len());
    for col in columns {
        let decoded = build_property_column(layer, features, col);
        let mut prop = OwnedProperty::Decoded(decoded);
        prop.encode_with(property_encoder_for(&prop))
            .map_err(|e| MltEncodeError::Encode(e.to_string()))?;
        properties.push(prop);
    }

    Ok(properties)
}

fn property_encoder_for(prop: &OwnedProperty) -> ScalarEncoder {
    let values = match prop {
        OwnedProperty::Decoded(decoded) => &decoded.values,
        OwnedProperty::Encoded(_) => return ScalarEncoder::int(PresenceStream::Present, IntEncoder::varint()),
    };
    match values {
        PropValue::Str(_) => ScalarEncoder::str_fsst(
            PresenceStream::Present,
            IntEncoder::varint(),
            IntEncoder::varint(),
        ),
        PropValue::F32(_) | PropValue::F64(_) => ScalarEncoder::float(PresenceStream::Present),
        PropValue::Bool(_) => ScalarEncoder::bool(PresenceStream::Present),
        _ => ScalarEncoder::int(PresenceStream::Present, IntEncoder::varint()),
    }
}

fn build_property_column(
    layer: &LayerBuilder,
    features: &[Feature],
    column: &MltColumnModel,
) -> DecodedProperty {
    let values = match column.column_type {
        MltColumnType::String => {
            let mut out = Vec::with_capacity(features.len());
            for feature in features {
                let mapped = feature_value_for_key(layer, feature, &column.key)
                    .map(value_to_string);
                out.push(mapped);
            }
            PropValue::Str(out)
        }
        MltColumnType::Float => {
            let mut out = Vec::with_capacity(features.len());
            for feature in features {
                let mapped = feature_value_for_key(layer, feature, &column.key).and_then(|v| match v {
                    Value::Float(x) => Some(*x),
                    _ => None,
                });
                out.push(mapped);
            }
            PropValue::F32(out)
        }
        MltColumnType::Double => {
            let mut out = Vec::with_capacity(features.len());
            for feature in features {
                let mapped = feature_value_for_key(layer, feature, &column.key).and_then(|v| match v {
                    Value::Double(x) => Some(*x),
                    _ => None,
                });
                out.push(mapped);
            }
            PropValue::F64(out)
        }
        MltColumnType::Int => {
            let mut out = Vec::with_capacity(features.len());
            for feature in features {
                let mapped = feature_value_for_key(layer, feature, &column.key).and_then(|v| match v {
                    Value::Int(x) => Some(*x),
                    _ => None,
                });
                out.push(mapped);
            }
            PropValue::I64(out)
        }
        MltColumnType::UInt => {
            let mut out = Vec::with_capacity(features.len());
            for feature in features {
                let mapped = feature_value_for_key(layer, feature, &column.key).and_then(|v| match v {
                    Value::UInt(x) => Some(*x),
                    _ => None,
                });
                out.push(mapped);
            }
            PropValue::U64(out)
        }
        MltColumnType::SInt => {
            let mut out = Vec::with_capacity(features.len());
            for feature in features {
                let mapped = feature_value_for_key(layer, feature, &column.key).and_then(|v| match v {
                    Value::SInt(x) => Some(*x),
                    _ => None,
                });
                out.push(mapped);
            }
            PropValue::I64(out)
        }
        MltColumnType::Bool => {
            let mut out = Vec::with_capacity(features.len());
            for feature in features {
                let mapped = feature_value_for_key(layer, feature, &column.key).and_then(|v| match v {
                    Value::Bool(x) => Some(*x),
                    _ => None,
                });
                out.push(mapped);
            }
            PropValue::Bool(out)
        }
        MltColumnType::Mixed => {
            let mut out = Vec::with_capacity(features.len());
            for feature in features {
                let mapped = feature_value_for_key(layer, feature, &column.key)
                    .map(value_to_string);
                out.push(mapped);
            }
            PropValue::Str(out)
        }
    };

    DecodedProperty {
        name: column.key.clone(),
        values,
    }
}

fn feature_value_for_key<'a>(layer: &'a LayerBuilder, feature: &Feature, key: &str) -> Option<&'a Value> {
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
                    out.push(Coord { x: cursor_x, y: cursor_y });
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
                    current.push(Coord { x: cursor_x, y: cursor_y });
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
                    current.push(Coord { x: cursor_x, y: cursor_y });
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
        let mut geometry_mix = MltGeometryMix::default();
        for feature in layer.features() {
            match feature.geom_type {
                GeomType::Point => geometry_mix.points += 1,
                GeomType::LineString => geometry_mix.lines += 1,
                GeomType::Polygon => geometry_mix.polygons += 1,
            }
            for &(k_idx, v_idx) in &feature.tags {
                let Some(key) = layer.key(k_idx) else { continue };
                let Some(value) = layer.value(v_idx) else {
                    continue;
                };
                let value_ty = value_type(value);
                if let Some(existing) = columns.iter_mut().find(|c| c.key == key) {
                    existing.value_count += 1;
                    if existing.column_type != value_ty {
                        existing.column_type = MltColumnType::Mixed;
                        existing.observed_type_count = 2;
                    }
                } else {
                    columns.push(MltColumnModel {
                        key: key.to_string(),
                        column_type: value_ty,
                        value_count: 1,
                        observed_type_count: 1,
                    });
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::mvt::{Feature, GeomType, LayerBuilder};
    use geo_types::Geometry;
    use serde::Deserialize;
    use std::collections::HashMap;

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
        assert_eq!(model.layers[0].columns[0].column_type, MltColumnType::String);
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

        let mut parsed = mlt_core::parse_layers(&encoded).expect("encoded mlt should parse");
        assert_eq!(parsed.len(), 1);
        parsed[0].decode_all().expect("decode_all should succeed");
        let l01 = parsed[0].as_layer01().expect("expected tag01 layer");
        assert_eq!(l01.name, "test");
        assert_eq!(l01.extent, 4096);
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
                geometry: fixture.geometry,
                tags: vec![(key, val)],
            });

            let encoded = encode_tile(&[&layer]).unwrap();
            let mut parsed = mlt_core::parse_layers(&encoded).unwrap();
            assert_eq!(parsed.len(), 1, "fixture {}", fixture.id);
            parsed[0].decode_all().unwrap();
            let fc = mlt_core::geojson::FeatureCollection::from_layers(&parsed).unwrap();
            assert_eq!(fc.features.len(), 1, "fixture {}", fixture.id);
            let got = geometry_name(&fc.features[0].geometry);
            assert_eq!(
                got, fixture.expected_geojson_type,
                "fixture {} ({})",
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

    fn decoded_property_kind_and_count(
        values: &mlt_core::v01::PropValue,
    ) -> (&'static str, usize) {
        match values {
            mlt_core::v01::PropValue::Bool(v) => ("bool", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::I8(v) => ("i8", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::U8(v) => ("u8", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::I32(v) => ("i32", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::U32(v) => ("u32", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::I64(v) => ("i64", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::U64(v) => ("u64", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::F32(v) => ("f32", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::F64(v) => ("f64", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::Str(v) => ("str", v.iter().filter(|x| x.is_some()).count()),
            mlt_core::v01::PropValue::SharedDict => ("shared_dict", 0),
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
            let mut parsed = mlt_core::parse_layers(&encoded).expect("mlt parse should succeed");
            assert_eq!(parsed.len(), 1, "case {}", case.id);
            parsed[0].decode_all().expect("decode_all should succeed");
            let l01 = parsed[0].as_layer01().expect("expected tag01 layer");

            let expected: HashMap<&str, (&str, usize)> = case
                .expected
                .iter()
                .map(|e| (e.key.as_str(), (e.kind.as_str(), e.non_null)))
                .collect();

            let mut seen = 0usize;
            for prop in &l01.properties {
                let decoded = match prop {
                    mlt_core::v01::Property::Decoded(v) => v,
                    mlt_core::v01::Property::Encoded(_) => panic!("property should be decoded"),
                };
                if let Some((want_kind, want_non_null)) = expected.get(decoded.name.as_str()) {
                    let (got_kind, got_non_null) = decoded_property_kind_and_count(&decoded.values);
                    assert_eq!(
                        got_kind, *want_kind,
                        "case {} property '{}' kind mismatch",
                        case.id, decoded.name
                    );
                    assert_eq!(
                        got_non_null, *want_non_null,
                        "case {} property '{}' non_null mismatch",
                        case.id, decoded.name
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
}
