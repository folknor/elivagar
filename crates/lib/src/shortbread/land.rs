// Land coverage layers: land, sites, buildings, addresses.

use super::osm_value;
use super::*;
use smallvec::{SmallVec, smallvec};

// ---------------------------------------------------------------------------
// Land
// ---------------------------------------------------------------------------

wire_kinds! {
    /// Every `kind` value the land layer emits. Each kind owns its wire name, its
    /// zoom floor and its paint rank, so the matcher, the verifier and the
    /// interning dictionary read one vocabulary. Zoom is a property of the output
    /// kind in this profile: the one kind reached from two tags (`forest`, from
    /// `landuse=forest` and `natural=wood`) has the same floor through both.
    pub enum LandKind {
        Residential = "residential",
        Commercial = "commercial",
        Retail = "retail",
        Industrial = "industrial",
        Garages = "garages",
        Railway = "railway",
        Brownfield = "brownfield",
        Greenfield = "greenfield",
        Landfill = "landfill",
        Quarry = "quarry",
        Farmyard = "farmyard",
        Farmland = "farmland",
        Meadow = "meadow",
        Orchard = "orchard",
        Vineyard = "vineyard",
        Allotments = "allotments",
        PlantNursery = "plant_nursery",
        GreenhouseHorticulture = "greenhouse_horticulture",
        Park = "park",
        GolfCourse = "golf_course",
        RecreationGround = "recreation_ground",
        VillageGreen = "village_green",
        Cemetery = "cemetery",
        GraveYard = "grave_yard",
        BareRock = "bare_rock",
        Scree = "scree",
        Shingle = "shingle",
        Sand = "sand",
        Beach = "beach",
        Grassland = "grassland",
        Heath = "heath",
        Scrub = "scrub",
        Bog = "bog",
        Marsh = "marsh",
        StringBog = "string_bog",
        Swamp = "swamp",
        WetMeadow = "wet_meadow",
        Grass = "grass",
        Garden = "garden",
        Playground = "playground",
        MiniatureGolf = "miniature_golf",
        Forest = "forest",
        Cliff = "cliff",
    }
}

impl LandKind {
    /// The kind whose wire name is `name`, if the land layer emits it.
    pub fn from_name(name: &str) -> Option<LandKind> {
        Self::ALL.into_iter().find(|k| k.as_str() == name)
    }

    /// The lowest zoom the kind is emitted at.
    pub const fn min_zoom(self) -> u8 {
        use LandKind::*;
        match self {
            Forest => 7,
            Residential
            | Commercial
            | Retail
            | Industrial
            | Garages
            | Railway
            | Brownfield
            | Greenfield
            | Landfill
            | Quarry
            | Farmyard
            | Farmland
            | Meadow
            | Orchard
            | Vineyard
            | Allotments
            | PlantNursery
            | GreenhouseHorticulture
            | RecreationGround
            | VillageGreen => 10,
            Park | GolfCourse | Garden | Playground | MiniatureGolf | Grass | BareRock | Scree
            | Shingle | Sand | Beach | Grassland | Heath | Scrub | Bog | Marsh | StringBog
            | Swamp | WetMeadow => 11,
            Cliff => 12,
            Cemetery | GraveYard => 13,
        }
    }

    /// Within-layer paint rank, in seven background-first bands: smaller
    /// ranks are emitted first and so painted below.
    pub const fn paint_rank(self) -> u8 {
        use LandKind::*;
        match self {
            Residential | Commercial | Retail | Industrial | Garages | Railway | Brownfield
            | Greenfield | Landfill | Quarry | Farmyard => 0,
            Farmland
            | Meadow
            | Orchard
            | Vineyard
            | Allotments
            | PlantNursery
            | GreenhouseHorticulture => 1,
            Park | GolfCourse | RecreationGround | VillageGreen | Cemetery | GraveYard => 2,
            BareRock | Scree | Shingle | Sand | Beach | Grassland | Heath | Scrub | Bog | Marsh
            | StringBog | Swamp | WetMeadow => 3,
            Grass | Garden | Playground | MiniatureGolf => 4,
            Forest => 5,
            Cliff => 6,
        }
    }
}

fn land_layer_match(kind: LandKind, geom_expect: GeomExpect) -> LayerMatch {
    LayerMatch {
        layer: Layer::Land,
        min_zoom: kind.min_zoom(),
        geom_expect,
        paint_rank: kind.paint_rank(),
        attrs: smallvec![attr_kind(kind)],
    }
}

pub(super) fn match_land(tags: &Tags<'_>, out: &mut Hits) {
    if let Some(kind) = land_match(tags) {
        out.push(land_layer_match(kind, GeomExpect::Polygon));
    }
}

pub(super) fn match_land_lines(tags: &Tags<'_>, out: &mut Hits) {
    if tags.has_value("natural", "cliff") {
        out.push(land_layer_match(LandKind::Cliff, GeomExpect::Line));
    }
}

