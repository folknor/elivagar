use rustc_hash::FxHashSet;
use smallvec::SmallVec;
use std::num::NonZeroU32;

use super::stats::CapEvent;
use crate::geometry::int_ocean::{
    IntRect, OSM_DP_TOL_PX, PinnedRings, Shape, TILE_BUFFER_I32, quantize_polygon_into,
    quantize_polygon_pinned_into, shape_bbox,
};
use crate::geometry::polygon::{Area, ExtentUnits, MIN_POLY_AREA_AREA};
use crate::geometry::pyramid::{
    PyramidCell, PyramidParams, PyramidScratch, PyramidZooms, RingCapError, Simplifier,
    emit_shape_pyramid, tile_cover,
};
use crate::geometry::{
    self,
    clip::{self, ClipRect},
    projection::{MercBbox, Point},
    tiles::{BUFFER_FRACTION, merc_bbox},
};
use crate::multipolygon::MemberWay;
use crate::mvt::{self, GeomType};
#[cfg(test)]
use crate::shortbread::AttrValue;
use crate::shortbread::{GeomExpect, Layer, LayerMatch, OsmGeomType, PolygonPointStrategy};
pub(crate) use crate::sort::RecordTally;
use crate::sort::{self, LayerZoom, SortRecord};
use crate::tiling::{MAX_ZOOM, TileId, ZOOM_COUNT, ZoomRange};
use crate::wire_format::{
    append_feature_data_with_attrs, encode_attrs_bytes, encode_feature_data_with_attrs,
};
use rustc_hash::FxHashMap;

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

fn relation_vertex_is_pinned(keys: &FxHashSet<(i64, i64)>, point: &Point) -> bool {
    let key = merc_point_key(point);
    // Ring unwrapping and seam emission can translate the geometry by whole
    // worlds. The source member keys still use the original wrapped copy.
    let wrapped_x = key.0.rem_euclid(1_000_000_000_000);
    keys.contains(&key)
        || keys.contains(&(wrapped_x, key.1))
        || (wrapped_x == 0 && keys.contains(&(1_000_000_000_000, key.1)))
}

// ---------------------------------------------------------------------------
// Polygon enrichment (area-dependent attrs + min_zoom overrides)
// ---------------------------------------------------------------------------

/// Enrich polygon matches with geometry-dependent data.
/// - BoundaryLabels: `way_area` in hectares, min_zoom override based on area thresholds.
pub(super) fn enrich_polygon_matches(matches: &mut [LayerMatch], area_m2: f64) {
    for m in matches.iter_mut() {
        if m.layer == Layer::BoundaryLabels {
            crate::shortbread::boundaries::enrich_boundary_label(m, area_m2);
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
    pub(super) attrs_by_zoom: [Option<Vec<u8>>; ZOOM_COUNT],
    pub(super) pin_keys: FxHashSet<(i64, i64)>,
    pub(super) base_pins: FxHashSet<(i32, i32)>,
    pub(super) cap_events: Vec<CapEvent>,
    /// Quantized base shape + pin flags, ring Vecs recycled across features
    /// (H6: one fresh Shape per feature was 5.2 GB per denmark build).
    pub(super) shape_base: Shape,
    pub(super) flags_base: PinnedRings,
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
            flags_base: PinnedRings::default(),
        }
    }
}

pub(super) struct MultipolygonEmitScratch {
    pub(super) pyramid: PyramidScratch,
    pub(super) attrs_by_zoom: [Option<Vec<u8>>; ZOOM_COUNT],
    pub(super) base_pins: FxHashSet<(i32, i32)>,
    pub(super) cap_events: Vec<CapEvent>,
    /// See `PolygonEmitScratch::shape_base`.
    pub(super) shape_base: Shape,
    pub(super) flags_base: PinnedRings,
}

