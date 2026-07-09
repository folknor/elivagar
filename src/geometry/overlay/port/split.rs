use crate::geometry::overlay::port::Solver;
use crate::geometry::overlay::port::cross::{CrossSolver, CrossType, EndMask};
use crate::geometry::overlay::port::grid::Fragment;
use crate::geometry::overlay::port::grid::{BorderVSegment, FragmentBuffer, GridLayout};
use crate::geometry::overlay::port::point::IntPoint;
use crate::geometry::overlay::port::segment::LineRange;
use crate::geometry::overlay::port::segment::Segment;
use crate::geometry::overlay::port::segment::ShapeCountBoolean;
use crate::geometry::overlay::port::segment::ShapeSegmentsMerge;
use crate::geometry::overlay::port::segment::ShapeSegmentsSort;
use crate::geometry::overlay::port::segment::XSegment;
use crate::geometry::overlay::port::sort::OneKeyAndCmpSort;
use alloc::vec::Vec;

pub(crate) struct SplitSolver {
    pub(super) marks: Vec<LineMark>,
    pub(super) reusable_marks: Vec<LineMark>,
    segment_sort_buffer: Vec<Segment<ShapeCountBoolean>>,
    pub(super) tree_buffer: TreeSplitBuffer,
    pub(super) fragment_buffer: FragmentBuffer,
    pub(super) border_points: Vec<IntPoint>,
}

impl SplitSolver {
    #[inline(always)]
    pub(crate) fn new() -> Self {
        Self {
            marks: Vec::new(),
            reusable_marks: Vec::new(),
            segment_sort_buffer: Vec::new(),
            tree_buffer: TreeSplitBuffer::new(),
            fragment_buffer: FragmentBuffer::empty(),
            border_points: Vec::new(),
        }
    }
}

impl SplitSolver {
    #[inline]
    pub(crate) fn split_segments(
        &mut self,
        segments: &mut Vec<Segment<ShapeCountBoolean>>,
        solver: &Solver,
    ) -> bool {
        if segments.is_empty() {
            return false;
        }

        segments.sort_by_ab(&mut self.segment_sort_buffer);
        let any_merged = segments.merge_if_needed();
        if segments.is_empty() {
            return true;
        }

        let any_intersection = self.split(segments, solver);
        any_merged | any_intersection
    }

    #[inline]
    fn split(&mut self, segments: &mut Vec<Segment<ShapeCountBoolean>>, solver: &Solver) -> bool {
        let is_list = solver.is_list_split(segments);
        let snap_radius = solver.snap_radius();
        if is_list {
            return self.list_split(snap_radius, segments, solver);
        }

        let is_fragmentation = solver.is_fragmentation_required(segments);

        if is_fragmentation {
            self.fragment_split(snap_radius, segments, solver)
        } else {
            self.tree_split(snap_radius, segments, solver)
        }
    }

    pub(super) fn cross(
        i: usize,
        j: usize,
        ei: &XSegment,
        ej: &XSegment,
        marks: &mut Vec<LineMark>,
        radius: i64,
    ) -> bool {
        let cross = if let Some(cross) = CrossSolver::cross(ei, ej, radius) {
            cross
        } else {
            return false;
        };

        match cross.cross_type {
            CrossType::Pure => {
                marks.push(LineMark {
                    index: i,
                    point: cross.point,
                });
                marks.push(LineMark {
                    index: j,
                    point: cross.point,
                });
            }
            CrossType::TargetEnd => {
                marks.push(LineMark {
                    index: j,
                    point: cross.point,
                });
            }
            CrossType::OtherEnd => {
                marks.push(LineMark {
                    index: i,
                    point: cross.point,
                });
            }
            CrossType::Overlay => {
                let mask = CrossSolver::collinear(ei, ej);
                if mask == 0 {
                    return false;
                }

                if mask.is_target_a() {
                    marks.push(LineMark {
                        index: j,
                        point: ei.a,
                    });
                }

                if mask.is_target_b() {
                    marks.push(LineMark {
                        index: j,
                        point: ei.b,
                    });
                }

                if mask.is_other_a() {
                    marks.push(LineMark {
                        index: i,
                        point: ej.a,
                    });
                }

                if mask.is_other_b() {
                    marks.push(LineMark {
                        index: i,
                        point: ej.b,
                    });
                }
            }
        }

        cross.is_round
    }

