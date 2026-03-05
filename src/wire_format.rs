// Feature data wire format (sort record payload).
//
// Binary serialization/deserialization of feature data that flows through
// the external merge sort between PBF processing and tile assembly.
//
// No version byte: this is an internal ephemeral format used within a single
// pipeline run (in data/tilegen_tmp/). Never persisted across versions or shared.
//
// Format:
//   u64   osm_id
//   u8    geom_type (1=point, 2=line, 3=polygon)
//   u16   geometry command count
//   u32×  geometry commands
//   u8    attribute count
//   per attribute:
//     u8   key_id (index into KEY_NAMES table)
//     u8   value_type (0=string, 1=int, 2=bool, 3=float, 4=interned string)
//     value bytes

use crate::mvt::{self, GeomType, LayerBuilder, Value};
use crate::shortbread::{self, AttrValue};

const _: () = assert!(cfg!(target_endian = "little"), "wire format assumes little-endian");

/// Static key ID table. All attribute key strings used in the Shortbread schema.
/// Index = key_id (u8), used in wire format. New keys MUST be appended (never reorder).
const KEY_NAMES: &[&str] = &[
    "kind",                    // 0
    "name",                    // 1
    "name_en",                 // 2
    "name_de",                 // 3
    "admin_level",             // 4
    "bridge",                  // 5
    "tunnel",                  // 6
    "link",                    // 7
    "rail",                    // 8
    "surface",                 // 9
    "tracktype",               // 10
    "service",                 // 11
    "oneway",                  // 12
    "oneway_reverse",          // 13
    "bicycle",                 // 14
    "horse",                   // 15
    "ref",                     // 16
    "ref_rows",                // 17
    "ref_cols",                // 18
    "population",              // 19
    "maritime",                // 20
    "disputed",                // 21
    "iata",                    // 22
    "housenumber",             // 23
    "housename",               // 24
    "amenity",                 // 25
    "highway",                 // 26
    "office",                  // 27
    "leisure",                 // 28
    "man_made",                // 29
    "tourism",                 // 30
    "cuisine",                 // 31
    "vending",                 // 32
    "religion",                // 33
    "denomination",            // 34
    "sport",                   // 35
    "tower:type",              // 36
    "information",             // 37
    "atm",                     // 38
    "recycling:glass_bottles", // 39
    "recycling:paper",         // 40
    "recycling:clothes",       // 41
    "recycling:scrap_metal",   // 42
    "shop",                    // 43
    "emergency",               // 44
    "historic",                // 45
    "way_area",                // 46 (test-only: Float roundtrip)
    "height",                  // 47 (test-only: zoom-dependent Float)
    "ele",                     // 48 (POI elevation in meters)
    "min_height",              // 49
    "building:levels",         // 50
];

#[allow(clippy::cast_possible_truncation)]
fn key_to_id(key: &str) -> u8 {
    KEY_NAMES.iter().position(|&k| k == key)
        .unwrap_or_else(|| panic!("unknown wire format key: {key:?}")) as u8
}

