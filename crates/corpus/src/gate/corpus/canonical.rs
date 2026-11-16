//! The canonical semantic tile hash - brokkr's per the redesign (`elivagar.md`,
//! "Code that leaves the crate": *the canonical semantic tile hash
//! (`streaming_tile_hash`) and the canonicalization rules*).
//!
//! This is a **verbatim port** of elivagar's frozen `streaming_tile_hash`
//! (pre-redesign `regress.rs`, commit `0129ef3~1`). It is the sole definition of
//! every committed corpus leaf: the committed `corpus/*/leaves` were written by
//! this exact algorithm and this port must reproduce them bit-for-bit (no
//! re-bless - the committed baseline is the parity oracle). The
//! `elivagar-stream-*` domain strings are the version pins: any semantic change
//! here is a corpus-rotation + full-recalibration event, never a quiet edit.
//!
//! It decodes MVT off the wire directly (via `protohoggr`'s cursor and
//! elivagar's shared `decode_detail_attr`), canonicalizing on the fly:
//! unordered collections (layers, features, geometry components) are reduced by
//! hashing their sorted child digests, while rings keep their encoded point
//! order and rotation. The separate `tile_detail` decoder is regress's, not this
//! path's; the equivalence tests keep the two one relation.
//!
//! Casts here are faithful wire-decode arithmetic (zig-zag, signed area) that
//! were `#[allow]`ed at the source; the long match arms are the MVT geometry
//! command grammar.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::unnecessary_sort_by
)]

use std::cmp::Ordering;
use std::io::{self, Read};
use std::sync::Arc;

use flate2::read::GzDecoder;

use super::super::constants::mvt;
use super::super::eliv::{
    CanonRingRole, Cursor, DetailAttr, Strictness, WIRE_LEN, WIRE_VARINT, decode_detail_attr,
};
use super::canon_sink::{CanonSink, HashSink, detail_attrs_hash, write_len, write_string};

// ---------------------------------------------------------------------------
// Public entry: decompress + streaming hash. Signature preserved from the
// elivagar original so `corpus::compute` calls it unchanged.
// ---------------------------------------------------------------------------

/// Reusable decompression buffer, so a full-corpus walk allocates once.
#[derive(Default)]
pub struct DecodeScratch {
    buf: Vec<u8>,
}

impl DecodeScratch {
    /// Also the decompression buffer for regress's detail pass, so a whole-
    /// archive diff reuses one allocation per rayon worker.
    pub(crate) fn decompress(&mut self, raw: &[u8]) -> io::Result<&[u8]> {
        self.buf.clear();
        GzDecoder::new(raw).read_to_end(&mut self.buf)?;
        Ok(&self.buf)
    }
}

