//! Minimal MVT layer summary: names, feature counts by geometry type, keys.
//!
//! A protobuf walker, not container code - it lived in the PMTiles reader
//! only because verify and assembly first needed it there. Malformed layer
//! or feature fields are skipped rather than reported; callers needing a
//! strict decode use `tile_detail`.

use std::io;

use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

pub(crate) struct MvtLayer {
    pub(crate) name: String,
    pub(crate) feature_count: usize,
    pub(crate) points: usize,
    pub(crate) lines: usize,
    pub(crate) polygons: usize,
    pub(crate) keys: Vec<String>,
}

/// Decode the top-level MVT tile structure to extract layer names and feature counts.
pub(crate) fn decode_mvt_layers(data: &[u8]) -> io::Result<Vec<MvtLayer>> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if field == 3 && wire_type == WIRE_LEN {
            let sub = cursor
                .read_len_delimited()
                .map_err(|e| io::Error::other(format!("mvt layer: {e}")))?;
            layers.push(decode_mvt_layer(sub));
        } else {
            cursor
                .skip_field(wire_type)
                .map_err(|e| io::Error::other(format!("mvt skip: {e}")))?;
        }
    }
    Ok(layers)
}

fn decode_mvt_layer(data: &[u8]) -> MvtLayer {
    let mut layer = MvtLayer {
        name: String::new(),
        feature_count: 0,
        points: 0,
        lines: 0,
        polygons: 0,
        keys: Vec::new(),
    };
    let mut features: Vec<&[u8]> = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if wire_type == WIRE_LEN {
            if let Ok(sub) = cursor.read_len_delimited() {
                match field {
                    1 => layer.name = String::from_utf8_lossy(sub).to_string(),
                    2 => features.push(sub),
                    3 => layer.keys.push(String::from_utf8_lossy(sub).to_string()),
                    _ => {}
                }
            }
        } else if cursor.skip_field(wire_type).is_err() {
            break;
        }
    }
    layer.feature_count = features.len();
    for feat_data in features {
        let mut fc = Cursor::new(feat_data);
        while let Ok(Some((ff, fw))) = fc.read_tag() {
            if ff == 3 && fw == WIRE_VARINT {
                if let Ok(gt) = fc.read_varint() {
                    match gt {
                        1 => layer.points += 1,
                        2 => layer.lines += 1,
                        3 => layer.polygons += 1,
                        _ => {}
                    }
                }
            } else if fc.skip_field(fw).is_err() {
                break;
            }
        }
    }
    layer
}
