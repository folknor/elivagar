//! Concrete i32 shape helpers, monomorphized from i_shape 3.0.0.
//!
//! Upstream is generic over the `IntNumber` coordinate; the overlay engine only
//! uses i32, so the contour/shape aliases, the `ContourExtension` predicates,
//! the `Simplify` collinear-cleanup path, and the `Reserve` helper are all
//! fixed at i32 here. Behaviour is byte-for-byte the generic code at
//! `I = i32`; the reference lives in `research/i_shape`.

use crate::geometry::overlay::port::prim::IntPoint;
use alloc::vec::Vec;

pub(crate) type IntContour = Vec<IntPoint>;
pub(crate) type IntShape = Vec<IntContour>;
pub(crate) type IntShapes = Vec<IntShape>;
pub(crate) type IntPath = Vec<IntPoint>;

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

pub(crate) trait ContourExtension {
    fn unsafe_area(&self) -> i64;
    fn is_clockwise_ordered(&self) -> bool;
}

impl ContourExtension for [IntPoint] {
    /// Positive double area if counter-clockwise, negative otherwise.
    fn unsafe_area(&self) -> i64 {
        let n = self.len();
        let mut p0 = self[n - 1];
        let mut area = 0i64;
        for &p1 in self {
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
        for &pi in self {
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
