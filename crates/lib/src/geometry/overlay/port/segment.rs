use crate::geometry::overlay::port::ShapeType;
use crate::geometry::overlay::port::point::IntPoint;
use crate::geometry::overlay::port::point::Triangle;
use crate::geometry::overlay::port::sort::TwoKeysAndCmpSort;
use alloc::vec::Vec;
use core::cmp::Ordering;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LineRange {
    pub(crate) min: i32,
    pub(crate) max: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct XSegment {
    pub(crate) a: IntPoint,
    pub(crate) b: IntPoint,
}

impl XSegment {
    #[inline(always)]
    pub(crate) fn y_range(&self) -> LineRange {
        if self.a.y < self.b.y {
            LineRange {
                min: self.a.y,
                max: self.b.y,
            }
        } else {
            LineRange {
                min: self.b.y,
                max: self.a.y,
            }
        }
    }

    #[inline(always)]
    pub(crate) fn is_not_vertical(&self) -> bool {
        self.a.x != self.b.x
    }

    #[inline(always)]
    pub(crate) fn is_not_intersect_y_range(&self, range: &LineRange) -> bool {
        range.min > self.a.y && range.min > self.b.y || range.max < self.a.y && range.max < self.b.y
    }
}

impl PartialOrd for XSegment {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for XSegment {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> Ordering {
        let a = self.a.cmp(&other.a);
        if a == Ordering::Equal {
            self.b.cmp(&other.b)
        } else {
            a
        }
    }
}

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
            if segment.is_under_segment(best) {
                *best = segment;
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

#[cfg(test)]
mod geom_v_segment_tests {
    use crate::geometry::overlay::port::point::IntPoint;
    use crate::geometry::overlay::port::segment::VSegment;
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

#[derive(Clone, Copy)]
pub(crate) struct End {
    pub(crate) index: usize,
    pub(crate) point: IntPoint,
}

impl Default for End {
    #[inline(always)]
    fn default() -> Self {
        Self {
            index: 0,
            point: IntPoint::ZERO,
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub(crate) struct IdPoint {
    pub(crate) id: usize,
    pub(crate) point: IntPoint,
}

impl IdPoint {
    pub(crate) fn new(id: usize, point: IntPoint) -> Self {
        Self { id, point }
    }
}

pub(crate) trait WindingCount
where
    Self: Clone + Copy + Send + Sync,
{
    fn is_not_empty(&self) -> bool;
    fn new(subj: i32, clip: i32) -> Self;
    fn with_shape_type(shape_type: ShapeType) -> (Self, Self);
    fn add(self, count: Self) -> Self;
    fn invert(self) -> Self;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShapeCountBoolean {
    pub subj: i32,
    pub clip: i32,
}

impl ShapeCountBoolean {
    pub(crate) const SUBJ_DIRECT: ShapeCountBoolean = ShapeCountBoolean { subj: 1, clip: 0 };
    pub(crate) const SUBJ_INVERT: ShapeCountBoolean = ShapeCountBoolean { subj: -1, clip: 0 };
    pub(crate) const CLIP_DIRECT: ShapeCountBoolean = ShapeCountBoolean { subj: 0, clip: 1 };
    pub(crate) const CLIP_INVERT: ShapeCountBoolean = ShapeCountBoolean { subj: 0, clip: -1 };
}

impl WindingCount for ShapeCountBoolean {
    #[inline(always)]
    fn is_not_empty(&self) -> bool {
        self.subj != 0 || self.clip != 0
    }

    #[inline(always)]
    fn new(subj: i32, clip: i32) -> Self {
        Self { subj, clip }
    }

    #[inline(always)]
    fn with_shape_type(shape_type: ShapeType) -> (Self, Self) {
        match shape_type {
            ShapeType::Subject => (
                ShapeCountBoolean::SUBJ_DIRECT,
                ShapeCountBoolean::SUBJ_INVERT,
            ),
            ShapeType::Clip => (
                ShapeCountBoolean::CLIP_DIRECT,
                ShapeCountBoolean::CLIP_INVERT,
            ),
        }
    }

    #[inline(always)]
    fn add(self, count: Self) -> Self {
        let subj = self.subj + count.subj;
        let clip = self.clip + count.clip;

        Self { subj, clip }
    }

    #[inline(always)]
    fn invert(self) -> Self {
        Self {
            subj: -self.subj,
            clip: -self.clip,
        }
    }
}

pub type SegmentFill = u8;

pub const NONE: SegmentFill = 0;

pub const SUBJ_TOP: SegmentFill = 0b0001;
pub const SUBJ_BOTTOM: SegmentFill = 0b0010;
pub const CLIP_TOP: SegmentFill = 0b0100;
pub const CLIP_BOTTOM: SegmentFill = 0b1000;

pub const SUBJ_BOTH: SegmentFill = SUBJ_TOP | SUBJ_BOTTOM;
pub const CLIP_BOTH: SegmentFill = CLIP_TOP | CLIP_BOTTOM;
pub const BOTH_TOP: SegmentFill = SUBJ_TOP | CLIP_TOP;
pub const BOTH_BOTTOM: SegmentFill = SUBJ_BOTTOM | CLIP_BOTTOM;

pub const ALL: SegmentFill = SUBJ_BOTH | CLIP_BOTH;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Segment<C> {
    pub(crate) x_segment: XSegment,
    pub(crate) count: C,
}

impl<C: WindingCount> Segment<C> {
    #[inline(always)]
    pub(crate) fn create_and_validate(a: IntPoint, b: IntPoint, count: C) -> Self {
        if a < b {
            Self {
                x_segment: XSegment { a, b },
                count,
            }
        } else {
            Self {
                x_segment: XSegment { a: b, b: a },
                count: count.invert(),
            }
        }
    }
}

impl<C> PartialEq<Self> for Segment<C> {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        self.x_segment == other.x_segment
    }
}

impl<C> Eq for Segment<C> {}

impl<C> PartialOrd for Segment<C> {
    #[inline(always)]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<C> Ord for Segment<C> {
    #[inline(always)]
    fn cmp(&self, other: &Self) -> Ordering {
        self.x_segment.cmp(&other.x_segment)
    }
}

pub(crate) trait BuildSegments {
    fn append_path_iter<It: Iterator<Item = IntPoint>>(
        &mut self,
        iter: It,
        shape_type: ShapeType,
        keep_same_line_points: bool,
    ) -> bool;
}

impl<C: WindingCount> BuildSegments for Vec<Segment<C>> {
    #[inline]
    fn append_path_iter<It: Iterator<Item = IntPoint>>(
        &mut self,
        iter: It,
        shape_type: ShapeType,
        keep_same_line_points: bool,
    ) -> bool {
        if keep_same_line_points {
            build_segments_with_filter::<DropOppositeCollinear, It, C>(self, iter, shape_type)
        } else {
            build_segments_with_filter::<DropCollinear, It, C>(self, iter, shape_type)
        }
    }
}

fn build_segments_with_filter<F: PointFilter, It: Iterator<Item = IntPoint>, C: WindingCount>(
    segments: &mut Vec<Segment<C>>,
    mut iter: It,
    shape_type: ShapeType,
) -> bool {
    // our goal add all not degenerate segments
    let mut p0 = if let Some(p) = iter.next() {
        p
    } else {
        return false;
    };
    let mut p1 = if let Some(p) = iter.find(|p| p0.ne(p)) {
        p
    } else {
        return true;
    };

    let mut filtered = false;

    let q0 = p0;

    for p2 in &mut iter {
        if F::include_point(p0, p1, p2) {
            p0 = p1;
            p1 = p2;
            break;
        }
        p1 = p2;
        filtered = true;
    }

    let q1 = p0;

    let (direct, invert) = C::with_shape_type(shape_type);

    // We close the loop with the first two points
    for p2 in &mut iter.chain([q0, q1]) {
        if !F::include_point(p0, p1, p2) {
            p1 = p2;
            filtered = true;
            continue;
        }
        segments.push(Segment::with_ab(p0, p1, direct, invert));

        p0 = p1;
        p1 = p2;
    }

    let add_last = p1 != p0;
    filtered |= !add_last;
    if add_last {
        segments.push(Segment::with_ab(p0, p1, direct, invert));
    }

    filtered
}

trait PointFilter {
    fn include_point(a: IntPoint, b: IntPoint, c: IntPoint) -> bool;
}

struct DropOppositeCollinear;
struct DropCollinear;

impl PointFilter for DropOppositeCollinear {
    #[inline]
    fn include_point(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> bool {
        let a = p1 - p0;
        let b = p1 - p2;

        if a.cross_product(b) != 0i64 {
            // not collinear
            return true;
        }

        // collinear - keep only if we keep going same direction
        a.dot_product(b) < 0i64
    }
}

impl PointFilter for DropCollinear {
    #[inline]
    fn include_point(p0: IntPoint, p1: IntPoint, p2: IntPoint) -> bool {
        let a = p1 - p0;
        let b = p1 - p2;
        a.cross_product(b) != 0i64
    }
}

impl<C: Send> Segment<C> {
    #[inline]
    pub(crate) fn with_ab(p0: IntPoint, p1: IntPoint, direct: C, invert: C) -> Self {
        if p0 < p1 {
            Self {
                x_segment: XSegment { a: p0, b: p1 },
                count: direct,
            }
        } else {
            Self {
                x_segment: XSegment { a: p1, b: p0 },
                count: invert,
            }
        }
    }
}

#[cfg(test)]
mod segm_build_tests {
    use crate::geometry::overlay::port::ShapeType;
    use crate::geometry::overlay::port::point::IntPoint;
    use crate::geometry::overlay::port::segment::BuildSegments;
    use crate::geometry::overlay::port::segment::Segment;
    use crate::geometry::overlay::port::segment::ShapeCountBoolean;
    use crate::geometry::overlay::port::segment::ShapeSegmentsMerge;
    use alloc::vec::Vec;

    #[test]
    fn test_0() {
        let points = [
            IntPoint::new(2, 0),
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
            IntPoint::new(2, 2),
        ];

        test_count(&points, 0, false);
    }

    #[test]
    fn test_1() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
            IntPoint::new(2, 2),
            IntPoint::new(2, 0),
        ];

        test_count(&points, 0, false);
    }

    #[test]
    fn test_2() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(0, 2),
            IntPoint::new(2, 2),
            IntPoint::new(2, 0),
        ];

        test_count(&points, 4, true);
    }

    #[test]
    fn test_roll_0() {
        let points = [
            IntPoint::new(1, 0),
            IntPoint::new(1, 0),
            IntPoint::new(1, 0),
            IntPoint::new(1, 0),
        ];

        test_roll_count(&points, 0, false);
        test_roll_count(&points, 0, true);
    }

    #[test]
    fn test_roll_1() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
        ];

        test_roll_count(&points, 0, false);
        test_roll_count(&points, 0, true);
    }

    #[test]
    fn test_roll_2() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
            IntPoint::new(0, 0),
        ];

        test_roll_count(&points, 0, false);
        test_roll_count(&points, 0, true);
    }

    #[test]
    fn test_roll_3() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
            IntPoint::new(2, 0),
            IntPoint::new(2, 2),
            IntPoint::new(2, 2),
        ];

        test_roll_count(&points, 3, false);
        test_roll_count(&points, 3, true);
    }

    #[test]
    fn test_roll_4() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(0, 2),
            IntPoint::new(2, 2),
            IntPoint::new(2, 0),
        ];

        test_roll_count(&points, 4, false);
        test_roll_count(&points, 4, true);
    }

    #[test]
    fn test_roll_5() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(1, 0),
            IntPoint::new(2, 0),
        ];

        test_roll_count(&points, 0, false);
        test_roll_count(&points, 0, true);
    }

    #[test]
    fn test_roll_6() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(1, 0),
            IntPoint::new(2, 0),
            IntPoint::new(3, 0),
        ];

        test_roll_count(&points, 0, false);
        test_roll_count(&points, 0, true);
    }

    #[test]
    fn test_roll_7() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
            IntPoint::new(2, 2),
            IntPoint::new(2, 0),
        ];

        test_roll_count(&points, 0, false);
        test_roll_count(&points, 0, true);
    }

    #[test]
    fn test_roll_8() {
        let points = [
            IntPoint::new(0, 3),
            IntPoint::new(-4, -3),
            IntPoint::new(4, -3),
            IntPoint::new(3, -3),
            IntPoint::new(0, 3),
        ];

        test_roll_count(&points, 3, false);
        test_roll_count(&points, 3, true);
    }

    #[test]
    fn test_roll_9() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(2, 0),
            IntPoint::new(2, 2),
            IntPoint::new(4, 2),
            IntPoint::new(2, 2),
            IntPoint::new(2, 0),
        ];

        test_roll_count(&points, 0, false);
        test_roll_count(&points, 0, true);
    }

    #[test]
    fn test_roll_10() {
        let points = [
            IntPoint::new(-10, 0),
            IntPoint::new(-10, -10),
            IntPoint::new(0, -10),
            IntPoint::new(10, -10),
            IntPoint::new(10, 0),
            IntPoint::new(10, 10),
            IntPoint::new(0, 10),
            IntPoint::new(-10, 10),
        ];

        test_roll_count(&points, 4, false);
        test_roll_count(&points, 8, true);
    }

    #[test]
    fn test_roll_11() {
        let points = [
            IntPoint::new(-1, 2),
            IntPoint::new(-1, 1),
            IntPoint::new(-2, 1),
            IntPoint::new(-1, 1),
            IntPoint::new(-1, -1),
            IntPoint::new(-1, -2),
            IntPoint::new(-1, -1),
            IntPoint::new(-2, -1),
            IntPoint::new(-1, -1),
            IntPoint::new(1, -1),
            IntPoint::new(2, -1),
            IntPoint::new(1, -1),
            IntPoint::new(1, -2),
            IntPoint::new(1, -1),
            IntPoint::new(1, 1),
            IntPoint::new(1, 2),
            IntPoint::new(1, 1),
            IntPoint::new(2, 1),
            IntPoint::new(1, 1),
            IntPoint::new(-1, 1),
        ];

        test_roll_count(&points, 4, false);
        test_roll_count(&points, 4, true);
    }

    #[test]
    fn test_roll_12() {
        let points = [
            IntPoint::new(0, 0),
            IntPoint::new(0, 2),
            IntPoint::new(1, 2),
            IntPoint::new(2, 2),
            IntPoint::new(3, 2),
            IntPoint::new(4, 2),
            IntPoint::new(5, 0),
        ];

        test_roll_count(&points, 4, false);
        test_roll_count(&points, 7, true);
    }

    #[test]
    fn test_roll_13() {
        let points = [
            IntPoint::new(0, 2),
            IntPoint::new(5, 2),
            IntPoint::new(4, 2),
            IntPoint::new(4, 0),
            IntPoint::new(1, 0),
            IntPoint::new(1, 2),
        ];

        test_roll_count(&points, 4, false);
        test_roll_count(&points, 4, true);
    }

    fn test_count(points: &[IntPoint], count: usize, keep_same_line_points: bool) {
        let mut segments: Vec<Segment<ShapeCountBoolean>> = Vec::new();
        segments.append_path_iter(
            points.iter().copied(),
            ShapeType::Subject,
            keep_same_line_points,
        );
        segments.merge_if_needed();

        assert_eq!(segments.len(), count);
    }

    fn test_roll_count(slice: &[IntPoint], count: usize, keep_same_line_points: bool) {
        let mut points = slice.to_vec();
        let n = points.len();
        let mut segments: Vec<Segment<ShapeCountBoolean>> = Vec::with_capacity(n);
        for _ in 0..n {
            segments.append_path_iter(
                points.iter().copied(),
                ShapeType::Subject,
                keep_same_line_points,
            );
            segments.merge_if_needed();

            assert_eq!(segments.len(), count);

            segments.clear();
            roll_points(&mut points);
        }
    }

    fn roll_points(points: &mut Vec<IntPoint>) {
        if points.len() <= 1 {
            return;
        }

        if let Some(last) = points.pop() {
            points.insert(0, last);
        }
    }
}

