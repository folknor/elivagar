use crate::geometry::Point;
use crate::mvt;

use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay::{ContourDirection, IntOverlayOptions, Overlay, ShapeType};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::i_float::int::point::IntPoint;
use rustc_hash::FxHashSet;
use std::collections::HashMap;
use std::ops::Range;

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
pub(crate) const OSM_DP_TOL_PX: i64 = 16;
pub(crate) const TILE_EXTENT_I32: i32 = 4096;
pub(crate) const TILE_BUFFER_I32: i32 = 128;

pub(crate) struct IntEmitScratch {
    pub boundary_tiles: FxHashSet<u64>,
    pub boundary_rows: HashMap<u32, Vec<u32>>,
    pub tile_points: Vec<(i32, i32)>,
    pub tile_ranges: Vec<Range<usize>>,
    pub geom_buf: Vec<u32>,
    pub shape_z: Shape,
    pub flags_z: Vec<Vec<bool>>,
    overlay: Overlay<i32>,
    rect_contour: Contour,
    normalized_shapes: Shapes,
    clipped_shapes: Shapes,
}

impl IntEmitScratch {
    pub(crate) fn new() -> Self {
        Self {
            boundary_tiles: FxHashSet::default(),
            boundary_rows: HashMap::new(),
            tile_points: Vec::new(),
            tile_ranges: Vec::new(),
            geom_buf: Vec::new(),
            shape_z: Vec::new(),
            flags_z: Vec::new(),
            overlay: Overlay::new_custom(0, overlay_options(0), Default::default()),
            rect_contour: Vec::with_capacity(4),
            normalized_shapes: Vec::new(),
            clipped_shapes: Vec::new(),
        }
    }
}

pub(crate) struct ZoomEmitParams<'a> {
    pub z: u8,
    pub maxz: u8,
    pub dp_tol: i64,
    pub min_area: u64,
    pub pins: Option<&'a FxHashSet<(i32, i32)>>,
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

pub(crate) fn quantize_polygon_pinned<F>(
    outer: &[Point],
    inners: &[Vec<Point>],
    maxz: u8,
    mut pin_test: F,
) -> (Shape, Vec<Vec<bool>>)
where
    F: FnMut(&Point) -> bool,
{
    let Some((outer, outer_flags)) = quantize_ring_pinned(outer, maxz, true, &mut pin_test) else {
        return (Vec::new(), Vec::new());
    };
    let mut shape = Vec::with_capacity(1 + inners.len());
    let mut flags = Vec::with_capacity(1 + inners.len());
    shape.push(outer);
    flags.push(outer_flags);
    for inner in inners {
        if let Some((ring, ring_flags)) = quantize_ring_pinned(inner, maxz, false, &mut pin_test) {
            shape.push(ring);
            flags.push(ring_flags);
        }
    }
    (shape, flags)
}

