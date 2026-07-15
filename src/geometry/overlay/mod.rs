//! Integer polygon overlay engine extracted from i_overlay 7.0.2.
//!
//! Portions of this module are derived from i_overlay and its helper crates,
//! copyright Nail Sharipov and licensed under MIT OR Apache-2.0. The pristine
//! reference sources are kept under `research/`.
//!
//! The ocean emission path runs on this engine and its output is cached in
//! the durable world-ocean artifact. A change that alters result bits must
//! bump OCEAN_POLICY_VERSION (src/ocean.rs) - without it, stale artifacts
//! keep key-validating and serve the pre-change geometry.

mod port;

use port::FillRule;
use port::{ContourDirection, IntOverlayOptions, Overlay, OverlayRule};

pub(crate) use port::IntPoint;

pub(crate) type Contour = Vec<IntPoint>;
pub(crate) type Shape = Vec<Contour>;
pub(crate) type Shapes = Vec<Shape>;

#[derive(Clone, Copy)]
pub(crate) enum ShapeType {
    Subject,
    Clip,
}

#[derive(Clone, Copy)]
pub(crate) enum BoolRule {
    Subject,
    Intersect,
}

pub(crate) struct BoolOverlay {
    inner: Overlay,
    pub(crate) min_output_area: u64,
}