    pub(super) fn apply(&mut self, segments: &mut Vec<Segment<ShapeCountBoolean>>) {
        self.marks.sort_by_index_and_point(&mut self.reusable_marks);
        self.marks.dedup();

        segments.reserve(self.marks.len());

        // split segments

        let mut i = 0;
        while i < self.marks.len() {
            let start = i;
            let m0 = self.marks[i];

            i += 1;
            while i < self.marks.len() && self.marks[i].index == m0.index {
                i += 1;
            }

            let s0 = unsafe {
                // SAFETY: m0.index < segments.len() (marks are built from valid segment indices).
                // We take at most one &mut to that element per group. We drop the &mut before any push,
                // so no aliasing or reallocation invalidation can occur.
                segments.get_unchecked_mut(m0.index)
            };

            let count = s0.count;
            let x_seg = s0.x_segment;

            if start + 1 == i {
                // single split
                *s0 = Segment::create_and_validate(x_seg.a, m0.point, count);
                let s1 = Segment::create_and_validate(m0.point, x_seg.b, count);
                segments.push(s1);

                continue;
            }

            // we have several points
            let sub_marks = &mut self.marks[start..i];
            Self::sort_sub_marks(sub_marks, x_seg);

            let m0 = sub_marks[0];
            *s0 = Segment::create_and_validate(x_seg.a, m0.point, count);

            let mut p0 = m0.point;

            for mi in sub_marks.iter().skip(1) {
                segments.push(Segment::create_and_validate(p0, mi.point, count));
                p0 = mi.point;
            }

            segments.push(Segment::create_and_validate(p0, x_seg.b, count));
        }

        segments.sort_by_ab(&mut self.segment_sort_buffer);
        segments.merge_if_needed();
    }

    #[inline]
    fn sort_sub_marks(marks: &mut [LineMark], x_seg: XSegment) {
        let mut j0 = 0;
        let mut j = 1;

        let m0 = marks[0];
        let mut x0 = m0.point.x;
        while j < marks.len() {
            let xi = marks[j].point.x;
            if x0 == xi {
                j += 1;
                continue;
            }

            if j0 + 1 < j {
                let (y0, y1) = Self::y_range(j0, j, x_seg, marks);
                Self::sort_sub_marks_by_y(y0, y1, &mut marks[j0..j]);
            }

            x0 = xi;
            j0 = j;
            j += 1;
        }

        if j0 + 1 < j {
            let (y0, y1) = Self::y_range(j0, j, x_seg, marks);
            Self::sort_sub_marks_by_y(y0, y1, &mut marks[j0..j]);
        }
    }

    #[inline]
    fn y_range(j0: usize, j1: usize, s: XSegment, marks: &[LineMark]) -> (i32, i32) {
        let y0 = if j0 == 0 {
            s.a.y
        } else {
            marks[j0 - 1].point.y
        };
        let y1 = if j1 == marks.len() {
            s.b.y
        } else {
            marks[j1].point.y
        };
        (y0, y1)
    }

