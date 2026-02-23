// Feature data wire format (sort record payload).
//
// Binary serialization/deserialization of feature data that flows through
// the external merge sort between PBF processing and tile assembly.
//
// Format:
//   u64   osm_id
//   u8    geom_type (1=point, 2=line, 3=polygon)
//   u16   geometry command count
//   u32×  geometry commands
//   u8    attribute count
//   per attribute:
//     u8   key_len, key bytes
//     u8   value_type (0=string, 1=int, 2=bool, 3=float)
//     value bytes

use crate::mvt::{self, GeomType, LayerBuilder, Value};
use crate::shortbread::{self, AttrValue};

const _: () = assert!(cfg!(target_endian = "little"), "wire format assumes little-endian");

/// Pre-encode the attribute portion for a given zoom level.
/// Returns bytes that can be appended to the geometry portion via
/// `encode_feature_data_with_attrs`.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn encode_attrs_bytes(attrs: &[shortbread::Attr], zoom: u8) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
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
    buf
}

/// Encode feature data with pre-encoded attribute bytes (P3 optimization).
pub(crate) fn encode_feature_data_with_attrs(
    osm_id: u64,
    geom_type: GeomType,
    geom_cmds: &[u32],
    attrs_bytes: &[u8],
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(11 + geom_cmds.len() * 4 + attrs_bytes.len());
    buf.extend_from_slice(&osm_id.to_le_bytes());
    buf.push(geom_type as u8);
    #[allow(clippy::cast_possible_truncation)]
    let cmd_count = geom_cmds.len() as u16;
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
    let attrs_bytes = encode_attrs_bytes(attrs, zoom);
    encode_feature_data_with_attrs(osm_id, geom_type, geom_cmds, &attrs_bytes)
}

/// Decode feature data from a sort record and add it to a layer builder.
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn add_feature_to_layer(layer: &mut LayerBuilder, data: &[u8]) {
    if data.len() < 11 {
        return;
    }

    // osm_id
    let osm_id = u64::from_le_bytes(data[0..8].try_into().expect("osm_id"));
    let mut pos: usize = 8;

    // geom_type
    let gt_byte = data[pos];
    pos += 1;
    let geom_type = match gt_byte {
        1 => GeomType::Point,
        2 => GeomType::LineString,
        3 => GeomType::Polygon,
        _ => return,
    };

    // geometry commands — bulk memcpy (little-endian wire format matches native u32 layout)
    let cmd_count = u16::from_le_bytes(data[pos..pos + 2].try_into().expect("cmd_count")) as usize;
    pos += 2;
    let cmd_bytes = cmd_count * 4;
    if pos + cmd_bytes > data.len() {
        return;
    }
    let mut geom_cmds = vec![0u32; cmd_count];
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

    let mut tag_pairs: Vec<(u16, u16)> = Vec::with_capacity(attr_count);
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
                layer.intern_value(Value::String(s.to_string()))
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