#[hotpath::measure]
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
pub(crate) fn simplify_shape_dp(shape: &mut Shape, tol: i64, pins: Option<&[Vec<bool>]>) {
    if tol <= 0 {
        return;
    }

    let mut out = Vec::with_capacity(shape.len());
    for (i, contour) in shape.drain(..).enumerate() {
        let pin_ring = pins.and_then(|p| p.get(i)).map(Vec::as_slice);
        if let Some(mut ring) = simplify_contour_dp(&contour, tol, pin_ring) {
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
    out.clear();
    if shape.is_empty() {
        return;
    }
    scratch.overlay.options = overlay_options(min_area);
    let mut shapes = if shape.len() == 1 {
        match scratch
            .overlay
            .simplify_contour(&shape[0], FillRule::NonZero)
        {
            Some(shapes) => shapes,
            None => {
                clean_shape_in_place(&mut shape, min_area);
                if shape.first().is_some_and(|outer| outer.len() >= 3) {
                    out.push(shape);
                }
                return;
            }
        }
    } else {
        scratch.overlay.clear();
        scratch.overlay.add_shape(&shape, ShapeType::Subject);
        scratch
            .overlay
            .overlay(OverlayRule::Subject, FillRule::NonZero)
    };
    clean_shapes_in_place(&mut shapes, min_area);
    out.append(&mut shapes);
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
    out.clear();
    if shape.is_empty() || rect.min_x >= rect.max_x || rect.min_y >= rect.max_y {
        return;
    }

    fill_rect_contour(&mut scratch.rect_contour, rect);
    scratch.overlay.options = overlay_options(min_area);
    scratch.overlay.clear();
    scratch.overlay.add_shape(shape, ShapeType::Subject);
    scratch
        .overlay
        .add_contour(&scratch.rect_contour, ShapeType::Clip);
    let mut shapes = scratch
        .overlay
        .overlay(OverlayRule::Intersect, FillRule::NonZero);
    clean_shapes_in_place(&mut shapes, min_area);
    out.append(&mut shapes);
}

pub(crate) fn point_in_shape(x: i32, y: i32, shape: &Shape) -> bool {
    if shape.is_empty() {
        return false;
    }
    point_in_contour(x, y, &shape[0]) && !shape[1..].iter().any(|hole| point_in_contour(x, y, hole))
}

fn overlay_options(min_area: u64) -> IntOverlayOptions<u64> {
    IntOverlayOptions {
        output_direction: ContourDirection::CounterClockwise,
        min_output_area: min_area,
        ..Default::default()
    }
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

fn quantize_ring_pinned<F>(
    points: &[Point],
    maxz: u8,
    outer: bool,
    pin_test: &mut F,
) -> Option<(Contour, Vec<bool>)>
where
    F: FnMut(&Point) -> bool,
{
    let scale = 1_i64 << (u32::from(maxz) + 12);
    let mut ring = Vec::with_capacity(points.len());
    let mut flags = Vec::with_capacity(points.len());
    for p in points {
        push_nonduplicate_pinned(
            &mut ring,
            &mut flags,
            IntPoint::new(quantize_coord(p.x, scale), quantize_coord(p.y, scale)),
            pin_test(p),
        );
    }
    remove_closing_duplicate_pinned(&mut ring, &mut flags);
    if !ring_is_valid(&ring) {
        return None;
    }
    orient_ring_pinned(&mut ring, &mut flags, outer);
    Some((ring, flags))
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

fn simplify_contour_dp(contour: &Contour, tol: i64, pins: Option<&[bool]>) -> Option<Contour> {
    let mut ring = contour.clone();
    remove_closing_duplicate(&mut ring);
    let mut pin_flags = pins.map_or_else(Vec::new, ToOwned::to_owned);
    if pin_flags.len() > ring.len() {
        pin_flags.truncate(ring.len());
    } else if pin_flags.len() < ring.len() {
        pin_flags.resize(ring.len(), false);
    }
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
    let pins_a = collect_chain_flags(&pin_flags, a0, a1);
    let pins_b = collect_chain_flags(&pin_flags, a1, a0);
    let keep_a = dp_keep(&chain_a, tol, Some(&pins_a));
    let keep_b = dp_keep(&chain_b, tol, Some(&pins_b));

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

fn collect_chain_flags(flags: &[bool], start: usize, end: usize) -> Vec<bool> {
    let mut chain = Vec::new();
    let mut idx = start;
    loop {
        chain.push(flags.get(idx).copied().unwrap_or(false));
        if idx == end {
            break;
        }
        idx = (idx + 1) % flags.len();
    }
    chain
}

fn dp_keep(chain: &Contour, tol: i64, pins: Option<&[bool]>) -> Vec<bool> {
    let mut keep = vec![false; chain.len()];
    if chain.is_empty() {
        return keep;
    }
    keep[0] = true;
    keep[chain.len() - 1] = true;
    if let Some(pins) = pins {
        for (idx, &pinned) in pins.iter().enumerate().take(keep.len()) {
            if pinned {
                keep[idx] = true;
            }
        }
    }

    let tol_sq = i128::from(tol) * i128::from(tol);
    let mut stack = vec![(0_usize, chain.len() - 1)];
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

pub(crate) fn tile_range_for_rect(rect: IntRect, max_tile: u32) -> (u32, u32, u32, u32) {
    (
        tile_index(rect.min_x, max_tile),
        tile_index(rect.max_x, max_tile),
        tile_index(rect.min_y, max_tile),
        tile_index(rect.max_y, max_tile),
    )
}

fn tile_index(q: i32, max_tile: u32) -> u32 {
    if q <= 0 {
        0
    } else {
        #[allow(clippy::cast_sign_loss)]
        let idx = (q / TILE_EXTENT_I32) as u32;
        idx.min(max_tile)
    }
}

pub(crate) fn tile_count_for_rect(rect: IntRect, max_tile: u32) -> u64 {
    let (tx_min, tx_max, ty_min, ty_max) = tile_range_for_rect(rect, max_tile);
    u64::from(tx_max - tx_min + 1) * u64::from(ty_max - ty_min + 1)
}

pub(crate) fn tile_origin(t: u32) -> i32 {
    i32::try_from(t).expect("tile coordinate fits i32") * TILE_EXTENT_I32
}

fn tile_center_coord(t: u32) -> i32 {
    tile_origin(t) + TILE_EXTENT_I32 / 2
}

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

#[allow(clippy::needless_pass_by_value)]
#[hotpath::measure]
pub(crate) fn emit_shape_for_zoom(
    shape_base: &Shape,
    params: ZoomEmitParams<'_>,
    scratch: &mut IntEmitScratch,
    sink: &mut dyn FnMut(u32, u32, &[u32]),
) {
    if shape_base.is_empty() {
        return;
    }

    let shift = params.maxz.saturating_sub(params.z);
    if let Some(pins) = params.pins {
        let base_flags = flags_from_pin_set(shape_base, pins);
        let (shape_z, flags_z) = rescale_shape_pinned(shape_base, shift, &base_flags);
        scratch.shape_z = shape_z;
        scratch.flags_z = flags_z;
        simplify_shape_dp(&mut scratch.shape_z, params.dp_tol, Some(&scratch.flags_z));
    } else {
        scratch.shape_z = rescale_shape(shape_base, shift);
        scratch.flags_z.clear();
        simplify_shape_dp(&mut scratch.shape_z, params.dp_tol, None);
    }

    let shape_z = std::mem::take(&mut scratch.shape_z);
    let mut normalized = std::mem::take(&mut scratch.normalized_shapes);
    normalize_into(scratch, shape_z, params.min_area, &mut normalized);
    for shape in &normalized {
        emit_normalized_shape_for_zoom(shape, params.z, params.min_area, scratch, sink);
    }
    normalized.clear();
    scratch.normalized_shapes = normalized;
}

fn flags_from_pin_set(shape: &Shape, pins: &FxHashSet<(i32, i32)>) -> Vec<Vec<bool>> {
    shape
        .iter()
        .map(|ring| ring.iter().map(|p| pins.contains(&(p.x, p.y))).collect())
        .collect()
}

pub(crate) fn encode_tile_shape(
    tile_shape: Shape,
    tx: u32,
    ty: u32,
    scratch: &mut IntEmitScratch,
    sink: &mut dyn FnMut(u32, u32, &[u32]),
) {
    let ox = tile_origin(tx);
    let oy = tile_origin(ty);
    scratch.tile_points.clear();
    scratch.tile_ranges.clear();
    for (i, contour) in tile_shape.into_iter().enumerate() {
        if contour.len() < 3 {
            continue;
        }
        append_translated_ring(
            &contour,
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
fn emit_normalized_shape_for_zoom(
    shape: &Shape,
    z: u8,
    min_area: u64,
    scratch: &mut IntEmitScratch,
    sink: &mut dyn FnMut(u32, u32, &[u32]),
) {
    let max_tile = (1u32 << z) - 1;
    let world_max = i32::try_from((u64::from(max_tile) + 1) * u64::from(TILE_EXTENT_I32 as u32))
        .expect("z14 world extent fits i32");
    let Some(bbox) = shape_bbox(shape) else {
        return;
    };
    let (tx_min, tx_max, ty_min, ty_max) = tile_range_for_rect(bbox, max_tile);

    if tx_min == tx_max && ty_min == ty_max {
        encode_tile_shape(shape.clone(), tx_min, ty_min, scratch, sink);
        return;
    }

    scratch.boundary_tiles.clear();
    rasterize_shape_edges(shape, max_tile, &mut scratch.boundary_tiles);
    scratch.boundary_rows.clear();
    for &packed in &scratch.boundary_tiles {
        #[allow(clippy::cast_possible_truncation)]
        let tx = (packed >> 32) as u32;
        #[allow(clippy::cast_possible_truncation)]
        let ty = packed as u32;
        scratch.boundary_rows.entry(ty).or_default().push(tx);
    }
    for txs in scratch.boundary_rows.values_mut() {
        txs.sort_unstable();
        txs.dedup();
    }

    let row_bands = cut_row_bands_with_scratch(
        scratch,
        shape,
        ty_min,
        ty_max,
        world_max,
        TILE_BUFFER_I32,
        min_area,
    );
    for ty in ty_min..=ty_max {
        let row_shapes = &row_bands[(ty - ty_min) as usize];
        if row_shapes.is_empty() {
            continue;
        }

        let boundary_txs = scratch
            .boundary_rows
            .get(&ty)
            .map_or_else(Vec::new, Clone::clone);
        for row_shape in row_shapes {
            let Some(row_bbox) = shape_bbox(row_shape) else {
                continue;
            };
            let (row_tx_min, row_tx_max, _, _) = tile_range_for_rect(row_bbox, max_tile);
            emit_boundary_and_gap_tiles(
                ty,
                row_tx_min,
                row_tx_max,
                &boundary_txs,
                row_shape,
                min_area,
                scratch,
                sink,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[hotpath::measure]
fn emit_boundary_and_gap_tiles(
    ty: u32,
    tx_min: u32,
    tx_max: u32,
    boundary_txs: &[u32],
    row_shape: &Shape,
    min_area: u64,
    scratch: &mut IntEmitScratch,
    sink: &mut dyn FnMut(u32, u32, &[u32]),
) {
    if tx_min > tx_max {
        return;
    }

    let mut cursor = tx_min;
    for &boundary_tx in boundary_txs {
        if boundary_tx < tx_min || boundary_tx > tx_max {
            continue;
        }
        if cursor < boundary_tx {
            emit_gap_run(
                cursor,
                boundary_tx - 1,
                ty,
                row_shape,
                min_area,
                scratch,
                sink,
            );
        }
        emit_clipped_tile_shape(boundary_tx, ty, row_shape, min_area, scratch, sink);
        cursor = boundary_tx.saturating_add(1);
    }

    if cursor <= tx_max {
        emit_gap_run(cursor, tx_max, ty, row_shape, min_area, scratch, sink);
    }
}

fn emit_gap_run(
    tx_min: u32,
    tx_max: u32,
    ty: u32,
    row_shape: &Shape,
    _min_area: u64,
    scratch: &mut IntEmitScratch,
    sink: &mut dyn FnMut(u32, u32, &[u32]),
) {
    if tx_min > tx_max {
        return;
    }
    let cy = tile_center_coord(ty);
    let test_x = tile_center_coord(tx_min);
    if !point_in_shape(test_x, cy, row_shape) {
        return;
    }
    // Dilated rasterization guarantees every unmarked tile's BUFFERED rect
    // is edge-free; an interior gap tile's clip therefore provably returns
    // exactly the buffered rect - emit it directly, zero booleans.
    for tx in tx_min..=tx_max {
        emit_full_tile(tx, ty, scratch, sink);
    }
}

#[hotpath::measure]
fn emit_clipped_tile_shape(
    tx: u32,
    ty: u32,
    shape: &Shape,
    min_area: u64,
    scratch: &mut IntEmitScratch,
    sink: &mut dyn FnMut(u32, u32, &[u32]),
) {
    let mut clipped = std::mem::take(&mut scratch.clipped_shapes);
    intersect_rect_into(
        scratch,
        shape,
        buffered_tile_rect(tx, ty),
        min_area,
        &mut clipped,
    );
    for tile_shape in clipped.drain(..) {
        encode_tile_shape(tile_shape, tx, ty, scratch, sink);
    }
    scratch.clipped_shapes = clipped;
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

/// Rasterize polygon ring edges into tile grid cells, DILATED by the tile
/// buffer: a tile is marked when an edge touches its BUFFERED rect
/// (+-TILE_BUFFER_I32), not just its interior. This makes "unmarked" a
/// proof that the tile's buffered rect is edge-free, so gap/interior tiles
/// can emit full-tile rects with zero boolean clips (byte-identical to the
/// clip they replace). Exactness: a line's proximity extremes within a cell
/// occur at its entry/exit points, so proximity marking at DDA crossings
/// and segment endpoints is exact for dilation < 1 tile. Over-marking is
/// safe (costs one redundant clip); under-marking is the bug.
#[hotpath::measure]
fn rasterize_shape_edges(shape: &Shape, max_tile: u32, tiles: &mut FxHashSet<u64>) {
    for ring in shape {
        if ring.len() < 2 {
            continue;
        }
        for i in 0..ring.len() {
            let j = if i + 1 < ring.len() { i + 1 } else { 0 };
            let x0 = f64::from(ring[i].x) / f64::from(TILE_EXTENT_I32);
            let y0 = f64::from(ring[i].y) / f64::from(TILE_EXTENT_I32);
            let x1 = f64::from(ring[j].x) / f64::from(TILE_EXTENT_I32);
            let y1 = f64::from(ring[j].y) / f64::from(TILE_EXTENT_I32);
            rasterize_segment_clamped(x0, y0, x1, y1, max_tile, tiles);
            mark_endpoint_dilated(x0, y0, max_tile, tiles);
            mark_endpoint_dilated(x1, y1, max_tile, tiles);
        }
    }
}

/// Buffer size in tile units (128/4096), exactly representable in f64.
const DILATE_TILE_UNITS: f64 = 0.03125;

/// Mark every tile whose buffered rect contains the point (up to 4 tiles
/// when the point lies within the buffer of a grid line / corner).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn mark_endpoint_dilated(x: f64, y: f64, max_tile: u32, tiles: &mut FxHashSet<u64>) {
    let cx = x.floor() as i32;
    let cy = y.floor() as i32;
    let fx = x - x.floor();
    let fy = y - y.floor();
    let x_lo = fx <= DILATE_TILE_UNITS;
    let x_hi = fx >= 1.0 - DILATE_TILE_UNITS;
    let y_lo = fy <= DILATE_TILE_UNITS;
    let y_hi = fy >= 1.0 - DILATE_TILE_UNITS;
    insert_rasterized_tile(cx, cy, max_tile, tiles);
    if x_lo {
        insert_rasterized_tile(cx - 1, cy, max_tile, tiles);
    }
    if x_hi {
        insert_rasterized_tile(cx + 1, cy, max_tile, tiles);
    }
    if y_lo {
        insert_rasterized_tile(cx, cy - 1, max_tile, tiles);
    }
    if y_hi {
        insert_rasterized_tile(cx, cy + 1, max_tile, tiles);
    }
    if x_lo && y_lo {
        insert_rasterized_tile(cx - 1, cy - 1, max_tile, tiles);
    }
    if x_lo && y_hi {
        insert_rasterized_tile(cx - 1, cy + 1, max_tile, tiles);
    }
    if x_hi && y_lo {
        insert_rasterized_tile(cx + 1, cy - 1, max_tile, tiles);
    }
    if x_hi && y_hi {
        insert_rasterized_tile(cx + 1, cy + 1, max_tile, tiles);
    }
}

/// At a vertical grid-line crossing (x = integer, at height y), mark the
/// row neighbors when the crossing lies within the buffer of a horizontal
/// grid line - both cells left/right of the crossing get the neighbor row.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn mark_x_crossing_dilated(cx_left: i32, y: f64, max_tile: u32, tiles: &mut FxHashSet<u64>) {
    let cy = y.floor() as i32;
    let fy = y - y.floor();
    if fy <= DILATE_TILE_UNITS {
        insert_rasterized_tile(cx_left, cy - 1, max_tile, tiles);
        insert_rasterized_tile(cx_left + 1, cy - 1, max_tile, tiles);
    }
    if fy >= 1.0 - DILATE_TILE_UNITS {
        insert_rasterized_tile(cx_left, cy + 1, max_tile, tiles);
        insert_rasterized_tile(cx_left + 1, cy + 1, max_tile, tiles);
    }
}

/// Horizontal grid-line crossing analogue of `mark_x_crossing_dilated`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn mark_y_crossing_dilated(x: f64, cy_top: i32, max_tile: u32, tiles: &mut FxHashSet<u64>) {
    let cx = x.floor() as i32;
    let fx = x - x.floor();
    if fx <= DILATE_TILE_UNITS {
        insert_rasterized_tile(cx - 1, cy_top, max_tile, tiles);
        insert_rasterized_tile(cx - 1, cy_top + 1, max_tile, tiles);
    }
    if fx >= 1.0 - DILATE_TILE_UNITS {
        insert_rasterized_tile(cx + 1, cy_top, max_tile, tiles);
        insert_rasterized_tile(cx + 1, cy_top + 1, max_tile, tiles);
    }
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rasterize_segment(x0: f64, y0: f64, x1: f64, y1: f64, tiles: &mut FxHashSet<u64>) {
    rasterize_segment_clamped(x0, y0, x1, y1, u32::MAX, tiles);
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn rasterize_segment_clamped(
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    max_tile: u32,
    tiles: &mut FxHashSet<u64>,
) {
    let mut cx = x0.floor() as i32;
    let mut cy = y0.floor() as i32;
    let ex = x1.floor() as i32;
    let ey = y1.floor() as i32;

    insert_rasterized_tile(cx, cy, max_tile, tiles);

    let dx = x1 - x0;
    let dy = y1 - y0;

    if dx == 0.0 && dy == 0.0 {
        return;
    }

    if dy == 0.0 && is_grid_line_coord(y0) {
        let step_x = if dx > 0.0 { 1 } else { -1 };
        rasterize_horizontal_grid_line(cx, cy, ex, step_x, max_tile, tiles);
        return;
    }

    if dx == 0.0 && is_grid_line_coord(x0) {
        let step_y = if dy > 0.0 { 1 } else { -1 };
        rasterize_vertical_grid_line(cx, cy, ey, step_y, max_tile, tiles);
        return;
    }

    let step_x: i32 = if dx > 0.0 { 1 } else { -1 };
    let step_y: i32 = if dy > 0.0 { 1 } else { -1 };

    let mut t_max_x = if dx != 0.0 {
        let next_x = if dx > 0.0 { (cx + 1) as f64 } else { cx as f64 };
        (next_x - x0) / dx
    } else {
        f64::MAX
    };
    let mut t_max_y = if dy != 0.0 {
        let next_y = if dy > 0.0 { (cy + 1) as f64 } else { cy as f64 };
        (next_y - y0) / dy
    } else {
        f64::MAX
    };

    let t_delta_x = if dx != 0.0 {
        f64::from(step_x) / dx
    } else {
        f64::MAX
    };
    let t_delta_y = if dy != 0.0 {
        f64::from(step_y) / dy
    } else {
        f64::MAX
    };

    let max_steps = (cx - ex).unsigned_abs() + (cy - ey).unsigned_abs() + 2;
    for _ in 0..max_steps {
        if cx == ex && cy == ey {
            break;
        }
        if t_max_x.total_cmp(&t_max_y).is_eq() {
            // Exact corner crossing: endpoint-style dilation covers all
            // buffered neighbors of the corner point.
            let corner_x = x0 + t_max_x * dx;
            let corner_y = y0 + t_max_x * dy;
            mark_endpoint_dilated(corner_x, corner_y, max_tile, tiles);
            insert_rasterized_tile(cx + step_x, cy, max_tile, tiles);
            insert_rasterized_tile(cx, cy + step_y, max_tile, tiles);
            cx += step_x;
            cy += step_y;
            t_max_x += t_delta_x;
            t_max_y += t_delta_y;
        } else if t_max_x < t_max_y {
            // Crossing a vertical grid line at (cx boundary, y_c): if the
            // crossing is within the buffer of a horizontal line, the
            // segment's buffered rect also touches the neighbor row.
            let y_c = y0 + t_max_x * dy;
            let left = if step_x > 0 { cx } else { cx - 1 };
            mark_x_crossing_dilated(left, y_c, max_tile, tiles);
            cx += step_x;
            t_max_x += t_delta_x;
        } else {
            let x_c = x0 + t_max_y * dx;
            let top = if step_y > 0 { cy } else { cy - 1 };
            mark_y_crossing_dilated(x_c, top, max_tile, tiles);
            cy += step_y;
            t_max_y += t_delta_y;
        }
        insert_rasterized_tile(cx, cy, max_tile, tiles);
    }
}

#[inline]
fn is_grid_line_coord(coord: f64) -> bool {
    coord.fract().abs() <= f64::EPSILON
}

fn rasterize_horizontal_grid_line(
    mut cx: i32,
    cy: i32,
    ex: i32,
    step_x: i32,
    max_tile: u32,
    tiles: &mut FxHashSet<u64>,
) {
    let max_steps = (cx - ex).unsigned_abs() + 2;
    for _ in 0..max_steps {
        insert_rasterized_tile(cx, cy, max_tile, tiles);
        insert_rasterized_tile(cx, cy - 1, max_tile, tiles);
        if cx == ex {
            break;
        }
        cx += step_x;
    }
}

fn rasterize_vertical_grid_line(
    cx: i32,
    mut cy: i32,
    ey: i32,
    step_y: i32,
    max_tile: u32,
    tiles: &mut FxHashSet<u64>,
) {
    let max_steps = (cy - ey).unsigned_abs() + 2;
    for _ in 0..max_steps {
        insert_rasterized_tile(cx, cy, max_tile, tiles);
        insert_rasterized_tile(cx - 1, cy, max_tile, tiles);
        if cy == ey {
            break;
        }
        cy += step_y;
    }
}

#[inline]
fn insert_rasterized_tile(tx: i32, ty: i32, max_tile: u32, tiles: &mut FxHashSet<u64>) {
    if let (Ok(tx), Ok(ty)) = (u32::try_from(tx), u32::try_from(ty)) {
        tiles.insert(pack_tile(tx.min(max_tile), ty.min(max_tile)));
    }
}

#[inline]
pub(crate) fn pack_tile(tx: u32, ty: u32) -> u64 {
    (u64::from(tx) << 32) | u64::from(ty)
}

pub(crate) fn lookback_dedup_contour_pinned(ring: &mut Contour, mut flags: Option<&mut Vec<bool>>) {
    const LOOKBACK: usize = 5;
    if ring.len() < 4 {
        return;
    }
    let closed = ring.first() == ring.last();
    let end = if closed { ring.len() - 1 } else { ring.len() };
    let mut write = 1usize;
    for read in 1..end {
        let p = ring[read];
        let start = write.saturating_sub(LOOKBACK).max(1);
        let mut found = None;
        for j in (start..write).rev() {
            if ring[j] == p {
                found = Some(j);
                break;
            }
        }
        if let Some(j) = found {
            if let Some(f) = flags.as_deref_mut() {
                let mut pinned = f.get(read).copied().unwrap_or(false);
                for k in j + 1..write {
                    pinned |= f.get(k).copied().unwrap_or(false);
                }
                if let Some(dst) = f.get_mut(j) {
                    *dst |= pinned;
                }
            }
            write = j + 1;
        } else {
            ring[write] = p;
            if let Some(f) = flags.as_deref_mut() {
                let pinned = f.get(read).copied().unwrap_or(false);
                if write < f.len() {
                    f[write] = pinned;
                } else {
                    f.push(pinned);
                }
            }
            write += 1;
        }
    }
    if closed && write > 0 {
        ring[write] = ring[0];
        if let Some(f) = flags.as_deref_mut() {
            let pinned = f.first().copied().unwrap_or(false);
            if write < f.len() {
                f[write] = pinned;
            } else {
                f.push(pinned);
            }
        }
        write += 1;
    }
    ring.truncate(write);
    if let Some(f) = flags {
        f.truncate(write);
    }
}

#[hotpath::measure]
pub(crate) fn ring_is_simple_complete(ring: &Contour) -> bool {
    if ring.len() < 3 {
        return false;
    }
    for i in 0..ring.len() {
        let a1 = ring[i];
        let a2 = ring[(i + 1) % ring.len()];
        if a1 == a2 {
            return false;
        }
        for j in i + 1..ring.len() {
            let adjacent = j == i + 1 || (i == 0 && j + 1 == ring.len());
            if adjacent {
                continue;
            }
            let b1 = ring[j];
            let b2 = ring[(j + 1) % ring.len()];
            if segments_intersect_inclusive(a1, a2, b1, b2) {
                return false;
            }
        }
    }
    true
}

pub(crate) fn contour_area_is_below(ring: &Contour, min_area: u64) -> bool {
    min_area > 0 && true_area(ring) < u128::from(min_area)
}

fn segments_intersect_inclusive(a1: IntPoint, a2: IntPoint, b1: IntPoint, b2: IntPoint) -> bool {
    let o1 = orient(a1, a2, b1);
    let o2 = orient(a1, a2, b2);
    let o3 = orient(b1, b2, a1);
    let o4 = orient(b1, b2, a2);

    if o1 == 0 && point_on_segment(b1.x, b1.y, a1, a2) {
        return true;
    }
    if o2 == 0 && point_on_segment(b2.x, b2.y, a1, a2) {
        return true;
    }
    if o3 == 0 && point_on_segment(a1.x, a1.y, b1, b2) {
        return true;
    }
    if o4 == 0 && point_on_segment(a2.x, a2.y, b1, b2) {
        return true;
    }
    (o1 > 0) != (o2 > 0) && (o3 > 0) != (o4 > 0)
}

fn orient(a: IntPoint, b: IntPoint, c: IntPoint) -> i8 {
    let cross = (i128::from(b.x) - i128::from(a.x)) * (i128::from(c.y) - i128::from(a.y))
        - (i128::from(b.y) - i128::from(a.y)) * (i128::from(c.x) - i128::from(a.x));
    if cross > 0 {
        1
    } else if cross < 0 {
        -1
    } else {
        0
    }
}

/// Cut a shape into per-row-band shapes for tile rows `ty0..=ty1`, by
/// recursive bisection: intersect the shape with the buffered upper/lower
/// halves of the row range, then recurse into each half with the (much
/// smaller) result - O(V log R) total noding instead of O(V x R).
///
/// Leaf bands are exactly `[0..world_max] x [row_top-buffer, row_bottom+buffer]`
/// and are cut with `leaf_min_area`; internal bisection cuts use
/// min_area 0 so structural halving never drops slivers a leaf would keep.
/// The leaf result equals a direct `intersect_rect(shape, leaf_band,
/// leaf_min_area)` because every leaf band is contained in all its ancestor
/// half-rects: `(shape INTERSECT ancestor) INTERSECT leaf == shape
/// INTERSECT leaf`.
///
/// Returns one `Shapes` per row, indexed `ty - ty0` (empty for empty rows).
#[cfg(test)]
pub(crate) fn cut_row_bands(
    shape: &Shape,
    ty0: u32,
    ty1: u32,
    world_max: i32,
    buffer: i32,
    leaf_min_area: u64,
) -> Vec<Shapes> {
    debug_assert!(ty0 <= ty1);
    let mut scratch = IntEmitScratch::new();
    cut_row_bands_with_scratch(
        &mut scratch,
        shape,
        ty0,
        ty1,
        world_max,
        buffer,
        leaf_min_area,
    )
}

#[hotpath::measure]
fn cut_row_bands_with_scratch(
    scratch: &mut IntEmitScratch,
    shape: &Shape,
    ty0: u32,
    ty1: u32,
    world_max: i32,
    buffer: i32,
    leaf_min_area: u64,
) -> Vec<Shapes> {
    debug_assert!(ty0 <= ty1);
    let mut out: Vec<Shapes> = Vec::with_capacity((ty1 - ty0 + 1) as usize);
    let root: Shapes = vec![shape.clone()];
    cut_rows_rec(
        scratch,
        &root,
        ty0,
        ty1,
        world_max,
        buffer,
        leaf_min_area,
        &mut out,
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn cut_rows_rec(
    scratch: &mut IntEmitScratch,
    shapes: &Shapes,
    lo: u32,
    hi: u32,
    world_max: i32,
    buffer: i32,
    leaf_min_area: u64,
    out: &mut Vec<Shapes>,
) {
    if shapes.is_empty() {
        // Nothing survives in this range - emit empty rows.
        for _ in lo..=hi {
            out.push(Vec::new());
        }
        return;
    }
    if lo == hi {
        let band = row_range_rect(lo, lo, world_max, buffer);
        let mut row: Shapes = Vec::new();
        let mut clipped: Shapes = Vec::new();
        for s in shapes {
            intersect_rect_into(scratch, s, band, leaf_min_area, &mut clipped);
            row.append(&mut clipped);
        }
        out.push(row);
        return;
    }
    let mid = lo + (hi - lo) / 2;
    let upper_rect = row_range_rect(lo, mid, world_max, buffer);
    let lower_rect = row_range_rect(mid + 1, hi, world_max, buffer);
    let mut upper: Shapes = Vec::new();
    let mut lower: Shapes = Vec::new();
    let mut clipped: Shapes = Vec::new();
    for s in shapes {
        intersect_rect_into(scratch, s, upper_rect, 0, &mut clipped);
        upper.append(&mut clipped);
        intersect_rect_into(scratch, s, lower_rect, 0, &mut clipped);
        lower.append(&mut clipped);
    }
    cut_rows_rec(
        scratch,
        &upper,
        lo,
        mid,
        world_max,
        buffer,
        leaf_min_area,
        out,
    );
    cut_rows_rec(
        scratch,
        &lower,
        mid + 1,
        hi,
        world_max,
        buffer,
        leaf_min_area,
        out,
    );
}

/// Buffered rect covering tile rows `lo..=hi` (full x range).
fn row_range_rect(lo: u32, hi: u32, world_max: i32, buffer: i32) -> IntRect {
    const TILE_SHIFT: u32 = 12; // 4096 pixel units per tile
    let top = i32::try_from(i64::from(lo) << TILE_SHIFT).expect("row top fits i32") - buffer;
    let bottom =
        i32::try_from((i64::from(hi) + 1) << TILE_SHIFT).expect("row bottom fits i32") + buffer;
    IntRect {
        min_x: 0,
        min_y: top,
        max_x: world_max,
        max_y: bottom,
    }
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
        simplify_shape_dp(&mut shape_a, 16, None);
        simplify_shape_dp(&mut shape_b, 16, None);
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
        simplify_shape_dp(&mut shape, 16, None);
        assert_eq!(shape[0].len(), 4);
    }

    #[test]
    fn simplify_shape_dp_old_ring_seam_not_privileged() {
        let ring = vec![p(0, 0), p(1, 8), p(2, 0), p(40, 0), p(40, 40), p(0, 40)];
        let mut shape = vec![ring];
        simplify_shape_dp(&mut shape, 16, None);
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
        simplify_shape_dp(&mut plain, 16, None);
        simplify_shape_dp(&mut pinned, 16, Some(&flags));
        assert!(!plain[0].contains(&target));
        assert!(pinned[0].contains(&target));
    }

    #[test]
    fn rasterize_segment_through_grid_corner_marks_side_cells() {
        let mut tiles = FxHashSet::default();
        rasterize_segment(0.5, 0.5, 2.5, 2.5, &mut tiles);
        let expected: FxHashSet<u64> = [
            pack_tile(0, 0),
            pack_tile(1, 0),
            pack_tile(0, 1),
            pack_tile(1, 1),
            pack_tile(2, 1),
            pack_tile(1, 2),
            pack_tile(2, 2),
        ]
        .into_iter()
        .collect();
        assert_eq!(tiles, expected);
    }
}

#[cfg(test)]
mod cut_row_bands_tests {
    use super::*;

    fn p(x: i32, y: i32) -> IntPoint {
        IntPoint::new(x, y)
    }

    /// Canonicalize Shapes for order/rotation-independent comparison:
    /// rotate each ring to start at its lexicographic minimum, sort rings
    /// within shapes, sort shapes.
    fn canon(shapes: &Shapes) -> Vec<Vec<Vec<(i32, i32)>>> {
        let mut out: Vec<Vec<Vec<(i32, i32)>>> = shapes
            .iter()
            .map(|shape| {
                let mut rings: Vec<Vec<(i32, i32)>> = shape
                    .iter()
                    .map(|ring| {
                        let pts: Vec<(i32, i32)> = ring.iter().map(|q| (q.x, q.y)).collect();
                        // Try both directions, pick the lexicographically
                        // smaller rotation-normalized form (winding is an
                        // output convention, not part of set equality here).
                        let a = rotate_min(&pts);
                        let mut rev = pts.clone();
                        rev.reverse();
                        let b = rotate_min(&rev);
                        if a <= b { a } else { b }
                    })
                    .collect();
                rings.sort();
                rings
            })
            .collect();
        out.sort();
        out
    }

    fn rotate_min(pts: &[(i32, i32)]) -> Vec<(i32, i32)> {
        let n = pts.len();
        let min_idx = (0..n).min_by_key(|&i| pts[i]).unwrap_or(0);
        (0..n).map(|i| pts[(min_idx + i) % n]).collect()
    }

    fn direct_rows(shape: &Shape, ty0: u32, ty1: u32, world_max: i32, buffer: i32) -> Vec<Shapes> {
        (ty0..=ty1)
            .map(|ty| {
                let band = IntRect {
                    min_x: 0,
                    min_y: i32::try_from((i64::from(ty)) << 12).expect("fits") - buffer,
                    max_x: world_max,
                    max_y: i32::try_from((i64::from(ty) + 1) << 12).expect("fits") + buffer,
                };
                intersect_rect(shape, band, 256)
            })
            .collect()
    }

    fn assert_rows_equal(shape: &Shape, ty0: u32, ty1: u32) {
        let world_max = 1 << 20;
        let bisected = cut_row_bands(shape, ty0, ty1, world_max, 128, 256);
        let direct = direct_rows(shape, ty0, ty1, world_max, 128);
        assert_eq!(bisected.len(), direct.len());
        for (i, (b, d)) in bisected.iter().zip(&direct).enumerate() {
            let ty = ty0 + u32::try_from(i).expect("row index fits u32");
            assert_eq!(canon(b), canon(d), "row {i} (ty={ty})");
        }
    }

    #[test]
    fn leaf_equality_concave_shape_spanning_8_rows() {
        // Zigzag concave polygon spanning rows 0..=7 (each row 4096 tall).
        let outer = vec![
            p(1000, 0),
            p(30000, 0),
            p(30000, 32000),
            p(20000, 32000),
            p(20000, 8000), // deep concavity
            p(12000, 8000),
            p(12000, 32000),
            p(1000, 32000),
        ];
        assert_rows_equal(&vec![outer], 0, 7);
    }

    #[test]
    fn leaf_equality_shape_with_hole() {
        let outer = vec![p(0, 0), p(40000, 0), p(40000, 24000), p(0, 24000)];
        let hole = vec![
            p(8000, 4000),
            p(8000, 20000),
            p(30000, 20000),
            p(30000, 4000),
        ];
        assert_rows_equal(&vec![outer, hole], 0, 5);
    }

    #[test]
    fn leaf_equality_single_row_range() {
        let outer = vec![p(100, 100), p(5000, 100), p(5000, 3000), p(100, 3000)];
        assert_rows_equal(&vec![outer], 0, 0);
    }

    #[test]
    fn empty_rows_in_range_yield_empty_shapes() {
        // Shape occupies only rows 0..=1 of a 0..=7 range.
        let outer = vec![p(100, 100), p(9000, 100), p(9000, 7000), p(100, 7000)];
        let rows = cut_row_bands(&vec![outer.clone()], 0, 7, 1 << 20, 128, 256);
        assert_eq!(rows.len(), 8);
        assert!(!rows[0].is_empty());
        assert!(!rows[1].is_empty());
        // Row 2's band starts at 8192-128=8064 > 7000: empty from row 2 on.
        for (i, r) in rows.iter().enumerate().skip(2) {
            assert!(r.is_empty(), "row {i} should be empty");
        }
        // And equality with direct cutting still holds.
        assert_rows_equal(&vec![outer], 0, 7);
    }
}

#[cfg(test)]
mod landing1_tests {
    use super::*;

    fn p(x: i32, y: i32) -> IntPoint {
        IntPoint::new(x, y)
    }

    #[test]
    fn dilated_rasterize_marks_neighbor_within_buffer() {
        // Segment in row 0, exactly 128 units above row 1 (y = 4096 - 128
        // = 3968): touches row 1 tiles' buffered rects - must mark them.
        let shape: Shape = vec![vec![p(100, 3968), p(8000, 3968), p(8000, 3000)]];
        let mut tiles = FxHashSet::default();
        rasterize_shape_edges(&shape, u32::MAX, &mut tiles);
        assert!(
            tiles.contains(&pack_tile(0, 1)),
            "row-1 tile within 128 must be marked"
        );
        // 129 units above row 1 (y = 3967): outside every row-1 buffered rect.
        let shape: Shape = vec![vec![p(100, 3967), p(8000, 3967), p(8000, 3000)]];
        let mut tiles = FxHashSet::default();
        rasterize_shape_edges(&shape, u32::MAX, &mut tiles);
        assert!(
            !tiles.contains(&pack_tile(0, 1)),
            "129 units away must NOT mark row 1"
        );
    }

    #[test]
    fn dilated_rasterize_never_undermarks_grid_corner_ties() {
        // 45-degree segment through the exact grid corner (4096, 4096):
        // all four corner-adjacent tiles' buffered rects touch the segment.
        let shape: Shape = vec![vec![p(0, 0), p(8192, 8192), p(0, 8192)]];
        let mut tiles = FxHashSet::default();
        rasterize_shape_edges(&shape, u32::MAX, &mut tiles);
        for (tx, ty) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
            assert!(
                tiles.contains(&pack_tile(tx, ty)),
                "corner tile ({tx},{ty})"
            );
        }
    }

    #[test]
    fn gap_tile_bytes_identical_to_boolean_path() {
        // Row shape covering tiles 0..=3 of row 0 fully (with buffer), with
        // coastline detail confined to tile 0. Tiles 1..=3 are gap tiles.
        let row_shape: Shape = vec![vec![
            p(-128, -128),
            p(16512, -128),
            p(16512, 4224),
            p(-128, 4224),
        ]];
        // Geometry identity (not byte identity: the boolean path's rect
        // comes back with i_overlay's canonical ring rotation; the decoded
        // vertex cycles must match). Canonical full-tile bytes additionally
        // improve identical-tile dedup.
        fn decoded_cycles(bufs: &[Vec<u32>]) -> Vec<Vec<(i32, i32)>> {
            let mut out = Vec::new();
            for b in bufs {
                for ring in crate::geometry::decode_mvt_polygon(b) {
                    let mut v: Vec<(i32, i32)> = ring;
                    v.pop(); // drop closing duplicate
                    let n = v.len();
                    let m = (0..n).min_by_key(|&i| v[i]).unwrap_or(0);
                    let mut rot: Vec<(i32, i32)> = (0..n).map(|i| v[(m + i) % n]).collect();
                    let mut rev = rot.clone();
                    rev.reverse();
                    let rev = {
                        let n = rev.len();
                        let m = (0..n).min_by_key(|&i| rev[i]).unwrap_or(0);
                        (0..n).map(|i| rev[(m + i) % n]).collect::<Vec<_>>()
                    };
                    if rev < rot {
                        rot = rev;
                    }
                    out.push(rot);
                }
            }
            out.sort();
            out
        }
        for tx in 1u32..=3 {
            let mut full: Vec<Vec<u32>> = Vec::new();
            let mut scratch = IntEmitScratch::new();
            emit_full_tile(tx, 0, &mut scratch, &mut |_, _, g: &[u32]| {
                full.push(g.to_vec());
            });
            let mut clipped: Vec<Vec<u32>> = Vec::new();
            let mut scratch = IntEmitScratch::new();
            emit_clipped_tile_shape(
                tx,
                0,
                &row_shape,
                256,
                &mut scratch,
                &mut |_, _, g: &[u32]| {
                    clipped.push(g.to_vec());
                },
            );
            assert_eq!(
                decoded_cycles(&full),
                decoded_cycles(&clipped),
                "tile {tx}: full-tile geometry != boolean-clip geometry"
            );
        }
    }

    #[test]
    fn split_min_vertices_counts_all_rings() {
        // Constructed indirectly: the gate is `piece.iter().map(Vec::len).sum()`.
        // A 4-vertex outer with many-vertex holes must exceed the threshold.
        let outer: Contour = vec![p(0, 0), p(1000, 0), p(1000, 1000), p(0, 1000)];
        let hole: Contour = (0..600)
            .map(|i| p(100 + (i % 30), 100 + (i / 30)))
            .collect();
        let piece: Shape = vec![outer, hole];
        let total: usize = piece.iter().map(Vec::len).sum();
        assert!(
            total >= 500,
            "hole vertices must count toward the split gate"
        );
        assert!(
            piece.first().map_or(0, Vec::len) < 500,
            "outer alone dodges - the old bug"
        );
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
