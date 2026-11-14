//! Ocean-layer ring simplicity and the `--geometry-stats` accumulator.

use super::tile::{CheckedFeature, CheckedLayer};
use super::{Finding, GeometryStats, ZoomGeomStats};
use crate::mvt::decode::{self, Words};
use crate::shortbread::Layer;

/// The ocean layers of a tile.
fn ocean_layers<'t, 'a>(
    tile: &'t [CheckedLayer<'a>],
) -> impl Iterator<Item = &'t CheckedLayer<'a>> {
    tile.iter()
        .filter(|layer| layer.message.name == Layer::Ocean.name())
}

/// A feature's polygon geometry bytes, None for any other type.
fn polygon_geometry<'f>(feature: &'f CheckedFeature<'_>) -> Option<&'f [u8]> {
    (feature.message.geom_type == 3)
        .then_some(feature.message.geometry.as_deref())
        .flatten()
}

/// Every self-intersecting or undecodable ocean polygon ring, appended in
/// wire order.
pub(super) fn check_rings(tile: &[CheckedLayer<'_>], problems: &mut Vec<std::sync::Arc<Finding>>) {
    for layer in ocean_layers(tile) {
        for (fi, feature) in layer.features.iter().enumerate() {
            let Some(gb) = polygon_geometry(feature) else {
                continue;
            };
            let rings = match decode::decode_polygon_rings(Words::packed(gb)) {
                Ok(rings) => rings,
                Err(error) => {
                    problems.push(std::sync::Arc::new(Finding::OceanDecode {
                        feature: fi,
                        cause: error.into(),
                    }));
                    continue;
                }
            };
            for (ri, ring) in rings.iter().enumerate() {
                if ring.len() >= 4 && !crate::geometry::polygon::ring_is_simple(ring) {
                    problems.push(std::sync::Arc::new(Finding::OceanSelfIntersection {
                        feature: fi,
                        ring: ri,
                        vertices: ring.len(),
                    }));
                }
            }
        }
    }
}

pub(super) fn add_zoom_stats(target: &mut ZoomGeomStats, delta: &ZoomGeomStats) {
    target.features += delta.features;
    target.rings += delta.rings;
    target.encoded_bytes += delta.encoded_bytes;
    target.consecutive_dups += delta.consecutive_dups;
    target.full_tile_features += delta.full_tile_features;
    target
        .ring_vertex_counts
        .extend_from_slice(&delta.ring_vertex_counts);
}

/// Accumulate the ocean-layer geometry statistics of one tile.
pub(super) fn collect_stats(tile: &[CheckedLayer<'_>], z: u8, stats: &mut GeometryStats) {
    for layer in ocean_layers(tile) {
        // A zero extent is reported by the geometry check.
        if layer.message.extent == 0 {
            continue;
        }
        collect_layer_stats(layer, z, stats);
    }
}

#[allow(clippy::cast_possible_truncation)]
fn collect_layer_stats(layer: &CheckedLayer<'_>, z: u8, stats: &mut GeometryStats) {
    let extent = i64::from(layer.message.extent);
    let zs = stats.per_zoom.entry(z).or_default();
    zs.encoded_bytes += layer.message.encoded_len as u64;
    for feature in &layer.features {
        let Some(gb) = polygon_geometry(feature) else {
            continue;
        };
        // A polygon that does not decode is reported by the geometry and
        // ring checks on the same tile; statistics skip it.
        let Ok(rings) = decode::decode_polygon_rings(Words::packed(gb)) else {
            continue;
        };
        if rings.is_empty() {
            continue;
        }

        zs.features += 1;
        zs.rings += rings.len() as u64;
        for ring in &rings {
            zs.ring_vertex_counts.push(ring.len() as u32);
            // Adjacent equal pairs; the closing vertex duplicates the FIRST
            // vertex (not its predecessor), so closure never counts.
            for pair in ring.windows(2) {
                if pair[0] == pair[1] {
                    zs.consecutive_dups += 1;
                }
            }
        }
        if rings.len() == 1 && is_full_tile_rect_with_extent(&rings[0], extent) {
            zs.full_tile_features += 1;
        }
    }
}

/// A closed 4-corner rectangle ring covering the full [0, extent] square
/// (i.e. a buffered full-tile ocean fill).
pub(super) fn is_full_tile_rect_with_extent(ring: &[(i32, i32)], ext: i64) -> bool {
    let n = if ring.first() == ring.last() && ring.len() > 1 {
        ring.len() - 1
    } else {
        ring.len()
    };
    if n != 4 {
        return false;
    }
    let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
    let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
    for &(x, y) in &ring[..n] {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
    }
    // Every vertex must be a corner of the bbox.
    let corners = [
        (min_x, min_y),
        (max_x, min_y),
        (max_x, max_y),
        (min_x, max_y),
    ];
    let all_corners = ring[..n].iter().all(|v| corners.contains(v));
    all_corners && min_x <= 0 && min_y <= 0 && i64::from(max_x) >= ext && i64::from(max_y) >= ext
}
