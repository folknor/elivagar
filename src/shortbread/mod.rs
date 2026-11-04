// Shortbread vector tile schema - hardcoded Rust implementation.
//
// Reference: <https://shortbread.geofabrik.de/>
// No YAML parsing - every layer, filter, and attribute mapping is compiled code.
//
// Layer matching is split by domain:
//   water.rs      - water polygons/lines/labels, dam, pier
//   boundaries.rs - boundaries, boundary_labels
//   place_labels.rs - place_labels
//   land.rs       - land, sites, buildings, addresses
//   streets.rs    - streets, street_polygons, street_labels, bridges
//   transport.rs  - aerialways, ferries, public_transport
//   pois.rs       - points of interest

use smallvec::SmallVec;
use std::borrow::Cow;

pub(crate) mod boundaries;
mod land;
pub(crate) mod osm_value;
pub(crate) mod paint_order;
mod place_labels;
pub(crate) mod pois;
mod streets;
mod transport;
mod water;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

// This list is in archived MVT tile order. The explicit order is checked
// against the layer ID because the ID also controls the wire sort key.
macro_rules! define_layers {
    (
        $(
            $variant:ident = $id:literal,
            $name:literal,
            tile_order = $tile_order:literal,
            min_zoom = $min_zoom:literal,
            fanout_cap = $fanout_cap:literal;
        )*
    ) => {
        const LAYER_COUNT: usize = [$(stringify!($variant)),*].len();

        /// MVT layer in the Shortbread schema.
        /// `#[repr(u8)]` makes the wire ID a zero-cost index into layer arrays.
        /// Tile order is explicit in this list and must match each wire ID.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(u8)]
        pub enum Layer {
            $($variant = $id,)*
        }

        impl Layer {
            /// All layers in archived MVT tile order.
            pub const ALL: [Layer; LAYER_COUNT] = [$(Self::$variant),*];

            /// MVT tile order. This currently matches the wire ID.
            pub const fn tile_order(self) -> u8 {
                match self {
                    $(Self::$variant => $tile_order,)*
                }
            }

            /// Minimum zoom floor for features in this layer.
            pub const fn min_zoom(self) -> u8 {
                match self {
                    $(Self::$variant => $min_zoom,)*
                }
            }

            /// Whether OSM polygon emission consults this layer's fanout cap.
            pub const fn supports_fanout_cap(self) -> bool {
                match self {
                    $(Self::$variant => $fanout_cap,)*
                }
            }

            /// MVT layer name string.
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// Parse an MVT layer name into a Layer variant.
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $($name => Some(Self::$variant),)*
                    _ => None,
                }
            }

            /// Total number of layers.
            pub const fn count() -> usize {
                LAYER_COUNT
            }
        }

        const _: () = {
            $(
                assert!($id == $tile_order);
                assert!(Layer::ALL[$tile_order] as u8 == $id);
            )*
        };
    };
}

define_layers! {
    WaterPolygons = 0, "water_polygons", tile_order = 0, min_zoom = 4, fanout_cap = true;
    WaterPolygonsLabels = 1, "water_polygons_labels", tile_order = 1, min_zoom = 4, fanout_cap = false;
    WaterLines = 2, "water_lines", tile_order = 2, min_zoom = 9, fanout_cap = false;
    WaterLinesLabels = 3, "water_lines_labels", tile_order = 3, min_zoom = 9, fanout_cap = false;
    DamLines = 4, "dam_lines", tile_order = 4, min_zoom = 12, fanout_cap = false;
    DamPolygons = 5, "dam_polygons", tile_order = 5, min_zoom = 12, fanout_cap = true;
    PierLines = 6, "pier_lines", tile_order = 6, min_zoom = 12, fanout_cap = false;
    PierPolygons = 7, "pier_polygons", tile_order = 7, min_zoom = 12, fanout_cap = true;
    Boundaries = 8, "boundaries", tile_order = 8, min_zoom = 0, fanout_cap = false;
    BoundaryLabels = 9, "boundary_labels", tile_order = 9, min_zoom = 2, fanout_cap = false;
    PlaceLabels = 10, "place_labels", tile_order = 10, min_zoom = 4, fanout_cap = false;
    Land = 11, "land", tile_order = 11, min_zoom = 7, fanout_cap = true;
    Sites = 12, "sites", tile_order = 12, min_zoom = 14, fanout_cap = true;
    Buildings = 13, "buildings", tile_order = 13, min_zoom = 14, fanout_cap = true;
    Addresses = 14, "addresses", tile_order = 14, min_zoom = 14, fanout_cap = false;
    Streets = 15, "streets", tile_order = 15, min_zoom = 5, fanout_cap = false;
    StreetPolygons = 16, "street_polygons", tile_order = 16, min_zoom = 11, fanout_cap = true;
    StreetLabels = 17, "street_labels", tile_order = 17, min_zoom = 10, fanout_cap = false;
    StreetLabelsPoints = 18, "street_labels_points", tile_order = 18, min_zoom = 12, fanout_cap = false;
    StreetsPolygonsLabels = 19, "streets_polygons_labels", tile_order = 19, min_zoom = 14, fanout_cap = false;
    Bridges = 20, "bridges", tile_order = 20, min_zoom = 12, fanout_cap = true;
    Aerialways = 21, "aerialways", tile_order = 21, min_zoom = 12, fanout_cap = false;
    Ferries = 22, "ferries", tile_order = 22, min_zoom = 10, fanout_cap = false;
    PublicTransport = 23, "public_transport", tile_order = 23, min_zoom = 11, fanout_cap = false;
    Pois = 24, "pois", tile_order = 24, min_zoom = 14, fanout_cap = false;
    Ocean = 25, "ocean", tile_order = 25, min_zoom = 0, fanout_cap = false;
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

/// Whether relation members can supply area geometry (including unmatched areas).
pub(crate) fn is_area_relation(tags: &Tags<'_>) -> bool {
    matches!(tags.get("type"), Some("multipolygon" | "boundary"))
}

/// The shared profile gate for storing and consuming area-relation member ways.
pub(crate) fn area_relation_matches(tags: &Tags<'_>) -> Option<SmallVec<[LayerMatch; 4]>> {
    if !is_area_relation(tags) {
        return None;
    }
    let matches = match_element(tags, OsmGeomType::MultiPolygon);
    (!matches.is_empty()).then_some(matches)
}

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
    #[cfg(test)]
    for matched in &out {
        assert!(
            matched.min_zoom >= matched.layer.min_zoom(),
            "{} emitted at z{} below its layer floor z{}",
            matched.layer.name(),
            matched.min_zoom,
            matched.layer.min_zoom(),
        );
    }
    out
}

fn match_node(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    place_labels::match_place_labels(tags, out);
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

/// Parse an integer tag using the locale policy in `osm_value::parse_int`.
fn parse_i64(s: &str) -> Option<i64> {
    osm_value::parse_int(s)
}

// ---------------------------------------------------------------------------
// Tests (see shortbread_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../shortbread_tests.rs"]
mod tests;
