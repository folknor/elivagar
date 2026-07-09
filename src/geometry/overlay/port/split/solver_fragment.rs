use crate::geometry::overlay::port::core::solver::Solver;
use crate::geometry::overlay::port::segm::boolean::ShapeCountBoolean;
use crate::geometry::overlay::port::segm::segment::Segment;
use crate::geometry::overlay::port::split::cross_solver::{CrossSolver, CrossType, EndMask};
use crate::geometry::overlay::port::split::fragment::Fragment;
use crate::geometry::overlay::port::split::grid_layout::{
    BorderVSegment, FragmentBuffer, GridLayout,
};
use crate::geometry::overlay::port::split::line_mark::LineMark;
use crate::geometry::overlay::port::split::snap_radius::SnapRadius;
use crate::geometry::overlay::port::split::solver::SplitSolver;
use alloc::vec::Vec;

impl SplitSolver {
    pub(super) fn fragment_split(
        &mut self,
        snap_radius: SnapRadius,
        segments: &mut Vec<Segment<ShapeCountBoolean>>,
        solver: &Solver,
    ) -> bool {
        let layout = if let Some(layout) =
            GridLayout::new(segments.iter().map(|it| it.x_segment), segments.len())
        {
            layout
        } else {
            return self.tree_split(snap_radius, segments, solver);
        };

        self.fragment_buffer.reset_layout(layout);

        let mut need_to_fix = true;
        let mut any_intersection = false;

        let mut snap_radius = snap_radius;

        while need_to_fix && segments.len() > 2 {
            self.marks.clear();

            self.fragment_buffer
                .init_fragment_buffer(segments.iter().map(|it| it.x_segment));
            for (i, segment) in segments.iter().enumerate() {
                self.fragment_buffer.add_segment(i, segment.x_segment);
            }

            let mut buffer = core::mem::replace(&mut self.fragment_buffer, FragmentBuffer::empty());
            need_to_fix = self.process(snap_radius.radius(), &mut buffer, solver);

            #[cfg(debug_assertions)]
            debug_assert!(buffer.is_on_border_sorted());
            let mut j = 0;
            while j < buffer.on_border.len() {
                let j0 = j;
                let x = buffer.on_border[j].x;
                j += 1;
                while j < buffer.on_border.len() && x == buffer.on_border[j].x {
                    j += 1;
                }

                let index = buffer.layout.index(x);
                if let Some(fragments) = buffer.groups.get(index) {
                    self.on_border_split(x, fragments, &mut buffer.on_border[j0..j]);
                }
            }

            if self.marks.is_empty() {
                self.fragment_buffer = buffer;
                return any_intersection;
            }

            any_intersection = true;
            buffer.clear();
            self.fragment_buffer = buffer;

            self.apply(segments);

            snap_radius.increment();
        }

        any_intersection
    }

    #[inline]
    fn process(&mut self, radius: i64, buffer: &mut FragmentBuffer, _solver: &Solver) -> bool {
        let mut is_any_round = false;
        for group in buffer.groups.iter_mut().take(buffer.active_groups) {
            if group.is_empty() {
                continue;
            }
            let any_round = Self::bin_split(radius, group, &mut self.marks);
            is_any_round = is_any_round || any_round;
        }
        is_any_round
    }

    fn bin_split(radius: i64, fragments: &mut [Fragment], marks: &mut Vec<LineMark>) -> bool {
        if fragments.len() < 2 {
            return false;
        }

        fragments.sort_unstable_by_key(|a| a.rect.min_y);

        let mut any_round = false;

        for (i, fi) in fragments.iter().enumerate().take(fragments.len() - 1) {
            for fj in fragments.iter().skip(i + 1) {
                if fi.rect.max_y < fj.rect.min_y {
                    break;
                }
                if !fi.rect.is_intersect_border_include(&fj.rect) {
                    continue;
                }

                // MARK: the intersection, ensuring the right order for deterministic results

                let is_round = if fi.x_segment < fj.x_segment {
                    Self::cross_fragments(fi, fj, radius, marks)
                } else {
                    Self::cross_fragments(fj, fi, radius, marks)
                };

                any_round = any_round || is_round
            }
        }

        any_round
    }

    fn on_border_split(
        &mut self,
        border_x: i32,
        fragments: &[Fragment],
        vertical_segments: &mut [BorderVSegment],
    ) {
        self.border_points.clear();
        for fragment in fragments.iter() {
            if fragment.x_segment.b.x == border_x {
                self.border_points.push(fragment.x_segment.b)
            }
        }

        if self.border_points.is_empty() {
            return;
        }

        self.border_points.sort_unstable_by_key(|p0| p0.y);
        vertical_segments.sort_by_key(|s0| s0.y_range.min);

        let mut i = 0;
        for s in vertical_segments.iter() {
            while i < self.border_points.len() && self.border_points[i].y <= s.y_range.min {
                i += 1;
            }
            let mut j = i;
            while j < self.border_points.len() && self.border_points[j].y < s.y_range.max {
                self.marks.push(LineMark {
                    index: s.id,
                    point: self.border_points[j],
                });
                j += 1;
            }
        }
    }

    fn cross_fragments(
        fi: &Fragment,
        fj: &Fragment,
        radius: i64,
        marks: &mut Vec<LineMark>,
    ) -> bool {
        let cross = if let Some(cross) = CrossSolver::cross(&fi.x_segment, &fj.x_segment, radius) {
            cross
        } else {
            return false;
        };

        let r = crate::geometry::overlay::port::prim::i_from_wide(radius);

        match cross.cross_type {
            CrossType::Overlay => {
                let mask = CrossSolver::collinear(&fi.x_segment, &fj.x_segment);
                if mask == 0 {
                    return false;
                }

                if !(fi.rect.contains_with_radius(fi.x_segment.a, r)
                    || fj.rect.contains_with_radius(fi.x_segment.a, r))
                {
                    return false;
                }

                if mask.is_target_a() {
                    marks.push(LineMark {
                        index: fj.index,
                        point: fi.x_segment.a,
                    });
                }

                if mask.is_target_b() {
                    marks.push(LineMark {
                        index: fj.index,
                        point: fi.x_segment.b,
                    });
                }

                if mask.is_other_a() {
                    marks.push(LineMark {
                        index: fi.index,
                        point: fj.x_segment.a,
                    });
                }

                if mask.is_other_b() {
                    marks.push(LineMark {
                        index: fi.index,
                        point: fj.x_segment.b,
                    });
                }
            }
            _ => {
                if !fi.rect.contains_with_radius(cross.point, r)
                    || !fj.rect.contains_with_radius(cross.point, r)
                {
                    return false;
                }

                match cross.cross_type {
                    CrossType::Pure => {
                        marks.push(LineMark {
                            index: fi.index,
                            point: cross.point,
                        });
                        marks.push(LineMark {
                            index: fj.index,
                            point: cross.point,
                        });
                    }
                    CrossType::TargetEnd => {
                        marks.push(LineMark {
                            index: fj.index,
                            point: cross.point,
                        });
                    }
                    CrossType::OtherEnd => {
                        marks.push(LineMark {
                            index: fi.index,
                            point: cross.point,
                        });
                    }
                    _ => {}
                }
            }
        }

        cross.is_round
    }
}
