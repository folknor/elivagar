//! Render a single MVT tile from a PMTiles archive as SVG.

use std::io::{self, Write};
use std::path::Path;

use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

use crate::pmtiles_reader::{PmtilesReader, find_entry};
use crate::pmtiles_writer::xy_to_tile_id;
use crate::shortbread::Layer;

use crate::geometry::tiles::EXTENT;

const LAYER_COLORS: [&str; Layer::count()] = [
    "#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948", "#b07aa1", "#ff9da7",
    "#9c755f", "#bab0ac", "#af7aa1", "#86bcb6", "#d37295", "#8cd17d", "#b6992d", "#499894",
    "#f1ce63", "#d4a6c8", "#9d7660", "#a0cbe8", "#ffbe7d", "#d7b5a6", "#003f5c", "#58508d",
    "#bc5090", "#ff6361",
];

const UNKNOWN_LAYER_COLOR: &str = "#555555";

/// Check that a tile or tile grid fits the PMTiles zoom matrix.
///
/// PMTiles tile IDs in this crate support zoom levels 0 through 31. Grid
/// coordinates use an exclusive end, so a grid ending at `2^z` is valid.
pub fn validate_tile_grid_coordinates(
    z: u8,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> io::Result<()> {
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "tile grid width and height must be nonzero",
        ));
    }
    if z > 31 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("tile zoom {z} is outside the supported range 0 through 31"),
        ));
    }

    let matrix_size = 1_u64 << z;
    let x_start = u64::from(x);
    let y_start = u64::from(y);
    let x_end = x_start + u64::from(width);
    let y_end = y_start + u64::from(height);

    if x_start >= matrix_size || x_end > matrix_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "tile grid x range {x}..{x_end} is outside zoom {z} (valid coordinates: 0..{})",
                matrix_size - 1
            ),
        ));
    }
    if y_start >= matrix_size || y_end > matrix_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "tile grid y range {y}..{y_end} is outside zoom {z} (valid coordinates: 0..{})",
                matrix_size - 1
            ),
        ));
    }

    Ok(())
}

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
    validate_tile_grid_coordinates(z, x0, y0, width, height)?;
    validate_layer_filter(layer_filter)?;

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
    let px_w = 512 * u64::from(width);
    let px_h = 512 * u64::from(height);

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

    // Collect unique layer names, then use a fixed order instead of MVT
    // appearance order so the same archive layer is painted consistently.
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
    layer_names.sort_by(|a, b| {
        layer_paint_order(a)
            .cmp(&layer_paint_order(b))
            .then_with(|| a.cmp(b))
    });

    // Render by layer (so all ocean across tiles shares the same group)
    let mut path_id: u32 = 0;
    for layer_name in &layer_names {
        let color = layer_color(layer_name);
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
                                    "    <path id=\"p{path_id}\" d=\"{d}\" fill=\"{color}\" fill-rule=\"nonzero\" stroke=\"{color}\" stroke-width=\"0.5\"/>\n"
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

/// Check that every requested filter name is a Shortbread layer.
pub fn validate_layer_filter(layer_filter: Option<&[&str]>) -> io::Result<()> {
    if let Some(filter) = layer_filter {
        for name in filter {
            if Layer::from_name(name).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown Shortbread layer '{name}'"),
                ));
            }
        }
    }
    Ok(())
}

fn layer_paint_order(name: &str) -> usize {
    match Layer::from_name(name) {
        Some(Layer::Ocean) => 0,
        Some(layer) => usize::from(layer.tile_order()) + 1,
        None => Layer::count() + 1,
    }
}

fn layer_color(name: &str) -> &'static str {
    Layer::from_name(name).map_or(UNKNOWN_LAYER_COLOR, |layer| {
        LAYER_COLORS[usize::from(layer.tile_order())]
    })
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
    let mut extent = EXTENT;
    let mut cursor = Cursor::new(data);
    while let Ok(Some((field, wire_type))) = cursor.read_tag() {
        if field == 5 && wire_type == WIRE_VARINT {
            let value = cursor
                .read_varint()
                .map_err(|e| io::Error::other(format!("layer extent: {e}")))?;
            if value == 0 || value > u64::from(u32::MAX) {
                return Err(io::Error::other("invalid layer extent"));
            }
            extent = f64::from(
                u32::try_from(value).map_err(|_| io::Error::other("invalid layer extent"))?,
            );
        } else if wire_type == WIRE_LEN {
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
        let mut feature = decode_feature(blob)?;
        // Normalize foreign layer coordinates into the SVG canvas grid.
        let scale = EXTENT / extent;
        for path in &mut feature.paths {
            for (x, y) in path {
                *x *= scale;
                *y *= scale;
            }
        }
        features.push(feature);
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
    use std::io;

    use super::{
        SvgFeature, SvgLayer, build_grid_svg, decode_geometry_commands, layer_color,
        validate_layer_filter, validate_tile_grid_coordinates,
    };

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

    #[test]
    fn tile_grid_coordinates_must_fit_zoom_matrix() {
        assert!(validate_tile_grid_coordinates(0, 0, 0, 1, 1).is_ok());
        assert!(validate_tile_grid_coordinates(2, 3, 3, 1, 1).is_ok());
        assert!(validate_tile_grid_coordinates(31, 2_147_483_647, 0, 1, 1).is_ok());
        assert!(validate_tile_grid_coordinates(0, 0, 0, 0, 1).is_err());
        assert!(validate_tile_grid_coordinates(0, 0, 0, 1, 0).is_err());
        assert_eq!(
            validate_tile_grid_coordinates(2, 3, 0, 2, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_tile_grid_coordinates(31, 2_147_483_648, 0, 1, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            validate_tile_grid_coordinates(32, 0, 0, 1, 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn svg_layers_have_stable_colors_and_ocean_is_painted_first() {
        fn point_layer(name: &str, x: f64) -> SvgLayer {
            SvgLayer {
                name: name.to_string(),
                features: vec![SvgFeature {
                    geom_type: 1,
                    paths: vec![vec![(x, 1.0)]],
                }],
            }
        }

        let ocean_color = layer_color("ocean");
        let svg = build_grid_svg(
            &[(
                0,
                0,
                vec![point_layer("land", 1.0), point_layer("ocean", 2.0)],
            )],
            1,
            1,
            None,
        );
        let reversed = build_grid_svg(
            &[(
                0,
                0,
                vec![point_layer("ocean", 2.0), point_layer("land", 1.0)],
            )],
            1,
            1,
            None,
        );

        assert!(svg.find("<g id=\"ocean\"").unwrap() < svg.find("<g id=\"land\"").unwrap());
        assert!(svg.contains(&format!("fill=\"{ocean_color}\"")));
        assert_eq!(svg, reversed);
    }

    #[test]
    fn svg_layer_filter_rejects_unknown_names() {
        assert!(validate_layer_filter(Some(&["ocean", "boundaries"])).is_ok());
        assert_eq!(
            validate_layer_filter(Some(&["oecan"])).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
