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

/// Remove quantization-induced backtrack spikes from a closed ring.
///
/// When dense f64 vertices are rounded to i32 tile coordinates, distinct points
/// can snap to the same integer, creating A→B→A sequences (spikes) and short
/// loops. These pass `ring_is_simple()` (which only detects proper crossings)
/// but break MapLibre's earcut tessellation.
///
/// Ported from tilemaker's `scaleRing()` (`coordinates_geom.cpp:36-52`):
/// each new point is checked against the previous `LOOKBACK` points. If a
/// match is found, the ring is truncated back to that point (killing the spike).
///
/// Must be called immediately after `to_tile_coords` / `to_tile_coords_into`,
/// before `close_and_orient` or any other processing.
pub(crate) fn dedup_quantized_ring(ring: &mut Vec<(i32, i32)>) {
    const LOOKBACK: usize = 5;
    if ring.len() < 4 {
        return;
    }
    // If the ring is closed (first == last), process only the interior vertices.
    // The closing vertex will be re-added by close_and_orient_cw/ccw.
    let closed = ring.first() == ring.last();
    let end = if closed { ring.len() - 1 } else { ring.len() };
    let mut write = 1usize; // always keep first point
    for read in 1..end {
        let p = ring[read];
        // Check against the last LOOKBACK written points (but never before index 1 -
        // never truncate back to remove the first vertex).
        let start = write.saturating_sub(LOOKBACK).max(1);
        let mut found = None;
        for j in (start..write).rev() {
            if ring[j] == p {
                found = Some(j);
                break;
            }
        }
        if let Some(j) = found {
            // Backtrack: truncate to the match point (kill the spike)
            write = j + 1;
        } else {
            ring[write] = p;
            write += 1;
        }
    }
    if closed && write > 0 {
        // Re-close the ring
        ring[write] = ring[0];
        write += 1;
    }
    ring.truncate(write);
}

