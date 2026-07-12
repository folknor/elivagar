use super::SIMPLIFY_PIXELS;
use super::projection::Point;

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
    simplify_impl(points, tolerance, &[], keep_buf, output)
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
    simplify_impl(points, tolerance, required_indices, keep_buf, output)
}

fn simplify_impl(
    points: &[Point],
    tolerance: f64,
    required_indices: &[usize],
    keep_buf: &mut Vec<bool>,
    output: &mut Vec<Point>,
) -> f64 {
    keep_buf.clear();
    keep_buf.resize(points.len(), false);
    for &idx in required_indices {
        if idx < points.len() {
            keep_buf[idx] = true;
        }
    }
    let closed = points.len() > 3
        && points
            .first()
            .zip(points.last())
            .is_some_and(|(first, last)| {
                first.x.to_bits() == last.x.to_bits() && first.y.to_bits() == last.y.to_bits()
            });
    let max_dev_sq = if closed {
        simplify_closed(points, tolerance * tolerance, keep_buf)
    } else {
        keep_buf[0] = true;
        keep_buf[points.len() - 1] = true;
        if required_indices.is_empty() {
            dp_recurse(points, 0, points.len() - 1, tolerance * tolerance, keep_buf)
        } else {
            dp_recurse_with_required(points, 0, points.len() - 1, tolerance * tolerance, keep_buf)
        }
    };
    for (i, &keep) in keep_buf.iter().enumerate() {
        if keep {
            output.push(points[i]);
        }
    }
    if closed {
        // The closed path retains vertices among indices 0..len-1 only (the
        // source closing duplicate at len-1 is never forced), so `output` holds
        // no trailing duplicate at this point - count distinct over the whole
        // slice. Keep the ring only when three distinct vertices survived; a
        // fully-collapsed input yields the forced {p0, pa, pb} and drops here
        // when those are not three distinct points.
        let mut distinct: Vec<(u64, u64)> = Vec::new();
        for point in output.iter() {
            let key = (point.x.to_bits(), point.y.to_bits());
            if !distinct.contains(&key) {
                distinct.push(key);
            }
        }
        let distinct = distinct.len();
        if distinct < 3 {
            output.clear();
        } else if output
            .first()
            .zip(output.last())
            .is_some_and(|(first, last)| {
                first.x.to_bits() != last.x.to_bits() || first.y.to_bits() != last.y.to_bits()
            })
        {
            output.push(output[0]);
        }
        // A valid simplified ring is never its own reverse (that is the
        // fabricated-spike shape L3 exists to prevent).
        debug_assert!(
            output.len() < 2 || !is_own_reverse(output.as_slice()),
            "closed-line simplify produced a palindrome"
        );
    }
    max_dev_sq
}

/// Simplify a closed line around a non-degenerate chord found by a two-sweep
/// farthest-pair search. This is a long real chord, not a geometric diameter.
fn simplify_closed(points: &[Point], tol_sq: f64, keep: &mut [bool]) -> f64 {
    let unique_len = points.len() - 1;
    let a = farthest_point(points, 0, unique_len);
    let b = farthest_point(points, a, unique_len);
    keep[0] = true;
    keep[a] = true;
    keep[b] = true;

    let mut rotated = Vec::with_capacity(points.len());
    let mut original = Vec::with_capacity(points.len());
    for offset in 0..unique_len {
        let idx = (a + offset) % unique_len;
        rotated.push(points[idx]);
        original.push(idx);
    }
    rotated.push(rotated[0]);
    original.push(a);
    let b_rotated = original[..unique_len]
        .iter()
        .position(|&idx| idx == b)
        .expect("farthest-pair anchor must be in rotated ring");
    let mut rotated_keep = vec![false; rotated.len()];
    for (rotated_idx, &original_idx) in original.iter().enumerate() {
        rotated_keep[rotated_idx] = keep[original_idx];
    }
    rotated_keep[0] = true;
    rotated_keep[b_rotated] = true;
    rotated_keep[unique_len] = true;
    let left = dp_recurse_with_required(&rotated, 0, b_rotated, tol_sq, &mut rotated_keep);
    let right =
        dp_recurse_with_required(&rotated, b_rotated, unique_len, tol_sq, &mut rotated_keep);
    for (rotated_idx, &original_idx) in original.iter().enumerate() {
        keep[original_idx] |= rotated_keep[rotated_idx];
    }
    left.max(right)
}

