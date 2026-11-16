//! Per-layer extent and geometry command limits.

use super::tile::CheckedLayer;
use super::{Finding, ValidationClass};
use crate::mvt::decode::{
    self, Command, CommandKind, CommandSink, GeometryError, Grammar, Point, Words,
};

pub(super) const MVT_EXTENT: i64 = crate::geometry::tiles::grid::EXTENT as i64;
const MVT_COORD_ABS_LIMIT: i64 = MVT_EXTENT * 32; // 131072
const MVT_DELTA_LIMIT: i64 = MVT_EXTENT * 16; // 65536
pub(super) const MVT_DELTA_LIMIT_SEAM: i64 = MVT_EXTENT * 4; // 16384

/// A nonzero extent and every feature's command stream within limits; the
/// first violation in wire order.
pub(super) fn check_layer(
    layer: &CheckedLayer<'_>,
    mut class: ValidationClass,
) -> Result<(), Finding> {
    let layer_name = layer.message.name;
    if layer.message.extent == 0 {
        return Err(Finding::ZeroExtent {
            layer: layer_name.to_string(),
        });
    }
    class.extent = i64::from(layer.message.extent);
    for (feat_idx, feature) in layer.features.iter().enumerate() {
        // The strict parse guarantees a geometry field.
        let geometry = feature.message.geometry.as_deref().unwrap_or_default();
        validate_geometry(
            Words::packed(geometry),
            feature.message.geom_type,
            class,
            geometry.len(),
        )
        .map_err(|cause| Finding::Geometry {
            layer: layer_name.to_string(),
            feature: feat_idx,
            cause: cause.into(),
        })?;
    }
    Ok(())
}

/// verify's checks on top of the shared decoder and its spec grammar: the
/// vtzero count cap and the extent-scaled delta and absolute limits.
struct GeometryLimits {
    max_count: u32,
    delta_limit: i64,
    coord_limit: i64,
}

impl CommandSink for GeometryLimits {
    fn command(&mut self, command: Command) -> Result<(), GeometryError> {
        let name = match command.kind {
            CommandKind::MoveTo => "MoveTo",
            CommandKind::LineTo => "LineTo",
            CommandKind::ClosePath => return Ok(()),
        };
        if command.count > self.max_count {
            return Err(GeometryError::Consumer(format!(
                "{name} count too large ({} > {})",
                command.count, self.max_count
            )));
        }
        Ok(())
    }

    fn point(&mut self, point: Point) -> Result<(), GeometryError> {
        let (dx, dy) = (i64::from(point.dx), i64::from(point.dy));
        if dx.abs() > self.delta_limit || dy.abs() > self.delta_limit {
            return Err(GeometryError::Consumer(format!(
                "suspicious geometry delta ({dx},{dy}) exceeds limit {}",
                self.delta_limit
            )));
        }
        if point.x.abs() > self.coord_limit || point.y.abs() > self.coord_limit {
            return Err(GeometryError::Consumer(format!(
                "geometry coordinate ({},{}) exceeds absolute limit {}",
                point.x, point.y, self.coord_limit
            )));
        }
        Ok(())
    }
}

pub(super) fn validate_geometry(
    words: Words<'_>,
    geom_type: u8,
    class: ValidationClass,
    raw_byte_len: usize,
) -> Result<(), GeometryError> {
    let delta_limit = if class.seam {
        MVT_DELTA_LIMIT_SEAM / MVT_EXTENT * class.extent
    } else {
        MVT_DELTA_LIMIT / MVT_EXTENT * class.extent
    };
    // Maximum valid count: vtzero uses raw_byte_len / 2 (each varint is >=1
    // byte, each point needs 2 varints). This is a generous upper bound that
    // avoids false positives on large but valid geometries.
    let mut limits = GeometryLimits {
        max_count: u32::try_from(raw_byte_len / 2).unwrap_or(u32::MAX),
        delta_limit,
        coord_limit: MVT_COORD_ABS_LIMIT / MVT_EXTENT * class.extent,
    };
    decode::decode(words, Some(Grammar::for_geom_type(geom_type)), &mut limits)?;
    Ok(())
}