/// Static value ID table for "kind" attribute values from the Shortbread schema.
/// Index = value_id (u8). Unknown values fall back to raw string encoding (type 0).
/// New values MUST be appended (never reorder).
const KIND_VALUES: &[&str] = &[
    // Water (14)
    "water",             // 0
    "glacier",           // 1
    "riverbank",         // 2
    "dock",              // 3
    "canal",             // 4
    "reservoir",         // 5
    "basin",             // 6
    "river",             // 7
    "stream",            // 8
    "ditch",             // 9
    "dam",               // 10
    "pier",              // 11
    "breakwater",        // 12
    "groyne",            // 13
    // Land (45)
    "forest",            // 14
    "farmland",          // 15
    "farmyard",          // 16
    "meadow",            // 17
    "orchard",           // 18
    "vineyard",          // 19
    "allotments",        // 20
    "brownfield",        // 21
    "cemetery",          // 22
    "commercial",        // 23
    "garages",           // 24
    "grass",             // 25
    "greenfield",        // 26
    "greenhouse_horticulture", // 27
    "industrial",        // 28
    "landfill",          // 29
    "plant_nursery",     // 30
    "quarry",            // 31
    "railway",           // 32
    "recreation_ground", // 33
    "residential",       // 34
    "retail",            // 35
    "village_green",     // 36
    "garden",            // 37
    "golf_course",       // 38
    "miniature_golf",    // 39
    "park",              // 40
    "playground",        // 41
    "bare_rock",         // 42
    "beach",             // 43
    "grassland",         // 44
    "heath",             // 45
    "sand",              // 46
    "scree",             // 47
    "scrub",             // 48
    "shingle",           // 49
    "bog",               // 50
    "marsh",             // 51
    "string_bog",        // 52
    "swamp",             // 53
    "wet_meadow",        // 54
    "grave_yard",        // 55
    "danger_area",       // 56
    "sports_centre",     // 57
    "construction",      // 58
    "bicycle_parking",   // 59
    "college",           // 60
    "hospital",          // 61
    "parking",           // 62
    "prison",            // 63
    "university",        // 64
    // Streets (28 + 5 _link)
    "motorway",          // 65
    "trunk",             // 66
    "primary",           // 67
    "secondary",         // 68
    "tertiary",          // 69
    "unclassified",      // 70
    "busway",            // 71
    "bus_guideway",      // 72
    "living_street",     // 73
    "service",           // 74
    "pedestrian",        // 75
    "track",             // 76
    "footway",           // 77
    "steps",             // 78
    "path",              // 79
    "cycleway",          // 80
    "runway",            // 81
    "taxiway",           // 82
    "rail",              // 83
    "narrow_gauge",      // 84
    "light_rail",        // 85
    "subway",            // 86
    "tram",              // 87
    "funicular",         // 88
    "monorail",          // 89
    "motorway_junction", // 90
    "bridge",            // 91
    "motorway_link",     // 92
    "trunk_link",        // 93
    "primary_link",      // 94
    "secondary_link",    // 95
    "tertiary_link",     // 96
    // Transport (19)
    "cable_car",         // 97
    "gondola",           // 98
    "goods",             // 99
    "chair_lift",        // 100
    "drag_lift",         // 101
    "t-bar",             // 102
    "j-bar",             // 103
    "platter",           // 104
    "rope_tow",          // 105
    "ferry",             // 106
    "aerialway_station", // 107
    "aerodrome",         // 108
    "helipad",           // 109
    "bus_station",       // 110
    "ferry_terminal",    // 111
    "station",           // 112
    "halt",              // 113
    "tram_stop",         // 114
    "bus_stop",          // 115
    // Place labels (13)
    "capital",           // 116
    "state_capital",     // 117
    "city",              // 118
    "town",              // 119
    "village",           // 120
    "hamlet",            // 121
    "suburb",            // 122
    "quarter",           // 123
    "neighbourhood",     // 124
    "isolated_dwelling", // 125
    "farm",              // 126
    "island",            // 127
    "locality",          // 128
];

#[allow(clippy::cast_possible_truncation)]
fn kind_value_to_id(s: &str) -> Option<u8> {
    KIND_VALUES.iter().position(|&v| v == s).map(|i| i as u8)
}

