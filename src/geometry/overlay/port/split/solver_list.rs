use crate::geometry::overlay::port::core::solver::Solver;
use crate::geometry::overlay::port::segm::boolean::ShapeCountBoolean;
use crate::geometry::overlay::port::segm::segment::Segment;
use crate::geometry::overlay::port::split::snap_radius::SnapRadius;
use crate::geometry::overlay::port::split::solver::SplitSolver;
use alloc::vec::Vec;

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
                    need_to_fix = need_to_fix || is_round
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
