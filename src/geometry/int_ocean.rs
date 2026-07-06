use crate::geometry::Point;

use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay::{ContourDirection, IntOverlayOptions, Overlay};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::core::simplify::Simplify;
use i_overlay::i_float::int::point::IntPoint;

pub(crate) type Contour = Vec<IntPoint>;
pub(crate) type Shape = Vec<Contour>;
pub(crate) type Shapes = Vec<Shape>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IntRect {
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

pub(crate) const OCEAN_DP_TOL_PX: i64 = 16;

pub(crate) fn quantize_polygon(outer: &[Point], inners: &[Vec<Point>], maxz: u8) -> Shape {
    let Some(outer) = quantize_ring(outer, maxz, true) else {
        return Vec::new();
    };
    let mut shape = Vec::with_capacity(1 + inners.len());
    shape.push(outer);
    for inner in inners {
        if let Some(ring) = quantize_ring(inner, maxz, false) {
            shape.push(ring);
        }
    }
    shape
}

pub(crate) fn rescale_shape(shape: &Shape, s: u8) -> Shape {
    if s == 0 {
        return shape.clone();
    }

    let mut out = Vec::with_capacity(shape.len());
    for (i, contour) in shape.iter().enumerate() {
        let mut ring = Vec::with_capacity(contour.len());
        for &p in contour {
            push_nonduplicate(
                &mut ring,
                IntPoint::new(shift_round(p.x, s), shift_round(p.y, s)),
            );
        }
        remove_closing_duplicate(&mut ring);
        if ring_is_valid(&ring) {
            orient_ring(&mut ring, i == 0);
            out.push(ring);
        }
    }
    if out.first().is_some_and(|outer| outer.len() >= 3) {
        out
    } else {
        Vec::new()
    }
}

pub(crate) fn simplify_shape_dp(shape: &mut Shape, tol: i64) {
    if tol <= 0 {
        return;
    }

    let mut out = Vec::with_capacity(shape.len());
    for (i, contour) in shape.drain(..).enumerate() {
        if let Some(mut ring) = simplify_contour_dp(&contour, tol) {
            orient_ring(&mut ring, i == 0);
            out.push(ring);
        }
    }
    *shape = if out.first().is_some_and(|outer| outer.len() >= 3) {
        out
    } else {
        Vec::new()
    };
}

#[allow(clippy::needless_pass_by_value)]
pub(crate) fn normalize(shape: Shape, min_area: u64) -> Shapes {
    if shape.is_empty() {
        return Vec::new();
    }
    let options = overlay_options(min_area);
    clean_shapes(shape.as_slice().simplify(FillRule::NonZero, options), min_area)
}

pub(crate) fn intersect_rect(shape: &Shape, rect: IntRect, min_area: u64) -> Shapes {
    if shape.is_empty() || rect.min_x >= rect.max_x || rect.min_y >= rect.max_y {
        return Vec::new();
    }

    let rect_shape = rect_shape(rect);
    let options = overlay_options(min_area);
    let mut overlay = Overlay::with_shapes_options(
        std::slice::from_ref(shape),
        std::slice::from_ref(&rect_shape),
        options,
        Default::default(),
    );
    clean_shapes(overlay.overlay(OverlayRule::Intersect, FillRule::NonZero), min_area)
}

pub(crate) fn point_in_shape(x: i32, y: i32, shape: &Shape) -> bool {
    if shape.is_empty() {
        return false;
    }
    point_in_contour(x, y, &shape[0])
        && !shape[1..].iter().any(|hole| point_in_contour(x, y, hole))
}

fn overlay_options(min_area: u64) -> IntOverlayOptions<u64> {
    IntOverlayOptions {
        output_direction: ContourDirection::CounterClockwise,
        min_output_area: min_area,
        ..Default::default()
    }
}

fn rect_shape(rect: IntRect) -> Shape {
    vec![vec![
        IntPoint::new(rect.min_x, rect.min_y),
        IntPoint::new(rect.max_x, rect.min_y),
        IntPoint::new(rect.max_x, rect.max_y),
        IntPoint::new(rect.min_x, rect.max_y),
    ]]
}

fn quantize_ring(points: &[Point], maxz: u8, outer: bool) -> Option<Contour> {
    let scale = 1_i64 << (u32::from(maxz) + 12);
    let mut ring = Vec::with_capacity(points.len());
    for p in points {
        push_nonduplicate(
            &mut ring,
            IntPoint::new(quantize_coord(p.x, scale), quantize_coord(p.y, scale)),
        );
    }
    remove_closing_duplicate(&mut ring);
    if !ring_is_valid(&ring) {
        return None;
    }
    orient_ring(&mut ring, outer);
    Some(ring)
}

fn quantize_coord(v: f64, scale: i64) -> i32 {
    #[allow(clippy::cast_possible_truncation)]
    let rounded = (v * scale as f64).round() as i64;
    i32::try_from(rounded.clamp(0, scale)).expect("base ocean coordinate fits i32")
}

fn shift_round(v: i32, s: u8) -> i32 {
    let half = 1_i64 << (u32::from(s) - 1);
    let v = i64::from(v);
    #[allow(clippy::cast_possible_truncation)]
    if v >= 0 {
        i32::try_from((v + half) >> s).expect("rescaled ocean coordinate fits i32")
    } else {
        -i32::try_from((-v + half) >> s).expect("rescaled ocean coordinate fits i32")
    }
}

fn push_nonduplicate(ring: &mut Contour, point: IntPoint) {
    if ring.last().copied() != Some(point) {
        ring.push(point);
    }
}

fn remove_closing_duplicate(ring: &mut Contour) {
    if ring.len() >= 2 && ring.first() == ring.last() {
        ring.pop();
    }
}

fn ring_is_valid(ring: &Contour) -> bool {
    if ring.len() < 3 || signed_area_2x(ring) == 0 {
        return false;
    }
    let mut unique = Vec::with_capacity(3);
    for &p in ring {
        if !unique.contains(&p) {
            unique.push(p);
            if unique.len() >= 3 {
                return true;
            }
        }
    }
    false
}

fn orient_ring(ring: &mut Contour, outer: bool) {
    let area = signed_area_2x(ring);
    if (outer && area < 0) || (!outer && area > 0) {
        ring.reverse();
    }
}

fn signed_area_2x(ring: &Contour) -> i128 {
    let mut area = 0_i128;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        area += i128::from(ring[i].x) * i128::from(ring[j].y);
        area -= i128::from(ring[j].x) * i128::from(ring[i].y);
    }
    area
}

