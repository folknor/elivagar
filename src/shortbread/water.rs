// Water layers: water_polygons, water_polygons_labels, water_lines, water_lines_labels,
// dam_lines, dam_polygons, pier_lines, pier_polygons.

use super::*;
use smallvec::{SmallVec, smallvec};

// ---------------------------------------------------------------------------
// Water polygons + labels
// ---------------------------------------------------------------------------

/// Determine water polygon kind and min_zoom. Returns (kind, min_zoom).
fn water_polygon_match(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    if let Some(v) = tags.get("natural") {
        match v {
            "water" => return Some(("water", 4)),
            "glacier" => return Some(("glacier", 4)),
            _ => {}
        }
    }
    if let Some(v) = tags.get("waterway") {
        match v {
            "riverbank" => return Some(("riverbank", 4)),
            "dock" => return Some(("dock", 10)),
            "canal" => return Some(("canal", 10)),
            _ => {}
        }
    }
    if let Some(v) = tags.get("landuse") {
        match v {
            "reservoir" => return Some(("reservoir", 4)),
            "basin" => return Some(("basin", 4)),
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
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![attr_str("kind", kind)],
        });
    }
}

pub(super) fn match_water_polygons_labels(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if !has_name(tags) {
        return;
    }
    if let Some((kind, _base_zoom)) = water_polygon_match(tags) {
        let mut attrs = smallvec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        // All water polygon labels start at z14 per Shortbread spec.
        let label_zoom = 14;
        out.push(LayerMatch {
            layer: Layer::WaterPolygonsLabels,
            min_zoom: label_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::PolygonCentroid,
            paint_rank: 0,
            attrs,
        });
    }
}

// ---------------------------------------------------------------------------
// Water lines + labels
// ---------------------------------------------------------------------------

/// Determine water line kind and min_zoom.
fn water_line_match(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    let v = tags.get("waterway")?;
    match v {
        "canal" => Some(("canal", 9)),
        "river" => Some(("river", 9)),
        "stream" => Some(("stream", 14)),
        "ditch" => Some(("ditch", 14)),
        _ => None,
    }
}

pub(super) fn match_water_lines(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some((kind, min_zoom)) = water_line_match(tags) {
        let mut attrs = smallvec![attr_str("kind", kind)];
        if is_tunnel(tags) {
            attrs.push(attr_bool("tunnel", true));
        }
        if is_bridge(tags) {
            attrs.push(attr_bool("bridge", true));
        }
        out.push(LayerMatch {
            layer: Layer::WaterLines,
            min_zoom,
            max_zoom: 14,
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
        let mut attrs = smallvec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        if is_tunnel(tags) {
            attrs.push(attr_bool("tunnel", true));
        }
        if is_bridge(tags) {
            attrs.push(attr_bool("bridge", true));
        }
        // Label layer has its own min_zoom per Shortbread spec
        let label_zoom = match kind {
            "canal" | "river" => 12,
            _ => 14,
        };
        out.push(LayerMatch {
            layer: Layer::WaterLinesLabels,
            min_zoom: label_zoom,
            max_zoom: 14,
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
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            paint_rank: 0,
            attrs: smallvec![attr_str("kind", "dam")],
        });
    }
}

pub(super) fn match_dam_polygons(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if tags.has_value("waterway", "dam") {
        out.push(LayerMatch {
            layer: Layer::DamPolygons,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![attr_str("kind", "dam")],
        });
    }
}

pub(super) fn match_pier_lines(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(kind) = pier_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::PierLines,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            paint_rank: 0,
            attrs: smallvec![attr_str("kind", kind)],
        });
    }
}

pub(super) fn match_pier_polygons(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(kind) = pier_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::PierPolygons,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![attr_str("kind", kind)],
        });
    }
}

fn pier_kind(tags: &Tags<'_>) -> Option<&'static str> {
    let v = tags.get("man_made")?;
    match v {
        "pier" => Some("pier"),
        "breakwater" => Some("breakwater"),
        "groyne" => Some("groyne"),
        _ => None,
    }
}
