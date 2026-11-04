//! MVT tile decoder producing structural detail for out-of-crate adjudication.
//!
//! elivagar is the only decoder of a PMTiles archive in this system; brokkr
//! links the crate and decodes tiles through this module. Decoding stops at
//! structure: a [`DetailTile`] carries its layers, features, geometry
//! components and rings in **wire order**, with no canonicalization and no
//! hashing. Polygon rings use the first nonzero-area ring's orientation as
//! the exterior orientation; zero-area rings are skipped. Deciding what
//! counts as "the same tile" - ordering, digests, the comparison - is the
//! caller's, per the corpus redesign contract. The one knob here is
//! [`Strictness`]: the gate decodes strict so foreign structure
//! can never silently skip past it; a cross-producer comparison decodes
//! tolerant.

use std::sync::Arc;

use crate::geometry;
use protohoggr::{Cursor, WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT};

/// MVT decoding policy.
///
/// `Strict` rejects fields the MVT schema does not name and malformed feature
/// structure - the gate's requirement, so foreign structure cannot pass
/// unnoticed. `Tolerant` skips unknown fields and preserves permissive
/// decoding for comparing archives from producers that carry structure
/// elivagar never emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strictness {
    Strict,
    Tolerant,
}

/// Ring role recovered from geometry: point and line features carry a single
/// synthetic ring; polygon rings use the first nonzero-area ring's winding as
/// the exterior orientation, and zero-area rings are skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonRingRole {
    Point = 0,
    Path = 1,
    Outer = 2,
    Hole = 3,
}

/// A decoded MVT attribute value.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DetailAttr {
    String(Arc<str>),
    Float(u32),
    Double(u64),
    Int(i64),
    UInt(u64),
    SInt(i64),
    Bool(bool),
}

/// One geometry ring: its role plus its absolute integer vertices.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DetailRing {
    pub role: CanonRingRole,
    pub points: Vec<(i32, i32)>,
}

/// One geometry component: an outer ring plus any following holes (polygons),
/// or a single ring (points, lines).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DetailComponent {
    pub rings: Vec<DetailRing>,
}

/// One MVT feature, geometry decoded into components, tags resolved against
/// the layer key/value tables. Attributes and components are in wire order.
#[derive(Clone, Debug)]
pub struct DetailFeature {
    pub id: Option<u64>,
    pub geom_type: u8,
    pub attrs: Vec<(Arc<str>, DetailAttr)>,
    pub components: Vec<DetailComponent>,
}

/// One MVT layer with its features in wire order.
#[derive(Clone, Debug)]
pub struct DetailLayer {
    pub name: Arc<str>,
    pub extent: u32,
    pub version: u32,
    pub features: Vec<DetailFeature>,
}

/// A decoded MVT tile: its layers in wire order.
#[derive(Clone, Debug)]
pub struct DetailTile {
    pub layers: Vec<DetailLayer>,
}

/// Decode a decompressed MVT tile into structural detail.
pub fn decode_detail_tile(data: &[u8], strictness: Strictness) -> Result<DetailTile, String> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read tile tag: {error}"))?
    {
        if field == 3 && wire_type == WIRE_LEN {
            let bytes = cursor
                .read_len_delimited()
                .map_err(|error| format!("read tile layer: {error}"))?;
            layers.push(decode_detail_layer(bytes, strictness)?);
        } else if strictness == Strictness::Strict {
            return Err(format!("unknown tile field {field}"));
        } else {
            cursor
                .skip_field(wire_type)
                .map_err(|error| format!("skip tile field {field}: {error}"))?;
        }
    }
    Ok(DetailTile { layers })
}