fn simplify_contour_dp(contour: &Contour, tol: i64) -> Option<Contour> {
    let mut ring = contour.clone();
    remove_closing_duplicate(&mut ring);
    if ring.len() <= 3 {
        return ring_is_valid(&ring).then_some(ring);
    }

    let a0 = anchor_min_lex(&ring);
    let a1 = anchor_farthest(&ring, a0)?;
    if a0 == a1 {
        return None;
    }

    let chain_a = collect_chain(&ring, a0, a1);
    let chain_b = collect_chain(&ring, a1, a0);
    let keep_a = dp_keep(&chain_a, tol);
    let keep_b = dp_keep(&chain_b, tol);

    let mut out = Vec::with_capacity(keep_a.len() + keep_b.len());
    for (idx, &keep) in keep_a.iter().enumerate() {
        if keep {
            push_nonduplicate(&mut out, chain_a[idx]);
        }
    }
    for (idx, &keep) in keep_b.iter().enumerate() {
        if idx == 0 || idx + 1 == keep_b.len() {
            continue;
        }
        if keep {
            push_nonduplicate(&mut out, chain_b[idx]);
        }
    }
    remove_closing_duplicate(&mut out);
    ring_is_valid(&out).then_some(out)
}

fn anchor_min_lex(ring: &Contour) -> usize {
    ring.iter()
        .enumerate()
        .min_by_key(|(_, p)| (p.x, p.y))
        .map(|(idx, _)| idx)
        .expect("ring is nonempty")
}

fn anchor_farthest(ring: &Contour, a0: usize) -> Option<usize> {
    let origin = ring[a0];
    ring.iter()
        .enumerate()
        .filter(|(idx, _)| *idx != a0)
        .max_by_key(|(_, p)| (distance_sq(origin, **p), p.x, p.y))
        .map(|(idx, _)| idx)
}

