use std::f64::consts::PI;

use super::projection::Point;

/// Minimum polygon area in tile extent units² (1 sq pixel).
/// Polygon rings with |area| below this contribute nothing visible.
pub const MIN_POLY_AREA: i64 = super::tiles::PX * super::tiles::PX; // 256

/// The sign of a ring's doubled shoelace area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingOrientation {
    Positive,
    Negative,
}

/// Return a ring's orientation from its signed area or doubled shoelace sum.
///
/// Zero-area rings have no role under MapLibre's `classifyRings` semantics,
/// so they return `None` and are skipped by ring grouping. An unordered value
/// such as NaN also returns `None`.
#[allow(clippy::needless_pass_by_value)] // scalar area; by-value keeps call sites simple
pub fn ring_role<T>(area: T) -> Option<RingOrientation>
where
    T: PartialOrd + From<i8>,
{
    match area.partial_cmp(&T::from(0))? {
        std::cmp::Ordering::Greater => Some(RingOrientation::Positive),
        std::cmp::Ordering::Less => Some(RingOrientation::Negative),
        std::cmp::Ordering::Equal => None,
    }
}

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
    let c_sq = super::projection::EARTH_CIRCUMFERENCE * super::projection::EARTH_CIRCUMFERENCE;
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

/// Check if a closed ring (first == last) has any self-intersections.
/// Returns true if the ring is simple (no crossings). O(n²) but rings are
/// typically small after DP simplification.
pub fn ring_is_simple(ring: &[(i32, i32)]) -> bool {
    if ring.len() < 4 {
        return true;
    }
    let n = ring.len() - 1; // exclude closing vertex
    for i in 0..n {
        let a1 = ring[i];
        let a2 = ring[i + 1];
        // Check against non-adjacent edges
        for j in (i + 2)..n {
            let b1 = ring[j];
            let b2 = ring[(j + 1) % ring.len()];
            if segments_cross(a1, a2, b1, b2) {
                return false;
            }
        }
    }
    true
}

/// Test if two line segments (a1-a2) and (b1-b2) properly cross (not just touch).
fn segments_cross(a1: (i32, i32), a2: (i32, i32), b1: (i32, i32), b2: (i32, i32)) -> bool {
    let d1 = cross_sign(a1, a2, b1);
    let d2 = cross_sign(a1, a2, b2);
    let d3 = cross_sign(b1, b2, a1);
    let d4 = cross_sign(b1, b2, a2);
    // Proper crossing: endpoints of each segment on opposite sides of the other
    d1 != d2 && d3 != d4 && d1 != 0 && d2 != 0 && d3 != 0 && d4 != 0
}

/// Sign of the cross product (p2-p1) × (p3-p1).
fn cross_sign(p1: (i32, i32), p2: (i32, i32), p3: (i32, i32)) -> i8 {
    // Full-range i32 differences need 33 bits; their determinant needs i128.
    let dx2 = i128::from(p2.0) - i128::from(p1.0);
    let dy2 = i128::from(p2.1) - i128::from(p1.1);
    let dx3 = i128::from(p3.0) - i128::from(p1.0);
    let dy3 = i128::from(p3.1) - i128::from(p1.1);
    let cross = dx2 * dy3 - dy2 * dx3;
    if cross > 0 {
        1
    } else if cross < 0 {
        -1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::ring_is_simple;

    #[test]
    fn full_coordinate_range_square_is_simple() {
        let ring = [
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MIN),
            (i32::MAX, i32::MAX),
            (i32::MIN, i32::MAX),
            (i32::MIN, i32::MIN),
        ];
        assert!(ring_is_simple(&ring));
    }

    #[test]
    fn full_coordinate_range_bow_tie_is_not_simple() {
        let ring = [
            (i32::MIN, i32::MIN),
            (i32::MAX, i32::MAX),
            (i32::MIN, i32::MAX),
            (i32::MAX, i32::MIN),
            (i32::MIN, i32::MIN),
        ];
        assert!(!ring_is_simple(&ring));
    }
}
