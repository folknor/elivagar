//! Integer polygon overlay engine extracted from i_overlay.
//!
//! Only the Subject and Intersect rules at NonZero fill on i32 coordinates
//! are retained, and for those the result is held point-for-point equal to
//! the dev-dependency i_overlay by the differential oracle tests below. That
//! equivalence covers the retained operations and the options these tests
//! drive, not upstream's wider API.
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
    fn overlay_into_clears_dirty_output_for_empty_input() {
        // Mirrors the regression test upstream added with its 8.0.0 fix: the
        // early return on an empty post-split segment list must not leave
        // stale shapes in a dirty caller buffer. Production callers recycle
        // the buffer first, so only this test reaches the drain.
        let mut engine = BoolOverlay::new();
        let mut out: Shapes = vec![vec![vec![
            IntPoint::new(0, 0),
            IntPoint::new(10, 0),
            IntPoint::new(0, 10),
        ]]];
        engine.overlay_nested(BoolRule::Subject, &mut out);
        assert!(out.is_empty());
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
        // state leak - the class the denmark corpus digest would only catch if
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

    // Upstream issue 91 inputs (i_overlay tests/issue_91_tests.rs). Both
    // union to two base shapes; the padding squares push the output outer
    // count across the hole binder's 32-shape list/tree threshold.
    const PANIC_INPUT: &[&[(i32, i32)]] = &[
        &[(4079, 3454), (4090, 3462), (4083, 3471), (4073, 3464)],
        &[(4078, 3464), (4084, 3463), (4081, 3460)],
        &[(4072, 3463), (4080, 3471), (4061, 3471)],
    ];

    const OWNER_INPUT: &[&[(i32, i32)]] = &[
        &[(-4, 3), (-7, 6), (-4, 6)],
        &[(-4, 2), (-1, 5), (-4, 5)],
        &[(-3, 2), (-10, 5), (-6, 5)],
        &[(-3, 0), (-4, 1), (-1, 0)],
        &[(-3, 2), (-3, 5), (-2, 5)],
        &[(-4, 2), (-4, 4), (0, 4)],
        &[(-2, 1), (-5, 1), (-2, 4)],
    ];

    const OWNER_HOLE: &[(i32, i32)] = &[(-4, 3), (-4, 4), (-3, 2)];
    const OWNER_OUTER: &[(i32, i32)] = &[
        (-6, 5),
        (-10, 5),
        (-4, 2),
        (-5, 1),
        (-2, 1),
        (-2, 3),
        (0, 4),
        (-2, 4),
        (-3, 3),
        (-3, 5),
        (-4, 5),
        (-4, 6),
        (-7, 6),
    ];

    fn contour(points: &[(i32, i32)]) -> Contour {
        points.iter().map(|&(x, y)| IntPoint::new(x, y)).collect()
    }

    fn padded(input: &[&[(i32, i32)]], squares: i32) -> Shape {
        let mut shape: Shape = input.iter().map(|points| contour(points)).collect();
        for i in 0..squares {
            let x = 100 + 40 * i;
            shape.push(rect(x, 100, x + 10, 110));
        }
        shape
    }

    /// Orientation- and start-free form of a ring, for ownership checks.
    fn canonical(ring: &[IntPoint]) -> Contour {
        let mut ring = ring.to_vec();
        let start = ring
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| **p)
            .map(|(i, _)| i)
            .expect("non-empty ring");
        ring.rotate_left(start);
        if ring[1] > ring[ring.len() - 1] {
            ring[1..].reverse();
        }
        ring
    }

    fn assert_hole_owner(shapes: &Shapes, hole: &[(i32, i32)], outer: &[(i32, i32)]) {
        let hole = canonical(&contour(hole));
        let owners: Vec<&Shape> = shapes
            .iter()
            .filter(|shape| shape.iter().skip(1).any(|ring| canonical(ring) == hole))
            .collect();
        assert_eq!(owners.len(), 1, "expected exactly one owner: {shapes:?}");
        assert_eq!(
            canonical(&owners[0][0]),
            canonical(&contour(outer)),
            "hole attached to the wrong shape: {shapes:?}"
        );
        let hole_count: usize = shapes.iter().map(|shape| shape.len() - 1).sum();
        assert_eq!(hole_count, 1);
    }

    fn subject_union(engine: &mut BoolOverlay, shape: &Shape, out: &mut Shapes) {
        engine.clear();
        engine.add_shape(shape, ShapeType::Subject);
        engine.overlay_nested(BoolRule::Subject, out);
    }

    #[test]
    fn issue_91_touching_shapes_bind_hole_across_tree_threshold() {
        let mut engine = BoolOverlay::new();
        let mut out = Shapes::new();
        for squares in [0, 29, 30, 31] {
            let shape = padded(PANIC_INPUT, squares);
            // Recycle as production callers do, so this test isolates hole
            // binding from the output-replacement contract.
            engine.recycle(&mut out);
            subject_union(&mut engine, &shape, &mut out);
            let outers = usize::try_from(squares).expect("small count") + 2;
            assert_eq!(out.len(), outers, "squares={squares}");
            assert_hole_owner(&out, PANIC_INPUT[1], PANIC_INPUT[0]);
            let expected = oracle_overlay(&shape, None, OracleRule::Subject, OracleSolver::AUTO);
            assert_eq!(out, expected, "squares={squares}");
        }
    }

    #[test]
    fn issue_91_hole_stays_with_containing_shape() {
        let mut engine = BoolOverlay::new();
        let mut out = Shapes::new();
        for squares in [0, 29, 30, 31] {
            let shape = padded(OWNER_INPUT, squares);
            engine.recycle(&mut out);
            subject_union(&mut engine, &shape, &mut out);
            let outers = usize::try_from(squares).expect("small count") + 2;
            assert_eq!(out.len(), outers, "squares={squares}");
            assert_hole_owner(&out, OWNER_HOLE, OWNER_OUTER);
            let expected = oracle_overlay(&shape, None, OracleRule::Subject, OracleSolver::AUTO);
            assert_eq!(out, expected, "squares={squares}");
        }
    }

    #[test]
    fn differential_oracle_min_output_area() {
        // Holes are filtered independently of their outer: a 100x100 square
        // with a 2x2 hole (area 4) keeps the hole at thresholds 3 and 4 and
        // drops it at 5, while the outer survives all three.
        let square_with_hole = vec![rect(0, 0, 100, 100), {
            let mut hole = rect(40, 40, 42, 42);
            hole.reverse();
            hole
        }];
        let clip = rect(-5, -5, 60, 60);
        for min_area in [3, 4, 5] {
            assert_same_with_area(&square_with_hole, None, min_area);
            assert_same_with_area(&square_with_hole, Some(&clip), min_area);
        }

        let mut rng = Lcg::new(0x0a2e_a5ee_d000_0091);
        let mut engine = BoolOverlay::new();
        let mut out = Shapes::new();
        for case in 0..400_u32 {
            let rings = 1 + rng.usize(6);
            let shape = generated_shape(&mut rng, rings, 48);
            let clip = generated_rect(&mut rng);
            for clip in [None, Some(&clip)] {
                // Take real contour areas from an unfiltered run, so the sweep
                // hits each threshold just below, exactly at, and just above a
                // contour that is actually emitted - odd doubled areas
                // included, where the halving truncates.
                engine.min_output_area = 0;
                run_engine(&mut engine, &shape, clip, &mut out);
                let areas: Vec<u64> = out
                    .iter()
                    .flatten()
                    .map(Vec::as_slice)
                    .map(ring_area)
                    .step_by(3)
                    .take(3)
                    .collect();
                for area in areas {
                    for min_area in [area.saturating_sub(1).max(1), area, area + 1] {
                        engine.min_output_area = min_area;
                        run_engine(&mut engine, &shape, clip, &mut out);
                        let expected = oracle_overlay_with_area(&shape, clip, min_area);
                        assert_eq!(out, expected, "case {case}, min_area {min_area}");
                    }
                }
            }
        }
    }

    fn ring_area(ring: &[IntPoint]) -> u64 {
        let mut double_area = 0i64;
        let mut p0 = ring[ring.len() - 1];
        for &p1 in ring {
            double_area += i64::from(p0.x) * i64::from(p1.y) - i64::from(p0.y) * i64::from(p1.x);
            p0 = p1;
        }
        double_area.unsigned_abs() >> 1
    }

    fn run_engine(
        engine: &mut BoolOverlay,
        shape: &Shape,
        clip: Option<&Contour>,
        out: &mut Shapes,
    ) {
        engine.clear();
        engine.add_shape(shape, ShapeType::Subject);
        if let Some(clip) = clip {
            engine.add_contour(clip, ShapeType::Clip);
            engine.overlay_nested(BoolRule::Intersect, out);
        } else {
            engine.overlay_nested(BoolRule::Subject, out);
        }
    }

    fn assert_same_with_area(shape: &Shape, clip: Option<&Contour>, min_area: u64) {
        let mut engine = BoolOverlay::new();
        engine.min_output_area = min_area;
        let mut out = Shapes::new();
        run_engine(&mut engine, shape, clip, &mut out);
        assert_eq!(out, oracle_overlay_with_area(shape, clip, min_area));
    }

    fn oracle_overlay_with_area(shape: &Shape, clip: Option<&Contour>, min_area: u64) -> Shapes {
        let options = OracleOptions {
            min_output_area: min_area,
            ..oracle_options()
        };
        let mut overlay = OracleOverlay::new_custom(0, options, OracleSolver::AUTO);
        overlay.add_source(&to_oracle_shape(shape), OracleShapeType::Subject);
        let rule = if let Some(clip) = clip {
            overlay.add_contour(&to_oracle_contour(clip), OracleShapeType::Clip);
            OracleRule::Intersect
        } else {
            OracleRule::Subject
        };
        from_oracle_shapes(overlay.overlay(rule, OracleFillRule::NonZero))
    }

    #[test]
    fn production_area_filter_catches_the_perfect_contour_fast_path() {
        // The single-contour fast path applies no area filter (upstream
        // simplify_contour does not either); normalize_into must filter
        // afterwards for both windings. A 4x4 square has area 16.
        let ccw = rect(0, 0, 4, 4);
        let mut cw = ccw.clone();
        cw.reverse();
        for ring in [ccw, cw] {
            for (min_area, survives) in [(15, true), (16, true), (17, false)] {
                let out = crate::geometry::int_ocean::normalize(vec![ring.clone()], min_area);
                assert_eq!(!out.is_empty(), survives, "min_area {min_area}: {out:?}");
            }
        }
    }

    #[test]
    fn engine_output_replaces_a_dirty_buffer() {
        // Every entry point must replace `out`, never append to it: hole
        // binding scans all of `out`, so a stale shape could steal a hole.
        let dirty = || -> Shapes {
            let mut shapes = padded(PANIC_INPUT, 31)
                .into_iter()
                .map(|ring| vec![ring])
                .collect::<Shapes>();
            shapes.push(vec![rect(4000, 3400, 4200, 3500)]);
            shapes
        };
        let mut engine = BoolOverlay::new();

        let panic_case = padded(PANIC_INPUT, 30);
        let mut clean = Shapes::new();
        subject_union(&mut engine, &panic_case, &mut clean);
        let mut out = dirty();
        subject_union(&mut engine, &panic_case, &mut out);
        assert_eq!(out, clean, "nonempty overlay");

        let mut out = dirty();
        engine.clear();
        engine.overlay_nested(BoolRule::Subject, &mut out);
        assert!(out.is_empty(), "empty overlay: {out:?}");

        let ccw = rect(0, 0, 10, 10);
        let mut out = dirty();
        assert!(!engine.simplify_contour_into(&ccw, &mut out));
        assert!(out.is_empty(), "perfect simplify: {out:?}");

        let mut cw = ccw.clone();
        cw.reverse();
        let mut out = dirty();
        assert!(engine.simplify_contour_into(&cw, &mut out));
        let mut clean = Shapes::new();
        assert!(engine.simplify_contour_into(&cw, &mut clean));
        assert_eq!(out, clean, "reversed simplify");

        let flat = contour(&[(0, 0), (5, 0), (10, 0), (5, 0)]);
        let mut out = dirty();
        let rebuilt = engine.simplify_contour_into(&flat, &mut out);
        let mut clean = Shapes::new();
        assert_eq!(engine.simplify_contour_into(&flat, &mut clean), rebuilt);
        assert_eq!(out, clean, "collapsing simplify");
        assert!(out.is_empty(), "collapsing simplify: {out:?}");
    }

    #[test]
    fn differential_oracle_reuse_across_binder_threshold() {
        // One engine and one output buffer through a sequence that alternates
        // hole binding on both sides of the 32-outer threshold, empty output,
        // collapse, and plain holes, each checked against the oracle.
        let mut engine = BoolOverlay::new();
        let mut out = Shapes::new();
        let flat: Shape = vec![contour(&[(0, 0), (5, 0), (10, 0), (5, 0)])];
        let square_with_hole = vec![rect(0, 0, 100, 100), {
            let mut hole = rect(40, 40, 60, 60);
            hole.reverse();
            hole
        }];
        let sequence = [
            padded(PANIC_INPUT, 31),
            Shape::new(),
            padded(OWNER_INPUT, 0),
            flat.clone(),
            padded(OWNER_INPUT, 31),
            square_with_hole,
            padded(PANIC_INPUT, 29),
            flat,
            padded(PANIC_INPUT, 30),
        ];
        for round in 0..3 {
            for (step, shape) in sequence.iter().enumerate() {
                subject_union(&mut engine, shape, &mut out);
                let expected = oracle_overlay(shape, None, OracleRule::Subject, OracleSolver::AUTO);
                assert_eq!(out, expected, "round {round}, step {step}");
            }
        }
    }

    fn oracle_overlay(
        shape: &Shape,
        clip: Option<&Contour>,
        rule: OracleRule,
        solver: OracleSolver,
    ) -> Shapes {
        let mut overlay = OracleOverlay::new_custom(0, oracle_options(), solver);
        overlay.add_source(&to_oracle_shape(shape), OracleShapeType::Subject);
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
