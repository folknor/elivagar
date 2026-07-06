use protohoggr::{Cursor, WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT};

use crate::regress::{
    AttrVal, CanonComponent, CanonFeature, CanonLayer, CanonRing, CanonRingRole, CanonTile,
    sort_components,
};

pub fn decode_canonical(tile_data: &[u8]) -> Result<CanonTile, String> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(tile_data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|e| format!("read tile tag: {e}"))?
    {
        if field == 3 && wire_type == WIRE_LEN {
            let layer_data = cursor
                .read_len_delimited()
                .map_err(|e| format!("read tile layer: {e}"))?;
            layers.push(decode_canonical_layer(layer_data)?);
        } else {
            cursor
                .skip_field(wire_type)
                .map_err(|e| format!("skip tile field {field}: {e}"))?;
        }
    }
    layers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(CanonTile { layers })
}

#[derive(Clone)]
struct RawFeature {
    id: Option<u64>,
    tags: Vec<u32>,
    geom_type: u8,
    geometry: Vec<u32>,
}

fn decode_canonical_layer(data: &[u8]) -> Result<CanonLayer, String> {
    let mut name = String::new();
    let mut extent = 4096u32;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut raw_features = Vec::new();

    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|e| format!("read layer tag: {e}"))?
    {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read layer name: {e}"))?;
                name = String::from_utf8(bytes.to_vec())
                    .map_err(|e| format!("layer name is not UTF-8: {e}"))?;
            }
            (2, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read feature message: {e}"))?;
                raw_features.push(decode_raw_feature(bytes)?);
            }
            (3, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read layer key: {e}"))?;
                keys.push(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|e| format!("layer key is not UTF-8: {e}"))?,
                );
            }
            (4, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read layer value: {e}"))?;
                values.push(decode_attr_value(bytes)?);
            }
            (5, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|e| format!("read layer extent: {e}"))?;
                extent = u32::try_from(raw).map_err(|_| format!("extent out of range: {raw}"))?;
            }
            _ => {
                cursor
                    .skip_field(wire_type)
                    .map_err(|e| format!("skip layer field {field}: {e}"))?;
            }
        }
    }

    let mut features = Vec::with_capacity(raw_features.len());
    for raw in &raw_features {
        features.push(resolve_feature(raw, &keys, &values)?);
    }
    features.sort();

    Ok(CanonLayer {
        name,
        extent,
        features,
    })
}

fn decode_raw_feature(data: &[u8]) -> Result<RawFeature, String> {
    let mut id = None;
    let mut tags = Vec::new();
    let mut geom_type = 0u8;
    let mut geometry = Vec::new();

    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|e| format!("read feature tag: {e}"))?
    {
        match (field, wire_type) {
            (1, WIRE_VARINT) => {
                id = Some(
                    cursor
                        .read_varint()
                        .map_err(|e| format!("read feature id: {e}"))?,
                );
            }
            (2, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read feature tags: {e}"))?;
                tags = decode_packed_u32(bytes, "feature tags")?;
            }
            (3, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|e| format!("read feature type: {e}"))?;
                geom_type =
                    u8::try_from(raw).map_err(|_| format!("geometry type out of range: {raw}"))?;
            }
            (4, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read feature geometry: {e}"))?;
                geometry = decode_packed_u32(bytes, "feature geometry")?;
            }
            _ => {
                cursor
                    .skip_field(wire_type)
                    .map_err(|e| format!("skip feature field {field}: {e}"))?;
            }
        }
    }

    Ok(RawFeature {
        id,
        tags,
        geom_type,
        geometry,
    })
}

fn decode_attr_value(data: &[u8]) -> Result<AttrVal, String> {
    let mut cursor = Cursor::new(data);
    let mut out = None;
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|e| format!("read value tag: {e}"))?
    {
        let value = match (field, wire_type) {
            (1, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|e| format!("read string value: {e}"))?;
                Some(AttrVal::String(
                    String::from_utf8(bytes.to_vec())
                        .map_err(|e| format!("string value is not UTF-8: {e}"))?,
                ))
            }
            (2, WIRE_32BIT) => Some(AttrVal::Float(
                cursor
                    .read_fixed32()
                    .map_err(|e| format!("read float value: {e}"))?,
            )),
            (3, WIRE_64BIT) => Some(AttrVal::Double(
                cursor
                    .read_fixed64()
                    .map_err(|e| format!("read double value: {e}"))?,
            )),
            (4, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|e| format!("read int value: {e}"))?;
                #[allow(clippy::cast_possible_wrap)]
                Some(AttrVal::Int(raw as i64))
            }
            (5, WIRE_VARINT) => Some(AttrVal::UInt(
                cursor
                    .read_varint()
                    .map_err(|e| format!("read uint value: {e}"))?,
            )),
            (6, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|e| format!("read sint value: {e}"))?;
                Some(AttrVal::SInt(unzigzag64(raw)))
            }
            (7, WIRE_VARINT) => Some(AttrVal::Bool(
                cursor
                    .read_varint()
                    .map_err(|e| format!("read bool value: {e}"))?
                    != 0,
            )),
            _ => {
                cursor
                    .skip_field(wire_type)
                    .map_err(|e| format!("skip value field {field}: {e}"))?;
                None
            }
        };
        if let Some(value) = value {
            out = Some(value);
        }
    }
    out.ok_or_else(|| "empty MVT value".to_string())
}

