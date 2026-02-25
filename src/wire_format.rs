// Feature data wire format (sort record payload).
//
// Binary serialization/deserialization of feature data that flows through
// the external merge sort between PBF processing and tile assembly.
//
// No version byte: this is an internal ephemeral format used within a single
// pipeline run (in .tilegen_tmp/). Never persisted across versions or shared.
//
// Format:
//   u64   osm_id
//   u8    geom_type (1=point, 2=line, 3=polygon)
//   u32   geometry command count
//   u32×  geometry commands
//   u8    attribute count
//   per attribute:
//     u8   key_len, key bytes
//     u8   value_type (0=string, 1=int, 2=bool, 3=float)
//     value bytes

use crate::mvt::{self, GeomType, LayerBuilder, Value};
use crate::shortbread::{self, AttrValue};

const _: () = assert!(cfg!(target_endian = "little"), "wire format assumes little-endian");

/// Pre-encode the attribute portion for a given zoom level into `buf`.
/// The buffer is cleared and filled with bytes that can be appended to
/// the geometry portion via `encode_feature_data_with_attrs`.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_attrs_bytes(buf: &mut Vec<u8>, attrs: &[shortbread::Attr], zoom: u8) {
    buf.clear();
    let filtered_count = attrs.iter().filter(|(_, _, az)| zoom >= *az).count();
    buf.push(filtered_count as u8);
    for (key, val, attr_zoom) in attrs {
        if zoom < *attr_zoom {
            continue;
        }
        let kb = key.as_bytes();
        buf.push(kb.len() as u8);
        buf.extend_from_slice(kb);
        match val {
            AttrValue::Str(s) => {
                buf.push(0);
                let sb = s.as_bytes();
                // Length truncated to u16 (max 65535). Safe: Shortbread only
                // extracts tag keys (name, ref, cuisine, etc.) whose real-world
                // OSM values never approach 64KB.
                debug_assert!(sb.len() <= u16::MAX as usize, "string value exceeds u16 length");
                buf.extend_from_slice(&(sb.len() as u16).to_le_bytes());
                buf.extend_from_slice(sb);
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
/// Returns an owned Vec because the caller stores it as `SortRecord.data` which
/// must own its bytes for the chunk file → k-way merge pipeline. Passing a
/// `&mut Vec<u8>` would still require `.to_vec()` into the SortRecord, saving
/// only the capacity calculation.
pub(crate) fn encode_feature_data_with_attrs(
    osm_id: u64,
    geom_type: GeomType,
    geom_cmds: &[u32],
    attrs_bytes: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(13 + geom_cmds.len() * 4 + attrs_bytes.len());
    buf.extend_from_slice(&osm_id.to_le_bytes());
    buf.push(geom_type as u8);
    #[allow(clippy::cast_possible_truncation)]
    let cmd_count = geom_cmds.len() as u32;
    buf.extend_from_slice(&cmd_count.to_le_bytes());
    for &cmd in geom_cmds {
        buf.extend_from_slice(&cmd.to_le_bytes());
    }
    buf.extend_from_slice(attrs_bytes);
    buf
}

pub(crate) fn encode_feature_data(
    osm_id: u64,
    geom_type: GeomType,
    geom_cmds: &[u32],
    attrs: &[shortbread::Attr],
    zoom: u8,
) -> Vec<u8> {
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
    if data.len() < 13 {
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
    let cmd_count = u32::from_le_bytes(data[pos..pos + 4].try_into().expect("cmd_count")) as usize;
    pos += 4;
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
        let key_len = data[pos] as usize;
        pos += 1;
        if pos + key_len >= data.len() {
            break;
        }
        let key = std::str::from_utf8(&data[pos..pos + key_len]).unwrap_or("");
        pos += key_len;

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
}
