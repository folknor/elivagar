use crate::debug::RING_CAP;
use crate::geometry::int_ocean::{
    Contour, IntEmitScratch, IntPoint, IntRect, PinnedRings, Shape, Shapes, TILE_BUFFER_I32,
    TILE_EXTENT_I32, buffered_tile_rect, contour_area_is_below, copy_shape_into, emit_full_tile,
    encode_tile_shape, intersect_rect_into, normalize_into, point_in_contour,
    rescale_shape_pinned_into, shape_bbox, signed_area_2x, simplify_shape_dp, simplify_shape_vw,
    vw_area_threshold,
};
use crate::geometry::polygon::{self, Area, ExtentUnits, RingOrientation, WINDOW_DP_TOL};
use crate::mvt::MAX_FEATURE_RINGS;
use rustc_hash::FxHashSet;
use std::sync::atomic::Ordering;

// The ocean producer's output from this engine is cached world-wide in the
// durable ocean artifact (`ocean-build`, keyed by OCEAN_POLICY_VERSION in
// src/ocean/mod.rs). A change here that alters emitted bytes reaches artifact
// consumers only through a version bump - without it, existing artifacts
// keep key-validating and every artifact-active run serves the pre-change
// geometry while all standing gates stay green.
pub(crate) struct PyramidParams<'a> {
    pub maxz: u8,
    pub z_top: u8,
    pub z_bottom: u8,
    pub dp_tol: &'a (dyn Fn(u8) -> ExtentUnits + Sync),
    pub min_area: &'a (dyn Fn(u8) -> Area + Sync),
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
    edge_flags: PinnedRings,
    flags_z: PinnedRings,
}