fn resolve_feature(
    raw: &RawFeature,
    keys: &[String],
    values: &[AttrVal],
) -> Result<CanonFeature, String> {
    if !raw.tags.len().is_multiple_of(2) {
        return Err("feature tags have odd key/value count".to_string());
    }
    let mut attrs = Vec::with_capacity(raw.tags.len() / 2);
    let mut idx = 0usize;
    while idx < raw.tags.len() {
        let key_idx =
            usize::try_from(raw.tags[idx]).map_err(|_| "key index overflow".to_string())?;
        let val_idx =
            usize::try_from(raw.tags[idx + 1]).map_err(|_| "value index overflow".to_string())?;
        let key = keys
            .get(key_idx)
            .ok_or_else(|| format!("key index out of range: {key_idx}"))?
            .clone();
        let value = values
            .get(val_idx)
            .ok_or_else(|| format!("value index out of range: {val_idx}"))?
            .clone();
        attrs.push((key, value));
        idx += 2;
    }
    attrs.sort();

    let mut components = decode_components(raw.geom_type, &raw.geometry)?;
    sort_components(&mut components);

    Ok(CanonFeature {
        id: raw.id,
        geom_type: raw.geom_type,
        attrs,
        components,
    })
}

fn decode_components(geom_type: u8, commands: &[u32]) -> Result<Vec<CanonComponent>, String> {
    match geom_type {
        1 => decode_points(commands),
        2 => decode_lines(commands),
        3 => Ok(decode_polygon_components(commands)),
        _ => Ok(Vec::new()),
    }
}

fn decode_points(commands: &[u32]) -> Result<Vec<CanonComponent>, String> {
    let mut points = Vec::new();
    let mut cx = 0i32;
    let mut cy = 0i32;
    let mut i = 0usize;
    while i < commands.len() {
        let cmd = commands[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;
        if cmd_id != 1 {
            return Err(format!("point geometry contains command {cmd_id}"));
        }
        for _ in 0..cmd_count {
            if i + 1 >= commands.len() {
                return Err("truncated point geometry".to_string());
            }
            cx += unzigzag(commands[i]);
            cy += unzigzag(commands[i + 1]);
            i += 2;
            points.push((cx, cy));
        }
    }
    if points.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![CanonComponent {
        rings: vec![CanonRing {
            role: CanonRingRole::Point,
            points,
        }],
    }])
}

fn decode_lines(commands: &[u32]) -> Result<Vec<CanonComponent>, String> {
    let mut components = Vec::new();
    let mut current: Option<Vec<(i32, i32)>> = None;
    let mut cx = 0i32;
    let mut cy = 0i32;
    let mut i = 0usize;
    while i < commands.len() {
        let cmd = commands[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;
        match cmd_id {
            1 => {
                if let Some(path) = current.take() {
                    push_line_component(&mut components, path);
                }
                for n in 0..cmd_count {
                    if i + 1 >= commands.len() {
                        return Err("truncated line MoveTo".to_string());
                    }
                    cx += unzigzag(commands[i]);
                    cy += unzigzag(commands[i + 1]);
                    i += 2;
                    if n == 0 {
                        current = Some(vec![(cx, cy)]);
                    } else {
                        push_line_component(&mut components, vec![(cx, cy)]);
                    }
                }
            }
            2 => {
                let Some(path) = current.as_mut() else {
                    return Err("line LineTo without MoveTo".to_string());
                };
                for _ in 0..cmd_count {
                    if i + 1 >= commands.len() {
                        return Err("truncated line LineTo".to_string());
                    }
                    cx += unzigzag(commands[i]);
                    cy += unzigzag(commands[i + 1]);
                    i += 2;
                    path.push((cx, cy));
                }
            }
            7 => {}
            _ => return Err(format!("unknown line command {cmd_id}")),
        }
    }
    if let Some(path) = current {
        push_line_component(&mut components, path);
    }
    Ok(components)
}

fn push_line_component(components: &mut Vec<CanonComponent>, path: Vec<(i32, i32)>) {
    if path.is_empty() {
        return;
    }
    components.push(CanonComponent {
        rings: vec![CanonRing {
            role: CanonRingRole::Path,
            points: path,
        }],
    });
}

fn decode_polygon_components(commands: &[u32]) -> Vec<CanonComponent> {
    let rings = decode_mvt_polygon(commands);
    let mut components = Vec::new();
    for ring in rings {
        let role = if signed_area(&ring) > 0 {
            CanonRingRole::Outer
        } else {
            CanonRingRole::Hole
        };
        if role == CanonRingRole::Outer || components.is_empty() {
            components.push(CanonComponent {
                rings: vec![CanonRing { role, points: ring }],
            });
        } else if let Some(component) = components.last_mut() {
            component.rings.push(CanonRing { role, points: ring });
        }
    }
    components
}

fn decode_packed_u32(data: &[u8], ctx: &str) -> Result<Vec<u32>, String> {
    let mut out = Vec::new();
    let mut cursor = Cursor::new(data);
    while !cursor.is_empty() {
        let raw = cursor
            .read_varint()
            .map_err(|e| format!("read {ctx}: {e}"))?;
        out.push(u32::try_from(raw).map_err(|_| format!("{ctx} value out of range: {raw}"))?);
    }
    Ok(out)
}

fn signed_area(ring: &[(i32, i32)]) -> i128 {
    if ring.len() < 2 {
        return 0;
    }
    let mut area = 0i128;
    for pair in ring.windows(2) {
        area += i128::from(pair[0].0) * i128::from(pair[1].1);
        area -= i128::from(pair[1].0) * i128::from(pair[0].1);
    }
    area
}

#[inline]
fn unzigzag64(n: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((n >> 1) as i64) ^ (-((n & 1) as i64))
    }
}

