use super::*;
use std::borrow::Cow;

#[test]
fn test_highway_motorway_matches_streets_z5() {
    let tags = Tags(&[("highway", "motorway")]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let street = matches
        .iter()
        .find(|m| m.layer == Layer::Streets)
        .expect("should match Streets");
    assert_eq!(street.min_zoom, 5);
    assert_eq!(street.max_zoom, 14);
    let kind = street
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind attr");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("motorway")));
}

#[test]
fn test_building_yes_matches_buildings_z14() {
    let tags = Tags(&[("building", "yes")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let bldg = matches
        .iter()
        .find(|m| m.layer == Layer::Buildings)
        .expect("should match Buildings");
    assert_eq!(bldg.min_zoom, 14);
    assert_eq!(bldg.max_zoom, 14);
    assert!(bldg.attrs.is_empty());
}

#[test]
fn test_building_no_does_not_match() {
    let tags = Tags(&[("building", "no")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let bldg = matches.iter().find(|m| m.layer == Layer::Buildings);
    assert!(bldg.is_none(), "building=no should not match Buildings");
}

#[test]
fn test_building_emits_height_level_attrs() {
    let tags = Tags(&[
        ("building", "yes"),
        ("height", "24.5"),
        ("min_height", "10 ft"),
        ("building:levels", "7.5"),
    ]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let bldg = matches
        .iter()
        .find(|m| m.layer == Layer::Buildings)
        .expect("should match Buildings");
    assert_eq!(bldg.attrs.len(), 3);

    let height = bldg
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "height")
        .expect("should have height");
    assert_eq!(height.1, AttrValue::Float(24.5));

    let min_height = bldg
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "min_height")
        .expect("should have min_height");
    if let AttrValue::Float(v) = min_height.1 {
        assert!((v - 3.048).abs() < 1e-9, "expected 10 ft => 3.048 m, got {v}");
    } else {
        panic!("min_height should be float");
    }

    let levels = bldg
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "building:levels")
        .expect("should have building:levels");
    assert_eq!(levels.1, AttrValue::Float(7.5));
}

#[test]
fn test_building_ignores_unparseable_height_level_attrs() {
    let tags = Tags(&[
        ("building", "yes"),
        ("height", "unknown"),
        ("min_height", "NaN"),
        ("building:levels", "N/A"),
    ]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let bldg = matches
        .iter()
        .find(|m| m.layer == Layer::Buildings)
        .expect("should match Buildings");
    assert!(bldg.attrs.is_empty(), "invalid numeric tags should be ignored");
}

#[test]
fn test_building_parses_real_world_measurement_variants() {
    let tags = Tags(&[
        ("building", "yes"),
        ("height", "24,5 m"),
        ("min_height", "24 meters"),
        ("building:levels", "3,5"),
    ]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let bldg = matches
        .iter()
        .find(|m| m.layer == Layer::Buildings)
        .expect("should match Buildings");

    let height = bldg
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "height")
        .expect("should have height");
    assert_eq!(height.1, AttrValue::Float(24.5));

    let min_height = bldg
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "min_height")
        .expect("should have min_height");
    assert_eq!(min_height.1, AttrValue::Float(24.0));

    let levels = bldg
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "building:levels")
        .expect("should have building:levels");
    assert_eq!(levels.1, AttrValue::Float(3.5));
}

#[test]
fn test_building_levels_policy_edges() {
    let zero = Tags(&[("building", "yes"), ("building:levels", "0")]);
    let zero_matches = match_element(&zero, OsmGeomType::ClosedWay);
    let zero_bldg = zero_matches
        .iter()
        .find(|m| m.layer == Layer::Buildings)
        .expect("should match Buildings");
    let levels = zero_bldg
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "building:levels")
        .expect("zero levels should be retained");
    assert_eq!(levels.1, AttrValue::Float(0.0));

    let negative = Tags(&[("building", "yes"), ("building:levels", "-1")]);
    let negative_matches = match_element(&negative, OsmGeomType::ClosedWay);
    let negative_bldg = negative_matches
        .iter()
        .find(|m| m.layer == Layer::Buildings)
        .expect("should match Buildings");
    assert!(
        negative_bldg
            .attrs
            .iter()
            .all(|(k, _, _)| *k != "building:levels"),
        "negative levels should be rejected",
    );

    let fractional = Tags(&[("building", "yes"), ("building:levels", "7.5")]);
    let fractional_matches = match_element(&fractional, OsmGeomType::ClosedWay);
    let fractional_bldg = fractional_matches
        .iter()
        .find(|m| m.layer == Layer::Buildings)
        .expect("should match Buildings");
    let levels = fractional_bldg
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "building:levels")
        .expect("fractional levels should be retained");
    assert_eq!(levels.1, AttrValue::Float(7.5));
}