    #[inline]
    fn sort_sub_marks_by_y(y0: i32, y1: i32, marks: &mut [LineMark]) {
        // The x-coordinate is the same for every point
        // By default, the range should be sorted in ascending order by the y-coordinate.
        if y0 > y1 {
            // reverse the order to sort the range in descending order by the y-coordinate.
            marks.reverse();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) struct LineMark {
    pub(super) index: usize,
    pub(super) point: IntPoint,
}

pub(super) trait SortMarkByIndexAndPoint {
    fn sort_by_index_and_point(&mut self, reusable_buffer: &mut Vec<LineMark>);
}

impl SortMarkByIndexAndPoint for [LineMark] {
    #[inline]
    fn sort_by_index_and_point(&mut self, reusable_buffer: &mut Vec<LineMark>) {
        self.sort_by_one_key_then_by_and_buffer(
            reusable_buffer,
            |m| m.index,
            |m0, m1| m0.point.cmp(&m1.point),
        );
    }
}

pub(super) struct SnapRadius {
    current: usize,
    step: usize,
}

impl SnapRadius {
    pub(super) fn increment(&mut self) {
        self.current = 60.min(self.current + self.step);
    }

    pub(super) fn radius(&self) -> i64 {
        1i64 << self.current as u32
    }
}

impl Solver {
    pub(super) fn snap_radius(&self) -> SnapRadius {
        SnapRadius {
            current: self.precision.start,
            step: self.precision.progression,
        }
    }
}

impl SplitSolver {
    pub(super) fn list_split(
        &mut self,
        snap_radius: SnapRadius,
        segments: &mut Vec<Segment<ShapeCountBoolean>>,
        solver: &Solver,
    ) -> bool {
        let mut need_to_fix = true;

        let mut snap_radius = snap_radius;
        let mut any_intersection = false;
        while need_to_fix && segments.len() > 1 {
            need_to_fix = false;
            self.marks.clear();

            let radius = snap_radius.radius();

            for (i, si) in segments.iter().enumerate() {
                let xsi = &si.x_segment;
                let ri = xsi.y_range();
                for (j, sj) in segments.iter().enumerate().skip(i + 1) {
                    let xsj = &sj.x_segment;
                    if xsi.b.x < xsj.a.x {
                        break;
                    }

                    if xsj.is_not_intersect_y_range(&ri) {
                        continue;
                    }

                    let is_round = Self::cross(i, j, xsi, xsj, &mut self.marks, radius);
                    need_to_fix = need_to_fix || is_round;
                }
            }

            if self.marks.is_empty() {
                return any_intersection;
            }
            any_intersection = true;
            self.apply(segments);

            snap_radius.increment();

            if need_to_fix && !solver.is_list_split(segments) {
                // finish with tree solver if edges is become large
                self.tree_split(snap_radius, segments, solver);
                return true;
            }
        }

        any_intersection
    }
}

/// Inclusive integer y-range, from i_tree's `SegRange` at `R = i32`.
#[derive(Debug, Clone, Copy)]
struct SegRange {
    min: i32,
    max: i32,
}

#[derive(Clone, Copy)]
struct IdSegment {
    id: usize,
    x_segment: XSegment,
}

impl IdSegment {
    #[inline]
    fn expiration(&self) -> i32 {
        self.x_segment.b.x
    }
}

pub(super) struct TreeSplitBuffer {
    tree: ReusableSegExpTree,
}

impl TreeSplitBuffer {
    #[inline]
    pub(super) fn new() -> Self {
        Self {
            tree: ReusableSegExpTree::new(),
        }
    }
}

impl SplitSolver {
    pub(super) fn tree_split(
        &mut self,
        snap_radius: SnapRadius,
        segments: &mut Vec<Segment<ShapeCountBoolean>>,
        solver: &Solver,
    ) -> bool {
        let range: SegRange = if let Some(range) = segments.ver_range() {
            range.into()
        } else {
            return false;
        };
        let mut tree = core::mem::replace(&mut self.tree_buffer.tree, ReusableSegExpTree::new());
        if !tree.reset(range) {
            self.tree_buffer.tree = tree;
            return self.list_split(snap_radius, segments, solver);
        }

        let mut need_to_fix = true;
        let mut any_intersection = false;

        let mut snap_radius = snap_radius;

        while need_to_fix && segments.len() > 2 {
            need_to_fix = false;
            self.marks.clear();

            let radius = snap_radius.radius();

            for (i, si) in segments.iter().enumerate() {
                let time = si.x_segment.a.x;
                let si_range = si.x_segment.y_range().into();
                for sj in tree.iter_by_range(si_range, time) {
                    let (this_index, scan_index, this, scan) = if si.x_segment < sj.x_segment {
                        (i, sj.id, &si.x_segment, &sj.x_segment)
                    } else {
                        (sj.id, i, &sj.x_segment, &si.x_segment)
                    };

                    let is_round =
                        Self::cross(this_index, scan_index, this, scan, &mut self.marks, radius);

                    need_to_fix = is_round || need_to_fix;
                }

                tree.insert_by_range(si_range, si.id_segment(i));
            }

            if self.marks.is_empty() {
                self.tree_buffer.tree = tree;
                return any_intersection;
            }

            any_intersection = true;
            tree.clear();

            self.apply(segments);

            snap_radius.increment();
        }

        self.tree_buffer.tree = tree;
        any_intersection
    }
}

struct ReusableSegExpTree {
    layout: Option<TreeLayout>,
    chunks: Vec<TreeChunk>,
    active_chunks: usize,
}

impl ReusableSegExpTree {
    #[inline]
    fn new() -> Self {
        Self {
            layout: None,
            chunks: Vec::new(),
            active_chunks: 0,
        }
    }
}

impl ReusableSegExpTree {
    #[inline]
    fn reset(&mut self, range: SegRange) -> bool {
        let Some(layout) = TreeLayout::new(range.min, range.max) else {
            return false;
        };
        let count = layout.count();
        self.layout = Some(layout);
        self.active_chunks = count;
        if self.chunks.len() < count {
            self.chunks.resize_with(count, TreeChunk::new);
        }
        for chunk in &mut self.chunks {
            chunk.clear();
        }
        true
    }

    #[inline]
    fn insert_by_range(&mut self, range: SegRange, val: IdSegment) {
        let layout = self.layout.as_ref().expect("tree layout initialized");
        let mask = layout.insert_mask(range.min, range.max);
        let entity = TreeEntity { val, mask };
        for index in BitIter::new(mask) {
            self.chunk_mut(index).insert(entity);
        }
    }

    #[inline]
    fn iter_by_range(&mut self, range: SegRange, time: i32) -> ReusableSegExpTreeIter<'_> {
        let layout = self.layout.as_ref().expect("tree layout initialized");
        let mask = layout.intersect_mask(range.min, range.max);
        ReusableSegExpTreeIter::new(mask, time, self)
    }

    #[inline]
    fn clear(&mut self) {
        for chunk in self.chunks.iter_mut().take(self.active_chunks) {
            chunk.clear();
        }
    }

    #[inline]
    fn chunk(&self, index: usize) -> &TreeChunk {
        unsafe { self.chunks.get_unchecked(index) }
    }

    #[inline]
    fn chunk_mut(&mut self, index: usize) -> &mut TreeChunk {
        unsafe { self.chunks.get_unchecked_mut(index) }
    }
}

#[derive(Clone)]
struct TreeChunk {
    buffer: Vec<TreeEntity>,
}

impl TreeChunk {
    #[inline]
    fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    #[inline]
    fn entity(&self, index: usize) -> &TreeEntity {
        unsafe { self.buffer.get_unchecked(index) }
    }

