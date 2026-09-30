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
    /// Replaces `out`, as `overlay_into_nested` does: it is recycled on entry.
    /// `false` means the contour needs no rebuilding and `out` is empty;
    /// `true` means `out` holds the result (possibly nothing, if it collapsed).
    ///
    /// Models upstream `Overlay::simplify_contour`, whose perfect-contour
    /// fast path applies no `min_output_area` filter. Upstream's newer
    /// `simplify_source` takes that path only at a zero threshold; every
    /// production caller filters by area after this call, so the difference
    /// never reaches output.
    #[inline]
    pub fn simplify_contour_into(
        &mut self,
        contour: &[IntPoint],
        fill_rule: FillRule,
        out: &mut IntShapes,
    ) -> bool {
        self.simplify_contour_into_slow(contour, fill_rule, out)
    }

    /// The un-screened engine body. Kept separate so the screen's soundness
    /// test remains an independent reference when a future landing adds the
    /// conditional early return above this call.
    #[inline]
    fn simplify_contour_into_slow(
        &mut self,
        contour: &[IntPoint],
        fill_rule: FillRule,
        out: &mut IntShapes,
    ) -> bool {
        self.recycle_shapes(out);
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

/// True only for a strictly convex, CCW-wound, non-self-intersecting ring with
/// no collinear or duplicate vertices. This is conservative: any doubt is a
/// rejection, leaving the caller to use the full overlay solver.
///
/// Every cyclic triple must turn strictly left, which rejects collinear and
/// duplicate vertices while fixing both convexity and winding. The two sign
/// flips in each edge-vector component prove one revolution; left turns alone
/// would also admit a self-lapping spiral.
///
/// Retained after the E1 close (the strict-convex screen priced out below its
/// proceed threshold on norway - see reference/performance.md) as the predicate
/// and soundness gate for the follow-up E1b exact-classifier item. Currently
/// exercised only by the tests below, hence `cfg(test)`.
#[cfg(test)]
fn is_perfect_ccw_convex(contour: &[IntPoint]) -> bool {
    let n = contour.len();
    if n < 3 {
        return false;
    }

    let mut x = FlipCounter::default();
    let mut y = FlipCounter::default();
    for i in 0..n {
        let a = contour[i];
        let b = contour[(i + 1) % n];
        let c = contour[(i + 2) % n];
        let cross = (i128::from(b.x) - i128::from(a.x)) * (i128::from(c.y) - i128::from(a.y))
            - (i128::from(b.y) - i128::from(a.y)) * (i128::from(c.x) - i128::from(a.x));
        if cross <= 0 {
            return false;
        }
        x.push(i64::from(b.x) - i64::from(a.x));
        y.push(i64::from(b.y) - i64::from(a.y));
    }

    x.finish() == 2 && y.finish() == 2
}

#[cfg(test)]
#[derive(Default)]
struct FlipCounter {
    first: i8,
    previous: i8,
    flips: u32,
}

#[cfg(test)]
impl FlipCounter {
    #[inline]
    fn push(&mut self, delta: i64) {
        let sign = delta.signum() as i8;
        if sign == 0 {
            return;
        }
        if self.first == 0 {
            self.first = sign;
        } else if sign != self.previous {
            self.flips += 1;
        }
        self.previous = sign;
    }

    #[inline]
    fn finish(mut self) -> u32 {
        if self.first != 0 && self.previous != self.first {
            self.flips += 1;
        }
        self.flips
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::overlay::port::IntOverlayOptions;

    fn ring(points: &[(i32, i32)]) -> Vec<IntPoint> {
        points.iter().copied().map(IntPoint::from).collect()
    }

    fn assert_slow_verdict(contour: &[IntPoint]) {
        let mut overlay = Overlay::new_custom(0, IntOverlayOptions::default(), Default::default());
        let mut out = IntShapes::new();
        let rebuilt = overlay.simplify_contour_into_slow(contour, FillRule::NonZero, &mut out);
        assert!(
            !rebuilt,
            "screen accepted a contour rebuilt by the slow body: {contour:?}"
        );
        assert!(
            out.is_empty(),
            "screen accepted a contour that changed output: {contour:?}"
        );
    }

    #[test]
    fn perfect_ccw_convex_screen_cases() {
        let octagon = [
            (-2, -1),
            (-1, -2),
            (1, -2),
            (2, -1),
            (2, 1),
            (1, 2),
            (-1, 2),
            (-2, 1),
        ];
        for contour in [
            ring(&[(0, 0), (8, 0), (0, 5)]),
            ring(&[(-100, -50), (200, -50), (200, 100), (-100, 100)]),
            ring(&octagon),
            octagon
                .iter()
                .map(|&(x, y)| IntPoint::new(x * 10_000 + 17, y * 10_000 - 29))
                .collect(),
        ] {
            assert!(is_perfect_ccw_convex(&contour), "{contour:?}");
            assert_slow_verdict(&contour);
        }

        for contour in [
            ring(&[(0, 0), (0, 5), (8, 0)]),                 // CW
            ring(&[(0, 0), (8, 0), (3, 2), (8, 8), (0, 8)]), // concave
            ring(&[(0, 0), (4, 0), (8, 0), (8, 8), (0, 8)]), // collinear
            ring(&[(0, 0), (8, 0), (8, 0), (8, 8), (0, 8)]), // duplicate
            ring(&[(0, 0), (8, 8), (0, 8), (8, 0)]),         // bowtie
            ring(&[
                (0, 0),
                (6, 0),
                (6, 6),
                (0, 6),
                (1, 1),
                (5, 1),
                (5, 5),
                (1, 5),
            ]),
            // Self-lapping {5/2} pentagram: every cyclic triple turns strictly
            // left (all cross > 0), so only the revolution check rejects it -
            // this is the case that proves the two-flip guard is load-bearing
            // and left turns alone are unsound.
            ring(&[(100, 0), (-81, 59), (31, -95), (31, 95), (-81, -59)]),
            ring(&[]),
            ring(&[(0, 0)]),
            ring(&[(0, 0), (1, 0)]),
        ] {
            assert!(!is_perfect_ccw_convex(&contour), "{contour:?}");
        }
    }

    #[test]
    fn perfect_ccw_convex_screen_is_sound_on_random_contours() {
        let mut rng = Lcg::new(0x6f9d_52a1_3b70_cafe);
        for _case in 0..10_000 {
            let len = rng.usize(12);
            let contour: Vec<_> = (0..len)
                .map(|_| IntPoint::new(rng.coord(), rng.coord()))
                .collect();
            if is_perfect_ccw_convex(&contour) {
                assert_slow_verdict(&contour);
            }
        }

        // Random affine copies guarantee that the corpus also exercises many
        // accepted contours, rather than relying on a rare hit from arbitrary
        // point clouds.
        let base = [
            (-2, -1),
            (-1, -2),
            (1, -2),
            (2, -1),
            (2, 1),
            (1, 2),
            (-1, 2),
            (-2, 1),
        ];
        for _case in 0..2_000 {
            let scale = 1 + i32::try_from(rng.usize(10_000)).expect("bounded scale fits i32");
            let dx = rng.coord();
            let dy = rng.coord();
            let contour: Vec<_> = base
                .iter()
                .map(|&(x, y)| IntPoint::new(x * scale + dx, y * scale + dy))
                .collect();
            assert!(is_perfect_ccw_convex(&contour));
            assert_slow_verdict(&contour);
        }
    }

    struct Lcg {
        state: u64,
    }

    impl Lcg {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next(&mut self) -> u32 {
            self.state = self
                .state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (self.state >> 32) as u32
        }

        fn usize(&mut self, upper: usize) -> usize {
            if upper == 0 {
                return 0;
            }
            usize::try_from(self.next()).expect("u32 fits usize") % upper
        }

        fn coord(&mut self) -> i32 {
            i32::try_from(self.next() % 101).expect("bounded coordinate fits i32") - 50
        }
    }
}