#[test]
fn test_place_city_capital() {
    let tags = Tags(&[
        ("place", "city"),
        ("name", "Oslo"),
        ("capital", "yes"),
        ("population", "700000"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let place = matches
        .iter()
        .find(|m| m.layer == Layer::PlaceLabels)
        .expect("should match PlaceLabels");
    let kind = place
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("capital")));
    assert_eq!(place.min_zoom, 4);
}

#[test]
fn test_natural_wood_becomes_forest() {
    let tags = Tags(&[("natural", "wood")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let land = matches
        .iter()
        .find(|m| m.layer == Layer::Land)
        .expect("should match Land");
    let kind = land
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("forest")));
    assert_eq!(land.min_zoom, 7);
}

#[test]
fn test_layer_names() {
    assert_eq!(Layer::WaterPolygons.name(), "water_polygons");
    assert_eq!(Layer::Streets.name(), "streets");
    assert_eq!(Layer::Pois.name(), "pois");
    assert_eq!(Layer::Boundaries.name(), "boundaries");
    assert_eq!(Layer::PlaceLabels.name(), "place_labels");
    assert_eq!(Layer::Buildings.name(), "buildings");
    assert_eq!(Layer::Ferries.name(), "ferries");
    assert_eq!(Layer::PublicTransport.name(), "public_transport");
    assert_eq!(Layer::StreetLabelsPoints.name(), "street_labels_points");
    assert_eq!(
        Layer::StreetsPolygonsLabels.name(),
        "streets_polygons_labels"
    );
}

#[test]
fn test_layer_count() {
    assert_eq!(Layer::count(), 26);
}

#[test]
fn test_water_polygon_natural_water() {
    let tags = Tags(&[("natural", "water")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let water = matches
        .iter()
        .find(|m| m.layer == Layer::WaterPolygons)
        .expect("should match WaterPolygons");
    assert_eq!(water.min_zoom, 4);
    let kind = water
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("water")));
}

#[test]
fn test_boundary_admin_level_2() {
    let tags = Tags(&[
        ("boundary", "administrative"),
        ("admin_level", "2"),
        ("maritime", "yes"),
    ]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let boundary = matches
        .iter()
        .find(|m| m.layer == Layer::Boundaries)
        .expect("should match Boundaries");
    assert_eq!(boundary.min_zoom, 0);
    let maritime = boundary
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "maritime")
        .expect("should have maritime");
    assert_eq!(maritime.1, AttrValue::Bool(true));
}

#[test]
fn test_streets_tunnel_bridge() {
    let tags = Tags(&[
        ("highway", "primary"),
        ("tunnel", "yes"),
        ("bridge", "viaduct"),
    ]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let street = matches
        .iter()
        .find(|m| m.layer == Layer::Streets)
        .expect("should match Streets");
    let tunnel_attr = street
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "tunnel")
        .expect("should have tunnel");
    assert_eq!(tunnel_attr.1, AttrValue::Bool(true));
    let bridge_attr = street
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "bridge")
        .expect("should have bridge");
    assert_eq!(bridge_attr.1, AttrValue::Bool(true));
}

#[test]
fn test_street_link_stripped() {
    let tags = Tags(&[("highway", "motorway_link")]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let street = matches
        .iter()
        .find(|m| m.layer == Layer::Streets)
        .expect("should match Streets");
    let kind = street
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("motorway")));
    let link = street
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "link")
        .expect("should have link");
    assert_eq!(link.1, AttrValue::Bool(true));
}