fn distance_sq(a: IntPoint, b: IntPoint) -> i128 {
    let dx = i128::from(b.x) - i128::from(a.x);
    let dy = i128::from(b.y) - i128::from(a.y);
    dx * dx + dy * dy
}

fn collect_chain(ring: &Contour, start: usize, end: usize) -> Contour {
    let mut chain = Vec::new();
    let mut idx = start;
    loop {
        chain.push(ring[idx]);
        if idx == end {
            break;
        }
        idx = (idx + 1) % ring.len();
    }
    chain
}

fn dp_keep(chain: &Contour, tol: i64) -> Vec<bool> {
    let mut keep = vec![false; chain.len()];
    if chain.is_empty() {
        return keep;
    }
    keep[0] = true;
    keep[chain.len() - 1] = true;

    let tol_sq = i128::from(tol) * i128::from(tol);
    let mut stack = vec![(0_usize, chain.len() - 1)];
    while let Some((start, end)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        let Some((idx, num, den)) = farthest_from_segment(chain, start, end) else {
            continue;
        };
        let keep_point = if den == 0 {
            num > tol_sq
        } else {
            num > tol_sq * den
        };
        if keep_point {
            keep[idx] = true;
            stack.push((start, idx));
            stack.push((idx, end));
        }
    }
    keep
}

fn farthest_from_segment(chain: &Contour, start: usize, end: usize) -> Option<(usize, i128, i128)> {
    let a = chain[start];
    let b = chain[end];
    let dx = i128::from(b.x) - i128::from(a.x);
    let dy = i128::from(b.y) - i128::from(a.y);
    let den = dx * dx + dy * dy;
    let mut best: Option<(usize, i128, i32, i32)> = None;

    for (idx, &p) in chain.iter().enumerate().take(end).skip(start + 1) {
        let num = if den == 0 {
            distance_sq(a, p)
        } else {
            let px = i128::from(p.x) - i128::from(a.x);
            let py = i128::from(p.y) - i128::from(a.y);
            let cross = px * dy - py * dx;
            cross * cross
        };
        if best.is_none_or(|(_, best_num, best_x, best_y)| {
            num > best_num || (num == best_num && (p.x, p.y) > (best_x, best_y))
        }) {
            best = Some((idx, num, p.x, p.y));
        }
    }

    best.map(|(idx, num, _, _)| (idx, num, den))
}

fn clean_shapes(shapes: Shapes, min_area: u64) -> Shapes {
    let mut out = Vec::with_capacity(shapes.len());
    for shape in shapes {
        let mut clean = Vec::with_capacity(shape.len());
        for (i, contour) in shape.into_iter().enumerate() {
            let mut ring = Vec::with_capacity(contour.len());
            for p in contour {
                push_nonduplicate(&mut ring, p);
            }
            remove_closing_duplicate(&mut ring);
            if ring_is_valid(&ring) && true_area(&ring) >= u128::from(min_area) {
                orient_ring(&mut ring, i == 0);
                clean.push(ring);
            }
        }
        if clean.first().is_some_and(|outer| outer.len() >= 3) {
            out.push(clean);
        }
    }
    out
}

fn true_area(ring: &Contour) -> u128 {
    signed_area_2x(ring).unsigned_abs() / 2
}

