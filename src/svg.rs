//! Render a single MVT tile from a PMTiles archive as SVG.

use std::fmt::Write as FmtWrite;
use std::io::{self, Write};
use std::path::Path;

use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

use crate::pmtiles_reader::PmtilesReader;
use crate::pmtiles_writer::xy_to_tile_id;

const EXTENT: f64 = 4096.0;

const LAYER_COLORS: &[&str] = &[
    "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f",
    "#edc948", "#b07aa1", "#ff9da7", "#9c755f", "#bab0ac",
    "#af7aa1", "#86bcb6", "#d37295", "#8cd17d", "#b6992d",
    "#499894", "#f1ce63", "#d4a6c8", "#9d7660", "#a0cbe8",
    "#ffbe7d", "#d7b5a6",
];

/// Render a single tile as SVG and write to `out`.
pub fn render_tile_svg(
    pmtiles_path: &Path,
    z: u8,
    x: u32,
    y: u32,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut reader = PmtilesReader::open(pmtiles_path)?;
    let target_id = xy_to_tile_id(z, x, y);

    let entries = reader.read_all_entries()?;
    let entry = entries
        .iter()
        .find(|e| e.tile_id == target_id)
        .ok_or_else(|| io::Error::other(format!("tile z{z}/{x}/{y} not found in archive")))?;

    let decompressed = reader.read_tile(entry)?;
    let layers = decode_mvt_geometry(&decompressed)?;
    let svg = build_svg(&layers);
    out.write_all(svg.as_bytes())
}

/// Build the SVG string from decoded layers.
#[allow(clippy::let_underscore_must_use)]
fn build_svg(layers: &[SvgLayer]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {EXTENT} {EXTENT}\" width=\"512\" height=\"512\">\n"
    ));
    s.push_str(&format!(
        "  <rect width=\"{EXTENT}\" height=\"{EXTENT}\" fill=\"#f2efe9\"/>\n"
    ));

    for (i, layer) in layers.iter().enumerate() {
        let color = LAYER_COLORS[i % LAYER_COLORS.len()];
        s.push_str(&format!(
            "  <g id=\"{}\" opacity=\"0.8\">\n",
            layer.name
        ));

        for feature in &layer.features {
            match feature.geom_type {
                1 => {
                    for path in &feature.paths {
                        for &(px, py) in path {
                            s.push_str(&format!(
                                "    <circle cx=\"{px}\" cy=\"{py}\" r=\"4\" fill=\"{color}\"/>\n"
                            ));
                        }
                    }
                }
                2 => {
                    for path in &feature.paths {
                        if path.is_empty() {
                            continue;
                        }
                        let d = build_path_d(path, false);
                        s.push_str(&format!(
                            "    <path d=\"{d}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"1\"/>\n"
                        ));
                    }
                }
                3 => {
                    if feature.paths.is_empty() {
                        continue;
                    }
                    let mut d = String::new();
                    for path in &feature.paths {
                        if path.is_empty() {
                            continue;
                        }
                        if !d.is_empty() {
                            d.push(' ');
                        }
                        d.push_str(&build_path_d(path, true));
                    }
                    if !d.is_empty() {
                        s.push_str(&format!(
                            "    <path d=\"{d}\" fill=\"{color}\" fill-rule=\"evenodd\" stroke=\"{color}\" stroke-width=\"0.5\"/>\n"
                        ));
                    }
                }
                _ => {}
            }
        }

        s.push_str("  </g>\n");
    }

    s.push_str("</svg>\n");
    s
}

fn build_path_d(coords: &[(f64, f64)], close: bool) -> String {
    let mut d = String::new();
    for (i, &(px, py)) in coords.iter().enumerate() {
        if i == 0 {
            d.push_str(&format!("M{px:.1} {py:.1}"));
        } else {
            d.push_str(&format!(" L{px:.1} {py:.1}"));
        }
    }
    if close {
        d.push_str(" Z");
    }
    d
}

// ---------------------------------------------------------------------------
// MVT geometry decoder
// ---------------------------------------------------------------------------

struct SvgLayer {
    name: String,
    features: Vec<SvgFeature>,
}

struct SvgFeature {
    geom_type: u64,
    paths: Vec<Vec<(f64, f64)>>,
}