pub(crate) trait ShapeSegmentsMerge<C>
where
    C: WindingCount,
{
    fn merge_if_needed(&mut self) -> bool;
}

impl<C: WindingCount> ShapeSegmentsMerge<C> for Vec<Segment<C>> {
    fn merge_if_needed(&mut self) -> bool {
        if self.len() < 2 {
            return false;
        }

        let mut prev = &self[0].x_segment;
        for i in 1..self.len() {
            let this = &self[i].x_segment;
            if prev.eq(this) {
                let new_len = merge(self, i);
                self.truncate(new_len);
                return true;
            }
            prev = this;
        }

        false
    }
}

fn merge<C: WindingCount>(segments: &mut [Segment<C>], after: usize) -> usize {
    let mut i = after;
    let mut j = i - 1;
    let mut prev = segments[j];

    while i < segments.len() {
        if prev.x_segment.eq(&segments[i].x_segment) {
            prev.count = prev.count.add(segments[i].count);
        } else {
            if prev.count.is_not_empty() {
                segments[j] = prev;
                j += 1;
            }
            prev = segments[i];
        }
        i += 1;
    }

    if prev.count.is_not_empty() {
        segments[j] = prev;
        j += 1;
    }

    j
}

#[cfg(test)]
mod segm_merge_tests {
    use super::*;
    use crate::geometry::overlay::port::point::IntPoint;
    use crate::geometry::overlay::port::segment::ShapeCountBoolean;
    use alloc::vec;

