// Transport layers: aerialways, ferries, public_transport.

use super::*;
use smallvec::smallvec;

wire_kinds! {
    /// `kind` of aerialways: the OSM `aerialway` value, unchanged.
    pub(super) enum AerialwayKind {
        CableCar = "cable_car",
        Gondola = "gondola",
        Goods = "goods",
        ChairLift = "chair_lift",
        DragLift = "drag_lift",
        TBar = "t-bar",
        JBar = "j-bar",
        Platter = "platter",
        RopeTow = "rope_tow",
    }
}

wire_kinds! {
    /// `kind` of ferries.
    pub(super) enum FerryKind {
        Ferry = "ferry",
    }
}

wire_kinds! {
    /// `kind` of public_transport.
    pub(super) enum PublicTransportKind {
        Aerodrome = "aerodrome",
        Helipad = "helipad",
        BusStation = "bus_station",
        FerryTerminal = "ferry_terminal",
        Station = "station",
        Halt = "halt",
        TramStop = "tram_stop",
        BusStop = "bus_stop",
        AerialwayStation = "aerialway_station",
    }
}

// ---------------------------------------------------------------------------
// Aerialways
// ---------------------------------------------------------------------------

pub(super) fn match_aerialways(tags: &Tags<'_>, out: &mut Hits) {
    if let Some(kind) = aerialway_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::Aerialways,
            min_zoom: 12,
            geom_expect: GeomExpect::Line,
            paint_rank: 0,
            attrs: smallvec![attr_kind(kind)],
        });
    }
}

fn aerialway_kind(tags: &Tags<'_>) -> Option<AerialwayKind> {
    let v = tags.get("aerialway")?;
    AerialwayKind::ALL
        .into_iter()
        .find(|kind| kind.as_str() == v)
}

// ---------------------------------------------------------------------------
// Ferries
// ---------------------------------------------------------------------------

pub(super) fn match_ferries(tags: &Tags<'_>, out: &mut Hits) {
    if !tags.has_value("route", "ferry") {
        return;
    }
    let min_zoom = if tags.get("motor_vehicle") == Some("no") {
        12
    } else {
        10
    };
    let mut attrs = smallvec![attr_kind(FerryKind::Ferry)];
    attrs.extend(name_attrs(tags));
    out.push(LayerMatch {
        layer: Layer::Ferries,
        min_zoom,
        geom_expect: GeomExpect::Line,
        paint_rank: 0,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Public transport
// ---------------------------------------------------------------------------

pub(super) fn match_public_transport_point(tags: &Tags<'_>, out: &mut Hits) {
    match_public_transport(tags, GeomExpect::Point, out);
}

pub(super) fn match_public_transport_polygon(tags: &Tags<'_>, out: &mut Hits) {
    match_public_transport(
        tags,
        Layer::PublicTransport.polygon_point_geom_expect(),
        out,
    );
}

fn match_public_transport(tags: &Tags<'_>, geom_expect: GeomExpect, out: &mut Hits) {
    if let Some((kind, min_zoom)) = public_transport_match(tags) {
        let mut attrs = smallvec![attr_kind(kind)];
        attrs.extend(name_attrs(tags));
        if let Some(v) = tags.get("iata") {
            attrs.push(attr_dyn(StrKey::Iata, v));
        }
        out.push(LayerMatch {
            layer: Layer::PublicTransport,
            min_zoom,
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

fn public_transport_match(tags: &Tags<'_>) -> Option<(PublicTransportKind, u8)> {
    use PublicTransportKind as K;
    // The upstream raw coalesce can choose an unrelated present value, such
    // as aeroway=gate when railway=halt qualified the feature. Match allowed
    // key/value pairs in the upstream coalesce priority and keep aerialway's
    // special fallback last.
    first_tag_match(
        tags,
        &[
            ("aeroway", "aerodrome", (K::Aerodrome, 11)),
            ("aeroway", "helipad", (K::Helipad, 13)),
            ("amenity", "bus_station", (K::BusStation, 13)),
            ("amenity", "ferry_terminal", (K::FerryTerminal, 12)),
            ("railway", "station", (K::Station, 13)),
            ("railway", "halt", (K::Halt, 13)),
            ("railway", "tram_stop", (K::TramStop, 14)),
            ("highway", "bus_stop", (K::BusStop, 14)),
            ("aerialway", "station", (K::AerialwayStation, 13)),
        ],
    )
}
