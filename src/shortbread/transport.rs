// Transport layers: aerialways, ferries, public_transport.

use super::*;
use smallvec::{SmallVec, smallvec};

// ---------------------------------------------------------------------------
// Aerialways
// ---------------------------------------------------------------------------

pub(super) fn match_aerialways(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(kind) = aerialway_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::Aerialways,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            attrs: smallvec![attr_str("kind", kind)],
        });
    }
}

fn aerialway_kind(tags: &Tags<'_>) -> Option<&'static str> {
    let v = tags.get("aerialway")?;
    match v {
        "cable_car" => Some("cable_car"),
        "gondola" => Some("gondola"),
        "goods" => Some("goods"),
        "chair_lift" => Some("chair_lift"),
        "drag_lift" => Some("drag_lift"),
        "t-bar" => Some("t-bar"),
        "j-bar" => Some("j-bar"),
        "platter" => Some("platter"),
        "rope_tow" => Some("rope_tow"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Ferries
// ---------------------------------------------------------------------------

pub(super) fn match_ferries(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if !tags.has_value("route", "ferry") {
        return;
    }
    let min_zoom = if tags.get("motor_vehicle") == Some("no") {
        12
    } else {
        10
    };
    let mut attrs = smallvec![attr_str("kind", "ferry")];
    attrs.extend(name_attrs(tags));
    out.push(LayerMatch {
        layer: Layer::Ferries,
        min_zoom,
        max_zoom: 14,
        geom_expect: GeomExpect::Line,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Public transport
// ---------------------------------------------------------------------------

pub(super) fn match_public_transport_point(
    tags: &Tags<'_>,
    out: &mut SmallVec<[LayerMatch; 4]>,
) {
    if let Some((kind, min_zoom)) = public_transport_match(tags) {
        let mut attrs = smallvec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        if let Some(v) = tags.get("iata") {
            attrs.push(attr_dyn("iata", v));
        }
        out.push(LayerMatch {
            layer: Layer::PublicTransport,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Point,
            attrs,
        });
    }
}

pub(super) fn match_public_transport_centroid(
    tags: &Tags<'_>,
    out: &mut SmallVec<[LayerMatch; 4]>,
) {
    if let Some((kind, min_zoom)) = public_transport_match(tags) {
        let mut attrs = smallvec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        if let Some(v) = tags.get("iata") {
            attrs.push(attr_dyn("iata", v));
        }
        out.push(LayerMatch {
            layer: Layer::PublicTransport,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::PolygonPointOnSurface,
            attrs,
        });
    }
}

fn public_transport_match(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    if tags.has_value("aerialway", "station") {
        return Some(("aerialway_station", 13));
    }
    if let Some(v) = tags.get("aeroway") {
        return match v {
            "aerodrome" => Some(("aerodrome", 11)),
            "helipad" => Some(("helipad", 13)),
            _ => None,
        };
    }
    if let Some(v) = tags.get("amenity") {
        return match v {
            "bus_station" => Some(("bus_station", 13)),
            "ferry_terminal" => Some(("ferry_terminal", 12)),
            _ => None,
        };
    }
    if let Some(v) = tags.get("railway") {
        return match v {
            "station" => Some(("station", 13)),
            "halt" => Some(("halt", 13)),
            "tram_stop" => Some(("tram_stop", 14)),
            _ => None,
        };
    }
    if tags.has_value("highway", "bus_stop") {
        return Some(("bus_stop", 14));
    }
    None
}
