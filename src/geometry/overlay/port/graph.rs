//! This module defines the graph structure that represents the relationships between the paths in
//! subject and clip polygons after boolean operations. The graph helps in extracting final shapes
//! based on the overlay rule applied.

use crate::geometry::overlay::port::IntOverlayOptions;
use crate::geometry::overlay::port::extract::OverlayRule;
use crate::geometry::overlay::port::extract::VisitState;
use crate::geometry::overlay::port::fill::{GraphBuilder, GraphNode};
use crate::geometry::overlay::port::segment::End;
use crate::geometry::overlay::port::segment::IdPoint;
use crate::geometry::overlay::port::segment::SegmentFill;
use crate::geometry::overlay::port::segment::WindingCount;
use crate::geometry::overlay::port::sort::TwoKeysSort;
use alloc::vec::Vec;

/// A representation of geometric shapes organized for efficient boolean operations.
///
/// `OverlayGraph` is a core structure designed to facilitate the execution of boolean operations on shapes, such as union, intersection, and difference. It organizes and preprocesses geometric data, making it optimized for these operations. This struct is the result of compiling shape data into a form where boolean operations can be applied directly, efficiently managing the complex relationships between different geometric entities.
///
/// Use `OverlayGraph` to perform boolean operations on the geometric shapes you've added to an `Overlay`, after it has processed the shapes according to the specified build and overlay rules.
/// [More information](https://ishape-rust.github.io/iShape-js/overlay/overlay_graph/overlay_graph.html) about Overlay Graph.
pub struct OverlayGraph<'a> {
    pub(crate) options: IntOverlayOptions,
    pub(crate) nodes: &'a [OverlayNode],
    pub(crate) node_indices: &'a [u32],
    pub(crate) links: &'a [OverlayLink],
}

#[derive(Clone, Copy)]
pub(crate) struct OverlayNode {
    offset: u32,
    len: u32,
}

impl GraphNode for OverlayNode {
    #[inline]
    fn with_indices(indices: &[usize], node_indices: &mut Vec<u32>) -> Self {
        let offset = node_indices.len();
        node_indices.extend(indices.iter().map(|&index| index as u32));
        Self {
            offset: offset as u32,
            len: indices.len() as u32,
        }
    }
}

impl OverlayNode {
    #[inline]
    pub(crate) fn indices<'a>(&self, node_indices: &'a [u32]) -> &'a [u32] {
        let start = self.offset as usize;
        let end = start + self.len as usize;
        unsafe { node_indices.get_unchecked(start..end) }
    }
}

#[derive(Clone)]
pub(crate) struct OverlayLink {
    pub(crate) a: IdPoint,
    pub(crate) b: IdPoint,
    pub(crate) fill: SegmentFill,
}

impl OverlayLink {
    #[inline(always)]
    pub(crate) fn new(a: IdPoint, b: IdPoint, fill: SegmentFill) -> OverlayLink {
        OverlayLink { a, b, fill }
    }

    #[inline(always)]
    pub(crate) fn other(&self, node_id: usize) -> IdPoint {
        if self.a.id == node_id { self.b } else { self.a }
    }

    #[inline(always)]
    pub(crate) fn is_direct(&self) -> bool {
        self.a.point < self.b.point
    }
}

pub(crate) trait OverlayLinkFilter {
    fn filter_by_overlay_into(&self, overlay_rule: OverlayRule, buffer: &mut Vec<VisitState>);
}

impl<C, N> GraphBuilder<C, N>
where
    C: WindingCount,
    N: GraphNode,
{
    pub(super) fn build_nodes_and_connect_links(&mut self) {
        let n = self.links.len();
        if n == 0 {
            return;
        }

        self.build_ends();

        self.nodes.clear();
        self.nodes.reserve(n);
        self.node_indices.clear();
        self.node_indices.reserve(n * 2);

        let mut ai = 0;
        let mut bi = 0;
        self.node_scratch.clear();

        while ai < n || bi < n {
            let (a_cnt, next_ai, a_point) = if ai < n {
                let point = self.links[ai].a.point;
                let mut end = ai + 1;
                while end < n && self.links[end].a.point == point {
                    end += 1;
                }
                (end - ai, end, Some(point))
            } else {
                (0, ai, None)
            };

            let (b_cnt, next_bi, b_point) = if bi < n {
                let point = self.ends[bi].point;
                let mut end = bi + 1;
                while end < n && self.ends[end].point == point {
                    end += 1;
                }
                (end - bi, end, Some(point))
            } else {
                (0, bi, None)
            };

            let (consume_a, consume_b) = match (a_point, b_point) {
                (Some(a), Some(b)) if a == b => (a_cnt, b_cnt),
                (Some(a), Some(b)) if a < b => (a_cnt, 0),
                (Some(_), Some(_)) => (0, b_cnt),
                (Some(_), None) => (a_cnt, 0),
                (None, Some(_)) => (0, b_cnt),
                (None, None) => break,
            };

            let node_id = self.nodes.len();

            if consume_a > 0 {
                let start = ai;
                let end = ai + consume_a;
                for idx in start..end {
                    self.links[idx].a.id = node_id;
                    self.node_scratch.push(idx);
                }
                ai = next_ai;
            }

            if consume_b > 0 {
                let start = bi;
                let end = bi + consume_b;
                for idx in start..end {
                    let link_idx = self.ends[idx].index;
                    self.node_scratch.push(link_idx);
                    self.links[link_idx].b.id = node_id;
                }
                bi = next_bi;
            }

            debug_assert!(!self.node_scratch.is_empty());
            self.nodes.push(N::with_indices(
                self.node_scratch.as_slice(),
                &mut self.node_indices,
            ));
            self.node_scratch.clear();
        }
    }

    #[inline]
    fn build_ends(&mut self) {
        self.ends.clear();
        self.ends.reserve(self.links.len());
        for (i, link) in self.links.iter().enumerate() {
            self.ends.push(End {
                index: i,
                point: link.b.point,
            });
        }
        self.ends.sort_by_two_keys_and_buffer(
            &mut self.ends_sort_buffer,
            |e| e.point.x,
            |e| e.point.y,
        );
    }
}
