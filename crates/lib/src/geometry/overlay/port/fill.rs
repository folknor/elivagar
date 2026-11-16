use crate::geometry::overlay::port::FillRule;
use crate::geometry::overlay::port::IntOverlayOptions;
use crate::geometry::overlay::port::Solver;
use crate::geometry::overlay::port::extract::Int;
use crate::geometry::overlay::port::extract::OverlayRule;
use crate::geometry::overlay::port::extract::Reserve;
use crate::geometry::overlay::port::extract::VisitState;
use crate::geometry::overlay::port::graph::OverlayGraph;
use crate::geometry::overlay::port::graph::OverlayLink;
use crate::geometry::overlay::port::graph::OverlayLinkFilter;
use crate::geometry::overlay::port::point::IntPoint;
use crate::geometry::overlay::port::point::Triangle;
use crate::geometry::overlay::port::scan::{KeyExpCollection, KeyExpList, KeyExpTree};
use crate::geometry::overlay::port::segment::End;
use crate::geometry::overlay::port::segment::IdPoint;
use crate::geometry::overlay::port::segment::ShapeCountBoolean;
use crate::geometry::overlay::port::segment::WindingCount;
use crate::geometry::overlay::port::segment::{
    ALL, BOTH_BOTTOM, BOTH_TOP, NONE, SUBJ_BOTH, SUBJ_BOTTOM, SUBJ_TOP, Segment, SegmentFill,
};
use crate::geometry::overlay::port::sort::TwoKeysSort;
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

pub(crate) struct GraphBuilder<C> {
    sweep_runner: SweepRunner<C>,
    pub(super) links: Vec<OverlayLink>,
    pub(super) node_offsets: Vec<u32>,
    pub(super) node_indices: Vec<u32>,
    pub(super) node_scratch: Vec<usize>,
    pub(super) fills: Vec<SegmentFill>,
    pub(super) ends: Vec<End>,
    pub(super) ends_sort_buffer: Vec<End>,
    pub(super) point_sort_buffer: Vec<IntPoint>,
}

impl<C> GraphBuilder<C>
where
    C: WindingCount,
{
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            sweep_runner: SweepRunner::new(),
            links: Vec::new(),
            node_offsets: Vec::new(),
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
    S: KeyExpCollection<C>,
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
    list: Option<KeyExpList<C>>,
    tree: Option<KeyExpTree<C>>,
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
    fn take_scan_list(&mut self, capacity: usize) -> KeyExpList<C> {
        if let Some(mut list) = self.list.take() {
            list.clear();
            list.reserve_capacity(capacity);
            list
        } else {
            KeyExpList::new(capacity)
        }
    }

    #[inline]
    fn take_scan_tree(&mut self, capacity: usize) -> KeyExpTree<C> {
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

impl GraphBuilder<ShapeCountBoolean> {
    #[inline]
    pub(crate) fn build_boolean_overlay(
        &mut self,
        fill_rule: FillRule,
        overlay_rule: OverlayRule,
        options: IntOverlayOptions,
        solver: &Solver,
        segments: &[Segment<ShapeCountBoolean>],
    ) -> OverlayGraph<'_> {
        self.build_boolean_fills(fill_rule, solver, segments);
        match overlay_rule {
            OverlayRule::Subject => self.build_links_by_filter::<SubjectFilter>(segments),
            OverlayRule::Intersect => self.build_links_by_filter::<IntersectFilter>(segments),
        }
        self.boolean_graph(options)
    }

    #[inline]
    fn build_boolean_fills(
        &mut self,
        fill_rule: FillRule,
        solver: &Solver,
        segments: &[Segment<ShapeCountBoolean>],
    ) {
        match fill_rule {
            FillRule::NonZero => {
                self.build_fills_with_strategy::<NonZeroStrategy>(solver, segments);
            }
        }
    }

    #[inline]
    fn boolean_graph(&mut self, options: IntOverlayOptions) -> OverlayGraph<'_> {
        self.build_nodes_and_connect_links();
        OverlayGraph {
            node_offsets: &self.node_offsets,
            node_indices: &self.node_indices,
            links: &self.links,
            options,
        }
    }
}

impl FillStrategy<ShapeCountBoolean> for NonZeroStrategy {
    #[inline(always)]
    fn add_and_fill(
        this: ShapeCountBoolean,
        bot: ShapeCountBoolean,
    ) -> (ShapeCountBoolean, SegmentFill) {
        let top = bot.add(this);
        let subj_top = (top.subj != 0) as SegmentFill;
        let subj_bot = (bot.subj != 0) as SegmentFill;
        let clip_top = (top.clip != 0) as SegmentFill;
        let clip_bot = (bot.clip != 0) as SegmentFill;

        let fill = subj_top | (subj_bot << 1) | (clip_top << 2) | (clip_bot << 3);

        (top, fill)
    }
}

struct SubjectFilter;
struct IntersectFilter;

impl InclusionFilterStrategy for SubjectFilter {
    #[inline(always)]
    fn is_included(fill: SegmentFill) -> bool {
        fill.is_subject()
    }
}

impl InclusionFilterStrategy for IntersectFilter {
    #[inline(always)]
    fn is_included(fill: SegmentFill) -> bool {
        fill.is_intersect()
    }
}

trait BooleanFillFilter {
    fn is_subject(&self) -> bool;
    fn is_intersect(&self) -> bool;
}

impl BooleanFillFilter for SegmentFill {
    #[inline(always)]
    fn is_subject(&self) -> bool {
        let fill = *self;
        let subj = fill & SUBJ_BOTH;
        subj == SUBJ_TOP || subj == SUBJ_BOTTOM
    }

    #[inline(always)]
    fn is_intersect(&self) -> bool {
        let fill = *self;
        let top = fill & BOTH_TOP;
        let bottom = fill & BOTH_BOTTOM;

        (top == BOTH_TOP || bottom == BOTH_BOTTOM) && fill != ALL
    }
}

impl OverlayLinkFilter for [OverlayLink] {
    #[inline]
    fn filter_by_overlay_into(&self, overlay_rule: OverlayRule, buffer: &mut Vec<VisitState>) {
        match overlay_rule {
            OverlayRule::Subject => filter_subject_into(self, buffer),
            OverlayRule::Intersect => filter_intersect_into(self, buffer),
        }
    }
}

#[inline]
fn filter_subject_into(links: &[OverlayLink], buffer: &mut Vec<VisitState>) {
    buffer.clear();
    buffer.reserve_capacity(links.len());
    for link in links {
        buffer.push(VisitState::new(!link.fill.is_subject()));
    }
}

#[inline]
fn filter_intersect_into(links: &[OverlayLink], buffer: &mut Vec<VisitState>) {
    buffer.clear();
    buffer.reserve_capacity(links.len());
    for link in links {
        buffer.push(VisitState::new(!link.fill.is_intersect()));
    }
}

impl<C> GraphBuilder<C>
where
    C: WindingCount,
{
    pub(crate) fn test_contour_for_loops(
        &mut self,
        contour: &[IntPoint],
        buffer: &mut Vec<IntPoint>,
    ) -> bool {
        let n = contour.len();
        if n < 64 {
            for (i, a) in contour[..n.saturating_sub(1)].iter().enumerate() {
                if contour[i + 1..].contains(a) {
                    return true;
                }
            }
            return false;
        }

        buffer.clear();
        buffer.extend_from_slice(contour);
        buffer.sort_by_two_keys_and_buffer(&mut self.point_sort_buffer, |p| p.x, |p| p.y);

        buffer.windows(2).any(|w| w[0] == w[1])
    }
}
