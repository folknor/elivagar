use crate::geometry::overlay::{BoolOverlay, BoolRule, ShapeType};
use crate::geometry::polygon::{self, Area, Area2x, ExtentUnits, RENDER_PX, RingOrientation};
use crate::geometry::projection::Point;
use crate::mvt;

pub(crate) use crate::geometry::overlay::IntPoint;
use std::ops::Range;
use std::{cmp::Reverse, collections::BinaryHeap};

pub(crate) type Contour = crate::geometry::overlay::Contour;
pub(crate) type Shape = crate::geometry::overlay::Shape;
pub(crate) type Shapes = crate::geometry::overlay::Shapes;

/// Reused per-ring pin masks. A mask is paired with a contour only through
/// `PinnedRing`, which checks the point-for-point invariant before use.
#[derive(Default)]
pub(crate) struct PinnedRings {
    rings: Vec<Vec<bool>>,
}

impl PinnedRings {
    pub(crate) fn fill_for_shape(
        &mut self,
        shape: &Shape,
        mut fill: impl FnMut(usize, &Contour, &mut Vec<bool>),
    ) {
        self.rings.resize_with(shape.len(), Vec::new);
        for (idx, (points, pins)) in shape.iter().zip(&mut self.rings).enumerate() {
            pins.clear();
            fill(idx, points, pins);
            PinnedRing::new(points, pins);
        }
        self.rings.truncate(shape.len());
    }

    pub(crate) fn validate_for(&self, shape: &Shape) {
        assert_eq!(
            self.rings.len(),
            shape.len(),
            "pinned ring count does not match shape ring count"
        );
        for (points, pins) in shape.iter().zip(&self.rings) {
            PinnedRing::new(points, pins);
        }
    }

    /// Iterate pinned coordinates only after checking every ring and mask.
    pub(crate) fn pinned_points_for<'a>(
        &'a self,
        shape: &'a Shape,
    ) -> impl Iterator<Item = &'a IntPoint> + 'a {
        self.validate_for(shape);
        (0..shape.len()).flat_map(move |idx| self.ring_for(shape, idx).pinned_points())
    }

    fn ring_for<'a>(&'a self, shape: &'a Shape, idx: usize) -> PinnedRing<'a> {
        let points = &shape[idx];
        PinnedRing::new(points, &self.rings[idx])
    }

    fn prepare(&mut self, ring_count: usize) {
        self.rings.resize_with(ring_count, Vec::new);
        self.rings.truncate(ring_count);
        for pins in &mut self.rings {
            pins.clear();
        }
    }

    fn clear_contents(&mut self) {
        for pins in &mut self.rings {
            pins.clear();
        }
    }

    fn truncate(&mut self, ring_count: usize) {
        self.rings.truncate(ring_count);
    }

    fn ring_flags_mut(&mut self, idx: usize) -> &mut Vec<bool> {
        &mut self.rings[idx]
    }

    fn copy_for_shape(&mut self, shape: &Shape, source: &Self) {
        source.validate_for(shape);
        self.prepare(shape.len());
        for (idx, points) in shape.iter().enumerate() {
            self.rings[idx].extend_from_slice(&source.rings[idx]);
            PinnedRing::new(points, &self.rings[idx]);
        }
    }
}

/// A contour and the point-aligned pin mask that constrains its simplification.
struct PinnedRing<'a> {
    points: &'a Contour,
    flags: &'a [bool],
}

impl<'a> PinnedRing<'a> {
    fn new(points: &'a Contour, flags: &'a [bool]) -> Self {
        assert_eq!(
            points.len(),
            flags.len(),
            "pinned ring point count does not match pin flag count"
        );
        Self { points, flags }
    }

    fn pinned_points(self) -> impl Iterator<Item = &'a IntPoint> + 'a {
        self.points
            .iter()
            .zip(self.flags)
            .filter_map(|(point, &pinned)| pinned.then_some(point))
    }

    fn flags(&self) -> &'a [bool] {
        self.flags
    }
}

struct PinnedRingMut<'a> {
    points: &'a mut Contour,
    flags: &'a mut Vec<bool>,
}

impl<'a> PinnedRingMut<'a> {
    fn new(points: &'a mut Contour, flags: &'a mut Vec<bool>) -> Self {
        assert_eq!(
            points.len(),
            flags.len(),
            "pinned ring point count does not match pin flag count"
        );
        Self { points, flags }
    }

    fn push_nonduplicate(&mut self, point: IntPoint, pinned: bool) {
        if self.points.last().copied() == Some(point) {
            *self
                .flags
                .last_mut()
                .expect("a repeated point has a paired pin flag") |= pinned;
        } else {
            self.points.push(point);
            self.flags.push(pinned);
        }
    }

    fn remove_closing_duplicate(&mut self) {
        if self.points.len() >= 2 && self.points.first() == self.points.last() {
            let last = self.flags.pop().expect("closing point has a pin flag");
            *self.flags.first_mut().expect("first point has a pin flag") |= last;
            self.points.pop();
        }
    }

    fn orient(&mut self, outer: bool) {
        let expected = if outer {
            RingOrientation::Positive
        } else {
            RingOrientation::Negative
        };
        if polygon::should_reverse(signed_area_2x(self.points), expected) {
            self.points.reverse();
            self.flags.reverse();
        }
    }

