//! Render a single MVT tile from a PMTiles archive as SVG.

use std::io::{self, Write};
use std::path::Path;

use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

use crate::pmtiles_reader::{PmtilesReader, find_entry};
use crate::pmtiles_writer::xy_to_tile_id;

const EXTENT: f64 = 4096.0;

const LAYER_COLORS: &[&str] = &[
    "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948", "#b07aa1", "#ff9da7",
    "#9c755f", "#bab0ac", "#af7aa1", "#86bcb6", "#d37295", "#8cd17d", "#b6992d", "#499894",
    "#f1ce63", "#d4a6c8", "#9d7660", "#a0cbe8", "#ffbe7d", "#d7b5a6",
];

/// Render a single tile as SVG and write to `out`.
pub fn render_tile_svg(
    pmtiles_path: &Path,
    z: u8,
    x: u32,
    y: u32,
    out: &mut dyn Write,
) -> io::Result<()> {
    render_tile_grid_svg(pmtiles_path, z, x, y, 1, 1, None, out)
}

/// Render a grid of tiles as a single SVG. Each tile is offset by its position
/// in the grid. Width=1, height=1 produces the same output as `render_tile_svg`.
/// If `layer_filter` is Some, only render layers whose names are in the list.
#[allow(clippy::too_many_arguments)]
pub fn render_tile_grid_svg(
    pmtiles_path: &Path,
    z: u8,
    x0: u32,
    y0: u32,
    width: u32,
    height: u32,
    layer_filter: Option<&[&str]>,
    out: &mut dyn Write,
) -> io::Result<()> {
    let mut reader = PmtilesReader::open(pmtiles_path)?;
    let runs = reader.read_all_runs()?;

    // Collect decoded layers per tile position
    let mut tile_layers: Vec<(u32, u32, Vec<SvgLayer>)> = Vec::new();
    for dy in 0..height {
        for dx in 0..width {
            let tx = x0 + dx;
            let ty = y0 + dy;
            let target_id = xy_to_tile_id(z, tx, ty);
            if let Some(entry) = find_entry(&runs, target_id) {
                let decompressed = reader.read_tile(&entry)?;
                let layers = decode_mvt_geometry(&decompressed)?;
                tile_layers.push((dx, dy, layers));
            }
        }
    }

    let svg = build_grid_svg(&tile_layers, width, height, layer_filter);
    out.write_all(svg.as_bytes())
}

