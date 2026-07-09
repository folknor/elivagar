//! Concrete i32 shape helpers, monomorphized from i_shape 3.0.0.
//!
//! Upstream is generic over the `IntNumber` coordinate; the overlay engine only
//! uses i32, so the contour/shape aliases, the `ContourExtension` predicates,
//! the `Simplify` collinear-cleanup path, area/points-count, the `Reserve`
//! helper, and the flat contour buffer are all fixed at i32 here. Behaviour is
//! byte-for-byte the generic code at `I = i32`; the reference lives in
//! `research/i_shape`.

use crate::geometry::overlay::port::prim::IntPoint;
use alloc::vec::Vec;
use core::ops::Range;

pub(crate) type IntContour = Vec<IntPoint>;
pub(crate) type IntShape = Vec<IntContour>;
pub(crate) type IntShapes = Vec<IntShape>;
pub(crate) type IntPath = Vec<IntPoint>;

/// Build an `IntShape` from nested `[x, y]` literals (i_shape's `int_shape!`).
#[allow(unused_macros)]
macro_rules! int_shape {
    ($([$([$x:expr, $y:expr]),* $(,)?]),* $(,)?) => {
        alloc::vec![$(
            alloc::vec![$(
                $crate::geometry::overlay::port::prim::IntPoint::new($x, $y)
            ),*]
        ),*]
    };
}
pub(crate) use int_shape;

/// Grows a `Vec` toward `new_capacity` without ever shrinking (i_shape's
/// `Reserve`).
pub(crate) trait Reserve {
    fn reserve_capacity(&mut self, new_capacity: usize);
}

impl<T> Reserve for Vec<T> {
    #[inline]
    fn reserve_capacity(&mut self, new_capacity: usize) {
        let old_capacity = self.capacity();
        if old_capacity < new_capacity {
            self.reserve(new_capacity - old_capacity);
        }
    }
}

pub(crate) trait PointsCount {
    fn points_count(&self) -> usize;
}

impl PointsCount for [IntContour] {
    #[inline(always)]
    fn points_count(&self) -> usize {
        self.iter().fold(0, |acc, path| acc + path.len())
    }
}

impl PointsCount for [IntShape] {
    #[inline(always)]
    fn points_count(&self) -> usize {
        self.iter().fold(0, |acc, shape| acc + shape.points_count())
    }
}

pub(crate) trait Area {
    fn area_two(&self) -> i64;
    fn area(&self) -> i64;
}

impl Area for [IntPoint] {
    #[inline]
    fn area_two(&self) -> i64 {
        self.unsafe_area()
    }

    #[inline]
    fn area(&self) -> i64 {
        self.area_two() / 2
    }
}

impl Area for [IntContour] {
    #[inline]
    fn area_two(&self) -> i64 {
        let mut s = 0i64;
        for path in self.iter() {
            s = s.wrapping_add(path.area_two());
        }
        s
    }

    #[inline]
    fn area(&self) -> i64 {
        self.area_two() / 2
    }
}

impl Area for [IntShape] {
    #[inline]
    fn area_two(&self) -> i64 {
        let mut s = 0i64;
        for shape in self.iter() {
            s = s.wrapping_add(shape.area_two());
        }
        s
    }

    #[inline]
    fn area(&self) -> i64 {
        self.area_two() / 2
    }
}

pub(crate) trait ContourExtension {
    fn unsafe_area(&self) -> i64;
    fn is_clockwise_ordered(&self) -> bool;
    fn contains(&self, point: IntPoint) -> bool;
}

impl ContourExtension for [IntPoint] {
    /// Positive double area if counter-clockwise, negative otherwise.
    fn unsafe_area(&self) -> i64 {
        let n = self.len();
        let mut p0 = self[n - 1];
        let mut area = 0i64;
        for &p1 in self.iter() {
            let a = (p0.x as i64).wrapping_mul(p1.y as i64);
            let b = (p0.y as i64).wrapping_mul(p1.x as i64);
            area = area.wrapping_add(a).wrapping_sub(b);
            p0 = p1;
        }
        area
    }

    #[inline(always)]
    fn is_clockwise_ordered(&self) -> bool {
        self.unsafe_area() <= 0
    }

    fn contains(&self, point: IntPoint) -> bool {
        let n = self.len();
        let mut is_contain = false;
        let mut b = self[n - 1];
        for &a in self.iter() {
            let is_in_range = (a.y > point.y) != (b.y > point.y);
            if is_in_range {
                let dx = b.x - a.x;
                let dy = b.y - a.y;
                let sx = (point.y - a.y) * dx / dy + a.x;
                if point.x < sx {
                    is_contain = !is_contain;
                }
            }
            b = a;
        }
        is_contain
    }
}

/// In-place collinear simplification (i_shape's `Simplify` for a contour).
pub(crate) trait Simplify {
    /// `true` if the contour was modified, `false` if it was already simple.
    fn simplify_contour(&mut self) -> bool;
}