impl MultipolygonEmitScratch {
    pub(super) fn new() -> Self {
        Self {
            pyramid: PyramidScratch::new(),
            attrs_by_zoom: std::array::from_fn(|_| None),
            base_pins: FxHashSet::default(),
            cap_events: Vec::new(),
            shape_base: Shape::new(),
            flags_base: PinnedRings::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Sort record push helper
// ---------------------------------------------------------------------------

pub(super) struct RecordSink {
    pub(super) records: Vec<sort::PayloadRef>,
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
        self.payload.len() + self.records.len() * std::mem::size_of::<sort::PayloadRef>()
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

#[derive(Clone, Copy)]
struct FeatureId(u64);

#[derive(Clone, Copy)]
enum FeatureSource {
    Node,
    Way,
    Relation,
}

impl FeatureId {
    // Leave room for every type code below JavaScript's Number.MAX_SAFE_INTEGER.
    const MAX_OSM_ID: u64 = ((1_u64 << 53) - 1 - 3) / 10;

    fn for_osm_feature(layer: Layer, source: FeatureSource, osm_id: u64) -> Self {
        // Node, way, and relation ids reach this dispatch as their raw numeric
        // IDs, so mixed-source layers need an explicit type tag.
        let source_types = [
            layer.accepts(OsmGeomType::Node),
            layer.accepts(OsmGeomType::OpenWay) || layer.accepts(OsmGeomType::ClosedWay),
            layer.accepts(OsmGeomType::MultiPolygon),
        ];
        if source_types
            .into_iter()
            .filter(|accepted| *accepted)
            .count()
            < 2
        {
            return Self(osm_id);
        }

        // Match Planetiler's decimal type codes while keeping ids exact in
        // MapLibre's JavaScript Number feature-state keys. Ocean features are
        // emitted separately and keep their existing ids.
        assert!(
            osm_id <= Self::MAX_OSM_ID,
            "OSM id {osm_id} exceeds the JavaScript-safe namespaced feature id limit {}",
            Self::MAX_OSM_ID,
        );
        let type_code: u64 = match source {
            FeatureSource::Node => 1,
            FeatureSource::Way => 2,
            FeatureSource::Relation => 3,
        };
        Self(osm_id * 10 + type_code)
    }
}

/// The feature helpers keep the raw OSM id for diagnostics. This sink assigns
/// the layer-scoped, source-typed id when the feature enters the sort stream.
struct NamespacedFeatureSink<'a, T> {
    inner: &'a mut T,
    feature_id: FeatureId,
}

impl<T: FeatureRecordSink> FeatureRecordSink for NamespacedFeatureSink<'_, T> {
    fn push_feature(
        &mut self,
        key: sort::SortKey,
        _osm_id: u64,
        geom_type: GeomType,
        geom_buf: &[u32],
        attrs_buf: &[u8],
    ) {
        self.inner
            .push_feature(key, self.feature_id.0, geom_type, geom_buf, attrs_buf);
    }
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
        self.records.push(sort::PayloadRef {
            key,
            offset: range.start,
            len,
        });
        self.tally.record(key, len);
    }
}

/// Push a single encoded feature into the sort record buffer.
/// Shared by all geometry emitters (point, line, polygon, multipolygon).
#[inline]
#[allow(clippy::too_many_arguments)]
pub(super) fn push_sort_record<T: FeatureRecordSink + ?Sized>(
    tile_id: TileId,
    osm_id: u64,
    layer: Layer,
    paint_rank: u8,
    geom_type: GeomType,
    geom_buf: &[u32],
    attrs_buf: &[u8],
    records: &mut T,
) {
    let key = sort::SortKey::new(tile_id, layer, paint_rank);
    records.push_feature(key, osm_id, geom_type, geom_buf, attrs_buf);
}

/// OSM polygons quantize on the profile's top-zoom grid whatever the run's
/// zoom window, so a narrowed archive's polygons match a full build's.
const OSM_POLYGON_MAX_Z: u8 = MAX_ZOOM;

fn polygon_min_area(z: u8, layer: Layer) -> Area {
    if z >= OSM_POLYGON_MAX_Z || layer == Layer::StreetPolygons || layer == Layer::Bridges {
        Area::ZERO
    } else {
        MIN_POLY_AREA_AREA
    }
}

fn polygon_dp_tol(z: u8, tol_scale: f64) -> ExtentUnits {
    if z >= OSM_POLYGON_MAX_Z {
        ExtentUnits::new(0)
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let tol = (OSM_DP_TOL_PX as f64 * tol_scale).round() as i64;
        ExtentUnits::new(tol.max(0))
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

fn fill_base_pins(shape: &Shape, flags: &PinnedRings, out: &mut FxHashSet<(i32, i32)>) {
    out.clear();
    out.extend(flags.pinned_points_for(shape).map(|p| (p.x, p.y)));
}

fn polygon_z_start(merc: &[Point], z_lo: u8, z_hi: u8) -> Option<u8> {
    (z_lo..=z_hi)
        .find(|&z| z == OSM_POLYGON_MAX_Z || !geometry::simplify::merc_bbox_is_subpixel(merc, z))
}

fn tile_count_for_base_rect(rect: IntRect, maxz: u8, z: u8) -> u64 {
    let (tx_min, tx_max, ty_min, ty_max) = tile_cover(rect, maxz, z, TILE_BUFFER_I32);
    u64::from(tx_max - tx_min + 1) * u64::from(ty_max - ty_min + 1)
}

fn polygon_z_bottom_after_caps(
    shape_base: &Shape,
    z_start: u8,
    z_hi: u8,
    layer: Layer,
    osm_id: u64,
    fanout_cap: Option<NonZeroU32>,
    cap_events: &mut Vec<CapEvent>,
) -> Option<u8> {
    let Some(fanout_cap) = fanout_cap else {
        return Some(z_hi);
    };
    let bbox = shape_bbox(shape_base)?;
    let mut z_bottom = z_hi;
    let event = |z: u8, bbox_tiles: u64| CapEvent {
        at: LayerZoom::new(layer, z).expect("OSM polygon zooms are profile zooms"),
        bbox_tiles,
        osm_id,
    };
    for z in z_start..=z_hi {
        let bbox_tiles = tile_count_for_base_rect(bbox, OSM_POLYGON_MAX_Z, z);
        if bbox_tiles > u64::from(fanout_cap.get()) {
            cap_events.push(event(z, bbox_tiles));
            if z == z_start {
                return None;
            }
            z_bottom = z - 1;
            for capped_z in z + 1..=z_hi {
                let capped_tiles = tile_count_for_base_rect(bbox, OSM_POLYGON_MAX_Z, capped_z);
                cap_events.push(event(capped_z, capped_tiles));
            }
            break;
        }
    }
    Some(z_bottom)
}

/// The zoom plan of one OSM polygon under a run's zoom window `z_lo..=z_hi`.
///
/// Construction is canonical: the first zoom comes from the layer's own range
/// (`m.min_zoom..=m.max_zoom`), not the window, because the pyramid clips
/// every zoom from its root fragments and a root chosen inside a raised window
/// would reach that zoom along a different path than a full build does. Only
/// the descent's floor is cut at the window's top, which cannot change a
/// shallower zoom, and the sink drops zooms below the window. Returns
/// `(z_top, z_bottom)` for the pyramid, or `None` when the polygon has no tile
/// in the window. Cap events describe the requested archive: only zooms in
/// the window are kept.
#[allow(clippy::too_many_arguments)]
fn polygon_zoom_plan(
    shape_base: &Shape,
    merc: &[Point],
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    osm_id: u64,
    fanout_cap: Option<NonZeroU32>,
    cap_events: &mut Vec<CapEvent>,
) -> Option<(u8, u8)> {
    // `z_lo..=z_hi` is the layer range already intersected with the window
    // (`emit_match`), so the layer's own bounds contain it.
    let z_start = polygon_z_start(merc, m.min_zoom, m.max_zoom)?;
    if z_start > z_hi {
        return None;
    }
    let z_bottom = polygon_z_bottom_after_caps(
        shape_base, z_start, z_hi, m.layer, osm_id, fanout_cap, cap_events,
    );
    cap_events.retain(|event| event.at.zoom() >= z_lo);
    let z_bottom = z_bottom?;
    (z_bottom >= z_lo).then_some((z_start, z_bottom))
}

fn clear_attrs_cache(cache: &mut [Option<Vec<u8>>; ZOOM_COUNT]) {
    cache.fill_with(|| None);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Per-invocation emission policy. Layer indices must have a configured cap slot.
pub(super) struct EmitCtx<'a> {
    /// The run's zoom window: only zooms inside it are emitted.
    pub(super) window: ZoomRange,
    pub(super) fanout_caps: &'a [Option<NonZeroU32>],
    pub(super) polygon_simplify_factor: f64,
}

pub(super) enum FeatureGeometry<'a> {
    Node(&'a Point),
    Way {
        coords: &'a [Point],
        pins: &'a [bool],
        line: &'a mut LineEmitScratch,
        polygon: &'a mut PolygonEmitScratch,
        fanout: &'a mut super::stats::FanoutStats,
    },
    Area {
        outer: &'a [Point],
        inners: &'a [Vec<Point>],
        pins: &'a FxHashSet<(i64, i64)>,
        polygon: &'a mut MultipolygonEmitScratch,
        fanout: &'a mut super::stats::FanoutStats,
    },
    AreaLabel {
        point: &'a Point,
    },
}

fn harvest_cap_events(events: &mut Vec<CapEvent>, fanout: &mut super::stats::FanoutStats) {
    for event in events.drain(..) {
        fanout.record_cap(event);
    }
}

/// The only LayerMatch dispatch. Harvest after each polygon call, before the
/// next shifted copy or component can clear the emitter's event buffer.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn emit_match(
    osm_id: u64,
    geometry: FeatureGeometry<'_>,
    m: &LayerMatch,
    ctx: &EmitCtx<'_>,
    records: &mut impl FeatureRecordSink,
    point: &mut PointEmitScratch,
) -> Result<u64, RingCapError> {
    // A layer range that is empty (an area rule may raise `min_zoom` past
    // the top) or misses the window emits nothing.
    let Some(zooms) = ZoomRange::new(m.min_zoom, m.max_zoom).and_then(|r| r.intersect(ctx.window))
    else {
        return Ok(0);
    };
    let (z_lo, z_hi) = (zooms.first(), zooms.last());
    let source = match &geometry {
        FeatureGeometry::Node(_) => FeatureSource::Node,
        FeatureGeometry::Way { .. } => FeatureSource::Way,
        FeatureGeometry::Area { .. } | FeatureGeometry::AreaLabel { .. } => FeatureSource::Relation,
    };
    let feature_id = FeatureId::for_osm_feature(m.layer, source, osm_id);
    let mut records = NamespacedFeatureSink {
        inner: records,
        feature_id,
    };
    match m.geom_expect {
        GeomExpect::Point | GeomExpect::PolygonCentroid | GeomExpect::PolygonPointOnSurface => {
            Ok(match geometry {
                FeatureGeometry::Node(p) => {
                    // Preserve the node's projected world-edge coordinate;
                    // centroid wrapping applies only to way/area geometry.
                    let bbox = merc_bbox(std::slice::from_ref(p));
                    let mut count = 0;
                    for z in z_lo..=z_hi {
                        encode_attrs_bytes(&mut point.attrs_buf, &m.attrs, z);
                        geometry::tiles::for_each_tile_in_bbox(&bbox, z, |tx, ty| {
                            let (px, py) = geometry::projection::merc_to_tile_px(p, tx, ty, z);
                            mvt::encode_point(&mut point.geom_buf, px, py);
                            push_sort_record(
                                TileId::at(z, tx, ty),
                                osm_id,
                                m.layer,
                                m.paint_rank,
                                GeomType::Point,
                                &point.geom_buf,
                                &point.attrs_buf,
                                &mut records,
                            );
                            count += 1;
                        });
                    }
                    count
                }
                FeatureGeometry::Way { coords, .. } => emit_point_or_centroid(
                    osm_id,
                    coords,
                    None,
                    &merc_bbox(coords),
                    m,
                    z_lo,
                    z_hi,
                    &mut records,
                    point,
                ),
                FeatureGeometry::Area { outer, inners, .. } => {
                    if m.geom_expect == GeomExpect::Point {
                        // The profile's plain point rules describe OSM nodes,
                        // not polygon labels. match_multipolygon uses explicit
                        // centroid/surface rules instead; do not invent a label.
                        return Ok(0);
                    }
                    emit_point_or_centroid(
                        osm_id,
                        outer,
                        Some(inners),
                        &merc_bbox(outer),
                        m,
                        z_lo,
                        z_hi,
                        &mut records,
                        point,
                    )
                }
                FeatureGeometry::AreaLabel { point: p } => {
                    emit_projected_point(osm_id, *p, m, z_lo, z_hi, &mut records, point)
                }
            })
        }
        GeomExpect::Line | GeomExpect::Polygon => {
            let (coords, inners) = match &geometry {
                FeatureGeometry::Node(_) | FeatureGeometry::AreaLabel { .. } => return Ok(0),
                FeatureGeometry::Way { coords, .. } => (*coords, None),
                FeatureGeometry::Area { outer, inners, .. } => {
                    if m.geom_expect == GeomExpect::Line {
                        // Boundary lines emit once per member way in phase12,
                        // with relation metadata from the boundary prescan.
                        // Assembled area rings must not draw those lines again.
                        return Ok(0);
                    }
                    (*outer, Some(*inners))
                }
            };
            let cap = if m.geom_expect == GeomExpect::Polygon {
                ctx.fanout_caps[m.layer as usize]
            } else {
                None
            };
            let mut geometry = geometry;
            let mut count = 0;
            for shift in antimeridian_shifts_for_bbox(&merc_bbox(coords)) {
                let shifted;
                let shifted_inners;
                let (coords, holes) = if shift == 0.0 {
                    (coords, inners)
                } else {
                    shifted = coords
                        .iter()
                        .map(|p| Point {
                            x: p.x + shift,
                            y: p.y,
                        })
                        .collect::<Vec<_>>();
                    shifted_inners = inners.map(|rings| {
                        rings
                            .iter()
                            .map(|ring| {
                                ring.iter()
                                    .map(|p| Point {
                                        x: p.x + shift,
                                        y: p.y,
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .collect::<Vec<_>>()
                    });
                    (shifted.as_slice(), shifted_inners.as_deref())
                };
                count += match &mut geometry {
                    FeatureGeometry::Node(_) | FeatureGeometry::AreaLabel { .. } => unreachable!(),
                    FeatureGeometry::Way {
                        pins,
                        line,
                        polygon,
                        fanout,
                        ..
                    } => {
                        if m.geom_expect == GeomExpect::Line {
                            emit_line_feature(
                                osm_id,
                                coords,
                                pins,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                line,
                            )
                        } else {
                            let count = emit_polygon_feature(
                                osm_id,
                                coords,
                                pins,
                                m,
                                z_lo,
                                z_hi,
                                &mut records,
                                polygon,
                                cap,
                                ctx.polygon_simplify_factor,
                            );
                            // Harvest before propagating, so a failure
                            // leaves no stale events in the scratch.
                            harvest_cap_events(&mut polygon.cap_events, fanout);
                            count?
                        }
                    }
                    FeatureGeometry::Area {
                        pins,
                        polygon,
                        fanout,
                        ..
                    } => {
                        let count = emit_multipolygon_feature(
                            osm_id,
                            coords,
                            holes.expect("area holes"),
                            Some(pins),
                            m,
                            z_lo,
                            z_hi,
                            &mut records,
                            polygon,
                            cap,
                            ctx.polygon_simplify_factor,
                        );
                        harvest_cap_events(&mut polygon.cap_events, fanout);
                        count?
                    }
                };
            }
            Ok(count)
        }
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
    let use_surface = m.geom_expect != GeomExpect::Point
        && match m.layer.polygon_point_strategy() {
            Some(PolygonPointStrategy::CentroidIfConvex) => {
                inners.is_some_and(|holes| !holes.is_empty())
                    || !geometry::surface::is_convex(coords)
            }
            Some(PolygonPointStrategy::PointOnSurface) => true,
            Some(PolygonPointStrategy::Centroid) => false,
            None => m.geom_expect == GeomExpect::PolygonPointOnSurface,
        };
    let pt = if m.geom_expect == GeomExpect::Point {
        coords.first().copied()
    } else if use_surface {
        if let Some(holes) = inners {
            geometry::surface::point_on_surface_with_holes(coords, holes)
        } else {
            geometry::surface::point_on_surface(coords)
        }
    } else {
        geometry::surface::polygon_centroid(coords, inners.unwrap_or(&[]))
    };
    let Some(p) = pt else { return 0 };
    emit_projected_point(osm_id, p, m, z_lo, z_hi, records, scratch)
}

#[hotpath::measure]
#[allow(clippy::too_many_arguments)]
fn emit_projected_point(
    osm_id: u64,
    mut p: Point,
    m: &LayerMatch,
    z_lo: u8,
    z_hi: u8,
    records: &mut impl FeatureRecordSink,
    scratch: &mut PointEmitScratch,
) -> u64 {
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
        geometry::tiles::for_each_tile_in_bbox(&cbbox, z, |tx, ty| {
            let tile_id = TileId::at(z, tx, ty);
            let (px, py) = geometry::projection::merc_to_tile_px(&p, tx, ty, z);
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
        let single_tile = geometry::tiles::is_single_tile(&simp_bbox, z);

        // Skip min-size filtering at max zoom and for boundaries/streets
        let skip_size_filter =
            z >= MAX_ZOOM || m.layer == Layer::Boundaries || m.layer == Layer::Streets;
        geometry::tiles::for_each_tile_in_bbox(&simp_bbox, z, |tx, ty| {
            if single_tile {
                // Fast path: bbox fits in one tile - clipping is a no-op.
                if simplified.len() < 2 {
                    return;
                }
                geometry::tiles::to_tile_coords_into(&mut scratch.tc_buf, simplified, tx, ty, z);
                if !skip_size_filter && geometry::simplify::line_is_subpixel(&scratch.tc_buf) {
                    return;
                }
                mvt::encode_linestring(&mut scratch.geom_buf, &scratch.tc_buf);
                if scratch.geom_buf.is_empty() {
                    return;
                }
                let tile_id = TileId::at(z, tx, ty);
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
                clip::for_each_clipped_segment(simplified, &clip, |segment| {
                    if segment.len() < 2 {
                        return;
                    }
                    geometry::tiles::to_tile_coords_into(&mut scratch.tc_buf, segment, tx, ty, z);
                    if !skip_size_filter && geometry::simplify::line_is_subpixel(&scratch.tc_buf) {
                        return;
                    }
                    mvt::encode_linestring(&mut scratch.geom_buf, &scratch.tc_buf);
                    if scratch.geom_buf.is_empty() {
                        return;
                    }
                    let tile_id = TileId::at(z, tx, ty);
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
        // Each zoom simplifies the source independently, so the window alone
        // decides which zooms run.
        for z in (z_lo..=z_hi).rev() {
            if z < MAX_ZOOM
                && !skip_bbox_check
                && geometry::simplify::merc_bbox_is_subpixel(merc, z)
            {
                break;
            }
            if z < MAX_ZOOM {
                let tol = geometry::simplify::simplify_tolerance(z);
                let _ = geometry::simplify::simplify_into_with_required(
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
        // The cascade simplifies each zoom from the one above it, so it starts
        // at the layer's own top zoom, not the window's: a cascade begun at a
        // lowered window top would reach the window along a different chain
        // than a full build. Zooms above the window run but do not emit.
        geometry::simplify::for_each_zoom_simplified(
            merc,
            z_lo,
            m.max_zoom,
            2,
            |_| 1.0,
            skip_bbox_check,
            |z, simplified| {
                if z <= z_hi {
                    run_for_zoom(z, simplified);
                }
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
    fanout_cap: Option<NonZeroU32>,
    tol_scale: f64,
) -> Result<u64, RingCapError> {
    scratch.cap_events.clear();
    if merc.len() < 4 {
        return Ok(0);
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
        scratch
            .flags_base
            .fill_for_shape(&scratch.shape_base, |_, ring, flags| {
                flags.resize(ring.len(), false);
            });
    }
    if scratch.shape_base.is_empty() {
        return Ok(0);
    }
    fill_base_pins(
        &scratch.shape_base,
        &scratch.flags_base,
        &mut scratch.base_pins,
    );

    let Some((z_start, z_bottom)) = polygon_zoom_plan(
        &scratch.shape_base,
        merc,
        m,
        z_lo,
        z_hi,
        osm_id,
        fanout_cap,
        &mut scratch.cap_events,
    ) else {
        return Ok(0);
    };

    clear_attrs_cache(&mut scratch.attrs_by_zoom);
    let pins = (!scratch.base_pins.is_empty()).then_some(&scratch.base_pins);
    let dp_tol = |z| polygon_dp_tol(z, tol_scale);
    let min_area = |z| polygon_min_area(z, m.layer);
    let params = PyramidParams {
        zooms: PyramidZooms::new(OSM_POLYGON_MAX_Z, z_start, z_bottom)
            .expect("a polygon zoom plan lies inside the profile grid"),
        dp_tol: &dp_tol,
        min_area: &min_area,
        pins,
        tile_filter: None,
        simplifier: Simplifier::DouglasPeucker,
    };
    let mut count: u64 = 0;
    let attrs_by_zoom = &mut scratch.attrs_by_zoom;
    let pyramid = &mut scratch.pyramid;
    let mut sink = |PyramidCell { z, tx, ty }, geom: &[u32], _kind| {
        // Zooms above the window are constructed (see `polygon_zoom_plan`)
        // but not emitted.
        if z < z_lo {
            return;
        }
        let attrs_buf = attrs_by_zoom
            .get_mut(z as usize)
            .expect("OSM polygon zoom fits attr cache")
            .get_or_insert_with(|| {
                let mut buf = Vec::new();
                encode_attrs_bytes(&mut buf, &m.attrs, z);
                buf
            });
        let tile_id = TileId::at(z, tx, ty);
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
    emit_shape_pyramid(&scratch.shape_base, &params, pyramid, &mut sink)?;
    Ok(count)
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
    fanout_cap: Option<NonZeroU32>,
    tol_scale: f64,
) -> Result<u64, RingCapError> {
    emit_scratch.cap_events.clear();
    if let Some(keys) = preserve_vertex_keys.filter(|k| !k.is_empty()) {
        quantize_polygon_pinned_into(
            outer,
            inners,
            OSM_POLYGON_MAX_Z,
            |p| relation_vertex_is_pinned(keys, p),
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
        emit_scratch
            .flags_base
            .fill_for_shape(&emit_scratch.shape_base, |_, ring, flags| {
                flags.resize(ring.len(), false);
            });
    }
    if emit_scratch.shape_base.is_empty() {
        return Ok(0);
    }
    fill_base_pins(
        &emit_scratch.shape_base,
        &emit_scratch.flags_base,
        &mut emit_scratch.base_pins,
    );

    let Some((z_start, z_bottom)) = polygon_zoom_plan(
        &emit_scratch.shape_base,
        outer,
        m,
        z_lo,
        z_hi,
        osm_id,
        fanout_cap,
        &mut emit_scratch.cap_events,
    ) else {
        return Ok(0);
    };

    clear_attrs_cache(&mut emit_scratch.attrs_by_zoom);
    let pins = (!emit_scratch.base_pins.is_empty()).then_some(&emit_scratch.base_pins);
    let dp_tol = |z| polygon_dp_tol(z, tol_scale);
    let min_area = |z| polygon_min_area(z, m.layer);
    let params = PyramidParams {
        zooms: PyramidZooms::new(OSM_POLYGON_MAX_Z, z_start, z_bottom)
            .expect("a polygon zoom plan lies inside the profile grid"),
        dp_tol: &dp_tol,
        min_area: &min_area,
        pins,
        tile_filter: None,
        simplifier: Simplifier::DouglasPeucker,
    };
    let mut count: u64 = 0;
    let attrs_by_zoom = &mut emit_scratch.attrs_by_zoom;
    let pyramid = &mut emit_scratch.pyramid;
    let mut sink = |PyramidCell { z, tx, ty }, geom: &[u32], _kind| {
        // Zooms above the window are constructed (see `polygon_zoom_plan`)
        // but not emitted.
        if z < z_lo {
            return;
        }
        let attrs_buf = attrs_by_zoom
            .get_mut(z as usize)
            .expect("OSM polygon zoom fits attr cache")
            .get_or_insert_with(|| {
                let mut buf = Vec::new();
                encode_attrs_bytes(&mut buf, &m.attrs, z);
                buf
            });
        let tile_id = TileId::at(z, tx, ty);
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
    emit_shape_pyramid(&emit_scratch.shape_base, &params, pyramid, &mut sink)?;
    Ok(count)
}

#[cfg(test)]
mod landing2_tests {
    use super::*;
    use crate::mvt;
    use smallvec::smallvec;
    use std::borrow::Cow;

    #[test]
    fn mixed_source_ids_stay_distinct_as_javascript_numbers() {
        let mut numbers = Vec::new();
        for osm_id in [
            0,
            42,
            13_000_000_000,
            13_000_000_001,
            FeatureId::MAX_OSM_ID - 1,
            FeatureId::MAX_OSM_ID,
        ] {
            for source in [
                FeatureSource::Node,
                FeatureSource::Way,
                FeatureSource::Relation,
            ] {
                let id = FeatureId::for_osm_feature(Layer::Pois, source, osm_id).0;
                // Parsing as f64 models the consumer's JavaScript Number.
                let decimal = id.to_string();
                let number = decimal.parse::<f64>().expect("numeric feature id");
                assert_eq!(format!("{number:.0}"), decimal);
                assert!(!numbers.contains(&number), "consumer feature id collision");
                numbers.push(number);
            }
        }
    }

    #[test]
    fn mixed_source_ids_reject_raw_ids_above_the_safe_bound() {
        assert_eq!(FeatureId::MAX_OSM_ID, 900_719_925_474_098);
        for osm_id in [FeatureId::MAX_OSM_ID + 1, u64::MAX] {
            for source in [
                FeatureSource::Node,
                FeatureSource::Way,
                FeatureSource::Relation,
            ] {
                assert!(
                    std::panic::catch_unwind(|| {
                        FeatureId::for_osm_feature(Layer::Pois, source, osm_id)
                    })
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn dispatch_clamps_node_zooms_and_preserves_world_edge() {
        let m = LayerMatch {
            layer: Layer::PlaceLabels,
            min_zoom: 10,
            max_zoom: 14,
            geom_expect: GeomExpect::Point,
            paint_rank: 0,
            attrs: smallvec![],
        };
        let ctx = EmitCtx {
            window: ZoomRange::new(12, 12).expect("z12 window"),
            fanout_caps: &[],
            polygon_simplify_factor: 1.0,
        };
        let p = pt(1.0, 0.500_1);
        let mut records = Vec::new();
        let mut point = PointEmitScratch::new();
        assert_eq!(
            emit_match(
                7,
                FeatureGeometry::Node(&p),
                &m,
                &ctx,
                &mut records,
                &mut point
            ),
            Ok(1)
        );
        let (z, x, _) = records[0].key.tile().coord();
        assert_eq!((z, x), (12, 4095));
    }

    #[test]
    fn area_plain_point_and_line_are_explicit_skips() {
        // Entire square fits inside tile (2048, 2048) at z12.
        let outer = square(0.500_10, 0.500_11);
        let keys = FxHashSet::default();
        let mut polygon = MultipolygonEmitScratch::new();
        let mut point = PointEmitScratch::new();
        let mut fanout = super::super::stats::FanoutStats::new();
        let mut records = Vec::new();
        let ctx = EmitCtx {
            window: ZoomRange::new(12, 12).expect("z12 window"),
            fanout_caps: &[],
            polygon_simplify_factor: 1.0,
        };
        for geom_expect in [GeomExpect::Point, GeomExpect::Line] {
            let m = LayerMatch {
                layer: Layer::Boundaries,
                min_zoom: 12,
                max_zoom: 12,
                geom_expect,
                paint_rank: 0,
                attrs: smallvec![],
            };
            assert_eq!(
                emit_match(
                    7,
                    FeatureGeometry::Area {
                        outer: &outer,
                        inners: &[],
                        pins: &keys,
                        polygon: &mut polygon,
                        fanout: &mut fanout,
                    },
                    &m,
                    &ctx,
                    &mut records,
                    &mut point
                ),
                Ok(0)
            );
        }
        assert!(records.is_empty());
    }

    #[test]
    fn relation_pins_survive_world_translation() {
        let mut keys = FxHashSet::default();
        keys.insert(merc_point_key(&Point::new(0.25, 0.5)));
        keys.insert(merc_point_key(&Point::new(1.0, 0.5)));
        for x in [-0.75, 0.25, 1.25] {
            assert!(relation_vertex_is_pinned(&keys, &Point::new(x, 0.5)));
        }
        assert!(relation_vertex_is_pinned(&keys, &Point::new(0.0, 0.5)));
        assert!(!relation_vertex_is_pinned(&keys, &Point::new(0.5, 0.5)));
    }

    #[test]
    fn fanout_count_includes_buffer_neighbours() {
        // The bbox fits entirely inside one tile, but all four buffered
        // cells at this corner can receive its polygon.
        let rect = IntRect {
            min_x: 4100,
            min_y: 4100,
            max_x: 4200,
            max_y: 4200,
        };
        assert_eq!(tile_count_for_base_rect(rect, 2, 2), 4);
        let rect_base = IntRect {
            min_x: rect.min_x << 12,
            min_y: rect.min_y << 12,
            max_x: rect.max_x << 12,
            max_y: rect.max_y << 12,
        };
        assert_eq!(tile_count_for_base_rect(rect_base, 14, 2), 4);
    }

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
            if z < OSM_POLYGON_MAX_Z && geometry::simplify::merc_bbox_is_subpixel(merc, z) {
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
        let z_bottom = polygon_z_bottom_after_caps(
            &shape,
            0,
            4,
            Layer::Buildings,
            777,
            NonZeroU32::new(4),
            &mut cap_events,
        )
        .expect("low zooms stay below cap");
        assert_eq!(z_bottom, 1);
        let capped_zooms: Vec<u8> = cap_events.iter().map(|event| event.at.zoom()).collect();
        assert_eq!(capped_zooms, vec![2, 3, 4]);
        assert!(
            cap_events
                .windows(2)
                .all(|w| w[0].bbox_tiles <= w[1].bbox_tiles)
        );
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
            None,
            1.0,
        )
        .expect("emission succeeds");
        assert_eq!(
            usize::try_from(count).expect("record count fits usize"),
            records.len()
        );
        assert!(!records.is_empty());

        for rec in &records {
            let tile_id = rec.key.tile();
            let z = tile_id.zoom();
            let want_count = if z >= 13 { 2 } else { 1 };
            assert_eq!(attr_count(&rec.data), want_count, "z{z}");

            let mut lb = mvt::LayerBuilder::new("test");
            crate::wire_format::add_feature_to_layer(&mut lb, &rec.data)
                .expect("emitted record decodes");
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
