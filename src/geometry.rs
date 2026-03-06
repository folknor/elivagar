// Geometry engine: projection, simplification, clipping for vector tile generation.
//
// All operations work in Mercator [0,1] coordinate space unless stated otherwise.
// Pure Rust aside from smallvec for inline small-vec returns.

#[cfg(test)]
use smallvec::SmallVec;
use std::f64::consts::PI;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// Earth's equatorial circumference in meters.
const EARTH_CIRCUMFERENCE: f64 = 40_075_016.686;

/// Maximum latitude for Web Mercator (beyond this, projection diverges).
#[cfg(test)]
const MAX_LATITUDE: f64 = 85.051_129;

/// MVT tile extent (pixels per tile axis).
pub const EXTENT: f64 = 4096.0;

/// Simplification tolerance in rendered pixels (one tile = 256×256 px).
/// 1.0 = vertices within 1 pixel of the simplified line are removed.
/// Industry standard is 1.0; Tilemaker uses ~0.58 for comparison.
const SIMPLIFY_PIXELS: f64 = 1.0;

/// Buffer fraction of tile size for clipping (8 rendered pixels).
/// One tile = 256×256 rendered pixels; 8/256 of the tile width.
pub(crate) const BUFFER_FRACTION: f64 = 8.0 / 256.0;

/// One tile pixel in extent units: 4096 / 256 = 16.
#[allow(clippy::cast_possible_truncation)]
const PX: i64 = (EXTENT as i64) / 256;

/// Minimum line length squared in tile extent units (1 pixel = 16 extent units).
/// A line whose bounding box diagonal² is below this is sub-pixel and invisible.
pub const MIN_LINE_EXTENT_SQ: i64 = PX * PX; // 256

/// Minimum polygon area in tile extent units² (1 sq pixel).
/// Polygon rings with |area| below this contribute nothing visible.
pub const MIN_POLY_AREA: i64 = PX * PX; // 256

/// Check if a geometry's Mercator bounding box diagonal is sub-pixel at the
/// given zoom level. One pixel = 1 / (256 × 2^z) Mercator units. Used as a
/// pre-simplification early exit: if the entire geometry is sub-pixel, DP is
/// pointless and all coarser zooms can be skipped too.
pub(crate) fn merc_bbox_is_subpixel(points: &[Point], zoom: u8) -> bool {
    if points.len() < 2 {
        return true;
    }
    let (mut min_x, mut min_y) = (points[0].x, points[0].y);
    let (mut max_x, mut max_y) = (min_x, min_y);
    for p in &points[1..] {
        if p.x < min_x { min_x = p.x; }
        if p.x > max_x { max_x = p.x; }
        if p.y < min_y { min_y = p.y; }
        if p.y > max_y { max_y = p.y; }
    }
    let pixel = 1.0 / (256.0 * f64::from(1u32 << zoom));
    let dx = max_x - min_x;
    let dy = max_y - min_y;
    dx * dx + dy * dy < pixel * pixel
}

/// Check if a linestring's bounding box is sub-pixel (both dimensions < 1 px).
/// Returns true if the feature is too small and should be dropped.
pub fn line_is_subpixel(coords: &[(i32, i32)]) -> bool {
    if coords.len() < 2 {
        return true;
    }
    let (mut min_x, mut min_y) = coords[0];
    let (mut max_x, mut max_y) = (min_x, min_y);
    for &(x, y) in &coords[1..] {
        if x < min_x { min_x = x; }
        if x > max_x { max_x = x; }
        if y < min_y { min_y = y; }
        if y > max_y { max_y = y; }
    }
    let dx = i64::from(max_x - min_x);
    let dy = i64::from(max_y - min_y);
    dx * dx + dy * dy < MIN_LINE_EXTENT_SQ
}

/// Check if a polygon ring's area is sub-pixel. Uses the shoelace formula.
/// Returns true if the feature is too small and should be dropped.
pub fn ring_is_subpixel(coords: &[(i32, i32)]) -> bool {
    if coords.len() < 4 {
        return true;
    }
    let mut area: i64 = 0;
    let n = coords.len();
    for i in 0..n {
        let j = (i + 1) % n;
        area += i64::from(coords[i].0) * i64::from(coords[j].1);
        area -= i64::from(coords[j].0) * i64::from(coords[i].1);
    }
    area.abs() / 2 < MIN_POLY_AREA
}

// ---------------------------------------------------------------------------
// Point type
// ---------------------------------------------------------------------------

/// A 2D point in Mercator [0,1] space.
#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}
const _: () = assert!(std::mem::size_of::<Point>() == 16);

