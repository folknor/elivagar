use super::projection::Point;
use super::tiles::{EXTENT, RENDER_PX};

/// Simplification tolerance in rendered pixels (one tile = 256 by 256 px).
/// 1.0 removes vertices within 1 pixel of the simplified line.
/// Industry standard is 1.0; Tilemaker uses about 0.58 for comparison.
const SIMPLIFY_PIXELS: f64 = RENDER_PX.get() as f64 * 256.0 / EXTENT;

/// Minimum line length squared in tile extent units (1 pixel = 16 extent units).
/// A line whose bounding box diagonal² is below this is sub-pixel and invisible.
pub const MIN_LINE_EXTENT_SQ: i64 = RENDER_PX.get() * RENDER_PX.get();

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
        if p.x < min_x {
            min_x = p.x;
        }
        if p.x > max_x {
            max_x = p.x;
        }
        if p.y < min_y {
            min_y = p.y;
        }
        if p.y > max_y {
            max_y = p.y;
        }
    }
    let pixel = RENDER_PX.get() as f64 / (EXTENT * f64::from(1u32 << zoom));
    let dx = max_x - min_x;
    let dy = max_y - min_y;
    dx * dx + dy * dy < pixel * pixel
}

/// Check if a linestring's bounding box diagonal is shorter than 1 pixel.
/// Returns true if the feature is too small and should be dropped.
pub fn line_is_subpixel(coords: &[(i32, i32)]) -> bool {
    if coords.len() < 2 {
        return true;
    }
    let (mut min_x, mut min_y) = coords[0];
    let (mut max_x, mut max_y) = (min_x, min_y);
    for &(x, y) in &coords[1..] {
        if x < min_x {
            min_x = x;
        }
        if x > max_x {
            max_x = x;
        }
        if y < min_y {
            min_y = y;
        }
        if y > max_y {
            max_y = y;
        }
    }
    let dx = i64::from(max_x) - i64::from(min_x);
    let dy = i64::from(max_y) - i64::from(min_y);
    // Reject long axes before squaring so full-range i32 inputs cannot overflow.
    if dx >= RENDER_PX.get() || dy >= RENDER_PX.get() {
        return false;
    }
    dx * dx + dy * dy < MIN_LINE_EXTENT_SQ
}

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
#[cfg(test)]
pub fn simplify_into(
    points: &[Point],
    tolerance: f64,
    keep_buf: &mut Vec<bool>,
    output: &mut Vec<Point>,
) {
    simplify_into_with_required(points, tolerance, &[], keep_buf, output);
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
) {
    output.clear();
    if points.len() <= 2 {
        output.extend_from_slice(points);
        return;
    }
    simplify_impl(points, tolerance, required_indices, keep_buf, output);
}

