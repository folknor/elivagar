//! MVT tile decoder producing structural detail for out-of-crate adjudication.
//!
//! elivagar is the only decoder of a PMTiles archive in this system; brokkr
//! links the crate and decodes tiles through this module. Decoding stops at
//! structure: a [`DetailTile`] carries its layers, features, geometry
//! components and rings in **wire order**, with no canonicalization and no
//! hashing. Deciding what counts as "the same tile" - ordering, digests, the
//! comparison - is the caller's, per the corpus redesign contract. The one
//! knob here is [`Strictness`]: the gate decodes strict so foreign structure
//! can never silently skip past it; a cross-producer comparison decodes
//! tolerant.

use std::sync::Arc;

use protohoggr::{Cursor, WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT};

/// Unknown-wire-field policy.
///
/// `Strict` rejects any field the MVT schema does not name - the gate's
/// requirement, so a competitor's extra wire fields cannot pass unnoticed.
/// `Tolerant` skips unknown fields, for comparing archives from producers
/// that carry structure elivagar never emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strictness {
    Strict,
    Tolerant,
}

/// Ring role recovered from geometry: point and line features carry a single
/// synthetic ring; polygon rings are classified outer/hole by signed area.
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
    let mut tag_bytes = Vec::new();
    let mut geom_type = 0u8;
    let mut geometry = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read feature tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_VARINT) => {
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
        Some(geometry) => decode_detail_geometry(geom_type, &geometry)?,
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

fn decode_detail_geometry(geom_type: u8, data: &[u8]) -> Result<Vec<DetailComponent>, String> {
    match geom_type {
        1 => decode_detail_points(data),
        2 => decode_detail_lines(data),
        3 => decode_detail_polygons(data),
        _ => Ok(Vec::new()),
    }
}

fn decode_detail_points(data: &[u8]) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut points = Vec::new();
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "point command")?;
        if command & 0x7 != 1 {
            return Err(format!("point geometry contains command {}", command & 0x7));
        }
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

fn decode_detail_lines(data: &[u8]) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut components = Vec::new();
    let mut path: Option<Vec<(i32, i32)>> = None;
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "line command")?;
        let id = command & 0x7;
        let count = command >> 3;
        match id {
            1 => {
                if let Some(path) = path.take() {
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
            7 => {}
            _ => return Err(format!("unknown line command {id}")),
        }
    }
    if let Some(path) = path {
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

fn decode_detail_polygons(data: &[u8]) -> Result<Vec<DetailComponent>, String> {
    let mut cursor = Cursor::new(data);
    let mut rings: Vec<Vec<(i32, i32)>> = Vec::new();
    let (mut x, mut y) = (0i32, 0i32);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "polygon command")?;
        let id = command & 0x7;
        let count = command >> 3;
        match id {
            1 => {
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
                }
            }
            2 => {
                let ring = rings
                    .last_mut()
                    .ok_or_else(|| "polygon LineTo without MoveTo".to_string())?;
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
                if let Some(ring) = rings.last_mut()
                    && let Some(&first) = ring.first()
                {
                    // ClosePath does not alter x/y. This is required by MVT 4.3.3.3.
                    ring.push(first);
                }
            }
            _ => return Err(format!("unknown polygon command {id}")),
        }
    }

    // Group rings into components: an outer ring opens a component, following
    // holes attach to it. Winding sign classifies the role.
    let mut grouped: Vec<Vec<DetailRing>> = Vec::new();
    for ring in rings {
        let role = if signed_area(&ring) > 0 {
            CanonRingRole::Outer
        } else {
            CanonRingRole::Hole
        };
        let ring = make_detail_ring(role, ring);
        if role == CanonRingRole::Outer || grouped.is_empty() {
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
