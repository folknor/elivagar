use std::f64::consts::PI;

use super::projection::Point;

/// Distance in the integer tile extent coordinate space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ExtentUnits(i64);

impl ExtentUnits {
    pub(crate) const fn new(value: i64) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> i64 {
        self.0
    }

    pub(crate) const fn divided_by(self, divisor: i64) -> Self {
        Self(self.0 / divisor)
    }

    pub(crate) const fn squared_area(self) -> Area {
        let value = self.0 as i128;
        Area((value * value).unsigned_abs())
    }

    /// The existing VW threshold uses the pixel-square number in doubled-area
    /// units. This intentionally does not convert a true area into doubled area.
    pub(crate) const fn squared_area_2x(self) -> Area2x {
        let value = self.0 as i128;
        Area2x(value * value)
    }
}

/// True nonnegative area in integer tile extent units squared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Area(u128);

impl Area {
    pub(crate) const ZERO: Self = Self(0);
    pub(crate) const ONE: Self = Self(1);

    pub(crate) const fn from_u64(value: u64) -> Self {
        Self(value as u128)
    }

    #[cfg(test)]
    pub(crate) const fn get(self) -> u128 {
        self.0
    }

    pub(crate) fn as_u64(self) -> u64 {
        u64::try_from(self.0).expect("area threshold fits u64")
    }

    /// Inclusive area threshold used by polygon survival filters.
    pub(crate) const fn meets(self, minimum: Self) -> bool {
        self.0 >= minimum.0
    }

    /// A zero minimum disables this optional contour-drop filter.
    pub(crate) const fn below_nonzero_minimum(self, minimum: Self) -> bool {
        minimum.0 != 0 && !self.meets(minimum)
    }
}

/// Signed doubled shoelace area in integer tile extent units squared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Area2x(i128);

impl Area2x {
    pub(crate) const ZERO: Self = Self(0);

    #[cfg(test)]
    pub(crate) const fn new(value: i128) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> i128 {
        self.0
    }

    pub(crate) const fn meets(self, minimum: Self) -> bool {
        self.0 >= minimum.0
    }

    pub(crate) const fn magnitude(self) -> Self {
        if self.0 < 0 { Self(-self.0) } else { self }
    }

    /// Preserve the existing integer-area convention: odd doubled areas floor.
    pub(crate) const fn true_area(self) -> Area {
        Area(self.0.unsigned_abs() / 2)
    }
}

impl From<i8> for Area2x {
    fn from(value: i8) -> Self {
        Self(i128::from(value))
    }
}

/// One rendered pixel in integer tile extent units.
pub(crate) const RENDER_PX: ExtentUnits = ExtentUnits::new(super::tiles::grid::EXTENT as i64 / 256);

/// The second edge-window thinning pass is one eighth of a rendered pixel.
pub(crate) const WINDOW_DP_TOL: ExtentUnits = RENDER_PX.divided_by(8);

/// One rendered pixel squared, as a typed polygon area threshold.
pub(crate) const MIN_POLY_AREA_AREA: Area = RENDER_PX.squared_area();

/// The sign of a ring's doubled shoelace area in x/y coordinates.
/// Positive is mathematical counter-clockwise; on y-down tile coordinates it
/// renders clockwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RingOrientation {
    Positive,
    Negative,
}

/// Return a ring's orientation from its signed area or doubled shoelace sum.
///
/// Zero-area rings have no role under MapLibre's `classifyRings` semantics,
/// so they return `None` and are skipped by ring grouping. An unordered value
/// such as NaN also returns `None`.
#[allow(clippy::needless_pass_by_value)] // scalar area; by-value keeps call sites simple
pub(crate) fn ring_role<T>(area: T) -> Option<RingOrientation>
where
    T: PartialOrd + From<i8>,
{
    match area.partial_cmp(&T::from(0))? {
        std::cmp::Ordering::Greater => Some(RingOrientation::Positive),
        std::cmp::Ordering::Less => Some(RingOrientation::Negative),
        std::cmp::Ordering::Equal => None,
    }
}

/// Return whether a ring must be reversed to take the requested shoelace role.
/// A zero-area ring keeps its order, matching the polygon engine's old checks.
pub(crate) fn should_reverse(area: Area2x, expected: RingOrientation) -> bool {
    ring_role(area).is_some_and(|actual| actual != expected)
}