fn decode_detail_layer(data: &[u8], strictness: Strictness) -> Result<DetailLayer, String> {
    let mut name: Arc<str> = Arc::from("");
    let mut extent = 4096u32;
    let mut version = 1u32;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut feature_bytes = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read layer tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer name: {error}"))?;
                name = Arc::from(
                    std::str::from_utf8(bytes)
                        .map_err(|error| format!("layer name is not UTF-8: {error}"))?,
                );
            }
            (2, WIRE_LEN) => feature_bytes.push(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature message: {error}"))?,
            ),
            (3, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer key: {error}"))?;
                keys.push(Arc::from(
                    std::str::from_utf8(bytes)
                        .map_err(|error| format!("layer key is not UTF-8: {error}"))?,
                ));
            }
            (4, WIRE_LEN) => values.push(decode_detail_attr(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer value: {error}"))?,
                strictness,
            )?),
            (5, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read layer extent: {error}"))?;
                extent = u32::try_from(raw).map_err(|_| format!("extent out of range: {raw}"))?;
            }
            (15, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read layer version: {error}"))?;
                version = u32::try_from(raw).map_err(|_| format!("version out of range: {raw}"))?;
            }
            _ => {
                if strictness == Strictness::Strict {
                    return Err(format!("unknown layer field {field}"));
                }
                cursor
                    .skip_field(wire_type)
                    .map_err(|error| format!("skip layer field {field}: {error}"))?;
            }
        }
    }
    let mut features = Vec::with_capacity(feature_bytes.len());
    for bytes in feature_bytes {
        features.push(decode_detail_feature(bytes, &keys, &values, strictness)?);
    }
    Ok(DetailLayer {
        name,
        extent,
        version,
        features,
    })
}

/// Decode one MVT value message (the layer value table entry).
pub fn decode_detail_attr(data: &[u8], strictness: Strictness) -> Result<DetailAttr, String> {
    let mut out = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read value tag: {error}"))?
    {
        let value = match (field, wire_type) {
            (1, WIRE_LEN) => Some(DetailAttr::String(Arc::from(
                std::str::from_utf8(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read string value: {error}"))?,
                )
                .map_err(|error| format!("string value is not UTF-8: {error}"))?,
            ))),
            (2, WIRE_32BIT) => Some(DetailAttr::Float(
                cursor
                    .read_fixed32()
                    .map_err(|error| format!("read float value: {error}"))?,
            )),
            (3, WIRE_64BIT) => Some(DetailAttr::Double(
                cursor
                    .read_fixed64()
                    .map_err(|error| format!("read double value: {error}"))?,
            )),
            (4, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read int value: {error}"))?;
                #[allow(clippy::cast_possible_wrap)]
                Some(DetailAttr::Int(raw as i64))
            }
            (5, WIRE_VARINT) => Some(DetailAttr::UInt(
                cursor
                    .read_varint()
                    .map_err(|error| format!("read uint value: {error}"))?,
            )),
            (6, WIRE_VARINT) => Some(DetailAttr::SInt(unzigzag64(
                cursor
                    .read_varint()
                    .map_err(|error| format!("read sint value: {error}"))?,
            ))),
            (7, WIRE_VARINT) => Some(DetailAttr::Bool(
                cursor
                    .read_varint()
                    .map_err(|error| format!("read bool value: {error}"))?
                    != 0,
            )),
            _ => {
                if strictness == Strictness::Strict {
                    return Err(format!("unknown value field {field}"));
                }
                cursor
                    .skip_field(wire_type)
                    .map_err(|error| format!("skip value field {field}: {error}"))?;
                None
            }
        };
        if value.is_some() && strictness == Strictness::Strict && out.is_some() {
            return Err("MVT value has multiple value fields".to_string());
        }
        if value.is_some() {
            out = value;
        }
    }
    out.ok_or_else(|| "empty MVT value".to_string())
}

/// Decode one MVT feature message against the layer key/value tables.
pub fn decode_detail_feature(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
    strictness: Strictness,
) -> Result<DetailFeature, String> {
    let mut id = None;
    let mut seen_id = false;
    let mut tag_bytes = Vec::new();
    let mut geom_type = 0u8;
    let mut seen_geom_type = false;
    let mut geometry = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read feature tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_VARINT) => {
                if strictness == Strictness::Strict && seen_id {
                    return Err("duplicate feature id field".to_string());
                }
                seen_id = true;
                id = Some(
                    cursor
                        .read_varint()
                        .map_err(|error| format!("read feature id: {error}"))?,
                );
            }
            (2, WIRE_LEN) => {
                tag_bytes.extend_from_slice(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read feature tags: {error}"))?,
                );
            }
            (3, WIRE_VARINT) => {
                if strictness == Strictness::Strict && seen_geom_type {
                    return Err("duplicate feature type field".to_string());
                }
                seen_geom_type = true;
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read feature type: {error}"))?;
                geom_type =
                    u8::try_from(raw).map_err(|_| format!("geometry type out of range: {raw}"))?;
            }
            (4, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature geometry: {error}"))?;
                geometry
                    .get_or_insert_with(Vec::new)
                    .extend_from_slice(bytes);
            }
            _ => {
                if strictness == Strictness::Strict {
                    return Err(format!("unknown feature field {field}"));
                }
                cursor
                    .skip_field(wire_type)
                    .map_err(|error| format!("skip feature field {field}: {error}"))?;
            }
        }
    }
    let attrs = decode_detail_attrs(&tag_bytes, keys, values)?;
    let components = match geometry {
        Some(geometry) => decode_detail_geometry(geom_type, &geometry, strictness)?,
        None if strictness == Strictness::Strict => {
            return Err("feature has no geometry field".to_string());
        }
        None => Vec::new(),
    };
    Ok(DetailFeature {
        id,
        geom_type,
        attrs,
        components,
    })
}

