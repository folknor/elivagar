//! Concrete i32 shape helpers, monomorphized from i_shape 3.0.0.
//!
//! Upstream is generic over the `IntNumber` coordinate; the overlay engine only
//! uses i32, so the contour/shape aliases, the `ContourExtension` predicates,
//! the `Simplify` collinear-cleanup path, and the `Reserve` helper are all
//! fixed at i32 here. Behaviour is byte-for-byte the generic code at
//! `I = i32`; the reference lives in `research/i_shape`.

use crate::geometry::overlay::port::ContourDirection;
use crate::geometry::overlay::port::graph::OverlayGraph;
use crate::geometry::overlay::port::graph::OverlayLink;
use crate::geometry::overlay::port::graph::OverlayLinkFilter;
use crate::geometry::overlay::port::point::IntPoint;
use crate::geometry::overlay::port::point::IntVector;
use crate::geometry::overlay::port::point::Triangle;
use crate::geometry::overlay::port::scan::{KeyExpCollection, KeyExpList, KeyExpTree};
use crate::geometry::overlay::port::segment::{BOTH_TOP, SUBJ_TOP, SegmentFill};
use crate::geometry::overlay::port::segment::{BottomSegment, VSegment};
use crate::geometry::overlay::port::sort::TwoKeysAndCmpSort;
use alloc::vec::Vec;
use core::cmp::Ordering;

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
    fn simplify_contour(&mut self, scratch: &mut ContourSimplifier) -> bool;
}

impl Simplify for IntContour {
    #[inline]
    fn simplify_contour(&mut self, scratch: &mut ContourSimplifier) -> bool {
        if self.is_simple() {
            return false;
        }
        if !scratch.simplify_contour(self) {
            self.clear();
        }
        true
    }
}

trait SimpleContour {
    fn is_simple(&self) -> bool;
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
}

#[derive(Default)]
pub(crate) struct ContourSimplifier {
    nodes: Vec<SimplifyNode>,
    validated: Vec<bool>,
    output: IntContour,
}

impl ContourSimplifier {
    fn simplify_contour(&mut self, contour: &mut IntContour) -> bool {
        let mut n = contour.len();
        if n < 3 {
            return false;
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
                    return false;
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

        self.output.clear();
        self.output.reserve_capacity(n);
        node = self.nodes[first];
        for _ in 0..n {
            self.output.push(contour[node.index]);
            node = self.nodes[node.next];
        }
        contour.clear();
        contour.extend_from_slice(&self.output);

        true
    }
}

#[derive(Clone, Copy)]
struct SimplifyNode {
    next: usize,
    index: usize,
    prev: usize,
}

/// The boolean operations the two-op engine supports. i_overlay's Clip / Union
/// / Difference / InverseDifference / Xor are pruned - the engine is only ever
/// driven at Subject (self-union) and Intersect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OverlayRule {
    Subject,
    Intersect,
}

impl OverlayRule {
    #[inline(always)]
    pub(crate) fn is_fill_top(&self, fill: SegmentFill) -> bool {
        match self {
            OverlayRule::Subject => fill & SUBJ_TOP == SUBJ_TOP,
            OverlayRule::Intersect => fill & BOTH_TOP == BOTH_TOP,
        }
    }
}

pub(crate) struct NearestVector {
    c: IntPoint,       // center
    va: IntVector,     // our target vector
    vb: IntVector,     // nearest vector to Va by specified rotation
    ab_more_180: bool, // is angle between Va and Vb more than 180 degrees
    pub(crate) best_id: usize,
    rotation_factor: i64, // +1 for clockwise, -1 for counterclockwise
}

impl NearestVector {
    #[inline]
    pub(crate) fn new(
        c: IntPoint,
        a: IntPoint,
        b: IntPoint,
        best_id: usize,
        clockwise: bool,
    ) -> Self {
        let va = a - c;
        let vb = b - c;
        let (ab_more_180, rotation_factor) = if clockwise {
            (va.cross_product(vb) >= 0i64, 1i64)
        } else {
            (va.cross_product(vb) <= 0i64, -1i64)
        };
        Self {
            c,
            va,
            vb,
            ab_more_180,
            best_id,
            rotation_factor,
        }
    }

    #[inline]
    pub(crate) fn add(&mut self, p: IntPoint, id: usize) {
        let vp = p - self.c;
        let ap_more_180 = self.va.cross_product(vp) * self.rotation_factor >= 0i64;

        if self.ab_more_180 == ap_more_180 {
            if vp.cross_product(self.vb) * self.rotation_factor < 0i64 {
                self.vb = vp;
                self.best_id = id;
            }
        } else if self.ab_more_180 {
            self.ab_more_180 = false;
            self.vb = vp;
            self.best_id = id;
        }
    }
}

#[cfg(test)]
mod core_nearest_vector_tests {
    use crate::geometry::overlay::port::extract::NearestVector;
    use crate::geometry::overlay::port::point::IntPoint;
    use crate::geometry::overlay::port::point::IntVector;

    #[test]
    fn test_nearest_ccw_vector_creation() {
        let c = IntPoint::new(0, 0);
        let a = IntPoint::new(1, 0);
        let b = IntPoint::new(0, 1);

        let nearest_ccw = NearestVector::new(c, a, b, 0, false);

        assert_eq!(nearest_ccw.va, IntVector::new(1, 0));
        assert_eq!(nearest_ccw.vb, IntVector::new(0, 1));
        assert!(!nearest_ccw.ab_more_180);
    }

