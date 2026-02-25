// Street infrastructure layers: streets, street_polygons, street_labels,
// street_labels_points, streets_polygons_labels, bridges.

use super::*;
use smallvec::{SmallVec, smallvec};

// ---------------------------------------------------------------------------
// Streets (line)
// ---------------------------------------------------------------------------

/// Returns (kind_raw, min_zoom, is_railway) for a highway/aeroway/railway.
fn street_match<'a>(tags: &'a Tags<'a>) -> Option<(&'a str, u8, bool)> {
    if let Some(v) = tags.get("highway") {
        return highway_zoom(v).map(|z| (v, z, false));
    }
    if let Some(v) = tags.get("aeroway") {
        return aeroway_zoom(v).map(|z| (v, z, false));
    }
    if let Some(v) = tags.get("railway") {
        return railway_zoom(tags, v).map(|z| (v, z, true));
    }
    None
}

fn highway_zoom(v: &str) -> Option<u8> {
    match v {
        "motorway" => Some(5),
        "trunk" => Some(6),
        "motorway_link" => Some(9),
        "trunk_link" => Some(9),
        "primary" | "primary_link" => Some(8),
        "secondary" | "secondary_link" => Some(9),
        "tertiary" | "tertiary_link" => Some(10),
        "unclassified" => Some(12),
        "residential" => Some(12),
        "busway" | "bus_guideway" => Some(12),
        "living_street" | "service" | "pedestrian" | "track" | "footway"
        | "steps" | "path" | "cycleway" => Some(13),
        _ => None,
    }
}

fn aeroway_zoom(v: &str) -> Option<u8> {
    match v {
        "runway" => Some(11),
        "taxiway" => Some(13),
        _ => None,
    }
}

fn railway_zoom(tags: &Tags<'_>, v: &str) -> Option<u8> {
    match v {
        "rail" => {
            if tags.has("service") {
                Some(8)
            } else {
                Some(10)
            }
        }
        "narrow_gauge" => Some(10),
        "light_rail" | "subway" | "tram" | "funicular" | "monorail" => {
            Some(10)
        }
        _ => None,
    }
}

