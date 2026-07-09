//! This module provides methods to simplify paths and shapes by reducing complexity
//! (e.g., removing small artifacts or shapes below a certain area threshold) based on a build rule.

use crate::geometry::overlay::port::ContourDirection;
use crate::geometry::overlay::port::ContourDirection::Clockwise;
use crate::geometry::overlay::port::FillRule;
use crate::geometry::overlay::port::extract::ContourExtension;
use crate::geometry::overlay::port::extract::IntShapes;
use crate::geometry::overlay::port::extract::OverlayRule;
use crate::geometry::overlay::port::point::IntPoint;
use crate::geometry::overlay::port::segment::BuildSegments;
use crate::geometry::overlay::port::{Overlay, ShapeType};

enum ContourFillDirection {
    Reverse,
    Correct,
}

impl Overlay {
    #[inline]
    pub fn simplify_contour_into(
        &mut self,
        contour: &[IntPoint],
        fill_rule: FillRule,
        out: &mut IntShapes,
    ) -> bool {
        self.clear();

        let is_perfect = self.find_intersections(contour);

        if is_perfect {
            // the path is already perfect
            // need to check fill rule direction
            let fill_direction =
                Self::contour_direction(self.options.output_direction, fill_rule, contour);

            return match fill_direction {
                ContourFillDirection::Reverse => {
                    self.with_boolean_buffer(|buffer| {
                        buffer.push_reversed_contour_into(contour, out);
                    });
                    true
                }
                ContourFillDirection::Correct => false,
            };
        }

        let mut boolean_buffer = self.boolean_buffer.take().unwrap_or_default();

        self.graph_builder
            .build_boolean_overlay(
                fill_rule,
                OverlayRule::Subject,
                self.options,
                &self.solver,
                &self.segments,
            )
            .extract_shapes_into(OverlayRule::Subject, &mut boolean_buffer, out);

        self.boolean_buffer = Some(boolean_buffer);

        true
    }

    #[inline]
    fn contour_direction(
        output_direction: ContourDirection,
        fill_rule: FillRule,
        contour: &[IntPoint],
    ) -> ContourFillDirection {
        let contour_clockwise = contour.is_clockwise_ordered();
        let output_clockwise = output_direction == Clockwise;

        match fill_rule {
            FillRule::NonZero => {
                if contour_clockwise != output_clockwise {
                    ContourFillDirection::Reverse
                } else {
                    ContourFillDirection::Correct
                }
            }
        }
    }

    fn find_intersections(&mut self, contour: &[IntPoint]) -> bool {
        let append_modified = self.segments.append_path_iter(
            contour.iter().copied(),
            ShapeType::Subject,
            self.options.preserve_input_collinear,
        );

        let split_modified = self
            .split_solver
            .split_segments(&mut self.segments, &self.solver);

        if split_modified || append_modified || self.segments.is_empty() {
            return false;
        }

        let mut buffer = self.boolean_buffer.take().unwrap_or_default();
        let has_loops = self
            .graph_builder
            .test_contour_for_loops(contour, &mut buffer.points);
        self.boolean_buffer = Some(buffer);

        !has_loops
    }
}
