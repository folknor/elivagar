// Shortbread vector tile schema — hardcoded Rust implementation.
//
// Reference: <https://shortbread.geofabrik.de/>
// No YAML parsing — every layer, filter, and attribute mapping is compiled code.

use std::borrow::Cow;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// MVT layer in the Shortbread schema.
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
        Self::WaterPolygons, Self::WaterPolygonsLabels,
        Self::WaterLines, Self::WaterLinesLabels,
        Self::DamLines, Self::DamPolygons,
        Self::PierLines, Self::PierPolygons,
        Self::Boundaries, Self::BoundaryLabels,
        Self::PlaceLabels, Self::Land,
        Self::Sites, Self::Buildings, Self::Addresses,
        Self::Streets, Self::StreetPolygons,
        Self::StreetLabels, Self::StreetLabelsPoints, Self::StreetsPolygonsLabels,
        Self::Bridges, Self::Aerialways, Self::Ferries,
        Self::PublicTransport, Self::Pois, Self::Ocean,
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
            Self::DamLines | Self::DamPolygons | Self::PierLines | Self::PierPolygons
            | Self::StreetLabelsPoints | Self::Bridges | Self::Aerialways => 12,
            Self::Sites | Self::Buildings | Self::Addresses
            | Self::StreetsPolygonsLabels | Self::Pois => 14,
        }
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

/// Tag lookup helper.
pub struct Tags<'a>(pub &'a [(&'a str, &'a str)]);

