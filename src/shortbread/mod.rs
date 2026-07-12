// Shortbread vector tile schema - hardcoded Rust implementation.
//
// Reference: <https://shortbread.geofabrik.de/>
// No YAML parsing - every layer, filter, and attribute mapping is compiled code.
//
// Layer matching is split by domain:
//   water.rs      - water polygons/lines/labels, dam, pier
//   boundaries.rs - boundaries, boundary_labels, place_labels
//   land.rs       - land, sites, buildings, addresses
//   streets.rs    - streets, street_polygons, street_labels, bridges
//   transport.rs  - aerialways, ferries, public_transport
//   (pois.rs lives at crate root - see src/pois.rs)

use smallvec::SmallVec;
use std::borrow::Cow;

pub(crate) mod boundaries;
mod land;
pub(crate) mod paint_order;
mod streets;
mod transport;
mod water;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// MVT layer in the Shortbread schema.
/// `#[repr(u8)]` so `layer as u8` is a zero-cost index into layer arrays.
/// No `from_index()` needed - layer indices are only used as array offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Layer {
    WaterPolygons = 0,
    WaterPolygonsLabels = 1,
    WaterLines = 2,
    WaterLinesLabels = 3,
    DamLines = 4,
    DamPolygons = 5,
    PierLines = 6,
    PierPolygons = 7,
    Boundaries = 8,
    BoundaryLabels = 9,
    PlaceLabels = 10,
    Land = 11,
    Sites = 12,
    Buildings = 13,
    Addresses = 14,
    Streets = 15,
    StreetPolygons = 16,
    StreetLabels = 17,
    StreetLabelsPoints = 18,
    StreetsPolygonsLabels = 19,
    Bridges = 20,
    Aerialways = 21,
    Ferries = 22,
    PublicTransport = 23,
    Pois = 24,
    Ocean = 25,
}

impl Layer {
    /// All layers in enum order.
    pub const ALL: [Layer; 26] = [
        Self::WaterPolygons,
        Self::WaterPolygonsLabels,
        Self::WaterLines,
        Self::WaterLinesLabels,
        Self::DamLines,
        Self::DamPolygons,
        Self::PierLines,
        Self::PierPolygons,
        Self::Boundaries,
        Self::BoundaryLabels,
        Self::PlaceLabels,
        Self::Land,
        Self::Sites,
        Self::Buildings,
        Self::Addresses,
        Self::Streets,
        Self::StreetPolygons,
        Self::StreetLabels,
        Self::StreetLabelsPoints,
        Self::StreetsPolygonsLabels,
        Self::Bridges,
        Self::Aerialways,
        Self::Ferries,
        Self::PublicTransport,
        Self::Pois,
        Self::Ocean,
    ];

    /// MVT layer name string.
    pub fn name(self) -> &'static str {
        match self {
            Self::WaterPolygons => "water_polygons",
            Self::WaterPolygonsLabels => "water_polygons_labels",
            Self::WaterLines => "water_lines",
            Self::WaterLinesLabels => "water_lines_labels",
            Self::DamLines => "dam_lines",
            Self::DamPolygons => "dam_polygons",
            Self::PierLines => "pier_lines",
            Self::PierPolygons => "pier_polygons",
            Self::Boundaries => "boundaries",
            Self::BoundaryLabels => "boundary_labels",
            Self::PlaceLabels => "place_labels",
            Self::Land => "land",
            Self::Sites => "sites",
            Self::Buildings => "buildings",
            Self::Addresses => "addresses",
            Self::Streets => "streets",
            Self::StreetPolygons => "street_polygons",
            Self::StreetLabels => "street_labels",
            Self::StreetLabelsPoints => "street_labels_points",
            Self::StreetsPolygonsLabels => "streets_polygons_labels",
            Self::Bridges => "bridges",
            Self::Aerialways => "aerialways",
            Self::Ferries => "ferries",
            Self::PublicTransport => "public_transport",
            Self::Pois => "pois",
            Self::Ocean => "ocean",
        }
    }

    /// Minimum zoom level at which this layer appears in tiles.
    pub fn min_zoom(self) -> u8 {
        match self {
            Self::Boundaries => 0,
            Self::Ocean => 0,
            Self::BoundaryLabels => 2,
            Self::WaterPolygons | Self::WaterPolygonsLabels | Self::PlaceLabels => 4,
            Self::Streets => 5,
            Self::Land => 7,
            Self::WaterLines | Self::WaterLinesLabels => 9,
            Self::StreetLabels | Self::Ferries => 10,
            Self::StreetPolygons | Self::PublicTransport => 11,
            Self::DamLines
            | Self::DamPolygons
            | Self::PierLines
            | Self::PierPolygons
            | Self::StreetLabelsPoints
            | Self::Bridges
            | Self::Aerialways => 12,
            Self::Sites
            | Self::Buildings
            | Self::Addresses
            | Self::StreetsPolygonsLabels
            | Self::Pois => 14,
        }
    }

    /// Parse a layer name string (e.g. "water_polygons") into a Layer variant.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|l| l.name() == name)
    }

    /// Total number of layers.
    pub const fn count() -> usize {
        Self::ALL.len()
    }
}