/// Signed doubled shoelace area for integer coordinates, shared by integer
/// polygon processing and tile emission. The iterator avoids allocating for
/// callers whose points use a different concrete point type.
pub(crate) fn signed_area_2x(points: impl IntoIterator<Item = (i32, i32)>) -> Area2x {
    let mut points = points.into_iter();
    let Some(first) = points.next() else {
        return Area2x::ZERO;
    };
    let mut previous = first;
    let mut area = 0_i128;
    for current in points {
        area += i128::from(previous.0) * i128::from(current.1);
        area -= i128::from(current.0) * i128::from(previous.1);
        previous = current;
    }
    area += i128::from(previous.0) * i128::from(first.1);
    area -= i128::from(first.0) * i128::from(previous.1);
    Area2x(area)
}

/// One grouped polygon: an outer ring and the holes that follow it.
pub(crate) struct RingGroup<R> {
    pub(crate) outer: R,
    pub(crate) holes: Vec<R>,
}

/// Group decoded MVT polygon rings into polygons: the crate's one rule.
///
/// The first nonzero-area ring fixes the exterior orientation; each later
/// ring of that orientation opens a polygon and each of the other attaches
/// as a hole to the open one. Zero-area rings are skipped, except that a
/// feature's only ring is returned as a polygon whatever its area, before
/// any area is measured. Area wraps from the last vertex to the first, so an
/// unclosed ring (tolerant decoding of a foreign stream) is measured as
/// MapLibre's `calculateSignedArea` would.
///
/// This is MapLibre `classifyRings`, lone-ring shortcut included, minus its
/// 500-ring clamp: that is a rendering limit, and structural detail keeps
/// every contour. The shortcut is kept so the corpus hash (through
/// `tile_detail`), MLT, the ring-grouping oracle in scripts/validate and the
/// renderer all see the same polygon for a lone degenerate ring; skipping it
/// would make the hash blind to an encoder that started emitting such rings.
/// Zero signed area is not proof of invisibility (a bow-tie's lobes cancel),
/// which is a second reason not to drop it. Zero input rings give no groups.
/// `area` is the ring's [`signed_area_2x`].
pub(crate) fn group_rings<R>(
    rings: impl IntoIterator<Item = R>,
    area: impl Fn(&R) -> Area2x,
) -> Vec<RingGroup<R>> {
    let mut rings = rings.into_iter().peekable();
    let Some(first) = rings.next() else {
        return Vec::new();
    };
    if rings.peek().is_none() {
        return vec![RingGroup {
            outer: first,
            holes: Vec::new(),
        }];
    }
    let mut groups: Vec<RingGroup<R>> = Vec::new();
    let mut exterior = None;
    for ring in std::iter::once(first).chain(rings) {
        let Some(orientation) = ring_role(area(&ring)) else {
            continue;
        };
        match exterior {
            Some(exterior) if orientation != exterior => {
                if let Some(group) = groups.last_mut() {
                    group.holes.push(ring);
                }
            }
            _ => {
                exterior = Some(orientation);
                groups.push(RingGroup {
                    outer: ring,
                    holes: Vec::new(),
                });
            }
        }
    }
    groups
}

/// Signed doubled shoelace area of the triangle formed by three integer points.
pub(crate) fn triangle_area_2x(a: (i32, i32), b: (i32, i32), c: (i32, i32)) -> Area2x {
    let ab_x = i128::from(b.0) - i128::from(a.0);
    let ab_y = i128::from(b.1) - i128::from(a.1);
    let ac_x = i128::from(c.0) - i128::from(a.0);
    let ac_y = i128::from(c.1) - i128::from(a.1);
    triangle_area_2x_from_deltas(ab_x, ab_y, ac_x, ac_y)
}

pub(crate) fn triangle_area_2x_from_deltas(
    ab_x: i128,
    ab_y: i128,
    ac_x: i128,
    ac_y: i128,
) -> Area2x {
    Area2x(ab_x * ac_y - ab_y * ac_x)
}

/// Compute the signed area of a ring using the Shoelace formula.
///
/// Positive = counter-clockwise, negative = clockwise. On y-down tile
/// coordinates, positive rings render clockwise.
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
pub(crate) fn ring_is_simple(ring: &[(i32, i32)]) -> bool {
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
