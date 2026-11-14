//! MVT tile decoder producing structural detail for out-of-crate adjudication.
//!
//! elivagar is the only decoder of a PMTiles archive in this system; brokkr
//! links the crate and decodes tiles through this module. Decoding stops at
//! structure: a [`DetailTile`] carries its layers, features, geometry
//! components and rings in **wire order**, with no canonicalization and no
//! hashing. Polygon rings use the first nonzero-area ring's orientation as
//! the exterior orientation; zero-area rings are skipped unless a feature's
//! only ring, which is kept as MapLibre's classifyRings keeps it. Deciding what
//! counts as "the same tile" - ordering, digests, the comparison - is the
//! caller's, per the corpus redesign contract. The one knob here is
//! [`Strictness`]: the gate decodes strict so foreign structure
//! can never silently skip past it; a cross-producer comparison decodes
//! tolerant.

use std::sync::Arc;

use crate::geometry;
use crate::mvt::decode::{
    self, Command, CommandKind, CommandSink, GeometryError, Grammar, Point, Words,
};
use crate::mvt::message::{self, ValueMessage};

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
/// the exterior orientation, and zero-area rings are skipped unless one is
/// the feature's only ring (an Outer, whatever its area).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CanonRingRole {
    Point = 0,
    Path = 1,
    Outer = 2,
    Hole = 3,
}

/// A decoded MVT attribute value. `Float` and `Double` hold the IEEE bit
/// patterns, not the numbers, so `Eq`, `Ord` and `Hash` are total: equality
/// is bitwise (distinct signed zeros and NaN payloads stay distinct) and
/// ordering is unsigned bit-pattern order, not numeric float order.
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
    /// The wire `type` value, kept raw rather than as an enum: tolerant
    /// decoding preserves an unknown type (0, 4 and up) with empty
    /// components, and a known type with no geometry is distinct from it.
    /// The ring roles below are derived during decoding from this type and
    /// the geometry commands, so they do not restate it; an enum here would
    /// still admit a Point feature carrying Path rings, which only a geometry
    /// type with associated data would rule out.
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
///
/// Messages are walked by the crate's one message reader (`mvt::message`),
/// whose policy [`Strictness`] selects; this module adds the owned output,
/// the geometry builders and ring grouping.
pub fn decode_detail_tile(data: &[u8], strictness: Strictness) -> Result<DetailTile, String> {
    let layers = message::parse_tile(data, strictness)?
        .into_iter()
        .map(|layer| decode_detail_layer(&layer, strictness))
        .collect::<Result<_, _>>()?;
    Ok(DetailTile { layers })
}

fn decode_detail_layer(
    layer: &message::LayerMessage<'_>,
    strictness: Strictness,
) -> Result<DetailLayer, String> {
    let keys: Vec<Arc<str>> = layer.keys.iter().map(|&key| Arc::from(key)).collect();
    let values = layer
        .values
        .iter()
        .map(|bytes| decode_detail_attr(bytes, strictness))
        .collect::<Result<Vec<_>, _>>()?;
    let features = layer
        .features
        .iter()
        .map(|bytes| decode_detail_feature(bytes, &keys, &values, strictness))
        .collect::<Result<_, _>>()?;
    Ok(DetailLayer {
        name: Arc::from(layer.name),
        extent: layer.extent,
        version: layer.version,
        features,
    })
}

/// Decode one MVT value message (the layer value table entry).
pub fn decode_detail_attr(data: &[u8], strictness: Strictness) -> Result<DetailAttr, String> {
    #[allow(clippy::cast_possible_wrap)]
    Ok(match message::decode_value(data, strictness)? {
        ValueMessage::String(text) => DetailAttr::String(Arc::from(text)),
        ValueMessage::Float(bits) => DetailAttr::Float(bits),
        ValueMessage::Double(bits) => DetailAttr::Double(bits),
        ValueMessage::Int(raw) => DetailAttr::Int(raw as i64),
        ValueMessage::UInt(raw) => DetailAttr::UInt(raw),
        ValueMessage::SInt(raw) => DetailAttr::SInt(unzigzag64(raw)),
        ValueMessage::Bool(flag) => DetailAttr::Bool(flag),
    })
}

/// Decode one MVT feature message against the layer key/value tables.
pub fn decode_detail_feature(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
    strictness: Strictness,
) -> Result<DetailFeature, String> {
    let feature = message::parse_feature(data, strictness)?;
    let attrs = decode_detail_attrs(&feature.tags, keys, values)?;
    let components = match &feature.geometry {
        Some(geometry) => decode_detail_geometry(feature.geom_type, geometry, strictness)?,
        None => Vec::new(),
    };
    Ok(DetailFeature {
        id: feature.id,
        geom_type: feature.geom_type,
        attrs,
        components,
    })
}

