use smallvec::SmallVec;
use rustc_hash::FxHashSet;

use crate::geometry::{self, ClipRect, MercBbox, Point, BUFFER_FRACTION, merc_bbox};
use crate::geometry::int_ocean::{
    IntEmitScratch, OSM_DP_TOL_PX, Shape, ZoomEmitParams,
    buffered_tile_rect, contour_area_is_below, emit_shape_for_zoom, encode_tile_shape,
    intersect_rect, lookback_dedup_contour_pinned, normalize, quantize_polygon,
    quantize_polygon_pinned, rescale_shape, rescale_shape_pinned, ring_is_simple_complete,
    shape_bbox, simplify_shape_dp, tile_count_for_rect, tile_range_for_rect,
};
use crate::multipolygon::MemberWay;
use crate::mvt::{self, GeomType};
use crate::pmtiles_writer;
use crate::shortbread::{AttrValue, GeomExpect, Layer, LayerMatch};
use crate::sort::{self, SortRecord};
use crate::wire_format::{encode_attrs_bytes, encode_feature_data_with_attrs};
use rustc_hash::FxHashMap;

use super::stats::DeferralStats;

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
    pub(super) attrs_buf: Vec<u8>,
    pub(super) int_emit: IntEmitScratch,
    pub(super) pin_keys: FxHashSet<(i64, i64)>,
    pub(super) base_pins: FxHashSet<(i32, i32)>,
    /// Cap events: (layer_zoom_idx as u16, bbox_tiles as u64, osm_id as u64).
    pub(super) cap_events: Vec<(u16, u64, u64)>,
}

impl PolygonEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            attrs_buf: Vec::new(),
            int_emit: IntEmitScratch::new(),
            pin_keys: FxHashSet::default(),
            base_pins: FxHashSet::default(),
            cap_events: Vec::new(),
        }
    }
}

pub(super) struct MultipolygonEmitScratch {
    pub(super) attrs_buf: Vec<u8>,
    pub(super) int_emit: IntEmitScratch,
    pub(super) base_pins: FxHashSet<(i32, i32)>,
    /// Cap events: (layer_zoom_idx as u16, bbox_tiles as u64, osm_id as u64).
    pub(super) cap_events: Vec<(u16, u64, u64)>,
}

impl MultipolygonEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            attrs_buf: Vec::new(),
            int_emit: IntEmitScratch::new(),
            base_pins: FxHashSet::default(),
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

const OSM_POLYGON_MAX_Z: u8 = 14;
const OSM_TIER2_MAX_TILES: u64 = 64;

fn polygon_min_area(z: u8, layer: Layer) -> u64 {
    if z >= OSM_POLYGON_MAX_Z || layer == Layer::StreetPolygons || layer == Layer::Bridges {
        0
    } else {
        geometry::MIN_POLY_AREA as u64
    }
}

fn polygon_dp_tol(z: u8, seam_max_zoom: u8, tol_scale: f64) -> i64 {
    if z <= seam_max_zoom || z >= OSM_POLYGON_MAX_Z {
        0
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let tol = (OSM_DP_TOL_PX as f64 * tol_scale).round() as i64;
        tol.max(0)
    }
}

fn fill_pin_keys_from_mask(points: &[Point], preserve_vertex_mask: &[bool], out: &mut FxHashSet<(i64, i64)>) {
    out.clear();
    for (p, &keep) in points.iter().zip(preserve_vertex_mask) {
        if keep {
            out.insert(merc_point_key(p));
        }
    }
}

fn fill_base_pins(shape: &Shape, flags: &[Vec<bool>], out: &mut FxHashSet<(i32, i32)>) {
    out.clear();
    for (ring, ring_flags) in shape.iter().zip(flags) {
        for (p, &pinned) in ring.iter().zip(ring_flags) {
            if pinned {
                out.insert((p.x, p.y));
            }
        }
    }
}