fn simplify_impl(
    points: &[Point],
    tolerance: f64,
    required_indices: &[usize],
    keep_buf: &mut Vec<bool>,
    output: &mut Vec<Point>,
) {
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
    if closed {
        simplify_closed(points, tolerance * tolerance, keep_buf);
    } else {
        keep_buf[0] = true;
        keep_buf[points.len() - 1] = true;
        if required_indices.is_empty() {
            dp_recurse(points, 0, points.len() - 1, tolerance * tolerance, keep_buf);
        } else {
            dp_recurse_with_required(points, 0, points.len() - 1, tolerance * tolerance, keep_buf);
        }
    }
    for (i, &keep) in keep_buf.iter().enumerate() {
        if keep {
            output.push(points[i]);
        }
    }
    if closed {
        // The closed path retains vertices among indices 0..len-1 only (the
        // source closing duplicate at len-1 is never forced), so `output` holds
        // no trailing duplicate at this point. A loop that lies within
        // tolerance of its farthest-pair chord keeps the forced {p0, pa, pb};
        // when p0 coincides with an anchor (the start vertex sits at an
        // extremity) only the two chord ends remain, and re-closing them gives
        // the out-and-back [pa, pb, pa]. That stroke is kept, not dropped: it
        // draws the loop within tolerance, it is the same shape the loop takes
        // when a third vertex instead rounds onto an anchor at tile
        // quantization, and dropping it made whether a collapsed loop survived
        // depend on where its source way happens to start. Only a loop whose
        // points all coincide is removed, so a nonconstant closed source
        // always simplifies to at least two points at every tolerance (the
        // emitter's empty-result break relies on that being zoom-independent).
        // The forced p0 is NOT canonicalized away: junction pins are
        // incomplete (block-local detection misses cross-block and two-ref
        // ways when the PBF carries no injected pins), and keeping the source
        // start is what holds an unpinned junction at p0 in place.
        let first = output[0];
        let constant = output
            .iter()
            .all(|q| q.x.to_bits() == first.x.to_bits() && q.y.to_bits() == first.y.to_bits());
        if constant {
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
    }
}

/// Simplify a closed line around a non-degenerate chord found by a two-sweep
/// farthest-pair search. This is a long real chord, not a geometric diameter.
fn simplify_closed(points: &[Point], tol_sq: f64, keep: &mut [bool]) {
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
    dp_recurse_with_required(&rotated, 0, b_rotated, tol_sq, &mut rotated_keep);
    dp_recurse_with_required(&rotated, b_rotated, unique_len, tol_sq, &mut rotated_keep);
    for (rotated_idx, &original_idx) in original.iter().enumerate() {
        keep[original_idx] |= rotated_keep[rotated_idx];
    }
}

/// True when the vertex sequence equals its own reverse (bit-exact), i.e. the
/// palindrome / out-and-back shape.
#[cfg(test)]
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
fn dp_recurse(points: &[Point], start: usize, end: usize, tol_sq: f64, keep: &mut [bool]) {
    if end <= start + 1 {
        return;
    }
    let (max_idx, max_dist_sq) = find_farthest(points, start, end);
    if max_dist_sq > tol_sq {
        keep[max_idx] = true;
        dp_recurse(points, start, max_idx, tol_sq, keep);
        dp_recurse(points, max_idx, end, tol_sq, keep);
    }
}

fn dp_recurse_with_required(
    points: &[Point],
    start: usize,
    end: usize,
    tol_sq: f64,
    keep: &mut [bool],
) {
    if end <= start + 1 {
        return;
    }

    if let Some(split_idx) = (start + 1..end).find(|&i| keep[i]) {
        dp_recurse_with_required(points, start, split_idx, tol_sq, keep);
        dp_recurse_with_required(points, split_idx, end, tol_sq, keep);
        return;
    }

    let (max_idx, max_dist_sq) = find_farthest(points, start, end);
    if max_dist_sq > tol_sq {
        keep[max_idx] = true;
        dp_recurse_with_required(points, start, max_idx, tol_sq, keep);
        dp_recurse_with_required(points, max_idx, end, tol_sq, keep);
    }
}

/// Find the point farthest from the line segment `points[start]..points[end]`.
///
/// Must scan all intermediate points - DP correctness requires splitting at the
/// true maximum, not just any above-tolerance point. Early termination was
/// investigated and rejected: there's no usable upper bound to prune against,
/// and the loop body (one `perp_dist_sq` + compare per point) is already minimal.
/// The real DP cost driver is the number of recursive calls; the line emitter's
/// subpixel early break is what keeps those off invisible geometry.
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

#[cfg(test)]
mod subpixel_tests {
    use super::line_is_subpixel;

    #[test]
    fn full_coordinate_range_is_visible() {
        for coords in [
            [(i32::MIN, 0), (i32::MAX, 0)],
            [(0, i32::MIN), (0, i32::MAX)],
            [(i32::MIN, i32::MIN), (i32::MAX, i32::MAX)],
        ] {
            assert!(!line_is_subpixel(&coords));
        }
    }

    #[test]
    fn diagonal_controls_subpixel_threshold() {
        assert!(line_is_subpixel(&[(0, 0), (15, 0)]));
        assert!(!line_is_subpixel(&[(0, 0), (16, 0)]));
        assert!(line_is_subpixel(&[(0, 0), (11, 11)]));
        assert!(!line_is_subpixel(&[(0, 0), (12, 12)]));
        assert!(line_is_subpixel(&[
            (i32::MIN, i32::MIN),
            (i32::MIN + 1, i32::MIN + 1),
        ]));
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
    fn collapsed_closed_ring_survives_whatever_its_start() {
        // A long narrow loop whose thin dimension collapses under a coarse
        // tolerance. Started at an extremity, only the two chord ends remain;
        // started mid-side, the forced start adds a third vertex. Both must
        // survive as a closed stroke along the 20-unit chord - which of the
        // two the source starts at must not decide whether it is drawn.
        let at_extremity = [
            p(0.0, 0.0),
            p(10.0, 0.001),
            p(20.0, 0.0),
            p(10.0, -0.001),
            p(0.0, 0.0),
        ];
        let mid_side = [
            p(10.0, 0.001),
            p(20.0, 0.0),
            p(10.0, -0.001),
            p(0.0, 0.0),
            p(10.0, 0.001),
        ];
        let out = simplify(&at_extremity, 1.0);
        assert_eq!(out.len(), 3, "collapsed loop keeps its chord, got {out:?}");
        assert!(eq(out[0], out[2]), "result must be closed");
        assert!(is_own_reverse(&out), "the chord is an out-and-back");
        let out = simplify(&mid_side, 1.0);
        assert!(
            out.len() >= 3,
            "collapsed loop keeps its chord, got {out:?}"
        );
        assert!(eq(out[0], out[out.len() - 1]), "result must be closed");
        for anchor in [p(0.0, 0.0), p(20.0, 0.0)] {
            assert!(
                out.iter().any(|&v| eq(v, anchor)),
                "chord end {anchor:?} kept"
            );
        }
    }

    #[test]
    fn constant_closed_ring_drops() {
        let ring = [p(5.0, 5.0); 5];
        assert!(simplify(&ring, 1.0).is_empty());
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