pub(super) fn match_streets_line(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    let (kind_raw, min_zoom, is_rail) = match street_match(tags) {
        Some(v) => v,
        None => return,
    };
    let kind = street_kind(kind_raw);
    let is_link = kind_raw.ends_with("_link");
    // kind: always emitted. Boolean attrs only when true (saves ~3-6 tags/feature).
    let mut attrs = smallvec![attr_dyn("kind", kind)];
    if is_link {
        attrs.push(attr_bool_z("link", true, 11));
    }
    if is_rail {
        attrs.push(attr_bool("rail", true));
    }
    if is_tunnel(tags) {
        attrs.push(attr_bool_z("tunnel", true, 11));
    }
    if is_bridge(tags) {
        attrs.push(attr_bool_z("bridge", true, 11));
    }
    // Oneway: only for non-railway, only when true
    if !is_rail {
        let oneway_val = tags.get("oneway");
        let is_oneway = matches!(
            oneway_val,
            Some("yes") | Some("1") | Some("true") | Some("-1")
        );
        let is_reverse = oneway_val == Some("-1");
        if is_oneway {
            attrs.push(attr_bool_z("oneway", true, 14));
        }
        if is_reverse {
            attrs.push(attr_bool_z("oneway_reverse", true, 14));
        }
    }
    // Optional string attrs
    if let Some(v) = tags.get("tracktype") {
        attrs.push(attr_dyn_z("tracktype", v, 11));
    }
    if let Some(v) = tags.get("surface") {
        attrs.push(attr_dyn_z("surface", v, 11));
    }
    if let Some(v) = tags.get("service") {
        attrs.push(attr_dyn_z("service", v, 11));
    }
    if let Some(v) = tags.get("bicycle") {
        attrs.push(attr_dyn_z("bicycle", v, 14));
    }
    if let Some(v) = tags.get("horse") {
        attrs.push(attr_dyn_z("horse", v, 14));
    }
    out.push(LayerMatch {
        layer: Layer::Streets,
        min_zoom,
        max_zoom: 14,
        geom_expect: GeomExpect::Line,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Street polygons
// ---------------------------------------------------------------------------

pub(super) fn match_street_polygons(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some((kind, min_zoom)) = street_polygon_match(tags) {
        let mut attrs = smallvec![attr_str("kind", kind)];
        if is_bridge(tags) {
            attrs.push(attr_bool("bridge", true));
        }
        if is_tunnel(tags) {
            attrs.push(attr_bool("tunnel", true));
        }
        if let Some(v) = tags.get("service") {
            attrs.push(attr_dyn("service", v));
        }
        if let Some(v) = tags.get("surface") {
            attrs.push(attr_dyn("surface", v));
        }
        out.push(LayerMatch {
            layer: Layer::StreetPolygons,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs,
        });
    }
}

fn street_polygon_match(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    if let Some(v) = tags.get("area:aeroway") {
        return match v {
            "runway" => Some(("runway", 11)),
            "taxiway" => Some(("taxiway", 13)),
            _ => None,
        };
    }
    if let Some(v) = tags.get("highway") {
        return match v {
            "pedestrian" => Some(("pedestrian", 14)),
            "service" => Some(("service", 14)),
            _ => None,
        };
    }
    None
}

// ---------------------------------------------------------------------------
// Street labels
// ---------------------------------------------------------------------------

pub(super) fn match_street_labels_line(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    let has_ref = tags.has("ref");
    if !has_name(tags) && !has_ref {
        return;
    }
    let min_zoom = match street_label_zoom(tags) {
        Some(z) => z,
        None => return,
    };
    let kind = match street_label_kind_raw(tags) {
        Some(k) => k, // street_labels keeps raw kind (including _link suffix)
        None => return,
    };
    let mut attrs = smallvec![attr_dyn("kind", kind)];
    attrs.extend(name_attrs(tags));
    // ref: semicolons → newlines
    if let Some(r) = tags.get("ref") {
        let replaced = r.replace(';', "\n");
        let ref_rows = replaced.lines().count();
        let ref_cols = replaced
            .lines()
            .map(str::len)
            .max()
            .unwrap_or(0);
        attrs.push(attr_dyn("ref", &replaced));
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        {
            attrs.push(attr_int("ref_rows", ref_rows as i64));
            attrs.push(attr_int("ref_cols", ref_cols as i64));
        }
    }
    if is_tunnel(tags) {
        attrs.push(attr_bool("tunnel", true));
    }
    out.push(LayerMatch {
        layer: Layer::StreetLabels,
        min_zoom,
        max_zoom: 14,
        geom_expect: GeomExpect::Line,
        attrs,
    });
}

fn street_label_kind_raw<'a>(tags: &Tags<'a>) -> Option<&'a str> {
    if let Some(v) = tags.get("highway")
        && highway_zoom(v).is_some()
    {
        return Some(v);
    }
    if let Some(v) = tags.get("aeroway")
        && aeroway_zoom(v).is_some()
    {
        return Some(v);
    }
    if let Some(v) = tags.get("railway")
        && railway_zoom(tags, v).is_some()
    {
        return Some(v);
    }
    None
}

fn street_label_zoom(tags: &Tags<'_>) -> Option<u8> {
    if let Some(v) = tags.get("highway") {
        return street_label_zoom_highway(v);
    }
    if let Some(v) = tags.get("railway") {
        return match v {
            "rail" | "narrow_gauge" | "light_rail" | "subway" | "tram"
            | "funicular" | "monorail" => Some(10),
            _ => None,
        };
    }
    if let Some(v) = tags.get("aeroway") {
        return match v {
            "runway" => Some(11),
            "taxiway" => Some(13),
            _ => None,
        };
    }
    None
}

fn street_label_zoom_highway(v: &str) -> Option<u8> {
    match v {
        "motorway" => Some(10),
        "trunk" | "primary" => Some(12),
        "secondary" | "tertiary" | "motorway_link" | "trunk_link"
        | "primary_link" | "secondary_link" => Some(13),
        "tertiary_link" | "unclassified" | "residential" | "busway"
        | "bus_guideway" | "living_street" | "service" | "pedestrian"
        | "track" | "footway" | "steps" | "path" | "cycleway" => Some(14),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Street label points
// ---------------------------------------------------------------------------

pub(super) fn match_street_labels_points(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if !tags.has_value("highway", "motorway_junction") {
        return;
    }
    let mut attrs = smallvec![attr_str("kind", "motorway_junction")];
    if let Some(v) = tags.get("ref") {
        attrs.push(attr_dyn("ref", v));
    }
    attrs.extend(name_attrs(tags));
    out.push(LayerMatch {
        layer: Layer::StreetLabelsPoints,
        min_zoom: 12,
        max_zoom: 14,
        geom_expect: GeomExpect::Point,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Streets polygons labels
// ---------------------------------------------------------------------------

pub(super) fn match_streets_polygons_labels(
    tags: &Tags<'_>,
    out: &mut SmallVec<[LayerMatch; 4]>,
) {
    if !has_name(tags) {
        return;
    }
    if let Some((kind, _)) = street_polygon_match(tags) {
        let mut attrs = smallvec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        out.push(LayerMatch {
            layer: Layer::StreetsPolygonsLabels,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect: GeomExpect::PolygonPointOnSurface,
            attrs,
        });
    }
}

// ---------------------------------------------------------------------------
// Bridges
// ---------------------------------------------------------------------------

pub(super) fn match_bridges(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if tags.has_value("man_made", "bridge") {
        out.push(LayerMatch {
            layer: Layer::Bridges,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: smallvec![attr_str("kind", "bridge")],
        });
    }
}
