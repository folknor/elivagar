//! Concrete i32 geometric primitives, monomorphized from i_float 3.0.0.
//!
//! The upstream crate is generic over an `IntNumber` coordinate trait
//! (i16/i32/i64). The overlay engine only ever instantiates it at i32, so this
//! module fixes the coordinate type to i32, its wide product to i64, and the
//! unsigned wide to u64 - deleting the trait machinery, the float adapters, and
//! the vector/mesh surfaces entirely. Arithmetic is byte-for-byte the same as
//! the generic code at `I = i32`; the pristine reference lives in
//! `research/i_float`.

use core::cmp::Ordering;
use core::{fmt, ops};

/// i32 coordinate helpers matching `IntNumber` at `I = i32`
/// (`Wide = i64`, `WideUInt = u64`). Kept as an extension trait so the
/// engine's `.wide()` / `.to_uint()` call sites port over unchanged.
pub(crate) trait IntCoord {
    fn wide(self) -> i64;
    fn to_uint(self) -> u64;
}

impl IntCoord for i32 {
    #[inline(always)]
    fn wide(self) -> i64 {
        self as i64
    }
    #[inline(always)]
    fn to_uint(self) -> u64 {
        self as u64
    }
}

/// Wide (i64) helpers matching `WideIntNumber` at `Wide = i64`.
pub(crate) trait WideCoord {
    fn to_usize(self) -> usize;
}

impl WideCoord for i64 {
    #[inline(always)]
    fn to_usize(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct IntPoint {
    pub x: i32,
    pub y: i32,
}

impl IntPoint {
    pub const ZERO: Self = Self { x: 0, y: 0 };

    #[inline(always)]
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    #[inline(always)]
    pub fn sqr_distance(self, other: Self) -> i64 {
        (self - other).sqr_length()
    }
}

impl fmt::Display for IntPoint {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "[{}, {}]", self.x, self.y)
    }
}

impl From<[i32; 2]> for IntPoint {
    #[inline(always)]
    fn from(value: [i32; 2]) -> Self {
        IntPoint::new(value[0], value[1])
    }
}

impl From<(i32, i32)> for IntPoint {
    #[inline(always)]
    fn from(value: (i32, i32)) -> Self {
        IntPoint::new(value.0, value.1)
    }
}

impl PartialOrd for IntPoint {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IntPoint {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> Ordering {
        let x = self.x == other.x;
        if x && self.y == other.y {
            Ordering::Equal
        } else if self.x < other.x || x && self.y < other.y {
            Ordering::Less
        } else {
            Ordering::Greater
        }
    }
}

impl ops::Add for IntPoint {
    type Output = Self;

    #[inline(always)]
    fn add(self, other: Self) -> Self {
        IntPoint {
            x: self.x + other.x,
            y: self.y + other.y,
        }
    }
}

impl ops::Sub for IntPoint {
    type Output = IntVector;

    #[inline(always)]
    fn sub(self, other: Self) -> Self::Output {
        IntVector {
            x: (self.x as i64) - (other.x as i64),
            y: (self.y as i64) - (other.y as i64),
        }
    }
}

/// The wide (i64-component) difference vector, `IntVector<i32>` upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct IntVector {
    pub x: i64,
    pub y: i64,
}

impl IntVector {
    #[inline(always)]
    pub fn new(x: i64, y: i64) -> Self {
        Self { x, y }
    }

    #[inline(always)]
    pub fn cross_product(self, v: Self) -> i64 {
        let a = self.x * v.y;
        let b = self.y * v.x;
        a - b
    }

    #[inline(always)]
    pub fn dot_product(self, v: Self) -> i64 {
        let xx = self.x * v.x;
        let yy = self.y * v.y;
        xx + yy
    }

    #[inline(always)]
    pub fn sqr_length(self) -> i64 {
        self.x * self.x + self.y * self.y
    }
}

impl fmt::Display for IntVector {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "[{}, {}]", self.x, self.y)
    }
}

pub(crate) struct Triangle;