    #[test]
    fn test_nearest_ccw_vector_add_less_than_180() {
        let c = IntPoint::new(0, 0);
        let a = IntPoint::new(1, 0);
        let b = IntPoint::new(0, 1);

        let mut nearest_ccw = NearestVector::new(c, a, b, 0, false);
        let p = IntPoint::new(-1, 0);

        nearest_ccw.add(p, 1);
        assert_eq!(nearest_ccw.vb, IntVector::new(0, 1));
        assert!(!nearest_ccw.ab_more_180);
    }

    #[test]
    fn test_nearest_ccw_vector_add_more_than_180() {
        let c = IntPoint::new(0, 0);
        let a = IntPoint::new(1, 0);
        let b = IntPoint::new(-1, 0);

        let mut nearest_ccw = NearestVector::new(c, a, b, 0, false);
        let p = IntPoint::new(0, 1);
        nearest_ccw.add(p, 1);
        assert_eq!(nearest_ccw.vb, IntVector::new(0, 1));
        assert!(!nearest_ccw.ab_more_180);
    }

    #[test]
    fn test_nearest_ccw_vector_no_update() {
        let c = IntPoint::new(0, 0);
        let a = IntPoint::new(1, 0);
        let b = IntPoint::new(0, 1);

        let mut nearest_ccw = NearestVector::new(c, a, b, 0, false);
        let p = IntPoint::new(1, 1);

        nearest_ccw.add(p, 1);

        assert_eq!(nearest_ccw.vb, IntVector::new(1, 1));
    }

    #[test]
    fn test_ccw_0() {
        let c = IntPoint::new(-1, -1);
        let a = IntPoint::new(0, -1);
        let b = IntPoint::new(-2, -1);

        let mut nearest_ccw = NearestVector::new(c, a, b, 1, false);
        let p = IntPoint::new(-1, -2);

        nearest_ccw.add(p, 3);

        assert_eq!(nearest_ccw.best_id, 1);
    }

    #[test]
    fn test_nearest_cw_vector_creation() {
        let c = IntPoint::new(0, 0);
        let a = IntPoint::new(1, 0);
        let b = IntPoint::new(0, -1);

        let nearest_cw = NearestVector::new(c, a, b, 0, true);

        assert_eq!(nearest_cw.va, IntVector::new(1, 0));
        assert_eq!(nearest_cw.vb, IntVector::new(0, -1));
        assert!(!nearest_cw.ab_more_180);
    }

    #[test]
    fn test_nearest_cw_vector_add_less_than_180() {
        let c = IntPoint::new(0, 0);
        let a = IntPoint::new(1, 0);
        let b = IntPoint::new(0, -1);

        let mut nearest_cw = NearestVector::new(c, a, b, 0, true);
        let p = IntPoint::new(-1, 0);

        nearest_cw.add(p, 1);
        assert_eq!(nearest_cw.vb, IntVector::new(0, -1));
        assert!(!nearest_cw.ab_more_180);
    }

    #[test]
    fn test_nearest_cw_vector_add_more_than_180() {
        let c = IntPoint::new(0, 0);
        let a = IntPoint::new(1, 0);
        let b = IntPoint::new(-1, 0);

        let mut nearest_cw = NearestVector::new(c, a, b, 0, true);
        let p = IntPoint::new(0, -1);
        nearest_cw.add(p, 1);
        assert_eq!(nearest_cw.vb, IntVector::new(0, -1));
        assert!(!nearest_cw.ab_more_180);
    }

    #[test]
    fn test_nearest_cw_vector_no_update() {
        let c = IntPoint::new(0, 0);
        let a = IntPoint::new(1, 0);
        let b = IntPoint::new(0, -1);

        let mut nearest_cw = NearestVector::new(c, a, b, 0, true);
        let p = IntPoint::new(1, 1);

        nearest_cw.add(p, 1);

        assert_eq!(nearest_cw.vb, IntVector::new(0, -1));
    }