fn prepare_shape_for_zoom(
    shape_base: &Shape,
    flags_base: &[Vec<bool>],
    z: u8,
    dp_tol: i64,
    int_scratch: &mut IntEmitScratch,
) {
    let shift = OSM_POLYGON_MAX_Z.saturating_sub(z);
    if flags_base.is_empty() {
        int_scratch.shape_z = rescale_shape(shape_base, shift);
        int_scratch.flags_z.clear();
        simplify_shape_dp(&mut int_scratch.shape_z, dp_tol, None);
    } else {
        let (shape_z, flags_z) = rescale_shape_pinned(shape_base, shift, flags_base);
        int_scratch.shape_z = shape_z;
        int_scratch.flags_z = flags_z;
        simplify_shape_dp(&mut int_scratch.shape_z, dp_tol, Some(&int_scratch.flags_z));
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_int_tile_shape(
    osm_id: u64,
    layer: Layer,
    attrs_buf: &[u8],
    z: u8,
    tx: u32,
    ty: u32,
    tile_shape: Shape,
    int_scratch: &mut IntEmitScratch,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let mut emitted = 0;
    let mut sink = |out_tx: u32, out_ty: u32, geom: &[u32]| {
        let tile_id = pmtiles_writer::xy_to_tile_id(z, out_tx, out_ty);
        push_sort_record(tile_id, osm_id, layer, GeomType::Polygon, geom, attrs_buf, records);
        emitted += 1;
    };
    encode_tile_shape(tile_shape, tx, ty, int_scratch, &mut sink);
    emitted
}

#[allow(clippy::too_many_arguments)]
fn emit_tier1_single_ring(
    osm_id: u64,
    layer: Layer,
    attrs_buf: &[u8],
    z: u8,
    tx: u32,
    ty: u32,
    mut shape_z: Shape,
    mut flags_z: Vec<Vec<bool>>,
    dp_tol: i64,
    min_area: u64,
    int_scratch: &mut IntEmitScratch,
    records: &mut Vec<SortRecord>,
) -> u64 {
    if shape_z.len() != 1 || shape_z[0].len() < 3 {
        return 0;
    }
    let flags = if flags_z.is_empty() {
        None
    } else {
        flags_z.get_mut(0)
    };
    lookback_dedup_contour_pinned(&mut shape_z[0], flags);
    if !flags_z.is_empty() {
        simplify_shape_dp(&mut shape_z, dp_tol, Some(&flags_z));
    } else {
        simplify_shape_dp(&mut shape_z, dp_tol, None);
    }
    if shape_z.len() != 1 || shape_z[0].len() < 3 {
        return 0;
    }
    if contour_area_is_below(&shape_z[0], min_area) {
        return 0;
    }
    if ring_is_simple_complete(&shape_z[0]) {
        return emit_int_tile_shape(osm_id, layer, attrs_buf, z, tx, ty, shape_z, int_scratch, records);
    }

    let mut emitted = 0;
    for fixed in normalize(shape_z, min_area) {
        emitted += emit_int_tile_shape(osm_id, layer, attrs_buf, z, tx, ty, fixed, int_scratch, records);
    }
    emitted
}

#[allow(clippy::too_many_arguments)]
fn emit_normalized_per_tile(
    osm_id: u64,
    layer: Layer,
    attrs_buf: &[u8],
    z: u8,
    shape_z: Shape,
    min_area: u64,
    int_scratch: &mut IntEmitScratch,
    records: &mut Vec<SortRecord>,
) -> u64 {
    let max_tile = (1u32 << z) - 1;
    let mut emitted = 0;
    for shape in normalize(shape_z, min_area) {
        let Some(bbox) = shape_bbox(&shape) else {
            continue;
        };
        let (tx_min, tx_max, ty_min, ty_max) = tile_range_for_rect(bbox, max_tile);
        for ty in ty_min..=ty_max {
            for tx in tx_min..=tx_max {
                for tile_shape in intersect_rect(&shape, buffered_tile_rect(tx, ty), min_area) {
                    emitted += emit_int_tile_shape(
                        osm_id,
                        layer,
                        attrs_buf,
                        z,
                        tx,
                        ty,
                        tile_shape,
                        int_scratch,
                        records,
                    );
                }
            }
        }
    }
    emitted
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

        // Recompute bbox from simplified coords - at low zooms DP may reduce the
        // geometry to far fewer tiles than the original bbox suggests.
        let simp_bbox = merc_bbox(simplified);
        let single_tile = geometry::is_single_tile(&simp_bbox, z);

        // Skip min-size filtering at max zoom and for boundaries/streets
        let skip_size_filter = z >= 14
            || m.layer == Layer::Boundaries
            || m.layer == Layer::Streets;
        geometry::for_each_tile_in_bbox(&simp_bbox, z, |tx, ty| {
            if single_tile {
                // Fast path: bbox fits in one tile - clipping is a no-op.
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

    // Short connecting ways (e.g. motorway junction links) are physically subpixel
    // at low zoom but must survive to maintain road network connectivity.
    let skip_bbox_check = m.layer == Layer::Boundaries || m.layer == Layer::Streets;
    if has_pins {
        for z in (z_lo..=z_hi).rev() {
            if z < 14 && !skip_bbox_check && geometry::merc_bbox_is_subpixel(merc, z) {
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
        geometry::for_each_zoom_simplified(merc, z_lo, z_hi, 2, |_| 1.0, skip_bbox_check, |z, simplified| {
            run_for_zoom(z, simplified);
        });
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    let seam_max_zoom = if seam_max_zoom > 0
        && deferral_stats.is_some_and(|d| d.is_disabled(m.layer as u8))
    {
        0
    } else {
        seam_max_zoom
    };

    scratch.cap_events.clear();
    if merc.len() < 4 {
        return 0;
    }
    fill_pin_keys_from_mask(merc, preserve_vertex_mask, &mut scratch.pin_keys);
    let has_pins = !scratch.pin_keys.is_empty();
    let (shape_base, flags_base) = if has_pins {
        quantize_polygon_pinned(merc, &[], OSM_POLYGON_MAX_Z, |p| {
            scratch.pin_keys.contains(&merc_point_key(p))
        })
    } else {
        (quantize_polygon(merc, &[], OSM_POLYGON_MAX_Z), Vec::new())
    };
    if shape_base.is_empty() {
        return 0;
    }
    fill_base_pins(&shape_base, &flags_base, &mut scratch.base_pins);

    let mut count: u64 = 0;
    let mut run_for_zoom = |z: u8| {
        encode_attrs_bytes(&mut scratch.attrs_buf, &m.attrs, z);
        let dp_tol = polygon_dp_tol(z, seam_max_zoom, tol_scale);
        let min_area = polygon_min_area(z, m.layer);
        let max_tile = (1u32 << z) - 1;

        prepare_shape_for_zoom(&shape_base, &flags_base, z, 0, &mut scratch.int_emit);
        let Some(pre_bbox) = shape_bbox(&scratch.int_emit.shape_z) else {
            return;
        };
        let (pre_tx_min, pre_tx_max, pre_ty_min, pre_ty_max) = tile_range_for_rect(pre_bbox, max_tile);
        if pre_tx_min == pre_tx_max && pre_ty_min == pre_ty_max && scratch.int_emit.shape_z.len() == 1 {
            let shape_z = std::mem::take(&mut scratch.int_emit.shape_z);
            let flags_z = std::mem::take(&mut scratch.int_emit.flags_z);
            count += emit_tier1_single_ring(
                osm_id,
                m.layer,
                &scratch.attrs_buf,
                z,
                pre_tx_min,
                pre_ty_min,
                shape_z,
                flags_z,
                dp_tol,
                min_area,
                &mut scratch.int_emit,
                records,
            );
            return;
        }

        if scratch.int_emit.flags_z.is_empty() {
            simplify_shape_dp(&mut scratch.int_emit.shape_z, dp_tol, None);
        } else {
            simplify_shape_dp(&mut scratch.int_emit.shape_z, dp_tol, Some(&scratch.int_emit.flags_z));
        }
        let Some(bbox) = shape_bbox(&scratch.int_emit.shape_z) else {
            return;
        };
        let bbox_tiles = tile_count_for_rect(bbox, max_tile);
        if fanout_cap > 0 && bbox_tiles > u64::from(fanout_cap) {
            #[allow(clippy::cast_possible_truncation)]
            scratch.cap_events.push(((m.layer as usize * 15 + z as usize) as u16, bbox_tiles, osm_id));
            return;
        }

        if bbox_tiles <= OSM_TIER2_MAX_TILES {
            let shape_z = std::mem::take(&mut scratch.int_emit.shape_z);
            count += emit_normalized_per_tile(
                osm_id,
                m.layer,
                &scratch.attrs_buf,
                z,
                shape_z,
                min_area,
                &mut scratch.int_emit,
                records,
            );
        } else {
            let pins = (!scratch.base_pins.is_empty()).then_some(&scratch.base_pins);
            let mut emitted = 0;
            let mut sink = |tx: u32, ty: u32, geom: &[u32]| {
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, geom, &scratch.attrs_buf, records);
                emitted += 1;
            };
            emit_shape_for_zoom(
                &shape_base,
                ZoomEmitParams {
                    z,
                    maxz: OSM_POLYGON_MAX_Z,
                    dp_tol,
                    min_area,
                    pins,
                },
                &mut scratch.int_emit,
                &mut sink,
            );
            count += emitted;
        }
    };

    for z in (z_lo..=z_hi).rev() {
        if z < OSM_POLYGON_MAX_Z && geometry::merc_bbox_is_subpixel(merc, z) {
            break;
        }
        if seam_max_zoom > 0 && z <= seam_max_zoom
            && let Some(ds) = deferral_stats
        {
            ds.record(m.layer as u8, merc.len() as u64);
        }
        run_for_zoom(z);
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
    _simp_scratch: &mut geometry::SimplifyMultiScratch,
    seam_max_zoom: u8,
    deferral_stats: Option<&DeferralStats>,
    fanout_cap: u32,
    tol_scale: f64,
) -> u64 {
    let seam_max_zoom = if seam_max_zoom > 0
        && deferral_stats.is_some_and(|d| d.is_disabled(m.layer as u8))
    {
        0
    } else {
        seam_max_zoom
    };
    let (shape_base, flags_base) = if let Some(keys) = preserve_vertex_keys.filter(|k| !k.is_empty()) {
        quantize_polygon_pinned(outer, inners, OSM_POLYGON_MAX_Z, |p| keys.contains(&merc_point_key(p)))
    } else {
        (quantize_polygon(outer, inners, OSM_POLYGON_MAX_Z), Vec::new())
    };
    if shape_base.is_empty() {
        return 0;
    }
    emit_scratch.cap_events.clear();
    fill_base_pins(&shape_base, &flags_base, &mut emit_scratch.base_pins);

    let mut count: u64 = 0;
    let mut run_for_zoom = |z: u8| {
        encode_attrs_bytes(&mut emit_scratch.attrs_buf, &m.attrs, z);
        let dp_tol = polygon_dp_tol(z, seam_max_zoom, tol_scale);
        let min_area = polygon_min_area(z, m.layer);
        let max_tile = (1u32 << z) - 1;

        prepare_shape_for_zoom(&shape_base, &flags_base, z, 0, &mut emit_scratch.int_emit);
        let Some(pre_bbox) = shape_bbox(&emit_scratch.int_emit.shape_z) else {
            return;
        };
        let (pre_tx_min, pre_tx_max, pre_ty_min, pre_ty_max) = tile_range_for_rect(pre_bbox, max_tile);
        if pre_tx_min == pre_tx_max && pre_ty_min == pre_ty_max {
            if emit_scratch.int_emit.shape_z.len() == 1 {
                let shape_z = std::mem::take(&mut emit_scratch.int_emit.shape_z);
                let flags_z = std::mem::take(&mut emit_scratch.int_emit.flags_z);
                count += emit_tier1_single_ring(
                    osm_id,
                    m.layer,
                    &emit_scratch.attrs_buf,
                    z,
                    pre_tx_min,
                    pre_ty_min,
                    shape_z,
                    flags_z,
                    dp_tol,
                    min_area,
                    &mut emit_scratch.int_emit,
                    records,
                );
                return;
            }
            if emit_scratch.int_emit.flags_z.is_empty() {
                simplify_shape_dp(&mut emit_scratch.int_emit.shape_z, dp_tol, None);
            } else {
                simplify_shape_dp(
                    &mut emit_scratch.int_emit.shape_z,
                    dp_tol,
                    Some(&emit_scratch.int_emit.flags_z),
                );
            }
            let shape_z = std::mem::take(&mut emit_scratch.int_emit.shape_z);
            count += emit_normalized_per_tile(
                osm_id,
                m.layer,
                &emit_scratch.attrs_buf,
                z,
                shape_z,
                min_area,
                &mut emit_scratch.int_emit,
                records,
            );
            return;
        }

        if emit_scratch.int_emit.flags_z.is_empty() {
            simplify_shape_dp(&mut emit_scratch.int_emit.shape_z, dp_tol, None);
        } else {
            simplify_shape_dp(
                &mut emit_scratch.int_emit.shape_z,
                dp_tol,
                Some(&emit_scratch.int_emit.flags_z),
            );
        }
        let Some(bbox) = shape_bbox(&emit_scratch.int_emit.shape_z) else {
            return;
        };
        let bbox_tiles = tile_count_for_rect(bbox, max_tile);
        if fanout_cap > 0 && bbox_tiles > u64::from(fanout_cap) {
            #[allow(clippy::cast_possible_truncation)]
            emit_scratch.cap_events.push(((m.layer as usize * 15 + z as usize) as u16, bbox_tiles, osm_id));
            return;
        }

        if bbox_tiles <= OSM_TIER2_MAX_TILES {
            let shape_z = std::mem::take(&mut emit_scratch.int_emit.shape_z);
            count += emit_normalized_per_tile(
                osm_id,
                m.layer,
                &emit_scratch.attrs_buf,
                z,
                shape_z,
                min_area,
                &mut emit_scratch.int_emit,
                records,
            );
        } else {
            let pins = (!emit_scratch.base_pins.is_empty()).then_some(&emit_scratch.base_pins);
            let mut emitted = 0;
            let mut sink = |tx: u32, ty: u32, geom: &[u32]| {
                let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
                push_sort_record(tile_id, osm_id, m.layer, GeomType::Polygon, geom, &emit_scratch.attrs_buf, records);
                emitted += 1;
            };
            emit_shape_for_zoom(
                &shape_base,
                ZoomEmitParams {
                    z,
                    maxz: OSM_POLYGON_MAX_Z,
                    dp_tol,
                    min_area,
                    pins,
                },
                &mut emit_scratch.int_emit,
                &mut sink,
            );
            count += emitted;
        }
    };

    for z in (z_lo..=z_hi).rev() {
        if z < OSM_POLYGON_MAX_Z && geometry::merc_bbox_is_subpixel(outer, z) {
            break;
        }
        if seam_max_zoom > 0 && z <= seam_max_zoom
            && let Some(ds) = deferral_stats
        {
            let verts = outer.len() as u64 + inners.iter().map(|r| r.len() as u64).sum::<u64>();
            ds.record(m.layer as u8, verts);
        }
        run_for_zoom(z);
    }
    count
}

#[cfg(test)]
mod landing_b_tests {
    use super::*;
    use i_overlay::i_float::int::point::IntPoint;

    fn p(x: i32, y: i32) -> IntPoint {
        IntPoint::new(x, y)
    }

    #[test]
    fn landing_b_tier1_and_tier2_identical_bytes_for_single_tile_shape() {
        let shape: Shape = normalize(
            vec![vec![p(100, 100), p(900, 100), p(900, 900), p(100, 900)]],
            0,
        )
        .pop()
        .expect("square normalizes");
        let shape = intersect_rect(&shape, buffered_tile_rect(0, 0), 0)
            .pop()
            .expect("square intersects tile");
        let mut tier1_records = Vec::new();
        let mut tier2_records = Vec::new();
        let mut scratch = IntEmitScratch::new();
        let attrs = Vec::new();

        let emitted_tier1 = emit_tier1_single_ring(
            777,
            Layer::Buildings,
            &attrs,
            0,
            0,
            0,
            shape.clone(),
            Vec::new(),
            0,
            0,
            &mut scratch,
            &mut tier1_records,
        );
        let emitted_tier2 = emit_normalized_per_tile(
            777,
            Layer::Buildings,
            &attrs,
            0,
            shape,
            0,
            &mut scratch,
            &mut tier2_records,
        );

        assert_eq!(emitted_tier1, 1);
        assert_eq!(emitted_tier2, 1);
        assert_eq!(
            tier1_records[0].data,
            tier2_records[0].data,
            "tier1={:?} tier2={:?}",
            tier1_records[0].data,
            tier2_records[0].data,
        );
    }

    #[test]
    fn landing_b_tier1_bowtie_escalates_to_simple_output() {
        let shape: Shape = vec![vec![p(0, 0), p(20, 20), p(0, 20), p(20, 0)]];
        let mut records = Vec::new();
        let mut scratch = IntEmitScratch::new();
        let attrs = Vec::new();

        let emitted = emit_tier1_single_ring(
            778,
            Layer::Buildings,
            &attrs,
            0,
            0,
            0,
            shape,
            Vec::new(),
            0,
            0,
            &mut scratch,
            &mut records,
        );

        assert!(emitted > 0);
        for rec in records {
            let cmd_count = u32::from_le_bytes(rec.data[9..13].try_into().expect("cmd count")) as usize;
            let geom_start = 13;
            let mut cmds = Vec::with_capacity(cmd_count);
            for idx in 0..cmd_count {
                let offset = geom_start + idx * 4;
                cmds.push(u32::from_le_bytes(
                    rec.data[offset..offset + 4].try_into().expect("u32 command"),
                ));
            }
            for ring in crate::geometry::decode_mvt_polygon(&cmds) {
                assert!(crate::geometry::ring_is_simple(&ring), "non-simple ring {ring:?}");
            }
        }
    }
}
