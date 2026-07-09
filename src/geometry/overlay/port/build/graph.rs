use crate::geometry::overlay::port::build::builder::{GraphBuilder, GraphNode};
use crate::geometry::overlay::port::core::solver::Solver;
use crate::geometry::overlay::port::geom::end::End;
use crate::geometry::overlay::port::ksort::sort::two_keys::TwoKeysSort;
use crate::geometry::overlay::port::segm::winding::WindingCount;
use alloc::vec::Vec;

impl<C, N> GraphBuilder<C, N>
where
    C: WindingCount,
    N: GraphNode,
{
    pub(super) fn build_nodes_and_connect_links(&mut self, solver: &Solver) {
        let n = self.links.len();
        if n == 0 {
            return;
        }

        self.build_ends(solver);

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
    fn build_ends(&mut self, solver: &Solver) {
        self.ends.clear();
        self.ends.reserve(self.links.len());
        for (i, link) in self.links.iter().enumerate() {
            self.ends.push(End {
                index: i,
                point: link.b.point,
            });
        }
        self.ends.sort_by_two_keys_and_buffer(
            false,
            &mut self.ends_sort_buffer,
            |e| e.point.x,
            |e| e.point.y,
        );
    }
}
