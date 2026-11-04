// Street infrastructure layers: streets, street_polygons, street_labels,
// street_labels_points, streets_polygons_labels, bridges.

use super::*;
use smallvec::{SmallVec, smallvec};

const STREET_FLAG_ATTRIBUTE_MIN_ZOOM: u8 = 11;

// ---------------------------------------------------------------------------
// Streets (line)
// ---------------------------------------------------------------------------

/// Returns (kind_raw, min_zoom, is_railway) for the first listed match.
fn street_match(tags: &Tags<'_>) -> Option<(&'static str, u8, bool)> {
    let (kind, mut zoom, rail) = transport::first_tag_match(
        tags,
        &[
            ("highway", "motorway", ("motorway", 5, false)),
            ("highway", "trunk", ("trunk", 6, false)),
            ("highway", "motorway_link", ("motorway_link", 9, false)),
            ("highway", "trunk_link", ("trunk_link", 9, false)),
            ("highway", "primary", ("primary", 8, false)),
            ("highway", "primary_link", ("primary_link", 8, false)),
            ("highway", "secondary", ("secondary", 9, false)),
            ("highway", "secondary_link", ("secondary_link", 9, false)),
            ("highway", "tertiary", ("tertiary", 10, false)),
            ("highway", "tertiary_link", ("tertiary_link", 10, false)),
            ("highway", "unclassified", ("unclassified", 12, false)),
            ("highway", "residential", ("residential", 12, false)),
            ("highway", "busway", ("busway", 12, false)),
            ("highway", "bus_guideway", ("bus_guideway", 12, false)),
            ("highway", "living_street", ("living_street", 13, false)),
            ("highway", "service", ("service", 13, false)),
            ("highway", "pedestrian", ("pedestrian", 13, false)),
            ("highway", "track", ("track", 13, false)),
            ("highway", "footway", ("footway", 13, false)),
            ("highway", "steps", ("steps", 13, false)),
            ("highway", "path", ("path", 13, false)),
            ("highway", "cycleway", ("cycleway", 13, false)),
            ("aeroway", "runway", ("runway", 11, false)),
            ("aeroway", "taxiway", ("taxiway", 13, false)),
            ("railway", "rail", ("rail", 10, true)),
            ("railway", "narrow_gauge", ("narrow_gauge", 10, true)),
            ("railway", "light_rail", ("light_rail", 10, true)),
            ("railway", "subway", ("subway", 10, true)),
            ("railway", "tram", ("tram", 10, true)),
            ("railway", "funicular", ("funicular", 10, true)),
            ("railway", "monorail", ("monorail", 10, true)),
        ],
    )?;
    // The z8 service override applies to both rail kinds with a non-empty
    // service value. Empty and missing service follow the z10 rule.
    if rail
        && matches!(kind, "rail" | "narrow_gauge")
        && tags
            .get("service")
            .is_some_and(|service| !service.is_empty())
    {
        zoom = 8;
    }
    Some((kind, zoom, rail))
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
        attrs.push(attr_bool_z("link", true, STREET_FLAG_ATTRIBUTE_MIN_ZOOM));
    }
    if is_rail {
        attrs.push(attr_bool("rail", true));
    }
    if is_tunnel(tags) {
        attrs.push(attr_bool_z("tunnel", true, STREET_FLAG_ATTRIBUTE_MIN_ZOOM));
    }
    if is_bridge(tags) {
        attrs.push(attr_bool_z("bridge", true, STREET_FLAG_ATTRIBUTE_MIN_ZOOM));
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
    let paint_rank =
        paint_order::street_paint_rank(kind, is_link, is_tunnel(tags), is_bridge(tags));
    out.push(LayerMatch {
        layer: Layer::Streets,
        min_zoom,
        max_zoom: 14,
        geom_expect: GeomExpect::Line,
        paint_rank,
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
        let paint_rank =
            paint_order::street_paint_rank(kind, false, is_tunnel(tags), is_bridge(tags));
        out.push(LayerMatch {
            layer: Layer::StreetPolygons,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            paint_rank,
            attrs,
        });
    }
}

fn street_polygon_match(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    transport::first_tag_match(
        tags,
        &[
            ("area:aeroway", "runway", ("runway", 11)),
            ("area:aeroway", "taxiway", ("taxiway", 13)),
            ("highway", "pedestrian", ("pedestrian", 14)),
            ("highway", "service", ("service", 14)),
        ],
    )
}

// ---------------------------------------------------------------------------
// Street labels
// ---------------------------------------------------------------------------

pub(super) fn match_street_labels_line(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    let has_ref = tags.has("ref");
    if !has_name(tags) && !has_ref {
        return;
    }
    let (kind, min_zoom) = match street_label_kind_and_zoom(tags) {
        Some(class) => class,
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
            .map(|s| s.chars().count())
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
        paint_rank: 0,
        attrs,
    });
}

