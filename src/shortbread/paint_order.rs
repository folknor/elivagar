//! Deliberate within-layer paint ordering for layers where opaque features
//! commonly overlap. Smaller ranks are emitted first and therefore painted below.

pub const UNKNOWN_STREET_CLASS: u8 = 12;

/// Whether `kind` is one of the values the land matcher can emit.
pub fn is_known_land_kind(kind: &str) -> bool {
    matches!(
        kind,
        "residential"
            | "commercial"
            | "retail"
            | "industrial"
            | "garages"
            | "railway"
            | "brownfield"
            | "greenfield"
            | "landfill"
            | "quarry"
            | "farmyard"
            | "farmland"
            | "meadow"
            | "orchard"
            | "vineyard"
            | "allotments"
            | "plant_nursery"
            | "greenhouse_horticulture"
            | "park"
            | "golf_course"
            | "recreation_ground"
            | "village_green"
            | "cemetery"
            | "grave_yard"
            | "bare_rock"
            | "scree"
            | "shingle"
            | "sand"
            | "beach"
            | "grassland"
            | "heath"
            | "scrub"
            | "bog"
            | "marsh"
            | "string_bog"
            | "swamp"
            | "wet_meadow"
            | "grass"
            | "garden"
            | "playground"
            | "miniature_golf"
            | "forest"
            | "cliff"
    )
}

/// Whether `kind` is one of the values emitted by the streets matchers.
pub fn is_known_street_kind(kind: &str) -> bool {
    matches!(
        kind,
        "track"
            | "footway"
            | "steps"
            | "path"
            | "cycleway"
            | "pedestrian"
            | "living_street"
            | "service"
            | "busway"
            | "bus_guideway"
            | "taxiway"
            | "runway"
            | "unclassified"
            | "residential"
            | "tertiary"
            | "secondary"
            | "primary"
            | "trunk"
            | "motorway"
            | "funicular"
            | "monorail"
            | "tram"
            | "light_rail"
            | "subway"
            | "narrow_gauge"
            | "rail"
    )
}

pub fn land_paint_rank(kind: &str) -> u8 {
    match kind {
        "residential" | "commercial" | "retail" | "industrial" | "garages" | "railway"
        | "brownfield" | "greenfield" | "landfill" | "quarry" | "farmyard" => 0,
        "farmland"
        | "meadow"
        | "orchard"
        | "vineyard"
        | "allotments"
        | "plant_nursery"
        | "greenhouse_horticulture" => 1,
        "park" | "golf_course" | "recreation_ground" | "village_green" | "cemetery"
        | "grave_yard" => 2,
        "bare_rock" | "scree" | "shingle" | "sand" | "beach" | "grassland" | "heath" | "scrub"
        | "bog" | "marsh" | "string_bog" | "swamp" | "wet_meadow" => 3,
        "grass" | "garden" | "playground" | "miniature_golf" => 4,
        "forest" => 5,
        "cliff" => 6,
        _ => 0,
    }
}

pub fn street_class_rank(kind: &str) -> u8 {
    match kind {
        "track" => 0,
        "footway" => 1,
        "steps" => 2,
        "path" => 3,
        "cycleway" => 4,
        "pedestrian" => 5,
        "living_street" => 6,
        "service" => 7,
        "busway" => 8,
        "bus_guideway" => 9,
        "taxiway" => 10,
        "runway" => 11,
        "unclassified" => 12,
        "residential" => 13,
        "tertiary" => 14,
        "secondary" => 15,
        "primary" => 16,
        "trunk" => 17,
        "motorway" => 18,
        "funicular" => 19,
        "monorail" => 20,
        "tram" => 21,
        "light_rail" => 22,
        "subway" => 23,
        "narrow_gauge" => 24,
        "rail" => 25,
        _ => UNKNOWN_STREET_CLASS,
    }
}