impl Point {
    #[inline]
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// Axis-aligned bounding rectangle in Mercator [0,1] space.
#[derive(Clone, Copy, Debug)]
pub struct MercBbox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

// ---------------------------------------------------------------------------
// Projection: WGS84 → Mercator [0,1]
// ---------------------------------------------------------------------------

// Latitude LUT for project_e7: 2^18 + 1 entries, ~2 MB. Linear interpolation
// gives ~0.00032° error = 0.03 pixels at z14 (imperceptible). Eliminates
// tan/cos/ln transcendentals (~250-400 cycles) from the hot path.
const LUT_BITS: u32 = 18;
const LUT_SIZE: usize = (1 << LUT_BITS) + 1; // 262_145
const LAT_E7_MIN: i64 = -850_511_290; // -MAX_LATITUDE in e7
const LAT_E7_MAX: i64 = 850_511_290; //  MAX_LATITUDE in e7
const LAT_E7_RANGE: f64 = (LAT_E7_MAX - LAT_E7_MIN) as f64;

static LAT_LUT: OnceLock<Box<[f64]>> = OnceLock::new();

fn init_lat_lut() -> Box<[f64]> {
    let mut table = vec![0.0f64; LUT_SIZE];
    let scale = 1.0 / (LUT_SIZE - 1) as f64;
    for (i, entry) in table.iter_mut().enumerate() {
        let lat_e7 = LAT_E7_MIN as f64 + (i as f64 * scale) * LAT_E7_RANGE;
        let lat_rad = lat_e7 * 1e-7 * PI / 180.0;
        *entry = 0.5 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / (2.0 * PI);
    }
    table.into_boxed_slice()
}

/// Project a single WGS84 coordinate (lat_deg, lon_deg) to Mercator [0,1].
/// Uses exact transcendentals — for tests and one-off calls. Hot path uses
/// `project_e7` which goes through the LUT.
#[cfg(test)]
#[inline]
pub fn project(lat_deg: f64, lon_deg: f64) -> Point {
    let lat_clamped = lat_deg.clamp(-MAX_LATITUDE, MAX_LATITUDE);
    let x = (lon_deg + 180.0) / 360.0;
    let lat_rad = lat_clamped * PI / 180.0;
    let y = 0.5 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / (2.0 * PI);
    Point::new(x, y)
}

/// Project from fixed-point e7 integers to Mercator [0,1] via LUT.
/// ~3-4 cycles (table lookup + lerp) vs ~250-400 cycles (transcendentals).
#[inline]
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
pub fn project_e7(lat_e7: i32, lon_e7: i32) -> Point {
    let lut = LAT_LUT.get_or_init(init_lat_lut);
    let x = (f64::from(lon_e7) * 1e-7 + 180.0) / 360.0;

    let lat = i64::from(lat_e7).clamp(LAT_E7_MIN, LAT_E7_MAX);
    let frac = (lat - LAT_E7_MIN) as f64 / LAT_E7_RANGE;
    let idx_f = frac * (LUT_SIZE - 1) as f64;
    let idx = idx_f as usize;
    let t = idx_f - idx as f64;

    let y = if idx + 1 < LUT_SIZE {
        lut[idx] + t * (lut[idx + 1] - lut[idx])
    } else {
        lut[idx]
    };
    Point::new(x, y)
}

/// Convert EPSG:3857 (Web Mercator meters) to Mercator [0,1] coordinates.
#[inline]
pub fn from_epsg3857(x: f64, y: f64) -> Point {
    let half_c = EARTH_CIRCUMFERENCE / 2.0;
    Point::new(
        (x + half_c) / EARTH_CIRCUMFERENCE,
        (half_c - y) / EARTH_CIRCUMFERENCE,
    )
}

/// Inverse projection: Mercator y → latitude in degrees.
#[cfg(test)]
#[inline]
pub fn merc_y_to_lat(y: f64) -> f64 {
    let lat_rad = (PI * (1.0 - 2.0 * y)).sinh().atan();
    lat_rad * 180.0 / PI
}

// ---------------------------------------------------------------------------
// Tile coordinate conversion
// ---------------------------------------------------------------------------

/// Convert a Mercator [0,1] point to tile-local pixel coordinates.
///
/// Returns `(px_x, px_y)` as i32 suitable for MVT command encoding.
#[allow(clippy::cast_possible_truncation)]
pub fn merc_to_tile_px(p: &Point, tile_x: u32, tile_y: u32, zoom: u8) -> (i32, i32) {
    let z_scale = f64::from(1u32 << zoom);
    let px_x = (p.x * z_scale - f64::from(tile_x)) * EXTENT;
    let px_y = (p.y * z_scale - f64::from(tile_y)) * EXTENT;
    (px_x.round() as i32, px_y.round() as i32)
}

/// Compute the simplification tolerance for a given zoom level.
/// Returns the tolerance in Mercator [0,1] units corresponding to
/// `SIMPLIFY_PIXELS` rendered pixels at the given zoom.
#[inline]
pub fn simplify_tolerance(zoom: u8) -> f64 {
    let z_scale = f64::from(1u32 << zoom);
    SIMPLIFY_PIXELS / (256.0 * z_scale)
}

// ---------------------------------------------------------------------------
// Douglas-Peucker simplification
// ---------------------------------------------------------------------------

/// Simplify a polyline using the Douglas-Peucker algorithm, reusing caller-owned buffers.
///
/// Points whose perpendicular distance to the line segment between endpoints
/// is less than `tolerance` are removed. Result is left in `output`.
/// `keep_buf` is a reusable scratch buffer for the keep-flags array.
///
/// Returns the maximum deviation (squared) of any removed point. If this is less
/// than the next zoom's tolerance², the cascade has converged and further DP calls
/// can be skipped.
pub fn simplify_into(
    points: &[Point],
    tolerance: f64,
    keep_buf: &mut Vec<bool>,
    output: &mut Vec<Point>,
) -> f64 {
    output.clear();
    if points.len() <= 2 {
        output.extend_from_slice(points);
        return 0.0;
    }
    keep_buf.clear();
    keep_buf.resize(points.len(), false);
    keep_buf[0] = true;
    keep_buf[points.len() - 1] = true;
    let max_dev_sq = dp_recurse(points, 0, points.len() - 1, tolerance * tolerance, keep_buf);
    for (i, &k) in keep_buf.iter().enumerate() {
        if k {
            output.push(points[i]);
        }
    }
    max_dev_sq
}

/// Simplify with required vertex indices that must survive simplification.
///
/// `required_indices` are indices into `points`; out-of-range indices are ignored.
/// Endpoints are always preserved regardless of `required_indices`.
pub fn simplify_into_with_required(
    points: &[Point],
    tolerance: f64,
    required_indices: &[usize],
    keep_buf: &mut Vec<bool>,
    output: &mut Vec<Point>,
) -> f64 {
    output.clear();
    if points.len() <= 2 {
        output.extend_from_slice(points);
        return 0.0;
    }
    keep_buf.clear();
    keep_buf.resize(points.len(), false);
    keep_buf[0] = true;
    keep_buf[points.len() - 1] = true;
    for &idx in required_indices {
        if idx < points.len() {
            keep_buf[idx] = true;
        }
    }

    let max_dev_sq = dp_recurse_with_required(
        points,
        0,
        points.len() - 1,
        tolerance * tolerance,
        keep_buf,
    );
    for (i, &k) in keep_buf.iter().enumerate() {
        if k {
            output.push(points[i]);
        }
    }
    max_dev_sq
}

/// Convenience wrapper that allocates its own buffers. Use [`simplify_into`] in hot paths.
#[cfg(test)]
pub fn simplify(points: &[Point], tolerance: f64) -> Vec<Point> {
    let mut keep = Vec::new();
    let mut output = Vec::new();
    simplify_into(points, tolerance, &mut keep, &mut output);
    output
}

/// Recursive step of Douglas-Peucker. Uses squared tolerance to avoid sqrt.
/// Returns the maximum squared deviation found across all removed points.
fn dp_recurse(points: &[Point], start: usize, end: usize, tol_sq: f64, keep: &mut [bool]) -> f64 {
    if end <= start + 1 {
        return 0.0;
    }
    let (max_idx, max_dist_sq) = find_farthest(points, start, end);
    if max_dist_sq > tol_sq {
        keep[max_idx] = true;
        let left = dp_recurse(points, start, max_idx, tol_sq, keep);
        let right = dp_recurse(points, max_idx, end, tol_sq, keep);
        left.max(right)
    } else {
        // All points in this segment are within tolerance — max_dist_sq is
        // the largest deviation among them.
        max_dist_sq
    }
}

fn dp_recurse_with_required(
    points: &[Point],
    start: usize,
    end: usize,
    tol_sq: f64,
    keep: &mut [bool],
) -> f64 {
    if end <= start + 1 {
        return 0.0;
    }

    if let Some(split_idx) = (start + 1..end).find(|&i| keep[i]) {
        let left = dp_recurse_with_required(points, start, split_idx, tol_sq, keep);
        let right = dp_recurse_with_required(points, split_idx, end, tol_sq, keep);
        return left.max(right);
    }

    let (max_idx, max_dist_sq) = find_farthest(points, start, end);
    if max_dist_sq > tol_sq {
        keep[max_idx] = true;
        let left = dp_recurse_with_required(points, start, max_idx, tol_sq, keep);
        let right = dp_recurse_with_required(points, max_idx, end, tol_sq, keep);
        left.max(right)
    } else {
        max_dist_sq
    }
}

/// Find the point farthest from the line segment `points[start]..points[end]`.
///
/// Must scan all intermediate points — DP correctness requires splitting at the
/// true maximum, not just any above-tolerance point. Early termination was
/// investigated and rejected: there's no usable upper bound to prune against,
/// and the loop body (one `perp_dist_sq` + compare per point) is already minimal.
/// The real DP cost driver is the number of recursive calls, addressed by the
/// cascade-level optimizations (subpixel skip, convergence skip, vertex pre-check).
fn find_farthest(points: &[Point], start: usize, end: usize) -> (usize, f64) {
    let a = points[start];
    let b = points[end];
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let len_sq = dx * dx + dy * dy;

    let mut max_dist_sq: f64 = 0.0;
    let mut max_idx = start;

    for (i, pt) in points.iter().enumerate().take(end).skip(start + 1) {
        let dist_sq = perp_dist_sq(pt, &a, dx, dy, len_sq);
        if dist_sq > max_dist_sq {
            max_dist_sq = dist_sq;
            max_idx = i;
        }
    }
    (max_idx, max_dist_sq)
}

/// Squared perpendicular distance from `p` to the line through `a` with direction `(dx, dy)`.
#[inline]
fn perp_dist_sq(p: &Point, a: &Point, dx: f64, dy: f64, len_sq: f64) -> f64 {
    if len_sq < 1e-30 {
        // Degenerate segment: distance to point a
        let ex = p.x - a.x;
        let ey = p.y - a.y;
        return ex * ex + ey * ey;
    }
    let t = ((p.x - a.x) * dx + (p.y - a.y) * dy) / len_sq;
    let t_clamped = t.clamp(0.0, 1.0);
    let proj_x = a.x + t_clamped * dx;
    let proj_y = a.y + t_clamped * dy;
    let ex = p.x - proj_x;
    let ey = p.y - proj_y;
    ex * ex + ey * ey
}

// ---------------------------------------------------------------------------
// Cascading simplification helpers
// ---------------------------------------------------------------------------
// These deduplicate the zoom-descending simplification loop used in
// emit_line_feature, emit_polygon_feature, emit_multipolygon_feature,
// and emit_ocean_polygon. Douglas-Peucker simplification is hierarchical,
// so each zoom's result is always a subset of the previous zoom's.

/// Scratch buffers for [`for_each_zoom_simplified`], reused via thread-local storage.
/// Buffers grow to accommodate the largest geometry seen by each thread and stay allocated.
struct SimplifySingleScratch {
    cascade: Vec<Point>,
    keep_buf: Vec<bool>,
    simp_buf: Vec<Point>,
}

thread_local! {
    static SIMPLIFY_SINGLE_SCRATCH: std::cell::RefCell<SimplifySingleScratch> =
        const { std::cell::RefCell::new(SimplifySingleScratch {
            cascade: Vec::new(),
            keep_buf: Vec::new(),
            simp_buf: Vec::new(),
        }) };
}

/// Cascading simplification for a single geometry (line or polygon ring).
///
/// Iterates from `z_hi` down to `z_lo`, simplifying the geometry at each zoom
/// using the previous zoom's result. Calls `callback(z, &simplified)` at each
/// zoom level. Stops early if the simplified geometry drops below `min_points`.
/// Uses thread-local scratch buffers to avoid per-call allocation.
#[hotpath::measure]
pub fn for_each_zoom_simplified<F>(
    merc: &[Point],
    z_lo: u8,
    z_hi: u8,
    min_points: usize,
    mut callback: F,
) where
    F: FnMut(u8, &[Point]),
{
    SIMPLIFY_SINGLE_SCRATCH.with(|cell| {
    let scratch = &mut *cell.borrow_mut();
    let SimplifySingleScratch { cascade, keep_buf, simp_buf } = scratch;
    cascade.clear();
    cascade.extend_from_slice(merc);
    // Track max deviation² from last DP run for cascade convergence check.
    let mut last_max_dev_sq: f64 = f64::MAX;
    for z in (z_lo..=z_hi).rev() {
        if z < 14 {
            // Pre-DP subpixel check: if the cascade's bbox diagonal is < 1 pixel
            // at this zoom, the feature is invisible here and at all coarser zooms.
            // Skips DP entirely — O(1) vs O(n²).
            if merc_bbox_is_subpixel(cascade, z) {
                break;
            }
            // Option E: if cascade already has ≤ min_points vertices, DP can't
            // reduce further — skip the call entirely.
            if cascade.len() > min_points {
                let tol = simplify_tolerance(z);
                // Option D: if last DP's max deviation is already below this
                // zoom's tolerance, the cascade is optimal — skip DP.
                if last_max_dev_sq >= tol * tol {
                    last_max_dev_sq = simplify_into(cascade, tol, keep_buf, simp_buf);
                    std::mem::swap(cascade, simp_buf);
                }
            }
        }
        if cascade.len() < min_points {
            break;
        }
        callback(z, cascade);
    }
    }); // SIMPLIFY_SINGLE_SCRATCH.with
}

/// Reusable scratch buffers for [`for_each_zoom_simplified_multi`].
/// Hoist outside tight loops to avoid per-call allocation of cascade/simplification
/// buffers. Buffers grow to accommodate the largest polygon and stay allocated.
pub struct SimplifyMultiScratch {
    pub cascade_outer: Vec<Point>,
    pub cascade_inners: Vec<Vec<Point>>,
    pub keep_buf: Vec<bool>,
    pub simp_buf: Vec<Point>,
    pub inner_max_dev_sq: Vec<f64>,
}

impl SimplifyMultiScratch {
    pub fn new() -> Self {
        Self {
            cascade_outer: Vec::new(),
            cascade_inners: Vec::new(),
            keep_buf: Vec::new(),
            simp_buf: Vec::new(),
            inner_max_dev_sq: Vec::new(),
        }
    }
}

/// Cascading simplification for a multipolygon (outer ring + inner holes).
///
/// Same zoom-descending approach as [`for_each_zoom_simplified`], but also
/// simplifies inner rings and drops any that fall below 4 points.
/// Pass a [`SimplifyMultiScratch`] to reuse buffers across calls.
#[hotpath::measure]
pub fn for_each_zoom_simplified_multi<F>(
    outer: &[Point],
    inners: &[Vec<Point>],
    z_lo: u8,
    z_hi: u8,
    scratch: &mut SimplifyMultiScratch,
    mut callback: F,
) where
    F: FnMut(u8, &[Point], &[Vec<Point>]),
{
    // Destructure so the borrow checker sees independent fields
    // (needed for retain_mut closure to borrow keep_buf/simp_buf
    // while cascade_inners is mutably borrowed).
    let SimplifyMultiScratch {
        cascade_outer,
        cascade_inners,
        keep_buf,
        simp_buf,
        inner_max_dev_sq,
    } = scratch;

    cascade_outer.clear();
    cascade_outer.extend_from_slice(outer);

    // Reuse inner vecs where possible, growing the pool as needed
    for (i, inner) in inners.iter().enumerate() {
        if i < cascade_inners.len() {
            cascade_inners[i].clear();
            cascade_inners[i].extend_from_slice(inner);
        } else {
            cascade_inners.push(inner.clone());
        }
    }
    cascade_inners.truncate(inners.len());

    // Per-inner convergence tracking: skip simplify_into when max deviation
    // is already below tolerance (same optimization as outer ring at line 461).
    inner_max_dev_sq.clear();
    inner_max_dev_sq.resize(inners.len(), f64::MAX);

    let mut last_max_dev_sq: f64 = f64::MAX;
    for z in (z_lo..=z_hi).rev() {
        let tol = if z < 14 { simplify_tolerance(z) } else { 0.0 };
        if tol > 0.0 {
            if merc_bbox_is_subpixel(cascade_outer, z) {
                break;
            }
            let tol_sq = tol * tol;
            // Option E: skip if outer already at minimum, Option D: skip if converged
            if cascade_outer.len() > 4 && last_max_dev_sq >= tol_sq {
                last_max_dev_sq = simplify_into(cascade_outer, tol, keep_buf, simp_buf);
                std::mem::swap(cascade_outer, simp_buf);
                let mut write = 0;
                for read in 0..cascade_inners.len() {
                    if inner_max_dev_sq[read] >= tol_sq {
                        inner_max_dev_sq[read] = simplify_into(&cascade_inners[read], tol, keep_buf, simp_buf);
                        std::mem::swap(&mut cascade_inners[read], simp_buf);
                    }
                    if cascade_inners[read].len() >= 4 {
                        if write != read {
                            cascade_inners.swap(write, read);
                            inner_max_dev_sq.swap(write, read);
                        }
                        write += 1;
                    }
                }
                cascade_inners.truncate(write);
                inner_max_dev_sq.truncate(write);
            }
        }
        if cascade_outer.len() < 4 {
            break;
        }
        callback(z, cascade_outer, cascade_inners);
    }
}

// ---------------------------------------------------------------------------
// Cohen-Sutherland line clipping
// ---------------------------------------------------------------------------

/// Region outcodes for Cohen-Sutherland.
const INSIDE: u8 = 0b0000;
const LEFT: u8 = 0b0001;
const RIGHT: u8 = 0b0010;
const BOTTOM: u8 = 0b0100;
const TOP: u8 = 0b1000;

/// Compute the outcode for a point relative to a clipping rectangle.
#[inline]
fn outcode(p: &Point, rect: &ClipRect) -> u8 {
    let mut code = INSIDE;
    if p.x < rect.min_x {
        code |= LEFT;
    } else if p.x > rect.max_x {
        code |= RIGHT;
    }
    if p.y < rect.min_y {
        code |= BOTTOM;
    } else if p.y > rect.max_y {
        code |= TOP;
    }
    code
}

/// Axis-aligned clipping rectangle.
#[derive(Clone, Copy, Debug)]
pub struct ClipRect {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl ClipRect {
    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self { min_x, min_y, max_x, max_y }
    }