/// The canonical semantic hash of one raw (gzipped MVT) tile blob.
pub fn semantic_hash(raw: &[u8], scratch: &mut DecodeScratch) -> io::Result<u128> {
    let data = scratch.decompress(raw)?;
    streaming_tile_hash(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ---------------------------------------------------------------------------
// The detail canonical form: the same "same tile" relation, computed over
// elivagar's wire-order DetailTile instead of the raw MVT stream.
//
// This is the second half of the equivalence relation. The streaming hash above
// decodes MVT inline; this decodes through elivagar's `decode_detail_tile` and
// re-applies the identical canonicalization (stable-sort layers by name,
// multiset features, sort attrs, ordered rings within multiset components) with
// the same `elivagar-stream-*` domains - so the two are value-identical exactly
// when the two independent decode paths agree, which the equivalence tests pin.
//
// It remains an independent detail-decoder oracle for the equivalence tests.
// Production's raw-blob fingerprint is `semantic_hash`; regress calls that
// same function in its parallel canonical pass.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) fn detail_tile_hash(tile: &super::super::eliv::DetailTile) -> u128 {
    let mut layers: Vec<(&str, u128)> = tile
        .layers
        .iter()
        .map(|l| (l.name.as_ref(), detail_layer_hash(l)))
        .collect();
    layers.sort_by(|a, b| a.0.cmp(b.0));
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-tile-v1");
    write_len(&mut sink, layers.len());
    for (_, hash) in &layers {
        sink.bytes(&hash.to_le_bytes());
    }
    sink.finish()
}

#[cfg(test)]
fn detail_layer_hash(layer: &super::super::eliv::DetailLayer) -> u128 {
    let mut feature_hashes: Vec<u128> = layer.features.iter().map(detail_feature_hash).collect();
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-layer-v2");
    write_string(&mut sink, &layer.name);
    sink.bytes(&layer.extent.to_le_bytes());
    sink.bytes(&layer.version.to_le_bytes());
    sink.bytes(&multiset_hash(b"features", &mut feature_hashes).to_le_bytes());
    sink.finish()
}

#[cfg(test)]
fn detail_feature_hash(feature: &super::super::eliv::DetailFeature) -> u128 {
    let mut attrs = feature.attrs.clone();
    attrs.sort();
    let attrs_digest = detail_attrs_hash(&attrs);
    let geometry_digest = detail_geometry_hash(feature.geom_type, &feature.components);
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-feature-v1");
    match feature.id {
        Some(id) => {
            sink.bytes(&[1]);
            sink.bytes(&id.to_le_bytes());
        }
        None => sink.bytes(&[0]),
    }
    sink.bytes(&[feature.geom_type]);
    sink.bytes(&attrs_digest.to_le_bytes());
    sink.bytes(&geometry_digest.to_le_bytes());
    sink.finish()
}

#[cfg(test)]
fn detail_geometry_hash(geom_type: u8, components: &[super::super::eliv::DetailComponent]) -> u128 {
    let mut comp_hashes: Vec<u128> = components.iter().map(detail_component_hash).collect();
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-geometry-v1");
    sink.bytes(&[geom_type]);
    sink.bytes(&multiset_hash(b"components", &mut comp_hashes).to_le_bytes());
    sink.finish()
}

#[cfg(test)]
fn detail_component_hash(component: &super::super::eliv::DetailComponent) -> u128 {
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-component-v1");
    write_len(&mut sink, component.rings.len());
    for ring in &component.rings {
        sink.bytes(&detail_ring_hash(ring).to_le_bytes());
    }
    sink.finish()
}

/// Regress's canonical feature order: id, then geom type, then attributes, then
/// component geometry (ring count, then role and points per ring). The render
/// core sorts a layer's features through this so the SVG byte order is
/// deterministic and matches the canonical form; regress diffs through it too.
/// Verbatim from elivagar's `compare_detail_features` (needs none of the dropped
/// bbox/structure fields).
pub(crate) fn compare_detail_features(
    a: &super::super::eliv::DetailFeature,
    b: &super::super::eliv::DetailFeature,
) -> std::cmp::Ordering {
    a.id.cmp(&b.id)
        .then_with(|| a.geom_type.cmp(&b.geom_type))
        .then_with(|| a.attrs.cmp(&b.attrs))
        .then_with(|| compare_component_slices(&a.components, &b.components))
}

/// Canonical order of two components (ring count, then per-ring role + points).
/// The new wire-order decoder does not sort a feature's attrs/components the way
/// the old decoder did, so consumers must canonicalize a decoded feature through
/// this + an attr sort before `compare_detail_features` is meaningful.
pub(crate) fn compare_detail_components(
    a: &super::super::eliv::DetailComponent,
    b: &super::super::eliv::DetailComponent,
) -> std::cmp::Ordering {
    compare_component_slices(std::slice::from_ref(a), std::slice::from_ref(b))
}

pub(crate) trait CanonicalRingOrder {
    fn role(&self) -> CanonRingRole;
    fn points(&self) -> &[(i32, i32)];
}

pub(crate) trait CanonicalComponentOrder {
    type Ring: CanonicalRingOrder;

    fn rings(&self) -> &[Self::Ring];
}

impl CanonicalRingOrder for super::super::eliv::DetailRing {
    fn role(&self) -> CanonRingRole {
        self.role
    }

    fn points(&self) -> &[(i32, i32)] {
        &self.points
    }
}

impl CanonicalComponentOrder for super::super::eliv::DetailComponent {
    type Ring = super::super::eliv::DetailRing;

    fn rings(&self) -> &[Self::Ring] {
        &self.rings
    }
}

pub(crate) fn compare_component_slices<A, B>(a: &[A], b: &[B]) -> Ordering
where
    A: CanonicalComponentOrder,
    B: CanonicalComponentOrder,
{
    for (left, right) in a.iter().zip(b) {
        let left_rings = left.rings();
        let right_rings = right.rings();
        let rings = left_rings.len().cmp(&right_rings.len());
        if rings != Ordering::Equal {
            return rings;
        }
        for (left_ring, right_ring) in left_rings.iter().zip(right_rings) {
            let ring = left_ring
                .role()
                .cmp(&right_ring.role())
                .then_with(|| left_ring.points().cmp(right_ring.points()));
            if ring != Ordering::Equal {
                return ring;
            }
        }
    }
    a.len().cmp(&b.len())
}

#[cfg(test)]
fn detail_ring_hash(ring: &super::super::eliv::DetailRing) -> u128 {
    let mut points = HashSink::new();
    for &(x, y) in &ring.points {
        points.bytes(&x.to_le_bytes());
        points.bytes(&y.to_le_bytes());
    }
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-ring-v1");
    sink.bytes(&[ring.role as u8]);
    write_len(&mut sink, ring.points.len());
    sink.bytes(&points.finish().to_le_bytes());
    sink.finish()
}

/// Order-insensitive combine: hash the sorted child digests as a sequence.
/// Sorting instead of a wrapping-add sum avoids the additive collision structure
/// of sum-based multiset hashing - equal multisets, and only equal multisets,
/// produce the same sorted sequence.
fn multiset_hash(domain: &[u8], hashes: &mut [u128]) -> u128 {
    hashes.sort_unstable();
    let mut sink = HashSink::new();
    sink.bytes(domain);
    write_len(&mut sink, hashes.len());
    for hash in hashes.iter() {
        sink.bytes(&hash.to_le_bytes());
    }
    sink.finish()
}

// ---------------------------------------------------------------------------
// Feature tag resolution (feature packed tags -> resolved (key, value) attrs).
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Streaming tile / layer / feature.
// ---------------------------------------------------------------------------

fn streaming_tile_hash(data: &[u8]) -> Result<u128, String> {
    let mut layers = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read tile tag: {error}"))?
    {
        if field == mvt::TILE_LAYER && wire_type == WIRE_LEN {
            layers.push(streaming_layer_hash(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read tile layer: {error}"))?,
            )?);
        } else {
            return Err(format!("unknown tile field {field}"));
        }
    }
    // Mirror the detail decoder's stable sort by name: duplicate layer names
    // keep their encounter order bound, so the two hashes induce the same
    // equivalence relation.
    layers.sort_by(|a, b| a.0.cmp(b.0));
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-tile-v1");
    write_len(&mut sink, layers.len());
    for (_, hash) in &layers {
        sink.bytes(&hash.to_le_bytes());
    }
    Ok(sink.finish())
}

fn streaming_layer_hash(data: &[u8]) -> Result<(&str, u128), String> {
    let mut name = "";
    let mut extent = mvt::TILE_EXTENT;
    let mut version = mvt::DEFAULT_LAYER_VERSION;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut features = Vec::new();
    let mut cursor = Cursor::new(data);
    while let Some((field, wire_type)) = cursor
        .read_tag()
        .map_err(|error| format!("read layer tag: {error}"))?
    {
        match (field, wire_type) {
            (mvt::layer::NAME, WIRE_LEN) => {
                name = std::str::from_utf8(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read layer name: {error}"))?,
                )
                .map_err(|error| format!("layer name is not UTF-8: {error}"))?;
            }
            (mvt::layer::FEATURE, WIRE_LEN) => features.push(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature message: {error}"))?,
            ),
            (mvt::layer::KEY, WIRE_LEN) => keys.push(Arc::from(
                std::str::from_utf8(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read layer key: {error}"))?,
                )
                .map_err(|error| format!("layer key is not UTF-8: {error}"))?,
            )),
            (mvt::layer::VALUE, WIRE_LEN) => values.push(decode_detail_attr(
                cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read layer value: {error}"))?,
                Strictness::Strict,
            )?),
            (mvt::layer::EXTENT, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read layer extent: {error}"))?;
                extent = u32::try_from(raw).map_err(|_| format!("extent out of range: {raw}"))?;
            }
            (mvt::layer::VERSION, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read layer version: {error}"))?;
                version = u32::try_from(raw).map_err(|_| format!("version out of range: {raw}"))?;
            }
            _ => return Err(format!("unknown layer field {field}")),
        }
    }
    let mut feature_hashes = Vec::with_capacity(features.len());
    for feature in features {
        feature_hashes.push(streaming_feature_hash(feature, &keys, &values)?);
    }
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-layer-v2");
    write_string(&mut sink, name);
    sink.bytes(&extent.to_le_bytes());
    sink.bytes(&version.to_le_bytes());
    sink.bytes(&multiset_hash(b"features", &mut feature_hashes).to_le_bytes());
    Ok((name, sink.finish()))
}

fn streaming_feature_hash(
    data: &[u8],
    keys: &[Arc<str>],
    values: &[DetailAttr],
) -> Result<u128, String> {
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
            (mvt::feature::ID, WIRE_VARINT) => {
                id = Some(
                    cursor
                        .read_varint()
                        .map_err(|error| format!("read feature id: {error}"))?,
                );
            }
            (mvt::feature::TAGS, WIRE_LEN) => {
                tag_bytes.extend_from_slice(
                    cursor
                        .read_len_delimited()
                        .map_err(|error| format!("read feature tags: {error}"))?,
                );
            }
            (mvt::feature::GEOMETRY_TYPE, WIRE_VARINT) => {
                let raw = cursor
                    .read_varint()
                    .map_err(|error| format!("read feature type: {error}"))?;
                geom_type =
                    u8::try_from(raw).map_err(|_| format!("geometry type out of range: {raw}"))?;
            }
            (mvt::feature::GEOMETRY, WIRE_LEN) => {
                let bytes = cursor
                    .read_len_delimited()
                    .map_err(|error| format!("read feature geometry: {error}"))?;
                geometry
                    .get_or_insert_with(Vec::new)
                    .extend_from_slice(bytes);
            }
            _ => return Err(format!("unknown feature field {field}")),
        }
    }
    let mut attrs = decode_detail_attrs(&tag_bytes, keys, values)?;
    attrs.sort();
    let attrs_digest = detail_attrs_hash(&attrs);
    let geometry_digest =
        streaming_geometry_hash(geom_type, geometry.as_deref().unwrap_or_default())?;
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-feature-v1");
    match id {
        Some(id) => {
            sink.bytes(&[1]);
            sink.bytes(&id.to_le_bytes());
        }
        None => sink.bytes(&[0]),
    }
    sink.bytes(&[geom_type]);
    sink.bytes(&attrs_digest.to_le_bytes());
    sink.bytes(&geometry_digest.to_le_bytes());
    Ok(sink.finish())
}