fn decode_detail_attrs(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
) -> Result<Vec<(Arc<str>, DetailAttr)>, String> {
    let pairs = message::tag_pairs(data)?;
    let mut attrs = Vec::with_capacity(pairs.len());
    for (key_idx, value_idx) in pairs {
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
    // Strict runs the one spec 4.3.4 grammar; tolerant decodes unvalidated
    // and keeps the lenient quirks each builder documents. An unknown type
    // never reaches the decoder, so tolerant accepts its bytes unread.
    let grammar = |grammar| (strictness == Strictness::Strict).then_some(grammar);
    let words = Words::packed(data);
    match geom_type {
        1 => {
            let mut sink = PointSink::default();
            decode::decode(words, grammar(Grammar::Point), &mut sink)?;
            Ok(sink.finish())
        }
        2 => {
            let mut sink = LineSink::default();
            decode::decode(words, grammar(Grammar::LineString), &mut sink)?;
            Ok(sink.finish())
        }
        3 => {
            let mut sink = PolygonSink::default();
            decode::decode(words, grammar(Grammar::Polygon), &mut sink)?;
            Ok(group_detail_rings(sink.rings))
        }
        _ => Ok(Vec::new()),
    }
}

/// Points: every MoveTo parameter, in one Point ring. Tolerant accepts zero
/// counts and repeated MoveTo commands; any other command fails either way.
#[derive(Default)]
struct PointSink {
    points: Vec<(i32, i32)>,
}

impl CommandSink for PointSink {
    fn command(&mut self, command: Command) -> Result<(), GeometryError> {
        if command.kind == CommandKind::MoveTo {
            Ok(())
        } else {
            Err(GeometryError::Consumer(format!(
                "point geometry contains command {}",
                command.kind.id()
            )))
        }
    }

    fn point(&mut self, point: Point) -> Result<(), GeometryError> {
        self.points.push(point.xy_i32()?);
        Ok(())
    }
}

impl PointSink {
    fn finish(self) -> Vec<DetailComponent> {
        if self.points.is_empty() {
            Vec::new()
        } else {
            vec![make_detail_component(vec![make_detail_ring(
                CanonRingRole::Point,
                self.points,
            )])]
        }
    }
}

/// Lines: one Path component per MoveTo. Tolerant quirks, kept because the
/// cross-producer comparison depends on them: a MoveTo flushes the open path
/// (a zero-count MoveTo leaves none open); the extra points of a multi-point
/// MoveTo are emitted at once as one-point paths while the first point's path
/// stays open; ClosePath is ignored.
#[derive(Default)]
struct LineSink {
    components: Vec<DetailComponent>,
    path: Option<Vec<(i32, i32)>>,
    /// Points of the current MoveTo seen so far, None inside a LineTo.
    move_points: Option<u32>,
}

impl CommandSink for LineSink {
    fn command(&mut self, command: Command) -> Result<(), GeometryError> {
        match command.kind {
            CommandKind::MoveTo => {
                if let Some(path) = self.path.take() {
                    push_detail_line(&mut self.components, path);
                }
                self.move_points = Some(0);
            }
            CommandKind::LineTo => {
                if self.path.is_none() {
                    return Err(GeometryError::Consumer(
                        "line LineTo without MoveTo".to_string(),
                    ));
                }
                self.move_points = None;
            }
            CommandKind::ClosePath => {}
        }
        Ok(())
    }

    fn point(&mut self, point: Point) -> Result<(), GeometryError> {
        let xy = point.xy_i32()?;
        match self.move_points.as_mut() {
            Some(0) => {
                self.path = Some(vec![xy]);
                self.move_points = Some(1);
            }
            Some(_) => push_detail_line(&mut self.components, vec![xy]),
            None => {
                if let Some(path) = self.path.as_mut() {
                    path.push(xy);
                }
            }
        }
        Ok(())
    }
}

impl LineSink {
    fn finish(mut self) -> Vec<DetailComponent> {
        if let Some(path) = self.path.take() {
            push_detail_line(&mut self.components, path);
        }
        self.components
    }
}

fn push_detail_line(components: &mut Vec<DetailComponent>, path: Vec<(i32, i32)>) {
    if !path.is_empty() {
        components.push(make_detail_component(vec![make_detail_ring(
            CanonRingRole::Path,
            path,
        )]));
    }
}

/// Polygons: raw rings in wire order. Tolerant quirks: each MoveTo point
/// opens a ring, a LineTo after ClosePath extends the closed ring, and every
/// ClosePath (any count) appends the ring's first vertex once.
#[derive(Default)]
struct PolygonSink {
    rings: Vec<Vec<(i32, i32)>>,
    in_move: bool,
}

impl CommandSink for PolygonSink {
    fn command(&mut self, command: Command) -> Result<(), GeometryError> {
        match command.kind {
            CommandKind::MoveTo => self.in_move = true,
            CommandKind::LineTo => {
                if self.rings.is_empty() {
                    return Err(GeometryError::Consumer(
                        "polygon LineTo without MoveTo".to_string(),
                    ));
                }
                self.in_move = false;
            }
            CommandKind::ClosePath => {
                // ClosePath does not alter the cursor (MVT 4.3.3.3); the
                // decoder owns that, this only closes the ring.
                if let Some(ring) = self.rings.last_mut()
                    && let Some(&first) = ring.first()
                {
                    ring.push(first);
                }
            }
        }
        Ok(())
    }

    fn point(&mut self, point: Point) -> Result<(), GeometryError> {
        let xy = point.xy_i32()?;
        if self.in_move {
            self.rings.push(vec![xy]);
        } else if let Some(ring) = self.rings.last_mut() {
            ring.push(xy);
        }
        Ok(())
    }
}

/// Rings grouped by the crate's one ring-grouping rule
/// (`geometry::polygon::group_rings`).
fn group_detail_rings(rings: Vec<Vec<(i32, i32)>>) -> Vec<DetailComponent> {
    geometry::polygon::group_rings(rings, |ring| {
        geometry::polygon::signed_area_2x(ring.iter().copied())
    })
    .into_iter()
    .map(|group| {
        let mut rings = Vec::with_capacity(group.holes.len() + 1);
        rings.push(make_detail_ring(CanonRingRole::Outer, group.outer));
        rings.extend(
            group
                .holes
                .into_iter()
                .map(|hole| make_detail_ring(CanonRingRole::Hole, hole)),
        );
        make_detail_component(rings)
    })
    .collect()
}

fn make_detail_ring(role: CanonRingRole, points: Vec<(i32, i32)>) -> DetailRing {
    DetailRing { role, points }
}

fn make_detail_component(rings: Vec<DetailRing>) -> DetailComponent {
    DetailComponent { rings }
}

#[inline]
fn unzigzag64(value: u64) -> i64 {
    #[allow(clippy::cast_possible_wrap)]
    {
        ((value >> 1) as i64) ^ (-((value & 1) as i64))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use protohoggr::{WIRE_32BIT, WIRE_64BIT, WIRE_LEN, WIRE_VARINT};

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
                .contains("MoveTo missing LineTo")
        );

        let two_point_line = feature_message(Some(2), Some(&[9, 0, 0, 10, 2, 2]));
        assert!(decode_detail_feature(&two_point_line, &[], &[], Strictness::Strict).is_ok());

        let split_line_to = feature_message(Some(2), Some(&[9, 0, 0, 10, 2, 2, 10, 2, 2]));
        assert!(
            decode_detail_feature(&split_line_to, &[], &[], Strictness::Strict)
                .unwrap_err()
                .contains("LineTo must follow MoveTo")
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
                .contains("LineTo count must be greater than 1")
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

    #[test]
    fn lone_zero_area_ring_is_kept_as_classifyrings_keeps_it() {
        let collinear = [(0, 0), (10, 0), (20, 0), (0, 0)];
        let commands = polygon_commands(&[&collinear]);
        let feature = feature_message(Some(3), Some(&commands));
        let decoded = decode_detail_feature(&feature, &[], &[], Strictness::Strict).unwrap();
        assert_eq!(decoded.components.len(), 1);
        assert_eq!(decoded.components[0].rings.len(), 1);
        assert_eq!(decoded.components[0].rings[0].role, CanonRingRole::Outer);

        // Two zero-area rings: no shortcut, both skipped, as in MapLibre.
        let commands = polygon_commands(&[&collinear, &collinear]);
        let feature = feature_message(Some(3), Some(&commands));
        let decoded = decode_detail_feature(&feature, &[], &[], Strictness::Strict).unwrap();
        assert!(decoded.components.is_empty());

        // Tolerant: a lone one-point ring is kept too; no rings, no groups.
        let feature = feature_message(Some(3), Some(&[9, 2, 2]));
        let decoded = decode_detail_feature(&feature, &[], &[], Strictness::Tolerant).unwrap();
        assert_eq!(decoded.components.len(), 1);
        let feature = feature_message(Some(3), Some(&[]));
        let decoded = decode_detail_feature(&feature, &[], &[], Strictness::Tolerant).unwrap();
        assert!(decoded.components.is_empty());
    }

    #[test]
    fn tolerant_unclosed_ring_area_includes_the_closing_edge() {
        // (10,0) (0,0) (0,10) with no ClosePath: the open shoelace sum is 0,
        // which would drop the ring as zero-area; MapLibre's area wraps to
        // the first vertex and reads -100.
        let feature = feature_message(Some(3), Some(&[9, 20, 0, 18, 19, 0, 0, 20]));
        let decoded = decode_detail_feature(&feature, &[], &[], Strictness::Tolerant).unwrap();
        assert_eq!(decoded.components.len(), 1);
        assert_eq!(decoded.components[0].rings[0].role, CanonRingRole::Outer);
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