    #[test]
    fn test_merge_if_needed_empty() {
        let mut segments: Vec<Segment<ShapeCountBoolean>> = Vec::new();
        segments.merge_if_needed();
        assert!(
            segments.is_empty(),
            "Empty vector should remain empty after merge"
        );
    }

    #[test]
    fn test_merge_if_needed_single_element() {
        let a = IntPoint::new(1, 2);
        let b = IntPoint::new(3, 4);
        let count = ShapeCountBoolean::new(1, 1);
        let segment = Segment::create_and_validate(a, b, count);
        let mut segments = vec![segment];
        segments.merge_if_needed();
        assert_eq!(segments.len(), 1, "Single segment should remain unchanged");
        assert_eq!(
            segments[0], segment,
            "Segment should be unchanged after merge"
        );
    }

    #[test]
    fn test_merge_if_needed_no_merge() {
        let a1 = IntPoint::new(1, 2);
        let b1 = IntPoint::new(3, 4);
        let count1 = ShapeCountBoolean::new(1, 0);
        let segment1 = Segment::create_and_validate(a1, b1, count1);

        let a2 = IntPoint::new(5, 6);
        let b2 = IntPoint::new(7, 8);
        let count2 = ShapeCountBoolean::new(0, 1);
        let segment2 = Segment::create_and_validate(a2, b2, count2);

        let mut segments = vec![segment1, segment2];
        segments.merge_if_needed();

        assert_eq!(
            segments.len(),
            2,
            "Segments with different x_segments should not be merged"
        );
        assert_eq!(
            segments[0], segment1,
            "First segment should remain unchanged"
        );
        assert_eq!(
            segments[1], segment2,
            "Second segment should remain unchanged"
        );
    }

