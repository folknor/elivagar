use crate::debug::RING_CAP;
use crate::geometry::int_ocean::{
    Contour, IntEmitScratch, IntPoint, IntRect, Shape, Shapes, TILE_BUFFER_I32, TILE_EXTENT_I32,
    buffered_tile_rect, contour_area_is_below, copy_shape_into, emit_full_tile, encode_tile_shape,
    intersect_rect_into, normalize_into, point_in_contour, rescale_shape_pinned_into, shape_bbox,
    signed_area_2x, simplify_shape_dp, simplify_shape_vw, vw_area_threshold,
};
use crate::mvt::MAX_FEATURE_RINGS;
use rustc_hash::FxHashSet;
use std::sync::atomic::Ordering;

// The ocean producer's output from this engine is cached world-wide in the
// durable ocean artifact (`ocean-build`, keyed by OCEAN_POLICY_VERSION in
// src/ocean.rs). A change here that alters emitted bytes reaches artifact
// consumers only through a version bump - without it, existing artifacts
// keep key-validating and every artifact-active run serves the pre-change
// geometry while all standing gates stay green.
pub(crate) struct PyramidParams<'a> {
    pub maxz: u8,
    pub z_top: u8,
    pub z_bottom: u8,
    pub dp_tol: &'a (dyn Fn(u8) -> i64 + Sync),
    pub min_area: &'a (dyn Fn(u8) -> u64 + Sync),
    pub pins: Option<&'a FxHashSet<(i32, i32)>>,
    /// Ocean supplies this ownership predicate so artifact-interior cells are
    /// pruned before geometry work. OSM leaves it unset.
    pub tile_filter: Option<&'a (dyn Fn(u8, u32, u32) -> bool + Sync)>,
    pub simplifier: Simplifier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Simplifier {
    DouglasPeucker,
    Visvalingam,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PyramidEmitKind {
    Fragment,
    FullFill,
}

pub(crate) type PyramidSink<'a> = &'a mut dyn FnMut(u8, u32, u32, &[u32], PyramidEmitKind);

pub(crate) struct PyramidScratch {
    pub int: IntEmitScratch,
    frag_pool: Vec<Shapes>,
    edge_flags: Vec<Vec<bool>>,
    flags_z: Vec<Vec<bool>>,
}

impl PyramidScratch {
    pub(crate) fn new() -> Self {
        Self {
            int: IntEmitScratch::new(),
            frag_pool: Vec::new(),
            edge_flags: Vec::new(),
            flags_z: Vec::new(),
        }
    }

    fn take_shapes(&mut self) -> Shapes {
        self.frag_pool.pop().unwrap_or_default()
    }

    fn return_shapes(&mut self, mut shapes: Shapes) {
        self.int.recycle_shapes(&mut shapes);
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
    let mut items = root_fragments(shape_base, params, scratch);
    // Expand items one level at a time - largest fragment first - until
    // the pool is large enough for the parallel fan-out AND no item is
    // oversized. Expansion emits the expanded cell (and resolves
    // full/empty subtrees inline), so the returned items' subtrees are
    // disjoint and complete. The oversize rule exists for the straggler
    // tail: one dense-coastline cell must not become a near-second work
    // item while every other worker idles.
    const OVERSIZED_FRAGMENT_VERTICES: usize = 8192;
    loop {
        let need_count = items.len() < target;
        let Some(idx) = items
            .iter()
            .enumerate()
            .filter(|(_, (c, frag))| {
                c.z < params.z_bottom
                    && (need_count || shapes_vertices(frag) > OVERSIZED_FRAGMENT_VERTICES)
            })
            .max_by_key(|(_, (_, frag))| shapes_vertices(frag))
            .map(|(i, _)| i)
        else {
            break;
        };
        let (cell, frag) = items.swap_remove(idx);
        if !cell_is_owned(cell, params) {
            scratch.return_shapes(frag);
            continue;
        }
        if is_full_buffered_cell(&frag, params.maxz, cell) {
            emit_full_subtree(cell, params, scratch, sink);
            scratch.return_shapes(frag);
            continue;
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
            if child_frag.is_empty() {
                scratch.return_shapes(child_frag);
            } else {
                items.push((child, child_frag));
            }
        }
        scratch.return_shapes(frag);
    }
    items
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
    let mut root_shape = scratch.int.take_shape(shape_base.len());
    copy_shape_into(shape_base, &mut root_shape);
    normalize_into(&mut scratch.int, root_shape, 0, &mut normalized);
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
    root_bisect(
        RootRange {
            tx0: tx_min,
            tx1: tx_max,
            ty0: ty_min,
            ty1: ty_max,
        },
        normalized,
        params,
        scratch,
        &mut roots,
    );
    roots
}

#[derive(Clone, Copy)]
struct RootRange {
    tx0: u32,
    tx1: u32,
    ty0: u32,
    ty1: u32,
}

/// Recursive quadrant bisection of the z_top cell range: cuts the shape set
/// with buffered RANGE rects, halving the longer axis each level -
/// O(V log cells) total noding instead of the O(V x cells) of clipping the
/// whole shape once per root cell. The cut identity holds because a child
/// range's buffered rect is a subset of its parent's (same buffer, subset
/// range), same argument as the per-cell descent.
fn root_bisect(
    range: RootRange,
    frag: Shapes,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    out: &mut Vec<(PyramidCell, Shapes)>,
) {
    if frag.is_empty() {
        scratch.return_shapes(frag);
        return;
    }
    if range.tx0 == range.tx1 && range.ty0 == range.ty1 {
        out.push((
            PyramidCell {
                z: params.z_top,
                tx: range.tx0,
                ty: range.ty0,
            },
            frag,
        ));
        return;
    }
    let (a, b) = if range.tx1 - range.tx0 >= range.ty1 - range.ty0 {
        let mid = range.tx0 + (range.tx1 - range.tx0) / 2;
        (
            RootRange { tx1: mid, ..range },
            RootRange {
                tx0: mid + 1,
                ..range
            },
        )
    } else {
        let mid = range.ty0 + (range.ty1 - range.ty0) / 2;
        (
            RootRange { ty1: mid, ..range },
            RootRange {
                ty0: mid + 1,
                ..range
            },
        )
    };
    for half in [a, b] {
        let mut half_frag = scratch.take_shapes();
        intersect_shapes_with_rect(
            &mut scratch.int,
            &frag,
            buffered_range_rect_base(params.maxz, params.z_top, half),
            0,
            &mut half_frag,
        );
        root_bisect(half, half_frag, params, scratch, out);
    }
    scratch.return_shapes(frag);
}

fn buffered_range_rect_base(maxz: u8, z: u8, r: RootRange) -> IntRect {
    let tile_size = cell_tile_size_base(maxz, z);
    let buffer = i64::from(TILE_BUFFER_I32) << u32::from(maxz - z);
    IntRect {
        min_x: i32::try_from(i64::from(r.tx0) * tile_size - buffer).expect("range min x fits i32"),
        min_y: i32::try_from(i64::from(r.ty0) * tile_size - buffer).expect("range min y fits i32"),
        max_x: i32::try_from((i64::from(r.tx1) + 1) * tile_size + buffer)
            .expect("range max x fits i32"),
        max_y: i32::try_from((i64::from(r.ty1) + 1) * tile_size + buffer)
            .expect("range max y fits i32"),
    }
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

    if !cell_is_owned(cell, params) {
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
        let mut shape_z = scratch.int.take_shape(shape.len());
        rescale_shape_pinned_into(
            &mut scratch.int,
            shape,
            shift,
            &scratch.edge_flags,
            &mut shape_z,
            &mut scratch.flags_z,
        );
        if shape_z.is_empty() {
            scratch.int.recycle_owned_shape(shape_z);
            continue;
        }
        match params.simplifier {
            Simplifier::DouglasPeucker => simplify_shape_dp(
                &mut scratch.int,
                &mut shape_z,
                dp_tol,
                Some(&scratch.flags_z),
            ),
            Simplifier::Visvalingam => simplify_shape_vw(
                &mut scratch.int,
                &mut shape_z,
                vw_area_threshold(dp_tol),
                Some(&scratch.flags_z),
            ),
        }
        if shape_z.is_empty() {
            scratch.int.recycle_owned_shape(shape_z);
            continue;
        }
        // Brick 8 (spec 3.3 contingency, tripped by norway +34-38% coastal
        // layer bytes): the hard window pins keep full-resolution coastline
        // in every edge strip. Thin the pinned window runs with a second DP
        // at tolerance 2 - interior vertices already carry no sub-dp_tol
        // detail so they are unaffected; window runs drop to 2-unit
        // fidelity. Cross-side seam drift is now bounded by 2 zoom units
        // (sub-pixel at render) instead of zero; the seam-window test
        // budget matches. Skipped when dp_tol <= 2 (seam-deferral layers
        // stay verbatim).
        if dp_tol > 2 {
            thin_window_runs(
                &mut shape_z,
                cell,
                &mut scratch.edge_flags,
                &mut scratch.int,
            );
            if shape_z.is_empty() {
                scratch.int.recycle_owned_shape(shape_z);
                continue;
            }
        }

        if dp_tol > 0 && is_convex_single_ring(&shape_z) {
            if !contour_area_is_below(&shape_z[0], min_area) {
                encode_tile_shape(
                    &shape_z,
                    cell.tx,
                    cell.ty,
                    &mut scratch.int,
                    &mut |tx, ty, geom| {
                        sink(cell.z, tx, ty, geom, PyramidEmitKind::Fragment);
                    },
                );
            }
            scratch.int.recycle_owned_shape(shape_z);
            continue;
        }

        let mut normalized = scratch.take_shapes();
        normalize_into(&mut scratch.int, shape_z, min_area, &mut normalized);
        // Each normalized shape encodes as one polygon geometry, which is one
        // classified polygon in MapLibre - so the ring cap binds here and
        // nowhere downstream. Shapes at or under the cap (all but a handful
        // worldwide) take the existing path untouched.
        if normalized.iter().any(|s| s.len() > MAX_FEATURE_RINGS) {
            let mut capped = scratch.take_shapes();
            enforce_ring_cap(
                scratch,
                &mut normalized,
                buffered_tile_rect(cell.tx, cell.ty),
                &mut capped,
            );
            for tile_shape in &capped {
                encode_tile_shape(
                    tile_shape,
                    cell.tx,
                    cell.ty,
                    &mut scratch.int,
                    &mut |tx, ty, geom| {
                        sink(cell.z, tx, ty, geom, PyramidEmitKind::Fragment);
                    },
                );
            }
            scratch.return_shapes(capped);
        } else {
            for tile_shape in &normalized {
                encode_tile_shape(
                    tile_shape,
                    cell.tx,
                    cell.ty,
                    &mut scratch.int,
                    &mut |tx, ty, geom| {
                        sink(cell.z, tx, ty, geom, PyramidEmitKind::Fragment);
                    },
                );
            }
        }
        scratch.return_shapes(normalized);
    }
}

/// Move every shape of `normalized` into `out`, partitioning the ones over
/// [`MAX_FEATURE_RINGS`] so no emitted shape exceeds the cap. `rect` is the
/// shapes' clip rect in the same global zoom-z coordinates they are held in.
/// Returns true when at least one shape was partitioned.
fn enforce_ring_cap(
    scratch: &mut PyramidScratch,
    normalized: &mut Shapes,
    rect: IntRect,
    out: &mut Shapes,
) -> bool {
    let mut partitioned = false;
    for shape in normalized.drain(..) {
        if shape.len() > MAX_FEATURE_RINGS {
            partitioned = true;
            let before = out.len();
            partition_shape_to_ring_cap(scratch, shape, rect, out);
            RING_CAP.partitions.fetch_add(1, Ordering::Relaxed);
            RING_CAP.pieces.fetch_add(
                u64::try_from(out.len() - before).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        } else {
            out.push(shape);
        }
    }
    partitioned
}

/// Split one over-cap shape into pieces that each carry at most
/// [`MAX_FEATURE_RINGS`] contours, by recursive bisection of its clip rect.
///
/// A shape here is one outer plus N holes, which MapLibre groups into a
/// single classified polygon and then clamps to the 500 largest rings - so a
/// 510-hole ocean tile silently loses its smallest islands in every
/// MapLibre-semantics consumer. Only geometric partition fixes that: no
/// reordering or re-nesting keeps all the rings in one polygon.
///
/// The cut halves are `[min, mid]` and `[mid, max]` on the longer axis (ties
/// split x), both CLOSED at the shared integer coordinate `mid`, so the two
/// sides carry bit-identical cut vertices and abut seamlessly under nonzero
/// fill. Clipping runs at `min_area = 0`: island survival was already decided
/// by `normalize_into` upstream, and re-deciding it here would drop geometry
/// the partition is supposed to preserve. Pieces land in `out` in
/// depth-first, low-half-first order, which keeps two runs byte-identical.
fn partition_shape_to_ring_cap(
    scratch: &mut PyramidScratch,
    shape: Shape,
    rect: IntRect,
    out: &mut Shapes,
) {
    // Unreachable: more than MAX_FEATURE_RINGS disjoint positive-area
    // contours cannot inhabit a rect with no room to split. The error path
    // exists because "unreachable" is an argument, not a proof, and emitting
    // an over-cap shape anyway is precisely the silent-drop outcome this
    // partition exists to end.
    let (low, high) = split_rect(rect).expect(
        "a shape over the MapLibre ring cap cannot inhabit an indivisible rect; \
         emitting it would silently drop its smallest rings in MapLibre",
    );
    for half in [low, high] {
        let mut pieces = scratch.take_shapes();
        intersect_rect_into(&mut scratch.int, &shape, half, 0, &mut pieces);
        for piece in pieces.drain(..) {
            if piece.len() > MAX_FEATURE_RINGS {
                partition_shape_to_ring_cap(scratch, piece, half, out);
            } else {
                out.push(piece);
            }
        }
        scratch.return_shapes(pieces);
    }
    scratch.int.recycle_owned_shape(shape);
}

/// Halve a rect on its longer axis (tie: x), sharing the cut coordinate.
/// None when the chosen axis has no room left to cut - which, the axis being
/// the longer one, means neither axis does.
fn split_rect(rect: IntRect) -> Option<(IntRect, IntRect)> {
    let width = i64::from(rect.max_x) - i64::from(rect.min_x);
    let height = i64::from(rect.max_y) - i64::from(rect.min_y);
    if width >= height {
        let mid = rect.min_x + i32::try_from(width / 2).expect("rect half-width fits i32");
        (mid > rect.min_x).then_some((
            IntRect { max_x: mid, ..rect },
            IntRect { min_x: mid, ..rect },
        ))
    } else {
        let mid = rect.min_y + i32::try_from(height / 2).expect("rect half-height fits i32");
        (mid > rect.min_y).then_some((
            IntRect { max_y: mid, ..rect },
            IntRect { min_y: mid, ..rect },
        ))
    }
}

fn emit_full_subtree(
    cell: PyramidCell,
    params: &PyramidParams<'_>,
    scratch: &mut PyramidScratch,
    sink: PyramidSink<'_>,
) {
    if !cell_is_owned(cell, params) {
        return;
    }
    emit_full_tile(
        cell.tx,
        cell.ty,
        &mut scratch.int,
        &mut |out_tx, out_ty, geom| {
            sink(cell.z, out_tx, out_ty, geom, PyramidEmitKind::FullFill);
        },
    );
    if cell.z < params.z_bottom {
        for child in children(cell) {
            emit_full_subtree(child, params, scratch, sink);
        }
    }
}

fn cell_is_owned(cell: PyramidCell, params: &PyramidParams<'_>) -> bool {
    match params.tile_filter {
        Some(filter) => filter(cell.z, cell.tx, cell.ty),
        None => true,
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

/// Cut a fragment set against an axis-aligned rect (Spec 4 Landing 3,
/// pulled forward after profiling showed the general boolean dominating the
/// descent). Three tiers per shape:
/// - bbox strictly outside the rect: skip (a boundary touch yields only
///   zero-area geometry, which every downstream consumer drops);
/// - bbox inside the rect: clone, the cut is an identity;
/// - otherwise an exact integer Sutherland-Hodgman half-plane chain with
///   snap-rounded crossings, guarded per pass: a ring crossing the cut
///   line more than twice (would bridge into multiple components) or
///   carrying a vertex exactly on the line (in/out ambiguity) falls back
///   to the full i_overlay boolean - the R23-class safety net.
fn intersect_shapes_with_rect(
    scratch: &mut IntEmitScratch,
    shapes: &Shapes,
    rect: IntRect,
    _min_area: u64,
    out: &mut Shapes,
) {
    scratch.recycle_shapes(out);
    let mut clipped = Vec::new();
    for shape in shapes {
        let Some(bb) = shape_bbox(shape) else {
            continue;
        };
        if bb.min_x > rect.max_x
            || bb.max_x < rect.min_x
            || bb.min_y > rect.max_y
            || bb.max_y < rect.min_y
        {
            continue;
        }
        if bb.min_x >= rect.min_x
            && bb.max_x <= rect.max_x
            && bb.min_y >= rect.min_y
            && bb.max_y <= rect.max_y
        {
            // Identity tier: the cut is a no-op, so the output is a verbatim
            // copy - built from pooled rings instead of a fresh clone (this
            // is the most-hit tier in the descent, one copy per contained
            // shape per cell).
            let mut copy = scratch.take_shape(shape.len());
            copy_shape_into(shape, &mut copy);
            out.push(copy);
            continue;
        }
        if clip_shape_rect_fast(shape, rect, out) {
            continue;
        }
        intersect_rect_into(scratch, shape, rect, 0, &mut clipped);
        out.append(&mut clipped);
    }
}

#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
}

/// Fast rect clip of one shape via four half-plane passes with full
/// multi-crossing reconnection. Returns true when handled (result
/// components, possibly none, appended to `out`); false means the caller
/// must run the exact boolean instead. Holes are clipped independently
/// and re-nested by point-in-contour when the outer splits into multiple
/// components. Hole runs coincident with the outer along a cut line are
/// legal here - emission-time normalization resolves tangencies, exactly
/// as it does for the boolean's output.
fn clip_shape_rect_fast(shape: &Shape, rect: IntRect, out: &mut Shapes) -> bool {
    let passes: [(Axis, i32, bool); 4] = [
        (Axis::X, rect.min_x, false),
        (Axis::X, rect.max_x, true),
        (Axis::Y, rect.min_y, false),
        (Axis::Y, rect.max_y, true),
    ];

    let mut outers: Vec<Contour> = vec![shape[0].clone()];
    for &(axis, bound, keep_le) in &passes {
        let mut next = Vec::with_capacity(outers.len());
        for ring in &outers {
            let Some(parts) = clip_ring_half_plane_multi(ring, axis, bound, keep_le) else {
                return false;
            };
            next.extend(parts);
        }
        outers = next;
        if outers.is_empty() {
            // Outer vanished: holes are subsets of it, the whole shape's
            // intersection is empty. Handled.
            return true;
        }
    }
    outers.retain(|r| r.len() >= 3 && !contour_area_is_below(r, 1));
    if outers.is_empty() {
        return true;
    }

    let mut holes: Vec<Contour> = Vec::new();
    for hole in &shape[1..] {
        let mut parts = vec![hole.clone()];
        for &(axis, bound, keep_le) in &passes {
            let mut next = Vec::with_capacity(parts.len());
            for ring in &parts {
                let Some(sub) = clip_ring_half_plane_multi(ring, axis, bound, keep_le) else {
                    return false;
                };
                next.extend(sub);
            }
            parts = next;
            if parts.is_empty() {
                break;
            }
        }
        holes.extend(
            parts
                .into_iter()
                .filter(|r| r.len() >= 3 && !contour_area_is_below(r, 1)),
        );
    }

    // Enforce role winding (reconnection preserves geometry, not
    // necessarily traversal direction).
    for o in &mut outers {
        if signed_area_2x(o) < 0 {
            o.reverse();
        }
    }
    for h in &mut holes {
        if signed_area_2x(h) > 0 {
            h.reverse();
        }
    }

    if outers.len() == 1 {
        let mut component = Vec::with_capacity(1 + holes.len());
        component.extend(outers);
        component.extend(holes);
        out.push(component);
        return true;
    }

    let mut components: Vec<Shape> = outers.into_iter().map(|o| vec![o]).collect();
    'holes: for hole in holes {
        let probe = hole[0];
        for component in &mut components {
            if point_in_contour(probe.x, probe.y, &component[0]) {
                component.push(hole);
                continue 'holes;
            }
        }
        // A hole not inside any outer: rounding put its probe vertex on
        // or outside every boundary - ambiguous, let the boolean decide.
        return false;
    }
    out.extend(components);
    true
}

/// Half-plane clip of one simple ring with multi-crossing reconnection:
/// kept chains reconnect along the cut line by pairing SORTED crossings -
/// for a simple ring, polygon-interior intervals along the line lie
/// exactly between alternating sorted crossings (Jordan), so adjacent
/// pairs are the bridge segments. Vertices exactly ON the line classify
/// as inside (the crossing then falls exactly on the vertex, which the
/// exact rational crossing reproduces verbatim); a tangency touching the
/// line from outside produces two same-position crossings and is caught
/// by the tie guard. Returns None when the fast path cannot proceed
/// safely: an odd crossing count, two crossings whose snap-rounded line
/// positions collide (pairing ambiguity), or a bridge that fails to join
/// an exit to an entry (non-simple input).
#[allow(clippy::too_many_lines)]
fn clip_ring_half_plane_multi(
    ring: &Contour,
    axis: Axis,
    bound: i32,
    keep_le: bool,
) -> Option<Vec<Contour>> {
    let n = ring.len();
    if n < 3 {
        return Some(Vec::new());
    }
    let coord = |p: IntPoint| match axis {
        Axis::X => p.x,
        Axis::Y => p.y,
    };
    let along = |p: IntPoint| match axis {
        Axis::X => p.y,
        Axis::Y => p.x,
    };
    let inside = |c: i32| if keep_le { c <= bound } else { c >= bound };

    let mut any_in = false;
    let mut any_out = false;
    let mut first_out = None;
    for (i, &p) in ring.iter().enumerate() {
        let c = coord(p);
        if inside(c) {
            any_in = true;
        } else {
            any_out = true;
            if first_out.is_none() {
                first_out = Some(i);
            }
        }
    }
    if !any_out {
        return Some(vec![ring.clone()]);
    }
    if !any_in {
        return Some(Vec::new());
    }
    let start = first_out?;

    // Walk edges from an OUTSIDE vertex so every kept chain is contiguous:
    // entry crossing, kept vertices, exit crossing.
    let mut chains: Vec<Contour> = Vec::new();
    let mut current: Option<Contour> = None;
    for k in 0..n {
        let a = ring[(start + k) % n];
        let b = ring[(start + k + 1) % n];
        let a_in = inside(coord(a));
        let b_in = inside(coord(b));
        if a_in
            && let Some(chain) = current.as_mut()
            && chain.last().copied() != Some(a)
        {
            chain.push(a);
        }
        if a_in != b_in {
            let c = crossing_point(a, b, axis, bound);
            if a_in {
                let mut chain = current.take()?;
                if chain.last().copied() != Some(c) {
                    chain.push(c);
                }
                chains.push(chain);
            } else {
                current = Some(vec![c]);
            }
        }
    }
    if current.is_some() {
        return None;
    }
    if chains.is_empty() {
        return Some(Vec::new());
    }
    // Degenerate chains (entry == exit after rounding) poison pairing.
    if chains.iter().any(|ch| ch.len() < 2) {
        return None;
    }

    // Endpoint list: (position along line, chain index, is_entry).
    let mut endpoints: Vec<(i32, usize, bool)> = Vec::with_capacity(chains.len() * 2);
    for (idx, chain) in chains.iter().enumerate() {
        endpoints.push((along(chain[0]), idx, true));
        endpoints.push((along(chain[chain.len() - 1]), idx, false));
    }
    endpoints.sort_unstable_by_key(|&(pos, _, _)| pos);
    if endpoints.windows(2).any(|w| w[0].0 == w[1].0) {
        return None;
    }

    // Adjacent sorted pairs are bridges; each must join an exit to an
    // entry. entry_partner[chain] = chain whose entry the bridge from
    // this chain's exit reaches.
    let mut exit_to_entry = vec![usize::MAX; chains.len()];
    let (pairs, _) = endpoints.as_chunks::<2>();
    for pair in pairs {
        let (_, i0, entry0) = pair[0];
        let (_, i1, entry1) = pair[1];
        match (entry0, entry1) {
            (true, false) => exit_to_entry[i1] = i0,
            (false, true) => exit_to_entry[i0] = i1,
            _ => return None,
        }
    }
    if exit_to_entry.contains(&usize::MAX) {
        return None;
    }

    let mut visited = vec![false; chains.len()];
    let mut rings_out = Vec::new();
    for start_chain in 0..chains.len() {
        if visited[start_chain] {
            continue;
        }
        let mut ring_out: Contour = Vec::new();
        let mut c = start_chain;
        loop {
            visited[c] = true;
            for &p in &chains[c] {
                if ring_out.last().copied() != Some(p) {
                    ring_out.push(p);
                }
            }
            c = exit_to_entry[c];
            if c == start_chain {
                break;
            }
            if visited[c] {
                return None;
            }
        }
        if ring_out.len() >= 2 && ring_out.first() == ring_out.last() {
            ring_out.pop();
        }
        if ring_out.len() >= 3 {
            rings_out.push(ring_out);
        }
    }
    Some(rings_out)
}

/// Exact rational crossing of segment (a, b) with an axis line, rounded to
/// nearest (ties away from zero). The cut line is a grid coordinate, so
/// the un-rounded crossing is exact.
fn crossing_point(a: IntPoint, b: IntPoint, axis: Axis, bound: i32) -> IntPoint {
    match axis {
        Axis::X => {
            let num = i64::from(b.y - a.y) * i64::from(bound - a.x);
            let den = i64::from(b.x - a.x);
            let y = i64::from(a.y) + round_div(num, den);
            IntPoint::new(bound, i32::try_from(y).expect("clip crossing fits i32"))
        }
        Axis::Y => {
            let num = i64::from(b.x - a.x) * i64::from(bound - a.y);
            let den = i64::from(b.y - a.y);
            let x = i64::from(a.x) + round_div(num, den);
            IntPoint::new(i32::try_from(x).expect("clip crossing fits i32"), bound)
        }
    }
}

fn round_div(num: i64, den: i64) -> i64 {
    let (n, d) = if den < 0 { (-num, -den) } else { (num, den) };
    if n >= 0 {
        (n + d / 2) / d
    } else {
        -((-n + d / 2) / d)
    }
}

fn shapes_vertices(shapes: &Shapes) -> usize {
    shapes
        .iter()
        .map(|shape| shape.iter().map(Vec::len).sum::<usize>())
        .sum()
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

/// Second simplification pass over a zoom-scale shape: tolerance 2, with
/// only the ENDPOINTS of maximal in-window vertex runs pinned (window =
/// within the 128-unit buffer of the cell's tile edge lines, in zoom
/// coordinates). Run endpoints include the cut vertices on clip lines, so
/// fragment bounds survive; run interiors thin to 2-unit fidelity.
fn thin_window_runs(
    shape_z: &mut Shape,
    cell: PyramidCell,
    flags_buf: &mut Vec<Vec<bool>>,
    int: &mut IntEmitScratch,
) {
    let left = i64::from(cell.tx) * i64::from(TILE_EXTENT_I32);
    let right = (i64::from(cell.tx) + 1) * i64::from(TILE_EXTENT_I32);
    let top = i64::from(cell.ty) * i64::from(TILE_EXTENT_I32);
    let bottom = (i64::from(cell.ty) + 1) * i64::from(TILE_EXTENT_I32);
    let buffer = i64::from(TILE_BUFFER_I32);

    flags_buf.clear();
    let mut any_window = false;
    for ring in shape_z.iter() {
        let n = ring.len();
        let in_window = |p: IntPoint| {
            near_line(i64::from(p.x), left, buffer)
                || near_line(i64::from(p.x), right, buffer)
                || near_line(i64::from(p.y), top, buffer)
                || near_line(i64::from(p.y), bottom, buffer)
        };
        let mut flags = vec![false; n];
        for i in 0..n {
            if !in_window(ring[i]) {
                continue;
            }
            any_window = true;
            let prev = ring[if i == 0 { n - 1 } else { i - 1 }];
            let next = ring[if i + 1 < n { i + 1 } else { 0 }];
            if !in_window(prev) || !in_window(next) {
                flags[i] = true;
            }
        }
        flags_buf.push(flags);
    }
    if any_window {
        simplify_shape_dp(int, shape_z, 2, Some(flags_buf));
    }
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

/// True only for a genuinely convex, simple ring - a sound gate for
/// skipping normalization at emission.
///
/// Two conditions, both necessary, together sufficient:
/// 1. No reflex vertex: every consecutive-triple cross product shares one
///    nonzero sign (collinear triples skipped).
/// 2. Exactly one revolution: the edge-direction vector's x-component
///    changes sign exactly twice around the ring, and its y-component
///    exactly twice.
///
/// Condition 1 ALONE is unsound - a self-intersecting spiral that laps
/// more than once turns the same way at every vertex yet is non-simple,
/// and earcut mis-tessellates it (the land-layer defect this replaces).
/// Condition 2 is the discrete "winds exactly once": a convex polygon
/// splits into two x-monotone chains at its leftmost/rightmost vertices
/// (two x-flips) and two y-monotone chains (two y-flips); a lapping ring
/// flips four or more times.
fn is_convex_ring(ring: &Contour) -> bool {
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut sign = 0_i8;
    for i in 0..n {
        let a = ring[i];
        let b = ring[(i + 1) % n];
        let c = ring[(i + 2) % n];
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
    if sign == 0 {
        return false;
    }
    direction_flips(ring, Axis::X) == 2 && direction_flips(ring, Axis::Y) == 2
}

/// Count sign changes of one edge-vector component around the cyclic ring,
/// skipping zero components (vertical edges for X, horizontal for Y).
fn direction_flips(ring: &Contour, axis: Axis) -> u32 {
    let comp = |a: IntPoint, b: IntPoint| -> i32 {
        let d = match axis {
            Axis::X => i64::from(b.x) - i64::from(a.x),
            Axis::Y => i64::from(b.y) - i64::from(a.y),
        };
        d.signum() as i32
    };
    let n = ring.len();
    let mut signs: Vec<i32> = Vec::with_capacity(n);
    for i in 0..n {
        let s = comp(ring[i], ring[(i + 1) % n]);
        if s != 0 {
            signs.push(s);
        }
    }
    let m = signs.len();
    if m < 2 {
        return 0;
    }
    let mut flips = 0;
    for i in 0..m {
        if signs[i] != signs[(i + 1) % m] {
            flips += 1;
        }
    }
    flips
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
    use i_overlay::i_float::int::point::IntPoint as OraclePoint;
    use std::collections::BTreeMap;

    // The in-tree engine's IntPoint and the dev-dep i_overlay XOR oracle's
    // IntPoint are now distinct types; convert at the boundary.
    fn to_oracle_shape(shape: &Shape) -> Vec<Vec<OraclePoint>> {
        shape
            .iter()
            .map(|c| c.iter().map(|q| OraclePoint::new(q.x, q.y)).collect())
            .collect()
    }

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
            tile_filter: None,
            simplifier: Simplifier::DouglasPeucker,
        }
    }

    fn collect_pyramid(
        shape: &Shape,
        params: &PyramidParams<'_>,
    ) -> BTreeMap<(u8, u32, u32), Vec<u32>> {
        let mut scratch = PyramidScratch::new();
        let mut out = BTreeMap::new();
        emit_shape_pyramid(
            shape,
            params,
            &mut scratch,
            &mut |z, tx, ty, geom, _kind| {
                out.entry((z, tx, ty))
                    .or_insert_with(Vec::new)
                    .extend_from_slice(geom);
            },
        );
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
                                    &tile_shape,
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

    fn decoded_cycles(bufs: &[Vec<u32>]) -> Vec<Vec<(i32, i32)>> {
        let mut out = Vec::new();
        for b in bufs {
            for ring in decode_mvt_polygon(b) {
                let mut v = ring;
                v.pop();
                let n = v.len();
                let m = (0..n).min_by_key(|&i| v[i]).unwrap_or(0);
                let mut rot: Vec<(i32, i32)> = (0..n).map(|i| v[(m + i) % n]).collect();
                let mut rev = rot.clone();
                rev.reverse();
                let n = rev.len();
                let m = (0..n).min_by_key(|&i| rev[i]).unwrap_or(0);
                let rev: Vec<(i32, i32)> = (0..n).map(|i| rev[(m + i) % n]).collect();
                if rev < rot {
                    rot = rev;
                }
                out.push(rot);
            }
        }
        out.sort();
        out
    }

    #[test]
    fn convexity_early_out_vs_normalize_equivalence() {
        let quads = [
            shape(&[(100, 100), (900, 140), (870, 800), (120, 760)], &[]),
            shape(&[(40, 300), (500, 60), (980, 360), (680, 900)], &[]),
            shape(&[(200, 100), (850, 100), (980, 700), (260, 920)], &[]),
            shape(&[(80, 80), (960, 180), (820, 960), (140, 840)], &[]),
        ];
        let mut scratch = IntEmitScratch::new();
        for quad in quads {
            assert!(is_convex_single_ring(&quad));
            let mut fast = Vec::new();
            encode_tile_shape(&quad, 0, 0, &mut scratch, &mut |_, _, geom| {
                fast.push(geom.to_vec());
            });

            let mut normalized = Vec::new();
            normalize_into(&mut scratch, quad, 0, &mut normalized);
            let mut normal = Vec::new();
            for shape in &normalized {
                encode_tile_shape(shape, 0, 0, &mut scratch, &mut |_, _, geom| {
                    normal.push(geom.to_vec());
                });
            }
            assert_eq!(decoded_cycles(&fast), decoded_cycles(&normal));
        }
    }

    #[test]
    fn cut_identity_dp_tol_0_geometry_equivalence_reference() {
        // Landing 1 asserted byte identity against the boolean reference;
        // the Landing 3 splitter keeps the same geometry but not the same
        // bytes (snap-rounded crossings, retained collinear cut-line
        // vertices, verbatim identity-tier clones). The cut identity is
        // therefore pinned as per-tile geometric equivalence: same tile
        // set, XOR-empty decoded geometry per tile.
        let outer = [(900, 900), (13500, 900), (13500, 13200), (900, 13200)];
        let inner = [(5200, 5200), (5200, 7200), (7200, 7200), (7200, 5200)];
        let shape = shape(&outer, &[&inner]);
        let params = params(2, 0, 2, 0);
        let got = collect_pyramid(&shape, &params);
        let expected = collect_direct(&shape, &params);
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            expected.keys().collect::<Vec<_>>(),
            "tile sets differ"
        );
        let mut scratch = IntEmitScratch::new();
        for (key, got_geom) in &got {
            let window = IntRect {
                min_x: i32::MIN / 4,
                min_y: i32::MIN / 4,
                max_x: i32::MAX / 4,
                max_y: i32::MAX / 4,
            };
            let one_got: BTreeMap<(u8, u32, u32), Vec<u32>> =
                BTreeMap::from([(*key, got_geom.clone())]);
            let one_exp: BTreeMap<(u8, u32, u32), Vec<u32>> =
                BTreeMap::from([(*key, expected[key].clone())]);
            let got_shapes = commands_to_window_shapes(&one_got, window, &mut scratch);
            let exp_shapes = commands_to_window_shapes(&one_exp, window, &mut scratch);
            assert!(
                xor_shapes_empty(&got_shapes, &exp_shapes),
                "geometry diverges at {key:?}"
            );
        }
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
            &mut |z, tx, ty, geom, _kind| {
                left.insert((z, tx, ty), geom.to_vec());
            },
        );
        emit_shape_pyramid_cell(
            PyramidCell { z: 1, tx: 1, ty: 0 },
            right_frag,
            &params,
            &mut ps,
            &mut |z, tx, ty, geom, _kind| {
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
        // With Brick 8's window thinning, cross-side seam drift is bounded
        // by the 2-unit strip tolerance instead of zero: budget the XOR at
        // 2 units times the window height per side.
        let budget = 2 * 2 * u128::try_from(window.max_y - window.min_y).expect("window height");
        let xor_area = xor_shapes_area(&left_shapes, &right_shapes);
        assert!(
            xor_area <= budget,
            "seam window drift {xor_area} exceeds budget {budget}"
        );
    }

    #[test]
    fn seam_window_vw_xor_empty_for_shared_window() {
        // Clone of the DP seam test with the Visvalingam simplifier: the
        // edge-window pins are never removed under either simplifier, so the
        // shared seam vertices are identical on both sides and the tol-2
        // window thinning applies equally. Cross-side drift stays inside the
        // same 2-unit-per-side budget.
        let mut params = params(1, 1, 1, 16);
        params.simplifier = Simplifier::Visvalingam;
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
            &mut |z, tx, ty, geom, _kind| {
                left.insert((z, tx, ty), geom.to_vec());
            },
        );
        emit_shape_pyramid_cell(
            PyramidCell { z: 1, tx: 1, ty: 0 },
            right_frag,
            &params,
            &mut ps,
            &mut |z, tx, ty, geom, _kind| {
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
        let budget = 2 * 2 * u128::try_from(window.max_y - window.min_y).expect("window height");
        let xor_area = xor_shapes_area(&left_shapes, &right_shapes);
        assert!(
            xor_area <= budget,
            "seam window drift {xor_area} exceeds budget {budget}"
        );
    }

    #[test]
    fn is_convex_ring_rejects_all_same_turn_spiral() {
        // A self-intersecting ring that turns left at every vertex yet
        // laps more than once - the class that slipped through the old
        // cross-product-only test and shipped non-simple land polygons
        // past normalization. All consecutive turns are left (positive
        // cross), so the reflex check alone accepts it; the revolution
        // check must reject it.
        let spiral: Contour = vec![
            p(0, 0),
            p(100, 10),
            p(90, 110),
            p(-10, 100),
            p(-20, -20),
            p(130, -30),
            p(140, 140),
        ];
        // Confirm every turn is the same direction (the old test's basis).
        let n = spiral.len();
        let mut sign = 0_i8;
        let mut all_same = true;
        for i in 0..n {
            let a = spiral[i];
            let b = spiral[(i + 1) % n];
            let c = spiral[(i + 2) % n];
            let cross = (i128::from(b.x) - i128::from(a.x)) * (i128::from(c.y) - i128::from(a.y))
                - (i128::from(b.y) - i128::from(a.y)) * (i128::from(c.x) - i128::from(a.x));
            if cross == 0 {
                continue;
            }
            let s = if cross > 0 { 1 } else { -1 };
            if sign == 0 {
                sign = s;
            } else if sign != s {
                all_same = false;
            }
        }
        assert!(
            all_same,
            "test fixture must be all-same-turn to be meaningful"
        );
        // The sound test rejects it (more than one revolution).
        assert!(!is_convex_ring(&spiral), "spiral must not read as convex");
        // A genuine convex ring still passes.
        let square: Contour = vec![p(0, 0), p(100, 0), p(100, 100), p(0, 100)];
        assert!(is_convex_ring(&square), "square must read as convex");
        let pentagon: Contour = vec![p(50, 0), p(100, 40), p(80, 100), p(20, 100), p(0, 40)];
        assert!(
            is_convex_ring(&pentagon),
            "convex pentagon must read as convex"
        );
    }

    #[test]
    fn fast_rect_clip_equivalent_to_boolean() {
        // Jagged outer with a hole, clipped against a grid of rects that
        // exercise identity, disjoint, simple-crossing, multi-crossing
        // (fallback), and on-line-vertex (fallback) tiers. The fast path
        // plus fallback must be set-equivalent to the boolean (XOR empty).
        let subject = shape(
            &[
                (100, 100),
                (2100, 140),
                (2500, 900),
                (1900, 1300),
                (2600, 1700),
                (2000, 2500),
                (900, 2100),
                (300, 2600),
                (150, 1500),
                (700, 800),
            ],
            &[&[(1000, 1000), (1000, 1600), (1600, 1600), (1600, 1000)]],
        );
        let mut scratch = IntEmitScratch::new();
        let rects = [
            IntRect {
                min_x: 0,
                min_y: 0,
                max_x: 3000,
                max_y: 3000,
            }, // identity
            IntRect {
                min_x: 5000,
                min_y: 5000,
                max_x: 6000,
                max_y: 6000,
            }, // disjoint
            IntRect {
                min_x: 0,
                min_y: 0,
                max_x: 1200,
                max_y: 3000,
            }, // simple crossing
            IntRect {
                min_x: 800,
                min_y: 700,
                max_x: 2200,
                max_y: 1900,
            }, // cuts hole too
            IntRect {
                min_x: 0,
                min_y: 1400,
                max_x: 3000,
                max_y: 1500,
            }, // thin band, multi-crossing
            IntRect {
                min_x: 100,
                min_y: 0,
                max_x: 2500,
                max_y: 2600,
            }, // on-line vertices
        ];
        // Comb polygon: crosses a horizontal cut line many times, forcing
        // the multi-crossing reconnection to produce multiple components.
        let comb = shape(
            &[
                (101, 101),
                (2901, 101),
                (2901, 2201),
                (2501, 2201),
                (2501, 601),
                (2101, 601),
                (2101, 2201),
                (1701, 2201),
                (1701, 601),
                (1301, 601),
                (1301, 2201),
                (901, 2201),
                (901, 601),
                (501, 601),
                (501, 2201),
                (101, 2201),
            ],
            &[],
        );
        for subject in [&subject, &comb] {
            for rect in rects {
                let mut fast = Vec::new();
                intersect_shapes_with_rect(
                    &mut scratch,
                    &vec![subject.clone()],
                    rect,
                    0,
                    &mut fast,
                );
                let exact = intersect_rect(subject, rect, 0);
                // The fast path's snap-rounded crossings can differ from
                // i_overlay's noding by up to one unit ALONG the cut line,
                // so equivalence is up to slivers hugging the rect
                // boundary: total XOR area bounded by one unit times the
                // cut perimeter. Interior geometry is verbatim, so any
                // real divergence blows straight past this budget.
                let perimeter = 2
                    * (u128::try_from(i64::from(rect.max_x) - i64::from(rect.min_x))
                        .expect("rect width positive")
                        + u128::try_from(i64::from(rect.max_y) - i64::from(rect.min_y))
                            .expect("rect height positive"));
                let xor_area = xor_shapes_area(&fast, &exact);
                assert!(
                    xor_area <= perimeter,
                    "fast clip diverges from boolean for {rect:?}: xor area {xor_area} > budget {perimeter}"
                );
            }
        }
    }

    /// One tile-sized outer with `holes` square holes on a grid, normalized
    /// exactly as emission would hand it to the ring cap. Confined to tile
    /// (0, 0) at the test zoom per the single-tile test convention.
    fn holed_tile_shape(holes: usize) -> Shape {
        let outer = [(0, 0), (4096, 0), (4096, 4096), (0, 4096)];
        let mut rings: Vec<Vec<(i32, i32)>> = Vec::with_capacity(holes);
        for k in 0..holes {
            let col = i32::try_from(k % 25).expect("column fits i32");
            let row = i32::try_from(k / 25).expect("row fits i32");
            let x = 100 + col * 160;
            let y = 100 + row * 160;
            rings.push(vec![(x, y), (x, y + 40), (x + 40, y + 40), (x + 40, y)]);
        }
        let refs: Vec<&[(i32, i32)]> = rings.iter().map(Vec::as_slice).collect();
        let raw = shape(&outer, &refs);

        let mut int = IntEmitScratch::new();
        let mut normalized = Vec::new();
        normalize_into(&mut int, raw, 0, &mut normalized);
        assert_eq!(normalized.len(), 1, "fixture must normalize to one shape");
        assert_eq!(
            normalized[0].len(),
            holes + 1,
            "fixture must keep every hole"
        );
        normalized.remove(0)
    }

    fn cap_pieces(input: Shape, rect: IntRect) -> (Shapes, bool) {
        let mut scratch = PyramidScratch::new();
        let mut normalized: Shapes = vec![input];
        let mut out = Vec::new();
        let partitioned = enforce_ring_cap(&mut scratch, &mut normalized, rect, &mut out);
        (out, partitioned)
    }

    #[test]
    fn ring_cap_partition_bounds_every_piece() {
        let original = holed_tile_shape(600);
        let (pieces, partitioned) = cap_pieces(original.clone(), buffered_tile_rect(0, 0));
        assert!(partitioned, "601-contour shape must partition");
        assert!(pieces.len() >= 2, "partition must produce several pieces");
        for piece in &pieces {
            assert!(
                piece.len() <= MAX_FEATURE_RINGS,
                "piece carries {} contours, over the {MAX_FEATURE_RINGS} cap",
                piece.len()
            );
        }
    }

    #[test]
    fn ring_cap_partition_preserves_coverage() {
        // The referee is the i_overlay dev-dependency, not the in-tree engine
        // doing the partition: a fix that DELETED holes instead of splitting
        // the shape would clear the ring census just as well, and only an
        // independent XOR can tell the two apart.
        let original = holed_tile_shape(600);
        let (pieces, _) = cap_pieces(original.clone(), buffered_tile_rect(0, 0));
        let whole: Shapes = vec![original];
        assert_eq!(
            xor_shapes_area(&pieces, &whole),
            0,
            "partition changed the covered area"
        );
    }

    #[test]
    fn ring_cap_leaves_a_shape_at_the_cap_alone() {
        let at_cap = holed_tile_shape(MAX_FEATURE_RINGS - 1);
        assert_eq!(at_cap.len(), MAX_FEATURE_RINGS);
        let (pieces, partitioned) = cap_pieces(at_cap.clone(), buffered_tile_rect(0, 0));
        assert!(!partitioned, "a shape exactly at the cap must not split");
        assert_eq!(pieces, vec![at_cap], "shape must pass through verbatim");
    }

    #[test]
    fn ring_cap_partition_is_deterministic() {
        let original = holed_tile_shape(600);
        let (first, _) = cap_pieces(original.clone(), buffered_tile_rect(0, 0));
        let (second, _) = cap_pieces(original, buffered_tile_rect(0, 0));
        assert_eq!(first, second, "piece stream differs between runs");
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

    fn xor_shapes_area(a: &Shapes, b: &Shapes) -> u128 {
        let options = IntOverlayOptions {
            output_direction: i_overlay::core::overlay::ContourDirection::CounterClockwise,
            min_output_area: 0,
            ..Default::default()
        };
        let mut overlay = Overlay::new_custom(0, options, Default::default());
        for shape in a {
            overlay.add_source(&to_oracle_shape(shape), ShapeType::Subject);
        }
        for shape in b {
            overlay.add_source(&to_oracle_shape(shape), ShapeType::Clip);
        }
        let result = overlay.overlay(OverlayRule::Xor, FillRule::NonZero);
        result
            .iter()
            .flatten()
            .map(|ring| {
                let ours: Vec<IntPoint> = ring.iter().map(|q| IntPoint::new(q.x, q.y)).collect();
                signed_area_2x(&ours).unsigned_abs() / 2
            })
            .sum()
    }

    /// Set equality up to degenerate boundary artifacts: two equivalent
    /// decompositions with coincident edges can XOR to zero-AREA residue
    /// rings, which is_empty() would miscount as divergence.
    fn xor_shapes_empty(a: &Shapes, b: &Shapes) -> bool {
        xor_shapes_area(a, b) == 0
    }
}