/// What geometry the MVT feature should carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeomExpect {
    Point,
    Line,
    Polygon,
    PolygonCentroid,
    PolygonPointOnSurface,
}

/// Classification of the incoming OSM element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsmGeomType {
    Node,
    OpenWay,
    ClosedWay,
    MultiPolygon,
}

/// Tag lookup helper. Uses linear scan - with 3-15 tags per OSM element,
/// this is faster than sorting + binary search. Binary search was tried
/// (ca6f17e) and reverted: sort_unstable_by_key on every element plus
/// binary_search_by_key on every lookup added +55% to the PBF phase on
/// Denmark (20s → 31s). Linear scan wins at this size because the slice
/// fits in a cache line, key comparisons short-circuit on first byte
/// mismatch, and there's zero per-element setup cost.
pub struct Tags<'a>(pub &'a [(&'a str, &'a str)]);

impl<'a> Tags<'a> {
    pub fn get(&self, key: &str) -> Option<&'a str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| *k == key)
    }

    pub fn has_value(&self, key: &str, val: &str) -> bool {
        self.0.iter().any(|(k, v)| *k == key && *v == val)
    }

    pub fn has_any(&self, key: &str, vals: &[&str]) -> bool {
        self.0.iter().any(|(k, v)| *k == key && vals.contains(v))
    }
}