// ---------------------------------------------------------------------------
// Geometry: rings, components, and the three geometry kinds.
// ---------------------------------------------------------------------------

struct StreamingRing {
    points: HashSink,
    first: Option<(i32, i32)>,
    previous: Option<(i32, i32)>,
    vertices: usize,
    area: i128,
}

impl StreamingRing {
    fn new() -> Self {
        Self {
            points: HashSink::new(),
            first: None,
            previous: None,
            vertices: 0,
            area: 0,
        }
    }

    fn point(&mut self, point: (i32, i32)) {
        self.points.bytes(&point.0.to_le_bytes());
        self.points.bytes(&point.1.to_le_bytes());
        if let Some(previous) = self.previous {
            self.area += i128::from(previous.0) * i128::from(point.1)
                - i128::from(point.0) * i128::from(previous.1);
        } else {
            self.first = Some(point);
        }
        self.previous = Some(point);
        self.vertices += 1;
    }

    fn finish(self, role: CanonRingRole) -> u128 {
        let mut sink = HashSink::new();
        sink.bytes(b"elivagar-stream-ring-v1");
        sink.bytes(&[role as u8]);
        write_len(&mut sink, self.vertices);
        sink.bytes(&self.points.finish().to_le_bytes());
        sink.finish()
    }
}

