use crate::geometry::overlay::port::geom::x_segment::XSegment;
use crate::geometry::overlay::port::prim::IntCoord;
use crate::geometry::overlay::port::prim::IntPoint;
use crate::geometry::overlay::port::prim::Triangle;
use core::marker::PhantomData;

pub(super) type CollinearMask = u8;

pub(super) trait EndMask {
    fn is_target_a(&self) -> bool;
    fn is_target_b(&self) -> bool;
    fn is_other_a(&self) -> bool;
    fn is_other_b(&self) -> bool;

    fn new(target_a: bool, target_b: bool, other_a: bool, other_b: bool) -> Self;
}

const TARGET_A: u8 = 0b0001;
const TARGET_B: u8 = 0b0010;
const OTHER_A: u8 = 0b0100;
const OTHER_B: u8 = 0b1000;

impl EndMask for CollinearMask {
    #[inline(always)]
    fn is_target_a(&self) -> bool {
        self & TARGET_A == TARGET_A
    }

    #[inline(always)]
    fn is_target_b(&self) -> bool {
        self & TARGET_B == TARGET_B
    }

    #[inline(always)]
    fn is_other_a(&self) -> bool {
        self & OTHER_A == OTHER_A
    }

    #[inline(always)]
    fn is_other_b(&self) -> bool {
        self & OTHER_B == OTHER_B
    }

    #[inline(always)]
    fn new(target_a: bool, target_b: bool, other_a: bool, other_b: bool) -> Self {
        let a0 = target_a as u8;
        let b0 = (target_b as u8) << 1;
        let a1 = (other_a as u8) << 2;
        let b1 = (other_b as u8) << 3;

        a0 | b0 | a1 | b1
    }
}

pub(super) struct CrossResult {
    pub(super) point: IntPoint,
    pub(super) cross_type: CrossType,
    pub(super) is_round: bool,
}

pub(super) enum CrossType {
    Pure,
    TargetEnd,
    OtherEnd,
    Overlay,
}

pub(super) struct CrossSolver {
    phantom_data: PhantomData<i32>,
}

impl CrossSolver {
    pub(super) fn cross(target: &XSegment, other: &XSegment, radius: i64) -> Option<CrossResult> {
        let a0b0a1 = Triangle::clock_direction(target.a, target.b, other.a);
        let a0b0b1 = Triangle::clock_direction(target.a, target.b, other.b);

        let a1b1a0 = Triangle::clock_direction(other.a, other.b, target.a);
        let a1b1b0 = Triangle::clock_direction(other.a, other.b, target.b);

        let one = 1i64;

        let s = (one & (a0b0a1 + one))
            + (one & (a0b0b1 + one))
            + (one & (a1b1a0 + one))
            + (one & (a1b1b0 + one));

        if s == 4i64 {
            return Some(CrossResult {
                point: IntPoint::ZERO,
                cross_type: CrossType::Overlay,
                is_round: false,
            });
        }

        let is_not_cross = a0b0a1 == a0b0b1 || a1b1a0 == a1b1b0;

        if s > 1i64 || is_not_cross {
            return None;
        }

        if s != 0i64 {
            return if a0b0a1 == 0i64 {
                Some(CrossResult {
                    point: other.a,
                    cross_type: CrossType::OtherEnd,
                    is_round: false,
                })
            } else if a0b0b1 == 0i64 {
                Some(CrossResult {
                    point: other.b,
                    cross_type: CrossType::OtherEnd,
                    is_round: false,
                })
            } else if a1b1a0 == 0i64 {
                Some(CrossResult {
                    point: target.a,
                    cross_type: CrossType::TargetEnd,
                    is_round: false,
                })
            } else {
                Some(CrossResult {
                    point: target.b,
                    cross_type: CrossType::TargetEnd,
                    is_round: false,
                })
            };
        }

        Self::middle_cross(target, other, radius)
    }

    pub(super) fn collinear(target: &XSegment, other: &XSegment) -> CollinearMask {
        let a0 = target.a;
        let b0 = target.b;
        let a1 = other.a;
        let b1 = other.b;

        let v1 = b1 - a1;

        let aa0 = (a0 - a1).dot_product(v1).signum();
        let ab0 = (a0 - b1).dot_product(v1).signum();
        let ba0 = (b0 - a1).dot_product(v1).signum();
        let bb0 = (b0 - b1).dot_product(v1).signum();

        let aa1 = -aa0;
        let ab1 = -ba0;
        let ba1 = -ab0;
        let bb1 = -bb0;

        let is_target_a = aa0 == -ab0 && aa0 != 0i64;
        let is_target_b = ba0 == -bb0 && ba0 != 0i64;

        let is_other_a = aa1 == -ab1 && aa1 != 0i64;
        let is_other_b = ba1 == -bb1 && ba1 != 0i64;

        CollinearMask::new(is_target_a, is_target_b, is_other_a, is_other_b)
    }

