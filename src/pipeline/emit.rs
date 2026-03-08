use smallvec::SmallVec;
use rustc_hash::FxHashSet;

use crate::geometry::{self, ClipRect, MercBbox, Point, BUFFER_FRACTION, close_and_orient_cw, close_and_orient_ccw, merc_bbox};
use crate::multipolygon::MemberWay;
use crate::mvt::{self, GeomType};
use crate::pmtiles_writer;
use crate::shortbread::{AttrValue, GeomExpect, Layer, LayerMatch};
use crate::sort::{self, SortRecord};
use crate::wire_format::{encode_attrs_bytes, encode_feature_data_with_attrs};
use rustc_hash::FxHashMap;

use super::stats::DeferralStats;

/// Full-tile rectangle in tile coordinates (CW, closed). Buffer = 8 rendered pixels = 128 extent units.
/// Used for interior tiles where the polygon fully covers the tile.
pub(super) const INTERIOR_TILE_RING: [(i32, i32); 5] =
    [(-128, -128), (4224, -128), (4224, 4224), (-128, 4224), (-128, -128)];

// ---------------------------------------------------------------------------
// Antimeridian helpers (shared by phase12 and relations)
// ---------------------------------------------------------------------------

#[inline]
pub(super) fn wrap_unit_x(x: f64) -> f64 {
    x.rem_euclid(1.0)
}

pub(super) fn unwrap_antimeridian_path(points: &mut [Point], closed: bool) -> bool {
    if points.len() < 2 {
        return false;
    }
    let mut changed = false;
    for i in 1..points.len() {
        let prev = points[i - 1].x;
        let mut x = points[i].x;
        while x - prev > 0.5 {
            x -= 1.0;
            changed = true;
        }
        while prev - x > 0.5 {
            x += 1.0;
            changed = true;
        }
        points[i].x = x;
    }
    if closed {
        let last = points.len() - 1;
        points[last].x = points[0].x;
        points[last].y = points[0].y;
    }
    changed
}

pub(super) fn antimeridian_shifts_for_bbox(bbox: &MercBbox) -> SmallVec<[f64; 3]> {
    let mut shifts = SmallVec::new();
    shifts.push(0.0);
    if bbox.min_x < 0.0 {
        shifts.push(1.0);
    }
    if bbox.max_x > 1.0 {
        shifts.push(-1.0);
    }
    shifts
}

pub(super) fn mark_bbox_wrapped(mask: &geometry::LandMask, bbox: &MercBbox) {
    for shift in antimeridian_shifts_for_bbox(bbox) {
        let shifted = MercBbox {
            min_x: bbox.min_x + shift,
            max_x: bbox.max_x + shift,
            min_y: bbox.min_y,
            max_y: bbox.max_y,
        };
        mask.mark_bbox(&shifted);
    }
}

// ---------------------------------------------------------------------------
// Shared vertex helpers (used by both phase12 and relations)
// ---------------------------------------------------------------------------

#[inline]
pub(super) fn merc_point_key(p: &Point) -> (i64, i64) {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let x = (p.x * 1_000_000_000_000.0).round() as i64;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y = (p.y * 1_000_000_000_000.0).round() as i64;
    (x, y)
}

pub(super) fn relation_shared_vertex_keys(member_ways: &[MemberWay]) -> FxHashSet<(i64, i64)> {
    let mut counts: FxHashMap<(i64, i64), u8> = FxHashMap::default();
    let mut local: FxHashSet<(i64, i64)> = FxHashSet::default();
    for mw in member_ways {
        local.clear();
        if mw.coords.len() < 2 {
            continue;
        }
        let is_closed = mw.coords.len() >= 4
            && merc_point_key(&mw.coords[0]) == merc_point_key(&mw.coords[mw.coords.len() - 1]);
        let end = if is_closed {
            mw.coords.len().saturating_sub(1)
        } else {
            mw.coords.len()
        };
        for p in &mw.coords[..end] {
            local.insert(merc_point_key(p));
        }
        for key in &local {
            counts
                .entry(*key)
                .and_modify(|c| *c = c.saturating_add(1))
                .or_insert(1);
        }
    }
    counts
        .into_iter()
        .filter_map(|(k, c)| (c >= 2).then_some(k))
        .collect()
}

// ---------------------------------------------------------------------------
// Polygon enrichment (area-dependent attrs + min_zoom overrides)
// ---------------------------------------------------------------------------

/// Enrich polygon matches with geometry-dependent data.
/// - BoundaryLabels: `way_area` in hectares, min_zoom override based on area thresholds.
pub(super) fn enrich_polygon_matches(matches: &mut [LayerMatch], area_m2: f64) {
    for m in matches.iter_mut() {
        if m.layer == Layer::BoundaryLabels {
            // Add way_area in hectares
            let hectares = area_m2 / 10_000.0;
            m.attrs.push(("way_area", AttrValue::Float(hectares), 0));

            // Override min_zoom based on area (Planetiler thresholds)
            let area_km2 = area_m2 / 1e6;
            let admin_level = m.attrs.iter()
                .find(|(k, _, _)| *k == "admin_level")
                .and_then(|(_, v, _)| if let AttrValue::Int(n) = v { Some(*n) } else { None })
                .unwrap_or(0);

            if admin_level == 2 && area_km2 >= 2_000_000.0 {
                m.min_zoom = 2;
            } else if area_km2 >= 700_000.0 {
                m.min_zoom = 3;
            } else if area_km2 >= 100_000.0 {
                m.min_zoom = 4;
            }
            // else stays at 5 (default from match_boundary_labels)
        }
    }
}

