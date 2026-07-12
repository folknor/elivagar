use rustc_hash::FxHashSet;
use smallvec::SmallVec;

use crate::geometry::int_ocean::{
    IntRect, OSM_DP_TOL_PX, Shape, TILE_EXTENT_I32, quantize_polygon_into,
    quantize_polygon_pinned_into, shape_bbox,
};
use crate::geometry::pyramid::{PyramidParams, PyramidScratch, emit_shape_pyramid};
use crate::geometry::{self, BUFFER_FRACTION, ClipRect, MercBbox, Point, merc_bbox};
use crate::multipolygon::MemberWay;
use crate::mvt::{self, GeomType};
use crate::pmtiles_writer;
use crate::shortbread::{AttrValue, GeomExpect, Layer, LayerMatch};
use crate::sort::{self, SortRecord};
use crate::wire_format::{
    append_feature_data_with_attrs, encode_attrs_bytes, encode_feature_data_with_attrs,
};
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
            let admin_level = m
                .attrs
                .iter()
                .find(|(k, _, _)| *k == "admin_level")
                .and_then(|(_, v, _)| {
                    if let AttrValue::Int(n) = v {
                        Some(*n)
                    } else {
                        None
                    }
                })
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
    pub(super) pyramid: PyramidScratch,
    pub(super) attrs_by_zoom: [Option<Vec<u8>>; 15],
    pub(super) pin_keys: FxHashSet<(i64, i64)>,
    pub(super) base_pins: FxHashSet<(i32, i32)>,
    /// Cap events: (layer_zoom_idx as u16, bbox_tiles as u64, osm_id as u64).
    pub(super) cap_events: Vec<(u16, u64, u64)>,
    /// Quantized base shape + pin flags, ring Vecs recycled across features
    /// (H6: one fresh Shape per feature was 5.2 GB per denmark build).
    pub(super) shape_base: Shape,
    pub(super) flags_base: Vec<Vec<bool>>,
}

impl PolygonEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            pyramid: PyramidScratch::new(),
            attrs_by_zoom: std::array::from_fn(|_| None),
            pin_keys: FxHashSet::default(),
            base_pins: FxHashSet::default(),
            cap_events: Vec::new(),
            shape_base: Shape::new(),
            flags_base: Vec::new(),
        }
    }
}

pub(super) struct MultipolygonEmitScratch {
    pub(super) pyramid: PyramidScratch,
    pub(super) attrs_by_zoom: [Option<Vec<u8>>; 15],
    pub(super) base_pins: FxHashSet<(i32, i32)>,
    /// Cap events: (layer_zoom_idx as u16, bbox_tiles as u64, osm_id as u64).
    pub(super) cap_events: Vec<(u16, u64, u64)>,
    /// See `PolygonEmitScratch::shape_base`.
    pub(super) shape_base: Shape,
    pub(super) flags_base: Vec<Vec<bool>>,
}

impl MultipolygonEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            pyramid: PyramidScratch::new(),
            attrs_by_zoom: std::array::from_fn(|_| None),
            base_pins: FxHashSet::default(),
            cap_events: Vec::new(),
            shape_base: Shape::new(),
            flags_base: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Sort record push helper
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct RecordTally {
    pub(crate) layer_records: [u64; 32],
    pub(crate) layer_bytes: [u64; 32],
    pub(crate) layer_zoom_records: Box<[u64; 32 * 15]>,
    pub(crate) layer_zoom_bytes: Box<[u64; 32 * 15]>,
    pub(crate) total_records: u64,
    pub(crate) total_record_bytes: u64,
}

impl RecordTally {
    pub(super) fn new() -> Self {
        Self {
            layer_records: [0; 32],
            layer_bytes: [0; 32],
            layer_zoom_records: Box::new([0; 32 * 15]),
            layer_zoom_bytes: Box::new([0; 32 * 15]),
            total_records: 0,
            total_record_bytes: 0,
        }
    }

