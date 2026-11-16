//! Integer geometry primitives for the diff: bounding boxes, the discrete
//! Hausdorff distance and its nearest-point index, and polygon hole
//! containment.
//!
//! Everything here is exact integer arithmetic over MVT tile coordinates. That
//! is deliberate: displacements are reported in tile units and compared against
//! `--tol`, so a float path would make the tolerance verdict depend on rounding.
//! Distances are squared internally and only converted with `ceil_sqrt` at the
//! boundary, which keeps "moved by at most N" a conservative claim.
//!
//! Ported verbatim from elivagar's shed `regress.rs` (commit `0129ef3~1`),
//! retyped onto `prepared`'s augmented components.

use super::super::eliv::CanonRingRole;
use super::prepared::{PreparedComponent, PreparedFeature};

// ---------------------------------------------------------------------------
// Bounding boxes
// ---------------------------------------------------------------------------

/// An integer bounding box. `empty` is tracked explicitly rather than encoded as
/// an inverted range so a degenerate single-point box stays representable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Bbox {
    min_x: i32,
    min_y: i32,
    max_x: i32,
    max_y: i32,
    empty: bool,
}

impl Bbox {
    pub(crate) fn empty() -> Self {
        Self {
            empty: true,
            ..Self::default()
        }
    }

    pub(crate) fn from_points(points: &[(i32, i32)]) -> Self {
        let Some(&(x, y)) = points.first() else {
            return Self::empty();
        };
        let mut out = Self {
            min_x: x,
            min_y: y,
            max_x: x,
            max_y: y,
            empty: false,
        };
        for &(x, y) in &points[1..] {
            out.include_point(x, y);
        }
        out
    }

