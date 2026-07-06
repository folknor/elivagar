use super::projection::Point;
use super::SIMPLIFY_PIXELS;

/// Compute the simplification tolerance for a given zoom level.
/// Returns the tolerance in Mercator [0,1] units corresponding to
/// `SIMPLIFY_PIXELS` rendered pixels at the given zoom.
#[inline]
pub fn simplify_tolerance(zoom: u8) -> f64 {
    let z_scale = f64::from(1u32 << zoom);
    SIMPLIFY_PIXELS / (256.0 * z_scale)
}

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
        // All points in this segment are within tolerance - max_dist_sq is
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
/// Must scan all intermediate points - DP correctness requires splitting at the
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
///
/// `tol_scale` multiplies the base simplification tolerance. Use 1.0 for
/// standard behavior (lines). Values > 1.0 simplify more aggressively (fewer
/// vertices), useful for polygon fill layers where sub-pixel precision is less
/// important than for stroked lines.
#[hotpath::measure]
pub fn for_each_zoom_simplified<F, S>(
    merc: &[Point],
    z_lo: u8,
    z_hi: u8,
    min_points: usize,
    tol_scale: S,
    skip_bbox_check: bool,
    mut callback: F,
) where
    F: FnMut(u8, &[Point]),
    S: Fn(u8) -> f64,
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
            // Skips DP entirely - O(1) vs O(n²).
            // Skipped for connectivity-critical layers (streets, boundaries) where
            // short connecting ways must survive to maintain road network topology.
            if !skip_bbox_check && super::merc_bbox_is_subpixel(cascade, z) {
                break;
            }
            // Option E: if cascade already has ≤ min_points vertices, DP can't
            // reduce further - skip the call entirely.
            if cascade.len() > min_points {
                let tol = simplify_tolerance(z) * tol_scale(z);
                // Option D: if last DP's max deviation is already below this
                // zoom's tolerance, the cascade is optimal - skip DP.
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
#[allow(dead_code)]
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
///
/// `tol_scale` multiplies the base simplification tolerance (see
/// [`for_each_zoom_simplified`] for rationale).
#[hotpath::measure]
pub fn for_each_zoom_simplified_multi<F, S>(
    outer: &[Point],
    inners: &[Vec<Point>],
    z_lo: u8,
    z_hi: u8,
    scratch: &mut SimplifyMultiScratch,
    tol_scale: S,
    mut callback: F,
) where
    F: FnMut(u8, &[Point], &[Vec<Point>]),
    S: Fn(u8) -> f64,
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
        let tol = if z < 14 { simplify_tolerance(z) * tol_scale(z) } else { 0.0 };
        if tol > 0.0 {
            if super::merc_bbox_is_subpixel(cascade_outer, z) {
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
