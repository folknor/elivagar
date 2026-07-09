use crate::geometry::overlay::port::core::fill_rule::FillRule;
use crate::geometry::overlay::port::core::solver::Solver;
use crate::geometry::overlay::port::geom::end::End;
use crate::geometry::overlay::port::geom::v_segment::VSegment;
use crate::geometry::overlay::port::prim::Triangle;
use crate::geometry::overlay::port::segm::segment::{Segment, SegmentFill};
use crate::geometry::overlay::port::segm::winding::WindingCount;
use crate::geometry::overlay::port::tree::Expiration;
use crate::geometry::overlay::port::tree::key::exp::KeyExpCollection;
use crate::geometry::overlay::port::tree::key::list::KeyExpList;
use crate::geometry::overlay::port::tree::key::tree::KeyExpTree;
use crate::geometry::overlay::port::util::log::Int;
use alloc::vec::Vec;
use core::ops::ControlFlow;

pub(crate) trait FillStrategy<C> {
    fn add_and_fill(this: C, bot: C) -> (C, SegmentFill);
}

pub(crate) trait FillHandler<C> {
    type Output;
    fn handle(
        &mut self,
        index: usize,
        segment: &Segment<C>,
        fill: SegmentFill,
    ) -> ControlFlow<Self::Output>;
    fn finalize(self) -> Self::Output;
}

#[inline]
fn sweep_with_handler<C, F, S, H>(
    scan: &mut S,
    node: &mut Vec<End>,
    segments: &[Segment<C>],
    mut handler: H,
) -> H::Output
where
    C: WindingCount,
    F: FillStrategy<C>,
    S: KeyExpCollection<VSegment, i32, C>,
    H: FillHandler<C>,
{
    node.clear();
    let n = segments.len();
    let mut i = 0;

    while i < n {
        let p = segments[i].x_segment.a;

        node.push(End {
            index: i,
            point: segments[i].x_segment.b,
        });
        i += 1;

        while i < n && segments[i].x_segment.a == p {
            node.push(End {
                index: i,
                point: segments[i].x_segment.b,
            });
            i += 1;
        }

        if node.len() > 1 {
            node.sort_by(|s0, s1| Triangle::clock_order(p, s1.point, s0.point));
        }

        let mut sum_count =
            scan.first_less_or_equal_by(p.x, C::new(0, 0), |s| s.is_under_point_order(p));

        for se in node.iter() {
            let sid = unsafe { segments.get_unchecked(se.index) };
            let (new_sum, fill) = F::add_and_fill(sid.count, sum_count);
            sum_count = new_sum;

            if let ControlFlow::Break(result) = handler.handle(se.index, sid, fill) {
                return result;
            }

            if sid.x_segment.is_not_vertical() {
                scan.insert(sid.x_segment.into(), sum_count, p.x);
            }
        }

        node.clear();
    }

    handler.finalize()
}

pub(crate) struct SweepRunner<C> {
    list: Option<KeyExpList<VSegment, i32, C>>,
    tree: Option<KeyExpTree<VSegment, i32, C>>,
    node: Vec<End>,
}

impl<C: WindingCount> SweepRunner<C> {
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            list: None,
            tree: None,
            node: Vec::with_capacity(4),
        }
    }

    #[inline]
    pub(crate) fn run<F, H>(
        &mut self,
        solver: &Solver,
        segments: &[Segment<C>],
        handler: H,
    ) -> H::Output
    where
        F: FillStrategy<C>,
        H: FillHandler<C>,
    {
        let count = segments.len();
        if solver.is_list_fill(segments) {
            let capacity = count.log2_sqrt().max(4) * 2;
            let mut list = self.take_scan_list(capacity);
            let result =
                sweep_with_handler::<C, F, _, _>(&mut list, &mut self.node, segments, handler);
            self.list = Some(list);
            result
        } else {
            let capacity = count.log2_sqrt().max(8);
            let mut tree = self.take_scan_tree(capacity);
            let result =
                sweep_with_handler::<C, F, _, _>(&mut tree, &mut self.node, segments, handler);
            self.tree = Some(tree);
            result
        }
    }

    #[inline]
    fn take_scan_list(&mut self, capacity: usize) -> KeyExpList<VSegment, i32, C> {
        if let Some(mut list) = self.list.take() {
            list.clear();
            list.reserve_capacity(capacity);
            list
        } else {
            KeyExpList::new(capacity)
        }
    }

    #[inline]
    fn take_scan_tree(&mut self, capacity: usize) -> KeyExpTree<VSegment, i32, C> {
        if let Some(mut tree) = self.tree.take() {
            tree.clear();
            tree.reserve_capacity(capacity);
            tree
        } else {
            KeyExpTree::new(capacity)
        }
    }
}

pub(crate) struct NonZeroStrategy;