// ---------------------------------------------------------------------------
// Geometry validation helpers
// ---------------------------------------------------------------------------

pub(super) fn orient2d(a: (i32, i32), b: (i32, i32), c: (i32, i32)) -> i64 {
    let abx = i64::from(b.0) - i64::from(a.0);
    let aby = i64::from(b.1) - i64::from(a.1);
    let acx = i64::from(c.0) - i64::from(a.0);
    let acy = i64::from(c.1) - i64::from(a.1);
    abx * acy - aby * acx
}

fn on_segment(a: (i32, i32), b: (i32, i32), p: (i32, i32)) -> bool {
    let (min_x, max_x) = if a.0 <= b.0 { (a.0, b.0) } else { (b.0, a.0) };
    let (min_y, max_y) = if a.1 <= b.1 { (a.1, b.1) } else { (b.1, a.1) };
    p.0 >= min_x && p.0 <= max_x && p.1 >= min_y && p.1 <= max_y
}

pub(super) fn segments_intersect(a1: (i32, i32), a2: (i32, i32), b1: (i32, i32), b2: (i32, i32)) -> bool {
    let o1 = orient2d(a1, a2, b1);
    let o2 = orient2d(a1, a2, b2);
    let o3 = orient2d(b1, b2, a1);
    let o4 = orient2d(b1, b2, a2);

    if o1 == 0 && on_segment(a1, a2, b1) {
        return true;
    }
    if o2 == 0 && on_segment(a1, a2, b2) {
        return true;
    }
    if o3 == 0 && on_segment(b1, b2, a1) {
        return true;
    }
    if o4 == 0 && on_segment(b1, b2, a2) {
        return true;
    }
    (o1 > 0) != (o2 > 0) && (o3 > 0) != (o4 > 0)
}

pub(super) fn is_valid_simple_tile_ring(ring: &[(i32, i32)]) -> bool {
    if ring.len() < 4 || ring.first() != ring.last() {
        return false;
    }
    let edge_count = ring.len() - 1;
    for i in 0..edge_count {
        let a1 = ring[i];
        let a2 = ring[i + 1];
        for j in (i + 1)..edge_count {
            if j == i || j == i + 1 || (i == 0 && j == edge_count - 1) {
                continue;
            }
            let b1 = ring[j];
            let b2 = ring[j + 1];
            if segments_intersect(a1, a2, b1, b2) {
                return false;
            }
        }
    }
    true
}

fn orient2d_f64(a: &Point, b: &Point, c: &Point) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

fn on_segment_f64(a: &Point, b: &Point, p: &Point) -> bool {
    let min_x = a.x.min(b.x);
    let max_x = a.x.max(b.x);
    let min_y = a.y.min(b.y);
    let max_y = a.y.max(b.y);
    p.x >= min_x && p.x <= max_x && p.y >= min_y && p.y <= max_y
}

fn segments_intersect_f64(a1: &Point, a2: &Point, b1: &Point, b2: &Point) -> bool {
    let o1 = orient2d_f64(a1, a2, b1);
    let o2 = orient2d_f64(a1, a2, b2);
    let o3 = orient2d_f64(b1, b2, a1);
    let o4 = orient2d_f64(b1, b2, a2);
    let eps = 1e-15;

    if o1.abs() <= eps && on_segment_f64(a1, a2, b1) {
        return true;
    }
    if o2.abs() <= eps && on_segment_f64(a1, a2, b2) {
        return true;
    }
    if o3.abs() <= eps && on_segment_f64(b1, b2, a1) {
        return true;
    }
    if o4.abs() <= eps && on_segment_f64(b1, b2, a2) {
        return true;
    }
    (o1 > eps) != (o2 > eps) && (o3 > eps) != (o4 > eps)
}

pub(super) fn is_valid_simple_ring_points(ring: &[Point]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    // Accept both open rings [A,B,C] and closed rings [A,B,C,A].
    let is_closed = if ring.len() >= 4 {
        let first = &ring[0];
        let last = &ring[ring.len() - 1];
        (first.x - last.x).abs() < 1e-15 && (first.y - last.y).abs() < 1e-15
    } else {
        false
    };
    let n = if is_closed { ring.len() - 1 } else { ring.len() };
    if n < 3 {
        return false;
    }

    for i in 0..n {
        let a1 = &ring[i];
        let a2 = &ring[(i + 1) % n];
        for j in (i + 1)..n {
            if j == i || j == (i + 1) % n || (i == 0 && j == n - 1) {
                continue;
            }
            let b1 = &ring[j];
            let b2 = &ring[(j + 1) % n];
            if segments_intersect_f64(a1, a2, b1, b2) {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Scratch types for geometry emission
// ---------------------------------------------------------------------------

pub(super) struct PointEmitScratch {
    pub(super) geom_buf: Vec<u32>,
    pub(super) attrs_buf: Vec<u8>,
}

impl PointEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            geom_buf: Vec::new(),
            attrs_buf: Vec::new(),
        }
    }
}

pub(super) struct LineEmitScratch {
    pub(super) geom_buf: Vec<u32>,
    pub(super) attrs_buf: Vec<u8>,
    pub(super) tc_buf: Vec<(i32, i32)>,
    pub(super) simplify_keep: Vec<bool>,
    pub(super) simplify_buf: Vec<Point>,
    pub(super) pinned_idxs: Vec<usize>,
}

impl LineEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            geom_buf: Vec::new(),
            attrs_buf: Vec::new(),
            tc_buf: Vec::new(),
            simplify_keep: Vec::new(),
            simplify_buf: Vec::new(),
            pinned_idxs: Vec::new(),
        }
    }
}

