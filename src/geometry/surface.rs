use super::point_in_polygon;
use super::projection::Point;

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