fn street_label_kind_and_zoom(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    transport::first_tag_match(
        tags,
        &[
            ("highway", "motorway", ("motorway", 10)),
            ("highway", "trunk", ("trunk", 12)),
            ("highway", "primary", ("primary", 12)),
            ("highway", "secondary", ("secondary", 13)),
            ("highway", "tertiary", ("tertiary", 13)),
            ("highway", "motorway_link", ("motorway_link", 13)),
            ("highway", "trunk_link", ("trunk_link", 13)),
            ("highway", "primary_link", ("primary_link", 13)),
            ("highway", "secondary_link", ("secondary_link", 13)),
            ("highway", "tertiary_link", ("tertiary_link", 14)),
            ("highway", "unclassified", ("unclassified", 14)),
            ("highway", "residential", ("residential", 14)),
            ("highway", "busway", ("busway", 14)),
            ("highway", "bus_guideway", ("bus_guideway", 14)),
            ("highway", "living_street", ("living_street", 14)),
            ("highway", "service", ("service", 14)),
            ("highway", "pedestrian", ("pedestrian", 14)),
            ("highway", "track", ("track", 14)),
            ("highway", "footway", ("footway", 14)),
            ("highway", "steps", ("steps", 14)),
            ("highway", "path", ("path", 14)),
            ("highway", "cycleway", ("cycleway", 14)),
            ("aeroway", "runway", ("runway", 11)),
            ("aeroway", "taxiway", ("taxiway", 13)),
            ("railway", "rail", ("rail", 10)),
            ("railway", "narrow_gauge", ("narrow_gauge", 10)),
            ("railway", "light_rail", ("light_rail", 10)),
            ("railway", "subway", ("subway", 10)),
            ("railway", "tram", ("tram", 10)),
            ("railway", "funicular", ("funicular", 10)),
            ("railway", "monorail", ("monorail", 10)),
        ],
    )
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
        paint_rank: 0,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Streets polygons labels
// ---------------------------------------------------------------------------

pub(super) fn match_streets_polygons_labels(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if !has_name(tags) {
        return;
    }
    if let Some(kind) = street_polygon_label_kind(tags) {
        let mut attrs = smallvec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        out.push(LayerMatch {
            layer: Layer::StreetsPolygonsLabels,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect: Layer::StreetsPolygonsLabels.polygon_point_geom_expect(),
            paint_rank: 0,
            attrs,
        });
    }
}

fn street_polygon_label_kind(tags: &Tags<'_>) -> Option<&'static str> {
    transport::first_tag_match(
        tags,
        &[
            ("aeroway", "runway", "runway"),
            ("aeroway", "taxiway", "taxiway"),
            ("highway", "pedestrian", "pedestrian"),
            ("highway", "service", "service"),
        ],
    )
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
            paint_rank: 0,
            attrs: smallvec![attr_str("kind", "bridge")],
        });
    }
}
