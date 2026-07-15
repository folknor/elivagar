use std::cmp::Reverse;
use std::io::{self, Write};
use std::sync::Arc;

use crate::corpus::style::{Paint, Style};
use crate::pmtiles_reader::{ArchiveView, find_entry, gzip_decompress};
use crate::pmtiles_writer::xy_to_tile_id;
use crate::regress::{
    DetailAttr, DetailFeature, compare_detail_features, decode_detail_attr, decode_detail_feature,
};
use protohoggr::{Cursor, WIRE_LEN, WIRE_VARINT};

pub struct RenderTile {
    pub layers: Vec<RenderLayer>,
}
pub struct RenderLayer {
    pub name: Arc<str>,
    pub extent: u32,
    pub features: Vec<RenderFeature>,
}
pub struct RenderFeature {
    pub id: Option<u64>,
    pub geom_type: u8,
    pub(crate) attrs: Vec<(Arc<str>, DetailAttr)>,
    pub wire_paths: Vec<Vec<(i32, i32)>>,
}
pub struct RenderedSvg {
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
}

pub fn decode_render_tile(data: &[u8]) -> Result<RenderTile, String> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire)) = cursor.read_tag().map_err(|e| e.to_string())? {
        if field != 3 || wire != WIRE_LEN {
            return Err("invalid MVT tile".into());
        }
        let layer = cursor.read_len_delimited().map_err(|e| e.to_string())?;
        layers.push(decode_render_layer(layer)?);
    }
    Ok(RenderTile { layers })
}
// Decode one layer into canonical-order features carrying WIRE-order geometry:
// classifyRings (section 3.2) reproduces MapLibre only when it runs over rings
// in wire order, so geometry is walked verbatim while the canonical feature
// order comes from the detail decode's `compare_detail_features` comparator.
#[allow(clippy::type_complexity)]
fn decode_render_layer(data: &[u8]) -> Result<RenderLayer, String> {
    let (name, extent, keys, values, feature_bytes) = wire_layer(data)?;
    let mut items: Vec<(DetailFeature, Vec<Vec<(i32, i32)>>)> =
        Vec::with_capacity(feature_bytes.len());
    for bytes in &feature_bytes {
        let detail = decode_detail_feature(bytes, &keys, &values)?;
        let (_, wire_paths) = wire_feature_paths(bytes)?;
        items.push((detail, wire_paths));
    }
    items.sort_by(|a, b| compare_detail_features(&a.0, &b.0));
    let features = items
        .into_iter()
        .map(|(detail, wire_paths)| RenderFeature {
            id: detail.id,
            geom_type: detail.geom_type,
            attrs: detail.attrs,
            wire_paths,
        })
        .collect();
    Ok(RenderLayer {
        name: Arc::from(name),
        extent,
        features,
    })
}
pub fn classify_rings(rings: &[Vec<(i32, i32)>]) -> (Vec<Vec<usize>>, u32) {
    if rings.len() <= 1 {
        return (
            if rings.is_empty() {
                Vec::new()
            } else {
                vec![vec![0]]
            },
            0,
        );
    }
    let indexed: Vec<(usize, i128)> = rings
        .iter()
        .enumerate()
        .map(|(i, r)| (i, area(r)))
        .filter(|(_, a)| *a != 0)
        .collect();
    let Some((_, first)) = indexed.first().copied() else {
        return (Vec::new(), 0);
    };
    let outer = first > 0;
    let mut groups = Vec::new();
    for (i, signed) in indexed {
        if (signed > 0) == outer || groups.is_empty() {
            groups.push(vec![i]);
        } else if let Some(group) = groups.last_mut() {
            group.push(i);
        }
    }
    let mut clamped = 0u32;
    for group in &mut groups {
        if group.len() > 500 {
            let before = group.len();
            group.sort_by_key(|&index| Reverse(area(&rings[index]).unsigned_abs()));
            group.truncate(500);
            clamped = clamped.saturating_add(u32::try_from(before - 500).unwrap_or(u32::MAX));
        }
    }
    (groups, clamped)
}
fn area(ring: &[(i32, i32)]) -> i128 {
    if ring.is_empty() {
        return 0;
    }
    ring.iter()
        .enumerate()
        .map(|(i, p)| {
            let q = ring[(i + ring.len() - 1) % ring.len()];
            i128::from(q.0 - p.0) * i128::from(q.1 + p.1)
        })
        .sum()
}
pub(crate) fn path_data(paths: &[&[(i32, i32)]], close: bool) -> String {
    let mut out = String::new();
    for path in paths {
        if let Some((x, y)) = path.first() {
            out.push_str(&format!("M{x} {y}"));
            for (x, y) in &path[1..] {
                out.push_str(&format!(" L{x} {y}"));
            }
            if close {
                out.push_str(" Z");
            }
        }
    }
    out
}
pub fn render_svg(
    tile: &RenderTile,
    z: u8,
    x: u32,
    y: u32,
    style: &Style,
    layers: Option<&[String]>,
) -> Result<RenderedSvg, String> {
    let mut bytes = format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 4096 4096\" width=\"512\" height=\"512\">\n  <title>{z}/{x}/{y}</title>\n  <rect width=\"4096\" height=\"4096\" fill=\"{}\"/>\n", xml_attr(&style.file.background)).into_bytes();
    let mut refs: Vec<&RenderLayer> = tile
        .layers
        .iter()
        .filter(|l| layers.is_none_or(|wanted| wanted.iter().any(|name| name == l.name.as_ref())))
        .collect();
    refs.sort_by_key(|l| {
        (
            style.position(&l.name).unwrap_or(usize::MAX),
            l.name.as_ref(),
        )
    });
    let mut warnings = Vec::new();
    for layer in refs {
        if layer.extent == 0 || 4096 % layer.extent != 0 {
            return Err(format!(
                "{z}/{x}/{y} layer {} has non-integral extent {}",
                layer.name, layer.extent
            ));
        }
        if style.position(&layer.name).is_none() {
            warnings.push(format!("unstyled layer {}", layer.name));
        }
        let scale = 4096 / layer.extent;
        bytes.extend_from_slice(
            format!(
                "  <g id=\"{}\"{}>\n",
                xml_attr(&layer.name),
                if scale == 1 {
                    String::new()
                } else {
                    format!(" transform=\"scale({scale})\"")
                }
            )
            .as_bytes(),
        );
        for (i, f) in layer.features.iter().enumerate() {
            let paint = style.resolve(&layer.name, &f.attrs);
            emit_feature(&mut bytes, &layer.name, i, f, &paint);
        }
        bytes.extend_from_slice(b"  </g>\n");
    }
    bytes.extend_from_slice(b"</svg>\n");
    Ok(RenderedSvg { bytes, warnings })
}
fn emit_feature(out: &mut Vec<u8>, layer: &str, i: usize, f: &RenderFeature, paint: &Paint) {
    let id = xml_attr(&format!("{layer}-f{i}"));
    match f.geom_type {
        3 => {
            let (groups, clamped) = classify_rings(&f.wire_paths);
            for (j, g) in groups.iter().enumerate() {
                let paths: Vec<_> = g.iter().map(|&n| f.wire_paths[n].as_slice()).collect();
                let mut s = format!(
                    "    <path id=\"{}-p{j}\" d=\"{}\" fill=\"{}\" fill-rule=\"nonzero\"",
                    id,
                    xml_attr(&path_data(&paths, true)),
                    xml_attr(paint.fill.as_deref().unwrap_or("#ff00ff"))
                );
                if let Some(v) = &paint.fill_opacity {
                    s.push_str(&format!(" fill-opacity=\"{}\"", xml_attr(v)));
                }
                if let Some(v) = &paint.stroke {
                    s.push_str(&format!(" stroke=\"{}\"", xml_attr(v)));
                }
                if let Some(v) = &paint.stroke_width {
                    s.push_str(&format!(" stroke-width=\"{}\"", xml_attr(v)));
                }
                if clamped > 0 {
                    s.push_str(&format!(" data-clamped=\"{clamped}\""));
                }
                s.push_str("/>\n");
                out.extend_from_slice(s.as_bytes());
            }
        }
        2 => {
            let paths: Vec<_> = f.wire_paths.iter().map(Vec::as_slice).collect();
            let mut s = format!(
                "    <path id=\"{id}\" d=\"{}\" fill=\"none\" stroke=\"{}\" stroke-width=\"{}\"",
                xml_attr(&path_data(&paths, false)),
                xml_attr(
                    paint
                        .stroke
                        .as_deref()
                        .or(paint.fill.as_deref())
                        .unwrap_or("#ff00ff")
                ),
                xml_attr(paint.stroke_width.as_deref().unwrap_or("1"))
            );
            if let Some(v) = &paint.stroke_dasharray {
                s.push_str(&format!(" stroke-dasharray=\"{}\"", xml_attr(v)));
            }
            if let Some(v) = &paint.stroke_opacity {
                s.push_str(&format!(" stroke-opacity=\"{}\"", xml_attr(v)));
            }
            s.push_str("/>\n");
            out.extend_from_slice(s.as_bytes());
        }
        1 => {
            for (k, path) in f.wire_paths.iter().enumerate() {
                for (x, y) in path {
                    out.extend_from_slice(format!("    <circle id=\"{id}-p{k}\" cx=\"{x}\" cy=\"{y}\" r=\"{}\" fill=\"{}\"/>\n",paint.point_radius.unwrap_or(4),xml_attr(paint.fill.as_deref().or(paint.stroke.as_deref()).unwrap_or("#ff00ff"))).as_bytes());
                }
            }
        }
        _ => {}
    }
}
pub fn render_archive_tile(
    archive: &ArchiveView,
    z: u8,
    x: u32,
    y: u32,
    style: &Style,
    layers: Option<&[String]>,
) -> io::Result<RenderedSvg> {
    let runs = archive.read_all_runs()?;
    let id = xy_to_tile_id(z, x, y);
    let raw = if let Some(e) = find_entry(&runs, id) {
        gzip_decompress(archive.raw_blob_at(e.offset, e.length)?)?
    } else {
        Vec::new()
    };
    let tile = decode_render_tile(&raw).map_err(io::Error::other)?;
    render_svg(&tile, z, x, y, style, layers).map_err(io::Error::other)
}
pub fn dump_ring_grouping(archive: &ArchiveView, out: &mut dyn Write) -> io::Result<()> {
    let mut lines = Vec::new();
    for run in archive.read_all_runs()? {
        let raw = gzip_decompress(archive.raw_blob_at(run.offset, run.length)?)?;
        for tile_id in run.tile_id..run.tile_id + u64::from(run.run_length) {
            let (z, x, y) = crate::pmtiles_writer::tile_id_to_zxy(tile_id);
            let mut tile = Cursor::new(&raw);
            while let Some((field, wire)) = tile.read_tag().map_err(io::Error::other)? {
                if field != 3 || wire != WIRE_LEN {
                    return Err(io::Error::other("invalid MVT tile"));
                }
                let layer = tile.read_len_delimited().map_err(io::Error::other)?;
                let (name, _extent, _keys, _values, features) =
                    wire_layer(layer).map_err(io::Error::other)?;
                for (i, feature) in features.iter().enumerate() {
                    let (typ, rings) = wire_feature_paths(feature).map_err(io::Error::other)?;
                    if typ != 3 {
                        continue;
                    };
                    let (g, _) = classify_rings(&rings);
                    let group = if g.is_empty() {
                        "-".into()
                    } else {
                        g.iter()
                            .map(|v| {
                                v.iter()
                                    .map(|&n| rings[n].len().to_string())
                                    .collect::<Vec<_>>()
                                    .join("+")
                            })
                            .collect::<Vec<_>>()
                            .join("|")
                    };
                    lines.push(format!("{z}/{x}/{y} {name} {i} {group}"));
                }
            }
        }
    }
    lines.sort();
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}
#[allow(clippy::type_complexity)]
fn wire_layer(
    data: &[u8],
) -> Result<(String, u32, Vec<Arc<str>>, Vec<DetailAttr>, Vec<Vec<u8>>), String> {
    let mut c = Cursor::new(data);
    let mut name = String::new();
    let mut extent = 4096u32;
    let mut keys: Vec<Arc<str>> = Vec::new();
    let mut values = Vec::new();
    let mut features = Vec::new();
    while let Some((f, w)) = c.read_tag().map_err(|e| e.to_string())? {
        match (f, w) {
            (1, WIRE_LEN) => {
                name = std::str::from_utf8(c.read_len_delimited().map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?
                    .into();
            }
            (2, WIRE_LEN) => {
                features.push(c.read_len_delimited().map_err(|e| e.to_string())?.to_vec());
            }
            (3, WIRE_LEN) => keys.push(Arc::from(
                std::str::from_utf8(c.read_len_delimited().map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?,
            )),
            (4, WIRE_LEN) => values.push(decode_detail_attr(
                c.read_len_delimited().map_err(|e| e.to_string())?,
            )?),
            (5, WIRE_VARINT) => {
                extent = u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            }
            (15, WIRE_VARINT) => {
                c.read_varint().map_err(|e| e.to_string())?;
            }
            _ => return Err("unknown layer field".into()),
        }
    }
    Ok((name, extent, keys, values, features))
}
// Walk a feature message into (geom_type, paths) with rings/paths/points in
// WIRE order: MoveTo starts a path (each point its own path for points),
// LineTo extends the current path, ClosePath appends the ring's first point.
#[allow(clippy::type_complexity)]
fn wire_feature_paths(data: &[u8]) -> Result<(u8, Vec<Vec<(i32, i32)>>), String> {
    let mut c = Cursor::new(data);
    let (mut typ, mut geom) = (0u64, Vec::new());
    while let Some((f, w)) = c.read_tag().map_err(|e| e.to_string())? {
        match (f, w) {
            (1 | 3, WIRE_VARINT) => {
                let value = c.read_varint().map_err(|e| e.to_string())?;
                if f == 3 {
                    typ = value;
                }
            }
            (2 | 4, WIRE_LEN) => {
                let value = c.read_len_delimited().map_err(|e| e.to_string())?;
                if f == 4 {
                    geom.extend_from_slice(value);
                }
            }
            _ => return Err("unknown feature field".into()),
        }
    }
    let geom_type = u8::try_from(typ).map_err(|_| "geometry type out of range")?;
    let mut c = Cursor::new(&geom);
    let (mut x, mut y) = (0i32, 0i32);
    let mut paths: Vec<Vec<(i32, i32)>> = Vec::new();
    while !c.is_empty() {
        let command = u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
            .map_err(|_| "geometry command overflow")?;
        match command & 7 {
            1 => {
                for _ in 0..command >> 3 {
                    x = x
                        .checked_add(unzigzag(
                            u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                                .map_err(|_| "x overflow")?,
                        ))
                        .ok_or("x overflow")?;
                    y = y
                        .checked_add(unzigzag(
                            u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                                .map_err(|_| "y overflow")?,
                        ))
                        .ok_or("y overflow")?;
                    paths.push(vec![(x, y)]);
                }
            }
            2 => {
                let path = paths.last_mut().ok_or("LineTo without MoveTo")?;
                for _ in 0..command >> 3 {
                    x = x
                        .checked_add(unzigzag(
                            u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                                .map_err(|_| "x overflow")?,
                        ))
                        .ok_or("x overflow")?;
                    y = y
                        .checked_add(unzigzag(
                            u32::try_from(c.read_varint().map_err(|e| e.to_string())?)
                                .map_err(|_| "y overflow")?,
                        ))
                        .ok_or("y overflow")?;
                    path.push((x, y));
                }
            }
            7 => {
                if geom_type == 3
                    && let Some(r) = paths.last_mut()
                    && let Some(first) = r.first().copied()
                {
                    r.push(first);
                }
            }
            _ => return Err("unknown geometry command".into()),
        }
    }
    Ok((geom_type, paths))
}
#[allow(clippy::cast_possible_wrap)]
fn unzigzag(value: u32) -> i32 {
    ((value >> 1) as i32) ^ (-((value & 1) as i32))
}
pub fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
pub fn xml_attr(value: &str) -> String {
    xml_text(value)
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::too_many_lines, clippy::let_underscore_must_use)]
    use super::{classify_rings, xml_attr, xml_text};

    // Positive-winding square (impl shoelace sign): outer ring.
    fn pos(a: i32, b: i32) -> Vec<(i32, i32)> {
        vec![(a, b), (a + 1, b), (a + 1, b + 1), (a, b + 1), (a, b)]
    }
    // Reversed winding of `pos`: negative area, a hole under the first outer.
    fn neg(a: i32, b: i32) -> Vec<(i32, i32)> {
        vec![(a, b), (a, b + 1), (a + 1, b + 1), (a + 1, b), (a, b)]
    }

    #[test]
    fn classify_rings_keeps_single_zero_area_ring() {
        let rings = vec![vec![(0, 0), (0, 0), (0, 0)]];
        assert_eq!(classify_rings(&rings), (vec![vec![0]], 0));
    }

    #[test]
    fn classify_rings_skips_zero_area_and_attaches_hole() {
        let rings = vec![
            vec![(0, 0), (10, 0), (10, 10), (0, 0)],
            vec![(0, 0), (0, 0)],
            vec![(2, 2), (2, 8), (8, 8), (2, 2)],
        ];
        assert_eq!(classify_rings(&rings).0, vec![vec![0, 2]]);
    }

    #[test]
    fn classify_rings_two_outers_are_two_polygons() {
        let rings = vec![pos(0, 0), pos(100, 100)];
        assert_eq!(classify_rings(&rings), (vec![vec![0], vec![1]], 0));
    }

    #[test]
    fn classify_rings_calibrates_off_first_ring() {
        // First ring is negative-wound, so negative is "outer"; a following
        // positive ring is the opposite winding and joins as a hole.
        let rings = vec![neg(0, 0), pos(100, 100)];
        assert_eq!(classify_rings(&rings).0, vec![vec![0, 1]]);
    }

    #[test]
    fn classify_rings_clamps_at_500_with_ties() {
        let mut rings = vec![vec![(0, 0), (4096, 0), (4096, 4096), (0, 4096), (0, 0)]];
        for i in 0..500 {
            rings.push(neg(i * 2, 0));
        }
        let (groups, clamped) = classify_rings(&rings);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 500);
        assert_eq!(clamped, 1);
    }

    #[test]
    fn xml_escaping_neutralizes_metacharacters() {
        assert_eq!(xml_text("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        assert_eq!(xml_attr("\"x'&\""), "&quot;x&apos;&amp;&quot;");
        // A hostile layer/style value cannot break out of an attribute.
        assert!(!xml_attr("\"/><script>").contains('<'));
        assert!(!xml_attr("\"/><script>").contains('"'));
    }

    // Spec B section 5.6 determinism acceptance test. Renders a synthetic MVT
    // tile that touches every emission path - points, lines, a multi-ring
    // polygon carrying a zero-area ring, a >500-ring polygon that trips the
    // classifyRings clamp, and an unstyled layer that falls back to magenta -
    // twice, through two SEPARATE Style::load calls, and asserts the two SVG
    // byte streams are identical. The property under test is that no
    // hash-container iteration and no float formatting reaches an emitted byte:
    // any such regression would perturb these bytes between the two renders.
    #[test]
    fn synthetic_mvt_renders_byte_identically_across_loads() {
        use super::{decode_render_tile, render_svg};
        use crate::corpus::style::Style;
        use crate::mvt::{
            Feature, GeomType, LayerBuilder, encode_linestring, encode_point, encode_polygon,
            encode_tile,
        };
        use std::fs;
        use std::path::PathBuf;

        // Positive-wound (outer) and negative-wound (hole) closed rings, the
        // same winding convention the classify_rings unit tests above rely on.
        fn outer(x: i32, y: i32, s: i32) -> Vec<(i32, i32)> {
            vec![(x, y), (x + s, y), (x + s, y + s), (x, y + s), (x, y)]
        }
        fn hole(x: i32, y: i32, s: i32) -> Vec<(i32, i32)> {
            vec![(x, y), (x, y + s), (x + s, y + s), (x + s, y), (x, y)]
        }

        // Polygon layer: a multi-ring feature (outer + collinear zero-area ring
        // + hole) and a >500-ring feature that forces the maxRings clamp.
        let mut ocean = LayerBuilder::new("ocean");
        let multi: Vec<Vec<(i32, i32)>> = vec![
            outer(10, 10, 90),
            vec![(20, 20), (40, 20), (60, 20), (20, 20)], // collinear -> area 0
            hole(30, 30, 30),
        ];
        let multi_refs: Vec<&[(i32, i32)]> = multi.iter().map(Vec::as_slice).collect();
        let mut geom = Vec::new();
        encode_polygon(&mut geom, &multi_refs);
        ocean.add_feature(Feature {
            id: Some(1),
            geom_type: GeomType::Polygon,
            geometry: geom,
            tags: vec![],
        });
        // One outer plus 501 opposite-wound holes -> a single 502-ring polygon,
        // clamped to 500 (clamped count 2).
        let mut big: Vec<Vec<(i32, i32)>> = vec![outer(0, 0, 4000)];
        for i in 0..501 {
            big.push(hole(i * 2, 0, 1));
        }
        let big_refs: Vec<&[(i32, i32)]> = big.iter().map(Vec::as_slice).collect();
        let mut big_geom = Vec::new();
        encode_polygon(&mut big_geom, &big_refs);
        ocean.add_feature(Feature {
            id: Some(2),
            geom_type: GeomType::Polygon,
            geometry: big_geom,
            tags: vec![],
        });

        // Line layer, with a tag that exercises the style match table.
        let mut streets = LayerBuilder::new("streets");
        let k = streets.intern_key("kind");
        let v = streets.intern_string_value("motorway");
        let mut line_geom = Vec::new();
        encode_linestring(&mut line_geom, &[(0, 0), (500, 500), (1000, 200)]);
        streets.add_feature(Feature {
            id: Some(3),
            geom_type: GeomType::LineString,
            geometry: line_geom,
            tags: vec![(k, v)],
        });

        // Point layer.
        let mut labels = LayerBuilder::new("place_labels");
        let mut point_geom = Vec::new();
        encode_point(&mut point_geom, 2048, 2048);
        labels.add_feature(Feature {
            id: Some(4),
            geom_type: GeomType::Point,
            geometry: point_geom,
            tags: vec![],
        });

        // Unstyled layer: absent from the style, so it takes the magenta
        // fallback and raises an "unstyled layer" warning.
        let mut mystery = LayerBuilder::new("zzz_unstyled");
        let mut mystery_geom = Vec::new();
        encode_point(&mut mystery_geom, 100, 100);
        mystery.add_feature(Feature {
            id: Some(5),
            geom_type: GeomType::Point,
            geometry: mystery_geom,
            tags: vec![],
        });

        let tile = encode_tile(&[&ocean, &streets, &labels, &mystery]);

        // A minimal style written to the project-local target dir (never /tmp),
        // loaded twice so the two renders go through independent Style values.
        let dir =
            PathBuf::from("target").join(format!("corpus-determinism-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create target dir");
        let style_path = dir.join("style.toml");
        fs::write(
            &style_path,
            "background = \"#f2efe9\"\n\n\
             [[layer]]\nname = \"ocean\"\nfill = \"#aad3df\"\n\n\
             [[layer]]\nname = \"streets\"\nstroke = \"#dddddd\"\nstroke_width = \"1\"\n\
             [[layer.match]]\nkey = \"kind\"\nvalue = \"motorway\"\nstroke = \"#e892a2\"\nstroke_width = \"6\"\n\n\
             [[layer]]\nname = \"place_labels\"\nfill = \"#d63333\"\npoint_radius = 4\n",
        )
        .expect("write style");

        let style_a = Style::load(&style_path).expect("load style a");
        let style_b = Style::load(&style_path).expect("load style b");

        let tile_a = decode_render_tile(&tile).expect("decode a");
        let tile_b = decode_render_tile(&tile).expect("decode b");

        let a = render_svg(&tile_a, 5, 16, 9, &style_a, None).expect("render a");
        let b = render_svg(&tile_b, 5, 16, 9, &style_b, None).expect("render b");

        assert_eq!(
            a.bytes, b.bytes,
            "synthetic-MVT render must be byte-identical across separate Style::load calls"
        );

        // The exercised paths actually fired: the unstyled layer warned, and the
        // 502-ring polygon clamped by exactly 2.
        assert!(
            a.warnings.iter().any(|w| w.contains("zzz_unstyled")),
            "unstyled layer must warn: {:?}",
            a.warnings
        );
        let svg = std::str::from_utf8(&a.bytes).expect("utf8");
        assert!(
            svg.contains("data-clamped=\"2\""),
            "the >500-ring polygon must emit its clamp count"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
