use std::sync::atomic::{AtomicU8, Ordering};

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

    let mut tiles = Vec::with_capacity(
        ((tx_max - tx_min + 1) * (ty_max - ty_min + 1)) as usize,
    );
    for ty in ty_min..=ty_max {
        for tx in tx_min..=tx_max {
            tiles.push((tx, ty));
        }
    }
    tiles
}

/// Check whether a bbox falls entirely within a single tile at the given zoom.
///
/// When true, clipping is unnecessary — all geometry coordinates are within
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

/// Compute tile range for a Mercator bbox at a given zoom level.
/// Returns `(tx_min, tx_max, ty_min, ty_max)`.
#[inline]
pub fn tile_range_in_bbox(bbox: &MercBbox, zoom: u8) -> (u32, u32, u32, u32) {
    let z_scale = f64::from(1u32 << zoom);
    let max_tile = (1u32 << zoom).saturating_sub(1);
    (
        clamp_tile(bbox.min_x * z_scale, max_tile),
        clamp_tile(bbox.max_x * z_scale, max_tile),
        clamp_tile(bbox.min_y * z_scale, max_tile),
        clamp_tile(bbox.max_y * z_scale, max_tile),
    )
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

/// Convert a slice of Mercator points to tile pixel coordinates for MVT encoding.
#[allow(clippy::cast_possible_truncation)]
pub fn to_tile_coords(
    points: &[Point],
    tile_x: u32,
    tile_y: u32,
    zoom: u8,
) -> Vec<(i32, i32)> {
    let z_scale = f64::from(1u32 << zoom);
    let tx = f64::from(tile_x);
    let ty = f64::from(tile_y);
    points
        .iter()
        .map(|p| {
            let px_x = (p.x * z_scale - tx) * EXTENT;
            let px_y = (p.y * z_scale - ty) * EXTENT;
            (px_x.round() as i32, px_y.round() as i32)
        })
        .collect()
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
    MercBbox { min_x, min_y, max_x, max_y }
}

/// Close a tile-coordinate ring (if not already closed) and enforce clockwise winding (MVT outer).
pub(crate) fn close_and_orient_cw(ring: &mut Vec<(i32, i32)>) {
    if ring.first() != ring.last()
        && let Some(&first) = ring.first() {
            ring.push(first);
    }
    ensure_cw_tile(ring);
}

/// Close a tile-coordinate ring (if not already closed) and enforce counter-clockwise winding (MVT inner).
pub(crate) fn close_and_orient_ccw(ring: &mut Vec<(i32, i32)>) {
    if ring.first() != ring.last()
        && let Some(&first) = ring.first() {
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

// ---------------------------------------------------------------------------
// Land tile mask (z14 resolution bitset for ocean filtering)
// ---------------------------------------------------------------------------

/// Z14-resolution bitset recording which grid cells contain land features.
/// Thread-safe: uses atomic byte operations for concurrent writes from rayon.
/// 16384×16384 = 268M cells stored in 32 MB.
///
/// Ocean fill tiles are only generated for cells where the mask is set,
/// so higher resolution → fewer spurious ocean-only tiles.
pub(crate) struct LandMask {
    bits: Box<[AtomicU8]>,
}

impl LandMask {
    /// Zoom level of the mask grid.
    const ZOOM: u8 = 14;
    /// Grid dimension: 2^14 = 16384 tiles per axis.
    const DIM: usize = 1 << Self::ZOOM;
    /// Total bytes: 16384*16384/8 = 33,554,432 (32 MB).
    pub(super) const BYTES: usize = Self::DIM * Self::DIM / 8;

    /// Create an empty mask (no land anywhere). Heap-allocated (32 MB).
    pub fn new() -> Self {
        let mut bits = Vec::with_capacity(Self::BYTES);
        bits.resize_with(Self::BYTES, || AtomicU8::new(0));
        Self { bits: bits.into_boxed_slice() }
    }

    /// Mark all z14 cells covered by a Mercator bounding box.
    /// Hot path: called per-feature during PBF processing from rayon threads.
    pub fn mark_bbox(&self, bbox: &MercBbox) {
        #[allow(clippy::cast_possible_truncation)]
        let dim = Self::DIM as u32;
        let scale = dim as f64;
        let max_tile = dim - 1;
        let tx_min = clamp_tile(bbox.min_x * scale, max_tile);
        let tx_max = clamp_tile(bbox.max_x * scale, max_tile);
        let ty_min = clamp_tile(bbox.min_y * scale, max_tile);
        let ty_max = clamp_tile(bbox.max_y * scale, max_tile);
        for ty in ty_min..=ty_max {
            for tx in tx_min..=tx_max {
                self.set_bit(tx, ty);
            }
        }
    }

    /// Check whether a tile at any zoom level overlaps a z14 cell with land.
    /// For z ≥ 14: checks the single z14 cell (ancestor lookup).
    /// For z < 14: checks if ANY z14 descendant is set (early-exit scan).
    pub fn has_land(&self, z: u8, tx: u32, ty: u32) -> bool {
        if z >= Self::ZOOM {
            let shift = z - Self::ZOOM;
            self.get_bit(tx >> shift, ty >> shift)
        } else {
            let shift = Self::ZOOM - z;
            let x0 = tx << shift;
            let y0 = ty << shift;
            let count = 1u32 << shift;
            for dy in 0..count {
                for dx in 0..count {
                    if self.get_bit(x0 + dx, y0 + dy) {
                        return true;
                    }
                }
            }
            false
        }
    }

    #[inline]
    pub(super) fn set_bit(&self, tx: u32, ty: u32) {
        let idx = ty as usize * Self::DIM + tx as usize;
        let byte_idx = idx / 8;
        let bit_idx = idx % 8;
        self.bits[byte_idx].fetch_or(1 << bit_idx, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn get_bit(&self, tx: u32, ty: u32) -> bool {
        let idx = ty as usize * Self::DIM + tx as usize;
        let byte_idx = idx / 8;
        let bit_idx = idx % 8;
        (self.bits[byte_idx].load(Ordering::Relaxed) >> bit_idx) & 1 != 0
    }

    /// Serialize to 32 MB for checkpoint persistence.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.bits.iter().map(|b| b.load(Ordering::Relaxed)).collect()
    }

    /// Deserialize from bytes. Returns `None` if wrong length.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() != Self::BYTES {
            return None;
        }
        let bits: Vec<AtomicU8> = data.iter().map(|&b| AtomicU8::new(b)).collect();
        Some(Self { bits: bits.into_boxed_slice() })
    }

    /// Count how many z14 cells have land features.
    pub fn count_set(&self) -> u32 {
        self.bits
            .iter()
            .map(|b| b.load(Ordering::Relaxed).count_ones())
            .sum()
    }
}