/// Attribute value for a layer match.
#[derive(Clone, Debug, PartialEq)]
pub enum AttrValue {
    Str(Cow<'static, str>),
    Int(i64),
    Bool(bool),
    Float(f64),
}

/// A single attribute: (key, value, min_zoom).
/// min_zoom=0 means always emit; higher values mean only at that zoom+.
/// Kept as a tuple: destructuring is clear, and a named struct would bloat ~100 construction sites.
pub type Attr = (&'static str, AttrValue, u8);

/// A single layer match result.
/// SmallVec avoids heap allocation: most matches have 1-6 attrs (inline up to 8),
/// and most elements match 1-3 layers (match_element returns SmallVec<[_; 4]>).
pub struct LayerMatch {
    pub layer: Layer,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub geom_expect: GeomExpect,
    pub paint_rank: u8,
    pub attrs: SmallVec<[Attr; 8]>,
}
const _: () = assert!(std::mem::size_of::<LayerMatch>() == 408);

// ---------------------------------------------------------------------------
// Core dispatch
// ---------------------------------------------------------------------------

/// Match an OSM element against all Shortbread layers, returning every match.
#[hotpath::measure]
pub fn match_element(tags: &Tags<'_>, geom_type: OsmGeomType) -> SmallVec<[LayerMatch; 4]> {
    let mut out = SmallVec::new();
    match geom_type {
        OsmGeomType::Node => match_node(tags, &mut out),
        OsmGeomType::OpenWay => match_open_way(tags, &mut out),
        OsmGeomType::ClosedWay => match_closed_way(tags, &mut out),
        OsmGeomType::MultiPolygon => match_multipolygon(tags, &mut out),
    }
    out
}

fn match_node(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    boundaries::match_place_labels(tags, out);
    land::match_addresses_point(tags, out);
    streets::match_street_labels_points(tags, out);
    transport::match_public_transport_point(tags, out);
    pois::match_pois_point(tags, out);
}

fn match_open_way(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    water::match_water_lines(tags, out);
    water::match_water_lines_labels(tags, out);
    water::match_dam_lines(tags, out);
    water::match_pier_lines(tags, out);
    boundaries::match_boundaries_line(tags, out);
    streets::match_streets_line(tags, out);
    streets::match_street_labels_line(tags, out);
    transport::match_aerialways(tags, out);
    transport::match_ferries(tags, out);
    land::match_land_lines(tags, out);
}

fn match_closed_way(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    // Polygon layers
    water::match_water_polygons(tags, out);
    water::match_water_polygons_labels(tags, out);
    water::match_dam_polygons(tags, out);
    water::match_pier_polygons(tags, out);
    boundaries::match_boundary_labels(tags, out);
    land::match_land(tags, out);
    land::match_sites(tags, out);
    land::match_buildings(tags, out);
    land::match_addresses_centroid(tags, out);
    streets::match_street_polygons(tags, out);
    streets::match_streets_polygons_labels(tags, out);
    streets::match_bridges(tags, out);
    // Line layers (closed ways can still be lines)
    water::match_water_lines(tags, out);
    water::match_water_lines_labels(tags, out);
    water::match_dam_lines(tags, out);
    water::match_pier_lines(tags, out);
    boundaries::match_boundaries_line(tags, out);
    streets::match_streets_line(tags, out);
    streets::match_street_labels_line(tags, out);
    transport::match_aerialways(tags, out);
    transport::match_ferries(tags, out);
    land::match_land_lines(tags, out);
    // Point layers on closed ways
    transport::match_public_transport_centroid(tags, out);
    pois::match_pois_centroid(tags, out);
}

fn match_multipolygon(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    water::match_water_polygons(tags, out);
    water::match_water_polygons_labels(tags, out);
    water::match_dam_polygons(tags, out);
    water::match_pier_polygons(tags, out);
    boundaries::match_boundary_labels(tags, out);
    land::match_land(tags, out);
    land::match_sites(tags, out);
    land::match_buildings(tags, out);
    land::match_addresses_centroid(tags, out);
    streets::match_street_polygons(tags, out);
    streets::match_streets_polygons_labels(tags, out);
    streets::match_bridges(tags, out);
    // Point layers on multipolygons
    transport::match_public_transport_centroid(tags, out);
    pois::match_pois_centroid(tags, out);
}

// ---------------------------------------------------------------------------
// Shared attribute helpers
// ---------------------------------------------------------------------------

pub(crate) fn attr_str(key: &'static str, val: &'static str) -> Attr {
    (key, AttrValue::Str(Cow::Borrowed(val)), 0)
}

pub(crate) fn attr_dyn(key: &'static str, val: &str) -> Attr {
    (key, AttrValue::Str(Cow::Owned(val.to_string())), 0)
}

fn attr_dyn_z(key: &'static str, val: &str, min_zoom: u8) -> Attr {
    (key, AttrValue::Str(Cow::Owned(val.to_string())), min_zoom)
}

pub(crate) fn attr_int(key: &'static str, val: i64) -> Attr {
    (key, AttrValue::Int(val), 0)
}

pub(crate) fn attr_bool(key: &'static str, val: bool) -> Attr {
    (key, AttrValue::Bool(val), 0)
}

fn attr_bool_z(key: &'static str, val: bool, min_zoom: u8) -> Attr {
    (key, AttrValue::Bool(val), min_zoom)
}

fn is_tunnel(tags: &Tags<'_>) -> bool {
    tags.has_any("tunnel", &["yes", "building_passage"]) || tags.has_value("covered", "yes")
}

fn is_bridge(tags: &Tags<'_>) -> bool {
    tags.has_any(
        "bridge",
        &[
            "yes",
            "viaduct",
            "boardwalk",
            "cantilever",
            "covered",
            "low_water_crossing",
            "movable",
            "trestle",
        ],
    )
}

pub(crate) fn name_attrs(tags: &Tags<'_>) -> SmallVec<[Attr; 8]> {
    let mut attrs = SmallVec::new();
    if let Some(v) = tags.get("name")
        && !v.is_empty()
    {
        attrs.push(attr_dyn("name", v));
    }
    if let Some(v) = tags.get("name:en")
        && !v.is_empty()
    {
        attrs.push(attr_dyn("name_en", v));
    }
    if let Some(v) = tags.get("name:de")
        && !v.is_empty()
    {
        attrs.push(attr_dyn("name_de", v));
    }
    attrs
}

fn has_name(tags: &Tags<'_>) -> bool {
    tags.get("name").is_some_and(|v| !v.is_empty())
}

/// Strip _link suffix for street kind.
fn street_kind(val: &str) -> &str {
    val.strip_suffix("_link").unwrap_or(val)
}

/// Parse a tag value as i64, returning None on failure.
fn parse_i64(s: &str) -> Option<i64> {
    // Try direct parse first, then strip commas for values like "123,456"
    s.parse::<i64>()
        .ok()
        .or_else(|| s.replace(',', "").parse::<i64>().ok())
}

// ---------------------------------------------------------------------------
// POIs (see src/pois.rs)
// ---------------------------------------------------------------------------

use crate::pois;

// ---------------------------------------------------------------------------
// Tests (see shortbread_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../shortbread_tests.rs"]
mod tests;