pub fn street_paint_rank(kind: &str, link: bool, tunnel: bool, bridge: bool) -> u8 {
    let elev = if tunnel {
        0
    } else if bridge {
        2
    } else {
        1
    };
    street_class_rank(kind) * 6 + elev * 2 + u8::from(!link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_match_independent_fixture() {
        for (kind, rank) in [
            ("residential", 0),
            ("commercial", 0),
            ("retail", 0),
            ("industrial", 0),
            ("garages", 0),
            ("railway", 0),
            ("brownfield", 0),
            ("greenfield", 0),
            ("landfill", 0),
            ("quarry", 0),
            ("farmyard", 0),
            ("farmland", 1),
            ("meadow", 1),
            ("orchard", 1),
            ("vineyard", 1),
            ("allotments", 1),
            ("plant_nursery", 1),
            ("greenhouse_horticulture", 1),
            ("park", 2),
            ("golf_course", 2),
            ("recreation_ground", 2),
            ("village_green", 2),
            ("cemetery", 2),
            ("grave_yard", 2),
            ("bare_rock", 3),
            ("scree", 3),
            ("shingle", 3),
            ("sand", 3),
            ("beach", 3),
            ("grassland", 3),
            ("heath", 3),
            ("scrub", 3),
            ("bog", 3),
            ("marsh", 3),
            ("string_bog", 3),
            ("swamp", 3),
            ("wet_meadow", 3),
            ("grass", 4),
            ("garden", 4),
            ("playground", 4),
            ("miniature_golf", 4),
            ("forest", 5),
            ("cliff", 6),
        ] {
            assert!(is_known_land_kind(kind));
            assert_eq!(land_paint_rank(kind), rank);
        }
        for (kind, rank) in [
            ("track", 0),
            ("footway", 1),
            ("steps", 2),
            ("path", 3),
            ("cycleway", 4),
            ("pedestrian", 5),
            ("living_street", 6),
            ("service", 7),
            ("busway", 8),
            ("bus_guideway", 9),
            ("taxiway", 10),
            ("runway", 11),
            ("unclassified", 12),
            ("residential", 13),
            ("tertiary", 14),
            ("secondary", 15),
            ("primary", 16),
            ("trunk", 17),
            ("motorway", 18),
            ("funicular", 19),
            ("monorail", 20),
            ("tram", 21),
            ("light_rail", 22),
            ("subway", 23),
            ("narrow_gauge", 24),
            ("rail", 25),
        ] {
            assert!(is_known_street_kind(kind));
            assert_eq!(street_class_rank(kind), rank);
        }
    }

    /// Every kind `land_match` / `match_land_lines` can emit is in the table.
    /// Keeps the rank table and the matcher in lockstep: a new land kind that
    /// slips into the matcher without a rank trips this list.
    #[test]
    fn every_land_kind_has_rank() {
        for kind in [
            "residential",
            "commercial",
            "retail",
            "industrial",
            "garages",
            "railway",
            "brownfield",
            "greenfield",
            "landfill",
            "quarry",
            "farmyard",
            "farmland",
            "meadow",
            "orchard",
            "vineyard",
            "allotments",
            "plant_nursery",
            "greenhouse_horticulture",
            "park",
            "golf_course",
            "recreation_ground",
            "village_green",
            "cemetery",
            "grave_yard",
            "bare_rock",
            "scree",
            "shingle",
            "sand",
            "beach",
            "grassland",
            "heath",
            "scrub",
            "bog",
            "marsh",
            "string_bog",
            "swamp",
            "wet_meadow",
            "grass",
            "garden",
            "playground",
            "miniature_golf",
            "forest",
            "cliff",
        ] {
            assert!(is_known_land_kind(kind), "land kind {kind:?} has no rank");
        }
    }

    /// Every kind the streets / street_polygons matchers emit (post `_link`
    /// strip) is in the class table.
    #[test]
    fn every_street_kind_has_rank() {
        for kind in [
            "track",
            "footway",
            "steps",
            "path",
            "cycleway",
            "pedestrian",
            "living_street",
            "service",
            "busway",
            "bus_guideway",
            "taxiway",
            "runway",
            "unclassified",
            "residential",
            "tertiary",
            "secondary",
            "primary",
            "trunk",
            "motorway",
            "funicular",
            "monorail",
            "tram",
            "light_rail",
            "subway",
            "narrow_gauge",
            "rail",
        ] {
            assert!(
                is_known_street_kind(kind),
                "street kind {kind:?} has no rank"
            );
        }
    }

    #[test]
    fn street_link_ranks_below_parent() {
        assert!(
            street_paint_rank("motorway", true, false, false)
                < street_paint_rank("motorway", false, false, false)
        );
    }

    #[test]
    fn street_bridge_above_tunnel_within_kind() {
        assert!(
            street_paint_rank("motorway", false, true, false)
                < street_paint_rank("motorway", false, false, true)
        );
    }

    #[test]
    fn land_bands_background_first() {
        assert!(
            land_paint_rank("residential") < land_paint_rank("park")
                && land_paint_rank("park") < land_paint_rank("sand")
                && land_paint_rank("sand") < land_paint_rank("grass")
                && land_paint_rank("grass") < land_paint_rank("forest")
                && land_paint_rank("forest") < land_paint_rank("cliff")
        );
    }
}