/// Decode MVT polygon geometry commands back to closed rings in tile coordinates.
///
/// Inverse of `mvt::encode_polygon`. Walks MoveTo/LineTo/ClosePath commands,
/// undoes delta encoding, and returns one `Vec<(i32, i32)>` per ring (closed:
/// first == last vertex).
///
/// Designed for trusted in-pipeline data (output of our own `encode_polygon`).
/// On truncated input, may return partially decoded rings. Unknown command IDs
/// are silently skipped without consuming parameters (safe for well-formed
/// streams; may desync on malformed data).
pub fn decode_mvt_polygon(commands: &[u32]) -> Vec<Vec<(i32, i32)>> {
    let mut rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    let mut i = 0;

    while i < commands.len() {
        let cmd = commands[i];
        let cmd_id = cmd & 0x7;
        let cmd_count = cmd >> 3;
        i += 1;

        match cmd_id {
            1 => {
                // MoveTo - start a new ring. Only 1 MoveTo per ring in polygons.
                for _ in 0..cmd_count {
                    if i + 1 >= commands.len() {
                        return rings;
                    }
                    let dx = unzigzag(commands[i]);
                    let dy = unzigzag(commands[i + 1]);
                    i += 2;
                    cx += dx;
                    cy += dy;
                    rings.push(vec![(cx, cy)]);
                }
            }
            2 => {
                // LineTo - append vertices to the current ring.
                let Some(ring) = rings.last_mut() else {
                    // LineTo without a preceding MoveTo - skip.
                    i += (cmd_count as usize) * 2;
                    continue;
                };
                for _ in 0..cmd_count {
                    if i + 1 >= commands.len() {
                        return rings;
                    }
                    let dx = unzigzag(commands[i]);
                    let dy = unzigzag(commands[i + 1]);
                    i += 2;
                    cx += dx;
                    cy += dy;
                    ring.push((cx, cy));
                }
            }
            7 => {
                // ClosePath - close the current ring. Per MVT spec 4.3.3.3
                // the cursor is NOT changed (stays at the last LineTo vertex).
                if let Some(ring) = rings.last_mut()
                    && let Some(&first) = ring.first()
                {
                    ring.push(first);
                }
            }
            _ => {
                // Unknown command - skip.
            }
        }
    }
    rings
}

/// Encode closed rings back to MVT polygon geometry commands.
///
/// Inverse of `decode_mvt_polygon`. Each ring must be closed (first == last).
/// Uses `mvt::command()` and `mvt::zigzag()` for encoding.
#[allow(dead_code)]
pub fn encode_mvt_polygon(rings: &[Vec<(i32, i32)>], buf: &mut Vec<u32>) {
    buf.clear();
    let mut cx: i32 = 0;
    let mut cy: i32 = 0;
    for ring in rings {
        if ring.len() < 4 {
            // Degenerate ring (< 3 unique vertices + closing vertex)
            continue;
        }
        // MoveTo first point
        buf.push(crate::mvt::command(1, 1));
        buf.push(crate::mvt::zigzag(ring[0].0 - cx));
        buf.push(crate::mvt::zigzag(ring[0].1 - cy));
        cx = ring[0].0;
        cy = ring[0].1;
        // LineTo remaining points (skip last which is closing duplicate)
        #[allow(clippy::cast_possible_truncation)]
        let line_count = (ring.len() - 2) as u32;
        if line_count > 0 {
            buf.push(crate::mvt::command(2, line_count));
            for &(x, y) in &ring[1..ring.len() - 1] {
                buf.push(crate::mvt::zigzag(x - cx));
                buf.push(crate::mvt::zigzag(y - cy));
                cx = x;
                cy = y;
            }
        }
        // ClosePath - cursor unchanged per MVT spec 4.3.3.3 (cx/cy keep
        // the last LineTo vertex).
        buf.push(crate::mvt::command(7, 1));
    }
}

/// Zigzag-decode a u32 back to i32.
#[inline]
fn unzigzag(n: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((n >> 1) as i32) ^ (-((n & 1) as i32))
    }
}