    #[test]
    fn test_merge_if_needed_single_merge() {
        let a1 = IntPoint::new(1, 2);
        let b1 = IntPoint::new(3, 4);
        let count1 = ShapeCountBoolean::new(1, 0);
        let segment1 = Segment::create_and_validate(a1, b1, count1);

        let a2 = IntPoint::new(1, 2);
        let b2 = IntPoint::new(3, 4);
        let count2 = ShapeCountBoolean::new(0, 1);
        let segment2 = Segment::create_and_validate(a2, b2, count2);

        let mut segments = vec![segment1, segment2];
        segments.merge_if_needed();

        assert_eq!(segments.len(), 1, "Segments should be merged into one");
        let merged_count = ShapeCountBoolean::new(1, 1);
        let expected_segment = Segment::create_and_validate(a1, b1, merged_count);
        assert_eq!(
            segments[0], expected_segment,
            "Merged segment should have combined counts"
        );
    }

    #[test]
    fn test_merge_if_needed_multiple_merges() {
        let a = IntPoint::new(1, 2);
        let b = IntPoint::new(3, 4);

        let count1 = ShapeCountBoolean::new(1, 0);
        let count2 = ShapeCountBoolean::new(0, 1);
        let count3 = ShapeCountBoolean::new(2, 2);

        let segment1 = Segment::create_and_validate(a, b, count1);
        let segment2 = Segment::create_and_validate(a, b, count2);
        let segment3 = Segment::create_and_validate(a, b, count3);

        let mut segments = vec![segment1, segment2, segment3];
        segments.merge_if_needed();

        assert_eq!(segments.len(), 1, "All segments should be merged into one");
        let merged_count = ShapeCountBoolean::new(3, 3);
        let expected_segment = Segment::create_and_validate(a, b, merged_count);
        assert_eq!(
            segments[0], expected_segment,
            "Merged segment should have combined counts"
        );
    }

