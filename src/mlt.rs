use crate::geometry;
use crate::mvt::decode::{
    self, Command, CommandKind, CommandSink, GeometryError, Grammar, Point as CommandPoint, Words,
};
use crate::mvt::{Feature, GeomType, LayerBuilder, Value};

use geo_types::{
    Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
use mlt_core::encoder::{EncoderConfig, Profile};
use mlt_core::{PropKind, PropValue as MltPropValue, TileLayer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MltColumnType {
    String,
    Double,
    Int,
    Bool,
    Mixed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MltColumnModel {
    pub key: String,
    pub column_type: MltColumnType,
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
        source: MltGeometryError,
    },
    Encode(mlt_core::MltError),
}

impl std::fmt::Display for MltEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GeometryDecode {
                layer,
                feature_index,
                source,
            } => write!(
                f,
                "geometry decode failed for layer '{layer}' feature#{feature_index}: {source}"
            ),
            Self::Encode(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for MltEncodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::GeometryDecode { source, .. } => Some(source),
            Self::Encode(source) => Some(source),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MltGeometryError {
    Command(GeometryError),
    EmptyPoint,
    NoValidPaths,
    NoRings,
    NoNonzeroRings,
    ShortRing,
}

impl From<GeometryError> for MltGeometryError {
    fn from(error: GeometryError) -> Self {
        Self::Command(error)
    }
}

impl std::fmt::Display for MltGeometryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Command(error) => return error.fmt(f),
            Self::EmptyPoint => "point geometry is empty",
            Self::NoValidPaths => "linestring geometry has no valid paths",
            Self::NoRings => "polygon geometry has no rings",
            Self::NoNonzeroRings => "polygon geometry has no nonzero-area rings",
            Self::ShortRing => "polygon ring has fewer than 3 vertices",
        };
        f.write_str(message)
    }
}

impl std::error::Error for MltGeometryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Command(error) => Some(error),
            _ => None,
        }
    }
}

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
        .encode(EncoderConfig::from_profile(Profile::default()))
        .map_err(MltEncodeError::Encode)
}

fn build_mlt_tile_layer(layer: &LayerBuilder) -> Result<TileLayer, MltEncodeError> {
    let features = layer.features();
    let tile_model = build_tile_model(&[layer]);
    let columns = tile_model
        .layers
        .first()
        .map(|lm| lm.columns.as_slice())
        .unwrap_or(&[]);
    let mut builder = mlt_core::TileLayer::builder(
        layer.name(),
        crate::geometry::tiles::grid::EXTENT.unsigned_abs(),
    )
    .map_err(MltEncodeError::Encode)?;
    let mut property_keys = Vec::with_capacity(columns.len());

    for col in columns {
        let key = builder
            .add_property(col.key.clone(), mlt_prop_kind(col.column_type))
            .map_err(MltEncodeError::Encode)?;
        property_keys.push(key);
    }

    for (idx, feature) in features.iter().enumerate() {
        let geom =
            decode_feature_geometry(feature).map_err(|source| MltEncodeError::GeometryDecode {
                layer: layer.name().to_string(),
                feature_index: idx,
                source,
            })?;
        let mut feature_builder = builder.feature(geom);
        feature_builder.id(feature.id);
        for (col, key) in columns.iter().zip(&property_keys) {
            if let Some(value) = feature_value_for_key(layer, feature, &col.key) {
                feature_builder
                    .property(*key, value_to_mlt_prop(value, col.column_type))
                    .map_err(MltEncodeError::Encode)?;
            }
        }
        feature_builder.finish().map_err(MltEncodeError::Encode)?;
    }

    Ok(builder.finish())
}

fn mlt_prop_kind(column_type: MltColumnType) -> PropKind {
    match column_type {
        MltColumnType::String | MltColumnType::Mixed => PropKind::Str,
        MltColumnType::Double => PropKind::F64,
        MltColumnType::Int => PropKind::I64,
        MltColumnType::Bool => PropKind::Bool,
    }
}