    #[inline]
    fn insert(&mut self, entity: TreeEntity) {
        self.buffer.push(entity);
    }

    #[inline]
    fn clear(&mut self) {
        self.buffer.clear();
    }
}

#[derive(Clone, Copy)]
struct TreeEntity {
    val: IdSegment,
    mask: u64,
}

struct ReusableSegExpTreeIter<'a> {
    tree: &'a mut ReusableSegExpTree,
    time: i32,
    i0: usize,
    i1: usize,
    mask: u64,
    bit_iter: BitIter,
}

impl<'a> ReusableSegExpTreeIter<'a> {
    #[inline]
    fn new(mask: u64, time: i32, tree: &'a mut ReusableSegExpTree) -> Self {
        let mut iter = Self {
            tree,
            time,
            i0: 0,
            i1: 0,
            mask,
            bit_iter: BitIter::new(mask),
        };
        iter.i0 = iter.find_next_not_empty_chunk();
        iter
    }

    #[inline]
    fn find_next_not_empty_chunk(&mut self) -> usize {
        for next in &mut self.bit_iter {
            if next < self.tree.active_chunks && !self.tree.chunk(next).is_empty() {
                return next;
            }
        }
        usize::MAX
    }
}

impl Iterator for ReusableSegExpTreeIter<'_> {
    type Item = IdSegment;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while self.i0 < self.tree.active_chunks {
            let chunk = self.tree.chunk_mut(self.i0);
            let mut i = self.i1;
            while i < chunk.buffer.len() {
                let item = *chunk.entity(i);

                if item.val.expiration() < self.time {
                    chunk.buffer.swap_remove(i);
                    continue;
                }
                i += 1;

                let mask_int = item.mask & self.mask;
                let first_index = mask_int.trailing_zeros() as usize;

                if first_index == self.i0 {
                    self.i1 = i;
                    return Some(item.val);
                }
            }

            self.i0 = self.find_next_not_empty_chunk();
            self.i1 = 0;
        }

        None
    }
}