fn point_in_contour(x: i32, y: i32, ring: &Contour) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let a = ring[j];
        let b = ring[i];
        if point_on_segment(x, y, a, b) {
            return true;
        }
        let yi = a.y;
        let yj = b.y;
        if (yi > y) != (yj > y) {
            let lhs = i128::from(x - a.x) * i128::from(yj - yi);
            let rhs = i128::from(b.x - a.x) * i128::from(y - yi);
            if (yj > yi && lhs < rhs) || (yj < yi && lhs > rhs) {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

fn point_on_segment(x: i32, y: i32, a: IntPoint, b: IntPoint) -> bool {
    let px = i128::from(x);
    let py = i128::from(y);
    let ax = i128::from(a.x);
    let ay = i128::from(a.y);
    let bx = i128::from(b.x);
    let by = i128::from(b.y);
    (px - ax) * (by - ay) == (py - ay) * (bx - ax)
        && px >= ax.min(bx)
        && px <= ax.max(bx)
        && py >= ay.min(by)
        && py <= ay.max(by)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: i32, y: i32) -> IntPoint {
        IntPoint::new(x, y)
    }

    fn merc_ring(coords: &[(f64, f64)]) -> Vec<Point> {
        coords.iter().map(|&(x, y)| Point::new(x, y)).collect()
    }

    fn square_shape(min: i32, max: i32) -> Shape {
        vec![vec![p(min, min), p(max, min), p(max, max), p(min, max)]]
    }

    fn sorted_points(ring: &Contour) -> Vec<(i32, i32)> {
        let mut pts: Vec<(i32, i32)> = ring.iter().map(|p| (p.x, p.y)).collect();
        pts.sort_unstable();
        pts
    }

    fn assert_winding(shape: &Shape) {
        assert!(signed_area_2x(&shape[0]) > 0);
        for hole in &shape[1..] {
            assert!(signed_area_2x(hole) < 0);
        }
    }

    #[test]
    fn quantize_polygon_winding_enforced_for_misordered_input() {
        let outer = merc_ring(&[(0.0, 0.0), (0.0, 0.25), (0.25, 0.25), (0.25, 0.0)]);
        let hole = merc_ring(&[(0.05, 0.05), (0.20, 0.05), (0.20, 0.20), (0.05, 0.20)]);
        let shape = quantize_polygon(&outer, &[hole], 0);
        assert_eq!(shape.len(), 2);
        assert_winding(&shape);
    }

    #[test]
    fn quantize_polygon_duplicate_collapse() {
        let outer = merc_ring(&[
            (0.0, 0.0),
            (0.0, 0.0),
            (0.25, 0.0),
            (0.25, 0.25),
            (0.0, 0.25),
            (0.0, 0.0),
        ]);
        let shape = quantize_polygon(&outer, &[], 0);
        assert_eq!(shape[0].len(), 4);
        assert_ne!(shape[0].first(), shape[0].last());
    }

    #[test]
    fn quantize_polygon_ring_collapse_below_3_points_dropped() {
        let outer = merc_ring(&[(0.0, 0.0), (0.25, 0.0), (0.25, 0.0), (0.0, 0.0)]);
        let shape = quantize_polygon(&outer, &[], 0);
        assert!(shape.is_empty());
    }

    #[test]
    fn rescale_shape_s_zero_identity() {
        let shape = square_shape(0, 4096);
        assert_eq!(rescale_shape(&shape, 0), shape);
    }

    #[test]
    fn rescale_shape_s_gt_zero_within_one_of_direct_quantization() {
        let outer = merc_ring(&[(0.12345, 0.10), (0.35, 0.10), (0.35, 0.30), (0.12345, 0.30)]);
        let base = quantize_polygon(&outer, &[], 4);
        let scaled = rescale_shape(&base, 2);
        let direct = quantize_polygon(&outer, &[], 2);
        for (a, b) in scaled[0].iter().zip(&direct[0]) {
            assert!((a.x - b.x).abs() <= 1, "{} vs {}", a.x, b.x);
            assert!((a.y - b.y).abs() <= 1, "{} vs {}", a.y, b.y);
        }
    }

    #[test]
    fn rescale_shape_duplicate_collapse_after_downshift() {
        let shape = vec![vec![p(0, 0), p(1, 0), p(8, 8), p(0, 8)]];
        let scaled = rescale_shape(&shape, 4);
        assert_eq!(scaled[0], vec![p(0, 0), p(1, 1), p(0, 1)]);
    }

    #[test]
    fn simplify_shape_dp_rotation_invariance() {
        let ring = vec![p(0, 0), p(40, 0), p(40, 2), p(40, 40), p(0, 40), p(0, 2)];
        let mut shape_a = vec![ring.clone()];
        let mut rotated = ring[3..].to_vec();
        rotated.extend_from_slice(&ring[..3]);
        let mut shape_b = vec![rotated];
        simplify_shape_dp(&mut shape_a, 16);
        simplify_shape_dp(&mut shape_b, 16);
        assert_eq!(sorted_points(&shape_a[0]), sorted_points(&shape_b[0]));
    }

    #[test]
    fn simplify_shape_dp_staircase_collapses_at_tol_16() {
        let mut ring = Vec::new();
        ring.push(p(0, 0));
        for i in 1..40 {
            ring.push(p(i, if i % 2 == 0 { 0 } else { 1 }));
        }
        ring.extend([p(40, 20), p(0, 20)]);
        let mut shape = vec![ring];
        simplify_shape_dp(&mut shape, 16);
        assert_eq!(shape[0].len(), 4);
    }

    #[test]
    fn simplify_shape_dp_old_ring_seam_not_privileged() {
        let ring = vec![p(0, 0), p(1, 8), p(2, 0), p(40, 0), p(40, 40), p(0, 40)];
        let mut shape = vec![ring];
        simplify_shape_dp(&mut shape, 16);
        assert!(!shape[0].contains(&p(1, 8)));
    }

    #[test]
    fn normalize_figure_8_input_to_two_simple_polygons() {
        let shape = vec![vec![p(0, 0), p(20, 20), p(0, 20), p(20, 0)]];
        let shapes = normalize(shape, 0);
        assert_eq!(shapes.len(), 2);
        for shape in &shapes {
            assert_winding(shape);
        }
    }

    #[test]
    fn normalize_same_winding_outer_and_hole_is_corrected_by_quantize() {
        let outer = merc_ring(&[(0.0, 0.0), (0.25, 0.0), (0.25, 0.25), (0.0, 0.25)]);
        let hole = merc_ring(&[(0.05, 0.05), (0.20, 0.05), (0.20, 0.20), (0.05, 0.20)]);
        let shape = quantize_polygon(&outer, &[hole], 0);
        assert_winding(&shape);
    }

    #[test]
    fn normalize_sub_min_area_contour_dropped() {
        let shapes = normalize(square_shape(0, 10), 256);
        assert!(shapes.is_empty());
    }

    #[test]
    fn intersect_rect_concave_polygon_crossing_rect_edge_twice_splits() {
        let shape = vec![vec![
            p(0, 0),
            p(10, 0),
            p(10, 2),
            p(2, 2),
            p(2, 8),
            p(10, 8),
            p(10, 10),
            p(0, 10),
        ]];
        let out = intersect_rect(
            &shape,
            IntRect { min_x: 5, min_y: 0, max_x: 12, max_y: 10 },
            0,
        );
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn intersect_rect_hole_entirely_inside_rect_outputs_rect_outer_and_hole() {
        let shape = vec![
            vec![p(0, 0), p(10, 0), p(10, 10), p(0, 10)],
            vec![p(3, 3), p(3, 7), p(7, 7), p(7, 3)],
        ];
        let out = intersect_rect(
            &shape,
            IntRect { min_x: 0, min_y: 0, max_x: 10, max_y: 10 },
            0,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), 2);
        assert_winding(&out[0]);
    }

    #[test]
    fn intersect_rect_hole_partially_crossing_rect() {
        let shape = vec![
            vec![p(0, 0), p(10, 0), p(10, 10), p(0, 10)],
            vec![p(4, -2), p(4, 5), p(6, 5), p(6, -2)],
        ];
        let out = intersect_rect(
            &shape,
            IntRect { min_x: 0, min_y: 0, max_x: 10, max_y: 10 },
            0,
        );
        assert!(!out.is_empty());
        assert!(!point_in_shape(5, 2, &out[0]));
    }

    #[test]
    fn intersect_rect_output_winding_satisfies_invariant() {
        let shape = square_shape(0, 10);
        let out = intersect_rect(
            &shape,
            IntRect { min_x: 2, min_y: 2, max_x: 8, max_y: 8 },
            0,
        );
        assert_eq!(out.len(), 1);
        assert_winding(&out[0]);
    }

    #[test]
    fn point_in_shape_inside_outer_inside_hole_on_boundary_determinism() {
        let shape = vec![
            vec![p(0, 0), p(10, 0), p(10, 10), p(0, 10)],
            vec![p(3, 3), p(3, 7), p(7, 7), p(7, 3)],
        ];
        assert!(point_in_shape(1, 1, &shape));
        assert!(!point_in_shape(5, 5, &shape));
        assert!(point_in_shape(0, 5, &shape));
        assert!(!point_in_shape(3, 5, &shape));
    }
}