    /// Create a clip rect for a tile with optional buffer (in fraction of tile size).
    pub fn for_tile(tile_x: u32, tile_y: u32, zoom: u8, buffer_fraction: f64) -> Self {
        let z_scale = f64::from(1u32 << zoom);
        let inv_z = 1.0 / z_scale;
        let x0 = f64::from(tile_x) * inv_z;
        let y0 = f64::from(tile_y) * inv_z;
        let buf = buffer_fraction * inv_z;
        Self::new(x0 - buf, y0 - buf, x0 + inv_z + buf, y0 + inv_z + buf)
    }
}

/// O(1) AABB intersection test: does a Mercator bbox overlap a clip rect?
pub fn bbox_intersects_clip(bbox: &MercBbox, clip: &ClipRect) -> bool {
    bbox.max_x >= clip.min_x
        && bbox.min_x <= clip.max_x
        && bbox.max_y >= clip.min_y
        && bbox.min_y <= clip.max_y
}

/// Clip a linestring to an axis-aligned rectangle using Cohen-Sutherland.
///
/// Returns zero or more sub-linestrings (the line may enter and exit multiple times).
/// SmallVec<[_; 1]>: most clips produce exactly one segment, avoiding the outer heap alloc.
#[cfg(test)]
pub fn clip_linestring(line: &[Point], rect: &ClipRect) -> SmallVec<[Vec<Point>; 1]> {
    let mut result: SmallVec<[Vec<Point>; 1]> = SmallVec::new();
    for_each_clipped_segment(line, rect, |segment| {
        result.push(segment.to_vec());
    });
    result
}

thread_local! {
    static CLIP_LINE_SCRATCH: std::cell::RefCell<Vec<Point>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Clip a linestring and call `callback` for each visible sub-segment.
///
/// Uses a thread-local scratch buffer to avoid per-call allocation. The callback
/// receives a borrowed slice of the segment points.
#[hotpath::measure]
pub fn for_each_clipped_segment<F>(
    line: &[Point],
    rect: &ClipRect,
    mut callback: F,
) where
    F: FnMut(&[Point]),
{
    if line.len() < 2 {
        return;
    }
    // Outcode pre-test: reject entire linestring if all vertices are outside the same edge.
    let mut and_code = 0xFFu8;
    for p in line {
        and_code &= outcode(p, rect);
        if and_code == INSIDE {
            break;
        }
    }
    if and_code != INSIDE {
        return;
    }
    CLIP_LINE_SCRATCH.with(|cell| {
    let current = &mut *cell.borrow_mut();
    current.clear();

    for i in 0..(line.len() - 1) {
        if let Some((a, b)) = clip_segment(line[i], line[i + 1], rect) {
            let enters_from_outside = !points_near(&a, &line[i]);
            let exits_to_outside = !points_near(&b, &line[i + 1]);

            if enters_from_outside {
                if current.len() >= 2 { callback(current); }
                current.clear();
                current.push(a);
            } else if current.is_empty() {
                current.push(a);
            }
            current.push(b);

            if exits_to_outside {
                if current.len() >= 2 { callback(current); }
                current.clear();
            }
        } else {
            if current.len() >= 2 { callback(current); }
            current.clear();
        }
    }

    if current.len() >= 2 { callback(current); }
    }); // CLIP_LINE_SCRATCH.with
}

/// Test whether two points are essentially the same (within floating-point tolerance).
#[inline]
fn points_near(a: &Point, b: &Point) -> bool {
    (a.x - b.x).abs() < 1e-12 && (a.y - b.y).abs() < 1e-12
}

/// Clip a single line segment to the rectangle. Returns `None` if fully outside.
fn clip_segment(mut p0: Point, mut p1: Point, rect: &ClipRect) -> Option<(Point, Point)> {
    let mut code0 = outcode(&p0, rect);
    let mut code1 = outcode(&p1, rect);

    loop {
        if (code0 | code1) == INSIDE {
            return Some((p0, p1));
        }
        if (code0 & code1) != INSIDE {
            return None;
        }

        let code_out = if code0 != INSIDE { code0 } else { code1 };
        let clipped = intersect_edge(&p0, &p1, rect, code_out);

        if code_out == code0 {
            p0 = clipped;
            code0 = outcode(&p0, rect);
        } else {
            p1 = clipped;
            code1 = outcode(&p1, rect);
        }
    }
}

/// Find the intersection of the line p0→p1 with the rectangle edge indicated by `code`.
fn intersect_edge(p0: &Point, p1: &Point, rect: &ClipRect, code: u8) -> Point {
    let dx = p1.x - p0.x;
    let dy = p1.y - p0.y;

    if (code & TOP) != 0 {
        let t = (rect.max_y - p0.y) / dy;
        Point::new(p0.x + t * dx, rect.max_y)
    } else if (code & BOTTOM) != 0 {
        let t = (rect.min_y - p0.y) / dy;
        Point::new(p0.x + t * dx, rect.min_y)
    } else if (code & RIGHT) != 0 {
        let t = (rect.max_x - p0.x) / dx;
        Point::new(rect.max_x, p0.y + t * dy)
    } else {
        // LEFT
        let t = (rect.min_x - p0.x) / dx;
        Point::new(rect.min_x, p0.y + t * dy)
    }
}

// ---------------------------------------------------------------------------
// Sutherland-Hodgman polygon clipping
// ---------------------------------------------------------------------------

/// Clip a polygon ring to an axis-aligned rectangle using Sutherland-Hodgman.
///
/// The input ring should NOT have a duplicated closing vertex.
/// Result is left in `buf_a` after the call (4 edge swaps = even = back to original).
/// Callers should hoist `buf_a`/`buf_b` outside inner loops to avoid per-call allocation.
#[hotpath::measure]
pub fn clip_polygon_into(
    ring: &[Point],
    rect: &ClipRect,
    buf_a: &mut Vec<Point>,
    buf_b: &mut Vec<Point>,
) {
    buf_a.clear();
    buf_b.clear();
    if ring.is_empty() {
        return;
    }
    // Outcode pre-test: if all vertices share a common outside bit (all left,
    // all right, etc.), the polygon is entirely outside one edge — skip S-H.
    // One O(n) pass with 4 comparisons per vertex vs S-H's 4×O(n) with
    // intersection math.
    let mut and_code = 0xFFu8;
    for p in ring {
        and_code &= outcode(p, rect);
        if and_code == INSIDE {
            break; // Can't reject early, must run S-H
        }
    }
    if and_code != INSIDE {
        return; // All vertices outside the same edge — guaranteed empty
    }
    buf_a.extend_from_slice(ring);
    for edge in [
        Edge::Left(rect.min_x),
        Edge::Right(rect.max_x),
        Edge::Bottom(rect.min_y),
        Edge::Top(rect.max_y),
    ] {
        clip_polygon_edge_into(buf_a, edge, buf_b);
        std::mem::swap(buf_a, buf_b);
        buf_b.clear();
    }
    // Result is in buf_a
}

/// Convenience wrapper that allocates its own buffers. Use [`clip_polygon_into`] in hot paths.
pub fn clip_polygon(ring: &[Point], rect: &ClipRect) -> Vec<Point> {
    let mut buf_a = Vec::new();
    let mut buf_b = Vec::new();
    clip_polygon_into(ring, rect, &mut buf_a, &mut buf_b);
    buf_a
}

/// Which rectangle edge we are clipping against, and its coordinate value.
#[derive(Clone, Copy)]
enum Edge {
    Left(f64),
    Right(f64),
    Bottom(f64),
    Top(f64),
}

/// Is the point on the "inside" side of this edge?
#[inline]
fn is_inside(p: &Point, edge: Edge) -> bool {
    match edge {
        Edge::Left(x) => p.x >= x,
        Edge::Right(x) => p.x <= x,
        Edge::Bottom(y) => p.y >= y,
        Edge::Top(y) => p.y <= y,
    }
}

/// Intersect segment s→e with the clipping edge.
///
/// Division by dx (Left/Right) or dy (Top/Bottom) is safe: the caller only
/// invokes this when one point is inside and the other outside the edge,
/// which guarantees the relevant denominator is nonzero.
#[inline]
fn edge_intersect(s: &Point, e: &Point, edge: Edge) -> Point {
    let dx = e.x - s.x;
    let dy = e.y - s.y;
    match edge {
        Edge::Left(x) | Edge::Right(x) => {
            let t = (x - s.x) / dx;
            Point::new(x, s.y + t * dy)
        }
        Edge::Bottom(y) | Edge::Top(y) => {
            let t = (y - s.y) / dy;
            Point::new(s.x + t * dx, y)
        }
    }
}

/// Clip a polygon against a single edge, appending results to `output`.
fn clip_polygon_edge_into(polygon: &[Point], edge: Edge, output: &mut Vec<Point>) {
    if polygon.is_empty() {
        return;
    }
    let mut s = polygon[polygon.len() - 1];
    for &e in polygon {
        let e_inside = is_inside(&e, edge);
        let s_inside = is_inside(&s, edge);
        if e_inside {
            if !s_inside {
                output.push(edge_intersect(&s, &e, edge));
            }
            output.push(e);
        } else if s_inside {
            output.push(edge_intersect(&s, &e, edge));
        }
        s = e;
    }
}

// ---------------------------------------------------------------------------
// Ring orientation (Shoelace formula)
// ---------------------------------------------------------------------------

/// Compute the signed area of a ring using the Shoelace formula.
///
/// Positive = counter-clockwise, negative = clockwise.
/// The ring should NOT have a duplicated closing vertex (we wrap automatically).
pub fn signed_area(ring: &[Point]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut sum = 0.0;
    let n = ring.len();
    for i in 0..n {
        let j = (i + 1) % n;
        sum += ring[i].x * ring[j].y;
        sum -= ring[j].x * ring[i].y;
    }
    sum * 0.5
}

/// Returns `true` if the ring is wound counter-clockwise.
#[cfg(test)]
#[inline]
pub fn is_ccw(ring: &[Point]) -> bool {
    signed_area(ring) > 0.0
}

/// Returns `true` if the ring is wound clockwise.
#[cfg(test)]
#[inline]
pub fn is_cw(ring: &[Point]) -> bool {
    signed_area(ring) < 0.0
}

/// Reverse a ring's winding order in place.
#[cfg(test)]
pub fn reverse_ring(ring: &mut [Point]) {
    ring.reverse();
}

// ---------------------------------------------------------------------------
// Area calculation in square meters (approximate)
// ---------------------------------------------------------------------------

/// Area of a ring in square meters using per-edge latitude correction.
///
/// The ring is in Mercator [0,1] space. Mercator is conformal, so local scale
/// at latitude φ is C·cos(φ) in both x and y. The real-world area element is
/// `C² · cos²(φ(y)) · dx dy`. Via Green's theorem this becomes the line
/// integral `∮ x · cos²(φ(y)) dy`, discretized per edge with midpoint rule.
///
/// cos²(φ) is computed directly from Mercator y: cos²(lat(y)) = sech²(π(1-2y)).
/// One cosh() call per edge, O(n). Not on the hotpath.
pub fn area_sq_meters(ring: &[Point]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let n = ring.len();
    let c_sq = EARTH_CIRCUMFERENCE * EARTH_CIRCUMFERENCE;
    let mut sum = 0.0;
    for i in 0..n {
        let j = (i + 1) % n;
        let dy = ring[j].y - ring[i].y;
        let x_sum = ring[i].x + ring[j].x;
        let y_mid = (ring[i].y + ring[j].y) * 0.5;
        // cos²(lat(y)) = sech²(π(1 - 2y)) = 1/cosh²(π(1 - 2y))
        let arg = PI * (1.0 - 2.0 * y_mid);
        let cosh_val = arg.cosh();
        let cos_sq_lat = 1.0 / (cosh_val * cosh_val);
        sum += x_sum * dy * cos_sq_lat;
    }
    (sum.abs() * 0.5) * c_sq
}

// ---------------------------------------------------------------------------
// Point on surface (for polygon label placement)
// ---------------------------------------------------------------------------

/// Find a point guaranteed to be inside the polygon, suitable for label placement.
///
/// Scans horizontal lines through the polygon bbox, finds the longest interior
/// segment, and returns its midpoint.
pub fn point_on_surface(ring: &[Point]) -> Option<Point> {
    if ring.len() < 3 {
        return None;
    }

    let (bbox_min, bbox_max) = ring_bbox(ring);
    let height = bbox_max.y - bbox_min.y;

    if height < 1e-15 {
        return None;
    }

    let num_scans = 5;
    let mut best_point = None;
    let mut best_length: f64 = 0.0;

    for i in 1..=num_scans {
        let frac = f64::from(i) / f64::from(num_scans + 1);
        let scan_y = bbox_min.y + frac * height;
        scan_for_longest_segment(ring, scan_y, &mut best_point, &mut best_length);
    }

    best_point
}

/// Find a point inside `outer` but outside all `inners`, suitable for
/// multipolygon label placement.
pub fn point_on_surface_with_holes(outer: &[Point], inners: &[Vec<Point>]) -> Option<Point> {
    if inners.is_empty() {
        return point_on_surface(outer);
    }
    if outer.len() < 3 {
        return None;
    }

    let (bbox_min, bbox_max) = ring_bbox(outer);
    let height = bbox_max.y - bbox_min.y;
    if height < 1e-15 {
        return None;
    }

    let num_scans = 7;
    let mut best_point = None;
    let mut best_length: f64 = 0.0;

    for i in 1..=num_scans {
        let frac = f64::from(i) / f64::from(num_scans + 1);
        let scan_y = bbox_min.y + frac * height;
        scan_for_longest_segment_with_holes(
            outer,
            inners,
            scan_y,
            &mut best_point,
            &mut best_length,
        );
    }

    best_point.or_else(|| {
        let p = point_on_surface(outer)?;
        if inners.iter().any(|hole| point_in_polygon(&p, hole)) {
            None
        } else {
            Some(p)
        }
    })
}

/// Compute the axis-aligned bounding box of a ring.
fn ring_bbox(ring: &[Point]) -> (Point, Point) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in ring {
        if p.x < min_x { min_x = p.x; }
        if p.y < min_y { min_y = p.y; }
        if p.x > max_x { max_x = p.x; }
        if p.y > max_y { max_y = p.y; }
    }
    (Point::new(min_x, min_y), Point::new(max_x, max_y))
}

/// Scan a horizontal line at `scan_y` through the ring and update best point/length.
fn scan_for_longest_segment(
    ring: &[Point],
    scan_y: f64,
    best_point: &mut Option<Point>,
    best_length: &mut f64,
) {
    let mut intersections = collect_intersections(ring, scan_y);
    intersections.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    // Walk pairs of intersections (entry/exit)
    let mut i = 0;
    while i + 1 < intersections.len() {
        let x0 = intersections[i];
        let x1 = intersections[i + 1];
        let seg_len = x1 - x0;
        if seg_len > *best_length {
            *best_length = seg_len;
            *best_point = Some(Point::new((x0 + x1) * 0.5, scan_y));
        }
        i += 2;
    }
}

fn subtract_interval_list(segments: &mut Vec<(f64, f64)>, cut_start: f64, cut_end: f64) {
    if cut_end <= cut_start {
        return;
    }
    let mut next = Vec::with_capacity(segments.len() + 1);
    for (a, b) in segments.drain(..) {
        if cut_end <= a || cut_start >= b {
            next.push((a, b));
            continue;
        }
        if cut_start > a {
            next.push((a, cut_start));
        }
        if cut_end < b {
            next.push((cut_end, b));
        }
    }
    *segments = next;
}

fn scan_for_longest_segment_with_holes(
    outer: &[Point],
    inners: &[Vec<Point>],
    scan_y: f64,
    best_point: &mut Option<Point>,
    best_length: &mut f64,
) {
    let mut outer_xs = collect_intersections(outer, scan_y);
    outer_xs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mut i = 0;
    while i + 1 < outer_xs.len() {
        let x0 = outer_xs[i];
        let x1 = outer_xs[i + 1];
        i += 2;
        if x1 <= x0 {
            continue;
        }

        let mut segments = vec![(x0, x1)];
        for inner in inners {
            if inner.len() < 3 {
                continue;
            }
            let mut hole_xs = collect_intersections(inner, scan_y);
            hole_xs.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mut j = 0;
            while j + 1 < hole_xs.len() {
                subtract_interval_list(&mut segments, hole_xs[j], hole_xs[j + 1]);
                j += 2;
                if segments.is_empty() {
                    break;
                }
            }
            if segments.is_empty() {
                break;
            }
        }

        for (a, b) in segments {
            let seg_len = b - a;
            if seg_len <= *best_length {
                continue;
            }
            let candidate = Point::new((a + b) * 0.5, scan_y);
            if !point_in_polygon(&candidate, outer) {
                continue;
            }
            if inners.iter().any(|hole| point_in_polygon(&candidate, hole)) {
                continue;
            }
            *best_length = seg_len;
            *best_point = Some(candidate);
        }
    }
}

/// Collect x-coordinates where the scan line at `scan_y` intersects ring edges.
fn collect_intersections(ring: &[Point], scan_y: f64) -> Vec<f64> {
    let n = ring.len();
    let mut xs = Vec::new();
    for i in 0..n {
        let j = (i + 1) % n;
        let a = &ring[i];
        let b = &ring[j];
        let (lo_y, hi_y) = if a.y < b.y { (a.y, b.y) } else { (b.y, a.y) };
        // Check if scan line crosses this edge (half-open interval to avoid double-counting vertices)
        if scan_y >= lo_y && scan_y < hi_y {
            let t = (scan_y - a.y) / (b.y - a.y);
            xs.push(a.x + t * (b.x - a.x));
        }
    }
    xs
}

// ---------------------------------------------------------------------------
// Tile intersection
// ---------------------------------------------------------------------------

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
fn clamp_tile(val: f64, max_tile: u32) -> u32 {
    let floored = val.floor();
    if floored < 0.0 {
        0
    } else if floored > f64::from(max_tile) {
        max_tile
    } else {
        floored as u32
    }
}

/// Project a WGS84 bbox to Mercator and return the `MercBbox`.
#[cfg(test)]
pub fn project_bbox(south: f64, west: f64, north: f64, east: f64) -> MercBbox {
    let sw = project(south, west);
    let ne = project(north, east);
    MercBbox {
        min_x: sw.x,
        min_y: ne.y, // In Mercator, north has smaller y
        max_x: ne.x,
        max_y: sw.y,
    }
}

// ---------------------------------------------------------------------------
// MVT geometry helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tile-coordinate helpers (shared by tilegen + ocean)
// ---------------------------------------------------------------------------

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
// Point-in-polygon (ray casting)
// ---------------------------------------------------------------------------

/// Ray-casting point-in-polygon test. Returns true if `p` is inside `ring`.
///
/// Consolidated from ocean.rs and multipolygon.rs which both had independent
/// implementations of the same ray-casting algorithm.
pub fn point_in_polygon(p: &Point, ring: &[Point]) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let yi = ring[i].y;
        let yj = ring[j].y;
        if (yi > p.y) != (yj > p.y) {
            let intersect_x = ring[i].x + (p.y - yi) / (yj - yi) * (ring[j].x - ring[i].x);
            if p.x < intersect_x {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

// ---------------------------------------------------------------------------
// Interior tile detection
// ---------------------------------------------------------------------------

/// Check if a tile (given by its buffered `ClipRect`) is entirely interior to a polygon ring.
///
/// Single O(n) pass combining 4-corner PIP ray-cast with edge-bbox overlap check.
/// Returns true when no edge bbox overlaps the clip rect AND all 4 corners of the
/// clip rect are inside the ring. Conservative: may return false for tiles that are
/// truly interior but have a nearby edge bbox.
pub fn tile_is_interior(ring: &[Point], clip: &ClipRect) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }

    let corners: [(f64, f64); 4] = [
        (clip.min_x, clip.min_y),
        (clip.max_x, clip.min_y),
        (clip.max_x, clip.max_y),
        (clip.min_x, clip.max_y),
    ];
    let mut inside = [false; 4];

    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = (ring[i].x, ring[i].y);
        let (xj, yj) = (ring[j].x, ring[j].y);

        // Edge bbox overlap with clip rect — if any edge might cross the tile,
        // the polygon boundary could intersect it, so bail out conservatively.
        if xi.min(xj) <= clip.max_x
            && xi.max(xj) >= clip.min_x
            && yi.min(yj) <= clip.max_y
            && yi.max(yj) >= clip.min_y
        {
            return false;
        }

        // PIP ray-casting for all 4 corners simultaneously.
        for (k, &(cx, cy)) in corners.iter().enumerate() {
            if (yi > cy) != (yj > cy) {
                let intersect_x = xi + (cy - yi) / (yj - yi) * (xj - xi);
                if cx < intersect_x {
                    inside[k] = !inside[k];
                }
            }
        }

        j = i;
    }

    inside[0] && inside[1] && inside[2] && inside[3]
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
    const BYTES: usize = Self::DIM * Self::DIM / 8;

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
    fn set_bit(&self, tx: u32, ty: u32) {
        let idx = ty as usize * Self::DIM + tx as usize;
        let byte_idx = idx / 8;
        let bit_idx = idx % 8;
        self.bits[byte_idx].fetch_or(1 << bit_idx, Ordering::Relaxed);
    }

    #[inline]
    fn get_bit(&self, tx: u32, ty: u32) -> bool {
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

// ---------------------------------------------------------------------------
// Tests (see geometry_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "geometry_tests.rs"]
mod tests;
