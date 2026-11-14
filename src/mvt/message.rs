//! The one MVT protobuf message reader: tile, layer, feature and value.
//!
//! Every in-crate consumer of an encoded tile walks its messages here, so
//! there is one error policy: a read error (truncated tag, length or varint)
//! fails in both modes. [`Strictness::Strict`] additionally rejects fields the
//! MVT schema does not name, duplicate singular fields (layer name, extent
//! and version; feature id and type), features without a geometry field and
//! value messages carrying more than one value. `Tolerant` skips unknown
//! fields and lets the last duplicate win, for archives from producers that
//! carry structure elivagar never emits.
//!
//! The parsers are envelopes: a layer keeps its features and value table as
//! raw bytes and a feature keeps its packed tags and geometry raw. Parsing a
//! tile strictly therefore says the tile and layer messages are well formed,
//! not that every feature and value is; a consumer promising more (verify)
//! parses every feature and decodes every value itself. Geometry command
//! streams are decoded by `mvt::decode`, never here.

use std::borrow::Cow;

use protohoggr::{Cursor, WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT};

use crate::tile_detail::Strictness;

/// One layer message, envelope only.
pub(crate) struct LayerMessage<'a> {
    /// Empty when the name field is absent.
    pub(crate) name: &'a str,
    /// The MVT default 4096 when absent. Zero is returned as read.
    pub(crate) extent: u32,
    /// The MVT default 1 when absent.
    pub(crate) version: u32,
    pub(crate) keys: Vec<&'a str>,
    /// Raw value messages; see [`decode_value`].
    pub(crate) values: Vec<&'a [u8]>,
    /// Raw feature messages; see [`parse_feature`].
    pub(crate) features: Vec<&'a [u8]>,
    /// Byte length of the layer message body.
    pub(crate) encoded_len: usize,
}

/// One feature message, envelope only.
pub(crate) struct FeatureMessage<'a> {
    pub(crate) id: Option<u64>,
    /// Zero when the type field is absent.
    pub(crate) geom_type: u8,
    /// Packed key/value index pairs; see [`tag_pairs`]. Repeated fields
    /// concatenate.
    pub(crate) tags: Cow<'a, [u8]>,
    /// Packed command words. Repeated fields concatenate.
    pub(crate) geometry: Option<Cow<'a, [u8]>>,
}

/// One decoded value message, strings borrowed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ValueMessage<'a> {
    String(&'a str),
    Float(u32),
    Double(u64),
    Int(u64),
    UInt(u64),
    SInt(u64),
    Bool(bool),
}

fn strict(strictness: Strictness) -> bool {
    strictness == Strictness::Strict
}

/// The layers of a decompressed tile, in wire order. Under `Strict` the tile
/// message holds nothing but layer fields, so a tile with exactly one layer
/// is byte for byte that layer's tag, length and body.
pub(crate) fn parse_tile(
    data: &[u8],
    strictness: Strictness,
) -> Result<Vec<LayerMessage<'_>>, String> {
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
            layers.push(parse_layer(bytes, strictness)?);
        } else if strict(strictness) {
            return Err(format!("unknown tile field {field}"));
        } else {
            cursor
                .skip_field(wire_type)
                .map_err(|error| format!("skip tile field {field}: {error}"))?;
        }
    }
    Ok(layers)
}

fn read_u32(cursor: &mut Cursor<'_>, what: &str) -> Result<u32, String> {
    let raw = cursor
        .read_varint()
        .map_err(|error| format!("read layer {what}: {error}"))?;
    u32::try_from(raw).map_err(|_| format!("{what} out of range: {raw}"))
}

fn utf8<'a>(bytes: &'a [u8], what: &str) -> Result<&'a str, String> {
    std::str::from_utf8(bytes).map_err(|error| format!("{what} is not UTF-8: {error}"))
}

/// Reject a second occurrence of a singular field under `Strict`.
fn once(seen: &mut bool, strictness: Strictness, what: &str) -> Result<(), String> {
    if *seen && strict(strictness) {
        return Err(format!("duplicate {what} field"));
    }
    *seen = true;
    Ok(())
}

