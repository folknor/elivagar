// Land coverage layers: land, sites, buildings, addresses.

use super::*;
use smallvec::{SmallVec, smallvec};

// ---------------------------------------------------------------------------
// Land
// ---------------------------------------------------------------------------

pub(super) fn match_land(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some((kind, min_zoom)) = land_match(tags) {
        out.push(LayerMatch {
            layer: Layer::Land,
            min_zoom,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: smallvec![attr_str("kind", kind)],
        });
    }
}

fn land_match(tags: &Tags<'_>) -> Option<(&'static str, u8)> {
    if let Some(v) = tags.get("amenity")
        && v == "grave_yard"
    {
        return Some(("grave_yard", 13));
    }
    if let Some(v) = tags.get("landuse")
        && let r @ Some(_) = land_match_landuse(v)
    {
        return r;
    }
    if let Some(v) = tags.get("leisure")
        && let r @ Some(_) = land_match_leisure(v)
    {
        return r;
    }
    if let Some(v) = tags.get("natural")
        && let r @ Some(_) = land_match_natural(v)
    {
        return r;
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

pub(super) fn match_sites(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(kind) = sites_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::Sites,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: smallvec![attr_str("kind", kind)],
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

pub(super) fn match_buildings(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(v) = tags.get("building")
        && v != "no"
    {
        out.push(LayerMatch {
            layer: Layer::Buildings,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            attrs: smallvec![],
        });
    }
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Check if the element qualifies as a POI (excludes from address layer).
/// Uses specific value matching (via `pois::would_match_poi`) rather than bare
/// key presence, so that e.g. `office=company` does not suppress addresses.
fn is_poi_element(tags: &Tags<'_>) -> bool {
    crate::pois::would_match_poi(tags)
}

fn match_addresses(tags: &Tags<'_>, geom_expect: GeomExpect, out: &mut SmallVec<[LayerMatch; 4]>) {
    if is_poi_element(tags) {
        return;
    }
    let has_number = tags.has("addr:housenumber");
    let has_housename = tags.has("addr:housename");
    if !has_number && !has_housename {
        return;
    }
    let mut attrs = SmallVec::new();
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
        geom_expect,
        attrs,
    });
}

pub(super) fn match_addresses_point(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    match_addresses(tags, GeomExpect::Point, out);
}

pub(super) fn match_addresses_centroid(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    match_addresses(tags, GeomExpect::PolygonCentroid, out);
}
