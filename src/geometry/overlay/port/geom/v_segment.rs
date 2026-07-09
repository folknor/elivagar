use crate::geometry::overlay::port::geom::x_segment::XSegment;
use crate::geometry::overlay::port::prim::IntPoint;
use crate::geometry::overlay::port::prim::Triangle;
use crate::geometry::overlay::port::tree::{Expiration, ExpiredKey};
use core::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VSegment {
    pub(crate) a: IntPoint,
    pub(crate) b: IntPoint,
}

impl VSegment {
    #[inline(always)]
    fn is_under_segment_order(&self, other: &VSegment) -> Ordering {
        match self.a.cmp(&other.a) {
            Ordering::Less => Triangle::clock_order(self.a, other.a, self.b),
            Ordering::Equal => Triangle::clock_order(self.a, other.b, self.b),
            Ordering::Greater => Triangle::clock_order(other.a, other.b, self.a),
        }
    }

    #[inline(always)]
    pub(crate) fn is_under_point_order(&self, p: IntPoint) -> Ordering {
        debug_assert!(self.a.x <= p.x && p.x <= self.b.x);
        debug_assert!(p != self.a && p != self.b);

        Triangle::clock_order(self.a, p, self.b)
    }

    #[inline(always)]
    pub(crate) fn is_under_segment(&self, other: &VSegment) -> bool {
        match self.a.cmp(&other.a) {
            Ordering::Less => Triangle::is_clockwise(self.a, other.a, self.b),
            Ordering::Equal => Triangle::is_clockwise(self.a, other.b, self.b),
            Ordering::Greater => Triangle::is_clockwise(other.a, other.b, self.a),
        }
    }

    #[inline(always)]
    pub(crate) fn cmp_by_angle(&self, other: &Self) -> Ordering {
        // sort angles counterclockwise
        // debug_assert!(self.a == other.a);
        let v0 = self.b - self.a;
        let v1 = other.b - other.a;
        let cross = v0.cross_product(v1);
        0i64.cmp(&cross)
    }
}

pub(crate) trait BottomSegment {
    fn update_if_under(&mut self, segment: VSegment);
}

impl BottomSegment for Option<VSegment> {
    #[inline(always)]
    fn update_if_under(&mut self, segment: VSegment) {
        if let Some(best) = self {
            if segment.is_under_segment(&best) {
                *best = segment
            }
        } else {
            *self = Some(segment);
        }
    }
}

impl From<XSegment> for VSegment {
    #[inline(always)]
    fn from(seg: XSegment) -> Self {
        VSegment { a: seg.a, b: seg.b }
    }
}

impl PartialOrd<Self> for VSegment {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VSegment {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> Ordering {
        self.is_under_segment_order(other)
    }
}

impl ExpiredKey<i32> for VSegment {
    #[inline]
    fn expiration(&self) -> i32 {
        self.b.x
    }
}

#[cfg(test)]
mod tests {
    use crate::geometry::overlay::port::geom::v_segment::VSegment;
    use crate::geometry::overlay::port::prim::IntPoint;
    use core::cmp::Ordering;

    #[test]
    fn test_00() {
        let p = IntPoint::new(-10, 10);
        let s = VSegment {
            a: IntPoint::new(-10, -10),
            b: IntPoint::new(10, -10),
        };
        let order = s.is_under_point_order(p);
        assert_eq!(order, Ordering::Less);
    }
}