struct StreamingComponent {
    rings: Vec<u128>,
}

impl StreamingComponent {
    fn new() -> Self {
        Self { rings: Vec::new() }
    }
    fn finish(self) -> u128 {
        let mut sink = HashSink::new();
        sink.bytes(b"elivagar-stream-component-v1");
        write_len(&mut sink, self.rings.len());
        for ring in self.rings {
            sink.bytes(&ring.to_le_bytes());
        }
        sink.finish()
    }
}

fn streaming_geometry_hash(geom_type: u8, data: &[u8]) -> Result<u128, String> {
    let mut components = match geom_type {
        mvt::GEOMETRY_POINT => streaming_points(data)?,
        mvt::GEOMETRY_LINESTRING => streaming_lines(data)?,
        mvt::GEOMETRY_POLYGON => streaming_polygons(data)?,
        _ => Vec::new(),
    };
    let mut sink = HashSink::new();
    sink.bytes(b"elivagar-stream-geometry-v1");
    sink.bytes(&[geom_type]);
    sink.bytes(&multiset_hash(b"components", &mut components).to_le_bytes());
    Ok(sink.finish())
}

/// The MVT spec 4.3.4 command grammar, checked per command header before its
/// parameters are read and once more at the end of the stream. It mirrors the
/// grammar elivagar's strict `decode_detail_tile` enforces, so the streaming
/// hash refuses exactly the command streams the detail decode refuses and the
/// two stay one equivalence relation over the tiles they accept.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grammar {
    Point,
    LineString,
    Polygon,
}

struct GrammarState {
    grammar: Grammar,
    /// Point: a MoveTo was seen. LineString: a MoveTo awaits its LineTo.
    pending: bool,
    /// Polygon ring phase: 0 expects MoveTo, 1 LineTo, 2 ClosePath.
    phase: u8,
    any: bool,
}

impl GrammarState {
    fn new(grammar: Grammar) -> Self {
        Self {
            grammar,
            pending: false,
            phase: 0,
            any: false,
        }
    }