pub(crate) fn parse_layer(data: &[u8], strictness: Strictness) -> Result<LayerMessage<'_>, String> {
    let mut layer = LayerMessage {
        name: "",
        extent: crate::geometry::tiles::grid::EXTENT.unsigned_abs(),
        version: 1,
        keys: Vec::new(),
        values: Vec::new(),
        features: Vec::new(),
        encoded_len: data.len(),
    };
    let (mut seen_name, mut seen_extent, mut seen_version) = (false, false, false);
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read layer tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_LEN) => {
                once(&mut seen_name, strictness, "layer name")?;
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer name: {error}"))?;
                layer.name = utf8(bytes, "layer name")?;
            }
            (2, WIRE_LEN) => layer.features.push(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature message: {error}"))?,
            ),
            (3, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer key: {error}"))?;
                layer.keys.push(utf8(bytes, "layer key")?);
            }
            (4, WIRE_LEN) => layer.values.push(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer value: {error}"))?,
            ),
            (5, WIRE_VARINT) => {
                once(&mut seen_extent, strictness, "layer extent")?;
                layer.extent = read_u32(&mut cursor, "extent")?;
            }
            (15, WIRE_VARINT) => {
                once(&mut seen_version, strictness, "layer version")?;
                layer.version = read_u32(&mut cursor, "version")?;
            }
            _ => {
                if strict(strictness) {
                    return Err(format!("unknown layer field {field}"));
                }
                cursor
                    .skip_field(wire_type)
                    .map_err(|error| format!("skip layer field {field}: {error}"))?;
            }
        }
    }
    Ok(layer)
}

/// Append one occurrence of a packed repeated field. Each occurrence must be
/// whole varints on its own: one ending inside a varint would otherwise be
/// repaired by the next occurrence's bytes when they concatenate.
fn push_packed<'a>(
    slot: &mut Option<Cow<'a, [u8]>>,
    bytes: &'a [u8],
    what: &str,
) -> Result<(), String> {
    if bytes.last().is_some_and(|&byte| byte & 0x80 != 0) {
        return Err(format!("{what} ends inside a varint"));
    }
    match slot {
        None => *slot = Some(Cow::Borrowed(bytes)),
        Some(existing) => existing.to_mut().extend_from_slice(bytes),
    }
    Ok(())
}

pub(crate) fn parse_feature(
    data: &[u8],
    strictness: Strictness,
) -> Result<FeatureMessage<'_>, String> {
    let mut id = None;
    let mut geom_type = 0u8;
    let (mut seen_id, mut seen_type) = (false, false);
    let mut tags = None;
    let mut geometry = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read feature tag: {error}"))?
    {
        match (field, wire_type) {
            (1, WIRE_VARINT) => {
                once(&mut seen_id, strictness, "feature id")?;
                id = Some(
                    cursor
                        .read_varint()
                        .map_err(|error| format!("read feature id: {error}"))?,
                );
            }
            (2, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature tags: {error}"))?;
                push_packed(&mut tags, bytes, "feature tags")?;
            }
            (3, WIRE_VARINT) => {
                once(&mut seen_type, strictness, "feature type")?;
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
                push_packed(&mut geometry, bytes, "feature geometry")?;
            }
            _ => {
                if strict(strictness) {
                    return Err(format!("unknown feature field {field}"));
                }
                cursor
                    .skip_field(wire_type)
                    .map_err(|error| format!("skip feature field {field}: {error}"))?;
            }
        }
    }
    if geometry.is_none() && strict(strictness) {
        return Err("feature has no geometry field".to_string());
    }
    Ok(FeatureMessage {
        id,
        geom_type,
        tags: tags.unwrap_or(Cow::Borrowed(&[])),
        geometry,
    })
}

/// A feature's packed tags as (key index, value index) pairs. An odd count
/// or an index beyond u32 fails; bounds against the layer tables are the
/// caller's.
pub(crate) fn tag_pairs(tags: &[u8]) -> Result<Vec<(usize, usize)>, String> {
    let mut cursor = Cursor::new(tags);
    let mut pairs = Vec::with_capacity(tags.len() / 2);
    let index = |cursor: &mut Cursor<'_>, what: &str| -> Result<usize, String> {
        let raw = cursor
            .read_varint()
            .map_err(|error| format!("read feature tag {what}: {error}"))?;
        u32::try_from(raw)
            .map(|index| index as usize)
            .map_err(|_| format!("feature tag {what} index exceeds u32"))
    };
    while !cursor.is_empty() {
        let key = index(&mut cursor, "key")?;
        if cursor.is_empty() {
            return Err("feature tags have an odd index count".to_string());
        }
        let value = index(&mut cursor, "value")?;
        pairs.push((key, value));
    }
    Ok(pairs)
}

