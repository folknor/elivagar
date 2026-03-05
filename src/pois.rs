// POI (Point of Interest) matching for the Shortbread schema.
//
// Value arrays (AMENITY_VALUES, etc.) are searched via linear `.contains()`.
// Investigated binary_search and phf — not worth it. These arrays are only
// reached when the element has the relevant key (amenity, shop, etc.), which
// is a tiny fraction of all elements. Hotpath profiling confirms tag matching
// is NOT a bottleneck. The largest array (AMENITY_VALUES, 51 entries, ~400 B)
// fits in L1 cache; sequential scan with first-byte short-circuit is fast.
// binary_search saves ~19 comparisons per hit but hits are rare; phf adds a
// dependency for zero measurable gain.

use crate::shortbread::{attr_str, attr_dyn, attr_int, attr_bool, name_attrs, Attr, Tags, Layer, LayerMatch, GeomExpect};
use smallvec::{SmallVec, smallvec};

// ---------------------------------------------------------------------------
// Entry points (called from shortbread dispatch)
// ---------------------------------------------------------------------------

pub(crate) fn match_pois_point(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    match_pois(tags, GeomExpect::Point, out);
}

pub(crate) fn match_pois_centroid(tags: &Tags<'_>, out: &mut SmallVec<[LayerMatch; 4]>) {
    match_pois(tags, GeomExpect::PolygonPointOnSurface, out);
}

/// Returns true if the tags would match a POI (used by address layer to exclude POI elements).
/// Uses the same matching logic as `pois_match` — only specific values are considered POIs,
/// not bare key presence.
pub(crate) fn would_match_poi(tags: &Tags<'_>) -> bool {
    pois_match(tags).is_some()
}

fn match_pois(tags: &Tags<'_>, geom_expect: GeomExpect, out: &mut SmallVec<[LayerMatch; 4]>) {
    if let Some(mut attrs) = pois_match(tags) {
        attrs.extend(pois_common_attrs(tags));
        out.push(LayerMatch {
            layer: Layer::Pois,
            min_zoom: 14,
            max_zoom: 14,
            geom_expect,
            attrs,
        });
    }
}

// ---------------------------------------------------------------------------
// Common attrs
// ---------------------------------------------------------------------------

fn pois_common_attrs(tags: &Tags<'_>) -> SmallVec<[Attr; 8]> {
    let mut attrs = SmallVec::new();
    attrs.extend(name_attrs(tags));
    if let Some(v) = tags.get("addr:housename") {
        attrs.push(attr_dyn("housename", v));
    }
    if let Some(v) = tags.get("addr:housenumber") {
        attrs.push(attr_dyn("housenumber", v));
    }
    attrs
}

// ---------------------------------------------------------------------------
// Tag matching
// ---------------------------------------------------------------------------

/// Match POI tags. Returns the base attrs (the category key) if matched.
fn pois_match(tags: &Tags<'_>) -> Option<SmallVec<[Attr; 8]>> {
    if let Some(attrs) = pois_match_amenity(tags) {
        return Some(attrs);
    }
    if let Some(attrs) = pois_match_emergency(tags) {
        return Some(attrs);
    }
    if let Some(v) = tags.get("highway")
        && v == "emergency_access_point"
    {
        return Some(smallvec![attr_dyn("highway", v)]);
    }
    if let Some(attrs) = pois_match_historic(tags) {
        return Some(attrs);
    }
    if let Some(attrs) = pois_match_leisure(tags) {
        return Some(attrs);
    }
    if let Some(attrs) = pois_match_man_made(tags) {
        return Some(attrs);
    }
    if let Some(attrs) = pois_match_natural(tags) {
        return Some(attrs);
    }
    if tags.has_value("office", "diplomatic") {
        return Some(smallvec![attr_str("office", "diplomatic")]);
    }
    if let Some(attrs) = pois_match_shop(tags) {
        return Some(attrs);
    }
    if let Some(attrs) = pois_match_tourism(tags) {
        return Some(attrs);
    }
    None
}

#[allow(clippy::too_many_lines)]
fn pois_match_amenity(
    tags: &Tags<'_>,
) -> Option<SmallVec<[Attr; 8]>> {
    static AMENITY_VALUES: &[&str] = &[
        "arts_centre",
        "atm",
        "bank",
        "bar",
        "bench",
        "bicycle_parking",
        "bicycle_rental",
        "biergarten",
        "bus_station",
        "cafe",
        "car_sharing",
        "cinema",
        "clinic",
        "charging_station",
        "college",
        "community_centre",
        "courthouse",
        "dentist",
        "doctors",
        "drinking_water",
        "embassy",
        "fast_food",
        "fire_station",
        "fountain",
        "fuel",
        "hospital",
        "hunting_stand",
        "ice_cream",
        "kindergarten",
        "library",
        "marketplace",
        "nightclub",
        "nursing_home",
        "parking",
        "pharmacy",
        "place_of_worship",
        "police",
        "post_box",
        "post_office",
        "prison",
        "pub",
        "recycling",
        "restaurant",
        "school",
        "shelter",
        "swimming_pool",
        "taxi",
        "telephone",
        "theatre",
        "toilets",
        "townhall",
        "university",
        "vending_machine",
        "veterinary",
        "waste_basket",
    ];
    let v = tags.get("amenity")?;
    if !AMENITY_VALUES.contains(&v) {
        return None;
    }
    let mut attrs = smallvec![attr_dyn("amenity", v)];
    // Special attrs per amenity type
    poi_amenity_special_attrs(tags, v, &mut attrs);
    Some(attrs)
}