fn decode_detail_attrs(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
) -> Result<Vec<(Arc<str>, DetailAttr)>, String> {
    let mut cursor = Cursor::new(data);
    let mut attrs = Vec::with_capacity(data.len() / 2);
    while !cursor.is_empty() {
        let key_idx = usize::try_from(
            cursor
                .read_varint()
                .map_err(|error| format!("read feature tag key: {error}"))?,
        )
        .map_err(|_| "feature tag key index overflow".to_string())?;
        let value_idx = usize::try_from(
            cursor
                .read_varint()
                .map_err(|error| format!("read feature tag value: {error}"))?,
        )
        .map_err(|_| "feature tag value index overflow".to_string())?;
        attrs.push((
            Arc::clone(
                keys.get(key_idx)
                    .ok_or_else(|| format!("key index out of range: {key_idx}"))?,
            ),
            values
                .get(value_idx)
                .ok_or_else(|| format!("value index out of range: {value_idx}"))?
                .clone(),
        ));
    }
    Ok(attrs)
}

fn decode_detail_geometry(
    geom_type: u8,
    data: &[u8],
    strictness: Strictness,
) -> Result<Vec<DetailComponent>, String> {
    if strictness == Strictness::Strict && !matches!(geom_type, 1..=3) {
        return Err(format!("unsupported geometry type: {geom_type}"));
    }
    if strictness == Strictness::Strict && data.is_empty() {
        return Err("feature has empty geometry command stream".to_string());
    }
    match geom_type {
        1 => decode_detail_points(data, strictness),
        2 => decode_detail_lines(data, strictness),
        3 => decode_detail_polygons(data, strictness),
        _ => Ok(Vec::new()),
    }
}

fn decode_detail_points(
    data: &[u8],
    strictness: Strictness,
) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut points = Vec::new();
    let mut move_to_seen = false;
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "point command")?;
        if strictness == Strictness::Strict && command >> 3 == 0 {
            return Err("geometry command with zero repeat count".to_string());
        }
        if command & 0x7 != 1 {
            return Err(format!("point geometry contains command {}", command & 0x7));
        }
        if strictness == Strictness::Strict && move_to_seen {
            return Err("point geometry must use a single MoveTo command".to_string());
        }
        move_to_seen = true;
        for _ in 0..(command >> 3) {
            x = x
                .checked_add(unzigzag(read_geometry_varint(&mut cursor, "point x")?))
                .ok_or_else(|| "point x overflows i32".to_string())?;
            y = y
                .checked_add(unzigzag(read_geometry_varint(&mut cursor, "point y")?))
                .ok_or_else(|| "point y overflows i32".to_string())?;
            points.push((x, y));
        }
    }
    if points.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![make_detail_component(vec![make_detail_ring(
            CanonRingRole::Point,
            points,
        )])])
    }
}

