use crate::geometry::int_ocean::{
    Contour, IntEmitScratch, IntRect, Shape, Shapes, TILE_BUFFER_I32, TILE_EXTENT_I32,
    contour_area_is_below, emit_full_tile, encode_tile_shape, intersect_rect_into, normalize_into,
    rescale_shape_pinned, shape_bbox, simplify_shape_dp,
};
use i_overlay::i_float::int::point::IntPoint;
use rustc_hash::FxHashSet;

pub(crate) struct PyramidParams<'a> {
    pub maxz: u8,
    pub z_top: u8,
    pub z_bottom: u8,
    pub dp_tol: &'a (dyn Fn(u8) -> i64 + Sync),
    pub min_area: &'a (dyn Fn(u8) -> u64 + Sync),
    pub pins: Option<&'a FxHashSet<(i32, i32)>>,
}

pub(crate) type PyramidSink<'a> = &'a mut dyn FnMut(u8, u32, u32, &[u32]);

pub(crate) struct PyramidScratch {
    pub int: IntEmitScratch,
    frag_pool: Vec<Shapes>,
    edge_flags: Vec<Vec<bool>>,
}

impl PyramidScratch {
    pub(crate) fn new() -> Self {
        Self {
            int: IntEmitScratch::new(),
            frag_pool: Vec::new(),
            edge_flags: Vec::new(),
        }
    }

    fn take_shapes(&mut self) -> Shapes {
        self.frag_pool.pop().unwrap_or_default()
    }