#[derive(Clone, Copy)]
struct TreeLayout {
    min: i32,
    max: i32,
    scale: u32,
}

impl TreeLayout {
    #[inline]
    fn new(start: i32, end: i32) -> Option<Self> {
        let min = start;
        let max = end;
        if max < min {
            return None;
        }
        let span = (max as i64 - min as i64) as u32;
        // i_tree's LayoutUInt::HEAP_MIN_SPAN for u32.
        if span < 31 {
            return None;
        }
        let p = span.ilog2() + 1;
        if p < Heap32::POWER {
            return None;
        }
        let scale = p - Heap32::POWER;

        Some(Self { min, max, scale })
    }

    #[inline]
    fn index(&self, value: i32) -> u32 {
        ((value as i64 - self.min as i64) as u32) >> self.scale
    }

    #[inline]
    fn count(&self) -> usize {
        let order = self.index(self.max);
        Heap32::order_to_heap_index(order) as usize + 1
    }

    #[inline]
    fn insert_mask(&self, min: i32, max: i32) -> u64 {
        let start = self.index(min);
        let end = self.index(max);
        Heap32::range_to_place_mask(start, end)
    }

    #[inline]
    fn intersect_mask(&self, min: i32, max: i32) -> u64 {
        let start = self.index(min);
        let end = self.index(max);
        Heap32::range_to_intersect_mask(start, end)
    }
}

struct Heap32;

impl Heap32 {
    const SUB_CAPACITY: u32 = 31;
    const POWER: u32 = 32_u32.ilog2();

    #[inline]
    fn range_to_intersect_mask(start: u32, end: u32) -> u64 {
        debug_assert!(start < 32);
        debug_assert!(end < 32);

        let mut w = Self::range_to_fill_mask(start, end);

        let mut shift = 32;
        for _ in 0..6 {
            let mut lt = shift - 1;
            shift >>= 1;
            for _ in 0..shift {
                let rt = lt + 1;
                let pt = lt >> 1;

                let lt_bit = (w >> lt) & 1;
                let rt_bit = (w >> rt) & 1;
                let pt_bit = lt_bit | rt_bit;

                w |= pt_bit << pt;

                lt += 2;
            }
        }

        w
    }

    #[inline]
    fn range_to_place_mask(start: u32, end: u32) -> u64 {
        debug_assert!(start < 32);
        debug_assert!(end < 32);

        if end - start == 31 {
            return 1;
        }

        let mut w = Self::range_to_fill_mask(start, end);

        let mut m: u64 = 0;
        let mut shift = 32;
        for _ in 0..6 {
            let mut lt = shift - 1;
            shift >>= 1;

            for _ in 0..shift {
                let rt = lt + 1;
                let pt = lt >> 1;

                let lt_bit = (w >> lt) & 1;
                let rt_bit = (w >> rt) & 1;
                let pt_bit = lt_bit & rt_bit;

                w |= pt_bit << pt;
                m |= (lt_bit ^ pt_bit) << lt;
                m |= (rt_bit ^ pt_bit) << rt;

                lt += 2;
            }
        }

        m
    }

    #[inline]
    fn range_to_fill_mask(start: u32, end: u32) -> u64 {
        let i0 = Self::order_to_heap_index(start);
        let i1 = Self::order_to_heap_index(end);
        fill_bits(i0, i1)
    }

    #[inline]
    fn order_to_heap_index(order: u32) -> u32 {
        order + Self::SUB_CAPACITY
    }
}