    fn check(&mut self, id: u32, count: u32) -> Result<(), String> {
        let err = |message: &str| Err(message.to_string());
        self.any = true;
        if count == 0 {
            return err("geometry command with zero repeat count");
        }
        match (self.grammar, id) {
            (Grammar::Polygon, mvt::COMMAND_CLOSE_PATH) => {
                if count != 1 {
                    return err("ClosePath command count is not 1 (spec 4.3.3.3)");
                }
                match self.phase {
                    1 => return err("polygon ring has too few points (need >=3)"),
                    0 => return err("polygon ClosePath must follow LineTo (spec 4.3.4.4)"),
                    _ => self.phase = 0,
                }
            }
            (_, mvt::COMMAND_CLOSE_PATH) => return err("ClosePath in non-polygon geometry"),
            (Grammar::Point, mvt::COMMAND_MOVE_TO) => {
                if self.pending {
                    return err("point geometry must use a single MoveTo command (spec 4.3.4.2)");
                }
                self.pending = true;
            }
            (Grammar::Point, mvt::COMMAND_LINE_TO) => {
                return err("LineTo is invalid for point geometry (spec 4.3.4.2)");
            }
            (Grammar::LineString, mvt::COMMAND_MOVE_TO) => {
                if count != 1 {
                    return err("linestring MoveTo count must be 1 (spec 4.3.4.3)");
                }
                if self.pending {
                    return err("linestring MoveTo must follow a LineTo (spec 4.3.4.3)");
                }
                self.pending = true;
            }
            (Grammar::LineString, mvt::COMMAND_LINE_TO) => {
                if !self.pending {
                    return err("linestring LineTo must follow MoveTo (spec 4.3.4.3)");
                }
                self.pending = false;
            }
            (Grammar::Polygon, mvt::COMMAND_MOVE_TO) => {
                if count != 1 {
                    return err("polygon ring MoveTo count must be 1 (spec 4.3.4.4)");
                }
                match self.phase {
                    1 => return err("polygon ring missing LineTo"),
                    2 => return err("polygon ring missing ClosePath"),
                    _ => self.phase = 1,
                }
            }
            (Grammar::Polygon, mvt::COMMAND_LINE_TO) => {
                if self.phase != 1 {
                    return err("polygon LineTo must follow MoveTo (spec 4.3.4.4)");
                }
                if count <= 1 {
                    return err("polygon ring LineTo count must be greater than 1 (spec 4.3.4.4)");
                }
                self.phase = 2;
            }
            (_, id) => return Err(format!("unknown geometry command id {id}")),
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), String> {
        let err = |message: &str| Err(message.to_string());
        if !self.any {
            return err("feature has empty geometry command stream");
        }
        match self.grammar {
            Grammar::LineString if self.pending => {
                err("linestring MoveTo missing LineTo (spec 4.3.4.3)")
            }
            Grammar::Polygon if self.phase == 1 => err("polygon ring missing LineTo"),
            Grammar::Polygon if self.phase == 2 => err("polygon ring missing ClosePath"),
            _ => Ok(()),
        }
    }
}

fn unzigzag(value: u32) -> i32 {
    ((value >> 1) as i32) ^ (-((value & 1) as i32))
}

fn read_geometry_varint(cursor: &mut Cursor<'_>, context: &str) -> Result<u32, String> {
    let raw = cursor
        .read_varint()
        .map_err(|error| format!("read {context}: {error}"))?;
    u32::try_from(raw).map_err(|_| format!("{context} out of range: {raw}"))
}

fn next_geometry_point(
    cursor: &mut Cursor<'_>,
    x: &mut i32,
    y: &mut i32,
    context: &str,
) -> Result<(i32, i32), String> {
    *x = x
        .checked_add(unzigzag(read_geometry_varint(
            cursor,
            &format!("{context} x"),
        )?))
        .ok_or_else(|| format!("{context} x overflows i32"))?;
    *y = y
        .checked_add(unzigzag(read_geometry_varint(
            cursor,
            &format!("{context} y"),
        )?))
        .ok_or_else(|| format!("{context} y overflows i32"))?;
    Ok((*x, *y))
}

fn streaming_points(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut ring = StreamingRing::new();
    let mut grammar = GrammarState::new(Grammar::Point);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "point command")?;
        grammar.check(
            command & mvt::COMMAND_ID_MASK,
            command >> mvt::COMMAND_COUNT_SHIFT,
        )?;
        for _ in 0..(command >> mvt::COMMAND_COUNT_SHIFT) {
            ring.point(next_geometry_point(&mut cursor, &mut x, &mut y, "point")?);
        }
    }
    grammar.finish()?;
    Ok(if ring.vertices == 0 {
        Vec::new()
    } else {
        vec![
            StreamingComponent {
                rings: vec![ring.finish(CanonRingRole::Point)],
            }
            .finish(),
        ]
    })
}

fn streaming_lines(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut components = Vec::new();
    let mut path: Option<StreamingRing> = None;
    let mut grammar = GrammarState::new(Grammar::LineString);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "line command")?;
        grammar.check(
            command & mvt::COMMAND_ID_MASK,
            command >> mvt::COMMAND_COUNT_SHIFT,
        )?;
        match (
            command & mvt::COMMAND_ID_MASK,
            command >> mvt::COMMAND_COUNT_SHIFT,
        ) {
            (mvt::COMMAND_MOVE_TO, count) => {
                if let Some(path) = path.take() {
                    components.push(
                        StreamingComponent {
                            rings: vec![path.finish(CanonRingRole::Path)],
                        }
                        .finish(),
                    );
                }
                for index in 0..count {
                    let mut ring = StreamingRing::new();
                    ring.point(next_geometry_point(
                        &mut cursor,
                        &mut x,
                        &mut y,
                        "line MoveTo",
                    )?);
                    if index == 0 {
                        path = Some(ring);
                    } else {
                        components.push(
                            StreamingComponent {
                                rings: vec![ring.finish(CanonRingRole::Path)],
                            }
                            .finish(),
                        );
                    }
                }
            }
            (mvt::COMMAND_LINE_TO, count) => {
                let path = path
                    .as_mut()
                    .ok_or_else(|| "line LineTo without MoveTo".to_string())?;
                for _ in 0..count {
                    path.point(next_geometry_point(
                        &mut cursor,
                        &mut x,
                        &mut y,
                        "line LineTo",
                    )?);
                }
            }
            (mvt::COMMAND_CLOSE_PATH, _) => {}
            (id, _) => return Err(format!("unknown line command {id}")),
        }
    }
    grammar.finish()?;
    if let Some(path) = path {
        components.push(
            StreamingComponent {
                rings: vec![path.finish(CanonRingRole::Path)],
            }
            .finish(),
        );
    }
    Ok(components)
}