/// Decode one value message. Empty fails in both modes.
pub(crate) fn decode_value(
    data: &[u8],
    strictness: Strictness,
) -> Result<ValueMessage<'_>, String> {
    let mut out = None;
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read value tag: {error}"))?
    {
        let varint = |cursor: &mut Cursor<'_>, what: &str| {
            cursor
                .read_varint()
                .map_err(|error| format!("read {what} value: {error}"))
        };
        let value = match (field, wire_type) {
            (1, WIRE_LEN) => ValueMessage::String(utf8(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read string value: {error}"))?,
                "string value",
            )?),
            (2, WIRE_32BIT) => ValueMessage::Float(
                cursor
                    .read_fixed32()
                    .map_err(|error| format!("read float value: {error}"))?,
            ),
            (3, WIRE_64BIT) => ValueMessage::Double(
                cursor
                    .read_fixed64()
                    .map_err(|error| format!("read double value: {error}"))?,
            ),
            (4, WIRE_VARINT) => ValueMessage::Int(varint(&mut cursor, "int")?),
            (5, WIRE_VARINT) => ValueMessage::UInt(varint(&mut cursor, "uint")?),
            (6, WIRE_VARINT) => ValueMessage::SInt(varint(&mut cursor, "sint")?),
            (7, WIRE_VARINT) => ValueMessage::Bool(varint(&mut cursor, "bool")? != 0),
            _ => {
                if strict(strictness) {
                    return Err(format!("unknown value field {field}"));
                }
                cursor
                    .skip_field(wire_type)
                    .map_err(|error| format!("skip value field {field}: {error}"))?;
                continue;
            }
        };
        if out.is_some() && strict(strictness) {
            return Err("MVT value has multiple value fields".to_string());
        }
        out = Some(value);
    }
    out.ok_or_else(|| "empty MVT value".to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_packed_occurrence_ending_inside_a_varint_fails_even_when_the_next_repairs_it() {
        // field 4 len 1 [0x89], field 4 len 1 [0x00]: concatenated, 0x89 0x00
        // would read as one valid word.
        let feature = [0x18, 0x03, 0x22, 0x01, 0x89, 0x22, 0x01, 0x00];
        for strictness in [Strictness::Strict, Strictness::Tolerant] {
            assert!(
                parse_feature(&feature, strictness)
                    .err()
                    .unwrap()
                    .contains("ends inside a varint")
            );
        }
    }

    #[test]
    fn whole_packed_occurrences_concatenate() {
        let feature = [0x18, 0x01, 0x22, 0x01, 0x09, 0x22, 0x02, 0x02, 0x04];
        let parsed = parse_feature(&feature, Strictness::Strict).unwrap();
        assert_eq!(parsed.geometry.unwrap().as_ref(), &[0x09, 0x02, 0x04][..]);
    }

    #[test]
    fn strict_rejects_duplicate_layer_fields_tolerant_keeps_the_last() {
        // extent 4096 twice, then 512.
        let layer = [0x28, 0x80, 0x20, 0x28, 0x80, 0x04];
        assert!(
            parse_layer(&layer, Strictness::Strict)
                .err()
                .unwrap()
                .contains("duplicate layer extent")
        );
        assert_eq!(
            parse_layer(&layer, Strictness::Tolerant).unwrap().extent,
            512
        );
    }

    #[test]
    fn a_truncated_tag_fails_in_both_modes() {
        let tile = [0x1a, 0x05, 0x0a];
        for strictness in [Strictness::Strict, Strictness::Tolerant] {
            assert!(parse_tile(&tile, strictness).is_err());
        }
    }

    #[test]
    fn tag_pairs_reject_an_odd_count() {
        assert!(tag_pairs(&[0, 1, 2]).unwrap_err().contains("odd"));
        assert_eq!(tag_pairs(&[0, 1]).unwrap(), vec![(0, 1)]);
    }
}