pub(super) struct PolygonEmitScratch {
    pub(super) geom_buf: Vec<u32>,
    pub(super) attrs_buf: Vec<u8>,
    pub(super) tc_buf: Vec<(i32, i32)>,
    pub(super) simplify_keep: Vec<bool>,
    pub(super) simplify_buf: Vec<Point>,
    pub(super) pinned_idxs: Vec<usize>,
    pub(super) clip_a: Vec<Point>,
    pub(super) clip_b: Vec<Point>,
    pub(super) row_clip_a: Vec<Point>,
    pub(super) row_clip_b: Vec<Point>,
    /// Cap events: (layer_zoom_idx as u16, bbox_tiles as u64, osm_id as u64).
    pub(super) cap_events: Vec<(u16, u64, u64)>,
}

impl PolygonEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            geom_buf: Vec::new(),
            attrs_buf: Vec::new(),
            tc_buf: Vec::new(),
            simplify_keep: Vec::new(),
            simplify_buf: Vec::new(),
            pinned_idxs: Vec::new(),
            clip_a: Vec::new(),
            clip_b: Vec::new(),
            row_clip_a: Vec::new(),
            row_clip_b: Vec::new(),
            cap_events: Vec::new(),
        }
    }
}

pub(super) struct MultipolygonEmitScratch {
    pub(super) geom_buf: Vec<u32>,
    pub(super) attrs_buf: Vec<u8>,
    pub(super) required_idxs: Vec<usize>,
    pub(super) clip_a: Vec<Point>,
    pub(super) clip_b: Vec<Point>,
    pub(super) all_rings: Vec<Vec<(i32, i32)>>,
    pub(super) inner_bboxes: Vec<geometry::MercBbox>,
    pub(super) row_clip_a: Vec<Point>,
    pub(super) row_clip_b: Vec<Point>,
    pub(super) row_outer: Vec<Point>,
    pub(super) row_inners: Vec<Vec<Point>>,
    pub(super) row_inner_bboxes: Vec<geometry::MercBbox>,
    /// Cap events: (layer_zoom_idx as u16, bbox_tiles as u64, osm_id as u64).
    pub(super) cap_events: Vec<(u16, u64, u64)>,
}

impl MultipolygonEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            geom_buf: Vec::new(),
            attrs_buf: Vec::new(),
            required_idxs: Vec::new(),
            clip_a: Vec::new(),
            clip_b: Vec::new(),
            all_rings: Vec::new(),
            inner_bboxes: Vec::new(),
            row_clip_a: Vec::new(),
            row_clip_b: Vec::new(),
            row_outer: Vec::new(),
            row_inners: Vec::new(),
            row_inner_bboxes: Vec::new(),
            cap_events: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Sort record push helper
// ---------------------------------------------------------------------------