fn decode_detail_lines(
    data: &[u8],
    strictness: Strictness,
) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut components = Vec::new();
    let mut path: Option<Vec<(i32, i32)>> = None;
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "line command")?;
        let id = command & 0x7;
        let count = command >> 3;
        if strictness == Strictness::Strict && count == 0 {
            return Err("geometry command with zero repeat count".to_string());
        }
        match id {
            1 => {
                if strictness == Strictness::Strict && count != 1 {
                    return Err("linestring MoveTo count must be 1".to_string());
                }
                if let Some(path) = path.take() {
                    if strictness == Strictness::Strict && path.len() < 2 {
                        return Err("linestring has fewer than 2 points".to_string());
                    }
                    push_detail_line(&mut components, path);
                }
                for n in 0..count {
                    x = x
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "line MoveTo x",
                        )?))
                        .ok_or_else(|| "line x overflows i32".to_string())?;
                    y = y
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "line MoveTo y",
                        )?))
                        .ok_or_else(|| "line y overflows i32".to_string())?;
                    if n == 0 {
                        path = Some(vec![(x, y)]);
                    } else {
                        push_detail_line(&mut components, vec![(x, y)]);
                    }
                }
            }
            2 => {
                let path = path
                    .as_mut()
                    .ok_or_else(|| "line LineTo without MoveTo".to_string())?;
                if strictness == Strictness::Strict && path.len() > 1 {
                    return Err("linestring has multiple LineTo commands".to_string());
                }
                for _ in 0..count {
                    x = x
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "line LineTo x",
                        )?))
                        .ok_or_else(|| "line x overflows i32".to_string())?;
                    y = y
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "line LineTo y",
                        )?))
                        .ok_or_else(|| "line y overflows i32".to_string())?;
                    path.push((x, y));
                }
            }
            7 if strictness == Strictness::Strict => {
                return Err("ClosePath in non-polygon geometry".to_string());
            }
            7 => {}
            _ => return Err(format!("unknown line command {id}")),
        }
    }
    if let Some(path) = path {
        if strictness == Strictness::Strict && path.len() < 2 {
            return Err("linestring has fewer than 2 points".to_string());
        }
        push_detail_line(&mut components, path);
    }
    Ok(components)
}

fn push_detail_line(components: &mut Vec<DetailComponent>, path: Vec<(i32, i32)>) {
    if !path.is_empty() {
        components.push(make_detail_component(vec![make_detail_ring(
            CanonRingRole::Path,
            path,
        )]));
    }
}

#[allow(clippy::too_many_lines)]
fn decode_detail_polygons(
    data: &[u8],
    strictness: Strictness,
) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let mut ring_closed = false;
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "polygon command")?;
        let id = command & 0x7;
        let count = command >> 3;
        if strictness == Strictness::Strict && count == 0 {
            return Err("geometry command with zero repeat count".to_string());
        }
        match id {
            1 => {
                if strictness == Strictness::Strict && count != 1 {
                    return Err("polygon ring MoveTo count must be 1".to_string());
                }
                if strictness == Strictness::Strict && !rings.is_empty() && !ring_closed {
                    return Err("polygon ring missing ClosePath".to_string());
                }
                for _ in 0..count {
                    x = x
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "polygon MoveTo x",
                        )?))
                        .ok_or_else(|| "polygon x overflows i32".to_string())?;
                    y = y
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "polygon MoveTo y",
                        )?))
                        .ok_or_else(|| "polygon y overflows i32".to_string())?;
                    rings.push(vec![(x, y)]);
                    ring_closed = false;
                }
            }
            2 => {
                if strictness == Strictness::Strict && ring_closed {
                    return Err("polygon LineTo after ClosePath".to_string());
                }
                let ring = rings
                    .last_mut()
                    .ok_or_else(|| "polygon LineTo without MoveTo".to_string())?;
                if strictness == Strictness::Strict && ring.len() > 1 {
                    return Err("polygon ring has multiple LineTo commands".to_string());
                }
                if strictness == Strictness::Strict && count < 2 {
                    return Err("polygon ring LineTo count must be at least 2".to_string());
                }
                for _ in 0..count {
                    x = x
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "polygon LineTo x",
                        )?))
                        .ok_or_else(|| "polygon x overflows i32".to_string())?;
                    y = y
                        .checked_add(unzigzag(read_geometry_varint(
                            &mut cursor,
                            "polygon LineTo y",
                        )?))
                        .ok_or_else(|| "polygon y overflows i32".to_string())?;
                    ring.push((x, y));
                }
            }
            7 => {
                if strictness == Strictness::Strict && count != 1 {
                    return Err("ClosePath command count is not 1".to_string());
                }
                if strictness == Strictness::Strict && ring_closed {
                    return Err("polygon ring has repeated ClosePath".to_string());
                }
                if strictness == Strictness::Strict && rings.is_empty() {
                    return Err("polygon ClosePath without MoveTo".to_string());
                }
                if strictness == Strictness::Strict
                    && let Some(ring) = rings.last()
                    && ring.len() < 3
                {
                    return Err(format!(
                        "polygon ring has too few points ({}, need 3)",
                        ring.len()
                    ));
                }
                if let Some(ring) = rings.last_mut()
                    && let Some(&first) = ring.first()
                {
                    // ClosePath does not alter x/y. This is required by MVT 4.3.3.3.
                    ring.push(first);
                }
                if !rings.is_empty() {
                    ring_closed = true;
                }
            }
            _ => return Err(format!("unknown polygon command {id}")),
        }
    }

    if strictness == Strictness::Strict && rings.is_empty() {
        return Err("polygon geometry has no rings".to_string());
    }
    if strictness == Strictness::Strict && !ring_closed {
        return Err("polygon ring missing ClosePath".to_string());
    }

    // Match classifyRings: the first nonzero-area ring establishes exterior
    // winding, and zero-area rings do not participate in grouping.
    let mut grouped: Vec<Vec<DetailRing>> = Vec::new();
    let mut exterior_orientation = None;
    for ring in rings {
        let Some(orientation) = geometry::ring_role(signed_area(&ring)) else {
            continue;
        };
        let role = match exterior_orientation {
            Some(exterior) if orientation == exterior => CanonRingRole::Outer,
            Some(_) => CanonRingRole::Hole,
            None => {
                exterior_orientation = Some(orientation);
                CanonRingRole::Outer
            }
        };
        let ring = make_detail_ring(role, ring);
        if role == CanonRingRole::Outer {
            grouped.push(vec![ring]);
        } else if let Some(component) = grouped.last_mut() {
            component.push(ring);
        }
    }
    Ok(grouped.into_iter().map(make_detail_component).collect())
}

