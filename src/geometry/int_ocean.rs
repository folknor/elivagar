use crate::geometry::Point;
use crate::geometry::overlay::{BoolOverlay, BoolRule, ShapeType};
use crate::mvt;

pub(crate) use crate::geometry::overlay::IntPoint;
use std::ops::Range;

pub(crate) type Contour = crate::geometry::overlay::Contour;
pub(crate) type Shape = crate::geometry::overlay::Shape;
pub(crate) type Shapes = crate::geometry::overlay::Shapes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IntRect {
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

pub(crate) const OCEAN_DP_TOL_PX: i64 = 16;
pub(crate) const OSM_DP_TOL_PX: i64 = 16;
pub(crate) const TILE_EXTENT_I32: i32 = 4096;
pub(crate) const TILE_BUFFER_I32: i32 = 128;

pub(crate) struct IntEmitScratch {
    pub tile_points: Vec<(i32, i32)>,
    pub tile_ranges: Vec<Range<usize>>,
    pub geom_buf: Vec<u32>,
    pub dp: DpScratch,
    overlay: BoolOverlay,
    rect_contour: Contour,
}

impl IntEmitScratch {
    pub(crate) fn new() -> Self {
        Self {
            tile_points: Vec::new(),
            tile_ranges: Vec::new(),
            geom_buf: Vec::new(),
            dp: DpScratch::default(),
            overlay: BoolOverlay::new(),
            rect_contour: Vec::with_capacity(4),
        }
    }

    pub(crate) fn recycle_shapes(&mut self, shapes: &mut Shapes) {
        self.overlay.recycle(shapes);
    }

    #[allow(dead_code)]
    pub(crate) fn recycle_shape(&mut self, shape: &mut Shape) {
        self.overlay.recycle_shape(shape);
    }
}

/// Reusable temporaries for `simplify_shape_dp`. The DP simplifier ran at
/// ~10M calls per denmark build with ~11 heap allocations per contour
/// (ring clone, pin flags, two chains, two chain-flag vecs, two keep vecs,
/// two DP stacks, output) - 10.7 GB of churn, the largest exclusive
/// allocation sink in the 2026-07-09 profile. All of them live here now,
/// cleared per call, warm after the first contour. Output is written back
/// into the input contour's own allocation.
#[derive(Default)]
pub(crate) struct DpScratch {
    ring: Contour,
    pin_flags: Vec<bool>,
    chain_a: Contour,
    chain_b: Contour,
    pins_a: Vec<bool>,
    pins_b: Vec<bool>,
    keep_a: Vec<bool>,
    keep_b: Vec<bool>,
    stack: Vec<(usize, usize)>,
    out: Contour,
}

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

/// `quantize_polygon` writing into a reused Shape: each ring slot's Vec is
/// recycled (cleared, refilled) so a warm scratch quantizes with zero heap
/// allocations. Behavior identical to `quantize_polygon`; on an invalid
/// outer the output is left empty. Part of the H6 churn reduction -
/// `emit_polygon_feature` quantized 4.5M polygons per denmark build at one
/// fresh Shape each (5.2 GB exclusive).
pub(crate) fn quantize_polygon_into(
    outer: &[Point],
    inners: &[Vec<Point>],
    maxz: u8,
    out: &mut Shape,
) {
    let mut used = 0usize;
    if quantize_ring_at(out, &mut used, outer, maxz, true) {
        for inner in inners {
            quantize_ring_at(out, &mut used, inner, maxz, false);
        }
    } else {
        used = 0;
    }
    out.truncate(used);
}

/// `quantize_polygon_pinned` writing into reused Shape + flags buffers.
/// Same recycling contract as `quantize_polygon_into`.
pub(crate) fn quantize_polygon_pinned_into<F>(
    outer: &[Point],
    inners: &[Vec<Point>],
    maxz: u8,
    mut pin_test: F,
    out: &mut Shape,
    out_flags: &mut Vec<Vec<bool>>,
) where
    F: FnMut(&Point) -> bool,
{
    let mut used = 0usize;
    if quantize_ring_pinned_at(out, out_flags, &mut used, outer, maxz, true, &mut pin_test) {
        for inner in inners {
            quantize_ring_pinned_at(out, out_flags, &mut used, inner, maxz, false, &mut pin_test);
        }
    } else {
        used = 0;
    }
    out.truncate(used);
    out_flags.truncate(used);
}

/// Quantize one ring into slot `*used` of `out`, reusing the slot's Vec.
/// Advances `*used` only when the ring is valid, so an invalid ring's
/// leftovers are overwritten by the next candidate.
fn quantize_ring_at(
    out: &mut Shape,
    used: &mut usize,
    points: &[Point],
    maxz: u8,
    outer: bool,
) -> bool {
    if *used == out.len() {
        out.push(Vec::new());
    }
    let ring = &mut out[*used];
    ring.clear();
    let scale = 1_i64 << (u32::from(maxz) + 12);
    for p in points {
        push_nonduplicate(
            ring,
            IntPoint::new(quantize_coord(p.x, scale), quantize_coord(p.y, scale)),
        );
    }
    remove_closing_duplicate(ring);
    if !ring_is_valid(ring) {
        return false;
    }
    orient_ring(ring, outer);
    *used += 1;
    true
}

#[allow(clippy::too_many_arguments)]
fn quantize_ring_pinned_at<F>(
    out: &mut Shape,
    out_flags: &mut Vec<Vec<bool>>,
    used: &mut usize,
    points: &[Point],
    maxz: u8,
    outer: bool,
    pin_test: &mut F,
) -> bool
where
    F: FnMut(&Point) -> bool,
{
    if *used == out.len() {
        out.push(Vec::new());
    }
    if *used == out_flags.len() {
        out_flags.push(Vec::new());
    }
    let ring = &mut out[*used];
    let flags = &mut out_flags[*used];
    ring.clear();
    flags.clear();
    let scale = 1_i64 << (u32::from(maxz) + 12);
    for p in points {
        push_nonduplicate_pinned(
            ring,
            flags,
            IntPoint::new(quantize_coord(p.x, scale), quantize_coord(p.y, scale)),
            pin_test(p),
        );
    }
    remove_closing_duplicate_pinned(ring, flags);
    if !ring_is_valid(ring) {
        return false;
    }
    orient_ring_pinned(ring, flags, outer);
    *used += 1;
    true
}

/// Test-only reference: production rescaling goes through
/// `rescale_shape_pinned` (the pyramid always carries pin flags).
#[cfg(test)]
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

pub(crate) fn rescale_shape_pinned(
    shape: &Shape,
    s: u8,
    flags: &[Vec<bool>],
) -> (Shape, Vec<Vec<bool>>) {
    if s == 0 {
        return (shape.clone(), flags.to_vec());
    }

    let mut out = Vec::with_capacity(shape.len());
    let mut out_flags = Vec::with_capacity(shape.len());
    for (i, contour) in shape.iter().enumerate() {
        let ring_flags = flags.get(i).map_or(&[][..], Vec::as_slice);
        let mut ring = Vec::with_capacity(contour.len());
        let mut flags_ring = Vec::with_capacity(contour.len());
        for (idx, &p) in contour.iter().enumerate() {
            push_nonduplicate_pinned(
                &mut ring,
                &mut flags_ring,
                IntPoint::new(shift_round(p.x, s), shift_round(p.y, s)),
                ring_flags.get(idx).copied().unwrap_or(false),
            );
        }
        remove_closing_duplicate_pinned(&mut ring, &mut flags_ring);
        if ring_is_valid(&ring) {
            orient_ring_pinned(&mut ring, &mut flags_ring, i == 0);
            out.push(ring);
            out_flags.push(flags_ring);
        }
    }
    if out.first().is_some_and(|outer| outer.len() >= 3) {
        (out, out_flags)
    } else {
        (Vec::new(), Vec::new())
    }
}

#[hotpath::measure]
pub(crate) fn simplify_shape_dp(
    shape: &mut Shape,
    tol: i64,
    pins: Option<&[Vec<bool>]>,
    dp: &mut DpScratch,
) {
    if tol <= 0 {
        return;
    }

    // In place: each surviving contour is rewritten inside its own
    // allocation and compacted forward; dead contours drop with truncate.
    // Orientation follows the ORIGINAL ring index (index 0 is the outer),
    // matching the previous drain-and-rebuild semantics exactly.
    let mut write = 0usize;
    for read in 0..shape.len() {
        let pin_ring = pins.and_then(|p| p.get(read)).map(Vec::as_slice);
        if simplify_contour_dp_in_place(&mut shape[read], tol, pin_ring, read == 0, dp) {
            shape.swap(write, read);
            write += 1;
        }
    }
    shape.truncate(write);
    if !shape.first().is_some_and(|outer| outer.len() >= 3) {
        shape.clear();
    }
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn normalize(shape: Shape, min_area: u64) -> Shapes {
    let mut scratch = IntEmitScratch::new();
    let mut out = Vec::new();
    normalize_into(&mut scratch, shape, min_area, &mut out);
    out
}

#[hotpath::measure]
pub(crate) fn normalize_into(
    scratch: &mut IntEmitScratch,
    mut shape: Shape,
    min_area: u64,
    out: &mut Shapes,
) {
    scratch.overlay.recycle(out);
    if shape.is_empty() {
        return;
    }
    scratch.overlay.min_output_area = min_area;
    if shape.len() == 1 {
        if scratch.overlay.simplify_contour_into(&shape[0], out) {
            clean_shapes_in_place(out, min_area);
            return;
        }
        clean_shape_in_place(&mut shape, min_area);
        if shape.first().is_some_and(|outer| outer.len() >= 3) {
            out.push(shape);
        }
        return;
    }

    scratch.overlay.clear();
    scratch.overlay.add_shape(&shape, ShapeType::Subject);
    scratch.overlay.overlay_nested(BoolRule::Subject, out);
    clean_shapes_in_place(out, min_area);
}

#[cfg(test)]
pub(crate) fn intersect_rect(shape: &Shape, rect: IntRect, min_area: u64) -> Shapes {
    let mut scratch = IntEmitScratch::new();
    let mut out = Vec::new();
    intersect_rect_into(&mut scratch, shape, rect, min_area, &mut out);
    out
}

#[hotpath::measure]
pub(crate) fn intersect_rect_into(
    scratch: &mut IntEmitScratch,
    shape: &Shape,
    rect: IntRect,
    min_area: u64,
    out: &mut Shapes,
) {
    scratch.overlay.recycle(out);
    if shape.is_empty() || rect.min_x >= rect.max_x || rect.min_y >= rect.max_y {
        return;
    }

    fill_rect_contour(&mut scratch.rect_contour, rect);
    scratch.overlay.min_output_area = min_area;
    scratch.overlay.clear();
    scratch.overlay.add_shape(shape, ShapeType::Subject);
    scratch
        .overlay
        .add_contour(&scratch.rect_contour, ShapeType::Clip);
    scratch.overlay.overlay_nested(BoolRule::Intersect, out);
    clean_shapes_in_place(out, min_area);
}

#[allow(dead_code)]
pub(crate) fn point_in_shape(x: i32, y: i32, shape: &Shape) -> bool {
    if shape.is_empty() {
        return false;
    }
    point_in_contour(x, y, &shape[0]) && !shape[1..].iter().any(|hole| point_in_contour(x, y, hole))
}

fn fill_rect_contour(out: &mut Contour, rect: IntRect) {
    out.clear();
    out.push(IntPoint::new(rect.min_x, rect.min_y));
    out.push(IntPoint::new(rect.max_x, rect.min_y));
    out.push(IntPoint::new(rect.max_x, rect.max_y));
    out.push(IntPoint::new(rect.min_x, rect.max_y));
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
    i32::try_from(rounded).expect("base coordinate fits i32")
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

fn push_nonduplicate_pinned(
    ring: &mut Contour,
    flags: &mut Vec<bool>,
    point: IntPoint,
    pinned: bool,
) {
    if ring.last().copied() == Some(point) {
        if let Some(last) = flags.last_mut() {
            *last |= pinned;
        }
    } else {
        ring.push(point);
        flags.push(pinned);
    }
}

fn remove_closing_duplicate(ring: &mut Contour) {
    if ring.len() >= 2 && ring.first() == ring.last() {
        ring.pop();
    }
}

fn remove_closing_duplicate_pinned(ring: &mut Contour, flags: &mut Vec<bool>) {
    if ring.len() >= 2 && ring.first() == ring.last() {
        if let Some(last) = flags.pop()
            && let Some(first) = flags.first_mut()
        {
            *first |= last;
        }
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

fn orient_ring_pinned(ring: &mut Contour, flags: &mut [bool], outer: bool) {
    let area = signed_area_2x(ring);
    if (outer && area < 0) || (!outer && area > 0) {
        ring.reverse();
        flags.reverse();
    }
}

pub(crate) fn signed_area_2x(ring: &Contour) -> i128 {
    let mut area = 0_i128;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        area += i128::from(ring[i].x) * i128::from(ring[j].y);
        area -= i128::from(ring[j].x) * i128::from(ring[i].y);
    }
    area
}

fn simplify_contour_dp_in_place(
    contour: &mut Contour,
    tol: i64,
    pins: Option<&[bool]>,
    outer: bool,
    dp: &mut DpScratch,
) -> bool {
    dp.ring.clear();
    dp.ring.extend_from_slice(contour);
    remove_closing_duplicate(&mut dp.ring);
    dp.pin_flags.clear();
    if let Some(p) = pins {
        dp.pin_flags.extend_from_slice(p);
    }
    if dp.pin_flags.len() > dp.ring.len() {
        dp.pin_flags.truncate(dp.ring.len());
    } else if dp.pin_flags.len() < dp.ring.len() {
        dp.pin_flags.resize(dp.ring.len(), false);
    }
    if dp.ring.len() <= 3 {
        if !ring_is_valid(&dp.ring) {
            return false;
        }
        contour.clear();
        contour.extend_from_slice(&dp.ring);
        orient_ring(contour, outer);
        return true;
    }

    let a0 = anchor_min_lex(&dp.ring);
    let Some(a1) = anchor_farthest(&dp.ring, a0) else {
        return false;
    };
    if a0 == a1 {
        return false;
    }

    collect_chain_into(&dp.ring, a0, a1, &mut dp.chain_a);
    collect_chain_into(&dp.ring, a1, a0, &mut dp.chain_b);
    collect_chain_flags_into(&dp.pin_flags, a0, a1, &mut dp.pins_a);
    collect_chain_flags_into(&dp.pin_flags, a1, a0, &mut dp.pins_b);
    dp_keep_into(&dp.chain_a, tol, &dp.pins_a, &mut dp.keep_a, &mut dp.stack);
    dp_keep_into(&dp.chain_b, tol, &dp.pins_b, &mut dp.keep_b, &mut dp.stack);

    dp.out.clear();
    for (idx, &keep) in dp.keep_a.iter().enumerate() {
        if keep {
            push_nonduplicate(&mut dp.out, dp.chain_a[idx]);
        }
    }
    for (idx, &keep) in dp.keep_b.iter().enumerate() {
        if idx == 0 || idx + 1 == dp.keep_b.len() {
            continue;
        }
        if keep {
            push_nonduplicate(&mut dp.out, dp.chain_b[idx]);
        }
    }
    remove_closing_duplicate(&mut dp.out);
    if !ring_is_valid(&dp.out) {
        return false;
    }
    contour.clear();
    contour.extend_from_slice(&dp.out);
    orient_ring(contour, outer);
    true
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

fn collect_chain_into(ring: &Contour, start: usize, end: usize, chain: &mut Contour) {
    chain.clear();
    let mut idx = start;
    loop {
        chain.push(ring[idx]);
        if idx == end {
            break;
        }
        idx = (idx + 1) % ring.len();
    }
}

fn collect_chain_flags_into(flags: &[bool], start: usize, end: usize, chain: &mut Vec<bool>) {
    chain.clear();
    let mut idx = start;
    loop {
        chain.push(flags.get(idx).copied().unwrap_or(false));
        if idx == end {
            break;
        }
        idx = (idx + 1) % flags.len();
    }
}

fn dp_keep_into(
    chain: &Contour,
    tol: i64,
    pins: &[bool],
    keep: &mut Vec<bool>,
    stack: &mut Vec<(usize, usize)>,
) {
    keep.clear();
    keep.resize(chain.len(), false);
    if chain.is_empty() {
        return;
    }
    keep[0] = true;
    keep[chain.len() - 1] = true;
    for (idx, &pinned) in pins.iter().enumerate().take(keep.len()) {
        if pinned {
            keep[idx] = true;
        }
    }

    let tol_sq = i128::from(tol) * i128::from(tol);
    stack.clear();
    stack.push((0_usize, chain.len() - 1));
    while let Some((start, end)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        if let Some(idx) = keep[start + 1..end].iter().position(|&k| k) {
            let pin_idx = start + 1 + idx;
            stack.push((start, pin_idx));
            stack.push((pin_idx, end));
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

fn clean_shapes_in_place(shapes: &mut Shapes, min_area: u64) {
    let mut write = 0usize;
    for read in 0..shapes.len() {
        clean_shape_in_place(&mut shapes[read], min_area);
        if shapes[read].first().is_some_and(|outer| outer.len() >= 3) {
            if write != read {
                shapes.swap(write, read);
            }
            write += 1;
        }
    }
    shapes.truncate(write);
}

fn clean_shape_in_place(shape: &mut Shape, min_area: u64) {
    let mut write = 0usize;
    for read in 0..shape.len() {
        clean_contour_in_place(&mut shape[read]);
        if ring_is_valid(&shape[read]) && true_area(&shape[read]) >= u128::from(min_area) {
            orient_ring(&mut shape[read], write == 0);
            if write != read {
                shape.swap(write, read);
            }
            write += 1;
        }
    }
    shape.truncate(write);
}

fn clean_contour_in_place(ring: &mut Contour) {
    if ring.is_empty() {
        return;
    }
    let mut write = 1usize;
    for read in 1..ring.len() {
        if ring[read] != ring[write - 1] {
            ring[write] = ring[read];
            write += 1;
        }
    }
    ring.truncate(write);
    remove_closing_duplicate(ring);
}

fn true_area(ring: &Contour) -> u128 {
    signed_area_2x(ring).unsigned_abs() / 2
}

pub(crate) fn point_in_contour(x: i32, y: i32, ring: &Contour) -> bool {
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

pub(crate) fn shape_bbox(shape: &Shape) -> Option<IntRect> {
    let mut points = shape.iter().flatten();
    let first = points.next()?;
    let (mut min_x, mut max_x) = (first.x, first.x);
    let (mut min_y, mut max_y) = (first.y, first.y);
    for p in points {
        min_x = min_x.min(p.x);
        max_x = max_x.max(p.x);
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    Some(IntRect {
        min_x,
        min_y,
        max_x,
        max_y,
    })
}

#[allow(dead_code)]
pub(crate) fn tile_range_for_rect(rect: IntRect, max_tile: u32) -> (u32, u32, u32, u32) {
    (
        tile_index(rect.min_x, max_tile),
        tile_index(rect.max_x, max_tile),
        tile_index(rect.min_y, max_tile),
        tile_index(rect.max_y, max_tile),
    )
}

#[allow(dead_code)]
fn tile_index(q: i32, max_tile: u32) -> u32 {
    if q <= 0 {
        0
    } else {
        #[allow(clippy::cast_sign_loss)]
        let idx = (q / TILE_EXTENT_I32) as u32;
        idx.min(max_tile)
    }
}

#[allow(dead_code)]
pub(crate) fn tile_count_for_rect(rect: IntRect, max_tile: u32) -> u64 {
    let (tx_min, tx_max, ty_min, ty_max) = tile_range_for_rect(rect, max_tile);
    u64::from(tx_max - tx_min + 1) * u64::from(ty_max - ty_min + 1)
}

pub(crate) fn tile_origin(t: u32) -> i32 {
    i32::try_from(t).expect("tile coordinate fits i32") * TILE_EXTENT_I32
}

#[allow(dead_code)]
pub(crate) fn buffered_tile_rect(tx: u32, ty: u32) -> IntRect {
    IntRect {
        min_x: tile_origin(tx) - TILE_BUFFER_I32,
        min_y: tile_origin(ty) - TILE_BUFFER_I32,
        max_x: tile_origin(tx + 1) + TILE_BUFFER_I32,
        max_y: tile_origin(ty + 1) + TILE_BUFFER_I32,
    }
}

#[allow(dead_code)]
fn rect_intersection(a: IntRect, b: IntRect) -> Option<IntRect> {
    let rect = IntRect {
        min_x: a.min_x.max(b.min_x),
        min_y: a.min_y.max(b.min_y),
        max_x: a.max_x.min(b.max_x),
        max_y: a.max_y.min(b.max_y),
    };
    (rect.min_x < rect.max_x && rect.min_y < rect.max_y).then_some(rect)
}

pub(crate) fn encode_tile_shape(
    tile_shape: &Shape,
    tx: u32,
    ty: u32,
    scratch: &mut IntEmitScratch,
    sink: &mut dyn FnMut(u32, u32, &[u32]),
) {
    let ox = tile_origin(tx);
    let oy = tile_origin(ty);
    scratch.tile_points.clear();
    scratch.tile_ranges.clear();
    for (i, contour) in tile_shape.iter().enumerate() {
        if contour.len() < 3 {
            continue;
        }
        append_translated_ring(
            contour,
            ox,
            oy,
            i == 0,
            &mut scratch.tile_points,
            &mut scratch.tile_ranges,
        );
    }
    if scratch.tile_ranges.is_empty() {
        return;
    }

    mvt::encode_polygon_ranges(
        &mut scratch.geom_buf,
        &scratch.tile_points,
        &scratch.tile_ranges,
    );
    if !scratch.geom_buf.is_empty() {
        sink(tx, ty, &scratch.geom_buf);
    }
}

fn append_translated_ring(
    contour: &Contour,
    ox: i32,
    oy: i32,
    clockwise: bool,
    points: &mut Vec<(i32, i32)>,
    ranges: &mut Vec<Range<usize>>,
) {
    let start = points.len();
    points.extend(contour.iter().map(|p| (p.x - ox, p.y - oy)));
    if points[start..].first() != points[start..].last()
        && let Some(&first) = points.get(start)
    {
        points.push(first);
    }
    let area = signed_area_tile_2x(&points[start..]);
    if (clockwise && area < 0) || (!clockwise && area > 0) {
        points[start..].reverse();
    }
    if points.len() >= start + 4 {
        ranges.push(start..points.len());
    } else {
        points.truncate(start);
    }
}

fn signed_area_tile_2x(ring: &[(i32, i32)]) -> i128 {
    let mut area = 0_i128;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        area += i128::from(ring[i].0) * i128::from(ring[j].1);
        area -= i128::from(ring[j].0) * i128::from(ring[i].1);
    }
    area
}

#[hotpath::measure]
pub(crate) fn emit_full_tile(
    tx: u32,
    ty: u32,
    scratch: &mut IntEmitScratch,
    sink: &mut dyn FnMut(u32, u32, &[u32]),
) {
    scratch.tile_points.clear();
    scratch.tile_ranges.clear();
    scratch.tile_points.extend_from_slice(&[
        (-TILE_BUFFER_I32, -TILE_BUFFER_I32),
        (TILE_EXTENT_I32 + TILE_BUFFER_I32, -TILE_BUFFER_I32),
        (
            TILE_EXTENT_I32 + TILE_BUFFER_I32,
            TILE_EXTENT_I32 + TILE_BUFFER_I32,
        ),
        (-TILE_BUFFER_I32, TILE_EXTENT_I32 + TILE_BUFFER_I32),
        (-TILE_BUFFER_I32, -TILE_BUFFER_I32),
    ]);
    scratch.tile_ranges.push(0..scratch.tile_points.len());
    mvt::encode_polygon_ranges(
        &mut scratch.geom_buf,
        &scratch.tile_points,
        &scratch.tile_ranges,
    );
    if !scratch.geom_buf.is_empty() {
        sink(tx, ty, &scratch.geom_buf);
    }
}

pub(crate) fn contour_area_is_below(ring: &Contour, min_area: u64) -> bool {
    min_area > 0 && true_area(ring) < u128::from(min_area)
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
        let mut dp = DpScratch::default();
        simplify_shape_dp(&mut shape_a, 16, None, &mut dp);
        simplify_shape_dp(&mut shape_b, 16, None, &mut dp);
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
        simplify_shape_dp(&mut shape, 16, None, &mut DpScratch::default());
        assert_eq!(shape[0].len(), 4);
    }

    #[test]
    fn simplify_shape_dp_old_ring_seam_not_privileged() {
        let ring = vec![p(0, 0), p(1, 8), p(2, 0), p(40, 0), p(40, 40), p(0, 40)];
        let mut shape = vec![ring];
        simplify_shape_dp(&mut shape, 16, None, &mut DpScratch::default());
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
            IntRect {
                min_x: 5,
                min_y: 0,
                max_x: 12,
                max_y: 10,
            },
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
            IntRect {
                min_x: 0,
                min_y: 0,
                max_x: 10,
                max_y: 10,
            },
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
            IntRect {
                min_x: 0,
                min_y: 0,
                max_x: 10,
                max_y: 10,
            },
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
            IntRect {
                min_x: 2,
                min_y: 2,
                max_x: 8,
                max_y: 8,
            },
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

    #[test]
    fn landing_b_antimeridian_quantization_is_unclamped() {
        let scale = 1_i32 << (14 + 12);
        let outer = merc_ring(&[
            (1.000_01, 0.500_00),
            (1.000_05, 0.500_00),
            (1.000_05, 0.500_04),
            (1.000_01, 0.500_04),
            (1.000_01, 0.500_00),
        ]);
        let shape = quantize_polygon(&outer, &[], 14);
        assert!(shape[0].iter().any(|p| p.x > scale));
        let shape_z = rescale_shape(&shape, 1);
        let fixed = normalize(shape_z, 0);
        assert!(!fixed.is_empty());
        let bbox = shape_bbox(&fixed[0]).expect("normalized shifted bbox");
        let clipped = intersect_rect(&fixed[0], bbox, 0);
        assert!(!clipped.is_empty());
    }

    #[test]
    fn landing_b_pin_aware_dp_keeps_pinned_vertices_under_aggressive_tolerance() {
        let target = p(50, 1);
        let mut plain = vec![vec![
            p(0, 0),
            p(25, 0),
            target,
            p(75, 0),
            p(100, 0),
            p(100, 100),
            p(0, 100),
        ]];
        let mut pinned = plain.clone();
        let flags = vec![vec![false, false, true, false, false, false, false]];
        let mut dp = DpScratch::default();
        simplify_shape_dp(&mut plain, 16, None, &mut dp);
        simplify_shape_dp(&mut pinned, 16, Some(&flags), &mut dp);
        assert!(!plain[0].contains(&target));
        assert!(pinned[0].contains(&target));
    }
}

#[cfg(test)]
mod landing1_tests {
    use super::*;

    fn p(x: i32, y: i32) -> IntPoint {
        IntPoint::new(x, y)
    }

    #[test]
    fn bounds_boolean_skipped_when_bbox_contained() {
        // A shape whose ring rotation i_overlay would canonicalize: if the
        // containment fast path is taken, the ring comes back VERBATIM.
        let ring: Contour = vec![p(500, 500), p(900, 500), p(900, 900), p(500, 900)];
        let shape: Shape = vec![ring.clone()];
        let bb = shape_bbox(&shape).expect("bbox");
        let data = IntRect {
            min_x: 0,
            min_y: 0,
            max_x: 4096,
            max_y: 4096,
        };
        assert!(bb.min_x >= data.min_x && bb.max_x <= data.max_x);
        // The skip is in ocean.rs push_quantized_pieces; its observable
        // contract is verbatim passthrough - assert the geometric identity
        // the skip relies on: intersect of a contained shape equals itself
        // up to ring rotation, so passthrough is legal.
        let out = intersect_rect(&shape, data, 0);
        assert_eq!(out.len(), 1);
        let mut got: Vec<(i32, i32)> = out[0][0].iter().map(|q| (q.x, q.y)).collect();
        let want: Vec<(i32, i32)> = ring.iter().map(|q| (q.x, q.y)).collect();
        // rotation-normalize both
        fn rot_min(v: &[(i32, i32)]) -> Vec<(i32, i32)> {
            let n = v.len();
            let m = (0..n).min_by_key(|&i| v[i]).unwrap_or(0);
            (0..n).map(|i| v[(m + i) % n]).collect()
        }
        got = rot_min(&got);
        assert_eq!(got, rot_min(&want));
    }
}
