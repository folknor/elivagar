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
            paint_rank: 0,
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
        paint_rank: 0,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Public transport
// ---------------------------------------------------------------------------

pub(super) fn match_public_transport_point(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    match_public_transport(tags, GeomExpect::Point, out);
}

pub(super) fn match_public_transport_centroid(
    tags: &Tags<'_>,
    out: &mut SmallVec<[LayerMatch; 4]>,
) {
    match_public_transport(tags, GeomExpect::PolygonPointOnSurface, out);
}

fn match_public_transport(
    tags: &Tags<'_>,
    geom_expect: GeomExpect,
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
            geom_expect,
            paint_rank: 0,
            attrs,
        });
    }
}

// Walk specific key/value rows in priority order. An unlisted value never
// blocks a later row, and tag storage order does not select the winner.
pub(super) fn first_tag_match<T: Copy>(tags: &Tags<'_>, rows: &[(&str, &str, T)]) -> Option<T> {
    rows.iter()
        .find(|(key, value, _)| tags.has_value(key, value))
        .map(|(_, _, result)| *result)
}

fn public_transport_match(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    first_tag_match(
        tags,
        &[
            ("aerialway", "station", ("aerialway_station", 13)),
            ("aeroway", "aerodrome", ("aerodrome", 11)),
            ("aeroway", "helipad", ("helipad", 13)),
            ("amenity", "bus_station", ("bus_station", 13)),
            ("amenity", "ferry_terminal", ("ferry_terminal", 12)),
            ("railway", "station", ("station", 13)),
            ("railway", "halt", ("halt", 13)),
            ("railway", "tram_stop", ("tram_stop", 14)),
            ("highway", "bus_stop", ("bus_stop", 14)),
        ],
    )
}