impl<'a> Tags<'a> {
    pub fn get(&self, key: &str) -> Option<&'a str> {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| *v)
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| *k == key)
    }

    pub fn has_value(&self, key: &str, val: &str) -> bool {
        self.0.iter().any(|(k, v)| *k == key && *v == val)
    }

    pub fn has_any(&self, key: &str, vals: &[&str]) -> bool {
        self.0
            .iter()
            .any(|(k, v)| *k == key && vals.contains(v))
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
pub type Attr = (&'static str, AttrValue, u8);

/// A single layer match result.
pub struct LayerMatch {
    pub layer: Layer,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub geom_expect: GeomExpect,
    pub attrs: Vec<Attr>,
}

// ---------------------------------------------------------------------------
// Core dispatch
// ---------------------------------------------------------------------------

/// Match an OSM element against all Shortbread layers, returning every match.
pub fn match_element(tags: &Tags<'_>, geom_type: OsmGeomType) -> Vec<LayerMatch> {
    let mut out = Vec::new();
    match geom_type {
        OsmGeomType::Node => match_node(tags, &mut out),
        OsmGeomType::OpenWay => match_open_way(tags, &mut out),
        OsmGeomType::ClosedWay => match_closed_way(tags, &mut out),
        OsmGeomType::MultiPolygon => match_multipolygon(tags, &mut out),
    }
    out
}

fn match_node(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    match_place_labels(tags, out);
    match_addresses_point(tags, out);
    match_street_labels_points(tags, out);
    match_public_transport_point(tags, out);
    pois::match_pois_point(tags, out);
}

fn match_open_way(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    match_water_lines(tags, out);
    match_water_lines_labels(tags, out);
    match_dam_lines(tags, out);
    match_pier_lines(tags, out);
    match_boundaries_line(tags, out);
    match_streets_line(tags, out);
    match_street_labels_line(tags, out);
    match_aerialways(tags, out);
    match_ferries(tags, out);
}

fn match_closed_way(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    // Polygon layers
    match_water_polygons(tags, out);
    match_water_polygons_labels(tags, out);
    match_dam_polygons(tags, out);
    match_pier_polygons(tags, out);
    match_boundary_labels(tags, out);
    match_land(tags, out);
    match_sites(tags, out);
    match_buildings(tags, out);
    match_addresses_centroid(tags, out);
    match_street_polygons(tags, out);
    match_streets_polygons_labels(tags, out);
    match_bridges(tags, out);
    // Line layers (closed ways can still be lines)
    match_water_lines(tags, out);
    match_water_lines_labels(tags, out);
    match_dam_lines(tags, out);
    match_pier_lines(tags, out);
    match_boundaries_line(tags, out);
    match_streets_line(tags, out);
    match_street_labels_line(tags, out);
    match_aerialways(tags, out);
    match_ferries(tags, out);
    // Point layers on closed ways
    match_public_transport_centroid(tags, out);
    pois::match_pois_centroid(tags, out);
}

fn match_multipolygon(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    match_water_polygons(tags, out);
    match_water_polygons_labels(tags, out);
    match_dam_polygons(tags, out);
    match_pier_polygons(tags, out);
    match_boundary_labels(tags, out);
    match_land(tags, out);
    match_sites(tags, out);
    match_buildings(tags, out);
    match_street_polygons(tags, out);
    match_streets_polygons_labels(tags, out);
    match_bridges(tags, out);
    // Boundaries are line from relation members
    match_boundaries_line(tags, out);
    // Point layers on multipolygons
    match_public_transport_centroid(tags, out);
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

fn attr_int(key: &'static str, val: i64) -> Attr {
    (key, AttrValue::Int(val), 0)
}

pub(crate) fn attr_bool(key: &'static str, val: bool) -> Attr {
    (key, AttrValue::Bool(val), 0)
}

fn _attr_float(key: &'static str, val: f64) -> Attr {
    (key, AttrValue::Float(val), 0)
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

pub(crate) fn name_attrs(tags: &Tags<'_>) -> Vec<Attr> {
    let mut attrs = Vec::new();
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
    tags.has("name")
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
// Water layers
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

fn match_water_polygons(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some((kind, min_zoom)) = water_polygon_match(tags) {
        out.push(LayerMatch {
            layer: Layer::WaterPolygons,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: vec![attr_str("kind", kind)],
        });
    }
}

fn match_water_polygons_labels(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if !has_name(tags) {
        return;
    }
    if let Some((kind, _base_zoom)) = water_polygon_match(tags) {
        let mut attrs = vec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        // Label layer has its own (higher) min_zoom per Shortbread spec
        let label_zoom = match kind {
            "dock" | "canal" => 14,
            _ => 14,
        };
        out.push(LayerMatch {
            layer: Layer::WaterPolygonsLabels,
            min_zoom: label_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::PolygonCentroid,
            attrs,
        });
    }
}

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

fn match_water_lines(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some((kind, min_zoom)) = water_line_match(tags) {
        let mut attrs = vec![attr_str("kind", kind)];
        attrs.push(attr_bool("tunnel", is_tunnel(tags)));
        attrs.push(attr_bool("bridge", is_bridge(tags)));
        out.push(LayerMatch {
            layer: Layer::WaterLines,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            attrs,
        });
    }
}

fn match_water_lines_labels(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if !has_name(tags) {
        return;
    }
    if let Some((kind, _base_zoom)) = water_line_match(tags) {
        let mut attrs = vec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        attrs.push(attr_bool("tunnel", is_tunnel(tags)));
        attrs.push(attr_bool("bridge", is_bridge(tags)));
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

fn match_dam_lines(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if tags.has_value("waterway", "dam") {
        out.push(LayerMatch {
            layer: Layer::DamLines,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            attrs: vec![attr_str("kind", "dam")],
        });
    }
}

fn match_dam_polygons(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if tags.has_value("waterway", "dam") {
        out.push(LayerMatch {
            layer: Layer::DamPolygons,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: vec![attr_str("kind", "dam")],
        });
    }
}

fn match_pier_lines(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some(kind) = pier_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::PierLines,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            attrs: vec![attr_str("kind", kind)],
        });
    }
}

fn match_pier_polygons(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some(kind) = pier_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::PierPolygons,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: vec![attr_str("kind", kind)],
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

// ---------------------------------------------------------------------------
// Boundaries
// ---------------------------------------------------------------------------

/// Returns admin_level (int) and min_zoom if boundary matches.
fn boundary_match(tags: &Tags<'_>) -> Option<(i64, u8)> {
    if !tags.has_value("boundary", "administrative") {
        return None;
    }
    let level_str = tags.get("admin_level")?;
    let level = parse_i64(level_str)?;
    match level {
        2 => Some((2, 0)),
        4 => Some((4, 7)),
        _ => None,
    }
}

fn match_boundaries_line(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some((admin_level, min_zoom)) = boundary_match(tags) {
        let maritime =
            tags.has("maritime") || tags.has_value("natural", "coastline");
        let disputed = tags.has_value("disputed", "yes");
        out.push(LayerMatch {
            layer: Layer::Boundaries,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            attrs: vec![
                attr_int("admin_level", admin_level),
                attr_bool("maritime", maritime),
                attr_bool("disputed", disputed),
            ],
        });
    }
}

fn match_boundary_labels(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some((admin_level, _)) = boundary_match(tags) {
        let mut attrs = vec![attr_int("admin_level", admin_level)];
        attrs.extend(name_attrs(tags));
        // Default min_zoom is 5. Area-based overrides (z2-z4 for large
        // territories) are applied later in tilegen when geometry is available.
        out.push(LayerMatch {
            layer: Layer::BoundaryLabels,
            min_zoom: 5,
            max_zoom: 14,
            geom_expect: GeomExpect::PolygonPointOnSurface,
            attrs,
        });
    }
}

// ---------------------------------------------------------------------------
// Place labels
// ---------------------------------------------------------------------------

fn match_place_labels(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if !has_name(tags) {
        return;
    }
    let place = match tags.get("place") {
        Some(v) => v,
        None => return,
    };
    if let Some((kind, min_zoom, pop_default)) = place_label_info(tags, place) {
        let mut attrs = vec![attr_str("kind", kind)];
        attrs.extend(name_attrs(tags));
        let population = tags
            .get("population")
            .and_then(parse_i64)
            .unwrap_or(pop_default);
        attrs.push(attr_int("population", population));
        out.push(LayerMatch {
            layer: Layer::PlaceLabels,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Point,
            attrs,
        });
    }
}

/// Returns (kind, min_zoom, population_default) for a place label.
fn place_label_info(
    tags: &Tags<'_>,
    place: &str,
) -> Option<(&'static str, u8, i64)> {
    // Capital detection
    let is_capital = tags.has_value("capital", "yes");
    let is_state_capital = tags.has_value("capital", "4");

    match place {
        "city" => {
            let kind = if is_capital {
                "capital"
            } else if is_state_capital {
                "state_capital"
            } else {
                "city"
            };
            let zoom = if is_capital || is_state_capital { 4 } else { 6 };
            Some((kind, zoom, 100_000))
        }
        "town" => Some(("town", if is_capital { 4 } else { 7 }, 5_000)),
        "village" => Some(("village", if is_capital { 4 } else { 10 }, 100)),
        "hamlet" => Some(("hamlet", if is_capital { 4 } else { 10 }, 50)),
        "suburb" => Some(("suburb", 10, 1_000)),
        "quarter" => Some(("quarter", 10, 500)),
        "neighbourhood" => Some(("neighbourhood", 10, 100)),
        "isolated_dwelling" => Some(("isolated_dwelling", 10, 5)),
        "farm" => Some(("farm", 10, 5)),
        "island" => Some(("island", 10, 0)),
        "locality" => Some(("locality", 10, 0)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Land
// ---------------------------------------------------------------------------

fn match_land(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some((kind, min_zoom)) = land_match(tags) {
        out.push(LayerMatch {
            layer: Layer::Land,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: vec![attr_str("kind", kind)],
        });
    }
}

fn land_match(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    if let Some(v) = tags.get("amenity")
        && v == "grave_yard"
    {
        return Some(("grave_yard", 13));
    }
    if let Some(v) = tags.get("landuse") {
        return land_match_landuse(v);
    }
    if let Some(v) = tags.get("leisure") {
        return land_match_leisure(v);
    }
    if let Some(v) = tags.get("natural") {
        return land_match_natural(v);
    }
    if let Some(v) = tags.get("wetland") {
        return land_match_wetland(v);
    }
    None
}

fn land_match_landuse(v: &str) -> Option<(&'static str, u8)> {
    match v {
        "forest" => Some(("forest", 7)),
        "farmland" => Some(("farmland", 10)),
        "farmyard" => Some(("farmyard", 10)),
        "meadow" => Some(("meadow", 10)),
        "orchard" => Some(("orchard", 10)),
        "vineyard" => Some(("vineyard", 10)),
        "allotments" => Some(("allotments", 10)),
        "brownfield" => Some(("brownfield", 10)),
        "cemetery" => Some(("cemetery", 13)),
        "commercial" => Some(("commercial", 10)),
        "garages" => Some(("garages", 10)),
        "grass" => Some(("grass", 11)),
        "greenfield" => Some(("greenfield", 10)),
        "greenhouse_horticulture" => Some(("greenhouse_horticulture", 10)),
        "industrial" => Some(("industrial", 10)),
        "landfill" => Some(("landfill", 10)),
        "plant_nursery" => Some(("plant_nursery", 10)),
        "quarry" => Some(("quarry", 10)),
        "railway" => Some(("railway", 10)),
        "recreation_ground" => Some(("recreation_ground", 10)),
        "residential" => Some(("residential", 10)),
        "retail" => Some(("retail", 10)),
        "village_green" => Some(("village_green", 10)),
        _ => None,
    }
}

fn land_match_leisure(v: &str) -> Option<(&'static str, u8)> {
    match v {
        "garden" => Some(("garden", 11)),
        "golf_course" => Some(("golf_course", 11)),
        "miniature_golf" => Some(("miniature_golf", 11)),
        "park" => Some(("park", 11)),
        "playground" => Some(("playground", 11)),
        _ => None,
    }
}

fn land_match_natural(v: &str) -> Option<(&'static str, u8)> {
    match v {
        "wood" => Some(("forest", 7)),
        "bare_rock" => Some(("bare_rock", 11)),
        "beach" => Some(("beach", 11)),
        "grassland" => Some(("grassland", 11)),
        "heath" => Some(("heath", 11)),
        "sand" => Some(("sand", 11)),
        "scree" => Some(("scree", 11)),
        "scrub" => Some(("scrub", 11)),
        "shingle" => Some(("shingle", 11)),
        _ => None,
    }
}

fn land_match_wetland(v: &str) -> Option<(&'static str, u8)> {
    match v {
        "bog" => Some(("bog", 11)),
        "marsh" => Some(("marsh", 11)),
        "string_bog" => Some(("string_bog", 11)),
        "swamp" => Some(("swamp", 11)),
        "wet_meadow" => Some(("wet_meadow", 11)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Sites
// ---------------------------------------------------------------------------

fn match_sites(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some(kind) = sites_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::Sites,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: vec![attr_str("kind", kind)],
        });
    }
}

fn sites_kind(tags: &Tags<'_>) -> Option<&'static str> {
    if tags.has_value("military", "danger_area") {
        return Some("danger_area");
    }
    if tags.has_value("leisure", "sports_centre") {
        return Some("sports_centre");
    }
    if tags.has_value("landuse", "construction") {
        return Some("construction");
    }
    if let Some(v) = tags.get("amenity") {
        return match v {
            "bicycle_parking" => Some("bicycle_parking"),
            "college" => Some("college"),
            "hospital" => Some("hospital"),
            "parking" => Some("parking"),
            "prison" => Some("prison"),
            "university" => Some("university"),
            _ => None,
        };
    }
    None
}

// ---------------------------------------------------------------------------
// Buildings
// ---------------------------------------------------------------------------

fn match_buildings(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some(v) = tags.get("building")
        && v != "no"
    {
        out.push(LayerMatch {
            layer: Layer::Buildings,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: vec![],
        });
    }
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Check if the element qualifies as a POI (excludes from address layer).
fn is_poi_element(tags: &Tags<'_>) -> bool {
    tags.has("amenity")
        || tags.has("shop")
        || tags.has("tourism")
        || tags.has("leisure")
        || tags.has("office")
}

fn match_addresses_point(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if is_poi_element(tags) {
        return;
    }
    let has_number = tags.has("addr:housenumber");
    let has_housename = tags.has("addr:housename");
    if !has_number && !has_housename {
        return;
    }
    let mut attrs = Vec::new();
    if let Some(v) = tags.get("addr:housename") {
        attrs.push(attr_dyn("housename", v));
    }
    if let Some(v) = tags.get("addr:housenumber") {
        attrs.push(attr_dyn("housenumber", v));
    }
    out.push(LayerMatch {
        layer: Layer::Addresses,
        min_zoom: 14,
        max_zoom: 14,
        geom_expect: GeomExpect::Point,
        attrs,
    });
}

fn match_addresses_centroid(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if is_poi_element(tags) {
        return;
    }
    let has_number = tags.has("addr:housenumber");
    let has_housename = tags.has("addr:housename");
    if !has_number && !has_housename {
        return;
    }
    let mut attrs = Vec::new();
    if let Some(v) = tags.get("addr:housename") {
        attrs.push(attr_dyn("housename", v));
    }
    if let Some(v) = tags.get("addr:housenumber") {
        attrs.push(attr_dyn("housenumber", v));
    }
    out.push(LayerMatch {
        layer: Layer::Addresses,
        min_zoom: 14,
        max_zoom: 14,
        geom_expect: GeomExpect::PolygonCentroid,
        attrs,
    });
}

// ---------------------------------------------------------------------------
// Streets
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

fn match_streets_line(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    let (kind_raw, min_zoom, is_rail) = match street_match(tags) {
        Some(v) => v,
        None => return,
    };
    let kind = street_kind(kind_raw);
    let is_link = kind_raw.ends_with("_link");
    // kind and rail: always emitted. Others have per-attribute min_zoom.
    let mut attrs = vec![
        attr_dyn("kind", kind),
        attr_bool_z("link", is_link, 11),
        attr_bool("rail", is_rail),
        attr_bool_z("tunnel", is_tunnel(tags), 11),
        attr_bool_z("bridge", is_bridge(tags), 11),
    ];
    // Oneway: for non-railway, check tags; for railway, always false
    if is_rail {
        attrs.push(attr_bool_z("oneway", false, 14));
        attrs.push(attr_bool_z("oneway_reverse", false, 14));
    } else {
        let oneway_val = tags.get("oneway");
        let is_oneway = matches!(
            oneway_val,
            Some("yes") | Some("1") | Some("true") | Some("-1")
        );
        let is_reverse = oneway_val == Some("-1");
        attrs.push(attr_bool_z("oneway", is_oneway, 14));
        attrs.push(attr_bool_z("oneway_reverse", is_reverse, 14));
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

fn match_street_polygons(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some((kind, min_zoom)) = street_polygon_match(tags) {
        let mut attrs = vec![
            attr_str("kind", kind),
            attr_bool("bridge", is_bridge(tags)),
            attr_bool("rail", false),
            attr_bool("tunnel", is_tunnel(tags)),
        ];
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

fn match_street_labels_line(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
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
    let mut attrs = vec![attr_dyn("kind", kind)];
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
    attrs.push(attr_bool("tunnel", is_tunnel(tags)));
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

fn match_street_labels_points(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if !tags.has_value("highway", "motorway_junction") {
        return;
    }
    let mut attrs = vec![attr_str("kind", "motorway_junction")];
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

fn match_streets_polygons_labels(
    tags: &Tags<'_>,
    out: &mut Vec<LayerMatch>,
) {
    if !has_name(tags) {
        return;
    }
    if let Some((kind, _)) = street_polygon_match(tags) {
        let mut attrs = vec![attr_str("kind", kind)];
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

fn match_bridges(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if tags.has_value("man_made", "bridge") {
        out.push(LayerMatch {
            layer: Layer::Bridges,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: vec![attr_str("kind", "bridge")],
        });
    }
}

// ---------------------------------------------------------------------------
// Aerialways
// ---------------------------------------------------------------------------

fn match_aerialways(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if let Some(kind) = aerialway_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::Aerialways,
            min_zoom: 12,
            max_zoom: 14,
            geom_expect: GeomExpect::Line,
            attrs: vec![attr_str("kind", kind)],
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

fn match_ferries(tags: &Tags<'_>, out: &mut Vec<LayerMatch>) {
    if !tags.has_value("route", "ferry") {
        return;
    }
    let min_zoom = if tags.get("motor_vehicle") == Some("no") {
        12
    } else {
        10
    };
    let mut attrs = vec![attr_str("kind", "ferry")];
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

fn match_public_transport_point(
    tags: &Tags<'_>,
    out: &mut Vec<LayerMatch>,
) {
    if let Some((kind, min_zoom)) = public_transport_match(tags) {
        let mut attrs = vec![attr_str("kind", kind)];
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

fn match_public_transport_centroid(
    tags: &Tags<'_>,
    out: &mut Vec<LayerMatch>,
) {
    if let Some((kind, min_zoom)) = public_transport_match(tags) {
        let mut attrs = vec![attr_str("kind", kind)];
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

// ---------------------------------------------------------------------------
// POIs (see src/pois.rs)
// ---------------------------------------------------------------------------

use crate::pois;

// ---------------------------------------------------------------------------
// Tests (see shortbread_tests.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "shortbread_tests.rs"]
mod tests;