fn streaming_polygons(data: &[u8]) -> Result<Vec<u128>, String> {
    let mut cursor = Cursor::new(data);
    let (mut x, mut y) = (0, 0);
    let mut components = Vec::new();
    let mut component = None;
    let mut ring = None;
    let add_ring = |ring: StreamingRing,
                    component: &mut Option<StreamingComponent>,
                    components: &mut Vec<u128>|
     -> Result<(), String> {
        let role = if ring.area > 0 {
            CanonRingRole::Outer
        } else {
            CanonRingRole::Hole
        };
        if role == CanonRingRole::Outer || component.is_none() {
            if let Some(component) = component.take() {
                components.push(component.finish());
            }
            *component = Some(StreamingComponent::new());
        }
        let Some(component) = component.as_mut() else {
            return Err("polygon ring has no component".into());
        };
        component.rings.push(ring.finish(role));
        Ok(())
    };
    let mut grammar = GrammarState::new(Grammar::Polygon);
    while !cursor.is_empty() {
        let command = read_geometry_varint(&mut cursor, "polygon command")?;
        grammar.check(
            command & mvt::COMMAND_ID_MASK,
            command >> mvt::COMMAND_COUNT_SHIFT,
        )?;
        match (
            command & mvt::COMMAND_ID_MASK,
            command >> mvt::COMMAND_COUNT_SHIFT,
        ) {
            (mvt::COMMAND_MOVE_TO, count) => {
                for _ in 0..count {
                    if let Some(ring) = ring.take() {
                        add_ring(ring, &mut component, &mut components)?;
                    }
                    let mut next = StreamingRing::new();
                    next.point(next_geometry_point(
                        &mut cursor,
                        &mut x,
                        &mut y,
                        "polygon MoveTo",
                    )?);
                    ring = Some(next);
                }
            }
            (mvt::COMMAND_LINE_TO, count) => {
                let ring = ring
                    .as_mut()
                    .ok_or_else(|| "polygon LineTo without MoveTo".to_string())?;
                for _ in 0..count {
                    ring.point(next_geometry_point(
                        &mut cursor,
                        &mut x,
                        &mut y,
                        "polygon LineTo",
                    )?);
                }
            }
            (mvt::COMMAND_CLOSE_PATH, _) => {
                if let Some(ring) = ring.as_mut()
                    && let Some(first) = ring.first
                {
                    ring.point(first);
                }
            }
            (id, _) => return Err(format!("unknown polygon command {id}")),
        }
    }
    grammar.finish()?;
    if let Some(ring) = ring {
        add_ring(ring, &mut component, &mut components)?;
    }
    if let Some(component) = component {
        components.push(component.finish());
    }
    Ok(components)
}

// ---------------------------------------------------------------------------
// The streaming <-> detail equivalence tests.
//
// Ported from elivagar's `regress/tests.rs` (0129ef3~1) - the mandatory guard
// that brokkr's two decode paths (the streaming corpus fingerprint above and
// the detail canonical form) remain one equivalence relation, so the digest and
// regress can never silently disagree about "the same tile". Both subjects are
// now brokkr code; elivagar's role is only the wire-order `decode_detail_tile`.
//
// The MVT fixture encoder lives in `corpus::fixture`, shared with the regress
// engine tests: `command`/`zigzag`/field framing are MVT-spec truths that
// self-check against the real decoder, and several fixtures must build hostile
// payloads (unknown fields at every message level, split packed fields) that a
// production encoder is designed never to emit.
// ---------------------------------------------------------------------------
#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::unwrap_used
)]
mod equivalence {
    use super::super::super::constants::mvt;
    use super::super::super::eliv::{
        decode_detail_tile, encode_bytes_field_always, encode_varint_field_always,
    };

    use super::super::super::eliv::Strictness;
    use super::super::fixture::{
        Feature, GeomType, LayerBuilder, Value, command, duplicate_name_layers_tile,
        encode_linestring, encode_tile, float_attr_tile, gzip, line_tile, multiline_tile,
        point_tile, polygon_tile, raw_geometry_tile, repeated_feature_tile, split_geometry_tile,
        split_tags_tile, two_named_layers_tile, versioned_layer_tile, zigzag,
    };
    use super::{DecodeScratch, semantic_hash, streaming_tile_hash};