impl Triangle {
    #[inline(always)]
    pub fn area_two(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> i64 {
        (p1 - p0).cross_product(p2 - p0)
    }

    #[inline(always)]
    pub fn is_clockwise(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> bool {
        Self::area_two(p0, p1, p2) < 0
    }

    #[inline(always)]
    pub fn is_cw_or_line(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> bool {
        Self::area_two(p0, p1, p2) <= 0
    }

    #[inline(always)]
    pub fn is_not_line(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> bool {
        Self::area_two(p0, p1, p2) != 0
    }

    #[inline(always)]
    pub fn is_line(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> bool {
        Self::area_two(p0, p1, p2) == 0
    }

    #[inline(always)]
    pub fn clock_direction(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> i64 {
        Self::area_two(p0, p2, p1).signum()
    }

    #[inline(always)]
    pub fn clock_order(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> Ordering {
        Self::area_two(p0, p1, p2).cmp(&0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IntRect {
    pub min_x: i32,
    pub max_x: i32,
    pub min_y: i32,
    pub max_y: i32,
}

impl IntRect {
    #[inline(always)]
    pub fn contains_with_radius(&self, point: IntPoint, radius: i32) -> bool {
        let min_x = self.min_x - radius;
        let max_x = self.max_x + radius;
        let min_y = self.min_y - radius;
        let max_y = self.max_y + radius;
        min_x <= point.x && point.x <= max_x && min_y <= point.y && point.y <= max_y
    }

    #[inline(always)]
    pub fn is_intersect_border_include(&self, other: &Self) -> bool {
        let x = self.min_x <= other.max_x && self.max_x >= other.min_x;
        let y = self.min_y <= other.max_y && self.max_y >= other.min_y;
        x && y
    }
}

/// Rounded 128-bit multiply-then-divide, monomorphized from i_float's
/// `UIntProduct::multiply(a, b).divide_with_rounding(divisor)` at the
/// engine's only instantiation (`WideUInt = u64`). The composite 64x64->128
/// product upstream computes bit-for-bit the same value as native u128
/// arithmetic (proven by i_float's own `test_composite_divide_matches_u128`),
/// so the cross solver's snap rounding is preserved exactly.
///
/// Preconditions (upstream `debug_assert`s): `0 < divisor < 2^63`, and the
/// rounded quotient fits in u64.
#[inline(always)]
pub(crate) fn mul_div_round(a: u64, b: u64, divisor: u64) -> u64 {
    product_multiply(a, b).divide_with_rounding(divisor)
}

/// The 128-bit product of two u64 operands, `UIntProduct<u64>` upstream. For
/// `WideUInt = u64` the composite 64x64->128 product is exactly native u128
/// multiplication (i_float `test_composite_divide_matches_u128`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Product(u128);

#[inline(always)]
pub(crate) fn product_multiply(a: u64, b: u64) -> Product {
    Product((a as u128) * (b as u128))
}

impl Product {
    /// Divide by `divisor` rounding to nearest, `0 < divisor < 2^63`.
    #[inline(always)]
    pub(crate) fn divide_with_rounding(self, divisor: u64) -> u64 {
        debug_assert!(divisor > 0);
        debug_assert!(divisor < (1u64 << 63));
        let divisor = divisor as u128;
        let result = self.0 / divisor;
        let remainder = self.0 - result * divisor;
        let half = (divisor >> 1) + (divisor & 1);
        if remainder >= half {
            (result + 1) as u64
        } else {
            result as u64
        }
    }
}

/// `IntNumber::from_wide` at `I = i32`: narrow an i64 back to i32.
#[inline(always)]
pub(crate) fn i_from_wide(value: i64) -> i32 {
    value as i32
}

/// `IntNumber::from_usize` at `I = i32`.
#[inline(always)]
pub(crate) fn i_from_usize(value: usize) -> i32 {
    value as i32
}

/// `IntNumber::from_uint` at `I = i32` (`WideUInt = u64`).
#[inline(always)]
pub(crate) fn i_from_uint(value: u64) -> i32 {
    value as i32
}

/// `WideIntNumber::from_uint` at `Wide = i64` (`WideUInt = u64`).
#[inline(always)]
pub(crate) fn wide_from_uint(value: u64) -> i64 {
    value as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_order() {
        assert_eq!(IntPoint::new(0, 0), IntPoint::new(0, 0));
        assert!(IntPoint::new(0, 0) < IntPoint::new(0, 4));
        assert!(IntPoint::new(1, 0) > IntPoint::new(0, 4));
    }

    #[test]
    fn sub_returns_wide_vector() {
        let v = IntPoint::new(i32::MIN, i32::MIN) - IntPoint::new(i32::MAX, i32::MAX);
        assert_eq!(v.x, i32::MIN as i64 - i32::MAX as i64);
        assert_eq!(v.y, i32::MIN as i64 - i32::MAX as i64);
    }

    #[test]
    fn mul_div_round_matches_reference() {
        // Mirrors i_float's u128-reference test for the composite product.
        let mut state = 0x4d59_5df4_d0f3_3173u64;
        let mut next = || {
            state ^= state << 7;
            state ^= state >> 9;
            state ^= state << 8;
            state
        };
        for _ in 0..10_000 {
            let a = next() & ((1u64 << 63) - 1);
            let b = next() & ((1u64 << 63) - 1);
            let product = (a as u128) * (b as u128);
            let high = (product >> 64) as u64;
            let divisor = high + (next() & 0xffff) + 1;
            let reference = {
                let d = divisor as u128;
                let q = product / d;
                let r = product - q * d;
                if r >= ((d >> 1) + (d & 1)) {
                    (q + 1) as u64
                } else {
                    q as u64
                }
            };
            assert_eq!(mul_div_round(a, b, divisor), reference);
        }
    }
}