fn read_geometry_varint(cursor: &mut Cursor<'_>, context: &str) -> Result<u32, String> {
    let raw = cursor
        .read_varint()
        .map_err(|error| format!("read {context}: {error}"))?;
    u32::try_from(raw).map_err(|_| format!("{context} out of range: {raw}"))
}

fn make_detail_ring(role: CanonRingRole, points: Vec<(i32, i32)>) -> DetailRing {
    DetailRing { role, points }
}

fn make_detail_component(rings: Vec<DetailRing>) -> DetailComponent {
    DetailComponent { rings }
}

#[inline]
fn unzigzag(value: u32) -> i32 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((value >> 1) as i32) ^ (-((value & 1) as i32))
    }
}

#[inline]
fn unzigzag64(value: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((value >> 1) as i64) ^ (-((value & 1) as i64))
    }
}

fn signed_area(ring: &[(i32, i32)]) -> i128 {
    ring.windows(2).fold(0i128, |area, pair| {
        area + i128::from(pair[0].0) * i128::from(pair[1].1)
            - i128::from(pair[1].0) * i128::from(pair[0].1)
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[allow(clippy::cast_possible_truncation)]
    fn push_varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn push_tag(out: &mut Vec<u8>, field: u32, wire_type: u32) {
        push_varint(out, (u64::from(field) << 3) | u64::from(wire_type));
    }

    fn push_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
        push_tag(out, field, WIRE_VARINT);
        push_varint(out, value);
    }

    #[allow(clippy::cast_possible_truncation)]
    fn push_bytes_field(out: &mut Vec<u8>, field: u32, value: &[u8]) {
        push_tag(out, field, WIRE_LEN);
        push_varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }

    fn packed_geometry(commands: &[u32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for &command in commands {
            push_varint(&mut bytes, u64::from(command));
        }
        bytes
    }

    fn feature_message(geom_type: Option<u64>, geometry: Option<&[u32]>) -> Vec<u8> {
        let mut feature = Vec::new();
        if let Some(geom_type) = geom_type {
            push_varint_field(&mut feature, 3, geom_type);
        }
        if let Some(commands) = geometry {
            push_bytes_field(&mut feature, 4, &packed_geometry(commands));
        }
        feature
    }

    fn tile_message(feature: &[u8], key: &str, value: &[u8]) -> Vec<u8> {
        let mut layer = Vec::new();
        push_bytes_field(&mut layer, 1, b"test");
        push_bytes_field(&mut layer, 2, feature);
        push_bytes_field(&mut layer, 3, key.as_bytes());
        push_bytes_field(&mut layer, 4, value);
        push_varint_field(&mut layer, 5, 4096);
        push_varint_field(&mut layer, 15, 2);

        let mut tile = Vec::new();
        push_bytes_field(&mut tile, 3, &layer);
        tile
    }

    #[allow(clippy::cast_sign_loss)]
    fn zigzag(value: i32) -> u32 {
        ((value << 1) ^ (value >> 31)) as u32
    }

    #[allow(clippy::cast_possible_truncation)]
    fn polygon_commands(rings: &[&[(i32, i32)]]) -> Vec<u32> {
        let mut commands = Vec::new();
        let (mut x, mut y) = (0i32, 0i32);
        for ring in rings {
            let ring = *ring;
            let open_ring = if ring.len() > 1 && ring.first() == ring.last() {
                &ring[..ring.len() - 1]
            } else {
                ring
            };
            let Some(&(first_x, first_y)) = open_ring.first() else {
                continue;
            };
            commands.extend([9, zigzag(first_x - x), zigzag(first_y - y)]);
            x = first_x;
            y = first_y;
            if open_ring.len() > 1 {
                commands.push(((open_ring.len() as u32 - 1) << 3) | 2);
                for &(next_x, next_y) in &open_ring[1..] {
                    commands.push(zigzag(next_x - x));
                    commands.push(zigzag(next_y - y));
                    x = next_x;
                    y = next_y;
                }
            }
            commands.push(15);
        }
        commands
    }

    #[test]
    fn strict_decodes_valid_tile_and_rejects_unknown_fields_that_tolerant_skips() {
        let mut feature = feature_message(Some(1), Some(&[9, 14, 4]));
        push_bytes_field(&mut feature, 2, &[0, 0]);
        let value = {
            let mut value = Vec::new();
            push_bytes_field(&mut value, 1, b"road");
            value
        };
        let tile = tile_message(&feature, "class", &value);

        let decoded = decode_detail_tile(&tile, Strictness::Strict).unwrap();
        assert_eq!(decoded.layers.len(), 1);
        assert_eq!(decoded.layers[0].name.as_ref(), "test");
        assert_eq!(decoded.layers[0].extent, 4096);
        assert_eq!(decoded.layers[0].version, 2);
        assert_eq!(
            decoded.layers[0].features[0].components[0].rings[0]
                .points
                .as_slice(),
            &[(7, 2)][..]
        );
        let attrs = &decoded.layers[0].features[0].attrs;
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].0.as_ref(), "class");
        assert_eq!(attrs[0].1, DetailAttr::String(Arc::<str>::from("road")));

        let mut tile_with_unknown = tile;
        push_varint_field(&mut tile_with_unknown, 9, 1);
        assert!(
            decode_detail_tile(&tile_with_unknown, Strictness::Strict)
                .unwrap_err()
                .contains("unknown tile field")
        );
        assert!(decode_detail_tile(&tile_with_unknown, Strictness::Tolerant).is_ok());
    }

    #[test]
    fn strict_rejects_ambiguous_singular_feature_fields() {
        let mut feature = Vec::new();
        push_varint_field(&mut feature, 1, 3);
        push_varint_field(&mut feature, 1, 4);
        push_varint_field(&mut feature, 3, 1);
        push_varint_field(&mut feature, 3, 2);
        push_bytes_field(&mut feature, 4, &packed_geometry(&[9, 0, 0, 10, 2, 2]));

        assert!(
            decode_detail_feature(&feature, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("duplicate feature id")
        );
        let tolerant = decode_detail_feature(&feature, &[], &[], Strictness::Tolerant).unwrap();
        assert_eq!(tolerant.id, Some(4));
        assert_eq!(tolerant.geom_type, 2);

        let mut duplicate_type = Vec::new();
        push_varint_field(&mut duplicate_type, 3, 1);
        push_varint_field(&mut duplicate_type, 3, 1);
        push_bytes_field(&mut duplicate_type, 4, &packed_geometry(&[9, 0, 0]));
        assert!(
            decode_detail_feature(&duplicate_type, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("duplicate feature type")
        );
    }

    #[test]
    fn strict_rejects_missing_or_unknown_geometry_while_tolerant_preserves_it() {
        let missing = feature_message(Some(1), None);
        assert!(
            decode_detail_feature(&missing, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("no geometry field")
        );
        assert!(decode_detail_feature(&missing, &[], &[], Strictness::Tolerant).is_ok());

        let unknown_type = feature_message(Some(4), Some(&[9, 0, 0]));
        assert!(
            decode_detail_feature(&unknown_type, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("unsupported geometry type")
        );
        assert!(
            decode_detail_feature(&unknown_type, &[], &[], Strictness::Tolerant)
                .unwrap()
                .components
                .is_empty()
        );

        let empty_geometry = feature_message(Some(1), Some(&[]));
        assert!(
            decode_detail_feature(&empty_geometry, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("empty geometry command stream")
        );
    }

    #[test]
    fn strict_geometry_checks_counts_and_line_structure() {
        let zero_point_count = feature_message(Some(1), Some(&[1]));
        assert!(
            decode_detail_feature(&zero_point_count, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("zero repeat count")
        );
        assert!(decode_detail_feature(&zero_point_count, &[], &[], Strictness::Tolerant).is_ok());

        let split_point_moves = feature_message(Some(1), Some(&[9, 0, 0, 9, 2, 2]));
        assert!(
            decode_detail_feature(&split_point_moves, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("single MoveTo")
        );

        let one_point_line = feature_message(Some(2), Some(&[9, 0, 0]));
        assert!(
            decode_detail_feature(&one_point_line, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("fewer than 2 points")
        );

        let two_point_line = feature_message(Some(2), Some(&[9, 0, 0, 10, 2, 2]));
        assert!(decode_detail_feature(&two_point_line, &[], &[], Strictness::Strict).is_ok());

        let split_line_to = feature_message(Some(2), Some(&[9, 0, 0, 10, 2, 2, 10, 2, 2]));
        assert!(
            decode_detail_feature(&split_line_to, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("multiple LineTo")
        );

        let closepath_line = feature_message(Some(2), Some(&[9, 0, 0, 10, 2, 2, 15]));
        assert!(
            decode_detail_feature(&closepath_line, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("ClosePath in non-polygon")
        );
        let tolerant_line =
            decode_detail_feature(&closepath_line, &[], &[], Strictness::Tolerant).unwrap();
        assert_eq!(
            tolerant_line.components[0].rings[0].points.as_slice(),
            &[(0, 0), (1, 1)][..]
        );

        let unclosed_polygon = feature_message(Some(3), Some(&[9, 0, 0, 18, 20, 0, 0, 20]));
        assert!(
            decode_detail_feature(&unclosed_polygon, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("missing ClosePath")
        );
        assert!(decode_detail_feature(&unclosed_polygon, &[], &[], Strictness::Tolerant).is_ok());

        let short_polygon_line_to = feature_message(Some(3), Some(&[9, 0, 0, 10, 20, 0, 15]));
        assert!(
            decode_detail_feature(&short_polygon_line_to, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("LineTo count must be at least 2")
        );
    }

    #[test]
    fn closepath_keeps_the_cursor_at_the_last_linetovertex() {
        let commands = [
            9, 0, 0, 26, 20, 0, 0, 20, 19, 0, 15, 9, 4, 15, 26, 0, 12, 12, 0, 0, 11, 15,
        ];
        let feature = feature_message(Some(3), Some(&commands));
        let decoded = decode_detail_feature(&feature, &[], &[], Strictness::Strict).unwrap();
        let rings = &decoded.components[0].rings;
        assert_eq!(rings.len(), 2);
        assert_eq!(
            rings[0].points.as_slice(),
            &[(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)][..]
        );
        assert_eq!(
            rings[1].points.as_slice(),
            &[(2, 2), (2, 8), (8, 8), (8, 2), (2, 2)][..]
        );
    }

    #[test]
    fn polygon_rings_are_grouped_by_winding_in_wire_order() {
        let outer = [(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)];
        let hole = [(2, 2), (2, 8), (8, 8), (8, 2), (2, 2)];
        let second_outer = [(20, 0), (30, 0), (30, 10), (20, 10), (20, 0)];
        let commands = polygon_commands(&[&outer, &hole, &second_outer]);
        let feature = feature_message(Some(3), Some(&commands));
        let decoded = decode_detail_feature(&feature, &[], &[], Strictness::Strict).unwrap();

        assert_eq!(decoded.components.len(), 2);
        assert_eq!(decoded.components[0].rings.len(), 2);
        assert_eq!(decoded.components[0].rings[0].role, CanonRingRole::Outer);
        assert_eq!(decoded.components[0].rings[1].role, CanonRingRole::Hole);
        assert_eq!(decoded.components[1].rings.len(), 1);
        assert_eq!(decoded.components[1].rings[0].role, CanonRingRole::Outer);
    }

    #[test]
    fn polygon_grouping_self_calibrates_and_skips_zero_area_rings() {
        let zero_area = [(0, 0), (10, 0), (20, 0), (0, 0)];
        let negative_first = [(0, 0), (0, 10), (10, 10), (10, 0), (0, 0)];
        let same_winding_outer = [(20, 0), (20, 10), (30, 10), (30, 0), (20, 0)];
        let opposite_winding_hole = [(2, 2), (8, 2), (8, 8), (2, 8), (2, 2)];
        let commands = polygon_commands(&[
            &zero_area,
            &negative_first,
            &same_winding_outer,
            &opposite_winding_hole,
        ]);
        let feature = feature_message(Some(3), Some(&commands));
        let decoded = decode_detail_feature(&feature, &[], &[], Strictness::Strict).unwrap();

        assert_eq!(decoded.components.len(), 2);
        assert_eq!(decoded.components[0].rings.len(), 1);
        assert_eq!(decoded.components[0].rings[0].role, CanonRingRole::Outer);
        assert_eq!(decoded.components[1].rings.len(), 2);
        assert_eq!(decoded.components[1].rings[0].role, CanonRingRole::Outer);
        assert_eq!(decoded.components[1].rings[1].role, CanonRingRole::Hole);
    }

    fn value_with_varint_field(field: u32, raw: u64) -> Vec<u8> {
        let mut value = Vec::new();
        push_varint_field(&mut value, field, raw);
        value
    }

    fn value_with_fixed32_field(field: u32, raw: u32) -> Vec<u8> {
        let mut value = Vec::new();
        push_tag(&mut value, field, WIRE_32BIT);
        value.extend_from_slice(&raw.to_le_bytes());
        value
    }

    fn value_with_fixed64_field(field: u32, raw: u64) -> Vec<u8> {
        let mut value = Vec::new();
        push_tag(&mut value, field, WIRE_64BIT);
        value.extend_from_slice(&raw.to_le_bytes());
        value
    }

    #[test]
    fn value_decoding_preserves_each_mvt_scalar_type() {
        let cases = [
            (
                {
                    let mut value = Vec::new();
                    push_bytes_field(&mut value, 1, b"text");
                    value
                },
                DetailAttr::String(Arc::<str>::from("text")),
            ),
            (
                value_with_fixed32_field(2, 1.25f32.to_bits()),
                DetailAttr::Float(1.25f32.to_bits()),
            ),
            (
                value_with_fixed64_field(3, (-3.5f64).to_bits()),
                DetailAttr::Double((-3.5f64).to_bits()),
            ),
            (
                value_with_varint_field(4, u64::MAX - 16),
                DetailAttr::Int(-17),
            ),
            (
                value_with_varint_field(5, u64::MAX),
                DetailAttr::UInt(u64::MAX),
            ),
            (value_with_varint_field(6, 17), DetailAttr::SInt(-9)),
            (value_with_varint_field(7, 2), DetailAttr::Bool(true)),
        ];

        for (bytes, expected) in cases {
            assert_eq!(
                decode_detail_attr(&bytes, Strictness::Strict).unwrap(),
                expected
            );
            assert_eq!(
                decode_detail_attr(&bytes, Strictness::Tolerant).unwrap(),
                expected
            );
        }
        assert!(
            decode_detail_attr(&[], Strictness::Strict)
                .unwrap_err()
                .contains("empty MVT value")
        );

        let mut unknown_then_string = Vec::new();
        push_varint_field(&mut unknown_then_string, 8, 1);
        push_bytes_field(&mut unknown_then_string, 1, b"text");
        assert!(decode_detail_attr(&unknown_then_string, Strictness::Strict).is_err());
        assert_eq!(
            decode_detail_attr(&unknown_then_string, Strictness::Tolerant).unwrap(),
            DetailAttr::String(Arc::<str>::from("text"))
        );

        let mut multiple_fields = value_with_varint_field(4, 1);
        push_varint_field(&mut multiple_fields, 5, 2);
        assert!(decode_detail_attr(&multiple_fields, Strictness::Strict).is_err());
        assert_eq!(
            decode_detail_attr(&multiple_fields, Strictness::Tolerant).unwrap(),
            DetailAttr::UInt(2)
        );
    }
}
