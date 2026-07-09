use crate::geometry::overlay::port::build::builder::{GraphBuilder, InclusionFilterStrategy};
use crate::geometry::overlay::port::build::sweep::{FillStrategy, NonZeroStrategy};
use crate::geometry::overlay::port::core::extract::VisitState;
use crate::geometry::overlay::port::core::fill_rule::FillRule;
use crate::geometry::overlay::port::core::graph::OverlayGraph;
use crate::geometry::overlay::port::core::graph::OverlayNode;
use crate::geometry::overlay::port::core::link::OverlayLink;
use crate::geometry::overlay::port::core::link::OverlayLinkFilter;
use crate::geometry::overlay::port::core::overlay::IntOverlayOptions;
use crate::geometry::overlay::port::core::overlay_rule::OverlayRule;
use crate::geometry::overlay::port::core::solver::Solver;
use crate::geometry::overlay::port::segm::boolean::ShapeCountBoolean;
use crate::geometry::overlay::port::segm::segment::{
    ALL, BOTH_BOTTOM, BOTH_TOP, SUBJ_BOTH, SUBJ_BOTTOM, SUBJ_TOP, Segment, SegmentFill,
};
use crate::geometry::overlay::port::segm::winding::WindingCount;
use crate::geometry::overlay::port::shape::Reserve;
use alloc::vec::Vec;

impl GraphBuilder<ShapeCountBoolean, OverlayNode> {
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
        self.boolean_graph(options, solver)
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
                self.build_fills_with_strategy::<NonZeroStrategy>(solver, segments)
            }
        }
    }

    #[inline]
    fn boolean_graph(&mut self, options: IntOverlayOptions, solver: &Solver) -> OverlayGraph<'_> {
        self.build_nodes_and_connect_links(solver);
        OverlayGraph {
            nodes: &self.nodes,
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
    for link in links.iter() {
        buffer.push(VisitState::new(!link.fill.is_subject()));
    }
}

#[inline]
fn filter_intersect_into(links: &[OverlayLink], buffer: &mut Vec<VisitState>) {
    buffer.clear();
    buffer.reserve_capacity(links.len());
    for link in links.iter() {
        buffer.push(VisitState::new(!link.fill.is_intersect()));
    }
}