fn poi_amenity_special_attrs(
    tags: &Tags<'_>,
    amenity: &str,
    attrs: &mut SmallVec<[Attr; 8]>,
) {
    match amenity {
        "restaurant" | "fast_food" | "pub" | "bar" | "cafe" => {
            if let Some(v) = tags.get("cuisine") {
                attrs.push(attr_dyn("cuisine", v));
            }
        }
        "vending_machine" => {
            if let Some(v) = tags.get("vending") {
                attrs.push(attr_dyn("vending", v));
            }
        }
        "place_of_worship" => {
            if let Some(v) = tags.get("religion") {
                attrs.push(attr_dyn("religion", v));
            }
            if let Some(v) = tags.get("denomination") {
                attrs.push(attr_dyn("denomination", v));
            }
        }
        "recycling" => {
            poi_recycling_attrs(tags, attrs);
        }
        "bank" => {
            let has_atm = tags.has_value("atm", "yes");
            attrs.push(attr_bool("atm", has_atm));
        }
        _ => {}
    }
}

fn poi_recycling_attrs(
    tags: &Tags<'_>,
    attrs: &mut SmallVec<[Attr; 8]>,
) {
    attrs.push(attr_bool(
        "recycling:glass_bottles",
        tags.has_value("recycling:glass_bottles", "yes"),
    ));
    attrs.push(attr_bool(
        "recycling:paper",
        tags.has_value("recycling:paper", "yes"),
    ));
    attrs.push(attr_bool(
        "recycling:clothes",
        tags.has_value("recycling:clothes", "yes"),
    ));
    attrs.push(attr_bool(
        "recycling:scrap_metal",
        tags.has_value("recycling:scrap_metal", "yes"),
    ));
}

/// Match a tag key against an allowed values list. Returns attrs with the matched value.
fn match_tag_in_list(
    tags: &Tags<'_>,
    key: &'static str,
    allowed: &[&str],
) -> Option<SmallVec<[Attr; 8]>> {
    let v = tags.get(key)?;
    if !allowed.contains(&v) {
        return None;
    }
    Some(smallvec![attr_dyn(key, v)])
}

fn pois_match_emergency(tags: &Tags<'_>) -> Option<SmallVec<[Attr; 8]>> {
    static VALUES: &[&str] = &[
        "defibrillator",
        "fire_hydrant",
        "phone",
        "fire_extinguisher",
        "fire_water_pond",
        "water_tank",
        "suction_point",
    ];
    match_tag_in_list(tags, "emergency", VALUES)
}

fn pois_match_historic(tags: &Tags<'_>) -> Option<SmallVec<[Attr; 8]>> {
    static VALUES: &[&str] = &[
        "archaeological_site",
        "battlefield",
        "castle",
        "fort",
        "memorial",
        "monument",
        "ruins",
        "wayside_cross",
        "wayside_shrine",
    ];
    match_tag_in_list(tags, "historic", VALUES)
}

fn pois_match_leisure(
    tags: &Tags<'_>,
) -> Option<SmallVec<[Attr; 8]>> {
    static LEISURE_VALUES: &[&str] = &[
        "dog_park",
        "golf_course",
        "ice_rink",
        "miniature_golf",
        "pitch",
        "playground",
        "sports_centre",
    ];
    let v = tags.get("leisure")?;
    if !LEISURE_VALUES.contains(&v) {
        return None;
    }
    let mut attrs = smallvec![attr_dyn("leisure", v)];
    // pitch and sports_centre get sport attr
    if (v == "pitch" || v == "sports_centre")
        && let Some(s) = tags.get("sport")
    {
        attrs.push(attr_dyn("sport", s));
    }
    Some(attrs)
}

fn pois_match_man_made(
    tags: &Tags<'_>,
) -> Option<SmallVec<[Attr; 8]>> {
    static MAN_MADE_VALUES: &[&str] = &[
        "lighthouse",
        "mast",
        "tower",
        "water_tower",
        "water_well",
        "windmill",
    ];
    let v = tags.get("man_made")?;
    if !MAN_MADE_VALUES.contains(&v) {
        return None;
    }
    let mut attrs = smallvec![attr_dyn("man_made", v)];
    if v == "tower"
        && let Some(tt) = tags.get("tower:type")
    {
        attrs.push(attr_dyn("tower:type", tt));
    }
    Some(attrs)
}

