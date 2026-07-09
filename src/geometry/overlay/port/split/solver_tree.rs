use crate::geometry::overlay::port::core::solver::Solver;
use crate::geometry::overlay::port::geom::line_range::LineRange;
use crate::geometry::overlay::port::geom::x_segment::XSegment;
use crate::geometry::overlay::port::ksort::sort::key::SortKey;
use crate::geometry::overlay::port::segm::boolean::ShapeCountBoolean;
use crate::geometry::overlay::port::segm::segment::Segment;
use crate::geometry::overlay::port::split::snap_radius::SnapRadius;
use crate::geometry::overlay::port::split::solver::SplitSolver;
use crate::geometry::overlay::port::tree::seg::exp::SegRange;
use crate::geometry::overlay::port::tree::{Expiration, LayoutNumber, LayoutUInt};
use alloc::vec::Vec;

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
        let range: SegRange<i32> = if let Some(range) = segments.ver_range() {
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
    fn reset(&mut self, range: SegRange<i32>) -> bool {
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
    fn insert_by_range(&mut self, range: SegRange<i32>, val: IdSegment) {
        let layout = self.layout.as_ref().expect("tree layout initialized");
        let mask = layout.insert_mask(range.min, range.max);
        let entity = TreeEntity { val, mask };
        for index in BitIter::new(mask) {
            self.chunk_mut(index).insert(entity);
        }
    }

    #[inline]
    fn iter_by_range(&mut self, range: SegRange<i32>, time: i32) -> ReusableSegExpTreeIter<'_> {
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
        let span = i32::range_span(min, max)?;
        if span < <i32 as LayoutNumber>::UInt::HEAP_MIN_SPAN {
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
        (value.offset_from(self.min) >> self.scale).to_u32()
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

impl From<LineRange> for SegRange<i32> {
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

        for edge in self.iter() {
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