    fn include_point(&mut self, x: i32, y: i32) {
        if self.empty {
            *self = Self {
                min_x: x,
                min_y: y,
                max_x: x,
                max_y: y,
                empty: false,
            };
            return;
        }
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    pub(crate) fn include_bbox(&mut self, other: Self) {
        if other.empty {
            return;
        }
        self.include_point(other.min_x, other.min_y);
        self.include_point(other.max_x, other.max_y);
    }

    /// Squared distance between the two boxes - a lower bound on the squared
    /// distance between any point of one and any point of the other, so the
    /// matcher can reject a candidate edge without touching its vertices.
    pub(crate) fn lower_bound_sq(self, other: Self) -> u64 {
        if self.empty || other.empty {
            return u64::MAX;
        }
        let dx = axis_gap(self.min_x, self.max_x, other.min_x, other.max_x);
        let dy = axis_gap(self.min_y, self.max_y, other.min_y, other.max_y);
        dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
    }

    /// Squared distance between the box centres, doubled in each axis to stay in
    /// integers. Only ever used to order candidates, so the constant factor is
    /// irrelevant and the doubling costs nothing.
    pub(crate) fn center_distance_sq(self, other: Self) -> u64 {
        let x = i64::from(self.min_x) + i64::from(self.max_x)
            - i64::from(other.min_x)
            - i64::from(other.max_x);
        let y = i64::from(self.min_y) + i64::from(self.max_y)
            - i64::from(other.min_y)
            - i64::from(other.max_y);
        square_i64(x).saturating_add(square_i64(y))
    }
}

fn axis_gap(a_min: i32, a_max: i32, b_min: i32, b_max: i32) -> u64 {
    if a_max < b_min {
        u64::try_from(i64::from(b_min) - i64::from(a_max)).unwrap_or(u64::MAX)
    } else if b_max < a_min {
        u64::try_from(i64::from(a_min) - i64::from(b_max)).unwrap_or(u64::MAX)
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Structural comparison
// ---------------------------------------------------------------------------

/// Do two components have the same ring shape - same count, same roles in the
/// same positions, same hole-containment verdict?
///
/// The containment term is what separates "a hole moved a little" from "a hole
/// left its outer", which is a topology change no tolerance should absorb.
pub(crate) fn component_structure_matches(
    current: &PreparedComponent,
    baseline: &PreparedComponent,
) -> bool {
    if current.rings.len() != baseline.rings.len()
        || current
            .rings
            .iter()
            .zip(&baseline.rings)
            .any(|(left, right)| left.role != right.role)
    {
        return false;
    }
    polygon_holes_contained(current) == polygon_holes_contained(baseline)
}

/// Does every hole ring start inside the component's outer ring? A component
/// with no outer ring vacuously passes (points and lines take this path).
fn polygon_holes_contained(component: &PreparedComponent) -> bool {
    let Some(outer) = component
        .rings
        .iter()
        .find(|ring| ring.role == CanonRingRole::Outer)
    else {
        return true;
    };
    component
        .rings
        .iter()
        .filter(|ring| ring.role == CanonRingRole::Hole)
        .all(|hole| {
            hole.points
                .first()
                .is_some_and(|&point| point_in_ring(point, &outer.points))
        })
}

/// Crossing-number point-in-polygon, in exact integers: the usual
/// `x < xi + (py - yi) * (xj - xi) / (yj - yi)` test with the division cleared,
/// flipping the comparison when the denominator is negative.
fn point_in_ring(point: (i32, i32), ring: &[(i32, i32)]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    // Widen before subtracting, as the rest of the module does: a decodable but
    // pathological coordinate pair straddling the i32 range overflows the
    // difference itself - a debug panic, a silent wrap in release - long before
    // the product it feeds could.
    let (px, py) = (i64::from(point.0), i64::from(point.1));
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = (i64::from(ring[i].0), i64::from(ring[i].1));
        let (xj, yj) = (i64::from(ring[j].0), i64::from(ring[j].1));
        if (yi > py) != (yj > py) {
            let lhs = (px - xi) * (yj - yi);
            let rhs = (xj - xi) * (py - yi);
            let crosses = if yj > yi { lhs < rhs } else { lhs > rhs };
            if crosses {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

// ---------------------------------------------------------------------------
// Distance
// ---------------------------------------------------------------------------

/// The closest component-to-component distance between two features, used only
/// to order candidate pairings. The bbox lower bound prunes before any Hausdorff
/// runs.
pub(crate) fn feature_distance(current: &PreparedFeature, baseline: &PreparedFeature) -> i32 {
    if current.geom_type != baseline.geom_type {
        return i32::MAX;
    }
    let mut best = i32::MAX;
    for cur in &current.components {
        for bl in &baseline.components {
            let lower = ceil_sqrt(cur.bbox.lower_bound_sq(bl.bbox));
            if lower >= best {
                continue;
            }
            best = best.min(component_distance(cur, bl));
        }
    }
    best
}

/// The worst per-ring Hausdorff distance between two components, ring `i` to
/// ring `i`. Callers pair components structurally first, so the positional zip
/// is meaningful: `classify_detail_components` rejects a ring-count or
/// ring-role difference as structural before this is consulted for a verdict.
pub(crate) fn component_distance(current: &PreparedComponent, baseline: &PreparedComponent) -> i32 {
    current
        .rings
        .iter()
        .zip(&baseline.rings)
        .map(|(left, right)| discrete_hausdorff(&left.points, &right.points))
        .max()
        .unwrap_or(0)
}

/// Symmetric discrete Hausdorff distance over two vertex sets: the furthest any
/// vertex of either is from its nearest vertex of the other.
fn discrete_hausdorff(a: &[(i32, i32)], b: &[(i32, i32)]) -> i32 {
    if a.is_empty() || b.is_empty() {
        return i32::MAX;
    }
    let ab = directed_distance(a, b);
    let ba = directed_distance(b, a);
    ceil_sqrt(ab.max(ba))
}

fn directed_distance(from: &[(i32, i32)], to: &[(i32, i32)]) -> u64 {
    let index = PointIndex::new(to);
    let mut maximum = 0u64;
    let mut rolling_start = 0usize;
    for &point in from {
        let (nearest, start) = index.nearest(point, rolling_start);
        rolling_start = start;
        maximum = maximum.max(nearest);
    }
    maximum
}

/// Nearest-vertex lookup. Small rings scan linearly from the previous hit -
/// consecutive query points on a ring land near each other, so the rolling start
/// usually finds the answer in a step or two - and larger ones build a KD tree,
/// where that locality no longer beats the tree descent.
enum PointIndex<'a> {
    Small(&'a [(i32, i32)]),
    Tree(KdNode),
}

impl<'a> PointIndex<'a> {
    fn new(points: &'a [(i32, i32)]) -> Self {
        if points.len() < super::super::constants::KD_TREE_THRESHOLD {
            Self::Small(points)
        } else {
            match KdNode::build(points.to_vec(), 0) {
                Some(tree) => Self::Tree(tree),
                None => Self::Small(points),
            }
        }
    }

    fn nearest(&self, point: (i32, i32), rolling_start: usize) -> (u64, usize) {
        match self {
            Self::Small(points) => {
                if points.is_empty() {
                    return (u64::MAX, 0);
                }
                let mut best = u64::MAX;
                let mut best_idx = 0usize;
                for step in 0..points.len() {
                    let idx = (rolling_start + step) % points.len();
                    let distance = squared_distance(point, points[idx]);
                    if distance < best {
                        best = distance;
                        best_idx = idx;
                        if best == 0 {
                            break;
                        }
                    }
                }
                (best, best_idx)
            }
            Self::Tree(tree) => (tree.nearest(point, u64::MAX), 0),
        }
    }
}

struct KdNode {
    point: (i32, i32),
    axis: usize,
    bbox: Bbox,
    left: Option<Box<Self>>,
    right: Option<Box<Self>>,
}

impl KdNode {
    fn build(mut points: Vec<(i32, i32)>, depth: usize) -> Option<Self> {
        if points.is_empty() {
            return None;
        }
        let axis = depth % 2;
        points.sort_unstable_by_key(|point| if axis == 0 { point.0 } else { point.1 });
        let middle = points.len() / 2;
        let right = points.split_off(middle + 1);
        let point = points.pop()?;
        let left = if points.is_empty() {
            None
        } else {
            Some(Box::new(Self::build(points, depth + 1)?))
        };
        let right = if right.is_empty() {
            None
        } else {
            Some(Box::new(Self::build(right, depth + 1)?))
        };
        let mut bbox = Bbox::from_points(&[point]);
        if let Some(left) = &left {
            bbox.include_bbox(left.bbox);
        }
        if let Some(right) = &right {
            bbox.include_bbox(right.bbox);
        }
        Some(Self {
            point,
            axis,
            bbox,
            left,
            right,
        })
    }

    fn nearest(&self, point: (i32, i32), mut best: u64) -> u64 {
        best = best.min(squared_distance(point, self.point));
        let (near, far) = if (self.axis == 0 && point.0 <= self.point.0)
            || (self.axis == 1 && point.1 <= self.point.1)
        {
            (&self.left, &self.right)
        } else {
            (&self.right, &self.left)
        };
        if let Some(near) = near
            && near.bbox.lower_bound_sq(Bbox::from_points(&[point])) < best
        {
            best = near.nearest(point, best);
        }
        if let Some(far) = far
            && far.bbox.lower_bound_sq(Bbox::from_points(&[point])) < best
        {
            best = far.nearest(point, best);
        }
        best
    }
}

// ---------------------------------------------------------------------------
// Integer arithmetic helpers
// ---------------------------------------------------------------------------

fn square_i64(value: i64) -> u64 {
    value.unsigned_abs().saturating_mul(value.unsigned_abs())
}

fn squared_distance(a: (i32, i32), b: (i32, i32)) -> u64 {
    square_i64(i64::from(a.0) - i64::from(b.0))
        .saturating_add(square_i64(i64::from(a.1) - i64::from(b.1)))
}

/// Round a squared distance up to an integer distance, so a reported
/// displacement never understates the movement it stands for.
fn ceil_sqrt(value: u64) -> i32 {
    let root = integer_sqrt(value);
    let ceil = if root.saturating_mul(root) < value {
        root + 1
    } else {
        root
    };
    i32::try_from(ceil).unwrap_or(i32::MAX)
}

/// Bitwise integer square root - no float round trip, so the result is exact for
/// the full u64 range.
fn integer_sqrt(value: u64) -> u64 {
    let mut result = 0u64;
    let mut bit = 1u64 << 62;
    while bit > value {
        bit >>= 2;
    }
    let mut remainder = value;
    while bit != 0 {
        if remainder >= result + bit {
            remainder -= result + bit;
            result = (result >> 1) + bit;
        } else {
            result >>= 1;
        }
        bit >>= 2;
    }
    result
}