    pub(super) fn record(&mut self, key: sort::SortKey, data_len: usize) {
        let layer = sort::layer_from_key(key) as usize;
        self.total_records += 1;
        self.total_record_bytes += data_len as u64;
        if layer < 32 {
            self.layer_records[layer] += 1;
            self.layer_bytes[layer] += data_len as u64;
            let zoom = sort::zoom_from_tile_id(sort::tile_id_from_key(key)) as usize;
            if zoom < 15 {
                let idx = layer * 15 + zoom;
                self.layer_zoom_records[idx] += 1;
                self.layer_zoom_bytes[idx] += data_len as u64;
            }
        }
    }

    pub(super) fn merge(&mut self, other: &Self) {
        self.total_records += other.total_records;
        self.total_record_bytes += other.total_record_bytes;
        for i in 0..32 {
            self.layer_records[i] += other.layer_records[i];
            self.layer_bytes[i] += other.layer_bytes[i];
        }
        for i in 0..(32 * 15) {
            self.layer_zoom_records[i] += other.layer_zoom_records[i];
            self.layer_zoom_bytes[i] += other.layer_zoom_bytes[i];
        }
    }
}

impl Default for RecordTally {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) struct RecordSink {
    pub(super) records: Vec<sort::PayloadRecord>,
    pub(super) payload: Vec<u8>,
    pub(super) tally: RecordTally,
}

impl RecordSink {
    pub(super) fn new() -> Self {
        Self {
            records: Vec::new(),
            payload: Vec::new(),
            tally: RecordTally::new(),
        }
    }

    pub(super) fn bytes(&self) -> usize {
        self.payload.len() + self.records.len() * std::mem::size_of::<sort::PayloadRecord>()
    }

    pub(super) fn clear_payload(&mut self) {
        self.records.clear();
        self.payload.clear();
    }
}

impl Default for RecordSink {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) trait FeatureRecordSink {
    fn push_feature(
        &mut self,
        key: sort::SortKey,
        osm_id: u64,
        geom_type: GeomType,
        geom_buf: &[u32],
        attrs_buf: &[u8],
    );
}

impl FeatureRecordSink for Vec<SortRecord> {
    fn push_feature(
        &mut self,
        key: sort::SortKey,
        osm_id: u64,
        geom_type: GeomType,
        geom_buf: &[u32],
        attrs_buf: &[u8],
    ) {
        let data = encode_feature_data_with_attrs(osm_id, geom_type, geom_buf, attrs_buf);
        self.push(SortRecord { key, data });
    }
}

impl FeatureRecordSink for RecordSink {
    fn push_feature(
        &mut self,
        key: sort::SortKey,
        osm_id: u64,
        geom_type: GeomType,
        geom_buf: &[u32],
        attrs_buf: &[u8],
    ) {
        let range = append_feature_data_with_attrs(
            &mut self.payload,
            osm_id,
            geom_type,
            geom_buf,
            attrs_buf,
        );
        let len = range.end - range.start;
        self.records.push((key, range.start, len));
        self.tally.record(key, len);
    }
}

/// Push a single encoded feature into the sort record buffer.
/// Shared by all geometry emitters (point, line, polygon, multipolygon).
#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn push_sort_record<T: FeatureRecordSink + ?Sized>(
    tile_id: u64,
    osm_id: u64,
    layer: Layer,
    paint_rank: u8,
    geom_type: GeomType,
    geom_buf: &[u32],
    attrs_buf: &[u8],
    records: &mut T,
) {
    let key = sort::make_sort_key(tile_id, layer as u8, paint_rank);
    records.push_feature(key, osm_id, geom_type, geom_buf, attrs_buf);
}

const OSM_POLYGON_MAX_Z: u8 = 14;

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