/// Repair post-quantization polygon topology using i_overlay's integer simplify.
///
/// After quantization to i32 tile coordinates, rounding can introduce T-junctions
/// (vertex landing exactly on a non-adjacent edge) and collinear overlaps that
/// pass `ring_is_simple()` but break earcut tessellation. This function resolves
/// all self-intersections by running i_overlay's sweep-line noder on the quantized
/// integer coordinates.
///
/// Returns a vec of simple polygons (each: outer + holes). Returns empty vec if
/// the input is degenerate. Applied unconditionally after quantization - all three
/// competitors (Planetiler, Tilemaker, Tippecanoe) repair every polygon, not just
/// detected failures.
pub(crate) fn repair_quantized_polygon(
    outer: &[(i32, i32)],
    holes: &[Vec<(i32, i32)>],
) -> Vec<Vec<Vec<(i32, i32)>>> {
    use i_overlay::core::fill_rule::FillRule;
    use i_overlay::core::simplify::Simplify;
    use i_overlay::core::overlay::IntOverlayOptions;

    if outer.len() < 4 {
        return Vec::new();
    }

    // Build i_overlay shape: outer + holes as Vec<Vec<IntPoint>>
    let to_ip = |ring: &[(i32, i32)]| -> Vec<i_overlay::i_float::int::point::IntPoint> {
        // Strip closing vertex if present (i_overlay auto-closes)
        let n = if ring.len() >= 2 && ring.first() == ring.last() {
            ring.len() - 1
        } else {
            ring.len()
        };
        ring[..n].iter().map(|&(x, y)| i_overlay::i_float::int::point::IntPoint::new(x, y)).collect()
    };

    let mut shape: Vec<Vec<i_overlay::i_float::int::point::IntPoint>> = Vec::with_capacity(1 + holes.len());
    shape.push(to_ip(outer));
    for hole in holes {
        if hole.len() >= 4 {
            shape.push(to_ip(hole));
        }
    }

    // Simplify resolves self-intersections, T-junctions, and collinear overlaps.
    // Uses NonZero fill rule to match MapLibre's earcut winding semantics.
    let result = shape.simplify(FillRule::NonZero, IntOverlayOptions::default());

    // Convert back: Vec<IntShape> = Vec<Vec<Vec<IntPoint>>> → Vec<Vec<Vec<(i32,i32)>>>
    result.into_iter().map(|poly| {
        poly.into_iter().map(|ring| {
            let mut r: Vec<(i32, i32)> = ring.into_iter().map(|p| (p.x, p.y)).collect();
            // Re-close the ring
            if r.len() >= 3 {
                let first = r[0];
                r.push(first);
            }
            r
        }).collect()
    }).collect()
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
// Tile-space polygon ring simplification (post-quantization cleanup)
// ---------------------------------------------------------------------------
// After Mercator→tile coordinate projection, integer quantization creates
// 1-2 pixel staircase artifacts that Mercator-space DP can't see. This pass
// removes those zigzags using DP directly on tile coordinates.

/// Simplify a closed tile-coordinate polygon ring in-place using Douglas-Peucker.
///
/// `tol_sq` is the squared tolerance in extent units (e.g. 16²=256 for 1 rendered pixel).
/// Preserves first/last point (ring closure) and ensures the ring retains at least
/// `min_points` vertices (4 for a valid polygon ring). Winding order is preserved.
///
/// If simplification creates a self-intersecting ring (DP can collapse narrow
/// channels), falls back to the unsimplified ring. This prevents earcut
/// tessellation artifacts in MapLibre/WebGL renderers.
pub(crate) fn simplify_tile_ring(ring: &mut Vec<(i32, i32)>, tol_sq: i64, min_points: usize) {
    if tol_sq <= 0 { return; }
    // Need at least a triangle + closing point to simplify
    if ring.len() <= min_points {
        return;
    }
    let closed = ring.first() == ring.last();
    let n = if closed { ring.len() - 1 } else { ring.len() };
    if n <= min_points {
        return;
    }

    // Build keep-flags array
    let mut keep = vec![false; n];
    keep[0] = true;
    keep[n - 1] = true;
    tile_dp_recurse(ring, 0, n - 1, tol_sq, &mut keep);

    let kept: usize = keep.iter().filter(|&&k| k).count();
    if kept < min_points || kept == n {
        // Can't simplify enough, or nothing was removed - leave ring unchanged
        return;
    }

    // Save original in case simplification creates self-intersections
    let original = ring.clone();

    // Compact in-place
    let mut write = 0;
    for read in 0..n {
        if keep[read] {
            ring[write] = ring[read];
            write += 1;
        }
    }
    // Re-close if it was closed
    if closed {
        ring[write] = ring[0];
        write += 1;
    }
    ring.truncate(write);

    // Revert if simplification created self-intersections
    if !super::ring_is_simple(ring) {
        ring.clear();
        ring.extend_from_slice(&original);
    }
}

fn tile_dp_recurse(ring: &[(i32, i32)], start: usize, end: usize, tol_sq: i64, keep: &mut [bool]) {
    if end <= start + 1 {
        return;
    }
    let (max_idx, max_dist_sq) = tile_find_farthest(ring, start, end);
    if max_dist_sq > tol_sq {
        keep[max_idx] = true;
        tile_dp_recurse(ring, start, max_idx, tol_sq, keep);
        tile_dp_recurse(ring, max_idx, end, tol_sq, keep);
    }
}

/// Find the point farthest from the segment `ring[start]..ring[end]`, in integer tile coords.
/// Returns (index, squared_distance).
#[allow(clippy::needless_range_loop)]
fn tile_find_farthest(ring: &[(i32, i32)], start: usize, end: usize) -> (usize, i64) {
    let (ax, ay) = (i64::from(ring[start].0), i64::from(ring[start].1));
    let (bx, by) = (i64::from(ring[end].0), i64::from(ring[end].1));
    let dx = bx - ax;
    let dy = by - ay;
    let len_sq = dx * dx + dy * dy;

    let mut max_dist_sq: i64 = 0;
    let mut max_idx = start;

    for i in (start + 1)..end {
        let (px, py) = (i64::from(ring[i].0), i64::from(ring[i].1));
        let dist_sq = if len_sq == 0 {
            let ex = px - ax;
            let ey = py - ay;
            ex * ex + ey * ey
        } else {
            // Cross product squared / len_sq = perpendicular distance squared
            let cross = (px - ax) * dy - (py - ay) * dx;
            // Use full precision: cross² can be up to ~(4096*4096)² ≈ 2^48,
            // well within i64 range. Divide by len_sq for perp dist².
            cross * cross / len_sq
        };
        if dist_sq > max_dist_sq {
            max_dist_sq = dist_sq;
            max_idx = i;
        }
    }
    (max_idx, max_dist_sq)
}

// ---------------------------------------------------------------------------
// Earcut-safety ring cleanup (post-quantization quality pass)
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Hole containment validation (earcut-safety for multipolygons)
// ---------------------------------------------------------------------------
// After DP simplification + clipping + quantization, holes may no longer sit
// cleanly inside their outer ring. Earcut produces garbage triangles when a
// hole extends outside the outer. We validate each hole and drop invalid ones.

/// Minimum hole area in tile extent² units at low zoom.
/// Holes smaller than this are sub-pixel and invisible - dropping them avoids
/// earcut-hostile geometry for zero visual cost.
/// 4 pixels² = 4 × 16² = 1024 extent² units (using 2× signed area = 2048).
const MIN_HOLE_AREA_2X: i64 = 2048;

/// Check if a point is inside a closed polygon ring using ray casting.
/// The ring must be closed (first == last). Returns true if the point is
/// strictly inside (not on the boundary).
fn point_in_ring(px: i32, py: i32, ring: &[(i32, i32)]) -> bool {
    if ring.len() < 4 {
        return false;
    }
    let mut inside = false;
    let n = ring.len() - 1; // exclude closing vertex
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i].0, ring[i].1);
        let (xj, yj) = (ring[j].0, ring[j].1);
        // Ray cast: horizontal ray from (px, py) to +infinity
        if ((yi > py) != (yj > py))
            && (i64::from(px) < i64::from(xj - xi) * i64::from(py - yi) / i64::from(yj - yi) + i64::from(xi))
        {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Validate and filter hole rings for a multipolygon feature in tile coordinates.
///
/// Drops holes that:
/// - Have a representative vertex outside the outer ring
/// - Have area below the minimum threshold (sub-pixel at low zoom)
///
/// `all_rings[0..ring_count]` contains outer (index 0) + holes (indices 1..ring_count).
/// Returns the new ring_count after filtering.
#[allow(clippy::needless_range_loop)]
pub(crate) fn filter_holes_for_outer(
    all_rings: &mut [Vec<(i32, i32)>],
    ring_count: usize,
) -> usize {
    if ring_count <= 1 {
        return ring_count;
    }
    // Phase 1: decide which holes to keep (immutable borrow of outer)
    // Use a small inline bitset - ring_count is always small (< 64 in practice).
    let mut keep_mask: u64 = 1; // bit 0 = outer, always kept
    {
        let outer = &all_rings[0];
        for read in 1..ring_count.min(64) {
            let hole = &all_rings[read];
            if hole.len() < 4 {
                continue;
            }
            // Area check: drop sub-pixel holes
            if signed_area_2x(hole).abs() < MIN_HOLE_AREA_2X {
                continue;
            }
            // Containment check: representative point must be inside outer.
            let (px, py) = hole[0];
            if point_in_ring(px, py, outer) {
                keep_mask |= 1 << read;
            } else if hole.len() >= 2 {
                // First vertex on boundary - try midpoint of first edge
                let (qx, qy) = hole[1];
                if point_in_ring((px + qx) / 2, (py + qy) / 2, outer) {
                    keep_mask |= 1 << read;
                }
            }
        }
    }
    // Phase 2: compact kept rings (mutable, no outer borrow)
    let mut write = 1;
    for read in 1..ring_count.min(64) {
        if keep_mask & (1 << read) != 0 {
            if write != read {
                all_rings.swap(write, read);
            }
            write += 1;
        }
    }
    write
}

/// Signed area × 2 of a closed ring (shoelace, no division).
fn signed_area_2x(ring: &[(i32, i32)]) -> i64 {
    let mut area: i64 = 0;
    for i in 0..ring.len().saturating_sub(1) {
        area += i64::from(ring[i].0) * i64::from(ring[i + 1].1)
              - i64::from(ring[i + 1].0) * i64::from(ring[i].1);
    }
    area
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
