use super::polygon::point_in_polygon;
use super::projection::Point;

/// Return the unsigned area and centroid of a ring. Open and closed rings are
/// both accepted; a repeated closing vertex is not counted as an extra point.
#[allow(clippy::float_cmp)] // Exact closure and zero area, as in is_convex.
fn ring_area_centroid(ring: &[Point]) -> Option<(f64, Point)> {
    let ring = if ring.len() > 1
        && ring
            .first()
            .zip(ring.last())
            .is_some_and(|(a, b)| a.x == b.x && a.y == b.y)
    {
        &ring[..ring.len() - 1]
    } else {
        ring
    };
    if ring.len() < 3 || ring.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
        return None;
    }

    // Work relative to the first vertex to keep the shoelace sums stable for
    // rings far from the origin.
    let origin = ring[0];
    let mut twice_area = 0.0;
    let mut weighted_x = 0.0;
    let mut weighted_y = 0.0;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        let ax = a.x - origin.x;
        let ay = a.y - origin.y;
        let bx = b.x - origin.x;
        let by = b.y - origin.y;
        let cross = ax * by - bx * ay;
        twice_area += cross;
        weighted_x += (ax + bx) * cross;
        weighted_y += (ay + by) * cross;
    }
    if !twice_area.is_finite()
        || twice_area == 0.0
        || !weighted_x.is_finite()
        || !weighted_y.is_finite()
    {
        return None;
    }

    let centroid = Point::new(
        origin.x + weighted_x / (3.0 * twice_area),
        origin.y + weighted_y / (3.0 * twice_area),
    );
    if !centroid.x.is_finite() || !centroid.y.is_finite() {
        return None;
    }
    Some((twice_area.abs() * 0.5, centroid))
}

/// Compute the planar area centroid of a polygon, subtracting the area of its
/// inner rings. Ring winding and closure convention do not affect the result.
/// Like other centroids, this point can still fall outside the polygon.
pub(crate) fn polygon_centroid(outer: &[Point], inners: &[Vec<Point>]) -> Option<Point> {
    let (outer_area, outer_centroid) = ring_area_centroid(outer)?;
    let mut area = outer_area;
    let mut weighted_x = outer_centroid.x * outer_area;
    let mut weighted_y = outer_centroid.y * outer_area;

    for inner in inners {
        let Some((hole_area, hole_centroid)) = ring_area_centroid(inner) else {
            if inner.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
                return None;
            }
            continue;
        };
        area -= hole_area;
        weighted_x -= hole_centroid.x * hole_area;
        weighted_y -= hole_centroid.y * hole_area;
    }

    if !area.is_finite() || area <= 0.0 || !weighted_x.is_finite() || !weighted_y.is_finite() {
        return None;
    }
    let centroid = Point::new(weighted_x / area, weighted_y / area);
    (centroid.x.is_finite() && centroid.y.is_finite()).then_some(centroid)
}

/// Whether a polygon is convex and winds once. Accept either ring closure
/// convention and ignore zero-length edges and collinear triples. Holes must
/// be ruled out by the caller before using this for centroid placement.
#[allow(clippy::float_cmp)] // Exact closure and turn signs; no tolerance-based geometry changes.
pub(crate) fn is_convex(ring: &[Point]) -> bool {
    let ring = if ring.len() > 1
        && ring
            .first()
            .zip(ring.last())
            .is_some_and(|(a, b)| a.x == b.x && a.y == b.y)
    {
        &ring[..ring.len() - 1]
    } else {
        ring
    };
    if ring.len() < 3 || ring.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
        return false;
    }
    let mut turn = 0.0_f64;
    let mut first = [0.0_f64; 2];
    let mut previous = [0.0_f64; 2];
    let mut flips = [0_u32; 2];
    let mut edges = ring
        .iter()
        .zip(ring.iter().cycle().skip(1))
        .map(|(a, b)| [b.x - a.x, b.y - a.y])
        .filter(|edge| *edge != [0.0, 0.0]);
    let Some(first_edge) = edges.next() else {
        return false;
    };
    let mut previous_edge = first_edge;
    for edge in edges.chain(std::iter::once(first_edge)) {
        let cross = previous_edge[0] * edge[1] - previous_edge[1] * edge[0];
        previous_edge = edge;
        if !cross.is_finite() {
            return false;
        }
        if cross != 0.0 {
            if turn != 0.0 && turn != cross.signum() {
                return false;
            }
            turn = cross.signum();
        }
        for (axis, delta) in edge.into_iter().enumerate() {
            if delta == 0.0 {
                continue;
            }
            let sign = delta.signum();
            if first[axis] == 0.0 {
                first[axis] = sign;
            } else if previous[axis] != sign {
                flips[axis] += 1;
            }
            previous[axis] = sign;
        }
    }
    for (axis_flips, (previous, first)) in flips.iter_mut().zip(previous.into_iter().zip(first)) {
        if previous != first {
            *axis_flips += 1;
        }
    }
    turn != 0.0 && flips == [2, 2]
}

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
        if p.x < min_x {
            min_x = p.x;
        }
        if p.y < min_y {
            min_y = p.y;
        }
        if p.x > max_x {
            max_x = p.x;
        }
        if p.y > max_y {
            max_y = p.y;
        }
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

#[cfg(test)]
mod convexity_tests {
    use super::*;

    #[test]
    fn convexity_ignores_closure_and_winding_but_rejects_multiple_laps() {
        let square = [
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(0.0, 1.0),
        ];
        assert!(is_convex(&square));
        let mut closed = square.to_vec();
        closed.push(square[0]);
        assert!(is_convex(&closed));
        closed.reverse();
        assert!(is_convex(&closed));
        let twice: Vec<_> = square.into_iter().cycle().take(8).collect();
        assert!(!is_convex(&twice));
        assert!(!is_convex(&[
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(2.0, 0.0)
        ]));
    }

    #[test]
    fn duplicate_vertices_cannot_hide_turns() {
        let convex = [
            Point::new(0.0, 0.0),
            Point::new(3.0, 0.0),
            Point::new(3.0, 3.0),
            Point::new(0.0, 3.0),
        ];
        let repeated: Vec<_> = convex
            .into_iter()
            .flat_map(|point| [point, point])
            .collect();
        assert!(is_convex(&repeated));

        let concave = [
            Point::new(0.0, 0.0),
            Point::new(3.0, 0.0),
            Point::new(2.0, 1.0),
            Point::new(2.0, 1.0),
            Point::new(1.0, 3.0),
            Point::new(0.0, 3.0),
        ];
        assert!(!is_convex(&concave));
    }
}