    fn middle_cross(target: &XSegment, other: &XSegment, radius: i64) -> Option<CrossResult> {
        let p = CrossSolver::cross_point(target, other);

        if Triangle::is_line(target.a, p, target.b) && Triangle::is_line(other.a, p, other.b) {
            return Some(CrossResult {
                point: p,
                cross_type: CrossType::Pure,
                is_round: false,
            });
        }

        // still can be common ends because of rounding
        // snap to nearest end with r (1^2 + 1^2 == 2)

        let ra0 = target.a.sqr_distance(p);
        let rb0 = target.b.sqr_distance(p);

        let ra1 = other.a.sqr_distance(p);
        let rb1 = other.b.sqr_distance(p);

        if ra0 <= radius || ra1 <= radius || rb0 <= radius || rb1 <= radius {
            let r0 = ra0.min(rb0);
            let r1 = ra1.min(rb1);

            if r0 <= r1 {
                let p = if ra0 < rb0 { target.a } else { target.b };
                // ignore if it's a clean point
                if Triangle::is_not_line(other.a, p, other.b) {
                    return Some(CrossResult {
                        point: p,
                        cross_type: CrossType::TargetEnd,
                        is_round: true,
                    });
                }
            } else {
                let p = if ra1 < rb1 { other.a } else { other.b };

                // ignore if it's a clean point
                if Triangle::is_not_line(target.a, p, target.b) {
                    return Some(CrossResult {
                        point: p,
                        cross_type: CrossType::OtherEnd,
                        is_round: true,
                    });
                }
            }
        }

        Some(CrossResult {
            point: p,
            cross_type: CrossType::Pure,
            is_round: true,
        })
    }

    fn cross_point(target: &XSegment, other: &XSegment) -> IntPoint {
        // edges are not parallel
        // any abs(x) and abs(y) < 2^30
        // The result must be < 2^30

        // Classic approach:

        // let dxA = a0.x - a1.x
        // let dyB = b0.y - b1.y
        // let dyA = a0.y - a1.y
        // let dxB = b0.x - b1.x
        //
        // let xyA = a0.x * a1.y - a0.y * a1.x
        // let xyB = b0.x * b1.y - b0.y * b1.x
        //
        // overflow is possible!
        // let kx = xyA * dxB - dxA * xyB
        //
        // overflow is possible!
        // let ky = xyA * dyB - dyA * xyB
        //
        // let divider = dxA * dyB - dyA * dxB
        //
        // let x = kx / divider
        // let y = ky / divider
        //
        // return IntPoint(x, y)

        // offset approach
        // move all picture by -a0. Point a0 will be equal (0, 0)

        // move a0.x to 0
        // move all by a0.x
        let a0x = target.a.x.wide();
        let a0y = target.a.y.wide();

        let a1x = target.b.x.wide() - a0x;
        let b0x = other.a.x.wide() - a0x;
        let b1x = other.b.x.wide() - a0x;

        // move a0.y to 0
        // move all by a0.y
        let a1y = target.b.y.wide() - a0y;
        let b0y = other.a.y.wide() - a0y;
        let b1y = other.b.y.wide() - a0y;

        let dy_b = b0y - b1y;
        let dx_b = b0x - b1x;

        // let xyA = 0
        let xy_b = b0x * b1y - b0y * b1x;

        let x0: i64;
        let y0: i64;

        // a1y and a1x cannot be zero simultaneously, because we will get edge a0<>a1 zero length and it is impossible

        if a1x == 0i64 {
            // dxB is not zero because it will be parallel case and it's impossible
            x0 = 0i64;
            y0 = xy_b / dx_b;
        } else if a1y == 0i64 {
            // dyB is not zero because it will be parallel case and it's impossible
            y0 = 0i64;
            x0 = -xy_b / dy_b;
        } else {
            // divider
            let div = a1y * dx_b - a1x * dy_b;

            // calculate result sign
            let s = div.signum() * xy_b.signum();
            let sx = a1x.signum() * s;
            let sy = a1y.signum() * s;

            // use custom u128 bit math with rounding
            let uxy_b = xy_b.unsigned_abs();
            let udiv = div.unsigned_abs();

            let kx =
                crate::geometry::overlay::port::prim::product_multiply(a1x.unsigned_abs(), uxy_b);
            let ky =
                crate::geometry::overlay::port::prim::product_multiply(a1y.unsigned_abs(), uxy_b);

            let ux = kx.divide_with_rounding(udiv);
            let uy = ky.divide_with_rounding(udiv);

            x0 = sx * crate::geometry::overlay::port::prim::wide_from_uint(ux);
            y0 = sy * crate::geometry::overlay::port::prim::wide_from_uint(uy);
        }

        let x = crate::geometry::overlay::port::prim::i_from_wide(x0 + a0x);
        let y = crate::geometry::overlay::port::prim::i_from_wide(y0 + a0y);

        IntPoint::new(x, y)
    }
}