    fn points(&self) -> &Contour {
        self.points
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IntRect {
    pub min_x: i32,
    pub min_y: i32,
    pub max_x: i32,
    pub max_y: i32,
}

// The OCEAN_* values, and any change to the bytes this module emits on the
// ocean path (quantize, rescale, DP/VW simplify, encode), are baked into the
// durable ocean artifact. Such a change requires bumping OCEAN_POLICY_VERSION
// in src/ocean/policy.rs so stale artifacts fail the key check instead of silently
// serving the old geometry.
pub(crate) const OCEAN_DP_TOL_PX: i64 = RENDER_PX.get();
// This remains 256 in doubled-area units, which is true Area(128). The
// independent min-area drop is Area(256). The existing code and golden fix
// these values and their output, but no source explains the factor of two.
pub(crate) const OCEAN_VW_AREA_2X: Area2x = RENDER_PX.squared_area_2x();
pub(crate) const OSM_DP_TOL_PX: i64 = RENDER_PX.get();
// Keep the existing names for pyramid and assemble callers; tiles owns the grid.
use super::tiles::grid::EXTENT_LOG2;
pub(crate) use super::tiles::grid::{BUFFER_UNITS as TILE_BUFFER_I32, EXTENT as TILE_EXTENT_I32};

pub(crate) struct IntEmitScratch {
    pub tile_points: Vec<(i32, i32)>,
    pub tile_ranges: Vec<Range<usize>>,
    pub geom_buf: Vec<u32>,
    dp: DpScratch,
    vw: VwScratch,
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
            vw: VwScratch::default(),
            overlay: BoolOverlay::new(),
            rect_contour: Vec::with_capacity(4),
        }
    }

    pub(crate) fn recycle_shapes(&mut self, shapes: &mut Shapes) {
        self.overlay.recycle(shapes);
    }

    pub(crate) fn take_shape(&mut self, ring_count: usize) -> Shape {
        self.overlay.take_shape(ring_count)
    }

    pub(crate) fn recycle_owned_shape(&mut self, shape: Shape) {
        self.overlay.recycle_owned_shape(shape);
    }

    fn recycle_shapes_from(&mut self, shapes: &mut Shapes, start: usize) {
        self.overlay.recycle_from(shapes, start);
    }

    fn recycle_contours_from(&mut self, shape: &mut Shape, start: usize) {
        self.overlay.recycle_contours_from(shape, start);
    }
}