/// Build SVG for a grid of tiles. Each tile's geometry is offset by its grid position.
#[allow(clippy::let_underscore_must_use, clippy::too_many_lines)]
fn build_grid_svg(
    tile_layers: &[(u32, u32, Vec<SvgLayer>)],
    width: u32,
    height: u32,
    layer_filter: Option<&[&str]>,
) -> String {
    let total_w = f64::from(width) * EXTENT;
    let total_h = f64::from(height) * EXTENT;
    let px_w = 512 * width;
    let px_h = 512 * height;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {total_w} {total_h}\" width=\"{px_w}\" height=\"{px_h}\">\n"
    ));
    s.push_str(&format!(
        "  <rect width=\"{total_w}\" height=\"{total_h}\" fill=\"#f2efe9\"/>\n"
    ));

    // Draw tile grid lines
    if width > 1 || height > 1 {
        s.push_str("  <g id=\"grid\" opacity=\"0.15\">\n");
        for dx in 1..width {
            let gx = f64::from(dx) * EXTENT;
            s.push_str(&format!(
                "    <line x1=\"{gx}\" y1=\"0\" x2=\"{gx}\" y2=\"{total_h}\" stroke=\"#000\" stroke-width=\"1\"/>\n"
            ));
        }
        for dy in 1..height {
            let gy = f64::from(dy) * EXTENT;
            s.push_str(&format!(
                "    <line x1=\"0\" y1=\"{gy}\" x2=\"{total_w}\" y2=\"{gy}\" stroke=\"#000\" stroke-width=\"1\"/>\n"
            ));
        }
        s.push_str("  </g>\n");
    }

    // Collect all unique layer names in order of first appearance
    let mut layer_names: Vec<String> = Vec::new();
    for (_, _, layers) in tile_layers {
        for layer in layers {
            if !layer_names.contains(&layer.name) {
                if let Some(filter) = layer_filter
                    && !filter.contains(&layer.name.as_str())
                {
                    continue;
                }
                layer_names.push(layer.name.clone());
            }
        }
    }

    // Render by layer (so all ocean across tiles shares the same group)
    let mut path_id: u32 = 0;
    for (li, layer_name) in layer_names.iter().enumerate() {
        let color = LAYER_COLORS[li % LAYER_COLORS.len()];
        s.push_str(&format!("  <g id=\"{layer_name}\" opacity=\"0.8\">\n"));

        for &(dx, dy, ref layers) in tile_layers {
            let ox = f64::from(dx) * EXTENT;
            let oy = f64::from(dy) * EXTENT;

            if let Some(layer) = layers.iter().find(|l| l.name == *layer_name) {
                for feature in &layer.features {
                    match feature.geom_type {
                        1 => {
                            for path in &feature.paths {
                                for &(px, py) in path {
                                    path_id += 1;
                                    s.push_str(&format!(
                                        "    <circle id=\"p{path_id}\" cx=\"{:.1}\" cy=\"{:.1}\" r=\"4\" fill=\"{color}\"/>\n",
                                        px + ox, py + oy
                                    ));
                                }
                            }
                        }
                        2 => {
                            for path in &feature.paths {
                                if path.is_empty() {
                                    continue;
                                }
                                path_id += 1;
                                let d = build_path_d_offset(path, false, ox, oy);
                                s.push_str(&format!(
                                    "    <path id=\"p{path_id}\" d=\"{d}\" fill=\"none\" stroke=\"{color}\" stroke-width=\"1\"/>\n"
                                ));
                            }
                        }
                        3 => {
                            if feature.paths.is_empty() {
                                continue;
                            }
                            path_id += 1;
                            let mut d = String::new();
                            for path in &feature.paths {
                                if path.is_empty() {
                                    continue;
                                }
                                if !d.is_empty() {
                                    d.push(' ');
                                }
                                d.push_str(&build_path_d_offset(path, true, ox, oy));
                            }
                            if !d.is_empty() {
                                s.push_str(&format!(
                                    "    <path id=\"p{path_id}\" d=\"{d}\" fill=\"{color}\" fill-rule=\"evenodd\" stroke=\"{color}\" stroke-width=\"0.5\"/>\n"
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        s.push_str("  </g>\n");
    }

    s.push_str("</svg>\n");
    s
}

fn build_path_d_offset(coords: &[(f64, f64)], close: bool, ox: f64, oy: f64) -> String {
    let mut d = String::new();
    for (i, &(px, py)) in coords.iter().enumerate() {
        if i == 0 {
            d.push_str(&format!("M{:.1} {:.1}", px + ox, py + oy));
        } else {
            d.push_str(&format!(" L{:.1} {:.1}", px + ox, py + oy));
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
                    // Per MVT spec 4.3.3.3 ClosePath does NOT move the cursor;
                    // the next ring's MoveTo is relative to the last LineTo vertex.
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::decode_geometry_commands;

    fn zz(n: i32) -> u32 {
        ((n << 1) ^ (n >> 31)).cast_unsigned()
    }

    #[test]
    fn polygon_closepath_leaves_cursor_at_last_lineto() {
        // MVT spec 4.3.3.3: ClosePath does not move the cursor.
        // Ring 1: (10,10) -> (20,10) -> (20,20) -> close; cursor stays (20,20).
        // Ring 2: (30,30) -> (40,30) -> (40,40) -> close.
        // Ring 2 MoveTo delta is encoded from ring 1's LAST vertex (20,20): +10,+10.
        let cmds = vec![
            (1 | (1 << 3)) as u32,
            zz(10),
            zz(10),
            (2 | (2 << 3)) as u32,
            zz(10),
            zz(0),
            zz(0),
            zz(10),
            (7 | (1 << 3)) as u32,
            (1 | (1 << 3)) as u32,
            zz(10),
            zz(10),
            (2 | (2 << 3)) as u32,
            zz(10),
            zz(0),
            zz(0),
            zz(10),
            (7 | (1 << 3)) as u32,
        ];

        let mut bytes = Vec::new();
        for v in cmds {
            let mut x = v;
            while x >= 0x80 {
                bytes.push(u8::try_from(x & 0x7f).expect("7-bit varint chunk fits in u8") | 0x80);
                x >>= 7;
            }
            bytes.push(u8::try_from(x).expect("final varint byte should fit in u8"));
        }

        let paths = decode_geometry_commands(&bytes, 3).unwrap();
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0][0], (10.0, 10.0));
        assert_eq!(paths[1][0], (30.0, 30.0));
    }
}