#[cfg(test)]
mod tests {
    use crate::geometry::overlay::port::geom::x_segment::XSegment;
    use crate::geometry::overlay::port::prim::IntPoint;
    use crate::geometry::overlay::port::split::cross_solver::{CrossSolver, CrossType};

    impl XSegment {
        fn new(a: IntPoint, b: IntPoint) -> Self {
            Self { a, b }
        }
    }

    #[test]
    fn test_simple_cross() {
        let s: i32 = 1024;

        let ea = XSegment::new(IntPoint::new(-s, 0), IntPoint::new(s, 0));
        let eb = XSegment::new(IntPoint::new(0, -s), IntPoint::new(0, s));

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Pure => {
                assert_eq!(IntPoint::ZERO, result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_big_cross_1() {
        let s: i32 = 1_024_000_000;

        let ea = XSegment::new(IntPoint::new(-s, 0), IntPoint::new(s, 0));
        let eb = XSegment::new(IntPoint::new(0, -s), IntPoint::new(0, s));

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Pure => {
                assert_eq!(IntPoint::ZERO, result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_big_cross_2() {
        let s: i32 = 1_024_000_000;

        let ea = XSegment::new(IntPoint::new(-s, 0), IntPoint::new(s, 0));
        let eb = XSegment::new(IntPoint::new(1024, -s), IntPoint::new(1024, s));

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Pure => {
                assert_eq!(IntPoint::new(1024, 0), result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_big_cross_3() {
        let s: i32 = 1_024_000_000;
        let q: i32 = s / 2;

        let ea = XSegment::new(IntPoint::new(-s, -s), IntPoint::new(s, s));
        let eb = XSegment::new(IntPoint::new(q, -s), IntPoint::new(q, s));

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Pure => {
                assert_eq!(IntPoint::new(512_000_000, 512_000_000), result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_left_end() {
        let s: i32 = 1_024_000_000;

        let ea = XSegment::new(IntPoint::new(-s, 0), IntPoint::new(s, 0));
        let eb = XSegment::new(IntPoint::new(-s, -s), IntPoint::new(-s, s));

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::TargetEnd => {
                assert_eq!(IntPoint::new(-s, 0), result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_right_end() {
        let s: i32 = 1_024_000_000;

        let ea = XSegment::new(IntPoint::new(-s, 0), IntPoint::new(s, 0));
        let eb = XSegment::new(IntPoint::new(s, -s), IntPoint::new(s, s));

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::TargetEnd => {
                assert_eq!(IntPoint::new(s, 0), result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_left_top() {
        let s: i32 = 1_024_000_000;

        let ea = XSegment::new(IntPoint::new(-s, s), IntPoint::new(s, s));
        let eb = XSegment::new(IntPoint::new(-s, s), IntPoint::new(-s, -s));

        let result = CrossSolver::cross(&ea, &eb, 2);
        debug_assert!(result.is_none());
    }

    #[test]
    fn test_real_case_1() {
        let ea = XSegment::new(IntPoint::new(7256, -14637), IntPoint::new(7454, -15045));
        let eb = XSegment::new(IntPoint::new(7343, -14833), IntPoint::new(7506, -15144));

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Pure => {}
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_real_case_2() {
        let ea = XSegment::new(
            IntPoint::new(-8555798, -1599355),
            IntPoint::new(-1024000, 0),
        );
        let eb = XSegment::new(
            IntPoint::new(-8571363, 1513719),
            IntPoint::new(-1023948, -10239),
        );

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Pure => {
                assert_eq!(IntPoint::new(-1048691, -5243), result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_real_case_3() {
        let ea = XSegment::new(
            IntPoint::new(-8555798, -1599355),
            IntPoint::new(513224, -5243),
        );
        let eb = XSegment::new(
            IntPoint::new(-8555798, -1599355),
            IntPoint::new(513224, -5243),
        );

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Overlay => {
                assert_eq!(IntPoint::ZERO, result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_real_case_4() {
        let ea = XSegment::new(
            IntPoint::new(-276659431, 380789039),
            IntPoint::new(-221915258, 435533212),
        );
        let eb = XSegment::new(
            IntPoint::new(-276659432, 380789038),
            IntPoint::new(-276659430, 380789040),
        );

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Overlay => {
                assert_eq!(IntPoint::ZERO, result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }

    #[test]
    fn test_penetration() {
        let s: i32 = 1024;

        let ea = XSegment::new(IntPoint::new(-s, 0), IntPoint::new(s / 2, 0));
        let eb = XSegment::new(IntPoint::new(0, 0), IntPoint::new(s, 0));

        let result = CrossSolver::cross(&ea, &eb, 2).expect("segments must cross");

        match result.cross_type {
            CrossType::Overlay => {
                assert_eq!(IntPoint::ZERO, result.point);
            }
            _ => {
                panic!("Fail cross result");
            }
        }
    }
}