    fn return_shapes(&mut self, mut shapes: Shapes) {
        shapes.clear();
        self.frag_pool.push(shapes);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PyramidCell {
    pub z: u8,
    pub tx: u32,
    pub ty: u32,
}

pub(crate) fn emit_shape_pyramid(
    shape_base: &Shape,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
) {
    if shape_base.is_empty() || !valid_params(params) {
        return;
    }

    let roots = root_fragments(shape_base, params, scratch);
    for (cell, frag) in roots {
        descend(cell, frag, params, scratch, sink);
    }
}

pub(crate) fn emit_shape_pyramid_cell(
    cell: PyramidCell,
    frag: Shapes,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
) {
    if !valid_params(params) || cell.z < params.z_top || cell.z > params.z_bottom {
        scratch.return_shapes(frag);
        return;
    }
    descend(cell, frag, params, scratch, sink);
}

pub(crate) fn split_for_parallel(
    shape_base: &Shape,
    params: &PyramidParams<'_>,
    target_items: usize,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
) -> Vec<(PyramidCell, Shapes)> {
    if shape_base.is_empty() || !valid_params(params) {
        return Vec::new();
    }

    let target = target_items.max(1);
    let mut frontier_depth = 0_u8;
    let mut capacity = 1_usize;
    while capacity < target && params.z_top + frontier_depth < params.z_bottom {
        frontier_depth += 1;
        capacity = capacity.saturating_mul(4);
    }
    let frontier_z = params.z_top + frontier_depth;

    let roots = root_fragments(shape_base, params, scratch);
    let mut out = Vec::with_capacity(capacity);
    for (cell, frag) in roots {
        split_descend(cell, frag, frontier_z, params, scratch, sink, &mut out);
    }
    out
}

fn valid_params(params: &PyramidParams<'_>) -> bool {
    params.z_top <= params.z_bottom && params.z_bottom <= params.maxz
}

fn root_fragments(
    shape_base: &Shape,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
) -> Vec<(PyramidCell, Shapes)> {
    let mut normalized = scratch.take_shapes();
    normalize_into(&mut scratch.int, shape_base.clone(), 0, &mut normalized);
    if normalized.is_empty() {
        scratch.return_shapes(normalized);
        return Vec::new();
    }

    let Some(bbox) = shapes_bbox(&normalized) else {
        scratch.return_shapes(normalized);
        return Vec::new();
    };
    let (tx_min, tx_max, ty_min, ty_max) =
        tile_range_for_base_rect(bbox, params.maxz, params.z_top);
    let mut roots = Vec::new();
    for ty in ty_min..=ty_max {
        for tx in tx_min..=tx_max {
            let cell = PyramidCell {
                z: params.z_top,
                tx,
                ty,
            };
            let mut frag = scratch.take_shapes();
            intersect_shapes_with_rect(
                &mut scratch.int,
                &normalized,
                buffered_cell_rect_base(params.maxz, cell),
                0,
                &mut frag,
            );
            if frag.is_empty() {
                scratch.return_shapes(frag);
            } else {
                roots.push((cell, frag));
            }
        }
    }
    scratch.return_shapes(normalized);
    roots
}

fn descend(
    cell: PyramidCell,
    frag: Shapes,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
) {
    if frag.is_empty() {
        scratch.return_shapes(frag);
        return;
    }

    if is_full_buffered_cell(&frag, params.maxz, cell) {
        emit_full_subtree(cell, params, scratch, sink);
        scratch.return_shapes(frag);
        return;
    }

    emit_cell(cell, &frag, params, scratch, sink);
    if cell.z == params.z_bottom {
        scratch.return_shapes(frag);
        return;
    }

    for child in children(cell) {
        let mut child_frag = scratch.take_shapes();
        intersect_shapes_with_rect(
            &mut scratch.int,
            &frag,
            buffered_cell_rect_base(params.maxz, child),
            0,
            &mut child_frag,
        );
        descend(child, child_frag, params, scratch, sink);
    }
    scratch.return_shapes(frag);
}

fn split_descend(
    cell: PyramidCell,
    frag: Shapes,
    frontier_z: u8,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
    out: &mut Vec<(PyramidCell, Shapes)>,
) {
    if frag.is_empty() {
        scratch.return_shapes(frag);
        return;
    }

    if is_full_buffered_cell(&frag, params.maxz, cell) {
        emit_full_subtree(cell, params, scratch, sink);
        scratch.return_shapes(frag);
        return;
    }

    if cell.z >= frontier_z || cell.z == params.z_bottom {
        out.push((cell, frag));
        return;
    }

    emit_cell(cell, &frag, params, scratch, sink);
    for child in children(cell) {
        let mut child_frag = scratch.take_shapes();
        intersect_shapes_with_rect(
            &mut scratch.int,
            &frag,
            buffered_cell_rect_base(params.maxz, child),
            0,
            &mut child_frag,
        );
        split_descend(child, child_frag, frontier_z, params, scratch, sink, out);
    }
    scratch.return_shapes(frag);
}

fn emit_cell(
    cell: PyramidCell,
    frag: &Shapes,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
) {
    let shift = params.maxz - cell.z;
    let dp_tol = (params.dp_tol)(cell.z);
    let min_area = (params.min_area)(cell.z);
    for shape in frag {
        build_edge_flags(shape, params, cell, &mut scratch.edge_flags);
        let (mut shape_z, flags_z) = rescale_shape_pinned(shape, shift, &scratch.edge_flags);
        if shape_z.is_empty() {
            continue;
        }
        simplify_shape_dp(&mut shape_z, dp_tol, Some(&flags_z));
        if shape_z.is_empty() {
            continue;
        }

        if dp_tol > 0 && is_convex_single_ring(&shape_z) {
            if !contour_area_is_below(&shape_z[0], min_area) {
                encode_tile_shape(
                    shape_z,
                    cell.tx,
                    cell.ty,
                    &mut scratch.int,
                    &mut |tx, ty, geom| {
                        sink(cell.z, tx, ty, geom);
                    },
                );
            }
            continue;
        }

        let mut normalized = scratch.take_shapes();
        normalize_into(&mut scratch.int, shape_z, min_area, &mut normalized);
        for tile_shape in normalized.drain(..) {
            encode_tile_shape(
                tile_shape,
                cell.tx,
                cell.ty,
                &mut scratch.int,
                &mut |tx, ty, geom| {
                    sink(cell.z, tx, ty, geom);
                },
            );
        }
        scratch.return_shapes(normalized);
    }
}

fn emit_full_subtree(
    cell: PyramidCell,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
) {
    for z in cell.z..=params.z_bottom {
        let delta = u32::from(z - cell.z);
        let scale = 1_u32 << delta;
        let tx0 = cell.tx << delta;
        let ty0 = cell.ty << delta;
        for ty in ty0..ty0 + scale {
            for tx in tx0..tx0 + scale {
                emit_full_tile(tx, ty, &mut scratch.int, &mut |out_tx, out_ty, geom| {
                    sink(z, out_tx, out_ty, geom);
                });
            }
        }
    }
}

fn children(cell: PyramidCell) -> [PyramidCell; 4] {
    let z = cell.z + 1;
    let tx = cell.tx * 2;
    let ty = cell.ty * 2;
    [
        PyramidCell { z, tx, ty },
        PyramidCell { z, tx: tx + 1, ty },
        PyramidCell { z, tx, ty: ty + 1 },
        PyramidCell {
            z,
            tx: tx + 1,
            ty: ty + 1,
        },
    ]
}

fn intersect_shapes_with_rect(
    scratch: &mut IntEmitScratch,
    shapes: &Shapes,
    rect: IntRect,
    min_area: u64,
    out: &mut Shapes,
) {
    out.clear();
    let mut clipped = Vec::new();
    for shape in shapes {
        intersect_rect_into(scratch, shape, rect, min_area, &mut clipped);
        out.append(&mut clipped);
    }
}

fn shapes_bbox(shapes: &Shapes) -> Option<IntRect> {
    let mut out: Option<IntRect> = None;
    for shape in shapes {
        let Some(bb) = shape_bbox(shape) else {
            continue;
        };
        out = Some(match out {
            Some(acc) => IntRect {
                min_x: acc.min_x.min(bb.min_x),
                min_y: acc.min_y.min(bb.min_y),
                max_x: acc.max_x.max(bb.max_x),
                max_y: acc.max_y.max(bb.max_y),
            },
            None => bb,
        });
    }
    out
}

fn tile_range_for_base_rect(rect: IntRect, maxz: u8, z: u8) -> (u32, u32, u32, u32) {
    let max_tile = (1_u32 << z) - 1;
    let tile_size = cell_tile_size_base(maxz, z);
    (
        base_tile_index(rect.min_x, tile_size, max_tile),
        base_tile_index(rect.max_x, tile_size, max_tile),
        base_tile_index(rect.min_y, tile_size, max_tile),
        base_tile_index(rect.max_y, tile_size, max_tile),
    )
}

fn base_tile_index(q: i32, tile_size: i64, max_tile: u32) -> u32 {
    if q <= 0 {
        0
    } else {
        let idx = i64::from(q).div_euclid(tile_size);
        u32::try_from(idx)
            .expect("tile index fits u32")
            .min(max_tile)
    }
}

fn buffered_cell_rect_base(maxz: u8, cell: PyramidCell) -> IntRect {
    let tile_size = cell_tile_size_base(maxz, cell.z);
    let buffer = i64::from(TILE_BUFFER_I32) << u32::from(maxz - cell.z);
    let tx = i64::from(cell.tx);
    let ty = i64::from(cell.ty);
    IntRect {
        min_x: i32::try_from(tx * tile_size - buffer).expect("cell min x fits i32"),
        min_y: i32::try_from(ty * tile_size - buffer).expect("cell min y fits i32"),
        max_x: i32::try_from((tx + 1) * tile_size + buffer).expect("cell max x fits i32"),
        max_y: i32::try_from((ty + 1) * tile_size + buffer).expect("cell max y fits i32"),
    }
}

fn cell_tile_size_base(maxz: u8, z: u8) -> i64 {
    i64::from(TILE_EXTENT_I32) << u32::from(maxz - z)
}

fn is_full_buffered_cell(frag: &Shapes, maxz: u8, cell: PyramidCell) -> bool {
    if frag.len() != 1 || frag[0].len() != 1 || frag[0][0].len() != 4 {
        return false;
    }
    let rect = buffered_cell_rect_base(maxz, cell);
    let ring = &frag[0][0];
    let corners = [
        IntPoint::new(rect.min_x, rect.min_y),
        IntPoint::new(rect.max_x, rect.min_y),
        IntPoint::new(rect.max_x, rect.max_y),
        IntPoint::new(rect.min_x, rect.max_y),
    ];
    // Both directions: every ring point is a corner AND every corner is
    // present. A degenerate 4-point ring repeating a corner must not
    // classify as full - it would emit an entire subtree of full tiles
    // for a sliver.
    if !ring.iter().all(|p| corners.contains(p)) || !corners.iter().all(|c| ring.contains(c)) {
        return false;
    }
    debug_assert_eq!(shape_bbox(&frag[0]), Some(rect));
    true
}

fn build_edge_flags(
    shape: &Shape,
    params: &PyramidParams<'_>,
    cell: PyramidCell,
    out: &mut Vec<Vec<bool>>,
) {
    out.clear();
    let tile_size = cell_tile_size_base(params.maxz, cell.z);
    let buffer = i64::from(TILE_BUFFER_I32) << u32::from(params.maxz - cell.z);
    let left = i64::from(cell.tx) * tile_size;
    let right = i64::from(cell.tx + 1) * tile_size;
    let top = i64::from(cell.ty) * tile_size;
    let bottom = i64::from(cell.ty + 1) * tile_size;
    for ring in shape {
        let mut flags = Vec::with_capacity(ring.len());
        for p in ring {
            let junction = params.pins.is_some_and(|pins| pins.contains(&(p.x, p.y)));
            let edge_window = near_line(i64::from(p.x), left, buffer)
                || near_line(i64::from(p.x), right, buffer)
                || near_line(i64::from(p.y), top, buffer)
                || near_line(i64::from(p.y), bottom, buffer);
            flags.push(junction || edge_window);
        }
        if ring.len() >= 2 {
            for i in 0..ring.len() {
                let j = if i + 1 < ring.len() { i + 1 } else { 0 };
                if segment_crosses_line(i64::from(ring[i].x), i64::from(ring[j].x), left)
                    || segment_crosses_line(i64::from(ring[i].x), i64::from(ring[j].x), right)
                    || segment_crosses_line(i64::from(ring[i].y), i64::from(ring[j].y), top)
                    || segment_crosses_line(i64::from(ring[i].y), i64::from(ring[j].y), bottom)
                {
                    flags[i] = true;
                    flags[j] = true;
                }
            }
        }
        out.push(flags);
    }
}

fn near_line(v: i64, line: i64, buffer: i64) -> bool {
    (v - line).abs() <= buffer
}

fn segment_crosses_line(a: i64, b: i64, line: i64) -> bool {
    a.min(b) <= line && a.max(b) >= line && a != b
}

fn is_convex_single_ring(shape: &Shape) -> bool {
    if shape.len() != 1 || shape[0].len() < 3 {
        return false;
    }
    is_convex_ring(&shape[0])
}

fn is_convex_ring(ring: &Contour) -> bool {
    let mut sign = 0_i8;
    for i in 0..ring.len() {
        let a = ring[i];
        let b = ring[(i + 1) % ring.len()];
        let c = ring[(i + 2) % ring.len()];
        let cross = (i128::from(b.x) - i128::from(a.x)) * (i128::from(c.y) - i128::from(a.y))
            - (i128::from(b.y) - i128::from(a.y)) * (i128::from(c.x) - i128::from(a.x));
        if cross == 0 {
            continue;
        }
        let this_sign = if cross > 0 { 1 } else { -1 };
        if sign == 0 {
            sign = this_sign;
        } else if sign != this_sign {
            return false;
        }
    }
    sign != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::decode_mvt_polygon;
    use crate::geometry::int_ocean::{
        buffered_tile_rect, emit_full_tile, intersect_rect, normalize, rescale_shape,
    };
    use i_overlay::core::fill_rule::FillRule;
    use i_overlay::core::overlay::{IntOverlayOptions, Overlay, ShapeType};
    use i_overlay::core::overlay_rule::OverlayRule;
    use std::collections::BTreeMap;

    fn p(x: i32, y: i32) -> IntPoint {
        IntPoint::new(x, y)
    }

    fn shape(outer: &[(i32, i32)], inners: &[&[(i32, i32)]]) -> Shape {
        let mut out = Vec::with_capacity(1 + inners.len());
        out.push(outer.iter().map(|&(x, y)| p(x, y)).collect());
        for inner in inners {
            out.push(inner.iter().map(|&(x, y)| p(x, y)).collect());
        }
        out
    }

    fn params(maxz: u8, z_top: u8, z_bottom: u8, tol: i64) -> PyramidParams<'static> {
        let dp_tol: &'static (dyn Fn(u8) -> i64 + Sync) = Box::leak(Box::new(move |_| tol));
        PyramidParams {
            maxz,
            z_top,
            z_bottom,
            dp_tol,
            min_area: &|_| 0,
            pins: None,
        }
    }

    fn collect_pyramid(
        shape: &Shape,
        params: &PyramidParams<'_>,
    ) -> BTreeMap<(u8, u32, u32), Vec<u32>> {
        let mut scratch = PyramidScratch::new();
        let mut out = BTreeMap::new();
        emit_shape_pyramid(shape, params, &mut scratch, &mut |z, tx, ty, geom| {
            out.entry((z, tx, ty))
                .or_insert_with(Vec::new)
                .extend_from_slice(geom);
        });
        out
    }

    fn collect_direct(
        shape_base: &Shape,
        params: &PyramidParams<'_>,
    ) -> BTreeMap<(u8, u32, u32), Vec<u32>> {
        let mut scratch = IntEmitScratch::new();
        let mut out = BTreeMap::new();
        for z in params.z_top..=params.z_bottom {
            let shift = params.maxz - z;
            let shape_z = rescale_shape(shape_base, shift);
            let normalized = normalize(shape_z, (params.min_area)(z));
            for shape in &normalized {
                let Some(bb) = shape_bbox(shape) else {
                    continue;
                };
                let max_tile = (1_u32 << z) - 1;
                let (tx_min, tx_max, ty_min, ty_max) =
                    crate::geometry::int_ocean::tile_range_for_rect(bb, max_tile);
                for ty in ty_min..=ty_max {
                    for tx in tx_min..=tx_max {
                        let clipped = intersect_rect(shape, buffered_tile_rect(tx, ty), 0);
                        for tile_shape in clipped {
                            if is_full_zoom_tile_shape(&tile_shape, tx, ty) {
                                emit_full_tile(
                                    tx,
                                    ty,
                                    &mut scratch,
                                    &mut |out_tx, out_ty, geom| {
                                        out.entry((z, out_tx, out_ty))
                                            .or_insert_with(Vec::new)
                                            .extend_from_slice(geom);
                                    },
                                );
                            } else {
                                encode_tile_shape(
                                    tile_shape,
                                    tx,
                                    ty,
                                    &mut scratch,
                                    &mut |out_tx, out_ty, geom| {
                                        out.entry((z, out_tx, out_ty))
                                            .or_insert_with(Vec::new)
                                            .extend_from_slice(geom);
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
        out
    }

    fn is_full_zoom_tile_shape(shape: &Shape, tx: u32, ty: u32) -> bool {
        if shape.len() != 1 || shape[0].len() != 4 {
            return false;
        }
        let rect = buffered_tile_rect(tx, ty);
        let corners = [
            p(rect.min_x, rect.min_y),
            p(rect.max_x, rect.min_y),
            p(rect.max_x, rect.max_y),
            p(rect.min_x, rect.max_y),
        ];
        shape[0].iter().all(|point| corners.contains(point))
    }

    #[test]
    fn cut_identity_dp_tol_0_byte_identity_reference() {
        let outer = [(900, 900), (13500, 900), (13500, 13200), (900, 13200)];
        let inner = [(5200, 5200), (5200, 7200), (7200, 7200), (7200, 5200)];
        let shape = shape(&outer, &[&inner]);
        let params = params(2, 0, 2, 0);
        assert_eq!(
            collect_pyramid(&shape, &params),
            collect_direct(&shape, &params)
        );
    }

    #[test]
    fn full_subtree_shortcut_emits_full_tile_record_set() {
        let params = params(2, 0, 2, 0);
        let rect = buffered_cell_rect_base(2, PyramidCell { z: 0, tx: 0, ty: 0 });
        let full: Shape = vec![vec![
            p(rect.min_x, rect.min_y),
            p(rect.max_x, rect.min_y),
            p(rect.max_x, rect.max_y),
            p(rect.min_x, rect.max_y),
        ]];

        let got = collect_pyramid(&full, &params);
        let mut expected = BTreeMap::new();
        let mut scratch = IntEmitScratch::new();
        for z in 0..=2 {
            let scale = 1_u32 << u32::from(z);
            for ty in 0..scale {
                for tx in 0..scale {
                    emit_full_tile(tx, ty, &mut scratch, &mut |out_tx, out_ty, geom| {
                        expected.insert((z, out_tx, out_ty), geom.to_vec());
                    });
                }
            }
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn seam_window_dp_tol_16_xor_empty_for_shared_window() {
        let params = params(1, 1, 1, 16);
        let source = shape(
            &[
                (3300, 600),
                (4850, 3500),
                (4620, 3650),
                (4230, 2450),
                (4090, 2520),
                (3970, 2380),
                (3650, 800),
            ],
            &[],
        );
        let mut int = IntEmitScratch::new();
        let left_frag = intersect_rect(
            &source,
            buffered_cell_rect_base(1, PyramidCell { z: 1, tx: 0, ty: 0 }),
            0,
        );
        let right_frag = intersect_rect(
            &source,
            buffered_cell_rect_base(1, PyramidCell { z: 1, tx: 1, ty: 0 }),
            0,
        );

        let mut left = BTreeMap::new();
        let mut right = BTreeMap::new();
        let mut ps = PyramidScratch::new();
        emit_shape_pyramid_cell(
            PyramidCell { z: 1, tx: 0, ty: 0 },
            left_frag,
            &params,
            &mut ps,
            &mut |z, tx, ty, geom| {
                left.insert((z, tx, ty), geom.to_vec());
            },
        );
        emit_shape_pyramid_cell(
            PyramidCell { z: 1, tx: 1, ty: 0 },
            right_frag,
            &params,
            &mut ps,
            &mut |z, tx, ty, geom| {
                right.insert((z, tx, ty), geom.to_vec());
            },
        );

        let window = IntRect {
            min_x: 4096 - 128,
            min_y: -128,
            max_x: 4096 + 128,
            max_y: 4096 + 128,
        };
        let left_shapes = commands_to_window_shapes(&left, window, &mut int);
        let right_shapes = commands_to_window_shapes(&right, window, &mut int);
        assert!(xor_shapes_empty(&left_shapes, &right_shapes));
    }

    #[test]
    fn empty_pruning_emits_nothing() {
        let params = params(2, 0, 2, 0);
        let got = collect_pyramid(&Vec::new(), &params);
        assert!(got.is_empty());
    }

    #[test]
    fn world_edge_cell_rects_extend_and_clamp_edge_cells() {
        let rect = buffered_cell_rect_base(2, PyramidCell { z: 1, tx: 0, ty: 0 });
        assert!(rect.min_x < 0);
        assert!(rect.min_y < 0);
        let max_rect = buffered_cell_rect_base(2, PyramidCell { z: 1, tx: 1, ty: 1 });
        assert!(max_rect.max_x > 1_i32 << 14);
        assert!(max_rect.max_y > 1_i32 << 14);

        let source = shape(&[(-80, 500), (700, 500), (700, 1400), (-80, 1400)], &[]);
        let params = params(2, 1, 1, 0);
        let got = collect_pyramid(&source, &params);
        assert!(!got.is_empty());
        assert!(got.keys().all(|&(_, tx, ty)| tx == 0 && ty == 0));
    }

    fn commands_to_window_shapes(
        records: &BTreeMap<(u8, u32, u32), Vec<u32>>,
        window: IntRect,
        scratch: &mut IntEmitScratch,
    ) -> Shapes {
        let mut out = Vec::new();
        for (&(_z, tx, ty), geom) in records {
            let ox = crate::geometry::int_ocean::tile_origin(tx);
            let oy = crate::geometry::int_ocean::tile_origin(ty);
            let rings = decode_mvt_polygon(geom);
            if rings.is_empty() {
                continue;
            }
            let mut tile_shape = Vec::with_capacity(rings.len());
            for ring in rings {
                let mut contour: Contour =
                    ring.into_iter().map(|(x, y)| p(x + ox, y + oy)).collect();
                if contour.first() == contour.last() {
                    contour.pop();
                }
                tile_shape.push(contour);
            }
            for normalized in normalize(tile_shape, 0) {
                let mut clipped = Vec::new();
                intersect_rect_into(scratch, &normalized, window, 0, &mut clipped);
                out.append(&mut clipped);
            }
        }
        out
    }

    fn xor_shapes_empty(a: &Shapes, b: &Shapes) -> bool {
        let options = IntOverlayOptions {
            output_direction: i_overlay::core::overlay::ContourDirection::CounterClockwise,
            min_output_area: 0,
            ..Default::default()
        };
        let mut overlay = Overlay::new_custom(0, options, Default::default());
        for shape in a {
            overlay.add_shape(shape, ShapeType::Subject);
        }
        for shape in b {
            overlay.add_shape(shape, ShapeType::Clip);
        }
        let result = overlay.overlay(OverlayRule::Xor, FillRule::NonZero);
        result.is_empty()
    }
}