#[derive(Default)]
struct VwScratch {
    work: Contour,
    pin_flags: Vec<bool>,
    prev: Vec<u32>,
    next: Vec<u32>,
    alive: Vec<bool>,
    heap: BinaryHeap<Reverse<VwEntry>>,
    seq: Vec<u32>,
    out: Contour,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct VwEntry {
    area2x: Area2x,
    x: i32,
    y: i32,
    generation: u32,
    idx: u32,
}

pub(crate) fn vw_area_threshold(dp_tol: ExtentUnits) -> Area2x {
    if dp_tol.get() <= 0 {
        Area2x::ZERO
    } else {
        OCEAN_VW_AREA_2X
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

/// The overlay engine's integer coordinate interval, `[-2^30, 2^30 - 1]`
/// (the in-tree port inherits i_overlay's `-2^(N-2)..=2^(N-2)-1` range for
/// an `N`-bit engine; outside it a coordinate difference or cross product
/// overflows). Every base coordinate the quantizer hands out lies in it.
pub(crate) const BASE_COORD_MIN: i64 = -(1 << 30);
pub(crate) const BASE_COORD_MAX: i64 = (1 << 30) - 1;

/// A polygon with a vertex that is non-finite or quantizes outside the
/// overlay interval. Quantization is unclamped on purpose (antimeridian
/// copies sit outside `[0, 1]`), so representability is a property of the
/// input, not of the grid zoom: an unwrapped OSM path follows its previous
/// vertex and can accumulate many world widths. The whole polygon is refused
/// rather than the offending ring, because dropping one hole would turn it
/// into fill. The refusal concerns numeric representability only; it is not
/// a policy on which geography is supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unrepresentable;

/// Base-grid scale for grid zoom `maxz`. Premise, not checked here: `maxz`
/// is at most the profile top (OSM polygons use the profile top, ocean its
/// fixed z7 and z14 pass grids), so the shift is at most 26 bits.
fn base_scale(maxz: u8) -> i64 {
    1_i64 << (u32::from(maxz) + EXTENT_LOG2)
}

fn coord_is_representable(v: f64, scale: i64) -> bool {
    #[allow(clippy::cast_precision_loss)]
    let r = (v * scale as f64).round();
    #[allow(clippy::cast_precision_loss)]
    let ok = r.is_finite() && r >= BASE_COORD_MIN as f64 && r <= BASE_COORD_MAX as f64;
    ok
}

/// Check every vertex of every ring - inners included even when the outer
/// would collapse - before any quantization.
fn check_representable(
    outer: &[Point],
    inners: &[Vec<Point>],
    maxz: u8,
) -> Result<(), Unrepresentable> {
    let scale = base_scale(maxz);
    let ok = std::iter::once(outer)
        .chain(inners.iter().map(Vec::as_slice))
        .flatten()
        .all(|p| coord_is_representable(p.x, scale) && coord_is_representable(p.y, scale));
    if ok { Ok(()) } else { Err(Unrepresentable) }
}

/// Quantize a polygon the caller already knows to be representable. The one
/// production caller is ocean, whose shapefile reader refuses any point
/// outside normalized `[-1, 2]`; on its z7 and z14 grids that bounds base
/// coordinates at `2^27`, inside the overlay interval.
pub(crate) fn quantize_polygon(outer: &[Point], inners: &[Vec<Point>], maxz: u8) -> Shape {
    check_representable(outer, inners, maxz)
        .expect("ocean points are reader-checked to normalized [-1, 2] on a grid of at most z14");
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
///
/// An unrepresentable polygon leaves `out` empty and returns the refusal;
/// ordinary collapse returns `Ok` with an empty shape.
pub(crate) fn quantize_polygon_into(
    outer: &[Point],
    inners: &[Vec<Point>],
    maxz: u8,
    out: &mut Shape,
) -> Result<(), Unrepresentable> {
    if let Err(e) = check_representable(outer, inners, maxz) {
        out.clear();
        return Err(e);
    }
    let mut used = 0usize;
    if quantize_ring_at(out, &mut used, outer, maxz, true) {
        for inner in inners {
            quantize_ring_at(out, &mut used, inner, maxz, false);
        }
    } else {
        used = 0;
    }
    out.truncate(used);
    Ok(())
}

/// Quantize a polygon with pin flags into reused Shape + flags buffers.
/// Same recycling and refusal contract as `quantize_polygon_into`; a refusal
/// clears the flags too.
pub(crate) fn quantize_polygon_pinned_into<F>(
    outer: &[Point],
    inners: &[Vec<Point>],
    maxz: u8,
    mut pin_test: F,
    out: &mut Shape,
    out_flags: &mut PinnedRings,
) -> Result<(), Unrepresentable>
where
    F: FnMut(&Point) -> bool,
{
    if let Err(e) = check_representable(outer, inners, maxz) {
        out.clear();
        out_flags.truncate(0);
        return Err(e);
    }
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
    out_flags.validate_for(out);
    Ok(())
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
    let scale = base_scale(maxz);
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
    out_flags: &mut PinnedRings,
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
    if *used == out_flags.rings.len() {
        out_flags.rings.push(Vec::new());
    }
    let ring = &mut out[*used];
    let flags = &mut out_flags.rings[*used];
    ring.clear();
    flags.clear();
    let mut paired = PinnedRingMut::new(ring, flags);
    let scale = base_scale(maxz);
    for p in points {
        paired.push_nonduplicate(
            IntPoint::new(quantize_coord(p.x, scale), quantize_coord(p.y, scale)),
            pin_test(p),
        );
    }
    paired.remove_closing_duplicate();
    if !ring_is_valid(paired.points()) {
        return false;
    }
    paired.orient(outer);
    *used += 1;
    true
}

/// Test-only reference: production rescaling goes through
/// `rescale_shape_pinned_into` (the pyramid always carries pin flags).
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
        } else if i == 0 {
            // Holes cannot survive independently of their original outer.
            return Vec::new();
        }
    }
    if out.first().is_some_and(|outer| outer.len() >= 3) {
        out
    } else {
        Vec::new()
    }
}

pub(crate) fn copy_shape_into(shape: &Shape, out: &mut Shape) {
    debug_assert_eq!(out.len(), shape.len());
    for (source, target) in shape.iter().zip(out.iter_mut()) {
        target.clear();
        target.extend_from_slice(source);
    }
}

pub(crate) fn rescale_shape_pinned_into(
    scratch: &mut IntEmitScratch,
    shape: &Shape,
    s: u8,
    flags: &PinnedRings,
    out: &mut Shape,
    out_flags: &mut PinnedRings,
) {
    assert_eq!(out.len(), shape.len(), "rescale output ring slots mismatch");
    flags.validate_for(shape);
    if s == 0 {
        copy_shape_into(shape, out);
        out_flags.copy_for_shape(shape, flags);
        return;
    }

    out_flags.prepare(shape.len());
    let mut used = 0usize;
    for (i, contour) in shape.iter().enumerate() {
        let ring_flags = flags.ring_for(shape, i);
        let ring = &mut out[used];
        ring.clear();
        let flags_ring = out_flags.ring_flags_mut(used);
        flags_ring.clear();
        let mut paired = PinnedRingMut::new(ring, flags_ring);
        for (idx, &p) in contour.iter().enumerate() {
            paired.push_nonduplicate(
                IntPoint::new(shift_round(p.x, s), shift_round(p.y, s)),
                ring_flags.flags()[idx],
            );
        }
        paired.remove_closing_duplicate();
        if ring_is_valid(paired.points()) {
            paired.orient(i == 0);
            used += 1;
        } else if i == 0 {
            // Do not compact a surviving hole into the outer slot.
            break;
        }
    }
    if !out.first().is_some_and(|outer| outer.len() >= 3) {
        used = 0;
    }
    scratch.recycle_contours_from(out, used);
    out_flags.truncate(used);
    assert_eq!(
        out.len(),
        out_flags.rings.len(),
        "rescaled pinned ring count does not match shape ring count"
    );
}

#[hotpath::measure]
pub(crate) fn simplify_shape_dp(
    scratch: &mut IntEmitScratch,
    shape: &mut Shape,
    tol: ExtentUnits,
    mut pins: Option<&mut PinnedRings>,
) {
    if let Some(pins) = pins.as_deref() {
        pins.validate_for(shape);
    }
    if tol.get() <= 0 {
        return;
    }

    // In place: each surviving contour is rewritten inside its own
    // allocation and compacted forward; dead contours return to the ring
    // pool. The original outer must survive before any hole is retained.
    let mut write = 0usize;
    for read in 0..shape.len() {
        let pin_ring = pins.as_deref().map(|p| p.rings[read].as_slice());
        if simplify_contour_dp_in_place(&mut shape[read], tol, pin_ring, read == 0, &mut scratch.dp)
        {
            shape.swap(write, read);
            write += 1;
        } else if read == 0 {
            scratch.recycle_contours_from(shape, 0);
            if let Some(pins) = pins.as_deref_mut() {
                pins.clear_contents();
            }
            return;
        }
    }
    scratch.recycle_contours_from(shape, write);
    if !shape.first().is_some_and(|outer| outer.len() >= 3) {
        scratch.recycle_contours_from(shape, 0);
    }
    if let Some(pins) = &mut pins {
        pins.clear_contents();
    }
}

#[hotpath::measure]
pub(crate) fn simplify_shape_vw(
    scratch: &mut IntEmitScratch,
    shape: &mut Shape,
    area_2x_thresh: Area2x,
    mut pins: Option<&mut PinnedRings>,
) {
    if let Some(pins) = pins.as_deref() {
        pins.validate_for(shape);
    }
    if area_2x_thresh <= Area2x::ZERO {
        return;
    }
    if shape.is_empty() {
        return;
    }
    let outer_pins = pins.as_deref().map(|p| p.rings[0].as_slice());
    if !simplify_contour_vw_in_place(
        &mut shape[0],
        area_2x_thresh,
        outer_pins,
        true,
        &mut scratch.vw,
    ) {
        scratch.recycle_contours_from(shape, 0);
        if let Some(pins) = pins.as_deref_mut() {
            pins.clear_contents();
        }
        return;
    }
    let mut write = 1usize;
    for read in 1..shape.len() {
        let pin_ring = pins.as_deref().map(|p| p.rings[read].as_slice());
        if simplify_contour_vw_in_place(
            &mut shape[read],
            area_2x_thresh,
            pin_ring,
            false,
            &mut scratch.vw,
        ) {
            shape.swap(write, read);
            write += 1;
        }
    }
    scratch.recycle_contours_from(shape, write);
    if let Some(pins) = &mut pins {
        pins.clear_contents();
    }
}

fn simplify_contour_vw_in_place(
    contour: &mut Contour,
    threshold: Area2x,
    pins: Option<&[bool]>,
    outer: bool,
    vw: &mut VwScratch,
) -> bool {
    vw.work.clear();
    vw.work.extend_from_slice(contour);
    vw.pin_flags.clear();
    if let Some(p) = pins {
        PinnedRing::new(contour, p);
        vw.pin_flags.extend_from_slice(p);
    } else {
        vw.pin_flags.resize(contour.len(), false);
    }
    {
        let mut paired = PinnedRingMut::new(&mut vw.work, &mut vw.pin_flags);
        paired.remove_closing_duplicate();
    }
    let n = vw.work.len();
    if n <= 3 {
        if !ring_is_valid(&vw.work) {
            return false;
        }
        contour.clear();
        contour.extend_from_slice(&vw.work);
        orient_ring(contour, outer);
        return true;
    }
    vw.prev.clear();
    vw.next.clear();
    vw.alive.clear();
    vw.seq.clear();
    vw.heap.clear();
    vw.prev
        .extend((0..n).map(|i| u32::try_from((i + n - 1) % n).expect("ring index fits u32")));
    vw.next
        .extend((0..n).map(|i| u32::try_from((i + 1) % n).expect("ring index fits u32")));
    vw.alive.resize(n, true);
    vw.seq.resize(n, 0);
    for i in 0..n {
        vw_refresh(i, vw);
    }
    let mut live = n;
    while live > 3 {
        let Some(Reverse(entry)) = vw.heap.pop() else {
            break;
        };
        let idx = entry.idx as usize;
        if !vw.alive[idx] || vw.seq[idx] != entry.generation {
            continue;
        }
        if entry.area2x.meets(threshold) {
            break;
        }
        let left = usize::try_from(vw.prev[idx]).expect("ring index fits usize");
        let right = usize::try_from(vw.next[idx]).expect("ring index fits usize");
        vw.alive[idx] = false;
        vw.next[left] = u32::try_from(right).expect("ring index fits u32");
        vw.prev[right] = u32::try_from(left).expect("ring index fits u32");
        live -= 1;
        vw_refresh(left, vw);
        vw_refresh(right, vw);
    }
    let Some(start) = (0..n)
        .filter(|&i| vw.alive[i])
        .min_by_key(|&i| (vw.work[i].x, vw.work[i].y))
    else {
        return false;
    };
    vw.out.clear();
    let mut i = start;
    loop {
        push_nonduplicate(&mut vw.out, vw.work[i]);
        i = usize::try_from(vw.next[i]).expect("ring index fits usize");
        if i == start {
            break;
        }
    }
    remove_closing_duplicate(&mut vw.out);
    if !ring_is_valid(&vw.out) {
        return false;
    }
    contour.clear();
    contour.extend_from_slice(&vw.out);
    orient_ring(contour, outer);
    true
}

fn vw_refresh(i: usize, vw: &mut VwScratch) {
    if !vw.alive[i] || vw.pin_flags[i] {
        return;
    }
    let a = vw.work[vw.prev[i] as usize];
    let b = vw.work[i];
    let c = vw.work[vw.next[i] as usize];
    let cross = polygon::triangle_area_2x_from_deltas(
        i128::from(b.x - a.x),
        i128::from(b.y - a.y),
        i128::from(c.x - a.x),
        i128::from(c.y - a.y),
    )
    .magnitude();
    vw.seq[i] = vw.seq[i].wrapping_add(1);
    vw.heap.push(Reverse(VwEntry {
        area2x: cross,
        x: b.x,
        y: b.y,
        generation: vw.seq[i],
        idx: u32::try_from(i).expect("ring index fits u32"),
    }));
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn normalize(shape: Shape, min_area: Area) -> Shapes {
    let mut scratch = IntEmitScratch::new();
    let mut out = Vec::new();
    normalize_into(&mut scratch, shape, min_area, &mut out);
    out
}

#[hotpath::measure]
pub(crate) fn normalize_into(
    scratch: &mut IntEmitScratch,
    mut shape: Shape,
    min_area: Area,
    out: &mut Shapes,
) {
    scratch.overlay.recycle(out);
    if shape.is_empty() {
        scratch.overlay.recycle_owned_shape(shape);
        return;
    }
    scratch.overlay.min_output_area = min_area;
    if shape.len() == 1 {
        // The port fast path omits its threshold filter. Both paths below
        // re-check through Area::meets, preserving that deliberate handoff.
        if scratch.overlay.simplify_contour_into(&shape[0], out) {
            scratch.overlay.recycle_owned_shape(shape);
            clean_shapes_in_place(scratch, out, min_area);
            return;
        }
        clean_shape_in_place(scratch, &mut shape, min_area);
        if shape.first().is_some_and(|outer| outer.len() >= 3) {
            out.push(shape);
        } else {
            // The cleaned shape is dead (clean keeps only >= 3-point rings,
            // so this means it is empty): return the pooled shell instead of
            // dropping it.
            scratch.overlay.recycle_owned_shape(shape);
        }
        return;
    }

    scratch.overlay.clear();
    scratch.overlay.add_shape(&shape, ShapeType::Subject);
    scratch.overlay.recycle_owned_shape(shape);
    scratch.overlay.overlay_nested(BoolRule::Subject, out);
    clean_shapes_in_place(scratch, out, min_area);
}

#[cfg(test)]
pub(crate) fn intersect_rect(shape: &Shape, rect: IntRect, min_area: Area) -> Shapes {
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
    min_area: Area,
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
    clean_shapes_in_place(scratch, out, min_area);
}

/// Shape-vs-shape Intersect. Production clipping is rect-vs-shape
/// (`intersect_rect_into` and friends); this general form survives only for
/// the coverage arithmetic in the simplifier property tests below.
#[cfg(test)]
pub(crate) fn intersect_shapes_into(
    scratch: &mut IntEmitScratch,
    subject: &Shape,
    clip: &Shape,
    out: &mut Shapes,
) {
    scratch.overlay.recycle(out);
    if subject.is_empty() || clip.is_empty() {
        return;
    }
    scratch.overlay.clear();
    scratch.overlay.add_shape(subject, ShapeType::Subject);
    scratch.overlay.add_shape(clip, ShapeType::Clip);
    scratch.overlay.overlay_nested(BoolRule::Intersect, out);
    clean_shapes_in_place(scratch, out, Area::ZERO);
}

fn fill_rect_contour(out: &mut Contour, rect: IntRect) {
    out.clear();
    out.push(IntPoint::new(rect.min_x, rect.min_y));
    out.push(IntPoint::new(rect.max_x, rect.min_y));
    out.push(IntPoint::new(rect.max_x, rect.max_y));
    out.push(IntPoint::new(rect.min_x, rect.max_y));
}

fn quantize_ring(points: &[Point], maxz: u8, outer: bool) -> Option<Contour> {
    let scale = base_scale(maxz);
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

/// Every caller has passed the polygon through `check_representable`, so the
/// rounded value is finite and inside the overlay interval; the debug
/// assertion restates that before the saturating float cast.
fn quantize_coord(v: f64, scale: i64) -> i32 {
    debug_assert!(coord_is_representable(v, scale));
    #[allow(clippy::cast_possible_truncation)]
    let rounded = (v * scale as f64).round() as i64;
    i32::try_from(rounded).expect("a representability-checked base coordinate fits i32")
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
    if ring.len() < 3 || polygon::ring_role(signed_area_2x(ring)).is_none() {
        return false;
    }
    // Distinct-point count saturating at 3, tracked in a fixed array: this
    // runs per ring inside the clean/rescale hot paths (millions of calls
    // per build), so it must not heap-allocate.
    let mut unique = [IntPoint::new(0, 0); 3];
    let mut count = 0usize;
    for &p in ring {
        if !unique[..count].contains(&p) {
            unique[count] = p;
            count += 1;
            if count >= 3 {
                return true;
            }
        }
    }
    false
}

fn orient_ring(ring: &mut Contour, outer: bool) {
    let expected = if outer {
        RingOrientation::Positive
    } else {
        RingOrientation::Negative
    };
    if polygon::should_reverse(signed_area_2x(ring), expected) {
        ring.reverse();
    }
}

pub(crate) fn signed_area_2x(ring: &Contour) -> Area2x {
    polygon::signed_area_2x(ring.iter().map(|point| (point.x, point.y)))
}

fn simplify_contour_dp_in_place(
    contour: &mut Contour,
    tol: ExtentUnits,
    pins: Option<&[bool]>,
    outer: bool,
    dp: &mut DpScratch,
) -> bool {
    dp.ring.clear();
    dp.ring.extend_from_slice(contour);
    dp.pin_flags.clear();
    if let Some(p) = pins {
        dp.pin_flags
            .extend_from_slice(PinnedRing::new(contour, p).flags());
    } else {
        dp.pin_flags.resize(contour.len(), false);
    }
    {
        let mut paired = PinnedRingMut::new(&mut dp.ring, &mut dp.pin_flags);
        paired.remove_closing_duplicate();
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
    let paired = PinnedRing::new(&dp.ring, &dp.pin_flags);
    collect_chain_flags_into(&paired, a0, a1, &mut dp.pins_a);
    collect_chain_flags_into(&paired, a1, a0, &mut dp.pins_b);
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

fn collect_chain_flags_into(
    ring: &PinnedRing<'_>,
    start: usize,
    end: usize,
    chain: &mut Vec<bool>,
) {
    chain.clear();
    let mut idx = start;
    loop {
        chain.push(ring.flags[idx]);
        if idx == end {
            break;
        }
        idx = (idx + 1) % ring.points.len();
    }
}

fn dp_keep_into(
    chain: &Contour,
    tol: ExtentUnits,
    pins: &[bool],
    keep: &mut Vec<bool>,
    stack: &mut Vec<(usize, usize)>,
) {
    assert_eq!(chain.len(), pins.len(), "DP chain pin count mismatch");
    keep.clear();
    keep.resize(chain.len(), false);
    if chain.is_empty() {
        return;
    }
    keep[0] = true;
    keep[chain.len() - 1] = true;
    for (idx, &pinned) in pins.iter().enumerate() {
        if pinned {
            keep[idx] = true;
        }
    }

    let tol = tol.get();
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

fn clean_shapes_in_place(scratch: &mut IntEmitScratch, shapes: &mut Shapes, min_area: Area) {
    let mut write = 0usize;
    for read in 0..shapes.len() {
        clean_shape_in_place(scratch, &mut shapes[read], min_area);
        if shapes[read].first().is_some_and(|outer| outer.len() >= 3) {
            if write != read {
                shapes.swap(write, read);
            }
            write += 1;
        }
    }
    scratch.recycle_shapes_from(shapes, write);
}

fn clean_shape_in_place(scratch: &mut IntEmitScratch, shape: &mut Shape, min_area: Area) {
    let mut write = 0usize;
    for read in 0..shape.len() {
        clean_contour_in_place(&mut shape[read]);
        if ring_is_valid(&shape[read]) && true_area(&shape[read]).meets(min_area) {
            orient_ring(&mut shape[read], read == 0);
            if write != read {
                shape.swap(write, read);
            }
            write += 1;
        } else if read == 0 {
            // Overlay output still obeys the outer-then-holes contract.
            scratch.recycle_contours_from(shape, 0);
            return;
        }
    }
    scratch.recycle_contours_from(shape, write);
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

fn true_area(ring: &Contour) -> Area {
    signed_area_2x(ring).true_area()
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

pub(crate) fn tile_origin(t: u32) -> i32 {
    i32::try_from(t).expect("tile coordinate fits i32") * TILE_EXTENT_I32
}

pub(crate) fn buffered_tile_rect(tx: u32, ty: u32) -> IntRect {
    super::pyramid::buffered_cell_rect_base(0, super::pyramid::PyramidCell { z: 0, tx, ty })
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
    // Tile y grows down, so positive shoelace is the rendered clockwise role.
    let start = points.len();
    points.extend(contour.iter().map(|p| (p.x - ox, p.y - oy)));
    if points[start..].first() != points[start..].last()
        && let Some(&first) = points.get(start)
    {
        points.push(first);
    }
    let expected = if clockwise {
        RingOrientation::Positive
    } else {
        RingOrientation::Negative
    };
    if polygon::should_reverse(signed_area_tile_2x(&points[start..]), expected) {
        points[start..].reverse();
    }
    if points.len() >= start + 4 {
        ranges.push(start..points.len());
    } else {
        points.truncate(start);
    }
}

fn signed_area_tile_2x(ring: &[(i32, i32)]) -> Area2x {
    polygon::signed_area_2x(ring.iter().copied())
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

pub(crate) fn contour_area_is_below(ring: &Contour, min_area: Area) -> bool {
    true_area(ring).below_nonzero_minimum(min_area)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tol(value: i64) -> ExtentUnits {
        ExtentUnits::new(value)
    }

    fn area2x(value: i128) -> Area2x {
        Area2x::new(value)
    }

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
        assert_eq!(
            polygon::ring_role(signed_area_2x(&shape[0])),
            Some(RingOrientation::Positive)
        );
        for hole in &shape[1..] {
            assert_eq!(
                polygon::ring_role(signed_area_2x(hole)),
                Some(RingOrientation::Negative)
            );
        }
    }

    #[test]
    #[should_panic(expected = "pinned ring point count does not match pin flag count")]
    fn pinned_rings_reject_mask_desynchronization() {
        let shape = square_shape(0, 100);
        let pins = PinnedRings {
            rings: vec![vec![false; 3]],
        };
        pins.validate_for(&shape);
    }

    #[test]
    #[should_panic(expected = "pinned ring count does not match shape ring count")]
    fn pinned_point_iteration_rejects_missing_ring_masks() {
        let shape = square_shape(0, 100);
        let pins = PinnedRings::default();
        assert_eq!(pins.pinned_points_for(&shape).count(), 0);
    }

    #[test]
    fn simplifiers_preserve_a_pin_on_the_closing_duplicate() {
        let target = p(50, 1);
        let original = vec![vec![
            target,
            p(100, 0),
            p(100, 100),
            p(0, 100),
            p(0, 0),
            target,
        ]];
        for use_vw in [false, true] {
            let mut shape = original.clone();
            let mut pins = PinnedRings::default();
            pins.fill_for_shape(&shape, |_, ring, flags| {
                flags.resize(ring.len(), false);
                *flags.last_mut().expect("closed ring has a last vertex") = true;
            });
            let mut scratch = IntEmitScratch::new();
            if use_vw {
                simplify_shape_vw(&mut scratch, &mut shape, area2x(256), Some(&mut pins));
            } else {
                simplify_shape_dp(&mut scratch, &mut shape, tol(16), Some(&mut pins));
            }
            assert!(
                shape[0].contains(&target),
                "closing vertex pin lost (VW={use_vw})"
            );
        }
    }

    #[test]
    fn representability_is_the_overlay_interval_on_the_rounded_value() {
        let scale = 1_i64 << 26;
        #[allow(clippy::cast_precision_loss)]
        let s = scale as f64;
        // Exact endpoints: -2^30 is in, 2^30 - 1 is in, 2^30 is out.
        assert!(coord_is_representable(-16.0, scale));
        assert!(coord_is_representable(
            ((1_i64 << 30) - 1) as f64 / s,
            scale
        ));
        assert!(!coord_is_representable(16.0, scale));
        // Rounding across the endpoints decides, not the raw product.
        assert!(coord_is_representable(
            (((1_i64 << 30) - 1) as f64 + 0.4) / s,
            scale
        ));
        assert!(!coord_is_representable(
            (((1_i64 << 30) - 1) as f64 + 0.6) / s,
            scale
        ));
        assert!(!coord_is_representable(
            (-(1_i64 << 30) as f64 - 0.6) / s,
            scale
        ));
        assert!(!coord_is_representable(f64::NAN, scale));
        assert!(!coord_is_representable(f64::INFINITY, scale));
        // -32 world widths used to pass as i32::MIN; it is outside the engine.
        assert!(!coord_is_representable(-32.0, scale));
    }

    #[test]
    fn unrepresentable_inner_refuses_the_whole_polygon_and_clears_buffers() {
        // A collapsed outer (all one point) must not hide an unrepresentable
        // inner; the refusal also clears leftovers from an earlier call.
        let good = merc_ring(&[(0.1, 0.1), (0.1, 0.2), (0.2, 0.2), (0.2, 0.1), (0.1, 0.1)]);
        let collapsed = merc_ring(&[(0.5, 0.5), (0.5, 0.5), (0.5, 0.5), (0.5, 0.5)]);
        let far = merc_ring(&[(40.0, 0.1), (40.0, 0.2), (40.1, 0.2), (40.0, 0.1)]);
        let mut shape = Shape::new();
        quantize_polygon_into(&good, &[], 14, &mut shape).expect("in-world");
        assert!(!shape.is_empty());
        assert_eq!(
            quantize_polygon_into(&collapsed, std::slice::from_ref(&far), 14, &mut shape),
            Err(Unrepresentable)
        );
        assert!(shape.is_empty());

        let mut flags = PinnedRings::default();
        quantize_polygon_pinned_into(&good, &[], 14, |_| true, &mut shape, &mut flags)
            .expect("in-world");
        assert!(!shape.is_empty());
        assert_eq!(
            quantize_polygon_pinned_into(&good, &[far], 14, |_| true, &mut shape, &mut flags),
            Err(Unrepresentable)
        );
        assert!(shape.is_empty());
        flags.validate_for(&shape);

        // Ordinary collapse is not a refusal.
        assert_eq!(
            quantize_polygon_into(&collapsed, &[], 14, &mut shape),
            Ok(())
        );
        assert!(shape.is_empty());
    }

    #[test]
    fn quantized_base_pins_follow_duplicate_collapse_and_ring_orientation() {
        let outer = merc_ring(&[
            (0.0, 0.0),
            (0.0, 0.25),
            (0.25, 0.25),
            (0.25, 0.0),
            (0.0, 0.0),
        ]);
        let mut shape = Shape::new();
        let mut flags = PinnedRings::default();
        quantize_polygon_pinned_into(
            &outer,
            &[],
            0,
            |point| point.x <= 0.0 && point.y > 0.0,
            &mut shape,
            &mut flags,
        )
        .expect("an in-world ring is representable");
        let pinned: Vec<_> = flags
            .pinned_points_for(&shape)
            .map(|point| (point.x, point.y))
            .collect();
        assert_eq!(pinned, vec![(0, 1024)]);
        assert_eq!(
            polygon::ring_role(signed_area_2x(&shape[0])),
            Some(RingOrientation::Positive)
        );
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
    fn rescale_drops_holes_when_original_outer_collapses() {
        // A contract fixture: the second ring must never become an outer,
        // regardless of whether the input holes were properly contained.
        let shape = vec![
            vec![p(0, 0), p(1, 0), p(1, 1), p(0, 1)],
            vec![p(4, 4), p(4, 12), p(12, 12), p(12, 4)],
        ];
        assert!(rescale_shape(&shape, 2).is_empty());
        let mut flags = PinnedRings::default();
        flags.fill_for_shape(&shape, |idx, ring, pins| {
            pins.resize(ring.len(), idx == 1);
        });
        let mut scratch = IntEmitScratch::new();
        let mut out = scratch.take_shape(shape.len());
        let mut out_flags = PinnedRings::default();
        rescale_shape_pinned_into(&mut scratch, &shape, 2, &flags, &mut out, &mut out_flags);
        assert!(out.is_empty());
        assert!(out_flags.rings.is_empty());
    }

    #[test]
    fn simplifiers_drop_pinned_holes_when_original_outer_collapses() {
        // Both rings are valid and the hole is contained. Only its vertices
        // are pinned, so an aggressive DP can destroy the enclosing outer.
        let shape = vec![
            vec![p(0, 0), p(100, 0), p(100, 100), p(0, 100)],
            vec![p(20, 20), p(20, 80), p(80, 80), p(80, 20)],
        ];
        let mut flags = PinnedRings::default();
        flags.fill_for_shape(&shape, |idx, ring, pins| {
            pins.resize(ring.len(), idx == 1);
        });
        let mut scratch = IntEmitScratch::new();
        let mut dp = shape.clone();
        simplify_shape_dp(&mut scratch, &mut dp, tol(1000), Some(&mut flags));
        assert!(dp.is_empty());

        // VW has a three-vertex floor, so use a degenerate original outer
        // to exercise its existing whole-shape rejection policy.
        let mut vw = shape;
        vw[0] = vec![p(0, 0), p(1, 0), p(2, 0)];
        let mut vw_flags = PinnedRings::default();
        vw_flags.fill_for_shape(&vw, |idx, ring, pins| {
            pins.resize(ring.len(), idx == 1);
        });
        simplify_shape_vw(&mut scratch, &mut vw, area2x(16), Some(&mut vw_flags));
        assert!(vw.is_empty());
    }

    #[test]
    fn clean_shape_does_not_promote_holes_after_outer_area_rejection() {
        // A malformed overlay result still must not promote its hole.
        let mut shape = vec![
            vec![p(0, 0), p(1, 0), p(1, 1), p(0, 1)],
            vec![p(4, 4), p(4, 12), p(12, 12), p(12, 4)],
        ];
        clean_shape_in_place(&mut IntEmitScratch::new(), &mut shape, Area::from_u64(2));
        assert!(shape.is_empty());
    }

    #[test]
    fn simplify_shape_dp_rotation_invariance() {
        let ring = vec![p(0, 0), p(40, 0), p(40, 2), p(40, 40), p(0, 40), p(0, 2)];
        let mut shape_a = vec![ring.clone()];
        let mut rotated = ring[3..].to_vec();
        rotated.extend_from_slice(&ring[..3]);
        let mut shape_b = vec![rotated];
        let mut scratch = IntEmitScratch::new();
        simplify_shape_dp(&mut scratch, &mut shape_a, tol(16), None);
        simplify_shape_dp(&mut scratch, &mut shape_b, tol(16), None);
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
        simplify_shape_dp(&mut IntEmitScratch::new(), &mut shape, tol(16), None);
        assert_eq!(shape[0].len(), 4);
    }

    #[test]
    fn simplify_shape_dp_old_ring_seam_not_privileged() {
        let ring = vec![p(0, 0), p(1, 8), p(2, 0), p(40, 0), p(40, 40), p(0, 40)];
        let mut shape = vec![ring];
        simplify_shape_dp(&mut IntEmitScratch::new(), &mut shape, tol(16), None);
        assert!(!shape[0].contains(&p(1, 8)));
    }

    #[test]
    fn simplify_shape_vw_rotation_invariance() {
        let ring = vec![p(0, 0), p(40, 0), p(40, 2), p(40, 40), p(0, 40), p(0, 2)];
        let mut a = vec![ring.clone()];
        let mut b = vec![ring[3..].to_vec()];
        b[0].extend_from_slice(&ring[..3]);
        let mut scratch = IntEmitScratch::new();
        simplify_shape_vw(&mut scratch, &mut a, area2x(256), None);
        simplify_shape_vw(&mut scratch, &mut b, area2x(256), None);
        assert_eq!(sorted_points(&a[0]), sorted_points(&b[0]));
    }

    #[test]
    fn simplify_shape_vw_keeps_pinned_vertices_under_aggressive_threshold() {
        let target = p(50, 1);
        let ring = vec![
            p(0, 0),
            p(25, 0),
            target,
            p(75, 0),
            p(100, 0),
            p(100, 100),
            p(0, 100),
        ];
        let mut plain = vec![ring.clone()];
        let mut pinned = vec![ring];
        let mut flags = PinnedRings::default();
        flags.fill_for_shape(&pinned, |_idx, ring, pins| {
            pins.resize(ring.len(), false);
            pins[2] = true;
        });
        let mut scratch = IntEmitScratch::new();
        simplify_shape_vw(&mut scratch, &mut plain, area2x(10_000), None);
        simplify_shape_vw(&mut scratch, &mut pinned, area2x(10_000), Some(&mut flags));
        assert!(!plain[0].contains(&target));
        assert!(pinned[0].contains(&target));
    }

    #[test]
    fn simplify_shape_vw_ring_floor_keeps_triangle() {
        let mut shape = vec![vec![p(0, 0), p(1, 0), p(100, 0), p(0, 100)]];
        simplify_shape_vw(&mut IntEmitScratch::new(), &mut shape, area2x(10_000), None);
        assert_eq!(shape[0].len(), 3);
    }

    #[test]
    fn simplify_shape_vw_drops_shape_when_outer_dies() {
        let mut shape = vec![
            vec![p(0, 0), p(1, 0), p(2, 0)],
            vec![p(10, 10), p(20, 10), p(20, 20), p(10, 20)],
        ];
        simplify_shape_vw(&mut IntEmitScratch::new(), &mut shape, area2x(16), None);
        assert!(shape.is_empty());
    }

    #[test]
    fn simplify_shape_vw_threshold_zero_is_verbatim() {
        let mut shape = vec![vec![p(5, 0), p(0, 0), p(0, 5), p(5, 5), p(5, 0)]];
        let original = shape.clone();
        simplify_shape_vw(&mut IntEmitScratch::new(), &mut shape, Area2x::ZERO, None);
        assert_eq!(shape, original);
    }

    /// One-sided coverage lost when `result` is used in place of `source`:
    /// `area(source) - area(source INTERSECT result)`, in 2x-pixel^2 units.
    fn one_sided_lost(source: &Contour, result: &Shape) -> i128 {
        let src_area = signed_area_2x(source).magnitude().get();
        if result.is_empty() {
            return src_area;
        }
        let source_shape = vec![source.clone()];
        let mut scratch = IntEmitScratch::new();
        let mut inter = Vec::new();
        intersect_shapes_into(&mut scratch, &source_shape, result, &mut inter);
        let inter_area: i128 = inter
            .iter()
            .map(|s| {
                s.iter()
                    .enumerate()
                    .map(|(i, r)| {
                        if i == 0 {
                            signed_area_2x(r).magnitude().get()
                        } else {
                            -signed_area_2x(r).magnitude().get()
                        }
                    })
                    .sum::<i128>()
            })
            .sum();
        (src_area - inter_area).max(0)
    }

    #[test]
    fn simplify_shape_vw_preserves_coverage_on_thin_fjord() {
        // A long, thin water finger: DP at tol 16 bounds only perpendicular
        // deviation, so the two near-coincident long sides flatten into a
        // chord and the whole feature is dropped, losing all its coverage.
        // VW bounds triangle area instead, so the finger's corner triangles
        // (base 400, height 2 -> area_2x 800) clear the 256 threshold and it
        // survives. This is the exact property the whole fix turns on.
        let fjord = vec![p(0, 0), p(400, 0), p(400, 2), p(0, 2)];

        let mut vw_shape = vec![fjord.clone()];
        simplify_shape_vw(
            &mut IntEmitScratch::new(),
            &mut vw_shape,
            OCEAN_VW_AREA_2X,
            None,
        );
        let vw_lost = one_sided_lost(&fjord, &vw_shape);

        let mut dp_shape = vec![fjord.clone()];
        simplify_shape_dp(
            &mut IntEmitScratch::new(),
            &mut dp_shape,
            tol(OCEAN_DP_TOL_PX),
            None,
        );
        let dp_lost = one_sided_lost(&fjord, &dp_shape);

        assert!(vw_lost <= 8, "VW lost {vw_lost} of the thin finger");
        assert!(
            dp_lost > 100 * vw_lost.max(1),
            "DP lost {dp_lost}, VW lost {vw_lost}; VW should preserve coverage"
        );
    }

    #[test]
    fn normalize_figure_8_input_to_two_simple_polygons() {
        let shape = vec![vec![p(0, 0), p(20, 20), p(0, 20), p(20, 0)]];
        let shapes = normalize(shape, Area::ZERO);
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
        let shapes = normalize(square_shape(0, 10), Area::from_u64(256));
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
            Area::ZERO,
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
            Area::ZERO,
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
            Area::ZERO,
        );
        assert!(!out.is_empty());
        assert!(out.iter().all(|shape| {
            !point_in_contour(5, 2, &shape[0])
                || shape[1..].iter().any(|hole| point_in_contour(5, 2, hole))
        }));
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
            Area::ZERO,
        );
        assert_eq!(out.len(), 1);
        assert_winding(&out[0]);
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
        let fixed = normalize(shape_z, Area::ZERO);
        assert!(!fixed.is_empty());
        let bbox = shape_bbox(&fixed[0]).expect("normalized shifted bbox");
        let clipped = intersect_rect(&fixed[0], bbox, Area::ZERO);
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
        let mut flags = PinnedRings::default();
        flags.fill_for_shape(&pinned, |_idx, ring, pins| {
            pins.resize(ring.len(), false);
            pins[2] = true;
        });
        let mut scratch = IntEmitScratch::new();
        simplify_shape_dp(&mut scratch, &mut plain, tol(16), None);
        simplify_shape_dp(&mut scratch, &mut pinned, tol(16), Some(&mut flags));
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
        // The skip is in ocean/emit.rs push_quantized_pieces; its observable
        // contract is verbatim passthrough - assert the geometric identity
        // the skip relies on: intersect of a contained shape equals itself
        // up to ring rotation, so passthrough is legal.
        let out = intersect_rect(&shape, data, Area::ZERO);
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