fn fill_pin_keys_from_mask(
    points: &[Point],
    preserve_vertex_mask: &[bool],
    out: &mut FxHashSet<(i64, i64)>,
) {
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

fn polygon_z_start(merc: &[Point], z_lo: u8, z_hi: u8) -> Option<u8> {
    (z_lo..=z_hi).find(|&z| z == OSM_POLYGON_MAX_Z || !geometry::merc_bbox_is_subpixel(merc, z))
}

fn tile_count_for_base_rect(rect: IntRect, maxz: u8, z: u8) -> u64 {
    let max_tile = (1_u32 << z) - 1;
    let tile_size = i64::from(TILE_EXTENT_I32) << u32::from(maxz - z);
    let tx_min = base_tile_index(rect.min_x, tile_size, max_tile);
    let tx_max = base_tile_index(rect.max_x, tile_size, max_tile);
    let ty_min = base_tile_index(rect.min_y, tile_size, max_tile);
    let ty_max = base_tile_index(rect.max_y, tile_size, max_tile);
    u64::from(tx_max - tx_min + 1) * u64::from(ty_max - ty_min + 1)
}

fn base_tile_index(q: i32, tile_size: i64, max_tile: u32) -> u32 {
    if q <= 0 {
        0
    } else {
        u32::try_from(i64::from(q).div_euclid(tile_size))
            .expect("tile index fits u32")
            .min(max_tile)
    }
}

fn polygon_z_bottom_after_caps(
    shape_base: &Shape,
    z_start: u8,
    z_hi: u8,
    layer: Layer,
    osm_id: u64,
    fanout_cap: u32,
    cap_events: &mut Vec<(u16, u64, u64)>,
) -> Option<u8> {
    if fanout_cap == 0 {
        return Some(z_hi);
    }
    let bbox = shape_bbox(shape_base)?;
    let mut z_bottom = z_hi;
    for z in z_start..=z_hi {
        let bbox_tiles = tile_count_for_base_rect(bbox, OSM_POLYGON_MAX_Z, z);
        if bbox_tiles > u64::from(fanout_cap) {
            #[allow(clippy::cast_possible_truncation)]
            cap_events.push((
                (layer as usize * 15 + z as usize) as u16,
                bbox_tiles,
                osm_id,
            ));
            if z == z_start {
                return None;
            }
            z_bottom = z - 1;
            for capped_z in z + 1..=z_hi {
                let capped_tiles = tile_count_for_base_rect(bbox, OSM_POLYGON_MAX_Z, capped_z);
                #[allow(clippy::cast_possible_truncation)]
                cap_events.push((
                    (layer as usize * 15 + capped_z as usize) as u16,
                    capped_tiles,
                    osm_id,
                ));
            }
            break;
        }
    }
    Some(z_bottom)
}

fn clear_attrs_cache(cache: &mut [Option<Vec<u8>>; 15]) {
    cache.fill_with(|| None);
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
    Point {
        x: sx / n,
        y: sy / n,
    }
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
    records: &mut impl FeatureRecordSink,
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

    let cbbox = MercBbox {
        min_x: p.x,
        min_y: p.y,
        max_x: p.x,
        max_y: p.y,
    };
    let mut count: u64 = 0;
    for z in z_lo..=z_hi {
        encode_attrs_bytes(&mut scratch.attrs_buf, &m.attrs, z);
        geometry::for_each_tile_in_bbox(&cbbox, z, |tx, ty| {
            let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
            let (px, py) = geometry::merc_to_tile_px(&p, tx, ty, z);
            mvt::encode_point(&mut scratch.geom_buf, px, py);
            push_sort_record(
                tile_id,
                osm_id,
                m.layer,
                m.paint_rank,
                GeomType::Point,
                &scratch.geom_buf,
                &scratch.attrs_buf,
                records,
            );
            count += 1;
        });
    }
    count
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
// Visible only without the hotpath feature (the measure macro's wrapping
// masks it under --all-features).
#[allow(clippy::too_many_lines)]
pub(super) fn emit_line_feature(
    osm_id: u64,
    merc: &[Point],
    preserve_vertex_mask: &[bool],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut impl FeatureRecordSink,
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
        let skip_size_filter = z >= 14 || m.layer == Layer::Boundaries || m.layer == Layer::Streets;
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
                push_sort_record(
                    tile_id,
                    osm_id,
                    m.layer,
                    m.paint_rank,
                    GeomType::LineString,
                    &scratch.geom_buf,
                    &scratch.attrs_buf,
                    records,
                );
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
                    push_sort_record(
                        tile_id,
                        osm_id,
                        m.layer,
                        m.paint_rank,
                        GeomType::LineString,
                        &scratch.geom_buf,
                        &scratch.attrs_buf,
                        records,
                    );
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
        geometry::for_each_zoom_simplified(
            merc,
            z_lo,
            z_hi,
            2,
            |_| 1.0,
            skip_bbox_check,
            |z, simplified| {
                run_for_zoom(z, simplified);
            },
        );
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
    records: &mut impl FeatureRecordSink,
    scratch: &mut PolygonEmitScratch,
    seam_max_zoom: u8,
    deferral_stats: Option<&DeferralStats>,
    fanout_cap: u32,
    tol_scale: f64,
) -> u64 {
    let seam_max_zoom =
        if seam_max_zoom > 0 && deferral_stats.is_some_and(|d| d.is_disabled(m.layer as u8)) {
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
    if has_pins {
        let pin_keys = &scratch.pin_keys;
        quantize_polygon_pinned_into(
            merc,
            &[],
            OSM_POLYGON_MAX_Z,
            |p| pin_keys.contains(&merc_point_key(p)),
            &mut scratch.shape_base,
            &mut scratch.flags_base,
        );
    } else {
        quantize_polygon_into(merc, &[], OSM_POLYGON_MAX_Z, &mut scratch.shape_base);
        scratch.flags_base.clear();
    }
    if scratch.shape_base.is_empty() {
        return 0;
    }
    fill_base_pins(
        &scratch.shape_base,
        &scratch.flags_base,
        &mut scratch.base_pins,
    );

    let Some(z_start) = polygon_z_start(merc, z_lo, z_hi) else {
        return 0;
    };
    let Some(z_bottom) = polygon_z_bottom_after_caps(
        &scratch.shape_base,
        z_start,
        z_hi,
        m.layer,
        osm_id,
        fanout_cap,
        &mut scratch.cap_events,
    ) else {
        return 0;
    };
    for z in z_start..=z_bottom {
        if seam_max_zoom > 0
            && z <= seam_max_zoom
            && let Some(ds) = deferral_stats
        {
            ds.record(m.layer as u8, merc.len() as u64);
        }
    }

    clear_attrs_cache(&mut scratch.attrs_by_zoom);
    let pins = (!scratch.base_pins.is_empty()).then_some(&scratch.base_pins);
    let dp_tol = |z| polygon_dp_tol(z, seam_max_zoom, tol_scale);
    let min_area = |z| polygon_min_area(z, m.layer);
    let params = PyramidParams {
        maxz: OSM_POLYGON_MAX_Z,
        z_top: z_start,
        z_bottom,
        dp_tol: &dp_tol,
        min_area: &min_area,
        pins,
        tile_filter: None,
    };
    let mut count: u64 = 0;
    let attrs_by_zoom = &mut scratch.attrs_by_zoom;
    let pyramid = &mut scratch.pyramid;
    let mut sink = |z: u8, tx: u32, ty: u32, geom: &[u32], _kind| {
        let attrs_buf = attrs_by_zoom
            .get_mut(z as usize)
            .expect("OSM polygon zoom fits attr cache")
            .get_or_insert_with(|| {
                let mut buf = Vec::new();
                encode_attrs_bytes(&mut buf, &m.attrs, z);
                buf
            });
        let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
        push_sort_record(
            tile_id,
            osm_id,
            m.layer,
            m.paint_rank,
            GeomType::Polygon,
            geom,
            attrs_buf,
            records,
        );
        count += 1;
    };
    emit_shape_pyramid(&scratch.shape_base, &params, pyramid, &mut sink);
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
    records: &mut impl FeatureRecordSink,
    emit_scratch: &mut MultipolygonEmitScratch,
    _simp_scratch: &mut geometry::SimplifyMultiScratch,
    seam_max_zoom: u8,
    deferral_stats: Option<&DeferralStats>,
    fanout_cap: u32,
    tol_scale: f64,
) -> u64 {
    let seam_max_zoom =
        if seam_max_zoom > 0 && deferral_stats.is_some_and(|d| d.is_disabled(m.layer as u8)) {
            0
        } else {
            seam_max_zoom
        };
    emit_scratch.cap_events.clear();
    if let Some(keys) = preserve_vertex_keys.filter(|k| !k.is_empty()) {
        quantize_polygon_pinned_into(
            outer,
            inners,
            OSM_POLYGON_MAX_Z,
            |p| keys.contains(&merc_point_key(p)),
            &mut emit_scratch.shape_base,
            &mut emit_scratch.flags_base,
        );
    } else {
        quantize_polygon_into(
            outer,
            inners,
            OSM_POLYGON_MAX_Z,
            &mut emit_scratch.shape_base,
        );
        emit_scratch.flags_base.clear();
    }
    if emit_scratch.shape_base.is_empty() {
        return 0;
    }
    fill_base_pins(
        &emit_scratch.shape_base,
        &emit_scratch.flags_base,
        &mut emit_scratch.base_pins,
    );

    let Some(z_start) = polygon_z_start(outer, z_lo, z_hi) else {
        return 0;
    };
    let Some(z_bottom) = polygon_z_bottom_after_caps(
        &emit_scratch.shape_base,
        z_start,
        z_hi,
        m.layer,
        osm_id,
        fanout_cap,
        &mut emit_scratch.cap_events,
    ) else {
        return 0;
    };
    for z in z_start..=z_bottom {
        if seam_max_zoom > 0
            && z <= seam_max_zoom
            && let Some(ds) = deferral_stats
        {
            let verts = outer.len() as u64 + inners.iter().map(|r| r.len() as u64).sum::<u64>();
            ds.record(m.layer as u8, verts);
        }
    }

    clear_attrs_cache(&mut emit_scratch.attrs_by_zoom);
    let pins = (!emit_scratch.base_pins.is_empty()).then_some(&emit_scratch.base_pins);
    let dp_tol = |z| polygon_dp_tol(z, seam_max_zoom, tol_scale);
    let min_area = |z| polygon_min_area(z, m.layer);
    let params = PyramidParams {
        maxz: OSM_POLYGON_MAX_Z,
        z_top: z_start,
        z_bottom,
        dp_tol: &dp_tol,
        min_area: &min_area,
        pins,
        tile_filter: None,
    };
    let mut count: u64 = 0;
    let attrs_by_zoom = &mut emit_scratch.attrs_by_zoom;
    let pyramid = &mut emit_scratch.pyramid;
    let mut sink = |z: u8, tx: u32, ty: u32, geom: &[u32], _kind| {
        let attrs_buf = attrs_by_zoom
            .get_mut(z as usize)
            .expect("OSM polygon zoom fits attr cache")
            .get_or_insert_with(|| {
                let mut buf = Vec::new();
                encode_attrs_bytes(&mut buf, &m.attrs, z);
                buf
            });
        let tile_id = pmtiles_writer::xy_to_tile_id(z, tx, ty);
        push_sort_record(
            tile_id,
            osm_id,
            m.layer,
            m.paint_rank,
            GeomType::Polygon,
            geom,
            attrs_buf,
            records,
        );
        count += 1;
    };
    emit_shape_pyramid(&emit_scratch.shape_base, &params, pyramid, &mut sink);
    count
}

#[cfg(test)]
mod landing2_tests {
    use super::*;
    use crate::mvt;
    use smallvec::smallvec;
    use std::borrow::Cow;

    fn pt(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    fn square(min: f64, max: f64) -> Vec<Point> {
        vec![
            pt(min, min),
            pt(max, min),
            pt(max, max),
            pt(min, max),
            pt(min, min),
        ]
    }

    fn brute_force_zoom_set(merc: &[Point], z_lo: u8, z_hi: u8) -> Vec<u8> {
        let mut out = Vec::new();
        for z in (z_lo..=z_hi).rev() {
            if z < OSM_POLYGON_MAX_Z && geometry::merc_bbox_is_subpixel(merc, z) {
                break;
            }
            out.push(z);
        }
        out.sort_unstable();
        out
    }

    fn planned_zoom_set(merc: &[Point], z_lo: u8, z_hi: u8) -> Vec<u8> {
        polygon_z_start(merc, z_lo, z_hi).map_or_else(Vec::new, |start| (start..=z_hi).collect())
    }

    #[test]
    fn zoom_set_parity_bruteforce() {
        let wide = square(0.25, 0.75);
        assert_eq!(
            planned_zoom_set(&wide, 0, 14),
            brute_force_zoom_set(&wide, 0, 14)
        );

        let tiny_z14_only = square(0.5, 0.500_000_001);
        assert_eq!(
            planned_zoom_set(&tiny_z14_only, 0, 14),
            brute_force_zoom_set(&tiny_z14_only, 0, 14)
        );
        assert_eq!(planned_zoom_set(&tiny_z14_only, 0, 14), vec![14]);

        let tiny_empty = square(0.5, 0.500_000_001);
        assert_eq!(
            planned_zoom_set(&tiny_empty, 0, 13),
            brute_force_zoom_set(&tiny_empty, 0, 13)
        );
        assert!(planned_zoom_set(&tiny_empty, 0, 13).is_empty());
    }

    #[test]
    fn cap_suffix_property() {
        let mut shape = Shape::new();
        quantize_polygon_into(&square(0.1, 0.9), &[], OSM_POLYGON_MAX_Z, &mut shape);
        let mut cap_events = Vec::new();
        let z_bottom =
            polygon_z_bottom_after_caps(&shape, 0, 4, Layer::Buildings, 777, 4, &mut cap_events)
                .expect("low zooms stay below cap");
        assert_eq!(z_bottom, 1);
        let capped_zooms: Vec<u8> = cap_events
            .iter()
            .map(|&(idx, _, _)| u8::try_from(idx as usize % 15).expect("zoom fits"))
            .collect();
        assert_eq!(capped_zooms, vec![2, 3, 4]);
        assert!(cap_events.windows(2).all(|w| w[0].1 <= w[1].1));
    }

    fn attr_count(data: &[u8]) -> u8 {
        let cmd_count = u32::from_le_bytes(data[9..13].try_into().expect("cmd count")) as usize;
        data[13 + cmd_count * 4]
    }

    #[test]
    fn zoom_gated_attr_parity() {
        let m = LayerMatch {
            layer: Layer::Buildings,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![
                ("kind", AttrValue::Str(Cow::Borrowed("test")), 0),
                ("height", AttrValue::Float(12.0), 13),
            ],
        };
        let mut records = Vec::new();
        let mut scratch = PolygonEmitScratch::new();
        let count = emit_polygon_feature(
            42,
            &square(0.500_10, 0.500_11),
            &[],
            &m,
            12,
            14,
            &mut records,
            &mut scratch,
            0,
            None,
            0,
            1.0,
        );
        assert_eq!(
            usize::try_from(count).expect("record count fits usize"),
            records.len()
        );
        assert!(!records.is_empty());

        for rec in &records {
            let tile_id = sort::tile_id_from_key(rec.key);
            let (z, _, _) = pmtiles_writer::tile_id_to_zxy(tile_id);
            let want_count = if z >= 13 { 2 } else { 1 };
            assert_eq!(attr_count(&rec.data), want_count, "z{z}");

            let mut lb = mvt::LayerBuilder::new("test");
            let mut gp = Vec::new();
            let mut tp = Vec::new();
            crate::wire_format::add_feature_to_layer(&mut lb, &rec.data, &mut gp, &mut tp);
            let feature = lb.test_feature(0);
            let keys: Vec<&str> = feature
                .tags
                .iter()
                .map(|(key, _)| lb.test_key(*key))
                .collect();
            if z >= 13 {
                assert!(keys.contains(&"height"));
            } else {
                assert!(!keys.contains(&"height"));
            }
        }
    }
}
