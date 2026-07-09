use crate::geometry::overlay::port::build::sweep::{FillHandler, FillStrategy, SweepRunner};
use crate::geometry::overlay::port::core::link::OverlayLink;
use crate::geometry::overlay::port::core::solver::Solver;
use crate::geometry::overlay::port::geom::end::End;
use crate::geometry::overlay::port::geom::id_point::IdPoint;
use crate::geometry::overlay::port::prim::IntPoint;
use crate::geometry::overlay::port::segm::segment::{NONE, Segment, SegmentFill};
use crate::geometry::overlay::port::segm::winding::WindingCount;
use crate::geometry::overlay::port::shape::Reserve;
use alloc::vec::Vec;
use core::ops::ControlFlow;

pub(super) trait InclusionFilterStrategy {
    fn is_included(fill: SegmentFill) -> bool;
}

pub(crate) struct StoreFillsHandler<'a> {
    fills: &'a mut Vec<SegmentFill>,
}

impl<'a> StoreFillsHandler<'a> {
    #[inline]
    pub(crate) fn new(fills: &'a mut Vec<SegmentFill>) -> Self {
        Self { fills }
    }
}

impl<C> FillHandler<C> for StoreFillsHandler<'_> {
    type Output = ();

    #[inline(always)]
    fn handle(
        &mut self,
        index: usize,
        _segment: &Segment<C>,
        fill: SegmentFill,
    ) -> ControlFlow<()> {
        // fills is pre-allocated to segments.len() and index is guaranteed
        // to be in range by the sweep algorithm
        unsafe { *self.fills.get_unchecked_mut(index) = fill };
        ControlFlow::Continue(())
    }

    #[inline(always)]
    fn finalize(self) {}
}

pub(crate) trait GraphNode {
    fn with_indices(indices: &[usize], node_indices: &mut Vec<u32>) -> Self;
}

pub(crate) struct GraphBuilder<C, N> {
    sweep_runner: SweepRunner<C>,
    pub(super) links: Vec<OverlayLink>,
    pub(super) nodes: Vec<N>,
    pub(super) node_indices: Vec<u32>,
    pub(super) node_scratch: Vec<usize>,
    pub(super) fills: Vec<SegmentFill>,
    pub(super) ends: Vec<End>,
    pub(super) ends_sort_buffer: Vec<End>,
    pub(super) point_sort_buffer: Vec<IntPoint>,
}

impl<C, N> GraphBuilder<C, N>
where
    C: WindingCount,
    N: GraphNode,
{
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            sweep_runner: SweepRunner::new(),
            links: Vec::new(),
            nodes: Vec::new(),
            node_indices: Vec::new(),
            node_scratch: Vec::with_capacity(4),
            fills: Vec::new(),
            ends: Vec::new(),
            ends_sort_buffer: Vec::new(),
            point_sort_buffer: Vec::new(),
        }
    }

    #[inline]
    pub(super) fn build_fills_with_strategy<F: FillStrategy<C>>(
        &mut self,
        solver: &Solver,
        segments: &[Segment<C>],
    ) {
        self.fills.resize(segments.len(), NONE);
        self.sweep_runner
            .run::<F, _>(solver, segments, StoreFillsHandler::new(&mut self.fills));
    }

    #[inline]
    pub(super) fn build_links_by_filter<F: InclusionFilterStrategy>(
        &mut self,
        segments: &[Segment<C>],
    ) {
        self.links.clear();
        self.links.reserve_capacity(segments.len());

        for (segment, &fill) in segments.iter().zip(&self.fills) {
            if !F::is_included(fill) {
                continue;
            }
            self.links.push(OverlayLink::new(
                IdPoint::new(0, segment.x_segment.a),
                IdPoint::new(0, segment.x_segment.b),
                fill,
            ));
        }
    }
}
