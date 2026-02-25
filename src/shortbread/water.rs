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
        return match v {
            "water" => Some(("water", 4)),
            "glacier" => Some(("glacier", 4)),
            _ => None,
        };
    }
    if let Some(v) = tags.get("waterway") {
        return match v {
            "riverbank" => Some(("riverbank", 4)),
            "dock" => Some(("dock", 10)),
            "canal" => Some(("canal", 10)),
            _ => None,
        };
    }
    if let Some(v) = tags.get("landuse") {
        return match v {
            "reservoir" => Some(("reservoir", 4)),
            "basin" => Some(("basin", 4)),
            _ => None,
        };
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