/// True when the vertex sequence equals its own reverse (bit-exact), i.e. the
/// palindrome / out-and-back shape.
fn is_own_reverse(points: &[Point]) -> bool {
    let n = points.len();
    (0..n).all(|i| {
        let a = points[i];
        let b = points[n - 1 - i];
        a.x.to_bits() == b.x.to_bits() && a.y.to_bits() == b.y.to_bits()
    })
}

fn farthest_point(points: &[Point], source: usize, unique_len: usize) -> usize {
    (0..unique_len)
        .max_by(|&left, &right| {
            let distance = |idx: usize| {
                let dx = points[idx].x - points[source].x;
                let dy = points[idx].y - points[source].y;
                dx * dx + dy * dy
            };
            distance(left).total_cmp(&distance(right))
        })
        .expect("closed ring has at least one unique vertex")
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
        let SimplifySingleScratch {
            cascade,
            keep_buf,
            simp_buf,
        } = scratch;
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
///
/// Parked: no production caller. The float-space multipolygon cascade was
/// superseded by the int_ocean integer geometry engine, which now handles all
/// polygon emission. Kept (with tests) rather than deleted because its
/// `SimplifyMultiScratch` is still threaded through the relation/emit pipeline
/// (`emit.rs` carries it as `_simp_scratch`), holding the path open for
/// re-enabling. Only the tests exercise it, hence `allow(dead_code)`.
#[allow(dead_code)]
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
        let tol = if z < 14 {
            simplify_tolerance(z) * tol_scale(z)
        } else {
            0.0
        };
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
                        inner_max_dev_sq[read] =
                            simplify_into(&cascade_inners[read], tol, keep_buf, simp_buf);
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

#[cfg(test)]
mod closed_ring_tests {
    use super::{Point, is_own_reverse, simplify};

    fn p(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn simplify_required(points: &[Point], tolerance: f64, required: &[usize]) -> Vec<Point> {
        let mut keep = Vec::new();
        let mut out = Vec::new();
        super::simplify_into_with_required(points, tolerance, required, &mut keep, &mut out);
        out
    }

    fn eq(a: Point, b: Point) -> bool {
        a.x.to_bits() == b.x.to_bits() && a.y.to_bits() == b.y.to_bits()
    }

    #[test]
    fn thin_closed_ring_drops_or_stays_valid() {
        // A long narrow loop: the thin dimension collapses under a coarse
        // tolerance. Either it drops entirely or it survives as a valid,
        // non-palindromic ring - never the fabricated hair shape.
        let ring = [
            p(0.0, 0.0),
            p(10.0, 0.001),
            p(20.0, 0.0),
            p(10.0, -0.001),
            p(0.0, 0.0),
        ];
        let out = simplify(&ring, 1.0);
        assert!(
            out.is_empty() || (out.len() >= 4 && !is_own_reverse(&out)),
            "thin ring must drop or stay a valid non-palindrome loop, got {out:?}"
        );
        if !out.is_empty() {
            // First == last (re-closed) and at least 3 distinct vertices.
            assert!(eq(out[0], out[out.len() - 1]), "result must be closed");
            assert!(
                !(out.len() == 4 && eq(out[0], out[2])),
                "must not be a [start, apex, start] hair"
            );
        }
    }

    #[test]
    fn fat_closed_ring_keeps_shape() {
        let square = [
            p(0.0, 0.0),
            p(10.0, 0.0),
            p(10.0, 10.0),
            p(0.0, 10.0),
            p(0.0, 0.0),
        ];
        let out = simplify(&square, 0.5);
        assert!(out.len() > 3, "fat ring must retain its shape, got {out:?}");
        assert!(eq(out[0], out[out.len() - 1]), "result must stay closed");
        assert!(!is_own_reverse(&out));
    }

    #[test]
    fn open_line_untouched_by_closed_path() {
        // Distinct endpoints: not a ring, so the closed path never runs.
        let line = [p(0.0, 0.0), p(10.0, 0.2), p(20.0, 0.0)];
        let out = simplify(&line, 0.001);
        assert_eq!(out.len(), 3);
        assert!(eq(out[0], line[0]) && eq(out[2], line[2]));
    }

    #[test]
    fn closed_ring_preserves_required_pin() {
        // Even with a coarse tolerance that would collapse the arcs, a pinned
        // interior vertex survives (pin machinery preserved on the ring path).
        let square = [
            p(0.0, 0.0),
            p(10.0, 0.0),
            p(10.0, 10.0),
            p(0.0, 10.0),
            p(0.0, 0.0),
        ];
        let out = simplify_required(&square, 1000.0, &[1]);
        assert!(
            out.iter().any(|&v| eq(v, square[1])),
            "pinned vertex must survive, got {out:?}"
        );
        assert!(!is_own_reverse(&out));
    }
}