    #[test]
    fn test_merge_if_needed_segments_with_inverted_order() {
        let a1 = IntPoint::new(3, 4);
        let b1 = IntPoint::new(1, 2);
        let count1 = ShapeCountBoolean::new(1, 0);
        // create_and_validate should order the points
        let segment1 = Segment::create_and_validate(a1, b1, count1);

        let a2 = IntPoint::new(1, 2);
        let b2 = IntPoint::new(3, 4);
        let count2 = ShapeCountBoolean::new(0, 1);
        let segment2 = Segment::create_and_validate(a2, b2, count2);

        let mut segments = vec![segment1, segment2];
        segments.merge_if_needed();

        // Both segments should have the same ordered x_segment
        assert_eq!(
            segments.len(),
            1,
            "Segments with inverted points should be merged"
        );

        let merged_count = ShapeCountBoolean::new(1, 1);
        let expected_segment =
            Segment::create_and_validate(IntPoint::new(1, 2), IntPoint::new(3, 4), merged_count);
        assert_eq!(
            segments[0], expected_segment,
            "Merged segment should have combined counts and ordered points"
        );
    }

    #[test]
    fn test_merge_if_needed_no_merge_different_x_segments() {
        let a1 = IntPoint::new(1, 1);
        let b1 = IntPoint::new(2, 2);
        let count1 = ShapeCountBoolean::new(1, 1);
        let segment1 = Segment::create_and_validate(a1, b1, count1);

        let a2 = IntPoint::new(3, 3);
        let b2 = IntPoint::new(4, 4);
        let count2 = ShapeCountBoolean::new(2, 2);
        let segment2 = Segment::create_and_validate(a2, b2, count2);

        let a3 = IntPoint::new(5, 5);
        let b3 = IntPoint::new(6, 6);
        let count3 = ShapeCountBoolean::new(3, 3);
        let segment3 = Segment::create_and_validate(a3, b3, count3);

        let mut segments = vec![segment1, segment2, segment3];
        segments.merge_if_needed();

        assert_eq!(
            segments.len(),
            3,
            "Segments with different x_segments should not be merged"
        );
        assert_eq!(
            segments[0], segment1,
            "First segment should remain unchanged"
        );
        assert_eq!(
            segments[1], segment2,
            "Second segment should remain unchanged"
        );
        assert_eq!(
            segments[2], segment3,
            "Third segment should remain unchanged"
        );
    }
}

pub(crate) trait ShapeSegmentsSort {
    fn sort_by_ab(&mut self, reusable_buffer: &mut Vec<Self::Item>);
    type Item;
}

impl<C: Send + Sync + Copy> ShapeSegmentsSort for [Segment<C>] {
    type Item = Segment<C>;

    #[inline]
    fn sort_by_ab(&mut self, reusable_buffer: &mut Vec<Segment<C>>) {
        self.sort_by_two_keys_then_by_and_buffer(
            reusable_buffer,
            |s| s.x_segment.a.x,
            |s| s.x_segment.a.y,
            |s0, s1| s0.x_segment.b.cmp(&s1.x_segment.b),
        );
    }
}