/// Pre-encode the attribute portion for a given zoom level into `buf`.
/// The buffer is cleared and filled with bytes that can be appended to
/// the geometry portion via `encode_feature_data_with_attrs`.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_attrs_bytes(buf: &mut Vec<u8>, attrs: &[shortbread::Attr], zoom: u8) {
    buf.clear();
    let filtered_count = attrs.iter().filter(|(_, _, az)| zoom >= *az).count().min(u8::MAX as usize);
    buf.push(filtered_count as u8);
    for (key, val, attr_zoom) in attrs {
        if zoom < *attr_zoom {
            continue;
        }
        let kid = key_to_id(key);
        buf.push(kid);
        match val {
            AttrValue::Str(s) => {
                // For the "kind" key (id 0), try interning the value
                if kid == 0 {
                    if let Some(vid) = kind_value_to_id(s) {
                        buf.push(4);
                        buf.push(vid);
                    } else {
                        buf.push(0);
                        let sb = s.as_bytes();
                        let slen = sb.len().min(u16::MAX as usize);
                        buf.extend_from_slice(&(slen as u16).to_le_bytes());
                        buf.extend_from_slice(&sb[..slen]);
                    }
                } else {
                    buf.push(0);
                    let sb = s.as_bytes();
                    let slen = sb.len().min(u16::MAX as usize);
                    buf.extend_from_slice(&(slen as u16).to_le_bytes());
                    buf.extend_from_slice(&sb[..slen]);
                }
            }
            AttrValue::Int(i) => {
                buf.push(1);
                buf.extend_from_slice(&i.to_le_bytes());
            }
            AttrValue::Bool(b) => {
                buf.push(2);
                buf.push(u8::from(*b));
            }
            AttrValue::Float(f) => {
                buf.push(3);
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
    }
}

/// Encode feature data with pre-encoded attribute bytes (P3 optimization).
/// Returns an owned `Box<[u8]>` because the caller stores it as `SortRecord.data`
/// which must own its bytes for the chunk file → k-way merge pipeline.
pub(crate) fn encode_feature_data_with_attrs(
    osm_id: u64,
    geom_type: GeomType,
    geom_cmds: &[u32],
    attrs_bytes: &[u8],
) -> Box<[u8]> {
    let mut buf = Vec::with_capacity(11 + geom_cmds.len() * 4 + attrs_bytes.len());
    buf.extend_from_slice(&osm_id.to_le_bytes());
    buf.push(geom_type as u8);
    #[allow(clippy::cast_possible_truncation)]
    let cmd_count = geom_cmds.len().min(u16::MAX as usize) as u16;
    buf.extend_from_slice(&cmd_count.to_le_bytes());
    for &cmd in geom_cmds {
        buf.extend_from_slice(&cmd.to_le_bytes());
    }
    buf.extend_from_slice(attrs_bytes);
    buf.into_boxed_slice()
}

pub(crate) fn encode_feature_data(
    osm_id: u64,
    geom_type: GeomType,
    geom_cmds: &[u32],
    attrs: &[shortbread::Attr],
    zoom: u8,
) -> Box<[u8]> {
    let mut attrs_bytes = Vec::with_capacity(64);
    encode_attrs_bytes(&mut attrs_bytes, attrs, zoom);
    encode_feature_data_with_attrs(osm_id, geom_type, geom_cmds, &attrs_bytes)
}

/// Decode feature data from a sort record and add it to a layer builder.
/// Silent returns on malformed data are intentional: this is an internal format
/// encoded by our own pipeline, so corruption means a code bug (caught by tests).
/// Logging here would add noise to a billion-call hot path.
#[hotpath::measure]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn add_feature_to_layer(
    layer: &mut LayerBuilder,
    data: &[u8],
    geom_pool: &mut Vec<Vec<u32>>,
    tags_pool: &mut Vec<Vec<(u16, u16)>>,
) {
    if data.len() < 11 {
        return;
    }

    // osm_id
    let osm_id = u64::from_le_bytes(data[0..8].try_into().expect("osm_id"));
    let mut pos: usize = 8;

    // geom_type — inline match mirrors the `as u8` encode on line 69.
    // A TryFrom impl would be more boilerplate than this 4-line match.
    let gt_byte = data[pos];
    pos += 1;
    let geom_type = match gt_byte {
        1 => GeomType::Point,
        2 => GeomType::LineString,
        3 => GeomType::Polygon,
        _ => return,
    };

    // geometry commands — bulk memcpy (little-endian wire format matches native u32 layout).
    // Each Feature owns its Vec<u32> because merge_same_attr_geometries needs random
    // access across all Features in a tile. Vecs are pooled per rayon worker — pop from
    // pool here, reclaimed after encode via LayerBuilder::reclaim_features.
    let cmd_count = u16::from_le_bytes(data[pos..pos + 2].try_into().expect("cmd_count")) as usize;
    pos += 2;
    let cmd_bytes = cmd_count * 4;
    if pos + cmd_bytes > data.len() {
        return;
    }
    let mut geom_cmds = geom_pool.pop().unwrap_or_default();
    geom_cmds.clear();
    geom_cmds.resize(cmd_count, 0);
    // SAFETY: On little-endian, u32 byte layout matches the wire format.
    // We copy `cmd_bytes` bytes from the data slice into the Vec's backing memory.
    // The source slice bounds are checked above. The destination is exactly `cmd_bytes` bytes.
    #[allow(unsafe_code)]
    unsafe {
        std::ptr::copy_nonoverlapping(
            data[pos..].as_ptr(),
            geom_cmds.as_mut_ptr().cast::<u8>(),
            cmd_bytes,
        );
    }
    pos += cmd_bytes;

    // attributes
    if pos >= data.len() {
        return;
    }
    let attr_count = data[pos] as usize;
    pos += 1;

    let mut tag_pairs = tags_pool.pop().unwrap_or_default();
    tag_pairs.clear();
    for _ in 0..attr_count {
        if pos >= data.len() {
            break;
        }
        let key_id = data[pos] as usize;
        pos += 1;
        if key_id >= KEY_NAMES.len() {
            break;
        }
        let key = KEY_NAMES[key_id];

        if pos >= data.len() {
            break;
        }
        let val_type = data[pos];
        pos += 1;

        let ki = layer.intern_key(key);
        let vi = match val_type {
            0 => {
                // string
                if pos + 2 > data.len() { break; }
                let slen = u16::from_le_bytes(data[pos..pos + 2].try_into().expect("slen")) as usize;
                pos += 2;
                if pos + slen > data.len() { break; }
                let s = std::str::from_utf8(&data[pos..pos + slen]).unwrap_or("");
                pos += slen;
                layer.intern_string_value(s)
            }
            1 => {
                // int
                if pos + 8 > data.len() { break; }
                let i = i64::from_le_bytes(data[pos..pos + 8].try_into().expect("int"));
                pos += 8;
                layer.intern_value(Value::Int(i))
            }
            2 => {
                // bool
                if pos >= data.len() { break; }
                let b = data[pos] != 0;
                pos += 1;
                layer.intern_value(Value::Bool(b))
            }
            3 => {
                // float
                if pos + 8 > data.len() { break; }
                let f = f64::from_le_bytes(data[pos..pos + 8].try_into().expect("float"));
                pos += 8;
                layer.intern_value(Value::Double(f))
            }
            4 => {
                // interned string (kind value)
                if pos >= data.len() { break; }
                let vid = data[pos] as usize;
                pos += 1;
                if vid >= KIND_VALUES.len() { break; }
                layer.intern_string_value(KIND_VALUES[vid])
            }
            _ => break,
        };
        tag_pairs.push((ki, vi));
    }

    layer.add_feature(mvt::Feature {
        id: Some(osm_id),
        geom_type,
        geometry: geom_cmds,
        tags: tag_pairs,
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::mvt::{GeomType, LayerBuilder, Value};
    use crate::shortbread::AttrValue;
    use std::borrow::Cow;
    use std::collections::HashMap;

    fn feature_tag_map(layer: &LayerBuilder, feature_idx: usize) -> HashMap<String, Value> {
        let mut out = HashMap::new();
        let f = layer.test_feature(feature_idx);
        for (k, v) in &f.tags {
            out.insert(layer.test_key(*k).to_string(), layer.test_value(*v).clone());
        }
        out
    }

    /// Test 1: Full roundtrip — encode a feature with all 4 attribute types,
    /// decode it via `add_feature_to_layer`, and verify every field matches.
    #[test]
    fn roundtrip_mixed_attribute_types() {
        let osm_id: u64 = 123_456_789;
        let geom_type = GeomType::LineString;
        // MoveTo(1,1) + LineTo(2,2): command(1,1)=9, zigzag(1)=2, zigzag(1)=2, command(2,1)=18, zigzag(1)=2, zigzag(1)=2
        let geom_cmds: Vec<u32> = vec![9, 2, 2, 18, 2, 2];

        let attrs: Vec<shortbread::Attr> = vec![
            ("name", AttrValue::Str(Cow::Borrowed("Main Street")), 0),
            ("admin_level", AttrValue::Int(4), 0),
            ("bridge", AttrValue::Bool(true), 0),
            ("way_area", AttrValue::Float(1234.5), 0),
        ];

        let encoded = encode_feature_data(osm_id, geom_type, &geom_cmds, &attrs, 14);

        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

        // Exactly 1 feature
        assert_eq!(layer.test_feature_count(), 1);

        let f = layer.test_feature(0);

        // osm_id roundtrips
        assert_eq!(f.id, Some(osm_id));

        // geom_type roundtrips
        assert_eq!(f.geom_type, geom_type);

        // geometry commands roundtrip (validates the unsafe memcpy path)
        assert_eq!(f.geometry, geom_cmds);

        // 4 attributes
        assert_eq!(f.tags.len(), 4);

        // Check each key-value pair
        let (k0, v0) = f.tags[0];
        assert_eq!(layer.test_key(k0), "name");
        assert_eq!(*layer.test_value(v0), Value::String("Main Street".to_string()));

        let (k1, v1) = f.tags[1];
        assert_eq!(layer.test_key(k1), "admin_level");
        assert_eq!(*layer.test_value(v1), Value::Int(4));

        let (k2, v2) = f.tags[2];
        assert_eq!(layer.test_key(k2), "bridge");
        assert_eq!(*layer.test_value(v2), Value::Bool(true));

        let (k3, v3) = f.tags[3];
        assert_eq!(layer.test_key(k3), "way_area");
        assert_eq!(*layer.test_value(v3), Value::Double(1234.5));
    }

    /// Test 2: Zoom-dependent attribute filtering — attrs with attr_zoom > current zoom
    /// must be excluded from the encoded output.
    #[test]
    fn zoom_dependent_attribute_filtering() {
        let osm_id: u64 = 42;
        let geom_type = GeomType::Point;
        let geom_cmds: Vec<u32> = vec![9, 10, 20]; // MoveTo(5, 10)

        let attrs: Vec<shortbread::Attr> = vec![
            // attr_zoom=0 → always emit
            ("kind", AttrValue::Str(Cow::Borrowed("city")), 0),
            // attr_zoom=0 → always emit
            ("bridge", AttrValue::Bool(false), 0),
            // attr_zoom=12 → only at zoom >= 12
            ("tunnel", AttrValue::Bool(true), 12),
            // attr_zoom=12 → only at zoom >= 12
            ("surface", AttrValue::Str(Cow::Borrowed("asphalt")), 12),
        ];

        // Encode at zoom=10: only attr_zoom <= 10 should survive
        let encoded = encode_feature_data(osm_id, geom_type, &geom_cmds, &attrs, 10);

        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

        assert_eq!(layer.test_feature_count(), 1);
        let f = layer.test_feature(0);

        // Only 2 attrs should be present (the zoom=0 ones)
        assert_eq!(f.tags.len(), 2);

        let (k0, v0) = f.tags[0];
        assert_eq!(layer.test_key(k0), "kind");
        assert_eq!(*layer.test_value(v0), Value::String("city".to_string()));

        let (k1, v1) = f.tags[1];
        assert_eq!(layer.test_key(k1), "bridge");
        assert_eq!(*layer.test_value(v1), Value::Bool(false));

        // Now encode at zoom=12: all 4 attrs should be present
        let encoded_z12 = encode_feature_data(osm_id, geom_type, &geom_cmds, &attrs, 12);

        let mut layer2 = LayerBuilder::new("test2");
        let mut gp2 = Vec::new();
        let mut tp2 = Vec::new();
        add_feature_to_layer(&mut layer2, &encoded_z12, &mut gp2, &mut tp2);

        let f2 = layer2.test_feature(0);
        assert_eq!(f2.tags.len(), 4);

        let (k2, v2) = f2.tags[2];
        assert_eq!(layer2.test_key(k2), "tunnel");
        assert_eq!(*layer2.test_value(v2), Value::Bool(true));

        let (k3, v3) = f2.tags[3];
        assert_eq!(layer2.test_key(k3), "surface");
        assert_eq!(*layer2.test_value(v3), Value::String("asphalt".to_string()));
    }

    /// Test 3: Interned kind value roundtrip — "kind" attributes with values in
    /// KIND_VALUES should be encoded as type 4 (interned ID) and decoded back
    /// to the original string.
    #[test]
    fn interned_kind_value_roundtrip() {
        let osm_id: u64 = 100;
        let geom_type = GeomType::Polygon;
        let geom_cmds: Vec<u32> = vec![9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15];

        // Use a known interned kind value
        let attrs: Vec<shortbread::Attr> = vec![
            ("kind", AttrValue::Str(Cow::Borrowed("residential")), 0),
        ];

        let encoded = encode_feature_data(osm_id, geom_type, &geom_cmds, &attrs, 14);

        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

        assert_eq!(layer.test_feature_count(), 1);
        let f = layer.test_feature(0);
        assert_eq!(f.tags.len(), 1);

        let (k0, v0) = f.tags[0];
        assert_eq!(layer.test_key(k0), "kind");
        assert_eq!(*layer.test_value(v0), Value::String("residential".to_string()));
    }

    /// Test 4: Multiple interned kind values from different categories roundtrip.
    #[test]
    fn interned_kind_values_multiple_categories() {
        let geom_cmds: Vec<u32> = vec![9, 10, 20];

        // Test values from different KIND_VALUES categories
        let test_kinds = ["water", "forest", "motorway", "park", "river"];

        for kind in test_kinds {
            let attrs: Vec<shortbread::Attr> = vec![
                ("kind", AttrValue::Str(Cow::Borrowed(kind)), 0),
            ];

            let encoded = encode_feature_data(1, GeomType::Point, &geom_cmds, &attrs, 14);

            let mut layer = LayerBuilder::new("test");
            let mut gp = Vec::new();
            let mut tp = Vec::new();
            add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

            let f = layer.test_feature(0);
            let (_, v0) = f.tags[0];
            assert_eq!(
                *layer.test_value(v0),
                Value::String(kind.to_string()),
                "kind={kind} should roundtrip through interned encoding"
            );
        }
    }

    /// Test 5: Non-interned kind value falls back to raw string encoding.
    #[test]
    fn non_interned_kind_value_fallback() {
        let geom_cmds: Vec<u32> = vec![9, 10, 20];

        // "custom_kind" is not in KIND_VALUES
        let attrs: Vec<shortbread::Attr> = vec![
            ("kind", AttrValue::Str(Cow::Borrowed("custom_kind")), 0),
        ];

        let encoded = encode_feature_data(1, GeomType::Point, &geom_cmds, &attrs, 14);

        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

        let f = layer.test_feature(0);
        let (k0, v0) = f.tags[0];
        assert_eq!(layer.test_key(k0), "kind");
        assert_eq!(*layer.test_value(v0), Value::String("custom_kind".to_string()));
    }

    /// Test 6: Attributes appear exactly at their minzoom boundary (no early leak).
    #[test]
    fn minzoom_boundary_mixed_types() {
        let osm_id: u64 = 999;
        let geom_type = GeomType::Point;
        let geom_cmds: Vec<u32> = vec![9, 10, 20];

        let attrs: Vec<shortbread::Attr> = vec![
            ("kind", AttrValue::Str(Cow::Borrowed("city")), 0),
            ("bridge", AttrValue::Bool(true), 5),
            ("admin_level", AttrValue::Int(6), 10),
            ("height", AttrValue::Float(42.0), 10),
        ];

        // Below boundary: only min_zoom <= 9 attrs survive.
        let encoded_z9 = encode_feature_data(osm_id, geom_type, &geom_cmds, &attrs, 9);
        let mut layer_z9 = LayerBuilder::new("z9");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer_z9, &encoded_z9, &mut gp, &mut tp);
        let z9_tags = feature_tag_map(&layer_z9, 0);
        assert_eq!(z9_tags.len(), 2, "z9 must not include min_zoom=10 attrs");
        assert_eq!(z9_tags.get("kind"), Some(&Value::String("city".to_string())));
        assert_eq!(z9_tags.get("bridge"), Some(&Value::Bool(true)));
        assert!(!z9_tags.contains_key("admin_level"));
        assert!(!z9_tags.contains_key("height"));

        // At boundary: min_zoom=10 attrs appear.
        let encoded_z10 = encode_feature_data(osm_id, geom_type, &geom_cmds, &attrs, 10);
        let mut layer_z10 = LayerBuilder::new("z10");
        let mut gp2 = Vec::new();
        let mut tp2 = Vec::new();
        add_feature_to_layer(&mut layer_z10, &encoded_z10, &mut gp2, &mut tp2);
        let z10_tags = feature_tag_map(&layer_z10, 0);
        assert_eq!(z10_tags.len(), 4, "z10 should include all attrs");
        assert_eq!(z10_tags.get("kind"), Some(&Value::String("city".to_string())));
        assert_eq!(z10_tags.get("bridge"), Some(&Value::Bool(true)));
        assert_eq!(z10_tags.get("admin_level"), Some(&Value::Int(6)));
        assert_eq!(z10_tags.get("height"), Some(&Value::Double(42.0)));
    }

    /// Test 7: Filtered attributes do not leak when the first attr in input is gated out.
    #[test]
    fn minzoom_filtering_leading_attr_gated_out() {
        let attrs: Vec<shortbread::Attr> = vec![
            ("surface", AttrValue::Str(Cow::Borrowed("asphalt")), 12),
            ("kind", AttrValue::Str(Cow::Borrowed("city")), 0),
            ("bridge", AttrValue::Bool(false), 0),
        ];
        let geom_cmds: Vec<u32> = vec![9, 10, 20];

        let encoded = encode_feature_data(1, GeomType::Point, &geom_cmds, &attrs, 11);
        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

        let tags = feature_tag_map(&layer, 0);
        assert_eq!(tags.len(), 2, "gated leading attr should be excluded");
        assert_eq!(tags.get("kind"), Some(&Value::String("city".to_string())));
        assert_eq!(tags.get("bridge"), Some(&Value::Bool(false)));
        assert!(!tags.contains_key("surface"));
    }

    /// Test 8: Pre-encoded attrs path preserves minzoom filtering semantics.
    #[test]
    fn preencoded_attrs_respect_minzoom() {
        let attrs: Vec<shortbread::Attr> = vec![
            ("kind", AttrValue::Str(Cow::Borrowed("city")), 0),
            ("tunnel", AttrValue::Bool(true), 12),
        ];
        let geom_cmds: Vec<u32> = vec![9, 10, 20];

        let mut attrs_bytes = Vec::new();
        encode_attrs_bytes(&mut attrs_bytes, &attrs, 11);
        let encoded = encode_feature_data_with_attrs(7, GeomType::Point, &geom_cmds, &attrs_bytes);

        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);
        let f = layer.test_feature(0);
        assert_eq!(f.tags.len(), 1);
        assert_eq!(layer.test_key(f.tags[0].0), "kind");
    }

    /// Test 9: If all attrs are above zoom, encoded attr section must be empty.
    #[test]
    fn minzoom_filtering_all_attrs_gated_out() {
        let attrs: Vec<shortbread::Attr> = vec![
            ("kind", AttrValue::Str(Cow::Borrowed("city")), 5),
            ("bridge", AttrValue::Bool(true), 5),
        ];
        let geom_cmds: Vec<u32> = vec![9, 10, 20];

        let encoded = encode_feature_data(42, GeomType::Point, &geom_cmds, &attrs, 4);
        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

        let f = layer.test_feature(0);
        assert_eq!(f.tags.len(), 0, "z4 should include no attrs when all min_zoom=5");
    }

    /// Test 10: Attr count byte is capped at u8::MAX for very large attr lists.
    #[test]
    fn attrs_count_is_capped_to_u8_max() {
        let geom_cmds: Vec<u32> = vec![9, 10, 20];
        let attrs: Vec<shortbread::Attr> = (0..300)
            .map(|_| ("kind", AttrValue::Str(Cow::Borrowed("city")), 0))
            .collect();

        let encoded = encode_feature_data(7, GeomType::Point, &geom_cmds, &attrs, 14);
        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

        let f = layer.test_feature(0);
        assert_eq!(f.tags.len(), 255, "attrs must be capped to u8::MAX entries");
    }

    #[test]
    fn attrs_cap_boundaries_remain_decodeable_with_mixed_values() {
        let geom_cmds: Vec<u32> = vec![9, 10, 20];
        let attrs: Vec<shortbread::Attr> = (0..300)
            .map(|i| {
                if i < 255 {
                    ("bridge", AttrValue::Bool(false), 0)
                } else {
                    ("bridge", AttrValue::Bool(true), 0)
                }
            })
            .collect();

        let encoded = encode_feature_data(77, GeomType::Point, &geom_cmds, &attrs, 14);
        let mut layer = LayerBuilder::new("test");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer, &encoded, &mut gp, &mut tp);

        let f = layer.test_feature(0);
        assert_eq!(f.tags.len(), 255);
        for (_, v) in &f.tags {
            assert_eq!(*layer.test_value(*v), Value::Bool(false));
        }
    }

    #[test]
    fn attrs_cap_and_minzoom_interaction_keeps_expected_prefix() {
        let geom_cmds: Vec<u32> = vec![9, 10, 20];
        let mut attrs: Vec<shortbread::Attr> = Vec::new();

        // 220 base attrs always present.
        for _ in 0..220 {
            attrs.push(("kind", AttrValue::Str(Cow::Borrowed("city")), 0));
        }
        // 120 attrs gated to z12.
        for _ in 0..120 {
            attrs.push(("name", AttrValue::Str(Cow::Borrowed("late")), 12));
        }

        // At z11, gated attrs are excluded and no cap is hit.
        let encoded_z11 = encode_feature_data(10, GeomType::Point, &geom_cmds, &attrs, 11);
        let mut layer_z11 = LayerBuilder::new("z11");
        let mut gp = Vec::new();
        let mut tp = Vec::new();
        add_feature_to_layer(&mut layer_z11, &encoded_z11, &mut gp, &mut tp);
        let f11 = layer_z11.test_feature(0);
        assert_eq!(f11.tags.len(), 220);
        for (k, _) in &f11.tags {
            assert_eq!(layer_z11.test_key(*k), "kind");
        }

        // At z12, both groups are eligible but capped at 255 total entries.
        let encoded_z12 = encode_feature_data(11, GeomType::Point, &geom_cmds, &attrs, 12);
        let mut layer_z12 = LayerBuilder::new("z12");
        let mut gp2 = Vec::new();
        let mut tp2 = Vec::new();
        add_feature_to_layer(&mut layer_z12, &encoded_z12, &mut gp2, &mut tp2);
        let f12 = layer_z12.test_feature(0);
        assert_eq!(f12.tags.len(), 255);
        let kind_count = f12
            .tags
            .iter()
            .filter(|(k, _)| layer_z12.test_key(*k) == "kind")
            .count();
        let name_count = f12
            .tags
            .iter()
            .filter(|(k, _)| layer_z12.test_key(*k) == "name")
            .count();
        assert_eq!(kind_count, 220);
        assert_eq!(name_count, 35);
    }

}