    type FeatureSpec<'a> = (Option<u64>, &'a [(i32, i32)]);

    // --- the two hashes over the same tile bytes ---

    fn stream(bytes: &[u8]) -> Result<u128, String> {
        streaming_tile_hash(bytes)
    }
    fn detail(bytes: &[u8]) -> Result<u128, String> {
        Ok(super::detail_tile_hash(&decode_detail_tile(
            bytes,
            Strictness::Strict,
        )?))
    }

    /// Both hashes must agree AND deliver the expected verdict: agreement alone
    /// would let correlated mistakes pass.
    fn assert_fingerprint_verdict(left: &[u8], right: &[u8], expect_equal: bool) {
        let detail_equal =
            detail(left).expect("decode left") == detail(right).expect("decode right");
        let streaming_equal =
            stream(left).expect("hash left") == stream(right).expect("hash right");
        assert_eq!(detail_equal, expect_equal, "detail verdict");
        assert_eq!(streaming_equal, expect_equal, "streaming verdict");
    }

    #[test]
    fn streaming_fingerprint_has_a_frozen_value() {
        let tile = line_tile(Some(1), "primary", &[(0, 0), (10, 10)]);
        let expected = 0xf009_d6ca_413d_6a83_596d_8ad9_b611_1e24;
        assert_eq!(stream(&tile).unwrap(), expected,);
        let compressed = gzip(&tile);
        assert_eq!(
            semantic_hash(&compressed, &mut DecodeScratch::default()).unwrap(),
            expected,
        );
    }

    // --- the equivalence tests ---

    #[test]
    fn streaming_fingerprint_matches_detail_hash_on_geometry() {
        let first = &[(0, 0), (10, 10)][..];
        let second = &[(20, 20), (30, 30)][..];
        // Multiline order across components is erased by both hashes.
        assert_fingerprint_verdict(
            &multiline_tile(&[first, second]),
            &multiline_tile(&[second, first]),
            true,
        );

        let outer = &[(0, 0), (100, 0), (100, 100), (0, 100), (0, 0)][..];
        let rotated = &[(100, 0), (100, 100), (0, 100), (0, 0), (100, 0)][..];
        assert_fingerprint_verdict(&polygon_tile(&[outer]), &polygon_tile(&[rotated]), false);
        assert_fingerprint_verdict(
            &line_tile(Some(1), "a", &[(0, 0), (10, 10)]),
            &line_tile(Some(1), "a", &[(0, 0), (11, 10)]),
            false,
        );

        // Hole order within a component stays semantic in both hashes.
        let hole_a = &[(10, 10), (10, 20), (20, 20), (20, 10), (10, 10)][..];
        let hole_b = &[(30, 30), (30, 40), (40, 40), (40, 30), (30, 30)][..];
        assert_fingerprint_verdict(
            &polygon_tile(&[outer, hole_a, hole_b]),
            &polygon_tile(&[outer, hole_b, hole_a]),
            false,
        );

        // Multipoint order stays semantic in both hashes.
        assert_fingerprint_verdict(
            &point_tile(&[(0, 0), (10, 10)]),
            &point_tile(&[(10, 10), (0, 0)]),
            false,
        );
    }

    /// Both decoders refuse every command stream the spec 4.3.4 grammar
    /// forbids, so neither hashes a tile the other will not read.
    #[test]
    fn streaming_and_detail_refuse_the_same_command_grammar() {
        let mv = |n| command(mvt::COMMAND_MOVE_TO, n);
        let lt = |n| command(mvt::COMMAND_LINE_TO, n);
        let close = command(mvt::COMMAND_CLOSE_PATH, 1);
        let z = zigzag;
        let refused: [(GeomType, Vec<u32>); 10] = [
            // A repeated-count line MoveTo.
            (
                GeomType::LineString,
                vec![mv(2), z(0), z(0), z(5), z(5), lt(1), z(5), z(5)],
            ),
            // A line MoveTo with no LineTo after it.
            (GeomType::LineString, vec![mv(1), z(0), z(0)]),
            // Two LineTos for one MoveTo.
            (
                GeomType::LineString,
                vec![mv(1), z(0), z(0), lt(1), z(2), z(2), lt(1), z(2), z(2)],
            ),
            // ClosePath in a line.
            (
                GeomType::LineString,
                vec![mv(1), z(0), z(0), lt(1), z(2), z(2), close],
            ),
            // A zero-count command.
            (GeomType::LineString, vec![mv(0)]),
            // Two point MoveTos.
            (GeomType::Point, vec![mv(1), z(0), z(0), mv(1), z(2), z(2)]),
            // A polygon ring without its ClosePath.
            (
                GeomType::Polygon,
                vec![mv(1), z(0), z(0), lt(2), z(10), z(0), z(0), z(10)],
            ),
            // A polygon ring of two points.
            (
                GeomType::Polygon,
                vec![mv(1), z(0), z(0), lt(1), z(10), z(0), close],
            ),
            // A repeated-count polygon MoveTo.
            (
                GeomType::Polygon,
                vec![
                    mv(2),
                    z(0),
                    z(0),
                    z(5),
                    z(5),
                    lt(2),
                    z(10),
                    z(0),
                    z(0),
                    z(10),
                    close,
                ],
            ),
            // An empty command stream.
            (GeomType::Polygon, Vec::new()),
        ];
        for (index, (geom_type, words)) in refused.into_iter().enumerate() {
            let tile = raw_geometry_tile(geom_type, words);
            assert!(stream(&tile).is_err(), "streaming accepted case {index}");
            assert!(detail(&tile).is_err(), "detail accepted case {index}");
        }
    }

    #[test]
    fn streaming_fingerprint_matches_detail_hash_on_layers_and_features() {
        let first = &[(0, 0), (10, 10)][..];
        let second = &[(20, 20), (30, 30)][..];

        // Distinct layer names: encoding order is erased by both hashes.
        assert_fingerprint_verdict(
            &two_named_layers_tile(false),
            &two_named_layers_tile(true),
            true,
        );
        // Duplicate layer names: stable sort binds encounter order; swap differs.
        assert_fingerprint_verdict(
            &duplicate_name_layers_tile(false),
            &duplicate_name_layers_tile(true),
            false,
        );
        // Feature multiplicity differs as multisets.
        assert_fingerprint_verdict(
            &repeated_feature_tile(&[first, first, second]),
            &repeated_feature_tile(&[first, second, second]),
            false,
        );
        // Attribute values are bit-exact: float vs double, and signed zeros.
        assert_fingerprint_verdict(
            &float_attr_tile(Value::Float(1.0)),
            &float_attr_tile(Value::Double(1.0)),
            false,
        );
        assert_fingerprint_verdict(
            &float_attr_tile(Value::Float(0.0)),
            &float_attr_tile(Value::Float(-0.0)),
            false,
        );

        // Attribute order within a feature is erased by both hashes.
        fn attrs_tile(reverse: bool) -> Vec<u8> {
            let mut layer = LayerBuilder::new("roads");
            let (class, name, primary, main) = if reverse {
                (
                    layer.intern_key("name"),
                    layer.intern_key("class"),
                    layer.intern_value(Value::String("main".to_string())),
                    layer.intern_value(Value::String("primary".to_string())),
                )
            } else {
                (
                    layer.intern_key("class"),
                    layer.intern_key("name"),
                    layer.intern_value(Value::String("primary".to_string())),
                    layer.intern_value(Value::String("main".to_string())),
                )
            };
            let mut geometry = Vec::new();
            encode_linestring(&mut geometry, &[(0, 0), (10, 10)]);
            layer.add_feature(Feature {
                id: Some(1),
                geom_type: GeomType::LineString,
                geometry,
                tags: vec![(class, primary), (name, main)],
            });
            encode_tile(&[&layer])
        }
        assert_fingerprint_verdict(&attrs_tile(false), &attrs_tile(true), true);

        // Feature order is erased by both hashes.
        fn build(order: [FeatureSpec<'_>; 2]) -> Vec<u8> {
            let mut layer = LayerBuilder::new("roads");
            let key = layer.intern_key("class");
            let value = layer.intern_value(Value::String("path".to_string()));
            for (id, coords) in order {
                let mut geometry = Vec::new();
                encode_linestring(&mut geometry, coords);
                layer.add_feature(Feature {
                    id,
                    geom_type: GeomType::LineString,
                    geometry,
                    tags: vec![(key, value)],
                });
            }
            encode_tile(&[&layer])
        }
        assert_fingerprint_verdict(
            &build([(Some(1), first), (Some(2), second)]),
            &build([(Some(2), second), (Some(1), first)]),
            true,
        );
    }

    #[test]
    fn layer_version_is_semantic_with_default_one() {
        assert_fingerprint_verdict(
            &versioned_layer_tile(Some(2)),
            &versioned_layer_tile(Some(3)),
            false,
        );
        assert_fingerprint_verdict(
            &versioned_layer_tile(None),
            &versioned_layer_tile(Some(1)),
            true,
        );
        assert_fingerprint_verdict(
            &versioned_layer_tile(None),
            &versioned_layer_tile(Some(2)),
            false,
        );
    }

    #[test]
    fn unknown_fields_are_hard_errors_at_every_message_level() {
        let level_tile = |unknown_at: u8| -> Vec<u8> {
            let mut value = Vec::new();
            encode_bytes_field_always(&mut value, mvt::value::STRING, b"x");
            if unknown_at == 3 {
                encode_varint_field_always(&mut value, 9, 1);
            }
            let mut feature = Vec::new();
            encode_varint_field_always(
                &mut feature,
                mvt::feature::GEOMETRY_TYPE,
                u64::from(mvt::GEOMETRY_POINT),
            );
            if unknown_at == 2 {
                encode_varint_field_always(&mut feature, 9, 1);
            }
            let mut layer = Vec::new();
            encode_bytes_field_always(&mut layer, mvt::layer::NAME, b"roads");
            encode_varint_field_always(&mut layer, mvt::layer::EXTENT, u64::from(mvt::TILE_EXTENT));
            encode_bytes_field_always(&mut layer, mvt::layer::VALUE, &value);
            encode_bytes_field_always(&mut layer, mvt::layer::FEATURE, &feature);
            if unknown_at == 1 {
                encode_varint_field_always(&mut layer, 9, 1);
            }
            let mut tile = Vec::new();
            encode_bytes_field_always(&mut tile, mvt::TILE_LAYER, &layer);
            if unknown_at == 0 {
                encode_varint_field_always(&mut tile, 9, 1);
            }
            tile
        };
        for level in 0..=3 {
            let tile = level_tile(level);
            assert!(
                stream(&tile).is_err(),
                "streaming must reject unknown field at level {level}"
            );
            assert!(
                decode_detail_tile(&tile, Strictness::Strict).is_err(),
                "detail decoder must reject unknown field at level {level}"
            );
        }
    }

    #[test]
    fn packed_fields_split_across_occurrences_hash_identically() {
        let mv = command(mvt::COMMAND_MOVE_TO, 1);
        let lt = command(mvt::COMMAND_LINE_TO, 1);
        let z0 = zigzag(0);
        let z10 = zigzag(10);
        let whole_geom = [mv, z0, z0, lt, z10, z10];
        let head = [mv, z0, z0];
        let tail = [lt, z10, z10];
        assert_fingerprint_verdict(
            &split_geometry_tile(&[&whole_geom]),
            &split_geometry_tile(&[&head, &tail]),
            true,
        );
        assert_fingerprint_verdict(
            &split_tags_tile(&[&[0, 0, 1, 1]]),
            &split_tags_tile(&[&[0, 0], &[1, 1]]),
            true,
        );
    }
}