fn decode_mvt_geometry(data: &[u8]) -> io::Result<Vec<SvgLayer>> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if field == 3 && wire_type == WIRE_LEN {
            let sub = cursor
                .read_len_delimited()
                .map_err(|e| io::Error::other(format!("mvt layer: {e}")))?;
            layers.push(decode_layer(sub)?);
        } else {
            cursor
                .skip_field(wire_type)
                .map_err(|e| io::Error::other(format!("mvt skip: {e}")))?;
        }
    }
    Ok(layers)
}

fn decode_layer(data: &[u8]) -> io::Result<SvgLayer> {
    let mut name = String::new();
    let mut feature_blobs: Vec<&[u8]> = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if wire_type == WIRE_LEN {
            if let Ok(sub) = cursor.read_len_delimited() {
                match field {
                    1 => name = String::from_utf8_lossy(sub).to_string(),
                    2 => feature_blobs.push(sub),
                    _ => {}
                }
            }
        } else if cursor.skip_field(wire_type).is_err() {
            break;
        }
    }

    let mut features = Vec::with_capacity(feature_blobs.len());
    for blob in feature_blobs {
        features.push(decode_feature(blob)?);
    }

    Ok(SvgLayer { name, features })
}

fn decode_feature(data: &[u8]) -> io::Result<SvgFeature> {
    let mut geom_type: u64 = 0;
    let mut geom_bytes: Option<&[u8]> = None;
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        match (field, wire_type) {
            (3, WIRE_VARINT) => {
                geom_type = cursor
                    .read_varint()
                    .map_err(|e| io::Error::other(format!("geom type: {e}")))?;
            }
            (4, WIRE_LEN) => {
                geom_bytes = Some(
                    cursor
                        .read_len_delimited()
                        .map_err(|e| io::Error::other(format!("geom data: {e}")))?,
                );
            }
            _ => {
                cursor
                    .skip_field(wire_type)
                    .map_err(|e| io::Error::other(format!("feature skip: {e}")))?;
            }
        }
    }

    let paths = match geom_bytes {
        Some(bytes) => decode_geometry_commands(bytes, geom_type)?,
        None => Vec::new(),
    };

    Ok(SvgFeature { geom_type, paths })
}

fn decode_geometry_commands(data: &[u8], geom_type: u64) -> io::Result<Vec<Vec<(f64, f64)>>> {
    let commands = decode_packed_varints(data)?;
    let mut paths: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    let mut cx: i64 = 0;
    let mut cy: i64 = 0;
    let mut i = 0;

    while i < commands.len() {
        let op = commands[i];
        i += 1;
        let id = op & 0x7;
        #[allow(clippy::cast_possible_truncation)]
        let count = (op >> 3) as usize;

        match id {
            1 => {
                // MoveTo
                for _ in 0..count {
                    if i + 1 >= commands.len() {
                        break;
                    }
                    if !current.is_empty() {
                        paths.push(std::mem::take(&mut current));
                    }
                    cx += zigzag_decode(commands[i]);
                    cy += zigzag_decode(commands[i + 1]);
                    i += 2;
                    current.push((cx as f64, cy as f64));
                }
            }
            2 => {
                // LineTo
                for _ in 0..count {
                    if i + 1 >= commands.len() {
                        break;
                    }
                    cx += zigzag_decode(commands[i]);
                    cy += zigzag_decode(commands[i + 1]);
                    i += 2;
                    current.push((cx as f64, cy as f64));
                }
            }
            7 if geom_type == 3 && !current.is_empty() => {
                if let Some(&first) = current.first() {
                    current.push(first);
                }
                paths.push(std::mem::take(&mut current));
            }
            _ => {}
        }
    }

    if !current.is_empty() {
        paths.push(current);
    }

    Ok(paths)
}

fn decode_packed_varints(data: &[u8]) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let mut shift = 0u32;
        let mut value: u64 = 0;
        loop {
            if i >= data.len() {
                return Err(io::Error::other("geometry varint truncated"));
            }
            let b = data[i];
            i += 1;
            value |= u64::from(b & 0x7f) << shift;
            if (b & 0x80) == 0 {
                break;
            }
            shift += 7;
            if shift > 63 {
                return Err(io::Error::other("geometry varint too long"));
            }
        }
        let v = u32::try_from(value).map_err(|_| io::Error::other("geometry command > u32"))?;
        out.push(v);
    }
    Ok(out)
}

fn zigzag_decode(v: u32) -> i64 {
    i64::from(v >> 1) ^ -i64::from(v & 1)
}
