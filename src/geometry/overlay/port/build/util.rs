use crate::geometry::overlay::port::build::builder::{GraphBuilder, GraphNode};
use crate::geometry::overlay::port::prim::IntPoint;
use crate::geometry::overlay::port::segm::winding::WindingCount;
use crate::geometry::overlay::port::sort::TwoKeysSort;
use alloc::vec::Vec;

impl<C, N> GraphBuilder<C, N>
where
    C: WindingCount,
    N: GraphNode,
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
