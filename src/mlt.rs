use crate::mvt::{LayerBuilder, Value};

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
    NotImplemented {
        layer_count: usize,
        feature_count: usize,
    },
}

impl std::fmt::Display for MltEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotImplemented {
                layer_count,
                feature_count,
            } => write!(
                f,
                "mlt encoder is not implemented yet (layers={layer_count}, features={feature_count})"
            ),
        }
    }
}

impl std::error::Error for MltEncodeError {}

pub(crate) fn encode_tile(layers: &[&LayerBuilder]) -> Result<Vec<u8>, MltEncodeError> {
    let model = build_tile_model(layers);
    Err(MltEncodeError::NotImplemented {
        layer_count: model.layer_count,
        feature_count: model.feature_count,
    })
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
                crate::mvt::GeomType::Point => geometry_mix.points += 1,
                crate::mvt::GeomType::LineString => geometry_mix.lines += 1,
                crate::mvt::GeomType::Polygon => geometry_mix.polygons += 1,
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
        assert_eq!(
            model.layers[0].columns[1].column_type,
            MltColumnType::Mixed
        );
        assert_eq!(model.layers[0].columns[1].value_count, 2);
        assert_eq!(model.layers[0].columns[1].observed_type_count, 2);
    }

    #[test]
    fn encode_tile_returns_counts_in_not_implemented_error() {
        let mut layer = LayerBuilder::new("test");
        layer.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Point,
            geometry: point_geom(),
            tags: Vec::new(),
        });

        let err = encode_tile(&[&layer]).expect_err("mlt path should be unimplemented");
        match err {
            MltEncodeError::NotImplemented {
                layer_count,
                feature_count,
            } => {
                assert_eq!(layer_count, 1);
                assert_eq!(feature_count, 1);
            }
        }
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
        // Columns sorted by key.
        assert_eq!(lm.columns[0].key, "level");
        assert_eq!(lm.columns[1].key, "name");
        // Each key appears on one feature only.
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
}
