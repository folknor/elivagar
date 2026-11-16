use super::projection::{MercBbox, Point};

/// Integer tile grid shared by geometry, encoders and decoder defaults.
pub(crate) mod grid {
    pub const EXTENT_LOG2: u32 = 12;
    pub const EXTENT: i32 = 1 << EXTENT_LOG2;
    pub const BUFFER_UNITS: i32 = 128;
}

/// Tile extent as a float for Mercator and line clipping arithmetic.
pub const EXTENT: f64 = grid::EXTENT as f64;

/// Buffer fraction of tile size for clipping (8 rendered pixels).
pub(crate) const BUFFER_FRACTION: f64 = grid::BUFFER_UNITS as f64 / EXTENT;

/// One rendered pixel in extent units.
pub(crate) use super::polygon::RENDER_PX;

/// Find all tiles at a given zoom level that intersect the given Mercator bbox.
///
/// Returns a list of `(tile_x, tile_y)` pairs.
#[cfg(test)]
pub fn tiles_for_bbox(bbox: &MercBbox, zoom: u8) -> Vec<(u32, u32)> {
    let mut tiles = Vec::new();
    for_each_tile_in_bbox(bbox, zoom, |tx, ty| tiles.push((tx, ty)));
    tiles
}

/// How far outside the unit world a bbox may lie and still be enumerated.
/// The projection clamps latitude to the Web Mercator limit, whose projected
/// y overshoots `[0, 1]` by about `7.1e-9`; a polar node or line sits in that
/// sliver and must still land on the edge tile. At z14 the slack is under
/// one MVT extent unit.
const WORLD_EDGE_SLACK: f64 = 1e-8;

/// The inclusive tile index range `(tx_min, tx_max, ty_min, ty_max)` a bbox
/// covers at `zoom`, or `None` when the bbox misses the world.
///
/// A bbox is admitted only if it meets `[-S, 1 + S]` on both axes (`S` is
/// `WORLD_EDGE_SLACK`), compared so that a NaN bound refuses. Admitted
/// indices are floored and clamped to the world edge. Refusing rather than
/// clamping is what keeps every emitted coordinate near its tile: an
/// antimeridian-unwrapped way can wind any number of world widths, the
/// emitters try only the copies shifted by one world either way, and a copy
/// lying wholly beyond the world used to clamp onto the edge tile and encode
/// far outside it, saturating the float-to-i32 casts at the extreme. Content
/// of such a way beyond the shifted copies is not emitted at all: wrapping is
/// bounded, not complete.
///
/// Premise, not checked here: the bbox is ordered and was folded from finite
/// coordinates (OSM input is fixed-point and label points are checked finite
/// before emission); a min/max fold can hide a NaN vertex.
#[inline]
fn tile_range(bbox: &MercBbox, zoom: u8) -> Option<(u32, u32, u32, u32)> {
    let lo = -WORLD_EDGE_SLACK;
    let hi = 1.0 + WORLD_EDGE_SLACK;
    let meets = bbox.max_x >= lo && bbox.min_x <= hi && bbox.max_y >= lo && bbox.min_y <= hi;
    if !meets {
        return None;
    }
    let z_scale = f64::from(1u32 << zoom);
    let max_tile = (1u32 << zoom).saturating_sub(1);
    Some((
        clamp_tile(bbox.min_x * z_scale, max_tile),
        clamp_tile(bbox.max_x * z_scale, max_tile),
        clamp_tile(bbox.min_y * z_scale, max_tile),
        clamp_tile(bbox.max_y * z_scale, max_tile),
    ))
}

/// Check whether a bbox falls entirely within a single tile at the given zoom.
///
/// When true, clipping is unnecessary - all geometry coordinates are within
/// the tile's non-buffered bounds (a subset of the buffered clip rect). The
/// floored indices are compared before any clamping, so a bbox reaching past
/// the world edge is never single-tile: it takes the clipped path, which
/// bounds its coordinates to the buffered rect.
#[inline]
pub fn is_single_tile(bbox: &MercBbox, zoom: u8) -> bool {
    let z_scale = f64::from(1u32 << zoom);
    let max_tile = f64::from((1u32 << zoom).saturating_sub(1));
    let x = (bbox.min_x * z_scale).floor();
    let y = (bbox.min_y * z_scale).floor();
    // Integral floats in tile-index range compare exactly.
    #[allow(clippy::float_cmp)]
    let same = x == (bbox.max_x * z_scale).floor() && y == (bbox.max_y * z_scale).floor();
    same && (0.0..=max_tile).contains(&x) && (0.0..=max_tile).contains(&y)
}

/// Iterate tiles for a bbox without allocating a Vec. A bbox that misses the
/// world yields nothing (see `tile_range`).
#[inline]
pub fn for_each_tile_in_bbox<F>(bbox: &MercBbox, zoom: u8, mut f: F)
where
    F: FnMut(u32, u32),
{
    let Some((tx_min, tx_max, ty_min, ty_max)) = tile_range(bbox, zoom) else {
        return;
    };
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

#[cfg(test)]
#[test]
fn float_and_integer_tile_buffers_agree() {
    assert_eq!(
        (EXTENT * BUFFER_FRACTION).to_bits(),
        f64::from(grid::BUFFER_UNITS).to_bits(),
        "derived float buffer must preserve the integer clip boundary",
    );
}