    #[test]
    fn test_cw_0() {
        let c = IntPoint::new(-1, -1);
        let a = IntPoint::new(0, -1);
        let b = IntPoint::new(-2, -1);

        let mut nearest_cw = NearestVector::new(c, a, b, 1, true);
        let p = IntPoint::new(-1, -2);

        nearest_cw.add(p, 3);

        assert_eq!(nearest_cw.best_id, 3);
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContourIndex {
    data: usize,
}

impl ContourIndex {
    pub(crate) const EMPTY: ContourIndex = ContourIndex { data: usize::MAX };

    #[inline]
    pub(crate) fn is_hole(&self) -> bool {
        self.data & 1 == 1
    }

    #[inline]
    pub(crate) fn index(&self) -> usize {
        self.data >> 1
    }

    #[inline]
    pub(crate) fn new_hole(index: usize) -> Self {
        Self {
            data: (index << 1) | 1,
        }
    }

    #[inline]
    pub(crate) fn new_shape(index: usize) -> Self {
        Self { data: index << 1 }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct IdSegment {
    pub(crate) contour_index: ContourIndex,
    pub(crate) v_segment: VSegment,
}

impl IdSegment {
    #[inline]
    fn new(data: ContourIndex, a: IntPoint, b: IntPoint) -> Self {
        Self {
            contour_index: data,
            v_segment: VSegment { a, b },
        }
    }

    #[inline]
    pub(crate) fn with_segment(data: ContourIndex, v_segment: VSegment) -> Self {
        Self {
            contour_index: data,
            v_segment,
        }
    }
}

pub(crate) trait IdSegments {
    fn append_id_segments(
        &self,
        buffer: &mut Vec<IdSegment>,
        id_data: ContourIndex,
        x_min: i32,
        x_max: i32,
        clockwise: bool,
    );
}

impl IdSegments for IntPath {
    #[inline]
    fn append_id_segments(
        &self,
        buffer: &mut Vec<IdSegment>,
        id_data: ContourIndex,
        x_min: i32,
        x_max: i32,
        clockwise: bool,
    ) {
        fn inner<It: Iterator<Item = IntPoint>>(
            mut iter: It,
            buffer: &mut Vec<IdSegment>,
            id_data: ContourIndex,
            x_min: i32,
            x_max: i32,
        ) {
            let Some(first) = iter.next() else {
                return;
            };
            let mut b = first;
            for a in iter {
                if a.x < b.x && x_min < b.x && a.x <= x_max {
                    buffer.push(IdSegment::new(id_data, a, b));
                }
                b = a;
            }
            let a = first;
            if a.x < b.x && x_min < b.x && a.x <= x_max {
                buffer.push(IdSegment::new(id_data, a, b));
            }
        }

        if clockwise {
            inner(self.iter().copied(), buffer, id_data, x_min, x_max);
        } else {
            inner(self.iter().rev().copied(), buffer, id_data, x_min, x_max);
        }
    }
}

#[derive(Default)]
pub(crate) struct BinderScratch {
    pub(crate) segments: Vec<IdSegment>,
    pub(crate) parent_for_child: Vec<usize>,
    pub(crate) children_count_for_parent: Vec<usize>,
    sort_buffer: Vec<IdSegment>,
    scan_list: Option<KeyExpList<ContourIndex>>,
    scan_tree: Option<KeyExpTree<ContourIndex>>,
}

impl BinderScratch {
    #[inline]
    fn prepare(&mut self, children_count: usize, shape_count: usize) {
        self.parent_for_child.clear();
        #[cfg(debug_assertions)]
        self.parent_for_child.resize(children_count, usize::MAX);
        #[cfg(not(debug_assertions))]
        self.parent_for_child.resize(children_count, 0);

        self.children_count_for_parent.clear();
        self.children_count_for_parent.resize(shape_count, 0);
    }

    #[inline]
    fn take_scan_list(&mut self, capacity: usize) -> KeyExpList<ContourIndex> {
        if let Some(mut list) = self.scan_list.take() {
            list.clear();
            list.reserve_capacity(capacity);
            list
        } else {
            KeyExpList::new(capacity)
        }
    }

    #[inline]
    fn take_scan_tree(&mut self, capacity: usize) -> KeyExpTree<ContourIndex> {
        if let Some(mut tree) = self.scan_tree.take() {
            tree.clear();
            tree.reserve_capacity(capacity);
            tree
        } else {
            KeyExpTree::new(capacity)
        }
    }
}

pub(crate) struct ShapeBinder;

impl ShapeBinder {
    #[inline]
    pub(crate) fn bind(
        shape_count: usize,
        hole_segments: &[IdSegment],
        segments: &[IdSegment],
        scratch: &mut BinderScratch,
    ) {
        if shape_count < 32 {
            let capacity = segments.len().log2_sqrt().max(4) * 2;
            let mut list = scratch.take_scan_list(capacity);
            Self::private_solve::<KeyExpList<ContourIndex>>(
                &mut list,
                shape_count,
                hole_segments,
                segments,
                scratch,
            );
            scratch.scan_list = Some(list);
        } else {
            let capacity = segments.len().log2_sqrt().max(8);
            let mut tree = scratch.take_scan_tree(capacity);
            Self::private_solve::<KeyExpTree<ContourIndex>>(
                &mut tree,
                shape_count,
                hole_segments,
                segments,
                scratch,
            );
            scratch.scan_tree = Some(tree);
        }
    }

    fn private_solve<S>(
        scan_list: &mut S,
        shape_count: usize,
        anchors: &[IdSegment],
        segments: &[IdSegment],
        scratch: &mut BinderScratch,
    ) where
        S: KeyExpCollection<ContourIndex>,
    {
        let children_count = anchors.len();
        scratch.prepare(children_count, shape_count);

        let mut j = 0;

        for anchor in anchors {
            let p = anchor.v_segment.a;

            while j < segments.len() {
                let id_segment = &segments[j];
                if id_segment.cmp_by_a_then_by_angle(anchor) == Ordering::Greater {
                    break;
                }

                if id_segment.v_segment.b.x > p.x {
                    scan_list.insert(id_segment.v_segment, id_segment.contour_index, p.x);
                }
                j += 1;
            }

            let target_id =
                scan_list.first_less(anchor.v_segment.a.x, ContourIndex::EMPTY, anchor.v_segment);
            let parent_index = if target_id.is_hole() {
                // index is a hole index
                // at this moment this hole parent is known
                scratch.parent_for_child[target_id.index()]
            } else {
                target_id.index()
            };

            let child_index = anchor.contour_index.index();

            scratch.parent_for_child[child_index] = parent_index;
            scratch.children_count_for_parent[parent_index] += 1;
        }
    }
}

pub(crate) trait JoinHoles {
    fn join_sorted_holes(
        &mut self,
        holes: &mut Vec<IntContour>,
        anchors: &mut Vec<IdSegment>,
        clockwise: bool,
        scratch: &mut BinderScratch,
    );
    fn scan_join(
        &mut self,
        holes: &mut Vec<IntPath>,
        hole_segments: &mut Vec<IdSegment>,
        clockwise: bool,
        scratch: &mut BinderScratch,
    );
}

impl JoinHoles for Vec<IntShape> {
    #[inline]
    fn join_sorted_holes(
        &mut self,
        holes: &mut Vec<IntContour>,
        anchors: &mut Vec<IdSegment>,
        clockwise: bool,
        scratch: &mut BinderScratch,
    ) {
        if self.is_empty() || holes.is_empty() {
            holes.clear();
            anchors.clear();
            return;
        }

        if self.len() == 1 {
            self[0].append(holes);
            anchors.clear();
            return;
        }
        debug_assert!(is_sorted(anchors));

        anchors.add_sort_by_angle();
        self.scan_join(holes, anchors, clockwise, scratch);
        anchors.clear();
    }

    fn scan_join(
        &mut self,
        holes: &mut Vec<IntPath>,
        hole_segments: &mut Vec<IdSegment>,
        clockwise: bool,
        scratch: &mut BinderScratch,
    ) {
        let x_min = hole_segments[0].v_segment.a.x;
        let x_max = hole_segments[hole_segments.len() - 1].v_segment.a.x;

        let capacity = self.iter().fold(0, |s, it| s + it[0].len()) / 2;
        scratch.segments.clear();
        scratch.segments.reserve(capacity);
        for (i, shape) in self.iter().enumerate() {
            shape[0].append_id_segments(
                &mut scratch.segments,
                ContourIndex::new_shape(i),
                x_min,
                x_max,
                clockwise,
            );
        }

        for (i, hole) in holes.iter().enumerate() {
            hole.append_id_segments(
                &mut scratch.segments,
                ContourIndex::new_hole(i),
                x_min,
                x_max,
                clockwise,
            );
        }

        scratch
            .segments
            .sort_by_a_then_by_angle_and_buffer(&mut scratch.sort_buffer);

        let segments = core::mem::take(&mut scratch.segments);
        ShapeBinder::bind(self.len(), hole_segments, &segments, scratch);
        scratch.segments = segments;

        for (shape_index, &capacity) in scratch.children_count_for_parent.iter().enumerate() {
            self[shape_index].reserve(capacity);
        }

        for (hole_index, hole) in holes.drain(..).enumerate() {
            let shape_index = scratch.parent_for_child[hole_index];
            self[shape_index].push(hole);
        }
        hole_segments.clear();
    }
}

pub(crate) trait LeftBottomSegment {
    fn left_bottom_segment(&self) -> VSegment;
    fn left_bottom_segment_from(&self, a: IntPoint) -> VSegment;
}

impl LeftBottomSegment for IntContour {
    fn left_bottom_segment(&self) -> VSegment {
        let mut a = *self.first().expect("bind contour is non-empty");
        for &p in self.iter().skip(1) {
            if p < a {
                a = p;
            }
        }

        self.left_bottom_segment_from(a)
    }

    fn left_bottom_segment_from(&self, a: IntPoint) -> VSegment {
        let n = self.len();
        let mut result: Option<VSegment> = None;

        for (i, &p) in self.iter().enumerate() {
            if p != a {
                continue;
            }

            // Self-touching contours can visit the left-bottom point several times.
            // Check every incident edge at that point and keep the lowest anchor edge.
            let b0 = self[(i + 1) % n];
            let b1 = self[(i + n - 1) % n];
            result.update_if_under(VSegment { a, b: b0 });
            result.update_if_under(VSegment { a, b: b1 });
        }

        result.unwrap_or(VSegment { a, b: a })
    }
}

#[inline]
fn is_sorted(segments: &[IdSegment]) -> bool {
    segments
        .windows(2)
        .all(|slice| slice[0].v_segment.a <= slice[1].v_segment.a)
}

impl IdSegment {
    #[inline]
    fn cmp_by_a_then_by_angle(&self, other: &Self) -> Ordering {
        self.v_segment
            .a
            .cmp(&other.v_segment.a)
            .then_with(|| self.v_segment.cmp_by_angle(&other.v_segment))
    }
}

pub(crate) trait SortByAngle {
    fn sort_by_a_then_by_angle_and_buffer(&mut self, reusable_buffer: &mut Vec<IdSegment>);
    fn add_sort_by_angle(&mut self);
}

impl SortByAngle for [IdSegment] {
    #[inline]
    fn sort_by_a_then_by_angle_and_buffer(&mut self, reusable_buffer: &mut Vec<IdSegment>) {
        self.sort_by_two_keys_then_by_and_buffer(
            reusable_buffer,
            |s| s.v_segment.a.x,
            |s| s.v_segment.a.y,
            |s0, s1| s0.v_segment.cmp_by_angle(&s1.v_segment),
        );
    }

    #[inline]
    fn add_sort_by_angle(&mut self) {
        // there is a very small chance that sort is required that's why we don't use regular sort

        let mut start = 0;
        while start < self.len() {
            let a = self[start].v_segment.a;
            let mut end = start + 1;

            while end < self.len() && self[end].v_segment.a == a {
                end += 1;
            }

            if end > start + 1 {
                self[start..end].sort_by(|s0, s1| s0.v_segment.cmp_by_angle(&s1.v_segment));
            }

            start = end;
        }
    }
}

#[cfg(test)]
mod bind_solver_tests {
    use crate::geometry::overlay::port::extract::{
        BinderScratch, JoinHoles, LeftBottomSegment, SortByAngle,
    };
    use crate::geometry::overlay::port::extract::{ContourIndex, IdSegment};
    use crate::geometry::overlay::port::point::IntPoint;
    use crate::geometry::overlay::port::segment::VSegment;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::cmp::Ordering;

    #[test]
    fn test_0() {
        let mut shapes = vec![
            vec![vec![
                IntPoint::new(-1, 2),
                IntPoint::new(-1, 4),
                IntPoint::new(-3, 4),
                IntPoint::new(-3, 2),
            ]],
            vec![vec![
                IntPoint::new(6, 0),
                IntPoint::new(6, 6),
                IntPoint::new(3, 6),
                IntPoint::new(2, 3),
                IntPoint::new(3, 0),
            ]],
            vec![vec![
                IntPoint::new(0, -1),
                IntPoint::new(0, -2),
                IntPoint::new(10, -2),
                IntPoint::new(10, -1),
            ]],
        ];

        let mut holes = vec![
            vec![
                IntPoint::new(2, 3),
                IntPoint::new(4, 4),
                IntPoint::new(4, 3),
            ],
            vec![
                IntPoint::new(2, 3),
                IntPoint::new(4, 2),
                IntPoint::new(3, 1),
            ],
        ];

        // Upstream test_0 went through join_unsorted_holes; that wrapper is
        // pruned (production always arrives pre-sorted), so build and sort
        // the anchors here and drive the same scan_join path directly.
        let mut hole_segments: Vec<_> = holes
            .iter()
            .enumerate()
            .map(|(id, path)| IdSegment {
                contour_index: ContourIndex::new_hole(id),
                v_segment: path.left_bottom_segment(),
            })
            .collect();
        hole_segments.sort_by_a_then_by_angle_and_buffer(&mut Vec::new());

        let mut scratch = BinderScratch::default();
        shapes.scan_join(&mut holes, &mut hole_segments, false, &mut scratch);

        assert_eq!(shapes[0].len(), 1);
        assert_eq!(shapes[1].len(), 3);
    }

    #[test]
    fn test_sort() {
        let s0 = VSegment {
            a: IntPoint::new(0, -2),
            b: IntPoint::new(10, -2),
        };
        let s1 = VSegment {
            a: IntPoint::new(2, 3),
            b: IntPoint::new(3, 0),
        };
        let by_a = s0.a.cmp(&s1.a);
        let long_result = match by_a {
            Ordering::Equal => s0.cmp_by_angle(&s1),
            _ => by_a,
        };

        let short_result = s0.a.cmp(&s1.b).then_with(|| s0.cmp_by_angle(&s1));

        assert_eq!(short_result, long_result);
        assert_eq!(Ordering::Less, long_result);
    }
}

#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Default)]
pub(crate) enum VisitState {
    #[default]
    Unvisited = 0,
    Skipped = 1,
    HoleVisited = 2,
    HullVisited = 3,
}

#[derive(Default)]
pub struct BooleanExtractionBuffer {
    pub(crate) points: Vec<IntPoint>,
    pub(crate) visited: Vec<VisitState>,
    simplifier: ContourSimplifier,
    holes: Vec<IntContour>,
    anchors: Vec<IdSegment>,
    binder: BinderScratch,
    contour_pool: Vec<IntContour>,
    shape_pool: Vec<IntShape>,
    #[cfg(test)]
    pool_balance: PoolBalance,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PoolBalance {
    pub(crate) contour_takes: u64,
    pub(crate) contour_recycles: u64,
    pub(crate) shape_takes: u64,
    pub(crate) shape_recycles: u64,
}

impl BooleanExtractionBuffer {
    pub(crate) fn take_contour(&mut self, cap: usize) -> IntContour {
        #[cfg(test)]
        {
            self.pool_balance.contour_takes += 1;
        }
        let mut contour = take_vec_with_capacity(&mut self.contour_pool, cap);
        contour.clear();
        contour
    }

    pub(crate) fn recycle_owned_contour(&mut self, contour: IntContour) {
        self.recycle_contour(contour);
    }

    pub(crate) fn take_shape(&mut self, ring_count: usize) -> IntShape {
        #[cfg(test)]
        {
            self.pool_balance.shape_takes += 1;
        }
        let mut shape = take_vec_with_capacity(&mut self.shape_pool, ring_count);
        shape.clear();
        shape.reserve_capacity(ring_count);
        for _ in 0..ring_count {
            shape.push(self.take_contour(0));
        }
        shape
    }

    pub(crate) fn recycle_owned_shape(&mut self, mut shape: IntShape) {
        #[cfg(test)]
        {
            self.pool_balance.shape_recycles += 1;
        }
        self.recycle_shape(&mut shape);
        self.shape_pool.push(shape);
    }

    pub(crate) fn recycle_shapes(&mut self, shapes: &mut IntShapes) {
        self.recycle_shapes_from(shapes, 0);
    }

    pub(crate) fn recycle_shapes_from(&mut self, shapes: &mut IntShapes, start: usize) {
        for mut shape in shapes.drain(start..) {
            #[cfg(test)]
            {
                self.pool_balance.shape_recycles += 1;
            }
            self.recycle_shape(&mut shape);
            self.shape_pool.push(shape);
        }
    }

    pub(crate) fn recycle_shape(&mut self, shape: &mut IntShape) {
        self.recycle_contours_from(shape, 0);
    }

    pub(crate) fn recycle_contours_from(&mut self, shape: &mut IntShape, start: usize) {
        for contour in shape.drain(start..) {
            self.recycle_contour(contour);
        }
    }

    fn take_contour_from_points(&mut self) -> IntContour {
        let mut contour = take_vec_with_capacity(&mut self.contour_pool, self.points.len());
        contour.clear();
        contour.extend_from_slice(&self.points);
        contour
    }

    fn recycle_contour(&mut self, mut contour: IntContour) {
        #[cfg(test)]
        {
            self.pool_balance.contour_recycles += 1;
        }
        contour.clear();
        self.contour_pool.push(contour);
    }

    #[cfg(test)]
    pub(crate) fn pool_balance(&self) -> PoolBalance {
        self.pool_balance
    }

    #[cfg(test)]
    pub(crate) fn contour_pool_len(&self) -> usize {
        self.contour_pool.len()
    }

    fn take_shape_with_contour(&mut self, contour: IntContour) -> IntShape {
        let mut shape = take_vec_with_capacity(&mut self.shape_pool, 1);
        shape.clear();
        shape.push(contour);
        shape
    }

    pub(crate) fn push_reversed_contour_into(&mut self, contour: &[IntPoint], out: &mut IntShapes) {
        let mut reversed = take_vec_with_capacity(&mut self.contour_pool, contour.len());
        reversed.clear();
        reversed.extend(contour.iter().rev().copied());
        let shape = self.take_shape_with_contour(reversed);
        out.push(shape);
    }
}

fn take_vec_with_capacity<T>(pool: &mut Vec<Vec<T>>, needed: usize) -> Vec<T> {
    let index = pool
        .iter()
        .rposition(|buffer| buffer.capacity() >= needed)
        .or_else(|| {
            pool.iter()
                .enumerate()
                .max_by_key(|(_, buffer)| buffer.capacity())
                .map(|(index, _)| index)
        });
    index.map_or_else(Vec::new, |index| pool.swap_remove(index))
}

impl OverlayGraph<'_> {
    #[inline]
    pub fn extract_shapes_into(
        &self,
        overlay_rule: OverlayRule,
        buffer: &mut BooleanExtractionBuffer,
        out: &mut IntShapes,
    ) {
        self.links
            .filter_by_overlay_into(overlay_rule, &mut buffer.visited);
        self.extract_into(overlay_rule, buffer, out);
    }

    pub(crate) fn extract_into(
        &self,
        overlay_rule: OverlayRule,
        buffer: &mut BooleanExtractionBuffer,
        out: &mut IntShapes,
    ) {
        let clockwise = self.options.output_direction == ContourDirection::Clockwise;

        buffer.holes.clear();
        buffer.anchors.clear();

        buffer.points.reserve_capacity(buffer.visited.len());

        let mut link_index = 0;
        let mut anchors_already_sorted = true;
        while link_index < buffer.visited.len() {
            if buffer.visited.is_visited(link_index) {
                link_index += 1;
                continue;
            }

            let left_top_link = unsafe {
                // Safety: `link_index` walks 0..buffer.visited.len(), and buffer.visited.len() <= self.links.len().
                GraphUtil::find_left_top_link(
                    self.links,
                    self.node_offsets,
                    self.node_indices,
                    link_index,
                    &buffer.visited,
                )
            };

            let link = unsafe {
                // Safety: `left_top_link` originates from `find_left_top_link`, which only returns
                // indices in 0..self.links.len(), so this lookup cannot go out of bounds.
                self.links.get_unchecked(left_top_link)
            };
            let is_hole = overlay_rule.is_fill_top(link.fill);
            let visited_state =
                [VisitState::HullVisited, VisitState::HoleVisited][is_hole as usize];

            let direction = is_hole == clockwise;
            let start_data = StartPathData::new(direction, link, left_top_link);

            self.find_contour(
                &start_data,
                direction,
                visited_state,
                &mut buffer.visited,
                &mut buffer.points,
            );
            let (is_valid, is_modified) = buffer.points.validate(
                self.options.min_output_area,
                self.options.preserve_output_collinear,
                &mut buffer.simplifier,
            );

            if !is_valid {
                link_index += 1;
                continue;
            }

            let contour = buffer.take_contour_from_points();

            if is_hole {
                let left_bottom = if clockwise { contour[1] } else { contour[0] };
                let mut v_segment = contour.left_bottom_segment_from(left_bottom);

                if is_modified {
                    let most_left = contour.left_bottom_segment();
                    if most_left != v_segment {
                        v_segment = most_left;
                        anchors_already_sorted = false;
                    }
                };

                debug_assert!(v_segment == contour.left_bottom_segment());
                let id_data = ContourIndex::new_hole(buffer.holes.len());
                buffer
                    .anchors
                    .push(IdSegment::with_segment(id_data, v_segment));
                buffer.holes.push(contour);
            } else {
                out.push(buffer.take_shape_with_contour(contour));
            }
        }

        if !anchors_already_sorted {
            buffer.anchors.sort_unstable_by_key(|s0| s0.v_segment.a);
        }

        out.join_sorted_holes(
            &mut buffer.holes,
            &mut buffer.anchors,
            clockwise,
            &mut buffer.binder,
        );
    }

    pub(crate) fn find_contour(
        &self,
        start_data: &StartPathData,
        clockwise: bool,
        visited_state: VisitState,
        visited: &mut [VisitState],
        points: &mut Vec<IntPoint>,
    ) {
        let mut link_id = start_data.link_id;
        let mut node_id = start_data.node_id;
        let last_node_id = start_data.last_node_id;

        visited.visit_edge(link_id, visited_state);
        points.clear();
        points.push(start_data.begin);

        let last_link_id = GraphUtil::next_link(
            self.links,
            self.node_offsets,
            self.node_indices,
            link_id,
            last_node_id,
            !clockwise,
            visited,
        );

        // Find a closed tour
        while link_id != last_link_id {
            link_id = GraphUtil::next_link(
                self.links,
                self.node_offsets,
                self.node_indices,
                link_id,
                node_id,
                clockwise,
                visited,
            );

            let link = unsafe {
                // Safety: `link_id` is always derived from a previous in-bounds index or
                // from `find_left_top_link`, so it remains in `0..self.links.len()`.
                self.links.get_unchecked(link_id)
            };
            node_id = points.push_node_and_get_other(link, node_id);

            visited.visit_edge(link_id, visited_state);
        }
    }
}

pub(crate) struct StartPathData {
    pub(crate) begin: IntPoint,
    pub(crate) node_id: usize,
    pub(crate) link_id: usize,
    pub(crate) last_node_id: usize,
}

impl StartPathData {
    #[inline(always)]
    pub(crate) fn new(direction: bool, link: &OverlayLink, link_id: usize) -> Self {
        if direction {
            Self {
                begin: link.b.point,
                node_id: link.a.id,
                link_id,
                last_node_id: link.b.id,
            }
        } else {
            Self {
                begin: link.a.point,
                node_id: link.b.id,
                link_id,
                last_node_id: link.a.id,
            }
        }
    }
}

pub(crate) trait GraphContour {
    fn validate(
        &mut self,
        min_output_area: u64,
        preserve_output_collinear: bool,
        simplifier: &mut ContourSimplifier,
    ) -> (bool, bool);
    fn push_node_and_get_other(&mut self, link: &OverlayLink, node_id: usize) -> usize;
}

impl GraphContour for IntContour {
    #[inline]
    fn validate(
        &mut self,
        min_output_area: u64,
        preserve_output_collinear: bool,
        simplifier: &mut ContourSimplifier,
    ) -> (bool, bool) {
        let is_modified = if !preserve_output_collinear {
            self.simplify_contour(simplifier)
        } else {
            false
        };

        if self.len() < 3 {
            return (false, is_modified);
        }

        if min_output_area == 0u64 {
            return (true, is_modified);
        }
        let area = self.unsafe_area();
        let abs_area = area.unsigned_abs() >> 1;
        let is_valid = abs_area >= min_output_area;

        (is_valid, is_modified)
    }

    #[inline]
    fn push_node_and_get_other(&mut self, link: &OverlayLink, node_id: usize) -> usize {
        if link.a.id == node_id {
            self.push(link.a.point);
            link.b.id
        } else {
            self.push(link.b.point);
            link.a.id
        }
    }
}

impl VisitState {
    #[inline(always)]
    pub(crate) fn new(skipped: bool) -> Self {
        let raw = skipped as u8; // 0 or 1
        debug_assert!(raw <= VisitState::Skipped as u8);
        // SAFETY: repr(u8) and raw is in range 0..=1
        unsafe { core::mem::transmute(raw) }
    }
}

pub(crate) trait Visit {
    fn is_visited(&self, index: usize) -> bool;
    fn is_not_visited(&self, index: usize) -> bool;
    fn visit_edge(&mut self, index: usize, state: VisitState);
}

// Safety: every call site creates `visited` slices with one entry per link/node,
// and they only pass indices directly obtained from those slices. That keeps
// `index < self.len()` true for the lifetime of the traversal.
impl Visit for [VisitState] {
    #[inline(always)]
    fn is_visited(&self, index: usize) -> bool {
        unsafe {
            // SAFETY: callers only pass indices derived from the visited slice itself, so index < len.
            *self.get_unchecked(index) != VisitState::Unvisited
        }
    }

    #[inline(always)]
    fn is_not_visited(&self, index: usize) -> bool {
        unsafe {
            // SAFETY: callers only pass indices derived from the visited slice itself, so index < len.
            *self.get_unchecked(index) == VisitState::Unvisited
        }
    }
    #[inline(always)]
    fn visit_edge(&mut self, index: usize, state: VisitState) {
        unsafe {
            // SAFETY: callers only pass indices derived from the visited slice itself, so index < len.
            *self.get_unchecked_mut(index) = state;
        }
    }
}

pub(crate) struct GraphUtil;

impl GraphUtil {
    /// # Safety
    /// * `link_index < links.len()`
    /// * `links[top.a.id]` must exist for every link
    /// * For bridge nodes, both `bridge[k] < links.len()`
    /// * `visited` is at least `links.len()` long (or whatever invariant applies)
    #[inline]
    pub(crate) unsafe fn find_left_top_link(
        links: &[OverlayLink],
        node_offsets: &[u32],
        node_indices: &[u32],
        link_index: usize,
        visited: &[VisitState],
    ) -> usize {
        let top = unsafe {
            // SAFETY: link_index is always < links.len(); callers either iterate that range or
            // pull the value from visited, which mirrors links.
            links.get_unchecked(link_index)
        };
        let indices = unsafe { Self::node_indices(node_offsets, node_indices, top.a.id) };

        debug_assert!(top.is_direct());

        if indices.len() == 2 {
            Self::find_left_top_link_on_bridge(links, indices)
        } else {
            Self::find_left_top_link_on_indices(links, top, link_index, indices, visited)
        }
    }

    #[inline(always)]
    fn find_left_top_link_on_indices(
        links: &[OverlayLink],
        link: &OverlayLink,
        link_index: usize,
        indices: &[u32],
        visited: &[VisitState],
    ) -> usize {
        let mut top_index = link_index;
        let mut top = link;

        // find most top link

        for &raw_i in indices {
            let i = raw_i as usize;
            if i == link_index {
                continue;
            }
            let link = unsafe {
                // SAFETY: indices holds link ids emitted by GraphBuilder, so each i < links.len().
                links.get_unchecked(i)
            };
            if !link.is_direct() || Triangle::is_clockwise(top.a.point, top.b.point, link.b.point) {
                continue;
            }

            if visited.is_visited(i) {
                continue;
            }

            top_index = i;
            top = link;
        }

        top_index
    }

    #[inline(always)]
    fn find_left_top_link_on_bridge(links: &[OverlayLink], bridge: &[u32]) -> usize {
        // SAFETY: every bridge index comes straight from GraphBuilder::build_nodes_and_connect_links,
        // which only records values in 0..links.len(), so the unchecked lookups stay in-bounds.
        let (l0, l1) = unsafe {
            (
                links.get_unchecked(bridge[0] as usize),
                links.get_unchecked(bridge[1] as usize),
            )
        };
        if Triangle::is_clockwise(l0.a.point, l0.b.point, l1.b.point) {
            bridge[0] as usize
        } else {
            bridge[1] as usize
        }
    }

    #[inline(always)]
    pub(crate) fn next_link(
        links: &[OverlayLink],
        node_offsets: &[u32],
        node_indices: &[u32],
        link_id: usize,
        node_id: usize,
        clockwise: bool,
        visited: &[VisitState],
    ) -> usize {
        let indices = unsafe { Self::node_indices(node_offsets, node_indices, node_id) };
        if indices.len() == 2 {
            let first = indices[0] as usize;
            let second = indices[1] as usize;
            if first == link_id { second } else { first }
        } else {
            GraphUtil::find_nearest_link_to(links, link_id, node_id, clockwise, indices, visited)
        }
    }

    #[inline(always)]
    unsafe fn node_indices<'a>(
        node_offsets: &[u32],
        node_indices: &'a [u32],
        node_id: usize,
    ) -> &'a [u32] {
        let start = unsafe { *node_offsets.get_unchecked(node_id) as usize };
        let end = unsafe { *node_offsets.get_unchecked(node_id + 1) as usize };
        unsafe { node_indices.get_unchecked(start..end) }
    }

    // Assumes: `indices` comes from GraphBuilder's CSR node storage, so every
    // element is a valid link index and at least one is still unvisited when we
    // enter. The unchecked accesses rely on that invariant.
    #[inline]
    fn find_nearest_link_to(
        links: &[OverlayLink],
        target_index: usize,
        node_id: usize,
        clockwise: bool,
        indices: &[u32],
        visited: &[VisitState],
    ) -> usize {
        let mut is_first = true;
        let mut first_index = 0;
        let mut second_index = usize::MAX;
        let mut pos = 0;
        for (i, &raw_link_index) in indices.iter().enumerate() {
            let link_index = raw_link_index as usize;
            if visited.is_not_visited(link_index) {
                if is_first {
                    first_index = link_index;
                    is_first = false;
                } else {
                    second_index = link_index;
                    pos = i;
                    break;
                }
            }
        }

        if second_index == usize::MAX {
            return first_index;
        }

        let target = unsafe {
            // SAFETY: target_index is either the incoming link_id or an entry from indices, both validated.
            links.get_unchecked(target_index)
        };
        let (c, a) = if target.a.id == node_id {
            (target.a.point, target.b.point)
        } else {
            (target.b.point, target.a.point)
        };

        // more the one vectors
        let b = unsafe {
            // SAFETY: first_index originates from indices, so it is within links.
            links.get_unchecked(first_index)
        }
        .other(node_id)
        .point;
        let mut vector_solver = NearestVector::new(c, a, b, first_index, clockwise);

        // add second vector
        vector_solver.add(
            unsafe {
                // SAFETY: second_index comes from indices just like first_index.
                links.get_unchecked(second_index)
            }
            .other(node_id)
            .point,
            second_index,
        );

        // check the rest vectors
        for &raw_link_index in indices.iter().skip(pos + 1) {
            let link_index = raw_link_index as usize;
            if visited.is_not_visited(link_index) {
                let p = unsafe {
                    // SAFETY: every link_index here is sourced from indices, so it addresses links.
                    links.get_unchecked(link_index)
                }
                .other(node_id)
                .point;
                vector_solver.add(p, link_index);
            }
        }

        vector_solver.best_id
    }
}

pub(crate) trait Int {
    fn log2_sqrt(&self) -> usize;
}

impl Int for usize {
    #[inline]
    fn log2_sqrt(&self) -> usize {
        let z = self.leading_zeros();
        let i = (usize::BITS - z) as usize;
        let n = (i + 1) >> 1;
        1 << n
    }
}

#[cfg(test)]
mod util_log_tests {
    use crate::geometry::overlay::port::extract::Int;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn test_0() {
        let tests: Vec<[usize; 2]> = vec![
            [0, 1],
            [1, 2],
            [3, 2],
            [15, 4],
            [16, 8],
            [255, 16],
            [256, 32],
        ];

        for test in tests {
            let a = test[0].log2_sqrt();
            let b = test[1];
            assert_eq!(a, b);
        }
    }
}