impl PyramidScratch {
    pub(crate) fn new() -> Self {
        Self {
            int: IntEmitScratch::new(),
            frag_pool: Vec::new(),
            edge_flags: PinnedRings::default(),
            flags_z: PinnedRings::default(),
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
    normalize_into(&mut scratch.int, root_shape, Area::ZERO, &mut normalized);
    if normalized.is_empty() {
        scratch.return_shapes(normalized);
        return Vec::new();
    }

    let Some(bbox) = shapes_bbox(&normalized) else {
        scratch.return_shapes(normalized);
        return Vec::new();
    };
    let (tx_min, tx_max, ty_min, ty_max) = tile_cover(bbox, params.maxz, params.z_top, 0);
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
                Some(&mut scratch.flags_z),
            ),
            Simplifier::Visvalingam => simplify_shape_vw(
                &mut scratch.int,
                &mut shape_z,
                vw_area_threshold(dp_tol),
                Some(&mut scratch.flags_z),
            ),
        }
        if shape_z.is_empty() {
            scratch.int.recycle_owned_shape(shape_z);
            continue;
        }
        // Brick 8 (spec 3.3 contingency, tripped by norway +34-38% coastal
        // layer bytes): the hard window pins keep full-resolution coastline
        // in every edge strip. Thin the pinned window runs with a second DP
        // at WINDOW_DP_TOL - interior vertices already carry no sub-dp_tol
        // detail so they are unaffected; window runs drop to 2-unit
        // fidelity. Cross-side seam drift is now bounded by 2 zoom units
        // (sub-pixel at render) instead of zero; the seam-window test
        // budget matches. This runs for VW too because dp_tol controls this
        // supplemental pass, not the selected primary simplifier. The strict
        // gate stays dp_tol > WINDOW_DP_TOL; seam-deferral layers stay verbatim.
        if dp_tol > WINDOW_DP_TOL {
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

        if dp_tol.get() > 0 && is_convex_single_ring(&shape_z) {
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
        intersect_rect_into(&mut scratch.int, &shape, half, Area::ZERO, &mut pieces);
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
/// - otherwise a snap-rounded integer half-plane chain with multi-crossing
///   reconnection; ambiguous crossing pairs fall back to the in-tree
///   polygon boolean engine.
fn intersect_shapes_with_rect(
    scratch: &mut IntEmitScratch,
    shapes: &Shapes,
    rect: IntRect,
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
        intersect_rect_into(scratch, shape, rect, Area::ZERO, &mut clipped);
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
    outers.retain(|r| r.len() >= 3 && !contour_area_is_below(r, Area::ONE));
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
                .filter(|r| r.len() >= 3 && !contour_area_is_below(r, Area::ONE)),
        );
    }

    // Enforce role winding (reconnection preserves geometry, not
    // necessarily traversal direction).
    for o in &mut outers {
        if polygon::should_reverse(signed_area_2x(o), RingOrientation::Positive) {
            o.reverse();
        }
    }
    for h in &mut holes {
        if polygon::should_reverse(signed_area_2x(h), RingOrientation::Negative) {
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

/// Exact rational crossing, rounding the displacement from the endpoint
/// with the smaller clipped-axis coordinate to nearest, ties away from zero.
/// Canonical endpoint order makes shared edges bit-identical even when two
/// rings traverse them oppositely. It is independent of the kept half-plane,
/// so both halves of a partition also retain the same cut coordinate.
fn crossing_point(a: IntPoint, b: IntPoint, axis: Axis, bound: i32) -> IntPoint {
    let forward = match axis {
        Axis::X => a.x < b.x,
        Axis::Y => a.y < b.y,
    };
    let (a, b) = if forward { (a, b) } else { (b, a) };
    match axis {
        Axis::X => {
            let num = (i128::from(b.y) - i128::from(a.y)) * (i128::from(bound) - i128::from(a.x));
            let den = i128::from(b.x) - i128::from(a.x);
            let y = i128::from(a.y) + round_div(num, den);
            IntPoint::new(bound, i32::try_from(y).expect("clip crossing fits i32"))
        }
        Axis::Y => {
            let num = (i128::from(b.x) - i128::from(a.x)) * (i128::from(bound) - i128::from(a.y));
            let den = i128::from(b.y) - i128::from(a.y);
            let x = i128::from(a.x) + round_div(num, den);
            IntPoint::new(i32::try_from(x).expect("clip crossing fits i32"), bound)
        }
    }
}

fn round_div(num: i128, den: i128) -> i128 {
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

/// Inclusive world-clamped bbox cover in base coordinates. `margin` is in
/// zoom-space units: zero preserves root selection; TILE_BUFFER_I32 bounds
/// buffered descendant emission. Boundary-only contacts may be included, so
/// this is a conservative fanout bound, not a count of nonempty polygons.
pub(crate) fn tile_cover(rect: IntRect, maxz: u8, z: u8, margin: i32) -> (u32, u32, u32, u32) {
    assert!(z <= maxz && margin >= 0);
    let max_tile = (1_u32 << z) - 1;
    let tile_size = cell_tile_size_base(maxz, z);
    let margin = i64::from(margin) << u32::from(maxz - z);
    let index = |q: i64| {
        u32::try_from(q.div_euclid(tile_size).clamp(0, i64::from(max_tile)))
            .expect("clamped tile index fits u32")
    };
    (
        index(i64::from(rect.min_x) - margin),
        index(i64::from(rect.max_x) + margin),
        index(i64::from(rect.min_y) - margin),
        index(i64::from(rect.max_y) + margin),
    )
}

pub(crate) fn buffered_cell_rect_base(maxz: u8, cell: PyramidCell) -> IntRect {
    buffered_range_rect_base(
        maxz,
        cell.z,
        RootRange {
            tx0: cell.tx,
            tx1: cell.tx,
            ty0: cell.ty,
            ty1: cell.ty,
        },
    )
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

/// Second simplification pass over a zoom-scale shape: tolerance WINDOW_DP_TOL, with
/// only the ENDPOINTS of maximal in-window vertex runs pinned (window =
/// within the 128-unit buffer of the cell's tile edge lines, in zoom
/// coordinates). Run endpoints include the cut vertices on clip lines, so
/// fragment bounds survive; run interiors thin to WINDOW_DP_TOL fidelity.
/// This deliberately classifies the rounded coordinates, unlike the initial
/// base-space pins in `build_edge_flags`. With a two-bit downshift, x=513
/// rounds to 128: outside the base strip of width 512, inside this strip.
/// Sharing a post-rescale window would change first-pass pins, not just
/// remove duplicate arithmetic. Preserve this distinction unless changing
/// polygon output and rotating the durable ocean artifact is intended.
fn thin_window_runs(
    shape_z: &mut Shape,
    cell: PyramidCell,
    flags_buf: &mut PinnedRings,
    int: &mut IntEmitScratch,
) {
    let left = i64::from(cell.tx) * i64::from(TILE_EXTENT_I32);
    let right = (i64::from(cell.tx) + 1) * i64::from(TILE_EXTENT_I32);
    let top = i64::from(cell.ty) * i64::from(TILE_EXTENT_I32);
    let bottom = (i64::from(cell.ty) + 1) * i64::from(TILE_EXTENT_I32);
    let buffer = i64::from(TILE_BUFFER_I32);

    let mut any_window = false;
    flags_buf.fill_for_shape(shape_z, |_idx, ring, flags| {
        let n = ring.len();
        let in_window = |p: IntPoint| {
            near_line(i64::from(p.x), left, buffer)
                || near_line(i64::from(p.x), right, buffer)
                || near_line(i64::from(p.y), top, buffer)
                || near_line(i64::from(p.y), bottom, buffer)
        };
        flags.resize(n, false);
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
    });
    if any_window {
        simplify_shape_dp(int, shape_z, WINDOW_DP_TOL, Some(flags_buf));
    }
}

// Classify before rescale, retaining source junctions and segment crossings.
// Do not replace this with the rounded window in `thin_window_runs`: at
// maxz=2, z=0, tx=ty=0, (513, 8192) is outside every base edge strip but
// rounds to (128, 2048), inside the left zoom-space strip. Both points fit
// within that one tile. Rescale carries these flags forward (OR-ing flags
// on collapsed vertices); rounding must not silently introduce new pins.
fn build_edge_flags(
    shape: &Shape,
    params: &PyramidParams<'_>,
    cell: PyramidCell,
    out: &mut PinnedRings,
) {
    let tile_size = cell_tile_size_base(params.maxz, cell.z);
    let buffer = i64::from(TILE_BUFFER_I32) << u32::from(params.maxz - cell.z);
    let left = i64::from(cell.tx) * tile_size;
    let right = i64::from(cell.tx + 1) * tile_size;
    let top = i64::from(cell.ty) * tile_size;
    let bottom = i64::from(cell.ty + 1) * tile_size;
    out.fill_for_shape(shape, |_idx, ring, flags| {
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
    });
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
        let cross = polygon::triangle_area_2x((a.x, a.y), (b.x, b.y), (c.x, c.y)).get();
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
    use crate::geometry::int_ocean::{
        buffered_tile_rect, emit_full_tile, intersect_rect, normalize, rescale_shape,
    };
    use crate::geometry::projection::Point;
    use crate::mvt::MAX_FEATURE_RINGS;
    use crate::mvt::decode::polygon_rings;
    use crate::test_support::next_random;
    use i_overlay::core::fill_rule::FillRule;
    use i_overlay::core::overlay::{IntOverlayOptions, Overlay, ShapeType};
    use i_overlay::core::overlay_rule::OverlayRule;
    use i_overlay::i_float::int::point::IntPoint as OraclePoint;
    use std::collections::BTreeMap;

    #[test]
    fn crossings_ignore_edge_direction_on_both_axes() {
        for (a, b, bound) in [
            (IntPoint::new(100, 110), IntPoint::new(102, 109), 101),
            (IntPoint::new(100, 109), IntPoint::new(102, 110), 101),
            (IntPoint::new(-102, -110), IntPoint::new(-100, -109), -101),
            (
                IntPoint::new(i32::MIN, i32::MAX),
                IntPoint::new(i32::MAX, i32::MIN),
                0,
            ),
        ] {
            for axis in [Axis::X, Axis::Y] {
                let (a, b) = match axis {
                    Axis::X => (a, b),
                    Axis::Y => (IntPoint::new(a.y, a.x), IntPoint::new(b.y, b.x)),
                };
                for cut in [a, b] {
                    let bound = match axis {
                        Axis::X => cut.x,
                        Axis::Y => cut.y,
                    };
                    assert_eq!(crossing_point(a, b, axis, bound), cut);
                }
                assert_eq!(
                    crossing_point(a, b, axis, bound),
                    crossing_point(b, a, axis, bound)
                );
            }
        }
    }

    #[test]
    fn clipped_neighbours_keep_identical_shared_edges() {
        // All coordinates fit in one z0 tile. The shared diagonal has a
        // half-integer crossing: endpoint-relative rounding without ordering
        // produced y=110 for one ring and y=109 for its neighbour, opening
        // a triangular gap with positive area in the kept half-plane.
        let a = IntPoint::new(100, 110);
        let b = IntPoint::new(102, 109);
        let neighbours = [
            vec![vec![a, IntPoint::new(100, 100), b]],
            vec![vec![a, b, IntPoint::new(102, 120)]],
        ];
        for keep_left in [false, true] {
            let rect = IntRect {
                min_x: if keep_left { 0 } else { 101 },
                max_x: if keep_left { 101 } else { 4096 },
                min_y: 0,
                max_y: 4096,
            };
            let cut = IntPoint::new(101, 109);
            let endpoint = if keep_left { a } else { b };
            for neighbour in &neighbours {
                let mut out = Vec::new();
                assert!(clip_shape_rect_fast(neighbour, rect, &mut out));
                assert_eq!(out.len(), 1);
                let ring = &out[0][0];
                assert!(ring.iter().enumerate().any(|(i, &p)| {
                    let q = ring[(i + 1) % ring.len()];
                    (p == cut && q == endpoint) || (p == endpoint && q == cut)
                }));
            }
        }
    }

    #[test]
    fn buffered_cover_bounds_intersecting_cells_and_preserves_roots() {
        // Each bbox fits within a single z2 tile. Check both sides of its
        // edges, world clamping, and an exact buffer boundary.
        for (lo, hi) in [
            (4100, 4200),
            (3980, 4000),
            (4224, 4300),
            (-20, 20),
            (16370, 16400),
        ] {
            let rect = IntRect {
                min_x: lo,
                min_y: lo,
                max_x: hi,
                max_y: hi,
            };
            let (x0, x1, y0, y1) = tile_cover(rect, 2, 2, TILE_BUFFER_I32);
            for ty in 0..4 {
                for tx in 0..4 {
                    let cell = buffered_cell_rect_base(2, PyramidCell { z: 2, tx, ty });
                    if rect.min_x < cell.max_x
                        && rect.max_x > cell.min_x
                        && rect.min_y < cell.max_y
                        && rect.max_y > cell.min_y
                    {
                        assert!(tx >= x0 && tx <= x1 && ty >= y0 && ty <= y1);
                    }
                }
            }
        }
        let rect = IntRect {
            min_x: 4100,
            min_y: 4100,
            max_x: 4200,
            max_y: 4200,
        };
        assert_eq!(tile_cover(rect, 2, 2, 0), (1, 1, 1, 1));
        assert_eq!(tile_cover(rect, 2, 2, TILE_BUFFER_I32), (0, 1, 0, 1));
    }

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
        let dp_tol: &'static (dyn Fn(u8) -> ExtentUnits + Sync) =
            Box::leak(Box::new(move |_| ExtentUnits::new(tol)));
        PyramidParams {
            maxz,
            z_top,
            z_bottom,
            dp_tol,
            min_area: &|_| Area::ZERO,
            pins: None,
            tile_filter: None,
            simplifier: Simplifier::DouglasPeucker,
        }
    }

    fn property_below(state: &mut u64, upper: u32) -> u32 {
        u32::try_from(next_random(state) % u64::from(upper)).expect("bounded random value fits u32")
    }

    fn property_ring(points: &[(u32, u32)], reverse: bool) -> Vec<Point> {
        let mut ring: Vec<_> = points
            .iter()
            .map(|&(x, y)| Point {
                x: (4096.0 + f64::from(x)) / 16_384.0,
                y: (4096.0 + f64::from(y)) / 16_384.0,
            })
            .collect();
        if reverse {
            ring.reverse();
        }
        ring
    }

    fn random_quantized_polygon(state: &mut u64, hole_count: usize) -> Shape {
        const GRID_SIDE: u32 = 24;
        let min_x = 300 + property_below(state, 160);
        let min_y = 300 + property_below(state, 160);
        let max_x = 3796 - property_below(state, 160);
        let max_y = 3796 - property_below(state, 160);
        let cut = 80 + property_below(state, 100);
        let outer = property_ring(
            &[
                (min_x + cut, min_y),
                (max_x - cut, min_y),
                (max_x, min_y + cut),
                (max_x, max_y - cut),
                (max_x - cut, max_y),
                (min_x + cut, max_y),
                (min_x, max_y - cut),
                (min_x, min_y + cut),
            ],
            next_random(state) & 1 != 0,
        );

        let safe_min_x = min_x + 2 * cut;
        let safe_min_y = min_y + 2 * cut;
        let safe_max_x = max_x - 2 * cut;
        let safe_max_y = max_y - 2 * cut;
        let slot_width = (safe_max_x - safe_min_x) / GRID_SIDE;
        let slot_height = (safe_max_y - safe_min_y) / GRID_SIDE;
        let mut slots: Vec<u32> = (0..GRID_SIDE * GRID_SIDE).collect();
        for index in (1..slots.len()).rev() {
            let range = u64::try_from(index).expect("slot index fits u64") + 1;
            let swap = usize::try_from(next_random(state) % range).expect("slot index fits usize");
            slots.swap(index, swap);
        }

        let mut holes = Vec::with_capacity(hole_count);
        for slot in slots.into_iter().take(hole_count) {
            let slot_x = slot % GRID_SIDE;
            let slot_y = slot / GRID_SIDE;
            let x0 = safe_min_x
                + slot_x * slot_width
                + slot_width / 4
                + property_below(state, (slot_width / 8).max(1));
            let y0 = safe_min_y
                + slot_y * slot_height
                + slot_height / 4
                + property_below(state, (slot_height / 8).max(1));
            let width = slot_width / 3 + property_below(state, (slot_width / 8).max(1));
            let height = slot_height / 3 + property_below(state, (slot_height / 8).max(1));
            holes.push(property_ring(
                &[
                    (x0, y0),
                    (x0 + width, y0),
                    (x0 + width, y0 + height),
                    (x0, y0 + height),
                ],
                next_random(state) & 1 != 0,
            ));
        }

        crate::geometry::int_ocean::quantize_polygon(&outer, &holes, 2)
    }

    fn decoded_contour(ring: &[(i32, i32)]) -> Vec<IntPoint> {
        ring.iter().map(|&(x, y)| p(x, y)).collect()
    }

    #[test]
    fn seeded_quantized_pyramids_decode_with_polygon_ring_invariants() {
        fn zero_tolerance(_: u8) -> ExtentUnits {
            ExtentUnits::new(0)
        }
        fn zero_min_area(_: u8) -> Area {
            Area::ZERO
        }

        let dp_tol: &(dyn Fn(u8) -> ExtentUnits + Sync) = &zero_tolerance;
        let min_area: &(dyn Fn(u8) -> Area + Sync) = &zero_min_area;
        let params = PyramidParams {
            maxz: 2,
            z_top: 2,
            z_bottom: 2,
            dp_tol,
            min_area,
            pins: None,
            tile_filter: None,
            simplifier: Simplifier::DouglasPeucker,
        };
        let mut seed = 0xbb67_ae85_84ca_a73b;

        for case in 0..20 {
            let hole_count = if case == 19 {
                501
            } else {
                usize::try_from(property_below(&mut seed, 25)).expect("hole count fits usize")
            };
            let shape_base = random_quantized_polygon(&mut seed, hole_count);
            assert!(!shape_base.is_empty(), "case {case} quantizes to a polygon");
            assert!(
                polygon::ring_role(signed_area_2x(&shape_base[0]))
                    == Some(RingOrientation::Positive),
                "case {case} puts a positive outer in slot zero"
            );
            for hole in &shape_base[1..] {
                assert_eq!(
                    polygon::ring_role(signed_area_2x(hole)),
                    Some(RingOrientation::Negative),
                    "case {case} hole winding"
                );
                assert!(
                    hole.iter()
                        .all(|point| point_in_contour(point.x, point.y, &shape_base[0])),
                    "case {case} quantized hole remains inside its outer"
                );
            }

            let mut scratch = PyramidScratch::new();
            let mut emitted = Vec::new();
            emit_shape_pyramid(
                &shape_base,
                &params,
                &mut scratch,
                &mut |_, _, _, geom, _| emitted.push(geom.to_vec()),
            );
            assert!(!emitted.is_empty(), "case {case} emits polygon geometry");
            if case == 19 {
                assert!(emitted.len() > 1, "over-cap input is partitioned");
            }

            for geometry in emitted {
                let rings = polygon_rings(&geometry);
                assert!(!rings.is_empty(), "case {case} decodes polygon rings");
                assert!(
                    rings.len() <= MAX_FEATURE_RINGS,
                    "case {case} emitted {} rings, over the {MAX_FEATURE_RINGS} cap",
                    rings.len()
                );
                let outer = decoded_contour(&rings[0]);
                assert!(
                    polygon::ring_role(signed_area_2x(&outer)) == Some(RingOrientation::Positive),
                    "case {case} has a positive outer in decoded slot zero"
                );
                for hole in &rings[1..] {
                    let hole = decoded_contour(hole);
                    assert!(
                        polygon::ring_role(signed_area_2x(&hole))
                            == Some(RingOrientation::Negative),
                        "case {case} decoded hole winding"
                    );
                    assert!(
                        hole.iter()
                            .all(|point| point_in_contour(point.x, point.y, &outer)),
                        "case {case} decoded hole remains inside its outer"
                    );
                }
            }
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
                let (tx_min, tx_max, ty_min, ty_max) = tile_cover(bb, z, z, 0);
                for ty in ty_min..=ty_max {
                    for tx in tx_min..=tx_max {
                        let clipped = intersect_rect(shape, buffered_tile_rect(tx, ty), Area::ZERO);
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
            for ring in polygon_rings(b) {
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
            normalize_into(&mut scratch, quad, Area::ZERO, &mut normalized);
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
            Area::ZERO,
        );
        let right_frag = intersect_rect(
            &source,
            buffered_cell_rect_base(1, PyramidCell { z: 1, tx: 1, ty: 0 }),
            Area::ZERO,
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
            Area::ZERO,
        );
        let right_frag = intersect_rect(
            &source,
            buffered_cell_rect_base(1, PyramidCell { z: 1, tx: 1, ty: 0 }),
            Area::ZERO,
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
            let cross = polygon::triangle_area_2x((a.x, a.y), (b.x, b.y), (c.x, c.y)).get();
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
                intersect_shapes_with_rect(&mut scratch, &vec![subject.clone()], rect, &mut fast);
                let exact = intersect_rect(subject, rect, Area::ZERO);
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
        normalize_into(&mut int, raw, Area::ZERO, &mut normalized);
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
            let rings = polygon_rings(geom);
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
            for normalized in normalize(tile_shape, Area::ZERO) {
                let mut clipped = Vec::new();
                intersect_rect_into(scratch, &normalized, window, Area::ZERO, &mut clipped);
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
                signed_area_2x(&ours).true_area().get()
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