/// Push a single encoded feature into the sort record buffer.
/// Shared by all geometry emitters (point, line, polygon, multipolygon).
#[inline]
pub(super) fn push_sort_record(
    tile_id: u64,
    osm_id: u64,
    layer: Layer,
    geom_type: GeomType,
    geom_buf: &[u32],
    attrs_buf: &[u8],
    records: &mut Vec<SortRecord>,
) {
    let data = encode_feature_data_with_attrs(osm_id, geom_type, geom_buf, attrs_buf);
    let key = sort::make_sort_key(tile_id, layer as u8, 0);
    records.push(SortRecord { key, data });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(super) fn centroid_of(points: &[Point]) -> Point {
    if points.is_empty() {
        return Point { x: 0.0, y: 0.0 };
    }
    let mut sx = 0.0;
    let mut sy = 0.0;
    for p in points {
        sx += p.x;
        sy += p.y;
    }
    #[allow(clippy::cast_precision_loss)]
    let n = points.len() as f64;
    Point { x: sx / n, y: sy / n }
}

// ---------------------------------------------------------------------------
// Feature emission helpers
// ---------------------------------------------------------------------------

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_point_or_centroid(
    osm_id: u64,
    coords: &[Point],
    inners: Option<&[Vec<Point>]>,
    _bbox: &MercBbox,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
    scratch: &mut PointEmitScratch,
) -> u64 {
    if coords.is_empty() {
        return 0;
    }
    let pt = if m.geom_expect == GeomExpect::PolygonPointOnSurface {
        if let Some(holes) = inners {
            geometry::point_on_surface_with_holes(coords, holes)
        } else {
            geometry::point_on_surface(coords)
        }
    } else {
        Some(centroid_of(coords))
    };
    let Some(mut p) = pt else { return 0 };
    p.x = wrap_unit_x(p.x);

    let cbbox = MercBbox { min_x: p.x, min_y: p.y, max_x: p.x, max_y: p.y };
    let mut count: u64 = 0;
    for z in z_lo..=z_hi {
        encode_attrs_bytes(&mut scratch.attrs_buf, &m.attrs, z);
        geometry::for_each_tile_in_bbox(&cbbox, z, |tx, ty| {
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
            mvt::encode_point(&mut scratch.geom_buf, px, py);
            push_sort_record(tile_id, osm_id, m.layer, GeomType::Point, &scratch.geom_buf, &scratch.attrs_buf, records);
            count += 1;
        });
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_line_feature(
    osm_id: u64,
    merc: &[Point],
    preserve_vertex_mask: &[bool],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
    scratch: &mut LineEmitScratch,
) -> u64 {
    let mut count: u64 = 0;
    scratch.pinned_idxs.clear();
    scratch.pinned_idxs.extend(
        preserve_vertex_mask
            .iter()
            .enumerate()
            .filter_map(|(i, &keep)| keep.then_some(i)),
    );
    let has_pins = !scratch.pinned_idxs.is_empty();
    let mut run_for_zoom = |z: u8, simplified: &[Point]| {
        encode_attrs_bytes(&mut scratch.attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simplified);
        let single_tile = geometry::is_single_tile(&simp_bbox, z);

        // Skip min-size filtering at max zoom and for boundaries/streets
        let skip_size_filter = z >= 14
            || m.layer == Layer::Boundaries
            || m.layer == Layer::Streets;
        geometry::for_each_tile_in_bbox(&simp_bbox, z, |tx, ty| {
            if single_tile {
                // Fast path: bbox fits in one tile — clipping is a no-op.
                if simplified.len() < 2 {
                    return;
                }
                geometry::to_tile_coords_into(&mut scratch.tc_buf, simplified, tx, ty, z);
                if !skip_size_filter && geometry::line_is_subpixel(&scratch.tc_buf) {
                    return;
                }
                mvt::encode_linestring(&mut scratch.geom_buf, &scratch.tc_buf);
                if scratch.geom_buf.is_empty() {
                    return;
                }
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                push_sort_record(tile_id, osm_id, m.layer, GeomType::LineString, &scratch.geom_buf, &scratch.attrs_buf, records);
                count += 1;
            } else {
                let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
                geometry::for_each_clipped_segment(simplified, &clip, |segment| {
                    if segment.len() < 2 {
                        return;
                    }
                    geometry::to_tile_coords_into(&mut scratch.tc_buf, segment, tx, ty, z);
                    if !skip_size_filter && geometry::line_is_subpixel(&scratch.tc_buf) {
                        return;
                    }
                    mvt::encode_linestring(&mut scratch.geom_buf, &scratch.tc_buf);
                    if scratch.geom_buf.is_empty() {
                        return;
                    }
                    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                    push_sort_record(tile_id, osm_id, m.layer, GeomType::LineString, &scratch.geom_buf, &scratch.attrs_buf, records);
                    count += 1;
                });
            }
        });
    };

    if has_pins {
        for z in (z_lo..=z_hi).rev() {
            if z < 14 && geometry::merc_bbox_is_subpixel(merc, z) {
                break;
            }
            if z < 14 {
                let tol = geometry::simplify_tolerance(z);
                let _ = geometry::simplify_into_with_required(
                    merc,
                    tol,
                    &scratch.pinned_idxs,
                    &mut scratch.simplify_keep,
                    &mut scratch.simplify_buf,
                );
                if scratch.simplify_buf.len() < 2 {
                    break;
                }
                run_for_zoom(z, &scratch.simplify_buf);
            } else {
                run_for_zoom(z, merc);
            }
        }
    } else {
        geometry::for_each_zoom_simplified(merc, z_lo, z_hi, 2, 1.0, |z, simplified| {
            run_for_zoom(z, simplified);
        });
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
pub(super) fn emit_polygon_feature(
    osm_id: u64,
    merc: &[Point],
    preserve_vertex_mask: &[bool],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
    scratch: &mut PolygonEmitScratch,
    seam_max_zoom: u8,
    deferral_stats: Option<&DeferralStats>,
    fanout_cap: u32,
    tol_scale: f64,
) -> u64 {
    // Auto-disable check: if deferral was killed for this layer, skip it.
    let seam_max_zoom = if seam_max_zoom > 0
        && deferral_stats.is_some_and(|d| d.is_disabled(m.layer as u8))
    {
        0
    } else {
        seam_max_zoom
    };
    // Single-ring polygon (no holes)
    let mut count: u64 = 0;
    scratch.pinned_idxs.clear();
    scratch.pinned_idxs.extend(
        preserve_vertex_mask
            .iter()
            .enumerate()
            .filter_map(|(i, &keep)| keep.then_some(i)),
    );
    let has_pins = !scratch.pinned_idxs.is_empty();
    scratch.cap_events.clear();
    let mut run_for_zoom = |z: u8, simplified: &[Point]| {
        encode_attrs_bytes(&mut scratch.attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simplified);
        let single_tile = geometry::is_single_tile(&simp_bbox, z);
        let skip_size_filter = z >= 14;
        let (tx_min, tx_max, ty_min, ty_max) = geometry::tile_range_in_bbox(&simp_bbox, z);

        // Fanout cap: skip this zoom if the bbox tile count exceeds the layer cap.
        if fanout_cap > 0 {
            let nx = (tx_max - tx_min + 1) as u64;
            let ny = (ty_max - ty_min + 1) as u64;
            let bbox_tiles = nx * ny;
            if bbox_tiles > u64::from(fanout_cap) {
                #[allow(clippy::cast_possible_truncation)]
                scratch.cap_events.push(((m.layer as usize * 15 + z as usize) as u16, bbox_tiles, osm_id));
                return;
            }
        }

        if single_tile {
            // Fast path: bbox fits in one tile — clipping is a no-op.
            let (tx, ty) = (tx_min, ty_min);
            if simplified.len() < 3 {
                return;
            }
            if z < 14 && !is_valid_simple_ring_points(simplified) {
                return;
            }
            geometry::to_tile_coords_into(&mut scratch.tc_buf, simplified, tx, ty, z);
            if !skip_size_filter && geometry::ring_is_subpixel(&scratch.tc_buf) {
                return;
            }
            close_and_orient_cw(&mut scratch.tc_buf);
            if z < 14 && !is_valid_simple_tile_ring(&scratch.tc_buf) {
                return;
            }

            mvt::encode_polygon(&mut scratch.geom_buf, &[&scratch.tc_buf]);
            if scratch.geom_buf.is_empty() {
                return;
            }
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, &scratch.geom_buf, &scratch.attrs_buf, records);
            count += 1;
        } else {
            let inv_z = 1.0 / f64::from(1u32 << z);
            let tile_buf = BUFFER_FRACTION * inv_z;
            let multi_row = ty_max > ty_min;

            for ty in ty_min..=ty_max {
                // F14: row pre-clip — restrict polygon to this row's Y-band.
                // Per-tile clips then process far fewer vertices.
                let row_source: &[Point] = if multi_row {
                    let row_rect = ClipRect::new(
                        0.0,
                        f64::from(ty) * inv_z - tile_buf,
                        1.0,
                        f64::from(ty + 1) * inv_z + tile_buf,
                    );
                    geometry::clip_polygon_into(
                        simplified, &row_rect, &mut scratch.row_clip_a, &mut scratch.row_clip_b,
                    );
                    if scratch.row_clip_a.len() < 3 {
                        continue;
                    }
                    &scratch.row_clip_a
                } else {
                    simplified
                };

                for tx in tx_min..=tx_max {
                    let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
                    if geometry::tile_is_interior(row_source, &clip) {
                        scratch.tc_buf.clear();
                        scratch.tc_buf.extend_from_slice(&INTERIOR_TILE_RING);
                    } else {
                        geometry::clip_polygon_into(
                            row_source, &clip, &mut scratch.clip_a, &mut scratch.clip_b,
                        );
                        if scratch.clip_a.len() < 3 {
                            continue;
                        }
                        if z < 14 && !is_valid_simple_ring_points(&scratch.clip_a) {
                            continue;
                        }
                        geometry::to_tile_coords_into(&mut scratch.tc_buf, &scratch.clip_a, tx, ty, z);
                        if !skip_size_filter && geometry::ring_is_subpixel(&scratch.tc_buf) {
                            continue;
                        }
                        close_and_orient_cw(&mut scratch.tc_buf);
                        if z < 14 && !is_valid_simple_tile_ring(&scratch.tc_buf) {
                            continue;
                        }
                    }

                    mvt::encode_polygon(&mut scratch.geom_buf, &[&scratch.tc_buf]);
                    if scratch.geom_buf.is_empty() {
                        continue;
                    }
                    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                    push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, &scratch.geom_buf, &scratch.attrs_buf, records);
                    count += 1;
                }
            }
        }
    };
    if seam_max_zoom > 0 {
        // Seam-reconcile mode: single zoom loop.
        // z <= seam_max_zoom: emit full-res geometry (assemble-phase reconciliation + tile-coord DP).
        // z (seam_max_zoom+1)..13: normal DP simplification.
        // z >= 14: full-res (existing behavior, no simplification).
        for z in (z_lo..=z_hi).rev() {
            if z < 14 && geometry::merc_bbox_is_subpixel(merc, z) {
                break;
            }
            if z <= seam_max_zoom || z >= 14 {
                if z <= seam_max_zoom
                    && let Some(ds) = deferral_stats {
                        ds.record(m.layer as u8, merc.len() as u64);
                    }
                run_for_zoom(z, merc);
            } else {
                let tol = geometry::simplify_tolerance(z) * tol_scale;
                if has_pins {
                    let _ = geometry::simplify_into_with_required(
                        merc,
                        tol,
                        &scratch.pinned_idxs,
                        &mut scratch.simplify_keep,
                        &mut scratch.simplify_buf,
                    );
                } else {
                    let _ = geometry::simplify_into(
                        merc,
                        tol,
                        &mut scratch.simplify_keep,
                        &mut scratch.simplify_buf,
                    );
                }
                if scratch.simplify_buf.len() < 4 {
                    break;
                }
                run_for_zoom(z, &scratch.simplify_buf);
            }
        }
    } else if has_pins {
        for z in (z_lo..=z_hi).rev() {
            if z < 14 && geometry::merc_bbox_is_subpixel(merc, z) {
                break;
            }
            if z < 14 {
                let tol = geometry::simplify_tolerance(z) * tol_scale;
                let _ = geometry::simplify_into_with_required(
                    merc,
                    tol,
                    &scratch.pinned_idxs,
                    &mut scratch.simplify_keep,
                    &mut scratch.simplify_buf,
                );
                if scratch.simplify_buf.len() < 4 {
                    break;
                }
                run_for_zoom(z, &scratch.simplify_buf);
            } else {
                run_for_zoom(z, merc);
            }
        }
    } else {
        geometry::for_each_zoom_simplified(merc, z_lo, z_hi, 4, tol_scale, |z, simplified| {
            run_for_zoom(z, simplified);
        });
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::too_many_lines, clippy::cognitive_complexity)]
pub(super) fn emit_multipolygon_feature(
    osm_id: u64,
    outer: &[Point],
    inners: &[Vec<Point>],
    preserve_vertex_keys: Option<&FxHashSet<(i64, i64)>>,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut Vec<SortRecord>,
    emit_scratch: &mut MultipolygonEmitScratch,
    simp_scratch: &mut geometry::SimplifyMultiScratch,
    seam_max_zoom: u8,
    deferral_stats: Option<&DeferralStats>,
    fanout_cap: u32,
    tol_scale: f64,
) -> u64 {
    let mut count: u64 = 0;
    emit_scratch.cap_events.clear();
    let mut emit_for_zoom = |z: u8, simp_outer: &[Point], simp_inners: &[Vec<Point>]| {
        encode_attrs_bytes(&mut emit_scratch.attrs_buf, &m.attrs, z);

        // Recompute bbox from simplified coords — at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simp_outer);

        // F11: Precompute inner ring bboxes for O(1) tile rejection.
        emit_scratch.inner_bboxes.clear();
        emit_scratch.inner_bboxes.extend(simp_inners.iter().map(|r| merc_bbox(r)));
        let single_tile = geometry::is_single_tile(&simp_bbox, z);
        let skip_size_filter = z >= 14;
        let (tx_min, tx_max, ty_min, ty_max) = geometry::tile_range_in_bbox(&simp_bbox, z);

        // Fanout cap: skip this zoom if the bbox tile count exceeds the layer cap.
        if fanout_cap > 0 {
            let nx = (tx_max - tx_min + 1) as u64;
            let ny = (ty_max - ty_min + 1) as u64;
            let bbox_tiles = nx * ny;
            if bbox_tiles > u64::from(fanout_cap) {
                #[allow(clippy::cast_possible_truncation)]
                emit_scratch.cap_events.push(((m.layer as usize * 15 + z as usize) as u16, bbox_tiles, osm_id));
                return;
            }
        }

        if single_tile {
            // Fast path: bbox fits in one tile — clipping is a no-op.
            let (tx, ty) = (tx_min, ty_min);
            let mut ring_count: usize = 0;
            if simp_outer.len() < 3 {
                return;
            }
            if z < 14 && !is_valid_simple_ring_points(simp_outer) {
                return;
            }
            if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
            geometry::to_tile_coords_into(&mut emit_scratch.all_rings[ring_count], simp_outer, tx, ty, z);
            if !skip_size_filter && geometry::ring_is_subpixel(&emit_scratch.all_rings[ring_count]) {
                return;
            }
            close_and_orient_cw(&mut emit_scratch.all_rings[ring_count]);
            if z < 14 && !is_valid_simple_tile_ring(&emit_scratch.all_rings[ring_count]) {
                return;
            }
            ring_count += 1;
            for inner in simp_inners {
                if inner.len() < 3 {
                    continue;
                }
                if z < 14 && !is_valid_simple_ring_points(inner) {
                    continue;
                }
                if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
                geometry::to_tile_coords_into(&mut emit_scratch.all_rings[ring_count], inner, tx, ty, z);
                if !skip_size_filter && geometry::ring_is_subpixel(&emit_scratch.all_rings[ring_count]) {
                    continue;
                }
                close_and_orient_ccw(&mut emit_scratch.all_rings[ring_count]);
                if z < 14 && !is_valid_simple_tile_ring(&emit_scratch.all_rings[ring_count]) {
                    continue;
                }
                ring_count += 1;
            }

            let ring_refs: SmallVec<[&[(i32, i32)]; 4]> = emit_scratch.all_rings[..ring_count].iter().map(Vec::as_slice).collect();
            mvt::encode_polygon(&mut emit_scratch.geom_buf, &ring_refs);
            if emit_scratch.geom_buf.is_empty() {
                return;
            }
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, &emit_scratch.geom_buf, &emit_scratch.attrs_buf, records);
            count += 1;
        } else {
            let inv_z = 1.0 / f64::from(1u32 << z);
            let tile_buf = BUFFER_FRACTION * inv_z;
            let multi_row = ty_max > ty_min;

            for ty in ty_min..=ty_max {
                // F14: row pre-clip — restrict polygon to this row's Y-band.
                let (outer_src, inners_src, inners_bbox_src): (&[Point], &[Vec<Point>], &[geometry::MercBbox]) =
                    if multi_row {
                        let row_rect = ClipRect::new(
                            0.0,
                            f64::from(ty) * inv_z - tile_buf,
                            1.0,
                            f64::from(ty + 1) * inv_z + tile_buf,
                        );
                        // Pre-clip outer to row.
                        geometry::clip_polygon_into(
                            simp_outer, &row_rect, &mut emit_scratch.row_clip_a, &mut emit_scratch.row_clip_b,
                        );
                        if emit_scratch.row_clip_a.len() < 3 {
                            continue;
                        }
                        std::mem::swap(&mut emit_scratch.row_outer, &mut emit_scratch.row_clip_a);

                        // Pre-clip inners to row.
                        let row_y_min = f64::from(ty) * inv_z - tile_buf;
                        let row_y_max = f64::from(ty + 1) * inv_z + tile_buf;
                        let mut row_inner_count = 0;
                        for (inner, ib) in simp_inners.iter().zip(&emit_scratch.inner_bboxes) {
                            if ib.max_y < row_y_min || ib.min_y > row_y_max {
                                continue;
                            }
                            geometry::clip_polygon_into(
                                inner, &row_rect, &mut emit_scratch.row_clip_a, &mut emit_scratch.row_clip_b,
                            );
                            if emit_scratch.row_clip_a.len() < 3 {
                                continue;
                            }
                            if row_inner_count < emit_scratch.row_inners.len() {
                                std::mem::swap(
                                    &mut emit_scratch.row_inners[row_inner_count],
                                    &mut emit_scratch.row_clip_a,
                                );
                            } else {
                                emit_scratch.row_inners.push(Vec::new());
                                std::mem::swap(
                                    emit_scratch.row_inners.last_mut().expect("just pushed"),
                                    &mut emit_scratch.row_clip_a,
                                );
                            }
                            row_inner_count += 1;
                        }
                        emit_scratch.row_inner_bboxes.clear();
                        emit_scratch.row_inner_bboxes.extend(
                            emit_scratch.row_inners[..row_inner_count].iter().map(|r| merc_bbox(r))
                        );
                        (
                            &emit_scratch.row_outer,
                            &emit_scratch.row_inners[..row_inner_count],
                            &emit_scratch.row_inner_bboxes,
                        )
                    } else {
                        (simp_outer, simp_inners, emit_scratch.inner_bboxes.as_slice())
                    };

                for tx in tx_min..=tx_max {
                    let clip = ClipRect::for_tile(tx, ty, z, BUFFER_FRACTION);
                    let mut ring_count: usize = 0;

                    if geometry::tile_is_interior(outer_src, &clip) {
                        // Interior tile: outer ring covers entire tile.
                        if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
                        emit_scratch.all_rings[ring_count].clear();
                        emit_scratch.all_rings[ring_count].extend_from_slice(&INTERIOR_TILE_RING);
                        ring_count += 1;
                    } else {
                        geometry::clip_polygon_into(
                            outer_src, &clip, &mut emit_scratch.clip_a, &mut emit_scratch.clip_b,
                        );
                        if emit_scratch.clip_a.len() < 3 {
                            continue;
                        }
                        if z < 14 && !is_valid_simple_ring_points(&emit_scratch.clip_a) {
                            continue;
                        }
                        if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
                        geometry::to_tile_coords_into(&mut emit_scratch.all_rings[ring_count], &emit_scratch.clip_a, tx, ty, z);
                        if !skip_size_filter && geometry::ring_is_subpixel(&emit_scratch.all_rings[ring_count]) {
                            continue;
                        }
                        close_and_orient_cw(&mut emit_scratch.all_rings[ring_count]);
                        if z < 14 && !is_valid_simple_tile_ring(&emit_scratch.all_rings[ring_count]) {
                            continue;
                        }
                        ring_count += 1;
                    }
                    // Inner rings: still need per-tile clipping (holes may be visible).
                    for (inner, ib) in inners_src.iter().zip(inners_bbox_src) {
                        if !geometry::bbox_intersects_clip(ib, &clip) { continue; }
                        geometry::clip_polygon_into(
                            inner, &clip, &mut emit_scratch.clip_a, &mut emit_scratch.clip_b,
                        );
                        if emit_scratch.clip_a.len() < 3 {
                            continue;
                        }
                        if z < 14 && !is_valid_simple_ring_points(&emit_scratch.clip_a) {
                            continue;
                        }
                        if ring_count >= emit_scratch.all_rings.len() { emit_scratch.all_rings.push(Vec::new()); }
                        geometry::to_tile_coords_into(&mut emit_scratch.all_rings[ring_count], &emit_scratch.clip_a, tx, ty, z);
                        if !skip_size_filter && geometry::ring_is_subpixel(&emit_scratch.all_rings[ring_count]) {
                            continue;
                        }
                        close_and_orient_ccw(&mut emit_scratch.all_rings[ring_count]);
                        if z < 14 && !is_valid_simple_tile_ring(&emit_scratch.all_rings[ring_count]) {
                            continue;
                        }
                        ring_count += 1;
                    }

                    let ring_refs: SmallVec<[&[(i32, i32)]; 4]> =
                        emit_scratch.all_rings[..ring_count].iter().map(Vec::as_slice).collect();
                    mvt::encode_polygon(&mut emit_scratch.geom_buf, &ring_refs);
                    if emit_scratch.geom_buf.is_empty() {
                        continue;
                    }
                    let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                    push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, &emit_scratch.geom_buf, &emit_scratch.attrs_buf, records);
                    count += 1;
                }
            }
        }
    };
    let has_keys = preserve_vertex_keys.is_some_and(|k| !k.is_empty());
    // Auto-disable check.
    let seam_max_zoom = if seam_max_zoom > 0
        && deferral_stats.is_some_and(|d| d.is_disabled(m.layer as u8))
    {
        0
    } else {
        seam_max_zoom
    };
    if seam_max_zoom > 0 {
        // Seam-reconcile mode: single zoom loop.
        // z <= seam_max_zoom: emit full-res geometry (assemble-phase reconciliation + tile-coord DP).
        // z (seam_max_zoom+1)..13: normal DP simplification (with or without preserve_vertex_keys).
        // z >= 14: full-res (existing behavior, no simplification).
        simp_scratch.cascade_outer.clear();
        simp_scratch.cascade_outer.extend_from_slice(outer);
        simp_scratch.cascade_inners.clear();
        simp_scratch.cascade_inners.extend(inners.iter().cloned());

        for z in (z_lo..=z_hi).rev() {
            if z < 14 && geometry::merc_bbox_is_subpixel(&simp_scratch.cascade_outer, z) {
                break;
            }
            if z <= seam_max_zoom || z >= 14 {
                if z <= seam_max_zoom
                    && let Some(ds) = deferral_stats {
                        let verts = outer.len() as u64 + inners.iter().map(|r| r.len() as u64).sum::<u64>();
                        ds.record(m.layer as u8, verts);
                    }
                emit_for_zoom(z, outer, inners);
            } else {
                // z 9..13: DP simplification.
                let tol = geometry::simplify_tolerance(z) * tol_scale;
                if simp_scratch.cascade_outer.len() > 4 {
                    if has_keys {
                        let keys = preserve_vertex_keys.expect("checked above");
                        emit_scratch.required_idxs.clear();
                        let outer_end = simp_scratch.cascade_outer.len().saturating_sub(1);
                        for (i, p) in simp_scratch.cascade_outer.iter().take(outer_end).enumerate() {
                            if keys.contains(&merc_point_key(p)) {
                                emit_scratch.required_idxs.push(i);
                            }
                        }
                        let _ = geometry::simplify_into_with_required(
                            &simp_scratch.cascade_outer,
                            tol,
                            &emit_scratch.required_idxs,
                            &mut simp_scratch.keep_buf,
                            &mut simp_scratch.simp_buf,
                        );
                    } else {
                        let _ = geometry::simplify_into(
                            &simp_scratch.cascade_outer,
                            tol,
                            &mut simp_scratch.keep_buf,
                            &mut simp_scratch.simp_buf,
                        );
                    }
                    std::mem::swap(&mut simp_scratch.cascade_outer, &mut simp_scratch.simp_buf);
                }

                for inner in &mut simp_scratch.cascade_inners {
                    if inner.len() <= 4 {
                        continue;
                    }
                    if has_keys {
                        let keys = preserve_vertex_keys.expect("checked above");
                        emit_scratch.required_idxs.clear();
                        let inner_end = inner.len().saturating_sub(1);
                        for (i, p) in inner.iter().take(inner_end).enumerate() {
                            if keys.contains(&merc_point_key(p)) {
                                emit_scratch.required_idxs.push(i);
                            }
                        }
                        let _ = geometry::simplify_into_with_required(
                            inner,
                            tol,
                            &emit_scratch.required_idxs,
                            &mut simp_scratch.keep_buf,
                            &mut simp_scratch.simp_buf,
                        );
                    } else {
                        let _ = geometry::simplify_into(
                            inner,
                            tol,
                            &mut simp_scratch.keep_buf,
                            &mut simp_scratch.simp_buf,
                        );
                    }
                    std::mem::swap(inner, &mut simp_scratch.simp_buf);
                }
                simp_scratch.cascade_inners.retain(|r| r.len() >= 4);

                if simp_scratch.cascade_outer.len() < 4 {
                    break;
                }
                emit_for_zoom(z, &simp_scratch.cascade_outer, &simp_scratch.cascade_inners);
            }
        }
    } else if has_keys {
        let keys = preserve_vertex_keys.expect("checked above");
        simp_scratch.cascade_outer.clear();
        simp_scratch.cascade_outer.extend_from_slice(outer);
        simp_scratch.cascade_inners.clear();
        simp_scratch.cascade_inners.extend(inners.iter().cloned());

        for z in (z_lo..=z_hi).rev() {
            if z < 14 {
                if geometry::merc_bbox_is_subpixel(&simp_scratch.cascade_outer, z) {
                    break;
                }
                let tol = geometry::simplify_tolerance(z) * tol_scale;
                if simp_scratch.cascade_outer.len() > 4 {
                    emit_scratch.required_idxs.clear();
                    let outer_end = simp_scratch.cascade_outer.len().saturating_sub(1);
                    for (i, p) in simp_scratch.cascade_outer.iter().take(outer_end).enumerate() {
                        if keys.contains(&merc_point_key(p)) {
                            emit_scratch.required_idxs.push(i);
                        }
                    }
                    let _ = geometry::simplify_into_with_required(
                        &simp_scratch.cascade_outer,
                        tol,
                        &emit_scratch.required_idxs,
                        &mut simp_scratch.keep_buf,
                        &mut simp_scratch.simp_buf,
                    );
                    std::mem::swap(&mut simp_scratch.cascade_outer, &mut simp_scratch.simp_buf);
                }

                for inner in &mut simp_scratch.cascade_inners {
                    if inner.len() <= 4 {
                        continue;
                    }
                    emit_scratch.required_idxs.clear();
                    let inner_end = inner.len().saturating_sub(1);
                    for (i, p) in inner.iter().take(inner_end).enumerate() {
                        if keys.contains(&merc_point_key(p)) {
                            emit_scratch.required_idxs.push(i);
                        }
                    }
                    let _ = geometry::simplify_into_with_required(
                        inner,
                        tol,
                        &emit_scratch.required_idxs,
                        &mut simp_scratch.keep_buf,
                        &mut simp_scratch.simp_buf,
                    );
                    std::mem::swap(inner, &mut simp_scratch.simp_buf);
                }
                simp_scratch.cascade_inners.retain(|r| r.len() >= 4);
            }
            if simp_scratch.cascade_outer.len() < 4 {
                break;
            }
            emit_for_zoom(z, &simp_scratch.cascade_outer, &simp_scratch.cascade_inners);
        }
    } else {
        geometry::for_each_zoom_simplified_multi(outer, inners, z_lo, z_hi, simp_scratch, tol_scale, |z, simp_outer, simp_inners| {
            emit_for_zoom(z, simp_outer, simp_inners);
        });
    }
    count
}