#[inline]
fn fill_bits(start: u32, end: u32) -> u64 {
    if start == 0 && end >= 63 {
        return u64::MAX;
    }
    let high = if end >= 63 {
        u64::MAX
    } else {
        (1_u64 << (end + 1)) - 1
    };
    let low = if start == 0 { 0 } else { (1_u64 << start) - 1 };
    high ^ low
}

struct BitIter {
    value: u64,
}

impl BitIter {
    #[inline]
    fn new(value: u64) -> Self {
        Self { value }
    }
}

impl Iterator for BitIter {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.value == 0 {
            return None;
        }
        let pos = self.value.trailing_zeros() as usize;
        self.value &= self.value - 1;
        Some(pos)
    }
}

impl From<LineRange> for SegRange {
    #[inline]
    fn from(value: LineRange) -> Self {
        Self {
            min: value.min,
            max: value.max,
        }
    }
}

trait VerticalRange {
    fn ver_range(&self) -> Option<LineRange>;
}

impl<C: Send> VerticalRange for Vec<Segment<C>> {
    fn ver_range(&self) -> Option<LineRange> {
        let mut min_y = self.first()?.x_segment.a.y;
        let mut max_y = min_y;

        for edge in self {
            min_y = min_y.min(edge.x_segment.a.y);
            max_y = max_y.max(edge.x_segment.a.y);
            min_y = min_y.min(edge.x_segment.b.y);
            max_y = max_y.max(edge.x_segment.b.y);
        }

        Some(LineRange {
            min: min_y,
            max: max_y,
        })
    }
}

impl<C: Send> Segment<C> {
    #[inline]
    fn id_segment(&self, id: usize) -> IdSegment {
        IdSegment {
            id,
            x_segment: self.x_segment,
        }
    }
}

impl SplitSolver {
    pub(super) fn fragment_split(
        &mut self,
        snap_radius: SnapRadius,
        segments: &mut Vec<Segment<ShapeCountBoolean>>,
        solver: &Solver,
    ) -> bool {
        let layout = if let Some(layout) =
            GridLayout::new(segments.iter().map(|it| it.x_segment), segments.len())
        {
            layout
        } else {
            return self.tree_split(snap_radius, segments, solver);
        };

        self.fragment_buffer.reset_layout(layout);

        let mut need_to_fix = true;
        let mut any_intersection = false;

        let mut snap_radius = snap_radius;

        while need_to_fix && segments.len() > 2 {
            self.marks.clear();

            self.fragment_buffer
                .init_fragment_buffer(segments.iter().map(|it| it.x_segment));
            for (i, segment) in segments.iter().enumerate() {
                self.fragment_buffer.add_segment(i, segment.x_segment);
            }

            let mut buffer = core::mem::replace(&mut self.fragment_buffer, FragmentBuffer::empty());
            need_to_fix = self.process(snap_radius.radius(), &mut buffer, solver);

            #[cfg(debug_assertions)]
            debug_assert!(buffer.is_on_border_sorted());
            let mut j = 0;
            while j < buffer.on_border.len() {
                let j0 = j;
                let x = buffer.on_border[j].x;
                j += 1;
                while j < buffer.on_border.len() && x == buffer.on_border[j].x {
                    j += 1;
                }

                let index = buffer.layout.index(x);
                if let Some(fragments) = buffer.groups.get(index) {
                    self.on_border_split(x, fragments, &mut buffer.on_border[j0..j]);
                }
            }

            if self.marks.is_empty() {
                self.fragment_buffer = buffer;
                return any_intersection;
            }

            any_intersection = true;
            buffer.clear();
            self.fragment_buffer = buffer;

            self.apply(segments);

            snap_radius.increment();
        }

        any_intersection
    }

    #[inline]
    fn process(&mut self, radius: i64, buffer: &mut FragmentBuffer, _solver: &Solver) -> bool {
        let mut is_any_round = false;
        for group in buffer.groups.iter_mut().take(buffer.active_groups) {
            if group.is_empty() {
                continue;
            }
            let any_round = Self::bin_split(radius, group, &mut self.marks);
            is_any_round = is_any_round || any_round;
        }
        is_any_round
    }

