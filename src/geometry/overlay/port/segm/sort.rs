use crate::geometry::overlay::port::segm::segment::Segment;
use crate::geometry::overlay::port::sort::TwoKeysAndCmpSort;
use alloc::vec::Vec;

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