impl Simplify for IntContour {
    #[inline]
    fn simplify_contour(&mut self) -> bool {
        if self.is_simple() {
            return false;
        }
        if let Some(contour) = self.simplified() {
            self.clear();
            self.extend(contour);
        } else {
            self.clear();
        }
        true
    }
}

trait SimpleContour {
    fn is_simple(&self) -> bool;
    fn simplified(&self) -> Option<IntContour>;
}

impl SimpleContour for [IntPoint] {
    fn is_simple(&self) -> bool {
        let count = self.len();
        if count < 3 {
            return false;
        }
        let mut p0 = self[count - 2];
        let p1 = self[count - 1];
        let mut v0 = p1 - p0;
        p0 = p1;
        for &pi in self.iter() {
            let vi = pi - p0;
            if vi.cross_product(v0) == 0 {
                return false;
            }
            v0 = vi;
            p0 = pi;
        }
        true
    }

    #[inline]
    fn simplified(&self) -> Option<IntContour> {
        ContourSimplifier::default().simplify_contour(self)
    }
}

#[derive(Default)]
struct ContourSimplifier {
    nodes: Vec<SimplifyNode>,
    validated: Vec<bool>,
}

impl ContourSimplifier {
    fn simplify_contour(&mut self, contour: &[IntPoint]) -> Option<IntContour> {
        let mut n = contour.len();
        if n < 3 {
            return None;
        }

        self.validated.clear();
        self.validated.resize(n, false);

        self.nodes.clear();
        self.nodes.reserve(n);

        let mut prev = n - 1;
        let mut next = 1;
        let last = n - 1;
        #[allow(clippy::explicit_counter_loop)]
        for index in 0..last {
            self.nodes.push(SimplifyNode { next, index, prev });
            prev = index;
            next += 1;
        }
        self.nodes.push(SimplifyNode {
            next: 0,
            index: last,
            prev,
        });

        let mut first: usize = 0;
        let mut node = self.nodes[first];
        let mut i = 0;
        while i < n {
            if self.validated[node.index] {
                node = self.nodes[node.next];
                continue;
            }

            let p0 = contour[node.prev];
            let p1 = contour[node.index];
            let p2 = contour[node.next];

            if (p1 - p0).cross_product(p2 - p1) == 0 {
                n -= 1;
                if n < 3 {
                    return None;
                }

                self.nodes[node.prev].next = node.next;
                self.nodes[node.next].prev = node.prev;

                if node.index == first {
                    first = node.next;
                }

                node = self.nodes[node.prev];

                if self.validated[node.prev] {
                    i -= 1;
                    self.validated[node.prev] = false;
                }
                if self.validated[node.next] {
                    i -= 1;
                    self.validated[node.next] = false;
                }
                if self.validated[node.index] {
                    i -= 1;
                    self.validated[node.index] = false;
                }
            } else {
                self.validated[node.index] = true;
                i += 1;
                node = self.nodes[node.next];
            }
        }

        let mut buffer = alloc::vec![IntPoint::ZERO; n];
        node = self.nodes[first];
        for item in buffer.iter_mut().take(n) {
            *item = contour[node.index];
            node = self.nodes[node.next];
        }

        Some(buffer)
    }
}

#[derive(Clone, Copy)]
struct SimplifyNode {
    next: usize,
    index: usize,
    prev: usize,
}

/// Flat point-plus-ranges contour store (i_shape's `FlatContoursBuffer`),
/// fixed to i32. Used as the extraction scratch inside the engine.
#[derive(Debug, Clone, Default)]
pub(crate) struct FlatContoursBuffer {
    pub points: Vec<IntPoint>,
    pub ranges: Vec<Range<usize>>,
}

impl FlatContoursBuffer {
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            points: Vec::with_capacity(capacity),
            ranges: Vec::new(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    #[inline]
    pub fn is_single_contour(&self) -> bool {
        self.ranges.len() == 1
    }

    #[inline]
    pub fn as_first_contour(&self) -> &[IntPoint] {
        if let Some(first_contour_range) = self.ranges.first() {
            &self.points[first_contour_range.clone()]
        } else {
            &self.points
        }
    }

    #[inline]
    pub fn as_first_contour_mut(&mut self) -> &mut [IntPoint] {
        if let Some(first_contour_range) = self.ranges.first() {
            &mut self.points[first_contour_range.clone()]
        } else {
            &mut self.points
        }
    }

    #[inline]
    pub fn clear_and_reserve(&mut self, points: usize, contours: usize) {
        self.points.reserve_capacity(points);
        self.points.clear();
        self.ranges.reserve_capacity(contours);
        self.ranges.clear();
    }

    #[inline]
    pub fn add_contour(&mut self, contour: &[IntPoint]) {
        let start = self.points.len();
        let end = start + contour.len();
        self.ranges.push(start..end);
        self.points.extend_from_slice(contour);
    }
}