/// Tag precedence: amenity, landuse, leisure, natural, then wetland. The
/// first key carrying a listed value wins.
fn land_match(tags: &Tags<'_>) -> Option<LandKind> {
    if let Some(v) = tags.get("amenity")
        && v == "grave_yard"
    {
        return Some(LandKind::GraveYard);
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

fn land_match_landuse(v: &str) -> Option<LandKind> {
    use LandKind::*;
    Some(match v {
        "forest" => Forest,
        "farmland" => Farmland,
        "farmyard" => Farmyard,
        "meadow" => Meadow,
        "orchard" => Orchard,
        "vineyard" => Vineyard,
        "allotments" => Allotments,
        "brownfield" => Brownfield,
        "cemetery" => Cemetery,
        "commercial" => Commercial,
        "garages" => Garages,
        "grass" => Grass,
        "greenfield" => Greenfield,
        "greenhouse_horticulture" => GreenhouseHorticulture,
        "industrial" => Industrial,
        "landfill" => Landfill,
        "plant_nursery" => PlantNursery,
        "quarry" => Quarry,
        "railway" => Railway,
        "recreation_ground" => RecreationGround,
        "residential" => Residential,
        "retail" => Retail,
        "village_green" => VillageGreen,
        _ => return None,
    })
}

fn land_match_leisure(v: &str) -> Option<LandKind> {
    use LandKind::*;
    Some(match v {
        "garden" => Garden,
        "golf_course" => GolfCourse,
        "miniature_golf" => MiniatureGolf,
        "park" => Park,
        "playground" => Playground,
        _ => return None,
    })
}

fn land_match_natural(v: &str) -> Option<LandKind> {
    use LandKind::*;
    Some(match v {
        "wood" => Forest,
        "bare_rock" => BareRock,
        "beach" => Beach,
        "grassland" => Grassland,
        "heath" => Heath,
        "sand" => Sand,
        "scree" => Scree,
        "scrub" => Scrub,
        "shingle" => Shingle,
        _ => return None,
    })
}

fn land_match_wetland(v: &str) -> Option<LandKind> {
    use LandKind::*;
    Some(match v {
        "bog" => Bog,
        "marsh" => Marsh,
        "string_bog" => StringBog,
        "swamp" => Swamp,
        "wet_meadow" => WetMeadow,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Sites
// ---------------------------------------------------------------------------

pub(super) fn match_sites(tags: &Tags<'_>, out: &mut Hits) {
    if let Some(kind) = sites_kind(tags) {
        out.push(LayerMatch {
            layer: Layer::Sites,
            min_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs: smallvec![attr_kind(kind)],
        });
    }
}

wire_kinds! {
    /// `kind` of sites.
    pub(super) enum SiteKind {
        DangerArea = "danger_area",
        SportsCenter = "sports_center",
        Construction = "construction",
        BicycleParking = "bicycle_parking",
        College = "college",
        Hospital = "hospital",
        Parking = "parking",
        Prison = "prison",
        University = "university",
    }
}

fn sites_kind(tags: &Tags<'_>) -> Option<SiteKind> {
    use SiteKind as K;
    transport::first_tag_match(
        tags,
        &[
            ("military", "danger_area", K::DangerArea),
            // OSM uses sports_centre, while the Shortbread sites fixture names
            // its kind sports_center. Match the OSM tag and emit the fixture kind.
            ("leisure", "sports_centre", K::SportsCenter),
            ("landuse", "construction", K::Construction),
            ("amenity", "bicycle_parking", K::BicycleParking),
            ("amenity", "college", K::College),
            ("amenity", "hospital", K::Hospital),
            ("amenity", "parking", K::Parking),
            ("amenity", "prison", K::Prison),
            ("amenity", "university", K::University),
        ],
    )
}

// ---------------------------------------------------------------------------
// Buildings
// ---------------------------------------------------------------------------

pub(super) fn match_buildings(tags: &Tags<'_>, out: &mut Hits) {
    if let Some(v) = tags.get("building")
        && v != "no"
    {
        let mut attrs = SmallVec::new();
        if let Some(height) = tags.get("height").and_then(osm_value::parse_length_m) {
            attrs.push(attr_float(FloatKey::Height, height));
        }
        if let Some(min_height) = tags.get("min_height").and_then(osm_value::parse_length_m) {
            attrs.push(attr_float(FloatKey::MinHeight, min_height));
        }
        if let Some(levels) = tags.get("building:levels").and_then(osm_value::parse_count) {
            attrs.push(attr_float(FloatKey::BuildingLevels, levels));
        }
        out.push(LayerMatch {
            layer: Layer::Buildings,
            min_zoom: 14,
            geom_expect: GeomExpect::Polygon,
            paint_rank: 0,
            attrs,
        });
    }
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Check if the element qualifies as a POI (excludes from address layer).
/// Uses specific value matching (via `pois::would_match_poi`) rather than bare
/// key presence, so that e.g. `office=company` does not suppress addresses.
/// Changes to the shared POI tag matcher also change address exclusions.
/// POI zoom and geometry restrictions live in separate dispatch paths.
fn is_poi_element(tags: &Tags<'_>) -> bool {
    crate::pois::would_match_poi(tags)
}

fn match_addresses(tags: &Tags<'_>, geom_expect: GeomExpect, out: &mut Hits) {
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
        attrs.push(attr_dyn(StrKey::Housename, v));
    }
    if let Some(v) = tags.get("addr:housenumber") {
        attrs.push(attr_dyn(StrKey::Housenumber, v));
    }
    out.push(LayerMatch {
        layer: Layer::Addresses,
        min_zoom: 14,
        geom_expect,
        paint_rank: 0,
        attrs,
    });
}

pub(super) fn match_addresses_point(tags: &Tags<'_>, out: &mut Hits) {
    match_addresses(tags, GeomExpect::Point, out);
}

pub(super) fn match_addresses_polygon(tags: &Tags<'_>, out: &mut Hits) {
    match_addresses(tags, Layer::Addresses.polygon_point_geom_expect(), out);
}

#[cfg(test)]
mod land_kind_tests {
    use super::LandKind;

    #[test]
    fn all_is_every_variant_in_order() {
        assert_eq!(LandKind::ALL.len(), LandKind::Cliff as usize + 1);
        for (i, kind) in LandKind::ALL.into_iter().enumerate() {
            assert_eq!(kind as usize, i);
            assert_eq!(LandKind::from_name(kind.as_str()), Some(kind));
        }
    }

    /// Independent of the rank method: a wrong edit to the method fails here
    /// even though producer and verifier share it.
    #[test]
    fn ranks_and_zooms_match_independent_fixture() {
        let fixture = [
            ("residential", 0, 10),
            ("commercial", 0, 10),
            ("retail", 0, 10),
            ("industrial", 0, 10),
            ("garages", 0, 10),
            ("railway", 0, 10),
            ("brownfield", 0, 10),
            ("greenfield", 0, 10),
            ("landfill", 0, 10),
            ("quarry", 0, 10),
            ("farmyard", 0, 10),
            ("farmland", 1, 10),
            ("meadow", 1, 10),
            ("orchard", 1, 10),
            ("vineyard", 1, 10),
            ("allotments", 1, 10),
            ("plant_nursery", 1, 10),
            ("greenhouse_horticulture", 1, 10),
            ("park", 2, 11),
            ("golf_course", 2, 11),
            ("recreation_ground", 2, 10),
            ("village_green", 2, 10),
            ("cemetery", 2, 13),
            ("grave_yard", 2, 13),
            ("bare_rock", 3, 11),
            ("scree", 3, 11),
            ("shingle", 3, 11),
            ("sand", 3, 11),
            ("beach", 3, 11),
            ("grassland", 3, 11),
            ("heath", 3, 11),
            ("scrub", 3, 11),
            ("bog", 3, 11),
            ("marsh", 3, 11),
            ("string_bog", 3, 11),
            ("swamp", 3, 11),
            ("wet_meadow", 3, 11),
            ("grass", 4, 11),
            ("garden", 4, 11),
            ("playground", 4, 11),
            ("miniature_golf", 4, 11),
            ("forest", 5, 7),
            ("cliff", 6, 12),
        ];
        assert_eq!(fixture.len(), LandKind::ALL.len());
        for (name, rank, zoom) in fixture {
            let kind = LandKind::from_name(name).expect("fixture kind is a LandKind");
            assert_eq!(kind.paint_rank(), rank, "{name}");
            assert_eq!(kind.min_zoom(), zoom, "{name}");
        }
    }

    #[test]
    fn land_bands_background_first() {
        use LandKind::*;
        let bands = [Residential, Farmland, Park, Sand, Grass, Forest, Cliff];
        for pair in bands.windows(2) {
            assert!(pair[0].paint_rank() < pair[1].paint_rank());
        }
    }

    /// corpus/style.toml is read by brokkr's canonical renderer; a land kind
    /// without a match row renders in the background color and reads as
    /// missing, so every `LandKind` must have one.
    #[test]
    fn corpus_style_styles_every_land_kind() {
        let style = include_str!("../../../../corpus/style.toml");
        let land = style
            .split("[[layer]]")
            .find(|block| block.contains("name = \"land\""))
            .expect("style has a land layer");
        let styled: Vec<&str> = land
            .lines()
            .filter_map(|line| line.trim().strip_prefix("value = \""))
            .filter_map(|rest| rest.strip_suffix('"'))
            .collect();
        for kind in LandKind::ALL {
            assert!(
                styled.contains(&kind.as_str()),
                "corpus/style.toml has no land match for {:?}",
                kind.as_str()
            );
        }
    }
}
