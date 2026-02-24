use super::*;
use std::borrow::Cow;

#[test]
fn test_highway_motorway_matches_streets_z5() {
    let mut t = [("highway", "motorway")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("building", "yes")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("building", "no")];
    sort_tags(&mut t);
    let tags = Tags(&t);
    let matches = match_element(&tags, OsmGeomType::ClosedWay);
    let bldg = matches.iter().find(|m| m.layer == Layer::Buildings);
    assert!(bldg.is_none(), "building=no should not match Buildings");
}

#[test]
fn test_place_city_capital() {
    let mut t = [
        ("place", "city"),
        ("name", "Oslo"),
        ("capital", "yes"),
        ("population", "700000"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("natural", "wood")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("natural", "water")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("boundary", "administrative"),
        ("admin_level", "2"),
        ("maritime", "yes"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("highway", "primary"),
        ("tunnel", "yes"),
        ("bridge", "viaduct"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("highway", "motorway_link")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("route", "ferry"),
        ("motor_vehicle", "no"),
        ("name", "Foot Ferry"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let ferry = matches
        .iter()
        .find(|m| m.layer == Layer::Ferries)
        .expect("should match Ferries");
    assert_eq!(ferry.min_zoom, 12);
}

#[test]
fn test_ferry_default_zoom() {
    let mut t = [("route", "ferry"), ("name", "Car Ferry")];
    sort_tags(&mut t);
    let tags = Tags(&t);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let ferry = matches
        .iter()
        .find(|m| m.layer == Layer::Ferries)
        .expect("should match Ferries");
    assert_eq!(ferry.min_zoom, 10);
}

#[test]
fn test_pois_restaurant_cuisine() {
    let mut t = [
        ("amenity", "restaurant"),
        ("name", "Pasta House"),
        ("cuisine", "italian"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
fn test_aerialway_cable_car() {
    let mut t = [("aerialway", "cable_car")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("aeroway", "aerodrome"),
        ("name", "Oslo Airport"),
        ("iata", "OSL"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("amenity", "restaurant"),
        ("addr:housenumber", "42"),
        ("name", "Bistro"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("addr:housenumber", "12B")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("highway", "motorway"), ("name", "E6")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("place", "city"),
        ("name", "Bergen"),
        ("capital", "4"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("amenity", "hospital")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("waterway", "dam")];
    sort_tags(&mut t);
    let tags = Tags(&t);
    let matches = match_element(&tags, OsmGeomType::OpenWay);
    let dam = matches
        .iter()
        .find(|m| m.layer == Layer::DamLines)
        .expect("should match DamLines");
    assert_eq!(dam.min_zoom, 12);
}

#[test]
fn test_bridge_polygon() {
    let mut t = [("man_made", "bridge")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("highway", "residential"), ("oneway", "-1")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [("place", "hamlet"), ("name", "Tiny")];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("amenity", "recycling"),
        ("recycling:glass_bottles", "yes"),
        ("recycling:paper", "yes"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("highway", "motorway"),
        ("ref", "E6;E18"),
        ("name", "Motorveien"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
    let mut t = [
        ("highway", "motorway_junction"),
        ("ref", "23"),
        ("name", "Exit 23"),
    ];
    sort_tags(&mut t);
    let tags = Tags(&t);
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
// YAML spec test — validates against Planetiler's shortbread.spec.yml
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

        let mut tag_refs: Vec<(&str, &str)> = tag_pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        sort_tags(&mut tag_refs);
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

            // Check min_zoom (skip for boundary_labels — area-dependent,
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

                    // Skip way_area — requires geometry area calculation
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
                            // Absent Bool(false) is OK — we omit false booleans
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
                // Skip this check when at_zoom is set — we don't implement
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