    fn bin_split(radius: i64, fragments: &mut [Fragment], marks: &mut Vec<LineMark>) -> bool {
        if fragments.len() < 2 {
            return false;
        }

        fragments.sort_unstable_by_key(|a| a.rect.min_y);

        let mut any_round = false;

        for (i, fi) in fragments.iter().enumerate().take(fragments.len() - 1) {
            for fj in fragments.iter().skip(i + 1) {
                if fi.rect.max_y < fj.rect.min_y {
                    break;
                }
                if !fi.rect.is_intersect_border_include(&fj.rect) {
                    continue;
                }

                // MARK: the intersection, ensuring the right order for deterministic results

                let is_round = if fi.x_segment < fj.x_segment {
                    Self::cross_fragments(fi, fj, radius, marks)
                } else {
                    Self::cross_fragments(fj, fi, radius, marks)
                };

                any_round = any_round || is_round;
            }
        }

        any_round
    }

    fn on_border_split(
        &mut self,
        border_x: i32,
        fragments: &[Fragment],
        vertical_segments: &mut [BorderVSegment],
    ) {
        self.border_points.clear();
        for fragment in fragments {
            if fragment.x_segment.b.x == border_x {
                self.border_points.push(fragment.x_segment.b);
            }
        }

        if self.border_points.is_empty() {
            return;
        }

        self.border_points.sort_unstable_by_key(|p0| p0.y);
        vertical_segments.sort_by_key(|s0| s0.y_range.min);

        let mut i = 0;
        for s in &*vertical_segments {
            while i < self.border_points.len() && self.border_points[i].y <= s.y_range.min {
                i += 1;
            }
            let mut j = i;
            while j < self.border_points.len() && self.border_points[j].y < s.y_range.max {
                self.marks.push(LineMark {
                    index: s.id,
                    point: self.border_points[j],
                });
                j += 1;
            }
        }
    }

    fn cross_fragments(
        fi: &Fragment,
        fj: &Fragment,
        radius: i64,
        marks: &mut Vec<LineMark>,
    ) -> bool {
        let cross = if let Some(cross) = CrossSolver::cross(&fi.x_segment, &fj.x_segment, radius) {
            cross
        } else {
            return false;
        };

        let r = crate::geometry::overlay::port::point::i_from_wide(radius);

        match cross.cross_type {
            CrossType::Overlay => {
                let mask = CrossSolver::collinear(&fi.x_segment, &fj.x_segment);
                if mask == 0 {
                    return false;
                }

                if !(fi.rect.contains_with_radius(fi.x_segment.a, r)
                    || fj.rect.contains_with_radius(fi.x_segment.a, r))
                {
                    return false;
                }

                if mask.is_target_a() {
                    marks.push(LineMark {
                        index: fj.index,
                        point: fi.x_segment.a,
                    });
                }

                if mask.is_target_b() {
                    marks.push(LineMark {
                        index: fj.index,
                        point: fi.x_segment.b,
                    });
                }

                if mask.is_other_a() {
                    marks.push(LineMark {
                        index: fi.index,
                        point: fj.x_segment.a,
                    });
                }

                if mask.is_other_b() {
                    marks.push(LineMark {
                        index: fi.index,
                        point: fj.x_segment.b,
                    });
                }
            }
            _ => {
                if !fi.rect.contains_with_radius(cross.point, r)
                    || !fj.rect.contains_with_radius(cross.point, r)
                {
                    return false;
                }

                match cross.cross_type {
                    CrossType::Pure => {
                        marks.push(LineMark {
                            index: fi.index,
                            point: cross.point,
                        });
                        marks.push(LineMark {
                            index: fj.index,
                            point: cross.point,
                        });
                    }
                    CrossType::TargetEnd => {
                        marks.push(LineMark {
                            index: fj.index,
                            point: cross.point,
                        });
                    }
                    CrossType::OtherEnd => {
                        marks.push(LineMark {
                            index: fi.index,
                            point: cross.point,
                        });
                    }
                    _ => {}
                }
            }
        }

        cross.is_round
    }
}