#[test]
fn test_ferry_with_motor_vehicle_no() {
    let tags = Tags(&[
        ("route", "ferry"),
        ("motor_vehicle", "no"),
        ("name", "Foot Ferry"),
    ]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let ferry = matches
        .iter()
        .find(|m| m.layer == Layer::Ferries)
        .expect("should match Ferries");
    assert_eq!(ferry.min_zoom, 12);
}

#[test]
fn test_ferry_default_zoom() {
    let tags = Tags(&[("route", "ferry"), ("name", "Car Ferry")]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let ferry = matches
        .iter()
        .find(|m| m.layer == Layer::Ferries)
        .expect("should match Ferries");
    assert_eq!(ferry.min_zoom, 10);
}

#[test]
fn test_pois_restaurant_cuisine() {
    let tags = Tags(&[
        ("amenity", "restaurant"),
        ("name", "Pasta House"),
        ("cuisine", "italian"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let poi = matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("should match Pois");
    let cuisine = poi
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "cuisine")
        .expect("should have cuisine");
    assert_eq!(cuisine.1, AttrValue::Str(Cow::Borrowed("italian")));
}

#[test]
fn test_pois_ev_charging_station() {
    let tags = Tags(&[
        ("amenity", "charging_station"),
        ("name", "FastCharge"),
        ("addr:housenumber", "12"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let poi = matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("should match Pois");
    let amenity = poi
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "amenity")
        .expect("should have amenity");
    assert_eq!(
        amenity.1,
        AttrValue::Str(Cow::Borrowed("charging_station"))
    );
    assert!(
        matches.iter().all(|m| m.layer != Layer::Addresses),
        "recognized POI amenity should suppress address output",
    );
}

#[test]
fn test_pois_ev_charging_station_closed_way_and_multipolygon() {
    let tags = Tags(&[
        ("amenity", "charging_station"),
        ("name", "Area Charger"),
        ("addr:housenumber", "7"),
    ]);

    let closed_way_matches = match_element(&tags, OsmGeomType::ClosedWay);
    let closed_poi = closed_way_matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("closed-way charging station should match POI centroid");
    assert_eq!(closed_poi.geom_expect, GeomExpect::PolygonPointOnSurface);
    assert!(
        closed_way_matches.iter().all(|m| m.layer != Layer::Addresses),
        "closed-way charging station should suppress address output",
    );

    let mp_matches = match_element(&tags, OsmGeomType::MultiPolygon);
    let mp_poi = mp_matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("multipolygon charging station should match POI centroid");
    assert_eq!(mp_poi.geom_expect, GeomExpect::PolygonPointOnSurface);
    assert!(
        mp_matches.iter().all(|m| m.layer != Layer::Addresses),
        "multipolygon charging station should suppress address output",
    );
}

#[test]
fn test_pois_ev_charging_station_rich_tag_matrix_is_stable() {
    let tags = Tags(&[
        ("amenity", "charging_station"),
        ("name", "FastCharge Downtown"),
        ("name:en", "FastCharge Downtown"),
        ("name:de", "SchnellLaden Zentrum"),
        ("addr:housenumber", "12B"),
        ("operator", "ChargeCo"),
        ("capacity", "8"),
        ("socket:type2", "yes"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let poi = matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("charging station should match Pois");
    let amenity = poi
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "amenity")
        .expect("should have amenity");
    assert_eq!(
        amenity.1,
        AttrValue::Str(Cow::Borrowed("charging_station"))
    );
    assert!(
        poi.attrs.iter().any(|(k, _, _)| *k == "name"),
        "name should be preserved on POI output"
    );
    assert!(
        poi.attrs.iter().any(|(k, _, _)| *k == "name_en"),
        "name:en should be preserved on POI output"
    );
    assert!(
        poi.attrs.iter().any(|(k, _, _)| *k == "name_de"),
        "name:de should be preserved on POI output"
    );
    assert!(
        poi.attrs.iter().any(|(k, _, _)| *k == "housenumber"),
        "housenumber should be preserved on POI output"
    );
    assert!(
        matches.iter().all(|m| m.layer != Layer::Addresses),
        "recognized charging station POI should continue suppressing address output",
    );
}

#[test]
fn test_pois_peak_with_ele_meters() {
    let tags = Tags(&[
        ("natural", "peak"),
        ("name", "Peak One"),
        ("ele", "2469"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let poi = matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("peak should match Pois");
    let natural = poi
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "natural")
        .expect("should have natural");
    assert_eq!(natural.1, AttrValue::Str(Cow::Borrowed("peak")));
    let ele = poi
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "ele")
        .expect("should have ele");
    assert_eq!(ele.1, AttrValue::Int(2469));
}

#[test]
fn test_pois_peak_with_ele_feet_conversion() {
    let tags = Tags(&[
        ("natural", "peak"),
        ("ele", "3281 ft"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let poi = matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("peak should match Pois");
    let ele = poi
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "ele")
        .expect("should have ele");
    assert_eq!(ele.1, AttrValue::Int(1000));
}

#[test]
fn test_pois_elevation_parser_format_coverage() {
    let cases = [
        ("1000m", Some(1000)),
        ("1,234 m", Some(1234)),
        ("1000;1200", Some(1000)),
        ("-50", Some(-50)),
        ("bad-value", None),
    ];
    for (raw, expected_ele) in cases {
        let tags = Tags(&[("natural", "peak"), ("ele", raw)]);
        let matches = match_element(&tags, OsmGeomType::Node);
        let poi = matches
            .iter()
            .find(|m| m.layer == Layer::Pois)
            .expect("peak should match Pois");
        let got = poi
            .attrs
            .iter()
            .find(|(k, _, _)| *k == "ele")
            .map(|(_, v, _)| v.clone());
        match (expected_ele, got) {
            (Some(exp), Some(AttrValue::Int(actual))) => assert_eq!(actual, exp, "ele='{raw}'"),
            (None, None) => {}
            _ => panic!("unexpected ele parse outcome for '{raw}'"),
        }
    }
}

#[test]
fn test_pois_volcano_and_mountain_pass_branches() {
    let volcano_tags = Tags(&[
        ("natural", "volcano"),
        ("ele", "2,500"),
    ]);
    let volcano_matches = match_element(&volcano_tags, OsmGeomType::Node);
    let volcano = volcano_matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("volcano should match Pois");
    let natural = volcano
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "natural")
        .expect("volcano should have natural attr");
    assert_eq!(natural.1, AttrValue::Str(Cow::Borrowed("volcano")));
    let ele = volcano
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "ele")
        .expect("volcano should have ele attr");
    assert_eq!(ele.1, AttrValue::Int(2500));

    let pass_tags = Tags(&[
        ("mountain_pass", "yes"),
        ("ele", "1500"),
    ]);
    let pass_matches = match_element(&pass_tags, OsmGeomType::Node);
    let pass = pass_matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("mountain_pass=yes should match Pois");
    let natural = pass
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "natural")
        .expect("pass should have natural attr");
    assert_eq!(natural.1, AttrValue::Str(Cow::Borrowed("pass")));
    let ele = pass
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "ele")
        .expect("pass should have ele attr");
    assert_eq!(ele.1, AttrValue::Int(1500));
}

#[test]
fn test_aerialway_cable_car() {
    let tags = Tags(&[("aerialway", "cable_car")]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let aerial = matches
        .iter()
        .find(|m| m.layer == Layer::Aerialways)
        .expect("should match Aerialways");
    assert_eq!(aerial.min_zoom, 12);
    let kind = aerial
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("cable_car")));
}

#[test]
fn test_public_transport_aerodrome() {
    let tags = Tags(&[
        ("aeroway", "aerodrome"),
        ("name", "Oslo Airport"),
        ("iata", "OSL"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let pt = matches
        .iter()
        .find(|m| m.layer == Layer::PublicTransport)
        .expect("should match PublicTransport");
    assert_eq!(pt.min_zoom, 11);
    let iata = pt
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "iata")
        .expect("should have iata");
    assert_eq!(iata.1, AttrValue::Str(Cow::Borrowed("OSL")));
}

#[test]
fn test_address_excluded_by_poi() {
    let tags = Tags(&[
        ("amenity", "restaurant"),
        ("addr:housenumber", "42"),
        ("name", "Bistro"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let addr = matches.iter().find(|m| m.layer == Layer::Addresses);
    assert!(
        addr.is_none(),
        "POI elements should not appear in Addresses"
    );
    // But it should appear as a POI
    let poi = matches.iter().find(|m| m.layer == Layer::Pois);
    assert!(poi.is_some(), "should match Pois");
}

#[test]
fn test_address_housenumber() {
    let tags = Tags(&[("addr:housenumber", "12B")]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let addr = matches
        .iter()
        .find(|m| m.layer == Layer::Addresses)
        .expect("should match Addresses");
    assert_eq!(addr.min_zoom, 14);
    let hn = addr
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "housenumber")
        .expect("should have housenumber");
    assert_eq!(hn.1, AttrValue::Str(Cow::Borrowed("12B")));
}

#[test]
fn test_tags_helper() {
    let tags = Tags(&[("highway", "motorway"), ("name", "E6")]);
    assert_eq!(tags.get("highway"), Some("motorway"));
    assert_eq!(tags.get("missing"), None);
    assert!(tags.has("name"));
    assert!(!tags.has("ref"));
    assert!(tags.has_value("highway", "motorway"));
    assert!(!tags.has_value("highway", "trunk"));
    assert!(tags.has_any("highway", &["trunk", "motorway"]));
    assert!(!tags.has_any("highway", &["trunk", "primary"]));
}

#[test]
fn test_state_capital() {
    let tags = Tags(&[
        ("place", "city"),
        ("name", "Bergen"),
        ("capital", "4"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let place = matches
        .iter()
        .find(|m| m.layer == Layer::PlaceLabels)
        .expect("should match PlaceLabels");
    let kind = place
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("state_capital")));
}

#[test]
fn test_sites_hospital() {
    let tags = Tags(&[("amenity", "hospital")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let site = matches
        .iter()
        .find(|m| m.layer == Layer::Sites)
        .expect("should match Sites");
    assert_eq!(site.min_zoom, 14);
    let kind = site
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("hospital")));
}

#[test]
fn test_dam_line() {
    let tags = Tags(&[("waterway", "dam")]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let dam = matches
        .iter()
        .find(|m| m.layer == Layer::DamLines)
        .expect("should match DamLines");
    assert_eq!(dam.min_zoom, 12);
}

#[test]
fn test_bridge_polygon() {
    let tags = Tags(&[("man_made", "bridge")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let bridge = matches
        .iter()
        .find(|m| m.layer == Layer::Bridges)
        .expect("should match Bridges");
    assert_eq!(bridge.min_zoom, 12);
    let kind = bridge
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("bridge")));
}

#[test]
fn test_oneway_reverse() {
    let tags = Tags(&[("highway", "residential"), ("oneway", "-1")]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let street = matches
        .iter()
        .find(|m| m.layer == Layer::Streets)
        .expect("should match Streets");
    let ow = street
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "oneway")
        .expect("should have oneway");
    assert_eq!(ow.1, AttrValue::Bool(true));
    let owr = street
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "oneway_reverse")
        .expect("should have oneway_reverse");
    assert_eq!(owr.1, AttrValue::Bool(true));
}

#[test]
fn test_place_population_default() {
    let tags = Tags(&[("place", "hamlet"), ("name", "Tiny")]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let place = matches
        .iter()
        .find(|m| m.layer == Layer::PlaceLabels)
        .expect("should match PlaceLabels");
    let pop = place
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "population")
        .expect("should have population");
    assert_eq!(pop.1, AttrValue::Int(50));
}

#[test]
fn test_recycling_attrs() {
    let tags = Tags(&[
        ("amenity", "recycling"),
        ("recycling:glass_bottles", "yes"),
        ("recycling:paper", "yes"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let poi = matches
        .iter()
        .find(|m| m.layer == Layer::Pois)
        .expect("should match Pois");
    let glass = poi
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "recycling:glass_bottles")
        .expect("should have glass_bottles");
    assert_eq!(glass.1, AttrValue::Bool(true));
    let clothes = poi
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "recycling:clothes")
        .expect("should have clothes");
    assert_eq!(clothes.1, AttrValue::Bool(false));
}

#[test]
fn test_street_ref_semicolons() {
    let tags = Tags(&[
        ("highway", "motorway"),
        ("ref", "E6;E18"),
        ("name", "Motorveien"),
    ]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let label = matches
        .iter()
        .find(|m| m.layer == Layer::StreetLabels)
        .expect("should match StreetLabels");
    let ref_attr = label
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "ref")
        .expect("should have ref");
    assert_eq!(ref_attr.1, AttrValue::Str(Cow::Borrowed("E6\nE18")));
    let rows = label
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "ref_rows")
        .expect("should have ref_rows");
    assert_eq!(rows.1, AttrValue::Int(2));
    let cols = label
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "ref_cols")
        .expect("should have ref_cols");
    assert_eq!(cols.1, AttrValue::Int(3));
}

#[test]
fn test_motorway_junction() {
    let tags = Tags(&[
        ("highway", "motorway_junction"),
        ("ref", "23"),
        ("name", "Exit 23"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let pt = matches
        .iter()
        .find(|m| m.layer == Layer::StreetLabelsPoints)
        .expect("should match StreetLabelsPoints");
    assert_eq!(pt.min_zoom, 12);
    let kind = pt
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("motorway_junction")));
}

// -----------------------------------------------------------------------
// YAML spec test - validates against Planetiler's shortbread.spec.yml
// -----------------------------------------------------------------------

fn input_geom_type(geom_str: &str) -> OsmGeomType {
    match geom_str {
        "point" => OsmGeomType::Node,
        "line" => OsmGeomType::OpenWay,
        "polygon" => OsmGeomType::ClosedWay,
        s if s.starts_with("POLYGON") => OsmGeomType::ClosedWay,
        other => panic!("unknown input geometry: {other}"),
    }
}

fn acceptable_geom_expects(output_geom: &str, input_geom: &str) -> Vec<GeomExpect> {
    match (output_geom, input_geom) {
        ("polygon", _) => vec![GeomExpect::Polygon],
        ("line", _) => vec![GeomExpect::Line],
        ("point", "point") => vec![GeomExpect::Point],
        ("point", _) => vec![
            GeomExpect::PolygonCentroid,
            GeomExpect::PolygonPointOnSurface,
        ],
        _ => panic!("unknown output geometry: {output_geom}"),
    }
}

fn yaml_to_attr_value(v: &serde_yaml::Value) -> Option<AttrValue> {
    use serde_yaml::Value;
    match v {
        Value::Bool(b) => Some(AttrValue::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(AttrValue::Int(i))
            } else {
                n.as_f64().map(AttrValue::Float)
            }
        }
        Value::String(s) => match s.as_str() {
            "true" => Some(AttrValue::Bool(true)),
            "false" => Some(AttrValue::Bool(false)),
            _ => Some(AttrValue::Str(Cow::Owned(s.clone()))),
        },
        _ => None,
    }
}

fn attr_values_match(key: &str, expected: &AttrValue, actual: &AttrValue) -> bool {
    let _ = key;
    match (expected, actual) {
        (AttrValue::Float(e), AttrValue::Float(a)) => (e - a).abs() < e.abs() * 1e-6,
        (AttrValue::Int(e), AttrValue::Float(a)) => (*e as f64 - a).abs() < 1.0,
        (AttrValue::Float(e), AttrValue::Int(a)) => (e - *a as f64).abs() < 1.0,
        (AttrValue::Int(e), AttrValue::Str(a)) => a.as_ref() == e.to_string(),
        (AttrValue::Str(e), AttrValue::Int(a)) => e.as_ref() == a.to_string(),
        _ => expected == actual,
    }
}

#[test]
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn shortbread_spec_yaml() {
    let yaml = std::fs::read_to_string("tests/fixtures/shortbread.spec.yml")
        .expect("failed to read shortbread.spec.yml");
    let doc: serde_yaml::Value =
        serde_yaml::from_str(&yaml).expect("failed to parse YAML");
    let cases = doc["examples"]
        .as_sequence()
        .expect("expected 'examples' array");

    let mut passed = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;
    let mut failures: Vec<String> = Vec::new();

    for case in cases {
        let name = case["name"].as_str().unwrap_or("unnamed");
        let input = &case["input"];

        // Skip ocean source tests
        if input["source"].as_str() == Some("ocean") {
            skipped += 1;
            continue;
        }

        let geom_str = input["geometry"].as_str().unwrap_or("point");
        let osm_geom = input_geom_type(geom_str);

        // Build tags
        let tag_pairs: Vec<(String, String)> = input["tags"]
            .as_mapping()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| {
                        let key = k.as_str().unwrap_or_default().to_string();
                        let val = match v {
                            serde_yaml::Value::String(s) => s.clone(),
                            serde_yaml::Value::Number(n) => n.to_string(),
                            serde_yaml::Value::Bool(b) => b.to_string(),
                            _ => String::new(),
                        };
                        (key, val)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let tag_refs: Vec<(&str, &str)> = tag_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let tags = Tags(&tag_refs);

        let matches = match_element(&tags, osm_geom);

        // Parse expected outputs
        let output = &case["output"];
        let expected_outputs: Vec<&serde_yaml::Value> = if output.is_null() {
            skipped += 1;
            continue;
        } else if output.is_sequence() {
            output.as_sequence().unwrap().iter().collect()
        } else if output.is_mapping() {
            vec![output]
        } else {
            skipped += 1;
            continue;
        };

        // Empty array = expect no matches
        if expected_outputs.is_empty() {
            if matches.is_empty() {
                passed += 1;
            } else {
                let layer_names: Vec<&str> =
                    matches.iter().map(|m| m.layer.name()).collect();
                failures.push(format!(
                    "[{name}] expected no matches, got: {layer_names:?}"
                ));
                failed += 1;
            }
            continue;
        }

        // Skip tests that only have at_zoom (no layer field)
        if expected_outputs.len() == 1
            && expected_outputs[0].get("layer").is_none()
        {
            skipped += 1;
            continue;
        }

        let mut case_ok = true;

        for exp in &expected_outputs {
            let Some(exp_layer) = exp["layer"].as_str() else {
                continue;
            };

            let found = matches.iter().find(|m| m.layer.name() == exp_layer);

            let Some(m) = found else {
                let layer_names: Vec<&str> =
                    matches.iter().map(|m| m.layer.name()).collect();
                failures.push(format!(
                    "[{name}] expected layer '{exp_layer}', got: {layer_names:?}"
                ));
                case_ok = false;
                continue;
            };

            // Check min_zoom (skip for boundary_labels - area-dependent,
            // requires geometry we don't have in match_element)
            if exp_layer != "boundary_labels"
                && let Some(expected_zoom) = exp["min_zoom"].as_u64()
            {
                #[allow(clippy::cast_possible_truncation)]
                let ez = expected_zoom as u8;
                if m.min_zoom != ez {
                    failures.push(format!(
                        "[{name}] layer '{exp_layer}' min_zoom: expected {ez}, got {}",
                        m.min_zoom
                    ));
                    case_ok = false;
                }
            }

            // Check output geometry type
            if let Some(exp_geom) = exp["geometry"].as_str() {
                let acceptable = acceptable_geom_expects(exp_geom, geom_str);
                if !acceptable.contains(&m.geom_expect) {
                    failures.push(format!(
                        "[{name}] layer '{exp_layer}' geometry: expected {exp_geom}, got {:?}",
                        m.geom_expect
                    ));
                    case_ok = false;
                }
            }

            // Check tags/attributes
            if let Some(exp_tags) = exp["tags"].as_mapping() {
                for (k, v) in exp_tags {
                    let key = k.as_str().unwrap_or_default();

                    // Skip way_area - requires geometry area calculation
                    if key == "way_area" {
                        continue;
                    }

                    let Some(expected_val) = yaml_to_attr_value(v) else {
                        continue;
                    };

                    let actual = m.attrs.iter().find(|(ak, _, _)| *ak == key);

                    match actual {
                        Some((_, actual_val, _)) => {
                            if !attr_values_match(key, &expected_val, actual_val) {
                                failures.push(format!(
                                    "[{name}] layer '{exp_layer}' attr '{key}': expected {expected_val:?}, got {actual_val:?}"
                                ));
                                case_ok = false;
                            }
                        }
                        None => {
                            // Absent Bool(false) is OK - we omit false booleans
                            // to save space (absent = default = false).
                            if expected_val != AttrValue::Bool(false) {
                                failures.push(format!(
                                    "[{name}] layer '{exp_layer}' missing attr '{key}' (expected {expected_val:?})"
                                ));
                                case_ok = false;
                            }
                        }
                    }
                }

                // If allow_extra_tags is false, check no unexpected attrs.
                // Skip this check when at_zoom is set - we don't implement
                // zoom-dependent attribute filtering yet.
                let has_at_zoom = exp.get("at_zoom").is_some();
                if !has_at_zoom
                    && exp.get("allow_extra_tags").and_then(serde_yaml::Value::as_bool)
                        == Some(false)
                {
                    for (ak, _, _) in &m.attrs {
                        let in_expected =
                            exp_tags.keys().any(|k| k.as_str() == Some(*ak));
                        if !in_expected {
                            failures.push(format!(
                                "[{name}] layer '{exp_layer}' unexpected extra attr '{ak}'"
                            ));
                            case_ok = false;
                        }
                    }
                }
            }
        }

        if case_ok {
            passed += 1;
        } else {
            failed += 1;
        }
    }

    eprintln!("\n=== Shortbread spec results ===");
    eprintln!("  Passed:  {passed}");
    eprintln!("  Failed:  {failed}");
    eprintln!("  Skipped: {skipped}");

    if !failures.is_empty() {
        eprintln!("\nFailures:");
        for f in &failures {
            eprintln!("  {f}");
        }
        panic!("{failed} test cases failed out of {}", passed + failed);
    }
}

// ---------------------------------------------------------------------------
// Bug regression tests (2026-03-03 audit)
// ---------------------------------------------------------------------------

#[test]
fn test_b1_wetland_with_natural_tag() {
    // B1: natural=wetland + wetland=bog must match land layer as "bog".
    // Previously, the early-return on natural= blocked the wetland= check.
    let tags = Tags(&[("natural", "wetland"), ("wetland", "bog")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let land = matches
        .iter()
        .find(|m| m.layer == Layer::Land)
        .expect("natural=wetland + wetland=bog should match Land");
    let kind = land
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind attr");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("bog")));
    assert_eq!(land.min_zoom, 11);
}

#[test]
fn test_b1_all_wetland_subtypes() {
    // All 5 wetland subtypes should match when accompanied by natural=wetland.
    for subtype in &["bog", "marsh", "swamp", "string_bog", "wet_meadow"] {
        let tags = Tags(&[("natural", "wetland"), ("wetland", subtype)]);
        let matches = match_element(&tags, OsmGeomType::ClosedWay);
        let land = matches
            .iter()
            .find(|m| m.layer == Layer::Land);
        assert!(
            land.is_some(),
            "natural=wetland + wetland={subtype} should match Land"
        );
    }
}

#[test]
fn test_b2_maritime_no() {
    // B2: maritime=no must NOT set maritime=true on boundaries.
    let tags = Tags(&[
        ("boundary", "administrative"),
        ("admin_level", "2"),
        ("maritime", "no"),
    ]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let boundary = matches
        .iter()
        .find(|m| m.layer == Layer::Boundaries)
        .expect("should match Boundaries");
    let maritime = boundary
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "maritime")
        .expect("should have maritime attr");
    assert_eq!(
        maritime.1,
        AttrValue::Bool(false),
        "maritime=no should produce maritime=false"
    );
}

#[test]
fn test_b2_maritime_yes() {
    // Ensure maritime=yes still works.
    let tags = Tags(&[
        ("boundary", "administrative"),
        ("admin_level", "2"),
        ("maritime", "yes"),
    ]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let boundary = matches
        .iter()
        .find(|m| m.layer == Layer::Boundaries)
        .expect("should match Boundaries");
    let maritime = boundary
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "maritime")
        .expect("should have maritime attr");
    assert_eq!(
        maritime.1,
        AttrValue::Bool(true),
        "maritime=yes should produce maritime=true"
    );
}

#[test]
fn test_b5_multipolygon_address() {
    // B5: multipolygon relations with addr:housenumber should produce address features.
    let tags = Tags(&[
        ("building", "yes"),
        ("addr:housenumber", "7"),
    ]);
    let matches = match_element(&tags, OsmGeomType::MultiPolygon);
    let addr = matches.iter().find(|m| m.layer == Layer::Addresses);
    assert!(
        addr.is_some(),
        "multipolygon with addr:housenumber should match Addresses"
    );
}

#[test]
fn test_b6_unrecognized_amenity_gets_address() {
    // B6: amenity=parking_entrance is not a POI - address should not be suppressed.
    let tags = Tags(&[
        ("amenity", "parking_entrance"),
        ("addr:housenumber", "5"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let addr = matches.iter().find(|m| m.layer == Layer::Addresses);
    assert!(
        addr.is_some(),
        "non-POI amenity should not suppress address"
    );
    let poi = matches.iter().find(|m| m.layer == Layer::Pois);
    assert!(poi.is_none(), "parking_entrance is not a POI");
}

#[test]
fn test_b6_office_company_gets_address() {
    // B6: office=company is not a POI - address should not be suppressed.
    let tags = Tags(&[
        ("office", "company"),
        ("addr:housenumber", "10"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let addr = matches.iter().find(|m| m.layer == Layer::Addresses);
    assert!(
        addr.is_some(),
        "office=company should not suppress address"
    );
}

#[test]
fn test_b6_recognized_amenity_still_excludes_address() {
    // Ensure recognized POI values still suppress addresses.
    let tags = Tags(&[
        ("amenity", "restaurant"),
        ("addr:housenumber", "42"),
    ]);
    let matches = match_element(&tags, OsmGeomType::Node);
    let addr = matches.iter().find(|m| m.layer == Layer::Addresses);
    assert!(addr.is_none(), "restaurant should suppress address");
    let poi = matches.iter().find(|m| m.layer == Layer::Pois);
    assert!(poi.is_some(), "restaurant should match POI");
}

#[test]
fn test_b7_land_fallthrough_multi_tag() {
    // B7: An element with an unrecognized landuse + recognized leisure should
    // fall through and match via leisure.
    let tags = Tags(&[("landuse", "military"), ("leisure", "park")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let land = matches
        .iter()
        .find(|m| m.layer == Layer::Land)
        .expect("landuse=military + leisure=park should match Land via leisure");
    let kind = land
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind attr");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("park")));
}

#[test]
fn test_b7_water_fallthrough_multi_tag() {
    // B7: An element with an unrecognized natural + recognized waterway should
    // fall through and match via waterway.
    let tags = Tags(&[("natural", "cliff"), ("waterway", "riverbank")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let water = matches
        .iter()
        .find(|m| m.layer == Layer::WaterPolygons)
        .expect("natural=cliff + waterway=riverbank should match WaterPolygons via waterway");
    let kind = water
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind attr");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("riverbank")));
}

#[test]
fn test_land_line_cliff_matches() {
    let tags = Tags(&[("natural", "cliff")]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let cliff = matches
        .iter()
        .find(|m| m.layer == Layer::Land && m.geom_expect == GeomExpect::Line)
        .expect("natural=cliff should match land line extension");
    assert_eq!(cliff.min_zoom, 12);
    let kind = cliff
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("cliff")));
}

#[test]
fn test_land_line_cliff_matches_closed_way() {
    let tags = Tags(&[("natural", "cliff")]);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let cliff = matches
        .iter()
        .find(|m| m.layer == Layer::Land && m.geom_expect == GeomExpect::Line)
        .expect("closed-way natural=cliff should also match land line extension");
    assert_eq!(cliff.min_zoom, 12);
    let kind = cliff
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("should have kind");
    assert_eq!(kind.1, AttrValue::Str(Cow::Borrowed("cliff")));
}

#[test]
fn test_land_line_cliff_conflict_with_highway_keeps_both_matches() {
    let tags = Tags(&[
        ("natural", "cliff"),
        ("highway", "primary"),
    ]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);

    let cliff = matches
        .iter()
        .find(|m| m.layer == Layer::Land && m.geom_expect == GeomExpect::Line)
        .expect("cliff line should still match alongside highway");
    let cliff_kind = cliff
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("cliff should have kind");
    assert_eq!(cliff_kind.1, AttrValue::Str(Cow::Borrowed("cliff")));

    let street = matches
        .iter()
        .find(|m| m.layer == Layer::Streets)
        .expect("highway should still match Streets");
    let street_kind = street
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("street should have kind");
    assert_eq!(street_kind.1, AttrValue::Str(Cow::Borrowed("primary")));
}

#[test]
fn test_land_line_cliff_conflict_with_waterway_keeps_both_matches() {
    let tags = Tags(&[
        ("natural", "cliff"),
        ("waterway", "stream"),
    ]);
    let matches = match_element(&tags, OsmGeomType::OpenWay);

    let cliff = matches
        .iter()
        .find(|m| m.layer == Layer::Land && m.geom_expect == GeomExpect::Line)
        .expect("cliff line should still match alongside waterway");
    let cliff_kind = cliff
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("cliff should have kind");
    assert_eq!(cliff_kind.1, AttrValue::Str(Cow::Borrowed("cliff")));

    let water = matches
        .iter()
        .find(|m| m.layer == Layer::WaterLines)
        .expect("waterway should still match WaterLines");
    let water_kind = water
        .attrs
        .iter()
        .find(|(k, _, _)| *k == "kind")
        .expect("water line should have kind");
    assert_eq!(water_kind.1, AttrValue::Str(Cow::Borrowed("stream")));
}