fn parse_ele_meters(raw: &str) -> Option<i64> {
    let mut s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    if let Some((first, _)) = s.split_once(';') {
        s = first.trim().to_string();
    }
    if s.is_empty() {
        return None;
    }

    let mut is_feet = false;
    if s.ends_with("feet") {
        is_feet = true;
        s.truncate(s.len().saturating_sub(4));
    } else if s.ends_with("ft") {
        is_feet = true;
        s.truncate(s.len().saturating_sub(2));
    } else if s.ends_with('\'') {
        is_feet = true;
        s.truncate(s.len().saturating_sub(1));
    } else if s.ends_with("meters") {
        s.truncate(s.len().saturating_sub(6));
    } else if s.ends_with("meter") {
        s.truncate(s.len().saturating_sub(5));
    } else if s.ends_with('m') {
        s.truncate(s.len().saturating_sub(1));
    }

    let num = normalize_numeric_token(s.split_whitespace().next()?);
    let value = num.parse::<f64>().ok()?;
    let meters = if is_feet { value * 0.3048 } else { value };
    let rounded = meters.round();
    if !rounded.is_finite() {
        return None;
    }
    rounded.to_string().parse::<i64>().ok()
}

fn normalize_numeric_token(token: &str) -> String {
    let t = token.trim();
    if !t.contains(',') {
        return t.to_string();
    }
    if t.contains('.') {
        // Assume commas are thousands separators when dot-decimal is present.
        return t.replace(',', "");
    }
    let comma_count = t.bytes().filter(|&b| b == b',').count();
    if comma_count == 1
        && let Some((left, right)) = t.split_once(',')
    {
        if right.len() == 3 && !left.is_empty() {
            // Likely thousands separator, e.g. 1,234
            return [left, right].concat();
        }
        // Likely locale decimal comma, e.g. 999,5
        return [left, ".", right].concat();
    }
    // Multiple commas: assume thousands grouping.
    t.replace(',', "")
}

fn pois_match_natural(tags: &Tags<'_>) -> Option<SmallVec<[Attr; 8]>> {
    if let Some(v) = tags.get("natural")
        && (v == "peak" || v == "volcano")
    {
        let mut attrs = smallvec![attr_dyn("natural", v)];
        if let Some(ele) = tags.get("ele")
            && let Some(meters) = parse_ele_meters(ele)
        {
            attrs.push(attr_int("ele", meters));
        }
        return Some(attrs);
    }
    if tags.has_value("mountain_pass", "yes") {
        let mut attrs = smallvec![attr_str("natural", "pass")];
        if let Some(ele) = tags.get("ele")
            && let Some(meters) = parse_ele_meters(ele)
        {
            attrs.push(attr_int("ele", meters));
        }
        return Some(attrs);
    }
    None
}

fn pois_match_shop(tags: &Tags<'_>) -> Option<SmallVec<[Attr; 8]>> {
    static VALUES: &[&str] = &[
        "alcohol",
        "bakery",
        "beauty",
        "beverages",
        "bicycle",
        "books",
        "butcher",
        "car",
        "car_parts",
        "car_repair",
        "chemist",
        "clothes",
        "computer",
        "confectionery",
        "convenience",
        "department_store",
        "doityourself",
        "electronics",
        "florist",
        "furniture",
        "garden_centre",
        "gift",
        "greengrocer",
        "hairdresser",
        "hardware",
        "jewelry",
        "kiosk",
        "laundry",
        "mall",
        "mobile_phone",
        "optician",
        "outdoor",
        "pet",
        "shoes",
        "sports",
        "stationery",
        "supermarket",
        "toys",
    ];
    match_tag_in_list(tags, "shop", VALUES)
}

fn pois_match_tourism(
    tags: &Tags<'_>,
) -> Option<SmallVec<[Attr; 8]>> {
    static TOURISM_VALUES: &[&str] = &[
        "alpine_hut",
        "artwork",
        "attraction",
        "camp_site",
        "caravan_site",
        "gallery",
        "guest_house",
        "hostel",
        "hotel",
        "information",
        "motel",
        "museum",
        "picnic_site",
        "viewpoint",
    ];
    let v = tags.get("tourism")?;
    if !TOURISM_VALUES.contains(&v) {
        return None;
    }
    let mut attrs = smallvec![attr_dyn("tourism", v)];
    if v == "information"
        && let Some(i) = tags.get("information")
    {
        attrs.push(attr_dyn("information", i));
    }
    Some(attrs)
}
