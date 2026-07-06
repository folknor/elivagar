use super::EXTENT;
use super::projection::{MercBbox, Point};

/// Find all tiles at a given zoom level that intersect the given Mercator bbox.
///
/// Returns a list of `(tile_x, tile_y)` pairs.
#[cfg(test)]
pub fn tiles_for_bbox(bbox: &MercBbox, zoom: u8) -> Vec<(u32, u32)> {
    let z_scale = f64::from(1u32 << zoom);
    let max_tile = (1u32 << zoom).saturating_sub(1);

    let tx_min = clamp_tile(bbox.min_x * z_scale, max_tile);
    let tx_max = clamp_tile(bbox.max_x * z_scale, max_tile);
    let ty_min = clamp_tile(bbox.min_y * z_scale, max_tile);
    let ty_max = clamp_tile(bbox.max_y * z_scale, max_tile);

    let mut tiles = Vec::with_capacity(((tx_max - tx_min + 1) * (ty_max - ty_min + 1)) as usize);
    for ty in ty_min..=ty_max {
        for tx in tx_min..=tx_max {
            tiles.push((tx, ty));
        }
    }
    tiles
}

/// Check whether a bbox falls entirely within a single tile at the given zoom.
///
/// When true, clipping is unnecessary - all geometry coordinates are within
/// the tile's non-buffered bounds (a subset of the buffered clip rect).
#[inline]
pub fn is_single_tile(bbox: &MercBbox, zoom: u8) -> bool {
    let z_scale = f64::from(1u32 << zoom);
    let max_tile = (1u32 << zoom).saturating_sub(1);
    let tx_min = clamp_tile(bbox.min_x * z_scale, max_tile);
    let tx_max = clamp_tile(bbox.max_x * z_scale, max_tile);
    tx_min == tx_max && {
        let ty_min = clamp_tile(bbox.min_y * z_scale, max_tile);
        let ty_max = clamp_tile(bbox.max_y * z_scale, max_tile);
        ty_min == ty_max
    }
}

/// Iterate tiles for a bbox without allocating a Vec.
#[inline]
pub fn for_each_tile_in_bbox<F>(bbox: &MercBbox, zoom: u8, mut f: F)
where
    F: FnMut(u32, u32),
{
    let z_scale = f64::from(1u32 << zoom);
    let max_tile = (1u32 << zoom).saturating_sub(1);

    let tx_min = clamp_tile(bbox.min_x * z_scale, max_tile);
    let tx_max = clamp_tile(bbox.max_x * z_scale, max_tile);
    let ty_min = clamp_tile(bbox.min_y * z_scale, max_tile);
    let ty_max = clamp_tile(bbox.max_y * z_scale, max_tile);

    for ty in ty_min..=ty_max {
        for tx in tx_min..=tx_max {
            f(tx, ty);
        }
    }
}

/// Clamp a floating-point tile coordinate to `[0, max_tile]` and floor to u32.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn clamp_tile(val: f64, max_tile: u32) -> u32 {
    let floored = val.floor();
    if floored < 0.0 {
        0
    } else if floored > f64::from(max_tile) {
        max_tile
    } else {
        floored as u32
    }
}

/// Buffer-reuse variant of `to_tile_coords` for hot paths (called per tile per feature).
/// Clears `buf` and writes tile pixel coordinates into it, avoiding a Vec alloc per call.
#[allow(clippy::cast_possible_truncation)]
pub fn to_tile_coords_into(
    buf: &mut Vec<(i32, i32)>,
    points: &[Point],
    tile_x: u32,
    tile_y: u32,
    zoom: u8,
) {
    buf.clear();
    let z_scale = f64::from(1u32 << zoom);
    let tx = f64::from(tile_x);
    let ty = f64::from(tile_y);
    buf.extend(points.iter().map(|p| {
        let px_x = (p.x * z_scale - tx) * EXTENT;
        let px_y = (p.y * z_scale - ty) * EXTENT;
        (px_x.round() as i32, px_y.round() as i32)
    }));
}

/// Compute a `MercBbox` bounding box from a slice of Mercator points.
pub(crate) fn merc_bbox(points: &[Point]) -> MercBbox {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in points {
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    MercBbox {
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

/// Close a tile-coordinate ring (if not already closed) and enforce clockwise winding (MVT outer).
pub(crate) fn close_and_orient_cw(ring: &mut Vec<(i32, i32)>) {
    if ring.first() != ring.last()
        && let Some(&first) = ring.first()
    {
        ring.push(first);
    }
    ensure_cw_tile(ring);
}

/// Close a tile-coordinate ring (if not already closed) and enforce counter-clockwise winding (MVT inner).
pub(crate) fn close_and_orient_ccw(ring: &mut Vec<(i32, i32)>) {
    if ring.first() != ring.last()
        && let Some(&first) = ring.first()
    {
        ring.push(first);
    }
    ensure_ccw_tile(ring);
}

/// Ensure a tile-coordinate ring is clockwise (for MVT outer rings).
/// In tile coordinates, Y increases downward, so positive signed area = CW.
fn ensure_cw_tile(ring: &mut [(i32, i32)]) {
    let area = signed_area_tile(ring);
    if area < 0.0 {
        ring.reverse();
    }
}

/// Ensure a tile-coordinate ring is counter-clockwise (for MVT inner rings).
fn ensure_ccw_tile(ring: &mut [(i32, i32)]) {
    let area = signed_area_tile(ring);
    if area > 0.0 {
        ring.reverse();
    }
}

/// Signed area of a ring in tile coordinates (Shoelace formula).
/// Positive = clockwise in screen coords (Y-down).
fn signed_area_tile(ring: &[(i32, i32)]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut sum: f64 = 0.0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        sum += f64::from(ring[i].0) * f64::from(ring[j].1);
        sum -= f64::from(ring[j].0) * f64::from(ring[i].1);
    }
    sum / 2.0
}
