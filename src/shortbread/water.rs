// Water layers: water_polygons, water_polygons_labels, water_lines, water_lines_labels,
// dam_lines, dam_polygons, pier_lines, pier_polygons.

use super::*;
use smallvec::{SmallVec, smallvec};

wire_kinds! {
    /// `kind` of water_polygons and water_polygons_labels.
    pub(super) enum WaterPolygonKind {
        Water = "water",
        Glacier = "glacier",
        Riverbank = "riverbank",
        Dock = "dock",
        Canal = "canal",
        Reservoir = "reservoir",
        Basin = "basin",
    }
}

wire_kinds! {
    /// `kind` of water_lines and water_lines_labels.
    pub(super) enum WaterLineKind {
        Canal = "canal",
        River = "river",
        Stream = "stream",
        Ditch = "ditch",
    }
}

wire_kinds! {
    /// `kind` of dam_lines and dam_polygons.
    pub(super) enum DamKind {
        Dam = "dam",
    }
}

wire_kinds! {
    /// `kind` of pier_lines and pier_polygons.
    pub(super) enum PierKind {
        Pier = "pier",
        Breakwater = "breakwater",
        Groyne = "groyne",
    }
}

// ---------------------------------------------------------------------------
// Water polygons + labels
// ---------------------------------------------------------------------------

/// Determine water polygon kind and min_zoom. Returns (kind, min_zoom).
fn water_polygon_match(tags: &Tags<'_>) -> Option<(WaterPolygonKind, u8)> {
    use WaterPolygonKind as K;
    if let Some(v) = tags.get("natural") {
        match v {
            "water" => return Some((K::Water, 4)),
            "glacier" => return Some((K::Glacier, 4)),
            _ => {}
        }
    }
    if let Some(v) = tags.get("waterway") {
        match v {
            "riverbank" => return Some((K::Riverbank, 4)),
            "dock" => return Some((K::Dock, 10)),
            "canal" => return Some((K::Canal, 10)),
            _ => {}
        }
    }
    if let Some(v) = tags.get("landuse") {
        match v {
            "reservoir" => return Some((K::Reservoir, 4)),
            "basin" => return Some((K::Basin, 4)),
            _ => {}
        }
    }
    None
}

pub(super) fn match_water_polygons(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some((kind, min_zoom)) = water_polygon_match(tags) {
        out.push(LayerMatch {
            layer: Layer::WaterPolygons,
            min_zoom,
            max_zoom: MAX_ZOOM,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![attr_kind(kind)],
        });
    }
}

pub(super) fn match_water_polygons_labels(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if !has_name(tags) {
        return;
    }
    if let Some((kind, _base_zoom)) = water_polygon_match(tags) {
        let mut attrs = smallvec![attr_kind(kind)];
        attrs.extend(name_attrs(tags));
        // All water polygon labels start at z14 per Shortbread spec.
        let label_zoom = 14;
        out.push(LayerMatch {
            layer: Layer::WaterPolygonsLabels,
            min_zoom: label_zoom,
            max_zoom: MAX_ZOOM,
            geom_expect: Layer::WaterPolygonsLabels.polygon_point_geom_expect(),
            paint_rank: 0,
            attrs,
        });
    }
}

// ---------------------------------------------------------------------------
// Water lines + labels
// ---------------------------------------------------------------------------

/// Determine water line kind and min_zoom.
fn water_line_match(tags: &Tags<'_>) -> Option<(WaterLineKind, u8)> {
    let v = tags.get("waterway")?;
    match v {
        "canal" => Some((WaterLineKind::Canal, 9)),
        "river" => Some((WaterLineKind::River, 9)),
        "stream" => Some((WaterLineKind::Stream, 14)),
        "ditch" => Some((WaterLineKind::Ditch, 14)),
        _ => None,
    }
}

pub(super) fn match_water_lines(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some((kind, min_zoom)) = water_line_match(tags) {
        let mut attrs = smallvec![attr_kind(kind)];
        if is_tunnel(tags) {
            attrs.push(attr_bool("tunnel", true));
        }
        if is_bridge(tags) {
            attrs.push(attr_bool("bridge", true));
        }
        out.push(LayerMatch {
            layer: Layer::WaterLines,
            min_zoom,
            max_zoom: MAX_ZOOM,
            geom_expect: GeomExpect::Line,
            paint_rank: 0,
            attrs,
        });
    }
}

pub(super) fn match_water_lines_labels(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if !has_name(tags) {
        return;
    }
    if let Some((kind, _base_zoom)) = water_line_match(tags) {
        let mut attrs = smallvec![attr_kind(kind)];
        attrs.extend(name_attrs(tags));
        if is_tunnel(tags) {
            attrs.push(attr_bool("tunnel", true));
        }
        if is_bridge(tags) {
            attrs.push(attr_bool("bridge", true));
        }
        // Label layer has its own min_zoom per Shortbread spec
        let label_zoom = match kind {
            WaterLineKind::Canal | WaterLineKind::River => 12,
            WaterLineKind::Stream | WaterLineKind::Ditch => 14,
        };
        out.push(LayerMatch {
            layer: Layer::WaterLinesLabels,
            min_zoom: label_zoom,
            max_zoom: MAX_ZOOM,
            geom_expect: GeomExpect::Line,
            paint_rank: 0,
            attrs,
        });
    }
}

// ---------------------------------------------------------------------------
// Dam + Pier
// ---------------------------------------------------------------------------

pub(super) fn match_dam_lines(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if tags.has_value("waterway", "dam") {
        out.push(LayerMatch {
            layer: Layer::DamLines,
            min_zoom: 12,
            max_zoom: MAX_ZOOM,
            geom_expect: GeomExpect::Line,
            paint_rank: 0,
            attrs: smallvec![attr_kind(DamKind::Dam)],
        });
    }
}

pub(super) fn match_dam_polygons(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if tags.has_value("waterway", "dam") {
        out.push(LayerMatch {
            layer: Layer::DamPolygons,
            min_zoom: 12,
            max_zoom: MAX_ZOOM,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![attr_kind(DamKind::Dam)],
        });
    }
}

pub(super) fn match_pier_lines(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(kind) = pier_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::PierLines,
            min_zoom: 12,
            max_zoom: MAX_ZOOM,
            geom_expect: GeomExpect::Line,
            paint_rank: 0,
            attrs: smallvec![attr_kind(kind)],
        });
    }
}

pub(super) fn match_pier_polygons(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(kind) = pier_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::PierPolygons,
            min_zoom: 12,
            max_zoom: MAX_ZOOM,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![attr_kind(kind)],
        });
    }
}

fn pier_kind(tags: &Tags<'_>) -> Option<PierKind> {
    let v = tags.get("man_made")?;
    match v {
        "pier" => Some(PierKind::Pier),
        "breakwater" => Some(PierKind::Breakwater),
        "groyne" => Some(PierKind::Groyne),
        _ => None,
    }
}
