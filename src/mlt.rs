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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MltLayerModel {
    pub name: String,
    pub feature_count: usize,
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
        for feature in layer.features() {
            for &(k_idx, v_idx) in &feature.tags {
                let Some(key) = layer.key(k_idx) else { continue };
                let Some(value) = layer.value(v_idx) else {
                    continue;
                };
                let value_ty = value_type(value);
                if let Some(existing) = columns.iter_mut().find(|c| c.key == key) {
                    if existing.column_type != value_ty {
                        existing.column_type = MltColumnType::Mixed;
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
        assert_eq!(model.layers[0].columns.len(), 2);
        assert_eq!(model.layers[0].columns[0].key, "kind");
        assert_eq!(model.layers[0].columns[0].column_type, MltColumnType::String);
        assert_eq!(model.layers[0].columns[1].key, "population");
        assert_eq!(
            model.layers[0].columns[1].column_type,
            MltColumnType::Mixed
        );
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
}