impl BoolOverlay {
    pub(crate) fn new() -> Self {
        Self {
            inner: Overlay::new_custom(0, overlay_options(0), Default::default()),
            min_output_area: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.inner.clear();
    }

    pub(crate) fn add_contour(&mut self, contour: &[IntPoint], shape_type: ShapeType) {
        self.inner
            .add_contour(contour, shape_type.into_port_shape_type());
    }

    pub(crate) fn add_shape(&mut self, shape: &Shape, shape_type: ShapeType) {
        self.inner
            .add_shape(shape, shape_type.into_port_shape_type());
    }

    pub(crate) fn overlay_nested(&mut self, rule: BoolRule, out: &mut Shapes) {
        self.sync_options();
        self.inner
            .overlay_into_nested(rule.into_port_rule(), FillRule::NonZero, out);
    }

    pub(crate) fn simplify_contour_into(&mut self, contour: &[IntPoint], out: &mut Shapes) -> bool {
        self.sync_options();
        self.inner
            .simplify_contour_into(contour, FillRule::NonZero, out)
    }

    pub(crate) fn recycle(&mut self, shapes: &mut Shapes) {
        self.inner.recycle_shapes(shapes);
    }

    pub(crate) fn take_shape(&mut self, ring_count: usize) -> Shape {
        self.inner.take_shape(ring_count)
    }

    pub(crate) fn recycle_owned_shape(&mut self, shape: Shape) {
        self.inner.recycle_owned_shape(shape);
    }

    pub(crate) fn recycle_from(&mut self, shapes: &mut Shapes, start: usize) {
        self.inner.recycle_shapes_from(shapes, start);
    }

    pub(crate) fn recycle_shape(&mut self, shape: &mut Shape) {
        self.inner.recycle_shape(shape);
    }

    pub(crate) fn recycle_contours_from(&mut self, shape: &mut Shape, start: usize) {
        self.inner.recycle_contours_from(shape, start);
    }

    fn sync_options(&mut self) {
        self.inner.options = overlay_options(self.min_output_area);
    }
}

impl ShapeType {
    fn into_port_shape_type(self) -> port::ShapeType {
        match self {
            Self::Subject => port::ShapeType::Subject,
            Self::Clip => port::ShapeType::Clip,
        }
    }
}

impl BoolRule {
    fn into_port_rule(self) -> OverlayRule {
        match self {
            Self::Subject => OverlayRule::Subject,
            Self::Intersect => OverlayRule::Intersect,
        }
    }
}

fn overlay_options(min_area: u64) -> IntOverlayOptions {
    IntOverlayOptions {
        output_direction: ContourDirection::CounterClockwise,
        min_output_area: min_area,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use i_overlay::core::fill_rule::FillRule as OracleFillRule;
    use i_overlay::core::overlay::{
        ContourDirection as OracleContourDirection, IntOverlayOptions as OracleOptions,
        Overlay as OracleOverlay, ShapeType as OracleShapeType,
    };
    use i_overlay::core::overlay_rule::OverlayRule as OracleRule;
    use i_overlay::core::solver::Solver as OracleSolver;
    use i_overlay::i_float::int::point::IntPoint as OraclePoint;

    // The in-tree engine and the dev-dep oracle now use distinct IntPoint
    // types (prim::IntPoint vs i_overlay's i_float point); convert at the
    // boundary. This is what lets the oracle stay an independent crates.io
    // build with no [patch] shenanigans.
    fn to_oracle_contour(contour: &Contour) -> Vec<OraclePoint> {
        contour.iter().map(|p| OraclePoint::new(p.x, p.y)).collect()
    }

    fn to_oracle_shape(shape: &Shape) -> Vec<Vec<OraclePoint>> {
        shape.iter().map(to_oracle_contour).collect()
    }

    fn from_oracle_shapes(shapes: Vec<Vec<Vec<OraclePoint>>>) -> Shapes {
        shapes
            .into_iter()
            .map(|shape| {
                shape
                    .into_iter()
                    .map(|contour| {
                        contour
                            .into_iter()
                            .map(|p| IntPoint::new(p.x, p.y))
                            .collect()
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn differential_oracle_mixed_subject_and_intersect() {
        let mut rng = Lcg::new(0x8f9d_a421_3b70_cafe);
        for case in 0..2_000_u32 {
            let max_points = if case % 31 == 0 { 200 } else { 64 };
            let rings = 1 + rng.usize(6);
            let shape = generated_shape(&mut rng, rings, max_points);
            assert_same_subject(&shape);

            let rect = generated_rect(&mut rng);
            assert_same_intersect(&shape, &rect);
        }
    }

    #[test]
    fn differential_oracle_large_split_strategies() {
        let tree_case = grid_rects(35, 35, 6);
        assert_same_subject_with_solver(&tree_case, port::Solver::AUTO, OracleSolver::AUTO);
        assert_same_subject_with_solver(&tree_case, port::Solver::TREE, OracleSolver::TREE);
        assert_same_subject_with_solver(&tree_case, port::Solver::LIST, OracleSolver::LIST);

        let frag_case = grid_rects(65, 65, 4);
        assert_same_subject_with_solver(&frag_case, port::Solver::AUTO, OracleSolver::AUTO);
        assert_same_subject_with_solver(&frag_case, port::Solver::FRAG, OracleSolver::FRAG);

        // Intersect above the 4,000 / 16,000 thresholds, forced-strategy: the
        // clip is a rect covering part of the dense grid, so the boolean stays
        // in the tree / fragment split band while exercising the Intersect
        // link filter and hole binder there (not just Subject).
        let tree_clip = rect(3, 3, 150, 150);
        assert_same_intersect_with_solver(
            &tree_case,
            &tree_clip,
            port::Solver::AUTO,
            OracleSolver::AUTO,
        );
        assert_same_intersect_with_solver(
            &tree_case,
            &tree_clip,
            port::Solver::TREE,
            OracleSolver::TREE,
        );

        let frag_clip = rect(3, 3, 180, 180);
        assert_same_intersect_with_solver(
            &frag_case,
            &frag_clip,
            port::Solver::AUTO,
            OracleSolver::AUTO,
        );
        assert_same_intersect_with_solver(
            &frag_case,
            &frag_clip,
            port::Solver::FRAG,
            OracleSolver::FRAG,
        );
    }

    #[test]
    fn differential_oracle_reused_engine_recycles() {
        // Exercises the Landing 2 recycle/pool path, which no other test
        // reaches: a single BoolOverlay and a single output buffer reused
        // across many ops, recycled between calls exactly as normalize_into
        // and intersect_rect_into drive them. A pooled-ring or recycled-shell
        // state leak - the class brokkr regress on denmark would only catch if
        // denmark happened to contain the offending shape - shows up here
        // point-for-point against dev-dep i_overlay.
        let mut rng = Lcg::new(0x1234_5678_9abc_def0);
        let mut engine = BoolOverlay::new();
        let mut out: Shapes = Vec::new();
        for case in 0..2_000_u32 {
            let rings = 1 + rng.usize(6);
            let shape = generated_shape(&mut rng, rings, 64);

            // Subject, mirroring normalize_into's multi-contour arm: recycle
            // the previous output back into the pool, then run afresh.
            engine.recycle(&mut out);
            engine.clear();
            engine.add_shape(&shape, ShapeType::Subject);
            engine.overlay_nested(BoolRule::Subject, &mut out);
            let expected = oracle_overlay(&shape, None, OracleRule::Subject, OracleSolver::AUTO);
            assert_eq!(out, expected, "subject case {case}");

            // Single-contour simplify fast path against the same warm engine,
            // so the pooled ring bodies from the op above are reused here. The
            // None/Some verdict (perfect-and-wound vs reversed/empty/rebuilt)
            // must match the dev-dep exactly, not just the output values.
            let mut simplified: Shapes = Vec::new();
            engine.recycle(&mut simplified);
            let port_rebuilt = engine.simplify_contour_into(&shape[0], &mut simplified);
            match oracle_simplify(&shape[0]) {
                Some(expected) => {
                    assert!(port_rebuilt, "simplify verdict case {case}");
                    assert_eq!(simplified, expected, "simplify case {case}");
                }
                None => assert!(!port_rebuilt, "simplify verdict case {case}"),
            }
            engine.recycle(&mut simplified);

            // Intersect, mirroring intersect_rect_into, reusing the same engine
            // and out without dropping either.
            let rect = generated_rect(&mut rng);
            engine.recycle(&mut out);
            engine.clear();
            engine.add_shape(&shape, ShapeType::Subject);
            engine.add_contour(&rect, ShapeType::Clip);
            engine.overlay_nested(BoolRule::Intersect, &mut out);
            let expected = oracle_overlay(
                &shape,
                Some(&rect),
                OracleRule::Intersect,
                OracleSolver::AUTO,
            );
            assert_eq!(out, expected, "intersect case {case}");
        }
        engine.recycle(&mut out);
        assert!(out.is_empty());
    }

    fn oracle_simplify(contour: &Contour) -> Option<Shapes> {
        let mut overlay = OracleOverlay::new_custom(0, oracle_options(), OracleSolver::AUTO);
        overlay
            .simplify_contour(&to_oracle_contour(contour), OracleFillRule::NonZero)
            .map(from_oracle_shapes)
    }

    fn assert_same_subject(shape: &Shape) {
        let mut actual = BoolOverlay::new();
        actual.add_shape(shape, ShapeType::Subject);
        let mut actual_out = Vec::new();
        actual.overlay_nested(BoolRule::Subject, &mut actual_out);

        let expected = oracle_overlay(shape, None, OracleRule::Subject, OracleSolver::AUTO);
        assert_eq!(actual_out, expected);
    }

    fn assert_same_intersect(shape: &Shape, clip: &Contour) {
        let mut actual = BoolOverlay::new();
        actual.add_shape(shape, ShapeType::Subject);
        actual.add_contour(clip, ShapeType::Clip);
        let mut actual_out = Vec::new();
        actual.overlay_nested(BoolRule::Intersect, &mut actual_out);

        let expected = oracle_overlay(shape, Some(clip), OracleRule::Intersect, OracleSolver::AUTO);
        assert_eq!(actual_out, expected);
    }

    fn assert_same_subject_with_solver(
        shape: &Shape,
        solver: port::Solver,
        oracle_solver: OracleSolver,
    ) {
        let mut actual = port::Overlay::new_custom(0, port_options(), solver);
        actual.add_shape(shape, port::ShapeType::Subject);
        let actual_out = actual.overlay(port::OverlayRule::Subject, port::FillRule::NonZero);

        let expected = oracle_overlay(shape, None, OracleRule::Subject, oracle_solver);
        assert_eq!(actual_out, expected);
    }

    fn assert_same_intersect_with_solver(
        shape: &Shape,
        clip: &Contour,
        solver: port::Solver,
        oracle_solver: OracleSolver,
    ) {
        let mut actual = port::Overlay::new_custom(0, port_options(), solver);
        actual.add_shape(shape, port::ShapeType::Subject);
        actual.add_contour(clip, port::ShapeType::Clip);
        let actual_out = actual.overlay(port::OverlayRule::Intersect, port::FillRule::NonZero);

        let expected = oracle_overlay(shape, Some(clip), OracleRule::Intersect, oracle_solver);
        assert_eq!(actual_out, expected);
    }

    fn oracle_overlay(
        shape: &Shape,
        clip: Option<&Contour>,
        rule: OracleRule,
        solver: OracleSolver,
    ) -> Shapes {
        let mut overlay = OracleOverlay::new_custom(0, oracle_options(), solver);
        overlay.add_shape(&to_oracle_shape(shape), OracleShapeType::Subject);
        if let Some(clip) = clip {
            overlay.add_contour(&to_oracle_contour(clip), OracleShapeType::Clip);
        }
        from_oracle_shapes(overlay.overlay(rule, OracleFillRule::NonZero))
    }

    fn port_options() -> port::IntOverlayOptions {
        port::IntOverlayOptions {
            output_direction: port::ContourDirection::CounterClockwise,
            min_output_area: 0,
            ..Default::default()
        }
    }

    fn oracle_options() -> OracleOptions<u64> {
        OracleOptions {
            output_direction: OracleContourDirection::CounterClockwise,
            min_output_area: 0,
            ..Default::default()
        }
    }

    fn generated_shape(rng: &mut Lcg, rings: usize, max_points: usize) -> Shape {
        let mut shape = Vec::with_capacity(rings);
        for ring_idx in 0..rings {
            let count = 3 + rng.usize(max_points.saturating_sub(2));
            let mut ring: Contour = Vec::with_capacity(count);
            let base_x = rng.coord();
            let base_y = rng.coord();
            for idx in 0..count {
                let point = if idx > 0 && idx % 17 == 0 {
                    ring[idx - 1]
                } else if idx > 1 && idx % 13 == 0 {
                    let a = ring[idx - 2];
                    let b = ring[idx - 1];
                    IntPoint::new((a.x + b.x) / 2, (a.y + b.y) / 2)
                } else {
                    let spread = 20 + i32::try_from((ring_idx + 1) * 37).expect("small ring index");
                    IntPoint::new(
                        base_x + rng.range_i32(spread),
                        base_y + rng.range_i32(spread),
                    )
                };
                ring.push(point);
            }
            shape.push(ring);
        }
        shape
    }

    fn generated_rect(rng: &mut Lcg) -> Contour {
        let x0 = rng.coord() / 2;
        let y0 = rng.coord() / 2;
        let w = 10 + rng.range_i32(500).abs();
        let h = 10 + rng.range_i32(500).abs();
        rect(x0, y0, x0 + w, y0 + h)
    }

    fn grid_rects(cols: usize, rows: usize, step: i32) -> Shape {
        let mut shape = Vec::with_capacity(cols * rows);
        for y in 0..rows {
            for x in 0..cols {
                let x0 = i32::try_from(x).expect("grid x fits i32") * step;
                let y0 = i32::try_from(y).expect("grid y fits i32") * step;
                shape.push(rect(x0, y0, x0 + 2, y0 + 2));
            }
        }
        shape
    }

    fn rect(min_x: i32, min_y: i32, max_x: i32, max_y: i32) -> Contour {
        vec![
            IntPoint::new(min_x, min_y),
            IntPoint::new(max_x, min_y),
            IntPoint::new(max_x, max_y),
            IntPoint::new(min_x, max_y),
        ]
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
            i32::try_from(self.next() % 100_001).expect("bounded coord fits i32") - 50_000
        }

        fn range_i32(&mut self, radius: i32) -> i32 {
            let span = u32::try_from(radius.saturating_mul(2).saturating_add(1))
                .expect("positive span fits u32");
            i32::try_from(self.next() % span).expect("bounded delta fits i32") - radius
        }
    }
}