fn value_to_mlt_prop(value: &Value, column_type: MltColumnType) -> MltPropValue {
    match column_type {
        MltColumnType::String => MltPropValue::Str(match value {
            Value::String(x) => Some(x.as_str().into()),
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
        MltColumnType::Bool => MltPropValue::Bool(match value {
            Value::Bool(x) => Some(*x),
            _ => None,
        }),
        MltColumnType::Mixed => {
            let text = match value {
                Value::String(x) => x.clone(),
                Value::Double(x) => x.to_string(),
                Value::Int(x) => x.to_string(),
                Value::Bool(x) => x.to_string(),
            };
            MltPropValue::Str(Some(text.into()))
        }
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

fn decode_feature_geometry(feature: &Feature) -> Result<Geometry<i32>, MltGeometryError> {
    match feature.geom_type {
        GeomType::Point => decode_point_geometry(&feature.geometry),
        GeomType::LineString => decode_line_geometry(&feature.geometry),
        GeomType::Polygon => decode_polygon_geometry(&feature.geometry),
    }
}

fn decode_point_geometry(commands: &[u32]) -> Result<Geometry<i32>, MltGeometryError> {
    let points = parse_points(commands)?;
    if points.is_empty() {
        return Err(MltGeometryError::EmptyPoint);
    }
    if points.len() == 1 {
        return Ok(Geometry::Point(Point(points[0])));
    }
    let multi = MultiPoint(points.into_iter().map(Point).collect());
    Ok(Geometry::MultiPoint(multi))
}

fn decode_line_geometry(commands: &[u32]) -> Result<Geometry<i32>, MltGeometryError> {
    let paths = parse_paths(commands, Grammar::LineString)?;
    let mut lines = Vec::new();
    for path in paths {
        if path.len() < 2 {
            continue;
        }
        lines.push(LineString(path));
    }

    if lines.is_empty() {
        return Err(MltGeometryError::NoValidPaths);
    }
    if lines.len() == 1 {
        return Ok(Geometry::LineString(lines.remove(0)));
    }
    Ok(Geometry::MultiLineString(MultiLineString(lines)))
}

fn decode_polygon_geometry(commands: &[u32]) -> Result<Geometry<i32>, MltGeometryError> {
    let ring_points = parse_paths(commands, Grammar::Polygon)?;
    let mut rings = Vec::new();
    for points in ring_points {
        rings.push(close_ring(points)?);
    }
    if rings.is_empty() {
        return Err(MltGeometryError::NoRings);
    }

    let mut polygons = group_rings_to_polygons(rings);
    if polygons.is_empty() {
        return Err(MltGeometryError::NoNonzeroRings);
    }
    if polygons.len() == 1
        && let Some(poly) = polygons.pop()
    {
        return Ok(Geometry::Polygon(poly));
    }
    Ok(Geometry::MultiPolygon(MultiPolygon(polygons)))
}

fn close_ring(mut points: Vec<Coord<i32>>) -> Result<LineString<i32>, MltGeometryError> {
    if points.len() < 3 {
        return Err(MltGeometryError::ShortRing);
    }
    if let (Some(first), Some(last)) = (points.first().copied(), points.last().copied())
        && first != last
    {
        points.push(first);
    }
    Ok(LineString(points))
}

fn group_rings_to_polygons(rings: Vec<LineString<i32>>) -> Vec<Polygon<i32>> {
    geometry::polygon::group_rings(rings, |ring| {
        geometry::polygon::signed_area_2x(ring.0.iter().map(|c| (c.x, c.y)))
    })
    .into_iter()
    .map(|group| Polygon::new(group.outer, group.holes))
    .collect()
}

/// Our encoder's point stream, under the strict point grammar.
fn parse_points(commands: &[u32]) -> Result<Vec<Coord<i32>>, MltGeometryError> {
    let mut paths = parse_paths(commands, Grammar::Point)?;
    Ok(paths.drain(..).flatten().collect())
}

/// Our encoder's line or polygon stream as paths, one per MoveTo point,
/// under the strict grammar for its type. ClosePath adds no vertex: polygon
/// conversion closes a ring only when it is open.
fn parse_paths(
    commands: &[u32],
    grammar: Grammar,
) -> Result<Vec<Vec<Coord<i32>>>, MltGeometryError> {
    struct Paths {
        paths: Vec<Vec<Coord<i32>>>,
        in_move: bool,
    }
    impl CommandSink for Paths {
        fn command(&mut self, command: Command) -> Result<(), GeometryError> {
            match command.kind {
                CommandKind::MoveTo => self.in_move = true,
                CommandKind::LineTo => self.in_move = false,
                CommandKind::ClosePath => {}
            }
            Ok(())
        }
        fn point(&mut self, point: CommandPoint) -> Result<(), GeometryError> {
            let (x, y) = point.xy_i32()?;
            if self.in_move || self.paths.is_empty() {
                self.paths.push(Vec::new());
            }
            if let Some(path) = self.paths.last_mut() {
                path.push(Coord { x, y });
            }
            Ok(())
        }
    }
    let mut paths = Paths {
        paths: Vec::new(),
        in_move: false,
    };
    decode::decode(Words::slice(commands), Some(grammar), &mut paths)?;
    Ok(paths.paths)
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
                let Some(key) = layer.key(k_idx) else {
                    continue;
                };
                let Some(value) = layer.value(v_idx) else {
                    continue;
                };
                let value_ty = value_type(value);
                if let Some(column) = columns.iter_mut().find(|c| c.key == key) {
                    if column.column_type != value_ty {
                        column.column_type = MltColumnType::Mixed;
                    }
                } else {
                    columns.push(MltColumnModel {
                        key: key.to_string(),
                        column_type: value_ty,
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
        Value::Double(_) => MltColumnType::Double,
        Value::Int(_) => MltColumnType::Int,
        Value::Bool(_) => MltColumnType::Bool,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{Feature, GeomType, LayerBuilder};
    use geo_types::Geometry;
    use mlt_core::LendingIterator as _;
    use serde::Deserialize;
    use serde_json::{Number, Value as JsonValue};
    use std::collections::BTreeMap;

    #[test]
    fn encode_errors_retain_typed_causes() {
        use std::error::Error;
        let mut layer = LayerBuilder::new("broken");
        layer.add_feature(Feature {
            id: None,
            geom_type: GeomType::Point,
            geometry: vec![9],
            tags: Vec::new(),
        });
        let error = encode_tile(&[&layer]).expect_err("truncated geometry");
        assert!(matches!(&error, MltEncodeError::GeometryDecode {
            layer, feature_index: 0,
            source: MltGeometryError::Command(GeometryError::Truncated),
        } if layer == "broken"));
        assert!(
            error
                .source()
                .expect("geometry cause")
                .source()
                .expect("command cause")
                .is::<GeometryError>()
        );
        let error = MltEncodeError::Encode(mlt_core::MltError::InvalidExtent(0));
        assert!(matches!(
            &error,
            MltEncodeError::Encode(mlt_core::MltError::InvalidExtent(0))
        ));
        assert!(
            error
                .source()
                .expect("encoder cause")
                .is::<mlt_core::MltError>()
        );
        for result in [
            decode_point_geometry(&[]),
            decode_line_geometry(&[]),
            decode_polygon_geometry(&[]),
        ] {
            assert!(matches!(
                result,
                Err(MltGeometryError::Command(GeometryError::Grammar(_)))
            ));
        }
        assert_eq!(close_ring(Vec::new()), Err(MltGeometryError::ShortRing));
    }

    fn polygon_ring(points: &[(i32, i32)]) -> LineString<i32> {
        LineString(points.iter().map(|&(x, y)| Coord { x, y }).collect())
    }

    #[test]
    fn polygon_grouping_uses_first_nonzero_winding() {
        let zero_area = polygon_ring(&[(0, 0), (10, 0), (20, 0), (0, 0)]);
        let negative_first = polygon_ring(&[(0, 0), (0, 10), (10, 10), (10, 0), (0, 0)]);
        let same_winding_outer = polygon_ring(&[(20, 0), (20, 10), (30, 10), (30, 0), (20, 0)]);
        let opposite_winding_hole = polygon_ring(&[(2, 2), (8, 2), (8, 8), (2, 8), (2, 2)]);

        let polygons = group_rings_to_polygons(vec![
            zero_area,
            negative_first,
            same_winding_outer,
            opposite_winding_hole,
        ]);

        assert_eq!(polygons.len(), 2);
        assert!(polygons[0].interiors().is_empty());
        assert_eq!(polygons[1].interiors().len(), 1);
    }

    #[test]
    fn lone_zero_area_ring_encodes_as_a_degenerate_polygon() {
        // MoveTo (0,0), LineTo (10,0) (20,0), ClosePath: one collinear ring.
        let mut layer = LayerBuilder::new("degenerate");
        layer.add_feature(Feature {
            id: None,
            geom_type: GeomType::Polygon,
            geometry: vec![9, 0, 0, 18, 20, 0, 20, 0, 15],
            tags: Vec::new(),
        });
        let Geometry::Polygon(polygon) =
            decode_feature_geometry(&layer.features()[0]).expect("lone ring is kept")
        else {
            panic!("expected one polygon");
        };
        assert!(polygon.interiors().is_empty());
        let encoded = encode_tile(&[&layer]).expect("mlt encode should succeed");
        let parsed = parse_decoded_layers(&encoded).expect("encoded mlt should parse");
        assert_eq!(parsed.len(), 1);
    }

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
    fn property_lookup_preserves_indices_above_u16() {
        let mut layer = LayerBuilder::new("wide");
        let mut key = 0;
        let mut value = 0;
        for i in 0..65_537_u32 {
            key = layer.intern_key(&format!("key_{i}"));
            value = layer.intern_value(Value::Int(i64::from(i)));
        }
        assert_eq!((key, value), (65_536, 65_536));
        let feature = Feature {
            id: None,
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(key, value)],
        };
        assert_eq!(
            feature_value_for_key(&layer, &feature, "key_65536"),
            Some(&Value::Int(65_536)),
        );
        layer.add_feature(feature);
        let model = build_tile_model(&[&layer]);
        assert_eq!(model.layers[0].columns.len(), 1);
        assert_eq!(model.layers[0].columns[0].key, "key_65536");
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
        let v_level = layer.intern_value(Value::Int(5));

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
    }

    #[test]
    fn build_tile_model_maps_pipeline_value_types() {
        let mut layer = LayerBuilder::new("types");
        let ks = layer.intern_key("s");
        let kd = layer.intern_key("d");
        let ki = layer.intern_key("i");
        let kb = layer.intern_key("b");

        let vs = layer.intern_value(Value::String("a".to_string()));
        let vd = layer.intern_value(Value::Double(2.5));
        let vi = layer.intern_value(Value::Int(-3));
        let vb = layer.intern_value(Value::Bool(true));

        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: vec![(ks, vs), (kd, vd), (ki, vi), (kb, vb)],
        });

        let model = build_tile_model(&[&layer]);
        let cols = &model.layers[0].columns;
        assert_eq!(
            cols.iter().find(|c| c.key == "s").map(|c| c.column_type),
            Some(MltColumnType::String)
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

    fn mvt_value_to_json(value: &Value) -> JsonValue {
        match value {
            Value::String(v) => JsonValue::String(v.clone()),
            Value::Double(v) => Number::from_f64(*v).map_or(JsonValue::Null, JsonValue::Number),
            Value::Int(v) => JsonValue::Number((*v).into()),
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
        let v_pop = layer.intern_value(Value::Int(700_000));
        let v_ratio = layer.intern_value(Value::Double(1.25));
        let v_rank = layer.intern_value(Value::Int(-2));
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
        let fixtures: Vec<GeometryFixture> =
            serde_json::from_str(fixtures_json).expect("valid fixture");

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

            let encoded = encode_tile(&[&layer]).expect("valid fixture");
            let parsed = parse_decoded_layers(&encoded).expect("valid fixture");
            assert_eq!(parsed.len(), 1, "fixture {}", fixture.id);
            let fc =
                mlt_core::geojson::FeatureCollection::from_layers(parsed).expect("valid fixture");
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
        let cases: Vec<PropertyFixtureCase> =
            serde_json::from_str(fixtures_json).expect("valid fixture");

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
            assert_eq!(
                decoded.len(),
                expected.len(),
                "case {} property count",
                case.id
            );
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
                expected.len(),
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
            let v_rank = layer.intern_value(Value::Int(i64::from(i % 10)));
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
